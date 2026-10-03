//! The operator console socket through the real `ibarad --role operator`, with a
//! fake `ssh` (the selected operator route) on PATH.
//!
//! - The socket lives in a private directory and refuses a second service.
//! - `status`, `directory` and `operator-status` answer with the bridge's
//!   version-2 envelope, keyed by the request id; failures keep the bridge's codes.
//! - There is no station: `status` says so and runs nothing, and the old
//!   station commands are unknown.
//! - `operator-observe` answers as a preview event; status and previews share
//!   one route per computer.
//! - `operator-control --op pause|resume` needs no owner or revision, and is
//!   ready only when the target confirms the new state.

use ibara::operator::directory::OperatorDirectory;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ENDPOINT: &str = "ibara_9ac2387e000000000000000000000000";
const COMPUTER: &str = "computer_7c3b2a19e8d4f6015b9a2c4d";

/// The target's `operator-v1` route: answers `session`, `status` and `observe`
/// for epoch_1 with the pinned endpoint and grant generation. The observe answer
/// is `$FAKE_LOG.observe`: a 480×270 tile PNG written by `Fixture::new`. `pause`
/// pauses; `resume` answers without resuming.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_LOG.ssh"
while IFS= read -r line; do
  case "$line" in
    *'"op":"session"'*) printf '%s\n' '{"result":{"endpoint_id":"ibara_9ac2387e000000000000000000000000","controller_epoch":"epoch_1","authorization_generation":3}}' ;;
    *'"op":"status"'*) printf '%s\n' '{"result":{"endpoint_id":"ibara_9ac2387e000000000000000000000000","controller_epoch":"epoch_1","authorization_generation":3,"observation":"allowed","outputs":[{"display_id":"HDMI-A-1"}],"owner":"none","ownership_revision":"r7","interactive_control":"available_if_exclusive"}}' ;;
    *'"op":"observe"'*) cat "$FAKE_LOG.observe" ;;
    *'"op":"pause"'*|*'"op":"resume"'*) printf '%s\n' '{"result":{"endpoint_id":"ibara_9ac2387e000000000000000000000000","controller_epoch":"epoch_1","authorization_generation":3,"paused":true,"pause_origin":"person","owner":"human"}}' ;;
  esac
done
"#;

