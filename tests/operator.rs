//! Operator side through the real `ibara` binary, with a fake `ssh` on PATH that
//! records every invocation.
//!
//! - `ibara mcp` is a lazy relay: no ssh until the first `tools/call`, exactly one
//!   route per computer for the session afterwards, and a tool result (never a
//!   crash) when a route fails.
//! - Without `--computer` one session covers every computer: status lists them all,
//!   begin picks one by name, and later calls follow the task to its computer.
//! - `ibara operator --serve` keeps one route per computer: a target refusal keeps
//!   it, a reply for the wrong epoch closes it.

use ibara::operator::directory::OperatorDirectory;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const ENDPOINT: &str = "ibara_9ac2387e000000000000000000000000";

// Work CLI failure cases (before implementation): invalid commands acquire a
// machine; shell quoting mutates argv; run starts a job on a different target;
// reconnection starts a second task; CLI completion silently finishes background
// work. Real CLI/relay, fixture SSH peer; controller/process proof is separate.
#[test]
fn work_cli_runs_and_reconnects_through_the_same_agent_transport() {
    let f=Fixture::fleet("work-cli");
    let db=f.db.to_str().unwrap();
    let (code,out,err)=f.run(&["work","--directory-db",db,"--computer","Tulip1","run","--goal","Build a plugin","--request-id","run-one","--","printf","%s","literal $(not-a-shell) argument"]);
    assert_eq!(code,0,"{out} {err}");
    let rows=f.route_lines();
    let calls:Vec<Value>=rows.iter().filter_map(|s| s.split_once(' ').and_then(|(_,j)|serde_json::from_str::<Value>(j).ok())).filter(|v|v["method"]=="tools/call").collect();
    let begin=calls.iter().find(|v|v["params"]["name"]=="computer_begin").unwrap();
    assert_eq!(begin["params"]["arguments"]["goal"],"Build a plugin");
    let exec=calls.iter().find(|v|v["params"]["name"]=="computer_exec").unwrap();
    assert_eq!(exec["params"]["arguments"]["task_ref"],"task_tulip1_1");
    assert_eq!(exec["params"]["arguments"]["command"],json!(["printf","%s","literal $(not-a-shell) argument"]));
    assert_eq!(exec["params"]["arguments"]["background"],true);
    assert!(!calls.iter().any(|v|v["params"]["name"]=="computer_finish"));
    let before=rows.len();
    let (code,out,err)=f.run(&["work","--directory-db",db,"--computer","Tulip1","exec","--task","task_tulip1_1","--request-id","step-two","--","pwd"]);
    assert_eq!(code,0,"{out} {err}");
    assert!(!f.route_lines()[before..].iter().any(|s|s.contains("\"name\":\"computer_begin\"")));
}

#[test]
fn work_cli_rejects_invalid_arguments_before_opening_a_route() {
    let f=Fixture::fleet("work-invalid");
    for args in [vec!["work","run","--goal","build"],vec!["work","exec","--","pwd"],vec!["work","cancel"]] {
        let (code,_,_)=f.run(&args); assert_ne!(code,0);
    }
    assert!(f.invocations().is_empty());
}

