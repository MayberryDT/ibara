//! End to end through the binaries: a real `ibarad` with temporary state and
//! runtime directories and stub desktop tools, reached through
//! `ibara agent-entry` (MCP over pipes) and `ibara admin`.

use ibara::server::policy::sha256_hex;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const IBARA: &str = env!("CARGO_BIN_EXE_ibara");
const IBARAD: &str = env!("CARGO_BIN_EXE_ibarad");

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ibara-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn entry(args: &[&str], command: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(IBARA);
    cmd.arg("agent-entry").args(args).stdin(Stdio::null());
    match command {
        Some(command) => cmd.env("SSH_ORIGINAL_COMMAND", command),
        None => cmd.env_remove("SSH_ORIGINAL_COMMAND"),
    };
    cmd.output().unwrap()
}

#[test]
fn agent_entry_refuses_anything_but_the_three_commands() {
    for command in [Some("shell"), Some("mcp "), Some(""), None] {
        let out = entry(&["test"], command);
        assert_eq!(out.status.code(), Some(64), "{command:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "Only mcp, transfer-v1 and operator-v1 are supported.\n");
    }
    // A principal that is not a plain name is refused before anything else.
    for principal in ["Test", "", "-x", &"a".repeat(65)] {
        let out = entry(&[principal], Some("mcp"));
        assert_eq!(out.status.code(), Some(64), "{principal:?}");
        assert!(out.stderr.is_empty());
    }
    // The legacy entry takes exactly one principal of at most 23 characters.
    assert_eq!(entry(&["--legacy", "test", "extra"], Some("mcp")).status.code(), Some(64));
    assert_eq!(entry(&["--legacy", &"a".repeat(24)], Some("mcp")).status.code(), Some(64));
    assert_eq!(entry(&["--legacy", "test"], Some("shell")).status.code(), Some(64));
    // An operator peer socket must be a plain absolute path.
    let out = Command::new(IBARA)
        .args(["agent-entry", "test"])
        .env("SSH_ORIGINAL_COMMAND", "operator-v1")
        .env("IBARA_OPERATOR_SOCKET_DIR", "relative/dir")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn ibarad_refuses_an_unknown_role() {
    for args in [&["--role", "console"][..], &["--role"], &["target"]] {
        let out = Command::new(IBARAD).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
}

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stub(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Hyprland with one unlocked screen.
const DESKTOP_HYPRCTL: &str = r#"case "$*" in
  *monitors*) echo '[{"id":0,"name":"IbaraVirtual","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"focused":true,"solitaryBlockedBy":[],"activeWorkspace":{"id":1,"name":"1"}}]' ;;
  *) echo '[]' ;;
esac"#;

/// Omarchy's stay-awake switch, its flag in a file beside it.
const DESKTOP_IDLE: &str = r#"flag="$(dirname "$0")/stay-awake"
case "$1" in
  status) if [ -e "$flag" ]; then echo '{"enabled":true}'; else echo '{"enabled":false}'; fi ;;
  stay-awake) touch "$flag" ;;
  allow-idle) rm -f "$flag" ;;
  *) exit 2 ;;
esac"#;

impl Daemon {
    fn start(name: &str) -> Daemon {
        Daemon::start_with(name, false)
    }