/// A 480×270 PNG, the largest tile: left half black, right half white, base64.
fn tile_png() -> String {
    let rgb: Vec<u8> = (0..270).flat_map(|_| (0..480u32).flat_map(|x| [if x < 240 { 0 } else { 255 }; 3])).collect();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, 480, 270);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, out)
}

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ibara-console-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["bin", "run", ".config/ibara"] {
            fs::DirBuilder::new().mode(0o700).recursive(true).create(root.join(dir)).unwrap();
        }
        for (name, script) in [("ssh", FAKE_SSH)] {
            let path = root.join("bin").join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for name in ["id_ed25519", "known_hosts"] {
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(root.join(name)).unwrap();
        }
        let mut directory = OperatorDirectory::open(&root.join("state/ibara/operator.sqlite")).unwrap();
        let record = json!({
            "computer_id": COMPUTER, "endpoint_id": ENDPOINT, "label": "Tulip1", "transport": "ssh",
            "host": "tulip1", "user": "vesper", "port": 2222,
            "identity_file_ref": format!("file:{}", root.join("id_ed25519").display()),
            "known_hosts_file_ref": format!("file:{}", root.join("known_hosts").display()),
            "trust_state": "verified", "binding_revision": 1, "authorization_generation": 3,
        });
        directory.register_verified_computer(&record, ENDPOINT, 0).unwrap();
        directory.close();
        let observe = json!({"result": {"endpoint_id": ENDPOINT, "controller_epoch": "epoch_1", "authorization_generation": 3,
            "png": tile_png(), "capture_time": "2026-09-25T12:00:00.000Z", "quality": "tile"}});
        fs::write(root.join("fake.observe"), format!("{observe}\n")).unwrap();
        Fixture { socket: root.join("run/ibara/ibarad.sock"), log: root.join("fake"), root }
    }

    fn start(&self) -> Child {
        let path = format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        Command::new(env!("CARGO_BIN_EXE_ibarad"))
            .args(["--role", "operator"])
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("IBARA_OPERATOR_DIRECTORY_DB", self.root.join("state/ibara/operator.sqlite"))
            .env("FAKE_LOG", &self.log)
            .env_remove("IBARA_STATION_DESCRIPTOR")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn connect(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(stream) = UnixStream::connect(&self.socket) {
                stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
                return Client { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream };
            }
            assert!(Instant::now() < deadline, "the socket never appeared");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn calls(&self, tool: &str) -> Vec<String> {
        fs::read_to_string(self.log.with_extension(tool)).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn send(&mut self, request: Value) {
        writeln!(self.writer, "{request}").unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("an answer line");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    /// Send one request and return its envelope.
    fn ask(&mut self, id: &str, command: &str, args: &[&str]) -> Value {
        self.send(json!({"id": id, "command": command, "args": args}));
        let reply = self.next();
        assert_eq!(reply["id"], id, "{reply}");
        assert_eq!(reply["envelope"]["request_id"], id, "{reply}");
        assert_eq!(reply["envelope"]["version"], 2, "{reply}");
        reply["envelope"].clone()
    }
}

fn stop(mut child: Child) -> i32 {
    // SAFETY: signalling a child we spawned.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap().code().unwrap_or(-1)
}

#[test]
fn the_socket_is_private_and_a_second_service_refuses_to_start() {
    let f = Fixture::new("private");
    let first = f.start();
    drop(f.connect());
    let dir_mode = fs::metadata(f.socket.parent().unwrap()).unwrap().permissions().mode() & 0o777;
    let socket_mode = fs::metadata(&f.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!((dir_mode, socket_mode), (0o700, 0o600));
    let second = f.start().wait_with_output().unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("Another ibarad already serves"));
    assert_eq!(stop(first), 0);
    assert!(!f.socket.exists(), "a clean stop removes the socket");
}

#[test]
fn an_operator_console_stops_cleanly_as_soon_as_its_socket_appears() {
    for attempt in 0..8 {
        let f = Fixture::new(&format!("early-stop-{attempt}"));
        let mut daemon = f.start();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f.socket.exists() {
            assert!(Instant::now() < deadline, "the socket never appeared");
            std::thread::sleep(Duration::from_millis(1));
        }
        // SAFETY: this is the daemon child owned by this E2E instance.
        unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
        let status = daemon.wait().unwrap();
        assert_eq!(status.code(), Some(0), "startup stop {attempt}: {status}");
        assert!(!f.socket.exists(), "a clean startup stop removes its socket");
    }
}

#[test]
fn failures_keep_the_bridge_codes() {
    let f = Fixture::new("failures");
    let daemon = f.start();
    let mut client = f.connect();

    // A line without an id cannot be answered; a request without a command is refused by id.
    client.send(json!({"command": "status"}));
    client.send(json!({"id": "bad-1", "args": []}));
    let refused = client.next();
    assert_eq!(refused["id"], "bad-1");
    assert_eq!(refused["envelope"]["error"]["code"], "INVALID_ARGUMENT");

    let wrong_epoch = client.ask("read-1-session-status", "operator-status", &["--computer", COMPUTER, "--epoch", "epoch_2"]);
    assert_eq!(wrong_epoch["connection"], "unauthorized");
    assert_eq!(wrong_epoch["error"]["code"], "OPERATOR_REFUSED");
    assert_eq!(wrong_epoch["error"]["message"], "Selected operator response identity, epoch or grant generation changed.");

    let unknown = client.ask("read-2", "operator-status", &["--computer", "computer_absent", "--epoch", "epoch_1"]);
    assert_eq!(unknown["error"]["code"], "OPERATOR_REFUSED");

    // A plugin from before every computer was reached over its pairing route:
    // a station command is unknown, and the station poll hears there is none.
    let scoped = {
        client.send(json!({"id": "read-3", "command": "tasks", "for_computer": COMPUTER}));
        client.next()["envelope"].clone()
    };
    assert_eq!((scoped["error"]["code"].as_str(), scoped["error"]["message"].as_str()), (Some("INVALID_ARGUMENT"), Some("Unknown command tasks.")));
    let status = client.ask("status-2", "status", &[]);
    assert_eq!((status["error"].is_null(), &status["data"]), (true, &json!({"station_configured": false})), "{status}");
    assert_eq!(stop(daemon), 0);
}

#[test]
fn status_directory_operator_status_and_previews_answer_over_one_route() {
    let f = Fixture::new("serve");
    let daemon = f.start();
    let mut client = f.connect();

    let directory = client.ask("read-1-directory", "directory", &[]);
    let computers = directory["data"]["computers"].as_array().unwrap();
    assert_eq!(computers.len(), 1);
    assert_eq!((computers[0]["computer_id"].as_str(), computers[0]["trust_state"].as_str()), (Some(COMPUTER), Some("verified")));

    let session = client.ask("read-2", "operator-session", &["--computer", COMPUTER]);
    assert_eq!(session["data"]["controller_epoch"], "epoch_1", "{session}");

    let args = ["--computer", COMPUTER, "--epoch", "epoch_1"];
    let status = client.ask("read-3-session-status", "operator-status", &args);
    assert_eq!(status["connection"], "ready", "{status}");
    assert_eq!(status["data"]["computer_id"], COMPUTER);
    assert_eq!(status["data"]["binding_revision"], 1);
    assert_eq!(status["data"]["result"]["outputs"][0]["display_id"], "HDMI-A-1");
    assert_eq!(status["data"]["result"]["authorization_generation"], 3);

    let observe = |id: &str, extra: &[&str]| {
        let mut args = vec!["--computer", COMPUTER, "--epoch", "epoch_1", "--display", "HDMI-A-1", "--quality", "tile"];
        args.extend_from_slice(extra);
        json!({"id": id, "command": "operator-observe", "args": args})
    };
    client.send(observe("preview-1", &[]));
    let event = client.next();
    assert_eq!((event["event"].as_str(), event["computer_id"].as_str()), (Some("preview"), Some(COMPUTER)), "{event}");
    assert_eq!(event["data"]["request_id"], "preview-1");
    assert_eq!(event["data"]["command"], "operator-observe");
    // The picture arrives as a private PPM file, not base64 in the line; without a
    // shown size, a tile fits 448×256.
    let result = &event["data"]["data"]["result"];
    assert!(result.get("png").is_none(), "{event}");
    let file = std::path::PathBuf::from(result["file"].as_str().unwrap());
    assert!(file.starts_with(f.root.join("run/ibara/previews")), "{}", file.display());
    assert!(file.extension().is_some_and(|e| e == "ppm"), "{}", file.display());
    let written = fs::read(&file).unwrap();
    assert!(written.starts_with(b"P6\n448 252\n255\n"), "{:?}", &written[..16]);
    assert_eq!(result["bytes"], written.len());
    assert_eq!(written.len(), b"P6\n448 252\n255\n".len() + 448 * 252 * 3);
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);

    // The shown size the plugin names: the frame fits it and keeps its aspect.
    client.send(observe("preview-3", &["--width", "256", "--height", "256"]));
    let event = client.next();
    let written = fs::read(event["data"]["data"]["result"]["file"].as_str().unwrap()).unwrap();
    let header = b"P6\n256 144\n255\n";
    assert!(written.starts_with(header), "{event}");
    assert_eq!((written[header.len()], written[header.len() + 255 * 3]), (0, 255), "the picture is not garbled");

    client.send(observe("preview-4", &["--width", "256"]));
    assert_eq!(client.next()["data"]["error"]["message"], "Selected observation size is invalid.");

    // Only the newest three frames of a computer and quality stay; closing keeps only the named ones.
    for id in ["preview-5", "preview-6"] {
        client.send(observe(id, &[]));
        assert!(client.next()["data"]["error"].is_null());
    }
    let frames = || {
        let mut names: Vec<String> = fs::read_dir(f.root.join("run/ibara/previews")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    };
    let kept = frames();
    assert_eq!(kept.len(), 3, "{kept:?}");
    let released = client.ask("release-1", "preview-release", &[kept[0].as_str()]);
    assert_eq!(released["data"], json!({"released": true}));
    assert_eq!(frames(), [kept[0].clone()]);

    client.send(json!({"id": "preview-2", "command": "operator-observe",
        "args": ["--computer", COMPUTER, "--epoch", "epoch_1", "--display", "HDMI-A-1", "--quality", "poster"]}));
    let invalid = client.next();
    assert_eq!(invalid["data"]["error"]["message"], "Selected observation quality is invalid.");

    let routes = f.calls("ssh");
    assert_eq!(routes.len(), 1, "session, status and preview share one route: {routes:?}");
    assert!(routes[0].ends_with("-l ibara-op-vesper tulip1 operator-v1"), "{}", routes[0]);
    assert_eq!(stop(daemon), 0);
}

#[test]
fn pausing_agents_needs_no_owner_and_a_confirmed_answer() {
    let f = Fixture::new("pause");
    let daemon = f.start();
    let mut client = f.connect();
    let control = |op: &'static str| ["--computer", COMPUTER, "--epoch", "epoch_1", "--op", op];

    let paused = client.ask("action-1", "operator-control", &control("pause"));
    assert_eq!(paused["connection"], "ready", "{paused}");
    assert_eq!(paused["data"]["computer_id"], COMPUTER);
    assert_eq!(paused["data"]["result"]["paused"], true);
    assert_eq!(paused["data"]["result"]["pause_origin"], "person");

    let unconfirmed = client.ask("action-2", "operator-control", &control("resume"));
    assert_eq!(unconfirmed["error"]["code"], "CONTROL_UNCERTAIN", "a target still paused is not reported as resumed: {unconfirmed}");

    let routes = f.calls("ssh");
    assert_eq!(routes.len(), 2, "each change has its own route: {routes:?}");
    assert_eq!(stop(daemon), 0);
}