// Real CLI and relay lose the launch response after the fixture accepts it.
// The saved identity must allow inspection without acquiring/launching again.
#[test]
fn work_cli_lost_reply_exposes_the_original_identity_and_does_not_relaunch() {
    let peer=FLEET_SSH.replace("    *'\"name\":\"computer_begin\"'*)", "    *'\"name\":\"computer_exec\"'*) printf 'accepted\\n' >> \"$FAKE_SSH_LOG.accepted\"; exit 0 ;;\n    *'\"name\":\"computer_begin\"'*)");
    let f=Fixture::with("work-lost",&peer,&[(COMPUTER,ENDPOINT,"Tulip1","tulip1")]);
    let db=f.db.to_str().unwrap();
    let (code,_,err)=f.run(&["work","--directory-db",db,"--computer","Tulip1","run","--goal","Build once","--","true"]);
    assert_ne!(code,0,"lost response must not claim success");
    let identities:Vec<Value>=err.lines().filter_map(|line|serde_json::from_str(line).ok()).collect();
    let saved=identities.iter().find(|v|v["work"]["task_ref"]=="task_tulip1_1").expect(&err);
    let request=saved["request_id"].as_str().unwrap();
    let receipt:Value=serde_json::from_slice(&fs::read(saved["receipt"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(receipt["work"]["task_ref"],"task_tulip1_1");
    let (code,out,err)=f.run(&["work","--directory-db",db,"--computer","Tulip1","run","--goal","Build once","--request-id",request,"--","true"]);
    assert_eq!(code,0,"{out} {err}");
    assert_eq!(f.route_lines().iter().filter(|s|s.contains("\"name\":\"computer_begin\"")).count(),1);
    assert_eq!(fs::read_to_string(f.log.with_extension("log.accepted")).unwrap(),"accepted\n");
}
const COMPUTER: &str = "computer_7c3b2a19e8d4f6015b9a2c4d";
const ENDPOINT0: &str = "ibara_b61b2fbb000000000000000000000000";
const COMPUTER0: &str = "computer_5a1e0c7b9d2f4e6a8b3c1d0e";

const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_SSH_LOG"
if [ "$FAKE_SSH_MODE" = fail ]; then
  echo "ssh: connect to host tulip1 port 2222: Connection refused" >&2
  exit 255
fi
eval last=\${$#}
if [ "$last" = operator-v1 ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"display_id":"refuse"'*) printf '%s\n' '{"error":{"code":"BUSY","message":"one capture at a time"}}' ;;
      *) printf '%s\n' '{"result":{"endpoint_id":"ibara_9ac2387e000000000000000000000000","controller_epoch":"epoch_1","authorization_generation":3,"png":"iVBO"}}' ;;
    esac
  done
  exit 0
fi
if [ "$last" = transfer-v1 ]; then
  while IFS= read -r line; do
    printf '%s\n' '{"result":{"artifact_ref":"artifact_x","size_bytes":5}}'
  done
  exit 0
fi
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake-target","version":"0"}}}' ;;
    *'"method":"tools/call"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"remote %s"}],"isError":false}}\n' "$id" "${IBARA_TRANSPORT_ROUTE_V1:+selected}" ;;
  esac
done
"#;

/// The target's `mcp` route for a fleet: answers as the computer named by the ssh
/// host (`tulip0`, `tulip1`), which it also calls itself, as a real computer
/// does in its situation line, fleet row, begin's `computer` and hints. Status
/// lists that one computer, begin opens `task_<host>_1`, a status ref names that
/// task as running or this computer (anything else ended), other calls answer
/// `{on: <host>}`. `$FAKE_SSH_DOWN` names a host that is unreachable. Every
/// host is logged to `$FAKE_SSH_LOG`, every line to `.lines`.
const FLEET_SSH: &str = r#"#!/bin/sh
eval host=\${$(($# - 1))}
printf '%s\n' "$host" >> "$FAKE_SSH_LOG"
if [ "$host" = "$FAKE_SSH_DOWN" ]; then
  echo "ssh: connect to host $host port 2222: No route to host" >&2
  exit 255
fi
while IFS= read -r line; do
  printf '%s %s\n' "$host" "$line" >> "$FAKE_SSH_LOG.lines"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake-target","version":"0"}}}'
      continue ;;
    *'"method":"tools/call"'*) ;;
    *) continue ;;
  esac
  case "$line" in
    *'"name":"computer_begin"'*) env='{"situation":"'$host' · you (codex) control · r1","status":"ok","since":[],"result":{"task_ref":"task_'$host'_1","computer":{"id":"cmp_'$host'","name":"'$host'"}}}' ;;
    *'"ref":"task_'$host'_1"'*) env='{"situation":"'$host' · nobody controls · r1","status":"ok","since":[],"result":{"ref":"task_'$host'_1","kind":"task","state":"running","children":[],"next":[]}}' ;;
    *'"ref":"cmp_'*) env='{"situation":"'$host' · nobody controls · r1","status":"ok","since":[],"result":{"ref":"cmp_'$host'","kind":"computer","state":"ready","summary":"'$host' · nobody controls · all available","children":[],"next":["computer_begin({computer: \"'$host'\", goal, request_id})"]}}' ;;
    *'"ref":'*) env='{"situation":"'$host' · nobody controls · r1","status":"ok","since":[],"result":{"ref":"x","kind":"task","state":"ended","children":[],"next":[]}}' ;;
    *'"name":"computer_status"'*) env='{"situation":"'$host' · nobody controls · r1","status":"ok","since":["'$host' woke"],"result":{"computers":[{"id":"cmp_'$host'","name":"'$host'","state":"ready","capabilities":"all available"}]}}' ;;
    *) env='{"situation":"'$host' · you (codex) control · r2","status":"ok","since":[],"result":{"on":"'$host'"}}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"structuredContent":%s,"isError":false}}\n' "$id" "$host" "$env"
