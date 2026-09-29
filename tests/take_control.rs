//! Take Control end to end: the viewer's ticket and the shared clipboard.
//!
//! The same world as `tests/pairing.rs`: a real target daemon (Tulip1) and a
//! real console (Vesper) over the fake sshd's forced command. Tulip1 has a
//! desktop session with one unlocked screen and Omarchy's stay-awake switch
//! (fakes); screen sharing is a stub program whose control socket this test
//! answers while the stub runs. On Vesper, `ibara-view` is `env`, so
//! `ibara-view connect` runs a `connect` script that keeps the bundle it reads
//! on stdin, and `systemd-run` runs its command in place. Each computer's
//! clipboard is a pair of files behind fake `wl-copy` and `wl-paste`
//! (including `--watch`); every copy is counted.
//!
//! Failure cases this must catch:
//! 1. The viewer gets no ticket, a ticket screen sharing never issued, or one
//!    issued for another certificate than this console registered; the bundle
//!    lacks the certificate to pin; the ticket leaks into the reply the ibara
//!    plugin sees.
//! 2. Text copied on the console computer during control does not reach the
//!    controlled computer, or arrives changed or as another kind.
//! 3. A picture copied on the controlled computer does not reach the console
//!    computer, or arrives changed. (Both are larger than one piece, so they
//!    travel in two over the real route and within its size bounds.)
//! 4. What either clipboard held before Take Control is shared.
//! 5. A copy that arrived is sent back (an echo), so a clipboard is set twice.
//! 6. After Hand Back copies still travel, a clipboard watcher keeps running on
//!    either computer, or screen sharing keeps running.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange as JSON lines in DIR.

mod support;

use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const TEXT: &str = "text/plain;charset=utf-8";
const PNG: &str = "image/png";
/// The certificate screen sharing on Tulip1 presents.
const STREAM_CERT: &str = "5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a5e7a";

/// `wl-copy [--type MIME]`: the clipboard is `../clipboard/{data,type}`; each
/// copy adds a line to `../clipboard/copies`.
const FAKE_WL_COPY: &str = r#"#!/bin/sh
d="$(dirname "$0")/../clipboard"
mkdir -p "$d"
type=text/plain
[ "$1" = --type ] && type=$2
cat > "$d/data.new" || exit 1
printf '%s\n' "$type" > "$d/type.new"
mv "$d/data.new" "$d/data"
mv "$d/type.new" "$d/type"
echo copied >> "$d/copies"
"#;

/// `wl-paste --list-types`, `wl-paste --no-newline --type MIME`, and
/// `wl-paste --watch COMMAND...`, which runs COMMAND with the content on its
/// stdin at start and after every copy, and notes its pid in `../clipboard/watchers`.
const FAKE_WL_PASTE: &str = r#"#!/bin/sh
d="$(dirname "$0")/../clipboard"
mkdir -p "$d"
case "$1" in
  --list-types)
    [ -f "$d/type" ] || { echo "Nothing is copied" >&2; exit 1; }
    exec cat "$d/type" ;;
  --watch)
    shift
    echo $$ >> "$d/watchers"
    seen=-1
    while :; do
      now=$(cat "$d/copies" 2>/dev/null | wc -l)
      if [ "$now" != "$seen" ]; then
        seen=$now
        if [ -f "$d/data" ]; then "$@" < "$d/data" || exit 0; else "$@" < /dev/null || exit 0; fi
      fi
      sleep 0.1
    done ;;
esac
want=
while [ $# -gt 0 ]; do
  case "$1" in --type) want=$2; shift 2 ;; *) shift ;; esac
done
[ -f "$d/data" ] && [ "$(cat "$d/type")" = "$want" ] || { echo "No suitable type of content copied" >&2; exit 1; }
exec cat "$d/data"
"#;

/// Screen sharing as `ibarad` starts it (`ibara-stream CONFIG`): notes its pid
/// for the control socket this test answers, then waits to be stopped.
const FAKE_STREAM: &str = r#"#!/bin/sh
echo $$ > "$(dirname "$0")/stream.pid"
exec sleep 600
"#;