    /// `start`; with `desktop`, the computer has a desktop session: stub
    /// Hyprland with one unlocked screen, a stub stay-awake switch, and a
    /// Wayland display, runtime directory and no session bus, so no real
    /// program on the developer's desktop answers.
    fn start_with(name: &str, desktop: bool) -> Daemon {
        let root = temp_root(name);
        for dir in ["home", "install", "operators", "bin"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("policy.json"), r#"{"principals":["test"]}"#).unwrap();
        std::fs::write(root.join("gateway.key"), "gateway-e2e-secret\n").unwrap();
        std::fs::write(root.join("admin.key"), "admin-e2e-secret\n").unwrap();
        std::fs::write(root.join("admin.sha256"), format!("{}\n", sha256_hex(b"admin-e2e-secret"))).unwrap();
        std::fs::write(root.join("fingerprints.json"), r#"{"fingerprints":{"test":"SHA256:transport-e2e"}}"#).unwrap();
        let projection = std::os::unix::net::UnixListener::bind(root.join("access.sock")).unwrap();
        std::thread::spawn(move || {
            for stream in projection.incoming().flatten() {
                let mut reader=std::io::BufReader::new(stream);
                let mut line=String::new();
                if reader.read_line(&mut line).is_err() {continue;}
                let request:Value=serde_json::from_str(&line).unwrap();
                assert_eq!(request["peers"]["test"]["agent"],true);
                let _=writeln!(reader.get_mut(),"{{\"ok\":true}}");
            }
        });
        let bin = root.join("bin");
        // Stub desktop tools: nothing on the developer's real desktop is touched.
        let hyprctl = stub(&bin, "hyprctl", if desktop { DESKTOP_HYPRCTL } else { "echo '[]'" });
        let grim = stub(&bin, "grim", "exit 1");
        let cua = stub(&bin, "cua-driver", "exit 1");
        let mut command = Command::new(IBARAD);
        command
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("DISPLAY")
            .env("HOME", root.join("home"))
            .env("IBARA_STATE_DIR", root.join("state"))
            .env("IBARA_DATA_DIR", root.join("data"))
            .env("IBARA_RUNTIME_DIR", root.join("run"))
            .env("IBARA_INSTALL_ROOT", root.join("install"))
            .env("IBARA_ACCESS_FINGERPRINTS", root.join("fingerprints.json"))
            .env("IBARA_ACCESS_SOCKET", root.join("access.sock"))
            .env("IBARA_POLICY", root.join("policy.json"))
            .env("IBARA_GATEWAY_KEY", root.join("gateway.key"))
            .env("IBARA_ADMIN_HASH", root.join("admin.sha256"))
            .env("IBARA_OPERATOR_ACCOUNTS", root.join("absent-operator-accounts.json"))
            .env("IBARA_OPERATOR_SOCKET_DIR", root.join("operators"))
            .env("IBARA_TEST_PREVIEW_TOOLS", "1")
            .env("IBARA_TEST_HYPRCTL", hyprctl)
            .env("IBARA_TEST_GRIM", grim)
            .env("IBARA_TEST_CUA", cua)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if desktop {
            let runtime = root.join("xdg-run");
            std::fs::create_dir_all(&runtime).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
            command
                .env("WAYLAND_DISPLAY", "wayland-e2e")
                .env("XDG_RUNTIME_DIR", runtime)
                .env("IBARA_TEST_IDLE", stub(&bin, "omarchy-toggle-idle", DESKTOP_IDLE))
                .env_remove("DBUS_SESSION_BUS_ADDRESS");
        }
        let mut child = command.spawn().unwrap();
        let (tx, rx) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut log = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(line) if line.contains("\"controller_ready\"") => break,
                Ok(line) => log.push(line),
                Err(_) => panic!("ibarad did not become ready: {log:#?} (exit {:?})", child.try_wait()),
            }
        }
        Daemon { child, root }
    }

    fn admin(&self, args: &[&str], key: &str) -> (Option<i32>, Value) {
        let out = Command::new(IBARA)
            .arg("admin")
            .args(args)
            .env("IBARA_ADMIN_SOCKET", self.root.join("run/admin.sock"))
            .env("IBARA_ADMIN_KEY", self.root.join(key))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        (out.status.code(), serde_json::from_str(&text).unwrap_or_else(|_| panic!("stdout {text:?} stderr {}", String::from_utf8_lossy(&out.stderr))))
    }
}

fn exchange(stdin: &mut impl Write, stdout: &mut impl BufRead, request: Value) -> Value {
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line:?}"));
        // The computer asking whether the client is still there.
        if reply["method"] == "ping" {
            writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": reply["id"], "result": {} })).unwrap();
            continue;
        }
        assert_eq!(reply["id"], request["id"], "{reply}");
        return reply;
    }
}