done
"#;

struct Fixture {
    root: PathBuf,
    log: PathBuf,
    bin: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        Self::with(tag, FAKE_SSH, &[(COMPUTER, ENDPOINT, "Tulip1", "tulip1")])
    }

    /// Tulip0 and Tulip1, answering as a fleet.
    fn fleet(tag: &str) -> Self {
        Self::with(tag, FLEET_SSH, &[(COMPUTER0, ENDPOINT0, "Tulip0", "tulip0"), (COMPUTER, ENDPOINT, "Tulip1", "tulip1")])
    }

    fn with(tag: &str, script: &str, computers: &[(&str, &str, &str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!("ibara-mcp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::DirBuilder::new().mode(0o700).recursive(true).create(root.join("bin")).unwrap();
        let bin = root.join("bin");
        let ssh = bin.join("ssh");
        fs::write(&ssh, script).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["id_ed25519", "known_hosts"] {
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(root.join(name)).unwrap();
        }
        let db = root.join("state/ibara/operator.sqlite");
        let mut directory = OperatorDirectory::open(&db).unwrap();
        for (computer, endpoint, label, host) in computers {
            let record = json!({
                "computer_id": computer, "endpoint_id": endpoint, "label": label, "transport": "ssh",
                "host": host, "user": "vesper", "port": 2222,
                "identity_file_ref": format!("file:{}", root.join("id_ed25519").display()),
                "known_hosts_file_ref": format!("file:{}", root.join("known_hosts").display()),
                "trust_state": "verified", "binding_revision": 1, "authorization_generation": 3,
            });
            directory.register_verified_computer(&record, endpoint, 0).unwrap();
        }
        directory.close();
        Fixture { log: root.join("ssh.log"), root, bin, db }
    }

    /// Every line a route received, as `host line`.
    fn route_lines(&self) -> Vec<String> {
        let lines = self.log.with_extension("log.lines");
        fs::read_to_string(lines).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
    }

    fn onboarding(&self) -> Option<Value> {
        let text = fs::read_to_string(self.root.join(".local/state/ibara/onboarding.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(&self.log).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
    }

    /// `ibara ARGS…` run to completion: its exit code, stdout and stderr.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = Command::new(env!("CARGO_BIN_EXE_ibara"))
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("FAKE_SSH_LOG", &self.log)
            .env("FAKE_SSH_MODE", "serve")
            .env_remove("XDG_STATE_HOME")
            .env_remove("IBARA_OPERATOR_DIRECTORY_DB")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (out.status.code().unwrap_or(-1), text(&out.stdout), text(&out.stderr))
    }

    fn spawn(&self, args: &[&str], mode: &str) -> Relay {
        self.spawn_with(args, mode, "")
    }

    fn spawn_with(&self, args: &[&str], mode: &str, down: &str) -> Relay {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default());
        let mut child = Command::new(env!("CARGO_BIN_EXE_ibara"))
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("FAKE_SSH_LOG", &self.log)
            .env("FAKE_SSH_MODE", mode)
            .env("FAKE_SSH_DOWN", down)
            .env("IBARA_STATION_DESCRIPTOR", self.root.join("station.json"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("IBARA_OPERATOR_DIRECTORY_DB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    return;
                }
            }
        });
        Relay { stdin: child.stdin.take(), child, lines }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Relay {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Relay {
    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn reply(&self) -> Value {
        let line = self.lines.recv_timeout(Duration::from_secs(20)).expect("relay reply");
        serde_json::from_str(&line).unwrap()
    }

    fn finish(mut self) -> i32 {
        drop(self.stdin.take());
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

fn initialize(relay: &mut Relay) -> Value {
    relay.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "codex", "version": "1"}}}));
    let init = relay.reply();
    relay.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    init
}

fn call(relay: &mut Relay, id: Value) -> Value {
    tool(relay, id, "computer_status", json!({}))
}

fn tool(relay: &mut Relay, id: Value, name: &str, arguments: Value) -> Value {
    relay.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}}));
    relay.reply()
}