/// Omarchy's stay-awake switch, its flag in a file beside it.
const FAKE_TOGGLE_IDLE: &str = r#"#!/bin/sh
flag="$(dirname "$0")/stay-awake"
case "$1" in
  status) if [ -e "$flag" ]; then echo '{"enabled":true}'; else echo '{"enabled":false}'; fi ;;
  stay-awake) touch "$flag" ;;
  allow-idle) rm -f "$flag" ;;
  *) exit 2 ;;
esac
"#;

/// Hyprland with one unlocked screen.
const FAKE_HYPRCTL: &str = r#"#!/bin/sh
case "$*" in
  *monitors*) echo '[{"id":0,"name":"IbaraVirtual","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"focused":true,"solitaryBlockedBy":[],"activeWorkspace":{"id":1,"name":"1"}}]' ;;
  *) echo '[]' ;;
esac
"#;

fn clipboard_dir(world: &World, name: &str) -> PathBuf {
    world.machine_bin(name).join("../clipboard")
}

/// A person copies on `name`, as its own `wl-copy` does.
fn copy(world: &World, name: &str, mime: &str, data: &[u8]) {
    let mut child = Command::new(world.machine_bin(name).join("wl-copy")).args(["--type", mime]).stdin(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    assert!(child.wait().unwrap().success());
}

/// What `name`'s clipboard holds: its kind and bytes.
fn clipboard(world: &World, name: &str) -> Option<(String, Vec<u8>)> {
    let dir = clipboard_dir(world, name);
    let mime = fs::read_to_string(dir.join("type")).ok()?;
    Some((mime.trim().to_string(), fs::read(dir.join("data")).ok()?))
}

/// How many times `name`'s clipboard was set.
fn copies(world: &World, name: &str) -> usize {
    fs::read_to_string(clipboard_dir(world, name).join("copies")).unwrap_or_default().lines().count()
}

fn watchers(world: &World, name: &str) -> Vec<u32> {
    fs::read_to_string(clipboard_dir(world, name).join("watchers")).unwrap_or_default().lines().map(|l| l.parse().unwrap()).collect()
}

/// A process that exists and is not a zombie.
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.rsplit_once(") ").is_some_and(|(_, rest)| !rest.starts_with('Z')))
}

