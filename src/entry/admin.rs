//! `ibara admin …`: the administrator CLI, a port of `computerctl.ts`.
//! Lifecycle, deploy, provisioning and the console parse its output, so
//! the argument grammar, the usage text, the pretty `{"result":…}` output
//! and the exit codes are unchanged. The installed
//! `/usr/local/bin/computerctl` wrapper execs this.

use super::{block_on, read_line, write_line};
use crate::server::authority::{charset, is_hex_lower, truthy, valid_challenge_ref, valid_principal};
use crate::server::env_nonempty;
use crate::server::policy::{js_trim, read_trimmed};
use serde_json::{Map, Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::BufReader;

const ALLOWED: &[&str] = &[
    "status", "doctor", "pause", "resume", "tasks", "task", "receipts", "operation", "artifacts", "artifact", "procedures",
    "procedure", "telemetry", "logs", "extend", "revoke", "approve_procedure", "quarantine_procedure", "transfer",
    "transfer-session", "grants", "grant", "revoke_grant", "share_task", "operator_access", "endpoint", "outputs",
    "enroll_operator", "activate_operator", "revoke_operator", "register_collector", "reconcile_operation", "amend_delivery",
    "attention", "answer_attention", "access", "access_sync", "access_set", "access_remove", "access_unpair",
];
const LOG_UNITS: &[&str] = &["controller", "output", "sunshine", "gateway"];
const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_SESSION_LINE: usize = 3 * 1024 * 1024;

pub const USAGE: &str = "Usage: ibara admin [COMMAND] [ARGS]   (computerctl is the same, as root)

Commands:
  status                              Availability, capabilities, lease and control (default)
  doctor                              Health checks; does not inject input or take screenshots
  pause                               Operator pause; revokes the live lease
  resume                              Clear human-control; does not restore a lease
  tasks [PRINCIPAL]                   List tasks, optionally for one principal
  task TASK_REF                       Task detail, recent receipts and artifact refs
  receipts TASK_REF [LIMIT] [CURSOR]  Paginated receipt summaries for a task
  operation REQUEST_OR_OP_REF         Look up a retained operation/receipt
  artifacts [LIMIT] [CURSOR]          Paginated published artifact inventory
  artifact ARTIFACT_REF               One artifact inventory record
  procedures                          Procedure inventory with status and digest
  procedure PROCEDURE_REF             Full procedure candidate/approved content
  telemetry                           Bounded CPU, memory and disk metrics
  logs UNIT [LINES]                   Bounded journal command for a named unit
  extend TASK_REF SECONDS             Add time to a finite task budget; unlimited tasks are unchanged
  revoke [TASK_REF]                   Revoke the live lease, or only if it matches TASK_REF
  grants [GROUP]                      List explicit agent group grants
  grant GROUP PRINCIPAL PEER_KEY      Pair a stable peer identity into a group
  revoke_grant GROUP PRINCIPAL        Revoke one paired principal
  share_task TASK_REF private|GROUP   Keep private or share with one explicit group
  operator_access                     Human operators: active, expiry, watch/files/take-control rights
  endpoint                            Stable controller endpoint identity and protocols
  outputs                             Bounded display inventory for authenticated operator onboarding
  enroll_operator ID                  Enroll named human operator for observation/files (admin only)
  activate_operator ID CHALLENGE_REF REVIEW_DIGEST  Activate only after target-local confirmation
  revoke_operator ID                  Revoke named human operator (admin only)
  register_collector PRINCIPAL HOST_ID  Bind authenticated collector identity; grants no trust
  access                              Identities, pairings and effective grants
  access_set JSON                     Set subject/capability/rule at expected_revision
  access_remove JSON                  Remove grant_id or all grants for subject
  access_unpair JSON                  Remove a pairing; a new key requires re-pairing
  access_sync                         Retry generated SSH transport cleanup
  attention [STATE] [TASK_REF]        Attention items (default open), optionally for one task
  answer_attention ATT_REF ANSWER [ANSWERED_BY]
                                      Answer an attention item; approve, yes, allow or ok approves a held step
  reconcile_operation OP_REF RESOLUTION EVIDENCE_JSON NOTE
                                      Append confirmed|not_occurred|abandoned reconciliation; never replay
  amend_delivery TASK_REF ID EXPECTED_REV HOST_ID ABS_PATH
                                      Amend a delivery obligation after reviewing its revision
  approve_procedure PROCEDURE_REF [SHA256]
                                      Prepare/install a reviewed candidate (root); optional digest binds the reviewed bytes
  quarantine_procedure PROCEDURE_REF  Quarantine a procedure so search does not return it as approved
  transfer JSON                       Operator artifact transfer request for small metadata actions
  transfer-session                    JSON-line operator transfer on stdin/stdout for chunked bytes
  help, --help, -h                    Print this help and exit

Unknown commands and extra or missing arguments are rejected before contacting the controller.
";

/// `^[A-Za-z0-9_.:-]{1,128}$` (task, op, artifact, procedure and group refs).
fn id_ref(s: &str) -> bool {
    charset(s, 1, 128, b"_.:-")
}

/// `^[A-Za-z0-9_.:-]{8,256}$` (host ids and peer keys).
fn host_id(s: &str) -> bool {
    charset(s, 8, 256, b"_.:-")
}

/// JavaScript `Number(s)` as far as `Number.isSafeInteger` can tell.
fn js_number(s: &str) -> Option<f64> {
    let t = js_trim(s);
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).ok().filter(|_| !digits.starts_with(['+', '-'])).map(|n| n as f64);
        }
    }
    if !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')) {
        return None;
    }
    t.parse().ok()
}