/// The contract envelope of a tool reply.
fn envelope(reply: &Value) -> &Value {
    &reply["result"]["structuredContent"]
}

#[test]
fn selected_relay_answers_locally_and_opens_one_route_on_first_call() {
    let f = Fixture::new("selected");
    let mut relay = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap(), "--computer", COMPUTER], "serve");
    let init = initialize(&mut relay);
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["serverInfo"]["name"].is_string());
    relay.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = relay.reply();
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"computer_status"), "{names:?}");
    relay.send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    assert_eq!(relay.reply(), json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
    assert!(f.invocations().is_empty(), "no ssh before the first tools/call");

    let first = call(&mut relay, json!("call-a"));
    assert_eq!(first["id"], "call-a", "the harness id is restored");
    assert_eq!(first["result"]["content"][0]["text"], "remote selected");
    let second = call(&mut relay, json!(7));
    assert_eq!(second["id"], 7);
    let invocations = f.invocations();
    assert_eq!(invocations.len(), 1, "one route for the session: {invocations:?}");
    let argv = &invocations[0];
    assert!(argv.starts_with("-F /dev/null -T -o BatchMode=yes"), "{argv}");
    assert!(argv.contains(&format!("-o HostKeyAlias={ENDPOINT}")), "{argv}");
    assert!(argv.ends_with("-p 2222 -l ibara-agent tulip1 mcp"), "{argv}");
    assert_eq!(relay.finish(), 0);
}

#[test]
fn a_failed_route_is_a_session_unavailable_tool_result_and_the_next_call_retries() {
    let f = Fixture::new("fail");
    let mut relay = f.spawn(&["mcp", "--computer", COMPUTER, "--directory-db", f.db.to_str().unwrap()], "fail");
    initialize(&mut relay);
    let failed = call(&mut relay, json!(10));
    assert_eq!(failed["id"], 10);
    assert_eq!(failed["result"]["isError"], true);
    let text = failed["result"].to_string();
    assert!(text.contains("SESSION_UNAVAILABLE"), "{text}");
    assert!(text.contains("Connection refused"), "the ssh diagnostic is named: {text}");
    assert_eq!(f.invocations().len(), 1);
    let again = call(&mut relay, json!(11));
    assert_eq!(again["id"], 11);
    assert_eq!(f.invocations().len(), 2, "a later call tries the route again");
    assert_eq!(relay.finish(), 0);
}

#[test]
fn fleet_status_without_computers_answers_here_without_creating_a_directory() {
    let f = Fixture::fleet("empty");
    let absent = f.root.join("elsewhere/operator.sqlite");
    let mut relay = f.spawn(&["mcp", "--directory-db", absent.to_str().unwrap()], "serve");
    initialize(&mut relay);
    let reply = call(&mut relay, json!(1));
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(envelope(&reply)["status"], "ok");
    assert_eq!(envelope(&reply)["result"]["computers"], json!([]));
    let begin = tool(&mut relay, json!(2), "computer_begin", json!({"goal": "g", "request_id": "r1"}));
    assert_eq!(envelope(&begin)["error"]["code"], "SESSION_UNAVAILABLE", "{begin}");
    assert!(f.invocations().is_empty(), "nothing to reach");
    assert!(!absent.exists(), "a status check does not create the directory");
    assert!(f.onboarding().is_none());
    assert_eq!(relay.finish(), 0);
}