/// Run a line relay (`transfer-v1`, `operator-v1`) with `input` on stdin; the reply lines.
fn relay(daemon: &Daemon, command: &str, extra: &[&str], input: &str) -> Vec<Value> {
    let run = daemon.root.join("run");
    let mut child = Command::new(IBARA)
        .args(["agent-entry", "test"])
        .arg(run.join("controller.sock"))
        .arg(daemon.root.join("gateway.key"))
        .args(extra)
        .env("SSH_ORIGINAL_COMMAND", command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{command}: {:?}", out.status);
    String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn mcp_session_and_admin_cli_through_a_real_ibarad() {
    let daemon = Daemon::start("mcp");
    let run = daemon.root.join("run");
    let mode = |name: &str| std::fs::metadata(run.join(name)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("controller.sock"), 0o660);
    assert_eq!(mode("admin.sock"), 0o600);

    let mut session = Command::new(IBARA)
        .args(["agent-entry", "test"])
        .arg(run.join("controller.sock"))
        .arg(daemon.root.join("gateway.key"))
        .env("SSH_ORIGINAL_COMMAND", "mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = session.stdin.take().unwrap();
    let mut stdout = BufReader::new(session.stdout.take().unwrap());
    let init = exchange(
        &mut stdin,
        &mut stdout,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "codex", "version": "1" } } }),
    );
    assert!(init["result"]["serverInfo"]["name"].is_string(), "{init}");
    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).unwrap();
    let list = exchange(&mut stdin, &mut stdout, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"computer_status") && names.contains(&"computer_observe"), "{names:?}");
    let status = exchange(
        &mut stdin,
        &mut stdout,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "computer_status", "arguments": {} } }),
    );
    let result = &status["result"];
    assert_eq!(result["isError"], false, "{status}");
    assert_eq!(result["structuredContent"]["status"], "ok", "{status}");
    assert!(result["structuredContent"]["situation"].as_str().is_some_and(|s| !s.is_empty()), "{status}");
    assert!(result["content"][0]["text"].as_str().is_some_and(|t| !t.is_empty()), "{status}");
    drop(stdin);
    assert!(session.wait().unwrap().success());

    // transfer-v1: one reply line per request line; an unparsable line is DELIVERY_UNAVAILABLE.
    let replies = relay(&daemon, "transfer-v1", &[], "{\"kind\":\"stat_artifact\",\"artifact_ref\":\"art_missing\"}\n\n");
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert!(replies[0].is_object(), "{replies:?}");
    assert_eq!(replies[1]["error"]["code"], "DELIVERY_UNAVAILABLE");
    // operator-v1 over the bearer route with a key the target never issued: the 403 body is relayed;
    // pairing_confirm without a pinned fingerprint never leaves the relay.
    std::fs::write(daemon.root.join("stranger.key"), "not-an-operator\n").unwrap();
    let stranger = daemon.root.join("stranger.key");
    let replies = relay(
        &daemon,
        "operator-v1",
        &[stranger.to_str().unwrap()],
        "{\"op\":\"status\"}\n{\"op\":\"pairing_confirm\",\"operator_key_fingerprint\":\"SHA256:x\"}\nnot json\n",
    );
    assert_eq!(replies[0], json!({ "error": { "code": "PERMISSION_DENIED", "message": "Unauthorized transport." } }));
    assert_eq!(replies[1]["error"]["message"], "Operator request failed; inspect target status.");
    assert_eq!(replies[2]["error"]["message"], "Invalid operator JSON.");

    // The administrator CLI: pretty {"result":…}, exit 0; a wrong key is a 403 body and exit 1.
    let (code, endpoint) = daemon.admin(&["endpoint"], "admin.key");
    assert_eq!(code, Some(0), "{endpoint}");
    assert!(endpoint["result"]["endpoint_id"].as_str().is_some_and(|id| id.len() >= 8), "{endpoint}");
    let (code, pause) = daemon.admin(&["pause"], "admin.key");
    assert_eq!(code, Some(0), "{pause}");
    assert_eq!(pause["result"]["ok"], true, "{pause}");
    let (code, refused) = daemon.admin(&["status"], "gateway.key");
    assert_eq!(code, Some(1));
    assert_eq!(refused, json!({ "error": { "code": "PERMISSION_DENIED", "message": "Unauthorized transport." } }));

    // SIGTERM: a clean exit that removes the sockets and the lock.
    let mut daemon = daemon;
    // SAFETY: plain signal delivery to our own child.
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "ibarad did not stop after SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
    for leftover in ["run/controller.sock", "run/admin.sock", "state/controller.lock"] {
        assert!(!daemon.root.join(leftover).exists(), "{leftover} left behind");
    }
}