/// `Number(s)` when it is a safe integer within `min..=max`.
fn int_in(s: &str, min: i64, max: i64) -> Option<i64> {
    let n = js_number(s)?;
    (n.fract() == 0.0 && n >= min as f64 && n <= max as f64).then_some(n as i64)
}

/// `path.isAbsolute(p) && path.normalize(p) === p`.
fn normal_absolute(p: &str) -> bool {
    let Some(rest) = p.strip_prefix('/') else { return false };
    if rest.is_empty() {
        return true;
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    rest.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn arg(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

/// Validate the arguments and build the action, before any controller contact.
pub fn build_action(operation: &str, args: &[String]) -> Result<Map<String, Value>, String> {
    let mut action = Map::new();
    action.insert("op".into(), json!(operation));
    let mut set = |key: &str, value: Value| {
        action.insert(key.into(), value);
    };
    let usage = |text: &str| Err(format!("Usage: ibara admin {text}"));
    match operation {
        "status" | "doctor" | "pause" | "resume" | "telemetry" | "procedures" | "endpoint" | "outputs" | "operator_access" | "access" | "access_sync" => {
            if !args.is_empty() {
                return usage(operation);
            }
        }
        "access_set" | "access_remove" | "access_unpair" => {
            if args.len()!=1 {return usage("access_set|access_remove|access_unpair JSON");}
            let fields:Map<String,Value>=serde_json::from_str(&args[0]).map_err(|_|"Expected a JSON object.".to_string())?;
            if fields.contains_key("op") {return Err("JSON cannot override op.".into());}
            action.extend(fields);
        }
        "enroll_operator" => {
            if args.len() != 5
                || !valid_principal(arg(args, 0))
                || !is_hex_lower(arg(args, 1), 64)
                || !charset(arg(args, 2), 8, 128, b"_.:-")
                || !charset(arg(args, 3), 8, 128, b"_.:-")
                || !charset(arg(args, 4), 16, 128, b"+/_=.:-")
            {
                return usage("enroll_operator ID REVIEW_DIGEST OPERATOR_ENDPOINT TARGET_ENDPOINT KEY_FINGERPRINT");
            }
            set("operator_id", json!(args[0]));
            set("review_digest", json!(args[1]));
            set("operator_endpoint_id", json!(args[2]));
            set("target_endpoint_id", json!(args[3]));
            set("operator_key_fingerprint", json!(args[4]));
            set("observe", json!(true));
            set("files", json!(true));
        }
        "activate_operator" => {
            if args.len() != 3 || !valid_principal(arg(args, 0)) || !valid_challenge_ref(arg(args, 1)) || !is_hex_lower(arg(args, 2), 64) {
                return usage("activate_operator ID CHALLENGE_REF REVIEW_DIGEST");
            }
            set("operator_id", json!(args[0]));
            set("challenge_ref", json!(args[1]));
            set("review_digest", json!(args[2]));
        }
        "revoke_operator" => {
            if args.len() != 1 || !valid_principal(arg(args, 0)) {
                return usage("revoke_operator ID");
            }
            set("operator_id", json!(args[0]));
        }
        "register_collector" => {
            if args.len() != 2 || !valid_principal(arg(args, 0)) || !host_id(arg(args, 1)) {
                return usage("register_collector PRINCIPAL HOST_ID");
            }
            set("principal", json!(args[0]));
            set("host_id", json!(args[1]));
        }
        "reconcile_operation" => {
            let note = arg(args, 3);
            if args.len() != 4
                || !id_ref(arg(args, 0))
                || !["confirmed", "not_occurred", "abandoned"].contains(&arg(args, 1))
                || note.is_empty()
                || note.encode_utf16().count() > 1000
            {
                return usage("reconcile_operation OP_REF RESOLUTION EVIDENCE_JSON NOTE");
            }
            let refs = serde_json::from_str::<Value>(&args[2]).ok().filter(|refs| {
                refs.as_array()
                    .is_some_and(|list| list.len() <= 30 && list.iter().all(|r| r.as_str().is_some_and(id_ref)))
            });
            let Some(refs) = refs else {
                return Err("EVIDENCE_JSON must be a JSON array of retained evidence references.".into());
            };
            set("operation_ref", json!(args[0]));
            set("resolution", json!(args[1]));
            set("evidence_refs", refs);
            set("note", json!(args[3]));
        }
        "amend_delivery" => {
            let revision = int_in(arg(args, 2), 1, 9_007_199_254_740_991);
            let (5, true, true, Some(revision), true, true) = (
                args.len(),
                id_ref(arg(args, 0)),
                id_ref(arg(args, 1)),
                revision,
                host_id(arg(args, 3)),
                normal_absolute(arg(args, 4)),
            ) else {
                return usage("amend_delivery TASK_REF ID EXPECTED_REV HOST_ID ABS_PATH");
            };
            set("task_ref", json!(args[0]));
            set("obligation_id", json!(args[1]));
            set("expected_revision", json!(revision));
            set("host_id", json!(args[3]));
            set("destination_path", json!(args[4]));
        }
        "tasks" => {
            if args.len() > 1 {
                return usage("tasks [PRINCIPAL]");
            }
            if !arg(args, 0).is_empty() {
                set("principal", json!(args[0]));
            }
        }
        "task" => {
            if args.len() != 1 || !id_ref(arg(args, 0)) {
                return usage("task TASK_REF");
            }
            set("task_ref", json!(args[0]));
        }
        "receipts" => {
            let text = "receipts TASK_REF [LIMIT] [CURSOR]";
            if args.is_empty() || args.len() > 3 || !id_ref(arg(args, 0)) {
                return usage(text);
            }
            set("task_ref", json!(args[0]));
            if !arg(args, 1).is_empty() {
                let Some(limit) = int_in(&args[1], 1, 100) else { return usage(text) };
                set("limit", json!(limit));
            }
            if !arg(args, 2).is_empty() {
                if !id_ref(&args[2]) {
                    return usage(text);
                }
                set("cursor", json!(args[2]));
            }
        }
        "operation" => {
            if args.len() != 1 || !id_ref(arg(args, 0)) {
                return usage("operation REQUEST_OR_OP_REF");
            }
            set("request_id", json!(args[0]));
        }
        "artifacts" => {
            let text = "artifacts [LIMIT] [CURSOR]";
            if args.len() > 2 {
                return usage(text);
            }
            if !arg(args, 0).is_empty() {
                let Some(limit) = int_in(&args[0], 1, 100) else { return usage(text) };
                set("limit", json!(limit));
            }
            if !arg(args, 1).is_empty() {
                if !id_ref(&args[1]) {
                    return usage(text);
                }
                set("cursor", json!(args[1]));
            }
        }
        "artifact" => {
            if args.len() != 1 || !id_ref(arg(args, 0)) {
                return usage("artifact ARTIFACT_REF");
            }
            set("artifact_ref", json!(args[0]));
        }
        "procedure" => {
            if args.len() != 1 || !id_ref(arg(args, 0)) {
                return usage("procedure PROCEDURE_REF");
            }
            set("procedure_ref", json!(args[0]));
        }
        "logs" => {
            let text = "logs UNIT [LINES]";
            if args.is_empty() || args.len() > 2 || !LOG_UNITS.contains(&arg(args, 0)) {
                return usage(text);
            }
            set("unit", json!(args[0]));
            if !arg(args, 1).is_empty() {
                let Some(lines) = int_in(&args[1], 1, 200) else { return usage(text) };
                set("lines", json!(lines));
            }
        }
        "extend" => {
            let seconds = int_in(arg(args, 1), 1, 86_400);
            let (2, false, Some(seconds)) = (args.len(), arg(args, 0).is_empty(), seconds) else {
                return usage("extend TASK_REF SECONDS (1–86400)");
            };
            set("task_ref", json!(args[0]));
            set("extra_seconds", json!(seconds));
        }
        "revoke" => {
            if args.len() > 1 {
                return usage("revoke [TASK_REF]");
            }
            if !arg(args, 0).is_empty() {
                set("task_ref", json!(args[0]));
            }
        }
        "approve_procedure" => {
            let text = "approve_procedure PROCEDURE_REF [SHA256]";
            if args.is_empty() || args.len() > 2 || !id_ref(arg(args, 0)) {
                return usage(text);
            }
            set("procedure_ref", json!(args[0]));
            if !arg(args, 1).is_empty() {
                if !is_hex_lower(&args[1], 64) {
                    return usage(text);
                }
                set("expected_sha256", json!(args[1]));
            }
        }
        "quarantine_procedure" => {
            if args.len() != 1 || !id_ref(arg(args, 0)) {
                return usage("quarantine_procedure PROCEDURE_REF");
            }
            set("procedure_ref", json!(args[0]));
        }
        "grants" => {
            if args.len() > 1 || (!arg(args, 0).is_empty() && !id_ref(&args[0])) {
                return usage("grants [GROUP]");
            }
            if !arg(args, 0).is_empty() {
                set("group", json!(args[0]));
            }
        }
        "grant" => {
            if args.len() != 3 || !id_ref(arg(args, 0)) || !valid_principal(arg(args, 1)) || !host_id(arg(args, 2)) {
                return usage("grant GROUP PRINCIPAL PEER_KEY");
            }
            set("group", json!(args[0]));
            set("principal", json!(args[1]));
            set("peer_key", json!(args[2]));
        }
        "revoke_grant" => {
            if args.len() != 2 || !id_ref(arg(args, 0)) || !valid_principal(arg(args, 1)) {
                return usage("revoke_grant GROUP PRINCIPAL");
            }
            set("group", json!(args[0]));
            set("principal", json!(args[1]));
        }
        "share_task" => {
            if args.len() != 2 || !id_ref(arg(args, 0)) || (args[1] != "private" && !id_ref(&args[1])) {
                return usage("share_task TASK_REF private|GROUP");
            }
            set("task_ref", json!(args[0]));
            if args[1] == "private" {
                set("visibility", json!("private"));
            } else {
                set("visibility", json!("shared"));
                set("group", json!(args[1]));
            }
        }
        "attention" => {
            if args.len() > 2 || args.iter().any(|a| !id_ref(a)) {
                return usage("attention [STATE] [TASK_REF]");
            }
            if !arg(args, 0).is_empty() {
                set("state", json!(args[0]));
            }
            if !arg(args, 1).is_empty() {
                set("task_ref", json!(args[1]));
            }
        }
        "answer_attention" => {
            let answer = js_trim(arg(args, 1));
            if !(2..=3).contains(&args.len())
                || !id_ref(arg(args, 0))
                || answer.is_empty()
                || answer.encode_utf16().count() > 1000
                || (args.len() == 3 && !id_ref(&args[2]))
            {
                return usage("answer_attention ATT_REF ANSWER [ANSWERED_BY]");
            }
            set("att_ref", json!(args[0]));
            set("answer", json!(answer));
            if args.len() == 3 {
                set("answered_by", json!(args[2]));
            }
        }
        "transfer" => {
            let request = match args {
                [json] => serde_json::from_str::<Value>(json).ok().filter(Value::is_object),
                _ => None,
            };
            let Some(request) = request else { return usage("transfer JSON") };
            set("principal", json!("operator"));
            set("connection_id", json!(crate::ids::id("operator_transfer")));
            set("request", request);
        }
        _ => return Err(format!("Unknown command: {operation}\n{USAGE}")),
    }
    Ok(action)
}

fn admin_socket() -> PathBuf {
    env_nonempty("IBARA_ADMIN_SOCKET").unwrap_or_else(|| "/run/agent-computer/admin.sock".into()).into()
}

fn admin_key() -> PathBuf {
    env_nonempty("IBARA_ADMIN_KEY").unwrap_or_else(|| "/etc/agent-computer/admin.key".into()).into()
}

/// `{kind:"admin", action}` to `admin.sock` with the admin bearer, read per request.
async fn admin_request(action: Value) -> Result<Value, String> {
    let key = admin_key();
    let token = read_trimmed(&key).map_err(|e| format!("Cannot read {}: {e}", key.display()))?;
    crate::http::post(&admin_socket(), Some(&token), &json!({ "kind": "admin", "action": action }), TIMEOUT)
        .await
        .map_err(|e| e.to_string())
}

/// `result.error || result.result?.status === 'error'`.
fn failed(result: &Value) -> bool {
    truthy(result.get("error")) || result.get("result").and_then(|r| r.get("status")).and_then(Value::as_str) == Some("error")
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn fail(message: &str) -> i32 {
    eprintln!("{message}");
    1
}

/// The station desktop user's home, from the root-owned `/etc/ibara/station.json`
/// (`agent_account`) and `/etc/passwd`.
fn desktop_home() -> Result<PathBuf, String> {
    let station = Path::new("/etc/ibara/station.json");
    let meta = std::fs::symlink_metadata(station).map_err(|e| format!("{}: {e}", station.display()))?;
    if !meta.file_type().is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err("Station descriptor is not a root-owned file.".into());
    }
    let descriptor: Value = std::fs::read(station)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("Station descriptor has no valid agent_account.")?;
    let account = descriptor.get("agent_account").and_then(Value::as_str).unwrap_or("");
    let valid = !account.is_empty()
        && account.len() <= 32
        && (account.as_bytes()[0].is_ascii_lowercase() || account.as_bytes()[0] == b'_')
        && account.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !valid {
        return Err("Station descriptor has no valid agent_account.".into());
    }
    let passwd = String::from_utf8_lossy(&std::fs::read("/etc/passwd").unwrap_or_default()).into_owned();
    let prefix = format!("{account}:");
    let fields: Vec<&str> = passwd.split('\n').find(|l| l.starts_with(&prefix)).map(|l| l.split(':').collect()).unwrap_or_default();
    let home = fields.get(5).copied().unwrap_or("");
    if fields.len() != 7 || !normal_absolute(home) || home == "/" {
        return Err(format!("Station account {account} has no home directory."));
    }
    Ok(PathBuf::from(home))
}

/// The root side of `approve_procedure`: install the reviewed candidate into
/// the approved directory, from the desktop user's state.
fn install_approved(procedure_ref: &str) -> i32 {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        return fail("Procedure installation requires the operator root command.");
    }
    let state = env_nonempty("IBARA_STATE_DIR");
    let data = env_nonempty("IBARA_DATA_DIR");
    let home = if state.is_none() || data.is_none() {
        match desktop_home() {
            Ok(home) => home,
            Err(message) => return fail(&message),
        }
    } else {
        PathBuf::new()
    };
    let state = state.map(PathBuf::from).unwrap_or_else(|| home.join(".local/state/agent-computer"));
    let data = data.map(PathBuf::from).unwrap_or_else(|| home.join(".local/share/agent-computer"));
    let approved = env_nonempty("IBARA_PROCEDURES_DIR").unwrap_or_else(|| "/opt/agent-computer/procedures-approved".into());
    let installed = crate::storage::procedures::install_approved_procedure(
        &state.join("pending-promotions").join(format!("{procedure_ref}.json")),
        Path::new(&approved),
        &data.join("candidates"),
    );
    match installed {
        Ok(installed) => {
            println!("{}", pretty(&installed));
            0
        }
        Err(e) => fail(&e.to_string()),
    }
}

async fn execute(operation: &str, action: Map<String, Value>) -> i32 {
    let connection_id = action.get("connection_id").cloned();
    let result = admin_request(Value::Object(action.clone())).await;
    if operation == "transfer" {
        let end = json!({ "op": "transfer", "principal": "operator", "connection_id": connection_id, "request": { "kind": "end_transfer" } });
        let _ = admin_request(end).await;
    }
    let result = match result {
        Ok(result) => result,
        Err(message) => return fail(&message),
    };
    if failed(&result) {
        println!("{}", pretty(&result));
        return 1;
    }
    if operation == "approve_procedure" {
        return install_approved(action.get("procedure_ref").and_then(Value::as_str).unwrap_or(""));
    }
    println!("{}", pretty(&result));
    0
}

fn error_line(code: &str, message: &str) -> String {
    json!({ "error": { "code": code, "message": message } }).to_string()
}

/// `transfer-session`: each stdin line (≤3 MiB, a JSON object) is one admin
/// `transfer`; each reply is printed as one line; `end_transfer` at the end.
async fn transfer_session() -> i32 {
    let connection_id = crate::ids::id("operator_transfer");
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut line = Vec::new();
    let mut code = 0;
    loop {
        let reply = match read_line(&mut input, MAX_SESSION_LINE, &mut line).await {
            Ok(None) | Err(_) => break,
            Ok(Some(false)) => {
                let _ = write_line(&mut output, &error_line("INVALID_ARGUMENT", "Transfer request too large.")).await;
                code = 1;
                break;
            }
            Ok(Some(true)) if js_trim(&String::from_utf8_lossy(&line)).is_empty() => continue,
            Ok(Some(true)) => match serde_json::from_slice::<Value>(&line) {
                Err(_) => {
                    code = 1;
                    error_line("INVALID_ARGUMENT", "Transfer request is not JSON.")
                }
                Ok(request) if !request.is_object() => {
                    code = 1;
                    error_line("INVALID_ARGUMENT", "Transfer request must be an object.")
                }
                Ok(request) => {
                    let action = json!({ "op": "transfer", "principal": "operator", "connection_id": connection_id, "request": request });
                    match admin_request(action).await {
                        Ok(result) => {
                            if failed(&result) {
                                code = 1;
                            }
                            result.to_string()
                        }
                        Err(message) => {
                            code = 1;
                            error_line("SESSION_UNAVAILABLE", &message)
                        }
                    }
                }
            },
        };
        if write_line(&mut output, &reply).await.is_err() {
            code = 1;
            break;
        }
    }
    let end = json!({ "op": "transfer", "principal": "operator", "connection_id": connection_id, "request": { "kind": "end_transfer" } });
    let _ = admin_request(end).await;
    code
}

pub fn main(argv: Vec<String>) -> i32 {
    let raw = argv.first().map(String::as_str);
    if matches!(raw, Some("help" | "--help" | "-h")) {
        if argv.len() > 1 {
            return fail("Usage: ibara admin help");
        }
        print!("{USAGE}");
        return 0;
    }
    let operation = raw.unwrap_or("status");
    if operation == "transfer-session" {
        if argv.len() != 1 {
            return fail("Usage: ibara admin transfer-session");
        }
        return block_on(transfer_session());
    }
    if !ALLOWED.contains(&operation) {
        return fail(&format!("Unknown command: {operation}\n{USAGE}"));
    }
    let args = if raw.is_none() { &[][..] } else { &argv[1..] };
    match build_action(operation, args) {
        Ok(action) => block_on(execute(operation, action)),
        Err(message) => fail(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bad_arguments_are_refused_before_any_contact() {
        let digest = "a".repeat(64);
        let refused: &[(&str, &[&str])] = &[
            ("status", &["extra"]),
            ("enroll_operator", &["vesper", "ABC", "host_12345678", "ibara_12345678", "SHA256:abcdefghijklmnop"]),
            ("enroll_operator", &["Vesper", &digest, "host_12345678", "ibara_12345678", "SHA256:abcdefghijklmnop"]),
            ("activate_operator", &["vesper", "pair_123", &digest]),
            ("receipts", &["task_1", "101"]),
            ("receipts", &["task_1", "1.5"]),
            ("artifacts", &["0"]),
            ("logs", &["sshd"]),
            ("logs", &["controller", "201"]),
            ("extend", &["task_1", "86401"]),
            ("extend", &["", "5"]),
            ("amend_delivery", &["task_1", "d1", "1", "host_12345678", "/tmp/../etc/passwd"]),
            ("amend_delivery", &["task_1", "d1", "1", "host_12345678", "relative/path"]),
            ("amend_delivery", &["task_1", "d1", "0", "host_12345678", "/tmp/x"]),
            ("reconcile_operation", &["op_1", "maybe", "[]", "note"]),
            ("reconcile_operation", &["op_1", "confirmed", "[]", ""]),
            ("share_task", &["task_1", "has space"]),
            ("transfer", &["[1]"]),
            ("approve_procedure", &["proc_1", "notahash"]),
            ("attention", &["open", "task 1"]),
            ("answer_attention", &["att_1"]),
            ("answer_attention", &["att_1", "  "]),
            ("answer_attention", &["att_1", "approve", "by someone"]),
        ];
        for (op, args) in refused {
            let err = build_action(op, &strings(args)).unwrap_err();
            assert!(err.starts_with("Usage: ibara admin"), "{op} {args:?}: {err}");
        }
        let err = build_action("reconcile_operation", &strings(&["op_1", "confirmed", "{}", "note"])).unwrap_err();
        assert_eq!(err, "EVIDENCE_JSON must be a JSON array of retained evidence references.");
    }

    #[test]
    fn actions_keep_the_typescript_shape() {
        let digest = "a".repeat(64);
        let action =
            build_action("enroll_operator", &strings(&["vesper", &digest, "host_12345678", "ibara_12345678", "SHA256:abcdefghijklmnop"]))
                .unwrap();
        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            format!(
                r#"{{"op":"enroll_operator","operator_id":"vesper","review_digest":"{digest}","operator_endpoint_id":"host_12345678","target_endpoint_id":"ibara_12345678","operator_key_fingerprint":"SHA256:abcdefghijklmnop","observe":true,"files":true}}"#
            )
        );
        let action = build_action("receipts", &strings(&["task_1", " 0x10 ", ""])).unwrap();
        assert_eq!(Value::Object(action), json!({ "op": "receipts", "task_ref": "task_1", "limit": 16 }));
        let action = build_action("share_task", &strings(&["task_1", "team"])).unwrap();
        assert_eq!(Value::Object(action), json!({ "op": "share_task", "task_ref": "task_1", "visibility": "shared", "group": "team" }));
    }
}