#[test]
fn fleet_status_lists_every_computer_and_an_unreachable_one_as_offline() {
    let f = Fixture::fleet("status");
    let mut relay = f.spawn_with(&["mcp", "--directory-db", f.db.to_str().unwrap()], "serve", "tulip0");
    initialize(&mut relay);
    let reply = call(&mut relay, json!("s"));
    assert_eq!(reply["id"], "s");
    assert_eq!(reply["result"]["isError"], false, "one computer down does not fail the fleet: {reply}");
    let computers = envelope(&reply)["result"]["computers"].as_array().unwrap().clone();
    let row = |id: &str| computers.iter().find(|c| c["id"] == id).cloned().unwrap_or(Value::Null);
    assert_eq!(row("cmp_tulip1")["state"], "ready", "Tulip1 as it describes itself: {computers:?}");
    let down = row("cmp_5a1e0c7b9d2f4e6a8b3c1d0e");
    assert_eq!((down["name"].as_str(), down["state"].as_str()), (Some("Tulip0"), Some("offline")), "{computers:?}");
    assert!(down["capabilities"].as_str().unwrap().contains("No route to host"), "{down}");
    assert_eq!(envelope(&reply)["since"], json!(["Tulip1: tulip1 woke"]));
    let mut hosts = f.invocations();
    hosts.sort();
    assert_eq!(hosts, ["tulip0", "tulip1"], "every computer is asked");
    assert_eq!(relay.finish(), 0);
}

#[test]
fn fleet_begin_picks_the_named_computer_and_later_calls_follow_the_task() {
    let f = Fixture::fleet("begin");
    let mut relay = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap()], "serve");
    initialize(&mut relay);
    let unknown = tool(&mut relay, json!(2), "computer_begin", json!({"computer": "hazel", "goal": "g", "request_id": "r1"}));
    assert_eq!(envelope(&unknown)["error"]["code"], "INVALID_ARGUMENT", "{unknown}");
    assert!(f.invocations().is_empty(), "a refused begin reaches no computer");
    assert!(f.onboarding().is_none(), "no task has begun");

    let began = tool(&mut relay, json!(3), "computer_begin", json!({"computer": "tulip0", "goal": "g", "request_id": "r2"}));
    assert_eq!(envelope(&began)["result"]["task_ref"], "task_tulip0_1", "{began}");
    assert_eq!(f.onboarding().map(|o| o["first_task_done"].clone()), Some(json!(true)), "the first task is recorded");
    let observed = tool(&mut relay, json!(4), "computer_observe", json!({"task_ref": "task_tulip0_1"}));
    assert_eq!(envelope(&observed)["result"]["on"], "tulip0", "{observed}");
    assert_eq!(f.invocations(), ["tulip0"], "only the chosen computer is reached");
    let lines = f.route_lines();
    let begin_line = lines.iter().find(|l| l.contains("computer_begin")).unwrap();
    assert!(begin_line.contains(r#""computer":"cmp_5a1e0c7b9d2f4e6a8b3c1d0e""#), "the computer goes on as its id: {begin_line}");
    assert_eq!(relay.finish(), 0);
}

#[test]
fn fleet_finds_the_computer_of_a_task_begun_in_another_session() {
    let f = Fixture::fleet("owner");
    let mut relay = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap()], "serve");
    initialize(&mut relay);
    let observed = tool(&mut relay, json!(1), "computer_observe", json!({"task_ref": "task_tulip1_1"}));
    assert_eq!(envelope(&observed)["result"]["on"], "tulip1", "{observed}");
    let again = tool(&mut relay, json!(2), "computer_observe", json!({"task_ref": "task_tulip1_1"}));
    assert_eq!(envelope(&again)["result"]["on"], "tulip1");
    let lines = f.route_lines();
    let observes: Vec<&String> = lines.iter().filter(|l| l.contains("computer_observe")).collect();
    assert!(observes.len() == 2 && observes.iter().all(|l| l.starts_with("tulip1 ")), "the call runs only on its owner: {lines:?}");
    assert_eq!(lines.iter().filter(|l| l.contains(r#""ref":"task_tulip1_1""#)).count(), 2, "asked each computer once, then remembered");
    let gone = tool(&mut relay, json!(3), "computer_observe", json!({"task_ref": "task_nowhere_9"}));
    assert_eq!(envelope(&gone)["error"]["code"], "INVALID_ARGUMENT", "{gone}");
    assert_eq!(relay.finish(), 0);
}

#[test]
fn agents_call_each_computer_by_the_name_this_console_gave_it() {
    let f = Fixture::fleet("renamed");
    let rename = |label: &str| {
        let mut directory = OperatorDirectory::open(&f.db).unwrap();
        directory.rename_computer(COMPUTER0, label).unwrap();
        directory.close();
    };
    let situation = |reply: &Value| envelope(reply)["situation"].as_str().unwrap_or_default().to_string();
    let text = |reply: &Value| reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    rename("Desk");
    let mut relay = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap()], "serve");
    initialize(&mut relay);
    let status = call(&mut relay, json!(1));
    let mut names: Vec<String> = envelope(&status)["result"]["computers"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    assert_eq!(names, ["Desk", "Tulip1"], "the fleet list uses this console's names: {status}");
    assert!(situation(&status).contains("Desk ready") && !situation(&status).contains("tulip0"), "{status}");

    let began = tool(&mut relay, json!(2), "computer_begin", json!({"computer": "Desk", "goal": "g", "request_id": "r1"}));
    assert!(situation(&began).starts_with("Desk · you (codex) control"), "{began}");
    assert_eq!(envelope(&began)["result"]["computer"]["name"], "Desk", "{began}");
    assert!(text(&began).starts_with("Desk · "), "an agent reading only the text sees the name too: {began}");
    let computer = tool(&mut relay, json!(3), "computer_status", json!({"ref": "cmp_5a1e0c7b9d2f4e6a8b3c1d0e"}));
    assert!(envelope(&computer)["result"]["summary"].as_str().unwrap().starts_with("Desk · nobody controls"), "{computer}");
    assert_eq!(envelope(&computer)["result"]["next"], json!(["computer_begin({computer: \"Desk\", goal, request_id})"]), "{computer}");

    rename("Study");
    let observed = tool(&mut relay, json!(4), "computer_observe", json!({"task_ref": "task_tulip0_1"}));
    assert!(situation(&observed).starts_with("Study · "), "a rename reaches the next answer: {observed}");
    assert_eq!(relay.finish(), 0);

    let mut pinned = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap(), "--computer", COMPUTER0], "serve");
    initialize(&mut pinned);
    let status = call(&mut pinned, json!(1));
    assert!(situation(&status).starts_with("Study · "), "{status}");
    assert_eq!(envelope(&status)["result"]["computers"][0]["name"], "Study", "{status}");
    rename("Den");
    let status = call(&mut pinned, json!(2));
    assert!(situation(&status).starts_with("Den · "), "a pinned session sees a rename too: {status}");
    assert_eq!(pinned.finish(), 0);
}