/// A call in the instant `ibarad` restarts, before it listens again: nothing
/// reached it, so the agent is told ibara is starting and may send the same call
/// again. Only a call that may have reached it keeps the reconcile wording.
#[test]
fn a_call_before_ibarad_listens_is_safe_to_repeat() {
    let root = temp_root("not-listening");
    std::fs::write(root.join("gateway.key"), "key\n").unwrap();
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap().trim().to_string();
    let begin = |socket: &Path| {
        let mut session = Command::new(IBARA)
            .args(["agent-entry", "test"])
            .arg(socket)
            .arg(root.join("gateway.key"))
            .env("SSH_ORIGINAL_COMMAND", "mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = session.stdin.take().unwrap();
        let mut stdout = BufReader::new(session.stdout.take().unwrap());
        let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                           "params": { "name": "computer_begin", "arguments": { "request_id": "req_1", "goal": "x" } } });
        let reply = exchange(&mut stdin, &mut stdout, call);
        drop(stdin);
        assert!(session.wait().unwrap().success());
        reply["result"]["structuredContent"].clone()
    };
    let starting = |envelope: &Value| {
        assert_eq!(envelope["status"], "error", "{envelope}");
        let error = &envelope["error"];
        assert_eq!(error["code"], "SESSION_UNAVAILABLE", "{envelope}");
        assert_eq!(error["message"], format!("ibara on {host} is starting; try again in a few seconds."), "{envelope}");
        assert_eq!((error["retry_safe"].as_bool(), error["requires_reconciliation"].as_bool()), (Some(true), Some(false)), "{envelope}");
        assert_eq!(
            error["next"],
            format!("Nothing was sent. Call computer_begin again in a few seconds with the same arguments; if this lasts more than a minute, ask a person to check ibara on {host}."),
            "{envelope}"
        );
        assert_eq!(situation(envelope), format!("{host} · ibara is starting"), "{envelope}");
    };
    // ibarad stopped: its socket is gone.
    starting(&begin(&root.join("controller.sock")));
    // The old socket file is still there, and nothing listens on it.
    drop(std::os::unix::net::UnixListener::bind(root.join("stale.sock")).unwrap());
    starting(&begin(&root.join("stale.sock")));
    // The call reached a controller that closed without answering: it may have run.
    let listener = std::os::unix::net::UnixListener::bind(root.join("closing.sock")).unwrap();
    let closer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = [0u8; 64];
        let _ = std::io::Read::read(&mut stream, &mut head);
    });
    let sent = begin(&root.join("closing.sock"));
    closer.join().unwrap();
    let error = &sent["error"];
    assert_eq!(
        (error["code"].as_str(), error["message"].as_str(), error["retry_safe"].as_bool(), error["requires_reconciliation"].as_bool()),
        (Some("SESSION_UNAVAILABLE"), Some("Controller unavailable; reconcile pending effects after reconnect."), Some(false), Some(true)),
        "{sent}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// How often `agent-entry` asks an [`Agent`]'s client whether it is still
/// there. The client counts as gone after three unanswered intervals.
const PING_MS: &str = "300";

/// One agent's MCP session to the computer: `ibara agent-entry` for principal
/// `test` over pipes, as sshd runs it for one SSH connection, initialized as
/// MCP client `client`. It stays open, heartbeating, until dropped. Its client
/// answers the computer's pings, as ibara's relay does, until the network under
/// it drops ([`Agent::drop_network`]).
struct Agent {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    replies: mpsc::Receiver<Value>,
    answering: Arc<AtomicBool>,
    next_id: u64,
}

impl Agent {
    fn start(daemon: &Daemon, client: &str) -> Agent {
        let mut child = Command::new(IBARA)
            .args(["agent-entry", "test"])
            .arg(daemon.root.join("run/controller.sock"))
            .arg(daemon.root.join("gateway.key"))
            .env("SSH_ORIGINAL_COMMAND", "mcp")
            .env("IBARA_TEST_CLIENT_PING_MS", PING_MS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let answering = Arc::new(AtomicBool::new(true));
        let (tx, replies) = mpsc::channel();
        let (to_computer, answers) = (stdin.clone(), answering.clone());
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line:?}"));
                if message["method"] == "ping" {
                    if answers.load(Ordering::SeqCst) {
                        let _ = writeln!(to_computer.lock(), "{}", json!({ "jsonrpc": "2.0", "id": message["id"], "result": {} }));
                    }
                } else if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let mut agent = Agent { child, stdin, replies, answering, next_id: 1 };
        agent.request(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                              "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": client, "version": "1" } } }));
        writeln!(agent.stdin.lock(), "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).unwrap();
        agent
    }

    fn request(&mut self, request: Value) -> Value {
        writeln!(self.stdin.lock(), "{request}").unwrap();
        let reply = self.replies.recv_timeout(Duration::from_secs(60)).expect("agent-entry answered");
        assert_eq!(reply["id"], request["id"], "{reply}");
        reply
    }

    /// One tool call; the envelope it answered.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        self.next_id += 1;
        self.request(json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call", "params": { "name": tool, "arguments": args } }))["result"]["structuredContent"]
            .clone()
    }

    /// The network under this session drops. The computer does not notice:
    /// the session stays open and heartbeating, but nothing reaches its
    /// client any more, so nothing answers.
    fn drop_network(&self) {
        self.answering.store(false, Ordering::SeqCst);
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn situation(envelope: &Value) -> &str {
    envelope["situation"].as_str().unwrap_or_default()
}

/// Wait until the fresh computer is ready, then begin a task as `agent`: the
/// task and its workspace.
fn begin_when_ready(agent: &mut Agent) -> (String, PathBuf) {
    // A fresh start is paused until the computer proves healthy.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !situation(&agent.call("computer_status", json!({}))).contains("nobody controls") {
        assert!(std::time::Instant::now() < deadline, "the computer never became ready");
        std::thread::sleep(Duration::from_millis(200));
    }
    let began = agent.call("computer_begin", json!({ "goal": "Write a note", "request_id": "begin-1" }));
    assert_eq!(began["status"], "ok", "{began}");
    (began["result"]["task_ref"].as_str().unwrap().to_string(), PathBuf::from(began["result"]["workspace"].as_str().unwrap()))
}