fn wait_for<T>(what: &str, seconds: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Answer `<state>/stream/control.sock` as ibara-stream does, while the stub
/// whose pid is in `pid_file` runs; the tickets it is asked to issue.
fn serve_stream(state: &Path, pid_file: PathBuf) -> Arc<Mutex<Vec<Value>>> {
    let dir = state.join("stream");
    fs::create_dir_all(&dir).unwrap();
    for d in [state, dir.as_path()] {
        fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
    let issued = Arc::new(Mutex::new(Vec::new()));
    let tickets = issued.clone();
    std::thread::spawn(move || {
        let (mut generation, mut closed) = (0u64, true);
        for stream in listener.incoming().flatten() {
            let pid = fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<u32>().ok());
            // Nothing answers while no stream runs.
            let Some(pid) = pid.filter(|p| alive(*p)) else { continue };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut reply = json!({"v": 1, "id": request["id"], "ok": true, "server_epoch": "stream-e2e"});
            match request["op"].as_str().unwrap() {
                "status" => {
                    reply["status"] = json!({
                        "generation": generation, "admission_closed": closed, "idle": false, "encoder": "vaapi",
                        "software_cap": null, "server_cert_sha256": STREAM_CERT, "pid": pid,
                    })
                }
                "set_generation" => (generation, closed) = (request["generation"].as_u64().unwrap(), false),
                "issue_ticket" => tickets.lock().push(request),
                "revoke" => {
                    closed = true;
                    reply["settlement"] = json!({"invalidated_generation": generation, "fence_generation": generation, "settled": true});
                }
                op => panic!("screen sharing was asked {op}"),
            }
            let _ = writeln!(reader.get_mut(), "{reply}");
        }
    });
    issued
}

/// Pair `console` to `node` as the same person's computer; its computer id.
fn add_own(console: &mut Console, node: &str) -> String {
    let started = console.ok("pair-start", &[node]);
    let paired = console.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    paired["computer_id"].as_str().unwrap().to_string()
}

fn on(console: &mut Console, computer: &str, epoch: &str, command: &str, extra: &[&str]) -> Value {
    let mut args = vec!["--computer", computer, "--epoch", epoch];
    args.extend_from_slice(extra);
    console.ask(command, &args)
}

/// `operator-control --op OP` with the owner and revision the computer reports now.
fn control(console: &mut Console, computer: &str, epoch: &str, op: &str) -> Value {
    let status = on(console, computer, epoch, "operator-status", &[]);
    let status = &status["data"]["result"];
    let (owner, revision) = (status["owner"].as_str().unwrap().to_string(), status["ownership_revision"].as_str().unwrap().to_string());
    let reply = on(console, computer, epoch, "operator-control", &["--op", op, "--owner", &owner, "--revision", &revision]);
    assert!(reply["error"].is_null(), "{op}: {reply}");
    reply["data"].clone()
}

#[test]
fn take_control_hands_the_viewer_its_ticket_and_shares_the_clipboard_both_ways() {
    let world = World::new("take-control");
    for name in ["tulip1", "vesper"] {
        write_executable(&world.machine_bin(name).join("wl-copy"), FAKE_WL_COPY);
        write_executable(&world.machine_bin(name).join("wl-paste"), FAKE_WL_PASTE);
    }
    // Tulip1: a desktop session with one unlocked screen, and screen sharing.
    let desk = world.root.join("desk-tulip1");
    fs::create_dir_all(&desk).unwrap();
    write_executable(&desk.join("ibara-stream"), FAKE_STREAM);
    write_executable(&desk.join("hyprctl"), FAKE_HYPRCTL);
    write_executable(&desk.join("omarchy-toggle-idle"), FAKE_TOGGLE_IDLE);
    let issued = serve_stream(&world.root.join("target-tulip1/state"), desk.join("stream.pid"));
    let (stream_bin, hyprctl, idle) = (desk.join("ibara-stream"), desk.join("hyprctl"), desk.join("omarchy-toggle-idle"));
    let env: [(&str, &OsStr); 5] = [
        ("IBARA_STREAM_BIN", stream_bin.as_os_str()),
        ("IBARA_STREAM_BIND", OsStr::new("127.0.0.2")),
        ("IBARA_TEST_HYPRCTL", hyprctl.as_os_str()),
        ("IBARA_TEST_IDLE", idle.as_os_str()),
        ("WAYLAND_DISPLAY", OsStr::new("wayland-e2e")),
    ];
    let _tulip1 = Target::start_with(&world, node("tulip1"), Some("Tulip1"), 300_000, &env);
    // Vesper: ibara-view (an ELF program; `env` runs `connect`), its identity, and
    // systemd-run and hyprctl that stand in for the session.
    let vesper_bin = world.machine_bin("vesper");
    std::os::unix::fs::symlink("/usr/bin/env", vesper_bin.join("ibara-view")).unwrap();
    write_executable(&vesper_bin.join("connect"), "#!/bin/sh\ncat > \"$(dirname \"$0\")/../viewer-bundle.json\"\n");
    write_executable(&vesper_bin.join("systemd-run"), "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n");
    write_executable(&vesper_bin.join("hyprctl"), "#!/bin/sh\nexit 1\n");
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let id = add_own(&mut vesper, "tulip1");
    // The viewer identity, as `ibara-view --create-identity` leaves it beside the console's own state.
    let identity = vesper.home.join(".local/state/ibara/viewer");
    fs::create_dir(&identity).unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).unwrap();
    let der = b"the viewer certificate of the console on Vesper";
    let pem = format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, der));
    fs::write(identity.join("cert.pem"), pem).unwrap();
    fs::write(identity.join("key.pem"), "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n").unwrap();
    let e = vesper.ok("operator-session", &["--computer", &id])["controller_epoch"].as_str().unwrap().to_string();
    copy(&world, "tulip1", TEXT, b"on Tulip1 before Take Control");
    copy(&world, "vesper", TEXT, b"on Vesper before Take Control");

    // Take Control: the viewer, and only the viewer, gets a ticket issued for this console's certificate.
    let taken = control(&mut vesper, &id, &e, "take_control");
    assert!(taken["viewer_started"] == json!(true) && taken["result"]["owner"].as_str().is_some_and(|o| o.starts_with("operator:")), "{taken}");
    let stream = &taken["result"]["stream"];
    assert!(stream.is_object() && stream.get("ticket").is_none(), "the ticket goes to the viewer only: {taken}");
    let bundle_file = vesper_bin.join("../viewer-bundle.json");
    let bundle: Value = wait_for("the viewer's bundle", 10, || fs::read_to_string(&bundle_file).ok().and_then(|b| serde_json::from_str(&b).ok()));
    let tickets = issued.lock().clone();
    assert_eq!(tickets.len(), 1, "{tickets:?}");
    assert_eq!(tickets[0]["client_cert_sha256"], ibara::server::policy::sha256_hex(der), "issued for this console's viewer");
    assert_eq!(bundle["ticket"], tickets[0]["ticket"], "{bundle}");
    assert_eq!((bundle["server_cert_sha256"].as_str(), bundle["computer_id"].as_str()), (Some(STREAM_CERT), Some(id.as_str())), "{bundle}");
    assert_eq!(bundle["identity_dir"], json!(identity), "{bundle}");

    // Both clipboards are watched once sharing starts: the console's at once, Tulip1's from the first poll.
    wait_for("both clipboard watchers", 15, || (!watchers(&world, "tulip1").is_empty() && !watchers(&world, "vesper").is_empty()).then_some(()));
    // Both larger than one piece (768 KiB), so each travels in two over the real route.
    let text = "Copied on Vesper during control: naïve café → ✓\n".repeat(20_000).into_bytes();
    copy(&world, "vesper", TEXT, &text);
    wait_for("Vesper's copy on Tulip1", 20, || (clipboard(&world, "tulip1") == Some((TEXT.into(), text.clone()))).then_some(()));
    let mut picture = b"\x89PNG\r\n\x1a\n".to_vec();
    picture.extend((0..1_048_576u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    copy(&world, "tulip1", PNG, &picture);
    wait_for("Tulip1's picture on Vesper", 20, || (clipboard(&world, "vesper") == Some((PNG.into(), picture.clone()))).then_some(()));
    // Neither starting content crossed, and nothing came back: each clipboard was set
    // by the person twice and by ibara once.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!((copies(&world, "tulip1"), copies(&world, "vesper")), (3, 3), "no starting content shared and no echo");
    assert_eq!(clipboard(&world, "tulip1"), Some((PNG.into(), picture.clone())));

    // Hand Back ends screen sharing, both watchers, and the sharing.
    let stream_pid: u32 = fs::read_to_string(desk.join("stream.pid")).unwrap().trim().parse().unwrap();
    let back = control(&mut vesper, &id, &e, "handback");
    assert_eq!((&back["result"]["owner"], &back["result"]["agent_resumed"]), (&json!("none"), &json!(false)), "{back}");
    wait_for("screen sharing and the clipboard watchers to end", 10, || {
        let running: Vec<u32> = [stream_pid].into_iter().chain(watchers(&world, "tulip1")).chain(watchers(&world, "vesper")).filter(|p| alive(*p)).collect();
        running.is_empty().then_some(())
    });
    copy(&world, "vesper", TEXT, b"on Vesper after Hand Back");
    copy(&world, "tulip1", TEXT, b"on Tulip1 after Hand Back");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(clipboard(&world, "tulip1"), Some((TEXT.into(), b"on Tulip1 after Hand Back".to_vec())));
    assert_eq!(clipboard(&world, "vesper"), Some((TEXT.into(), b"on Vesper after Hand Back".to_vec())));
    assert_eq!((copies(&world, "tulip1"), copies(&world, "vesper")), (4, 4), "nothing travels after Hand Back");
}