#[test]
fn unknown_computer_fails_at_startup_without_ssh() {
    let f = Fixture::new("unknown");
    let relay = f.spawn(&["mcp", "--directory-db", f.db.to_str().unwrap(), "--computer", "computer_absent"], "serve");
    assert_eq!(relay.finish(), 2);
    assert!(f.invocations().is_empty());
}

/// Failure cases this must catch:
/// 1. A command takes only the directory's `computer_` id, so the `cmp_` id and
///    the name an agent sees (computer_status, computer_begin) reach nothing.
/// 2. A name reaches the wrong computer: case decides, or only the full host works.
/// 3. An unknown name, or a name two computers share, still picks one, or the
///    refusal does not say which names and ids would work.
#[test]
fn every_command_takes_a_computer_by_name_host_or_either_id() {
    const DESK_HOST: &str = "tulip0.tail1234.ts.net";
    let f = Fixture::with("names", FAKE_SSH, &[(COMPUTER0, ENDPOINT0, "Desk", DESK_HOST), (COMPUTER, ENDPOINT, "Tulip1", "tulip1")]);
    let db = f.db.to_str().unwrap();
    let last_route = || f.invocations().last().cloned().unwrap_or_default();
    for name in ["Desk", "desk", DESK_HOST, "TULIP0", "cmp_5a1e0c7b9d2f4e6a8b3c1d0e", COMPUTER0] {
        let (code, out, err) = f.run(&["client", "--directory-db", db, "--computer", name, "stat", "artifact_x"]);
        assert_eq!(code, 0, "{name}: {err}");
        assert!(out.contains("\"artifact_ref\": \"artifact_x\""), "{name}: {out}");
        assert!(last_route().ends_with(&format!("{DESK_HOST} transfer-v1")), "{name} reaches Desk: {}", last_route());
    }

    let (code, out, err) = f.run(&["operator", "--directory-db", db, "--computer", "tulip1", "--epoch", "epoch_1", "--op", "status"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(ENDPOINT) && last_route().ends_with("tulip1 operator-v1"), "{out} {}", last_route());
    let mut pinned = f.spawn(&["mcp", "--directory-db", db, "--computer", "cmp_7c3b2a19e8d4f6015b9a2c4d"], "serve");
    initialize(&mut pinned);
    call(&mut pinned, json!(1));
    assert!(last_route().ends_with("tulip1 mcp"), "{}", last_route());
    assert_eq!(pinned.finish(), 0);
    let routes = f.invocations().len();

    let (code, _, err) = f.run(&["client", "--directory-db", db, "--computer", "hazel", "stat", "artifact_x"]);
    assert_eq!(code, 1);
    let desk = "\"Desk\" (cmp_5a1e0c7b9d2f4e6a8b3c1d0e, computer_5a1e0c7b9d2f4e6a8b3c1d0e)";
    let tulip1 = "\"Tulip1\" (cmp_7c3b2a19e8d4f6015b9a2c4d, computer_7c3b2a19e8d4f6015b9a2c4d)";
    assert!(err.contains("\"hazel\" is not one of your computers") && err.contains(desk) && err.contains(tulip1), "{err}");

    let (code, out, err) = f.run(&["client", "--directory-db", db, "--rename-computer", "tulip1", "desk"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(COMPUTER) && out.contains("\"label\":\"desk\""), "{out}");
    for command in [
        vec!["client", "--directory-db", db, "--computer", "Desk", "stat", "artifact_x"],
        vec!["operator", "--directory-db", db, "--computer", "DESK", "--epoch", "epoch_1", "--op", "status"],
    ] {
        let (code, _, err) = f.run(&command);
        assert_eq!(code, 1, "{command:?}");
        let tulip1 = "\"desk\" (cmp_7c3b2a19e8d4f6015b9a2c4d, computer_7c3b2a19e8d4f6015b9a2c4d)";
        assert!(err.contains("names more than one computer") && err.contains(desk) && err.contains(tulip1), "{err}");
    }
    let (code, _, _) = f.run(&["mcp", "--directory-db", db, "--computer", "desk"]);
    assert_eq!(code, 2, "a pinned session names one computer");
    assert_eq!(f.invocations().len(), routes, "no refused name reaches a computer");
}

#[test]
fn preview_service_keeps_a_route_through_refusals_and_drops_it_on_a_wrong_epoch() {
    let f = Fixture::new("serve");
    let mut serve = f.spawn(&["operator", "--serve", "--directory-db", f.db.to_str().unwrap()], "serve");
    let mut observe = |id: &str, epoch: &str, display: &str| {
        serve.send(json!({"id": id, "computer": COMPUTER, "epoch": epoch, "display": display, "quality": "tile"}));
        serve.reply()
    };
    let first = observe("1", "epoch_1", "DP-1");
    assert_eq!((first["id"].as_str(), first["ok"].as_bool()), (Some("1"), Some(true)), "{first}");
    assert_eq!(first["reply"]["record_id"], "DP-1");
    let refused = observe("2", "epoch_1", "refuse");
    assert_eq!(refused["error"], "BUSY: one capture at a time");
    assert_eq!(observe("3", "epoch_1", "DP-1")["ok"], true);
    assert_eq!(f.invocations().len(), 1, "a refusal keeps the route");
    let stale = observe("4", "epoch_2", "DP-1");
    assert_eq!(stale["error"], "Selected operator response identity, epoch or grant generation changed.");
    assert_eq!(observe("5", "epoch_1", "DP-1")["ok"], true);
    let invocations = f.invocations();
    assert_eq!(invocations.len(), 2, "a wrong-identity reply closes the route: {invocations:?}");
    assert!(invocations[1].ends_with("-l ibara-op-vesper tulip1 operator-v1"), "{}", invocations[1]);
    assert_eq!(serve.finish(), 0);
}

// Failure cases written before auto-routing: omitted computer rejected; all clients
// select first; busy race strands caller; uncertain send acquires twice; request
// replay changes target after restart; changed args silently reuse a binding;
// explicit targets migrate; one session cannot work on two separate computers.
#[test]
fn fleet_auto_begin_uses_free_targets_and_retains_request_route() {
    let f = Fixture::fleet("auto");
    let db=f.db.to_str().unwrap();
    let mut relay=f.spawn_with(&["mcp","--directory-db",db],"serve","tulip0");
    initialize(&mut relay);
    let args=json!({"goal":"any free computer","request_id":"auto-free"});
    let begun=tool(&mut relay,json!(2),"computer_begin",args.clone());
    assert_eq!(envelope(&begun)["result"]["task_ref"],"task_tulip1_1","{begun}");
    let second=tool(&mut relay,json!(3),"computer_begin",json!({"computer":"tulip1","goal":"explicit","request_id":"explicit-other"}));
    assert_eq!(envelope(&second)["result"]["task_ref"],"task_tulip1_1");
    assert_eq!(relay.finish(),0);
    // A new relay sees both ready; original request must stay on Tulip1.
    let mut relay=f.spawn(&["mcp","--directory-db",db],"serve"); initialize(&mut relay);
    let replay=tool(&mut relay,json!(2),"computer_begin",args.clone());
    assert_eq!(envelope(&replay)["result"]["task_ref"],"task_tulip1_1","{replay}");
    let mut upgraded=f.spawn(&["mcp","--directory-db",db],"serve");
    upgraded.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"codex","version":"2.0"}}}));
    upgraded.reply(); upgraded.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let replay2=tool(&mut upgraded,json!(2),"computer_begin",serde_json::from_str(r#"{"request_id":"auto-free","goal":"any free computer"}"#).unwrap());
    assert_eq!(envelope(&replay2)["result"]["task_ref"],"task_tulip1_1","client upgrade/reordered args changed route: {replay2}");
    assert_eq!(upgraded.finish(),0);
    let conflict=tool(&mut relay,json!(3),"computer_begin",json!({"goal":"different","request_id":"auto-free"}));
    assert_eq!(envelope(&conflict)["error"]["code"],"REQUEST_CONFLICT","{conflict}");
    assert_eq!(relay.finish(),0);
}

#[test]
fn fleet_auto_begin_falls_back_only_after_definite_refusal() {
    // Every target advertises ready; Tulip0 loses the acquisition race.
    for (tag,code,want) in [("busy","BUSY","ok"),("uncertain","OUTCOME_UNKNOWN","error")] {
        let mut script=FLEET_SSH.to_string();
        let needle="  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\"";
        let at=script.find(needle).unwrap();
        script.insert_str(at,&format!(r#"  case "$line" in
    *'"name":"computer_begin"'*) if [ "$host" = tulip0 ]; then env='{{"status":"error","error":{{"code":"{code}","message":"fixture refusal"}},"result":null}}'; fi ;;
  esac
"#));
        let f=Fixture::with(tag,&script,&[(COMPUTER0,ENDPOINT0,"Tulip0","tulip0"),(COMPUTER,ENDPOINT,"Tulip1","tulip1")]);
        let mut relay=f.spawn(&["mcp","--directory-db",f.db.to_str().unwrap()],"serve"); initialize(&mut relay);
        // Keep trying fresh request identities until the fair selector chooses Tulip0.
        let mut saw=false;
        for n in 0..32 {
            let reply=tool(&mut relay,json!(n+2),"computer_begin",json!({"goal":"race","request_id":format!("race-{n}")}));
            let lines=f.route_lines();
            if lines.iter().any(|l|l.starts_with("tulip0 ") && l.contains("computer_begin")) {
                assert_eq!(envelope(&reply)["status"],want,"{reply}");
                if code=="OUTCOME_UNKNOWN" {
                    let id=format!("race-{n}");
                    assert!(!lines.iter().any(|l|l.starts_with("tulip1 ") && l.contains("computer_begin") && l.contains(&id)),"uncertain request moved");
                }
                saw=true; break;
            }
        }
        assert!(saw,"selection never used Tulip0");
        assert_eq!(relay.finish(),0);
    }
}