/// The network under an agent's connection drops. The computer keeps the old
/// session open (sshd does not probe the client, and the session's heartbeats
/// go on), and the agent comes back over a new connection. Failure cases:
/// 1. The agent cannot use its own task from the new connection until the old
///    one lapses, and is told another agent controls the computer.
/// 2. Its begin there says another agent controls instead of naming its task,
///    or its next move is to wait for another request instead of to carry on;
///    and the task's status tells it to wait instead of to carry on.
/// 3. What it gets back is new control, not the control it held.
/// 4. After a person paused the computer while the agent was away, the agent
///    gets control back, or is told anything but that a person has it.
/// Another agent meanwhile: `controller::tests::reconnect`.
#[test]
fn an_agent_back_on_a_new_connection_carries_on_with_its_task_at_once() {
    let daemon = Daemon::start_with("reconnect", true);
    let lease = || daemon.admin(&["status"], "admin.key").1["result"]["lease"].clone();
    let mut dropped = Agent::start(&daemon, "codex");
    let (task, workspace) = begin_when_ready(&mut dropped);
    let held = lease();
    dropped.drop_network();

    // The agent's client gives up on the old connection only well after the
    // computer found it silent (about a minute against 15 s in production).
    let mut back = Agent::start(&daemon, "codex");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !situation(&back.call("computer_status", json!({}))).contains("you (codex@test) control") {
        assert!(std::time::Instant::now() < deadline, "the old session never counted as gone");
        std::thread::sleep(Duration::from_millis(100));
    }
    let begin = back.call("computer_begin", json!({ "goal": "Write a note again", "request_id": "begin-2" }));
    assert_eq!((begin["error"]["code"].as_str(), begin["error"]["task_ref"].as_str()), (Some("BUSY"), Some(task.as_str())), "{begin}");
    assert!(situation(&begin).contains("you (codex@test) control"), "{begin}");
    assert!(begin["error"]["next"].as_str().unwrap_or_default().starts_with("computer_act or computer_observe to continue"), "{begin}");
    let status = back.call("computer_status", json!({ "ref": task }));
    assert!(status["result"]["next"][0].as_str().unwrap_or_default().starts_with("computer_act or computer_observe to continue"), "{status}");
    let wrote = back.call("computer_files", json!({ "task_ref": task, "request_id": "write-1", "op": "write", "path": "note.txt", "text": "back\n" }));
    assert_eq!(wrote["status"], "ok", "{wrote}");
    assert!(situation(&wrote).contains("you (codex@test) control"), "{wrote}");
    assert_eq!(std::fs::read_to_string(workspace.join("note.txt")).unwrap(), "back\n");
    let resumed = lease();
    assert_eq!(resumed["generation"], held["generation"], "the same control, resumed: {resumed}");
    assert_ne!(resumed["connection_id"], held["connection_id"], "{resumed}");

    // A person pauses the computer while the agent is away again.
    drop(back);
    assert_eq!(daemon.admin(&["pause"], "admin.key").0, Some(0));
    let mut again = Agent::start(&daemon, "codex");
    let paused = again.call("computer_files", json!({ "task_ref": task, "request_id": "write-3", "op": "write", "path": "note.txt", "text": "again\n" }));
    assert_eq!(paused["error"]["code"], "HUMAN_CONTROL", "{paused}");
    assert!(situation(&paused).contains("paused for a person"), "{paused}");
    let retained = lease();
    assert_eq!(retained["task_ref"], task, "the paused task stays reserved: {retained}");
    assert_eq!(retained["connection_id"], resumed["connection_id"], "another connection did not take it under the person: {retained}");
    assert_eq!(std::fs::read_to_string(workspace.join("note.txt")).unwrap(), "back\n");
    drop(dropped);
}

/// Two sessions of the same client on one computer (two `codex` sessions),
/// both connected. Failure cases:
/// 1. The second session's begin names the first one's task and says "you
///    control", so it goes on to use that task.
/// 2. A call from the second session naming the task moves the first
///    session's control to it, and the first session's next input fails.
#[test]
fn a_second_session_of_the_same_agent_cannot_take_a_live_sessions_control() {
    let daemon = Daemon::start_with("second-session", true);
    let lease = || daemon.admin(&["status"], "admin.key").1["result"]["lease"].clone();
    let mut first = Agent::start(&daemon, "codex");
    let (task, workspace) = begin_when_ready(&mut first);
    let held = lease();

    let mut second = Agent::start(&daemon, "codex");
    // Long enough for a silent first session to count as gone.
    std::thread::sleep(Duration::from_millis(1500));
    let begin = second.call("computer_begin", json!({ "goal": "Write another note", "request_id": "begin-2" }));
    assert_eq!((begin["error"]["code"].as_str(), begin["error"]["message"].as_str()), (Some("BUSY"), Some("Control is unavailable.")), "{begin}");
    assert!(begin["error"]["task_ref"].is_null(), "{begin}");
    assert!(situation(&begin).contains("another agent controls"), "{begin}");
    let taken = second.call("computer_files", json!({ "task_ref": task, "request_id": "write-2", "op": "write", "path": "note.txt", "text": "second\n" }));
    assert_eq!((taken["error"]["code"].as_str(), taken["error"]["message"].as_str()), (Some("BUSY"), Some("Control is unavailable.")), "{taken}");
    assert_eq!(lease()["connection_id"], held["connection_id"], "the first session keeps its control");

    let wrote = first.call("computer_files", json!({ "task_ref": task, "request_id": "write-1", "op": "write", "path": "note.txt", "text": "first\n" }));
    assert_eq!(wrote["status"], "ok", "{wrote}");
    assert_eq!(std::fs::read_to_string(workspace.join("note.txt")).unwrap(), "first\n");
}
