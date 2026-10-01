//! Directory-routed commands: the private directory, the selected operator route
//! for status, tasks, files and previews, control transitions, and the viewer
//! (ibara-bridge.mjs:537-673, 678-770, 815-841).

use super::envelope::{Fault, Handled, clip};
use super::process::which;
use super::viewer::{KEYS_CHORD, ibara_view, identity_dir, launch_ibara_view, launch_viewer, viewer_identity};
use super::{Console, Ctx, clipboard, option, validated_id};
use crate::operator::directory::{ListedComputer, OperatorDirectory};
use crate::operator::{js, pattern};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Control transitions: the target allows 150 s; the bridge waited 180 s.
const CONTROL_DEADLINE: Duration = Duration::from_secs(180);
const TRANSPORT_TIMED_OUT: &str = "Selected operator transport timed out.";

/// The directory rows, or `None` when the directory cannot be read.
fn listed(ctx: &Ctx) -> Option<Vec<ListedComputer>> {
    OperatorDirectory::open(&ctx.console.database).and_then(|d| d.list_computers()).ok()
}

/// A verified row for `computer` whose host is a plain node name.
fn verified_row(ctx: &Ctx, computer: &str) -> Option<ListedComputer> {
    listed(ctx)?.into_iter().find(|r| r.computer_id == computer && r.trust_state == "verified" && pattern::node(&r.host))
}

fn or(message: &str, fallback: &str) -> String {
    if message.is_empty() { fallback.to_string() } else { message.to_string() }
}

/// `data.computer_id === computer && data.controller_epoch === epoch`.
fn binds(data: &Value, computer: &str, epoch: &str) -> bool {
    data.get("computer_id").and_then(Value::as_str) == Some(computer)
        && data.get("controller_epoch").and_then(Value::as_str) == Some(epoch)
}

/// `directory`: every computer in the private directory.
pub fn directory(ctx: &Ctx) -> Handled {
    Ok(match OperatorDirectory::open(&ctx.console.database).and_then(|d| d.list_computers()) {
        Err(error) => {
            ctx.failure("DIRECTORY_UNAVAILABLE", &or(&error.message, "Could not read the private computer directory."), "failed", true)
        }
        Ok(rows) => ctx.ready(json!({"computers": rows.iter().map(ListedComputer::to_json).collect::<Vec<_>>()})),
    })
}

/// A computer name is one plain line: C0, DEL, C1 and U+2028/2029 are refused.
fn name_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}')
}

/// `rename-computer --computer NAME --label TEXT`: the operator's own name for a
/// computer, kept only in this machine's private directory.
pub fn rename_computer(ctx: &Ctx) -> Handled {
    let usage = || Fault::plain("Usage: rename-computer --computer NAME --label TEXT");
    let (mut computer, mut label) = (None, None);
    for pair in ctx.args.chunks(2) {
        let [name, value] = pair else { return Err(usage()) };
        let slot = match name.as_str() {
            "--computer" => &mut computer,
            "--label" => &mut label,
            _ => return Err(usage()),
        };
        if slot.replace(value.as_str()).is_some() {
            return Err(usage());
        }
    }
    let computer = validated_id(computer, "computer_id")?;
    let label = js::trim(label.unwrap_or("")).to_string();
    if label.is_empty() || js::length(&label) > 128 {
        return Err(Fault::plain("A computer name must contain 1–128 characters."));
    }
    if label.chars().any(name_control) {
        return Err(Fault::plain("A computer name cannot contain control characters."));
    }
    let renamed = OperatorDirectory::open(&ctx.console.database).and_then(|mut d| d.rename_computer(&computer, &label));
    let row = match renamed {
        Ok(row) => row,
        Err(error) => {
            return Ok(ctx.failure("RENAME_REFUSED", &or(&error.message, "The computer could not be renamed."), "failed", true));
        }
    };
    if row.computer_id != computer || row.label != label {
        return Ok(ctx.failure("INVALID_RESPONSE", "The directory did not confirm the new name for this computer.", "failed", true));
    }
    Ok(ctx.ready(json!({"computer": row.to_json()})))
}

/// `operator-session --computer ID`: bootstrap the controller epoch.
pub async fn operator_session(ctx: &Ctx) -> Handled {
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let call = ctx.console.sessions.call(&computer, None, "session", Value::Null);
    let data = match tokio::time::timeout(ctx.timeout, call).await {
        Err(_) => return Err(Fault::Timeout(TRANSPORT_TIMED_OUT.into())),
        Ok(Err(error)) => {
            let message = or(&error.message, "Authenticated session discovery failed.");
            return Ok(ctx.failure("OPERATOR_REFUSED", &message, "unauthorized", false));
        }
        Ok(Ok(data)) => data,
    };
    let result = data.get("result");
    let bound = data.get("computer_id").and_then(Value::as_str) == Some(computer.as_str())
        && pattern::id(&js::string_or(data.get("controller_epoch").filter(|e| js::truthy(Some(e))), ""))
        && result.and_then(|r| r.get("endpoint_id")) == data.get("endpoint_id")
        && result.and_then(|r| r.get("controller_epoch")) == data.get("controller_epoch");
    if !bound {
        return Ok(ctx.failure(
            "IDENTITY_MISMATCH",
            "Authenticated session reply did not bind its computer, endpoint and epoch.",
            "unauthorized",
            false,
        ));
    }
    Ok(ctx.ready(data))
}

/// `operator-status`, `operator-task-status` and the read-only `operator-files` ops.
pub async fn operator_read(ctx: &Ctx) -> Handled {
    let command = ctx.head.command.as_str();
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let epoch = validated_id(option(&ctx.args, "--epoch"), "controller_epoch")?;
    let (op, fields) = match command {
        "operator-status" => ("status", Value::Null),
        "operator-task-status" => ("task_status", json!({"task_ref": validated_id(option(&ctx.args, "--task"), "task_ref")?})),
        _ => {
            let op = option(&ctx.args, "--op").unwrap_or("");
            if !["files_roots", "files_list", "files_status"].contains(&op) {
                return Err(Fault::plain("Only bounded read-only file selection is exposed through the panel."));
            }
            let root = option(&ctx.args, "--root").unwrap_or("");
            let directory = option(&ctx.args, "--directory").unwrap_or(".");
            let job = option(&ctx.args, "--job").unwrap_or("");
            if !root.is_empty() && !pattern::id(root) {
                return Err(Fault::plain("Invalid approved root ID."));
            }
            if !job.is_empty() && !pattern::id(job) {
                return Err(Fault::plain("Invalid file job ID."));
            }
            if js::length(directory) > 512 || directory.starts_with('/') || directory.split('/').any(|p| p == ".." || p.is_empty()) {
                return Err(Fault::plain("Invalid remote directory."));
            }
            let fields = match op {
                "files_roots" => json!({}),
                "files_status" => json!({"job_id": job}),
                _ => json!({"root_id": root, "relative_directory": directory}),
            };
            (op, fields)
        }
    };
    let call = ctx.console.sessions.call(&computer, Some(&epoch), op, fields);
    let data = match tokio::time::timeout(ctx.timeout, call).await {
        Err(_) => return Err(Fault::Timeout(TRANSPORT_TIMED_OUT.into())),
        Ok(Err(error)) => {
            let message = or(&error.message, "Selected operator request refused.");
            return Ok(ctx.failure("OPERATOR_REFUSED", &message, "unauthorized", false));
        }
        Ok(Ok(data)) => data,
    };
    if !binds(&data, &computer, &epoch) {
        return Ok(ctx.failure(
            "IDENTITY_MISMATCH",
            "Selected operator reply did not match the requested computer and epoch.",
            "unauthorized",
            false,
        ));
    }
    if op == "status" {
        super::everyday::remember(&ctx.console, &computer, &data["result"]);
    }
    Ok(ctx.ready(data))
}

/// One computer's preview turn. The gate stays in the shared map only while some
/// request holds or awaits it.
struct Turn<'a> {
    ctx: &'a Ctx,
    computer: String,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut gates = self.ctx.console.previews.lock().unwrap_or_else(|p| p.into_inner());
        // The map's reference and ours: nobody else waits for this computer.
        if Arc::strong_count(&self.gate) <= 2 {
            gates.remove(&self.computer);
        }
    }
}

/// `operator-observe`: one preview frame over the computer's long-lived route
/// (the bridge's `serve-previews`). Previews of one computer run one at a time: a
/// request that arrives while another is in flight waits for it, within its own
/// deadline. `--width W --height H` name the size the plugin shows this quality
/// at, in device pixels; the frame file is scaled to fit it. The picture comes
/// as JPEG, and not at all when the screen still matches the last picture of
/// this display at this size (`frames.rs`). The frame is not kept once the
/// envelope is returned.
pub async fn observe(ctx: &Ctx) -> Handled {
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let epoch = validated_id(option(&ctx.args, "--epoch"), "controller_epoch")?;
    let display = validated_id(option(&ctx.args, "--display"), "display_id")?;
    let quality = option(&ctx.args, "--quality").unwrap_or("");
    if !["tile", "selected"].contains(&quality) {
        return Err(Fault::plain("Selected observation quality is invalid."));
    }
    let side = |name| option(&ctx.args, name).map(|v| v.parse::<u32>().ok().filter(|v| super::frames::SHOWN_SIDE.contains(v)));
    let shown = match (side("--width"), side("--height")) {
        (None, None) => None,
        (Some(Some(width)), Some(Some(height))) => Some((width, height)),
        _ => return Err(Fault::plain("Selected observation size is invalid.")),
    };
    let gate = ctx.console.previews.lock().unwrap_or_else(|p| p.into_inner()).entry(computer.clone()).or_default().clone();
    let turn = Turn { ctx, computer: computer.clone(), gate };
    let capture = async {
        let _held = turn.gate.lock().await;
        let sent = ctx.console.frames.shown(&computer, quality, &display, shown);
        let mut fields = json!({"display_id": display, "quality": quality, "format": "jpeg"});
        if let Some(last) = &sent {
            fields["previous"] = json!(last.digest());
        }
        ctx.console.sessions.call(&computer, Some(&epoch), "observe", fields).await.map(|data| (data, sent))
    };
    let answer = tokio::time::timeout(ctx.timeout, capture).await;
    drop(turn);
    Ok(match answer {
        Err(_) => ctx.failure("TIMEOUT", "Preview did not return in time; no frame was delivered.", "offline", true),
        Ok(Err(error)) => ctx.failure("OPERATOR_REFUSED", &or(&error.message, "Selected operator request refused."), "unauthorized", false),
        Ok(Ok((data, _))) if !binds(&data, &computer, &epoch) => ctx.failure(
            "IDENTITY_MISMATCH",
            "Selected operator reply did not match the requested computer and epoch.",
            "unauthorized",
            false,
        ),
        // Watch is Ask First there: a person must approve this sitting first.
        Ok(Ok((data, _))) if data["result"]["state"] == "pending_approval" => {
            let label = verified_row(ctx, &computer).map(|row| row.label).unwrap_or_else(|| "that computer".into());
            let message = format!("CAPABILITY_UNAVAILABLE: Waiting for someone on {label} to approve watching.");
            ctx.failure("CAPABILITY_UNAVAILABLE", &message, "ready", true)
        }
        Ok(Ok((data, sent))) => match ctx.console.frames.store(data, &computer, quality, &display, shown, sent).await {
            Ok(data) => ctx.ready(data),
            Err(message) => ctx.failure("CAPABILITY_UNAVAILABLE", &format!("CAPABILITY_UNAVAILABLE: {message}"), "ready", true),
        },
    })
}

/// `^(none|human|operator:[C]{1,128}|agent:[C]{1,256})$` with C = `[A-Za-z0-9_.:-]`.
fn expected_owner(owner: &str) -> bool {
    let chars = |s: &str, max: usize| (1..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b));
    owner == "none"
        || owner == "human"
        || owner.strip_prefix("operator:").is_some_and(|rest| chars(rest, 128))
        || owner.strip_prefix("agent:").is_some_and(|rest| chars(rest, 256))
}

fn with(data: &Value, extra: &[(&str, Value)]) -> Value {
    let mut out = data.as_object().cloned().unwrap_or_default();
    for (key, value) in extra {
        out.insert((*key).to_string(), value.clone());
    }
    Value::Object(out)
}

/// `operator-control --op take_control|handback|pause|resume`: the control
/// transition over its own route. Before taking control this console's viewer
/// certificate is registered on the computer; with the ticket the computer
/// mints, ibara-view opens and the clipboard is shared until Hand Back, which
/// ends both. A computer from before ibara-view mints no ticket and gets
/// Moonlight. `pause` and `resume` need no owner or revision.
pub async fn selected_control(ctx: &Ctx) -> Handled {
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let epoch = validated_id(option(&ctx.args, "--epoch"), "controller_epoch")?;
    let op = option(&ctx.args, "--op").unwrap_or("");
    let pausing = ["pause", "resume"].contains(&op);
    if !pausing && !["take_control", "handback"].contains(&op) {
        return Err(Fault::plain("Unknown selected control operation."));
    }
    let fields = if pausing {
        Value::Null
    } else {
        let owner = option(&ctx.args, "--owner").unwrap_or("");
        let revision = option(&ctx.args, "--revision").unwrap_or("");
        if !expected_owner(owner) || !pattern::id(revision) {
            return Err(Fault::plain("Current owner and ownership revision are required."));
        }
        json!({"expected_owner": owner, "expected_ownership_revision": revision})
    };
    let view = if op == "take_control" { ibara_view() } else { None };
    if let Some(bin) = &view
        && let Err(refusal) = register_viewer(ctx, &computer, &epoch, bin).await
    {
        return Ok(refusal);
    }
    let call = ctx.console.sessions.call(&computer, Some(&epoch), op, fields);
    let data = match tokio::time::timeout(CONTROL_DEADLINE, call).await {
        Err(_) => return Err(Fault::Timeout(TRANSPORT_TIMED_OUT.into())),
        Ok(Err(error)) if op == "take_control" && view.is_none() && error.message.contains("does not know your viewer") => {
            return Ok(ctx.failure("MISSING_DEPENDENCY", VIEWER_MISSING, "missing-dependency", false));
        }
        Ok(Err(error)) => {
            let message = or(&error.message, "Control outcome is uncertain. Refresh this computer before any retry.");
            return Ok(ctx.failure("CONTROL_UNCERTAIN", &message, "failed", false));
        }
        Ok(Ok(data)) => data,
    };
    let reply = data.get("result").cloned().unwrap_or(Value::Null);
    let bound = binds(&data, &computer, &epoch)
        && reply.get("endpoint_id") == data.get("endpoint_id")
        && reply.get("controller_epoch").and_then(Value::as_str) == Some(epoch.as_str())
        && reply.get("authorization_generation") == data.get("expected_authorization_generation");
    if !bound {
        return Ok(ctx.failure(
            "IDENTITY_MISMATCH",
            "Control response changed target binding; inspect the target before retrying.",
            "unauthorized",
            false,
        ));
    }
    if reply.get("state").and_then(Value::as_str)==Some("pending_approval") {
        return Ok(ctx.ready(with(&data,&[("viewer_started",json!(false))])));
    }
    let uncertain = |message: &str| Ok(ctx.failure("CONTROL_UNCERTAIN", message, "failed", false));
    if pausing {
        if reply.get("paused") != Some(&json!(op == "pause")) {
            return uncertain("The computer did not confirm the change; refresh it before trying again.");
        }
        return Ok(ctx.ready(data));
    }
    if op == "handback" {
        let owner = reply.get("owner").and_then(Value::as_str).unwrap_or("");
        let settled = expected_owner(owner) && !owner.starts_with("operator:")
            && reply.get("agent_resumed").and_then(Value::as_bool).is_some_and(|resumed| !resumed || owner.starts_with("agent:"))
            && js::truthy(reply.get("ownership_revision"));
        if !settled {
            return uncertain("Handback did not prove settled ownership; inspect the target.");
        }
        clipboard::stop(&ctx.console, &computer);
        // The stream has ended; a viewer left running would block the next one.
        let closed = close_viewer(&ctx.console, &computer);
        return Ok(ctx.ready(with(&data, &[("viewer_started", json!(false)), ("viewer_closed", json!(closed))])));
    }
    let ready = reply.get("viewer_ready") == Some(&json!(true))
        && js::string_or(reply.get("owner").filter(|o| js::truthy(Some(o))), "").starts_with("operator:")
        && js::truthy(reply.get("control_generation"))
        && js::truthy(reply.get("ownership_revision"));
    if !ready {
        return uncertain("Takeover did not prove exclusive viewer readiness.");
    }
    let row = verified_row(ctx, &computer).filter(|row| {
        data.get("endpoint_id").and_then(Value::as_str) == Some(row.endpoint_id.as_str())
            && js::same_number(data.get("binding_revision"), row.binding_revision as f64)
            && js::same_number(data.get("expected_authorization_generation"), row.authorization_generation as f64)
    });
    let Some(row) = row else {
        return Ok(ctx.failure(
            "IDENTITY_MISMATCH",
            "Control is paused, but the selected viewer endpoint could not be revalidated.",
            "failed",
            false,
        ));
    };
    if let Some(stream) = reply.get("stream").filter(|s| s.is_object()) {
        let Some(bin) = view else {
            return Ok(ctx.failure("MISSING_DEPENDENCY", VIEWER_MISSING, "missing-dependency", false));
        };
        return open_stream(ctx, &computer, &epoch, &row, &bin, stream, &data).await;
    }
    let Some(moonlight) = which("moonlight") else {
        return Ok(ctx.failure("MISSING_DEPENDENCY", OLDER_HELD, "missing-dependency", false));
    };
    let pid = launch_viewer(&moonlight, &row.host).await?;
    remember_viewer(ctx, &computer, pid);
    Ok(ctx.ready(with(&data, &[("viewer_started", json!(true)), ("viewer_pid", json!(pid))])))
}

const VIEWER_MISSING: &str = "Take Control needs the ibara viewer (ibara-view) on this computer. Install it, then try again.";
/// A computer from before ibara-view, and no stream client for it here.
const OLDER_HELD: &str =
    "You have control of that computer, but it runs an older ibara whose screen can't be opened from here. Hand it back, update it, then take control again.";
const OLDER_SCREEN: &str = "That computer runs an older ibara whose screen can't be opened from here. Update it, then try again.";

/// Register this console's viewer certificate on `computer`: no PIN and no
/// restart there. A computer from before ibara-view does not know the
/// operation, which is fine: it streams with Moonlight. The refusal envelope
/// otherwise.
async fn register_viewer(ctx: &Ctx, computer: &str, epoch: &str, bin: &Path) -> Result<(), Value> {
    let sha = viewer_identity(bin).await.map_err(|fault| {
        let message = match fault {
            Fault::Coded(_, m) | Fault::Timeout(m) | Fault::Plain(m) => m,
        };
        ctx.failure("VIEWER_IDENTITY", &clip(&message, 300), "failed", true)
    })?;
    let call = ctx.console.sessions.call(computer, Some(epoch), "viewer_register", json!({"viewer_cert_sha256": sha}));
    match tokio::time::timeout(Duration::from_secs(20), call).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) if error.message.ends_with("Unknown operator operation.") => Ok(()),
        Ok(Err(error)) => {
            Err(ctx.failure("OPERATOR_REFUSED", &or(&error.message, "The computer did not accept this viewer."), "failed", true))
        }
        Err(_) => Err(ctx.failure("TIMEOUT", TRANSPORT_TIMED_OUT, "failed", true)),
    }
}

/// What ibara-view reads on stdin to stream `computer`.
fn viewer_bundle(computer: &str, epoch: &str, row: &ListedComputer, stream: &Value) -> Option<Value> {
    let text = |key: &str| stream.get(key).and_then(Value::as_str);
    let port = |key: &str| stream.get(key).and_then(Value::as_u64).filter(|p| (1..=65535).contains(p));
    let ticket = text("ticket").filter(|t| (32..=256).contains(&t.len()))?;
    let cert = text("server_cert_sha256").filter(|c| pattern::lower_hex(c, 64))?;
    Some(json!({
        "v": 1,
        "computer_id": computer,
        "computer_name": row.label,
        "host": row.host,
        "http_port": port("http_port")?,
        "https_port": port("https_port")?,
        "server_cert_sha256": cert,
        "ticket": ticket,
        "identity_dir": identity_dir(),
        "keys_chord": KEYS_CHORD,
        "console_socket": super::socket_path().ok(),
        "controller_epoch": epoch,
        "app": "Desktop",
        "width": stream.get("width").and_then(Value::as_u64).unwrap_or(1920),
        "height": stream.get("height").and_then(Value::as_u64).unwrap_or(1080),
        "fps": stream.get("fps").and_then(Value::as_u64).unwrap_or(30),
        "bitrate_kbps": stream.get("bitrate_kbps").and_then(Value::as_u64).unwrap_or(0),
    }))
}

/// Open ibara-view on the computer's stream with its ticket and share the
/// clipboard. The ticket never leaves this process except on the viewer's stdin.
async fn open_stream(ctx: &Ctx, computer: &str, epoch: &str, row: &ListedComputer, bin: &Path, stream: &Value, data: &Value) -> Handled {
    let Some(bundle) = viewer_bundle(computer, epoch, row, stream) else {
        return Ok(ctx.failure("CONTROL_UNCERTAIN", "The computer's viewer ticket was incomplete; choose Open Viewer.", "failed", false));
    };
    let pid = launch_ibara_view(bin, &bundle).await?;
    remember_viewer(ctx, computer, pid);
    clipboard::start(&ctx.console, computer, epoch);
    let mut data = data.clone();
    if let Some(stream) = data.get_mut("result").and_then(|r| r.get_mut("stream")).and_then(Value::as_object_mut) {
        stream.remove("ticket");
    }
    Ok(ctx.ready(with(&data, &[("viewer_started", json!(true)), ("viewer_pid", json!(pid))])))
}

/// `open-viewer --computer ID [--epoch E]`: open the viewer again for a
/// computer this console controls, with a fresh ticket from the computer
/// (`viewer_ticket`, for the holder only). It changes no ownership.
pub async fn open_viewer(ctx: &Ctx) -> Handled {
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let epoch = match option(&ctx.args, "--epoch") {
        Some(epoch) => validated_id(Some(epoch), "controller_epoch")?,
        None => {
            let session = ctx.console.sessions.call(&computer, None, "session", Value::Null);
            match tokio::time::timeout(ctx.timeout, session).await {
                Ok(Ok(data)) => validated_id(data.get("controller_epoch").and_then(Value::as_str), "controller_epoch")?,
                Ok(Err(error)) => return Ok(ctx.failure("OPERATOR_REFUSED", &or(&error.message, "The computer did not answer."), "failed", true)),
                Err(_) => return Err(Fault::Timeout(TRANSPORT_TIMED_OUT.into())),
            }
        }
    };
    let Some(row) = verified_row(ctx, &computer) else {
        return Ok(ctx.failure("IDENTITY_MISMATCH", "The selected viewer endpoint could not be revalidated.", "failed", false));
    };
    let view = ibara_view();
    if let Some(bin) = &view
        && let Err(refusal) = register_viewer(ctx, &computer, &epoch, bin).await
    {
        return Ok(refusal);
    }
    let call = ctx.console.sessions.call(&computer, Some(&epoch), "viewer_ticket", Value::Null);
    let data = match tokio::time::timeout(CONTROL_DEADLINE, call).await {
        Err(_) => return Err(Fault::Timeout(TRANSPORT_TIMED_OUT.into())),
        // A computer from before ibara-view: Moonlight, as before.
        Ok(Err(error)) if error.message.ends_with("Unknown operator operation.") => {
            let Some(moonlight) = which("moonlight") else {
                return Ok(ctx.failure("MISSING_DEPENDENCY", OLDER_SCREEN, "missing-dependency", false));
            };
            let pid = launch_viewer(&moonlight, &row.host).await?;
            remember_viewer(ctx, &computer, pid);
            return Ok(ctx.ready(json!({"computer_id": computer, "viewer_started": true, "viewer_pid": pid})));
        }
        Ok(Err(error)) if view.is_none() && error.message.contains("does not know your viewer") => {
            return Ok(ctx.failure("MISSING_DEPENDENCY", VIEWER_MISSING, "missing-dependency", false));
        }
        Ok(Err(error)) => {
            return Ok(ctx.failure("OPERATOR_REFUSED", &or(&error.message, "The computer did not open its screen."), "failed", true));
        }
        Ok(Ok(data)) => data,
    };
    let stream = data.get("result").and_then(|r| r.get("stream")).filter(|s| s.is_object()).cloned();
    let (Some(stream), true) = (stream, binds(&data, &computer, &epoch)) else {
        return Ok(ctx.failure("IDENTITY_MISMATCH", "The computer's answer did not match it; refresh it and try again.", "failed", false));
    };
    let Some(bin) = view else {
        return Ok(ctx.failure("MISSING_DEPENDENCY", VIEWER_MISSING, "missing-dependency", false));
    };
    open_stream(ctx, &computer, &epoch, &row, &bin, &stream, &data).await
}

fn remember_viewer(ctx: &Ctx, computer: &str, pid: u32) {
    if pid != 0 {
        ctx.console.viewers.lock().unwrap_or_else(|e| e.into_inner()).insert(computer.to_string(), pid);
    }
}

/// End the viewer this console started for `computer`, if it is still that
/// viewer (ibara-view, or Moonlight for an older computer). Whether one was ended.
pub(super) fn close_viewer(console: &Console, computer: &str) -> bool {
    let Some(pid) = console.viewers.lock().unwrap_or_else(|e| e.into_inner()).remove(computer) else { return false };
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signals a process this console started, checked by name first.
    matches!(comm.trim(), "ibara-view" | "moonlight") && unsafe { libc::kill(pid, libc::SIGTERM) } == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_owner_accepts_only_the_bridge_forms() {
        for good in ["none", "human", "operator:vesper", "agent:task_1"] {
            assert!(expected_owner(good), "{good}");
        }
        for bad in ["", "operator:", "agent:", "people", "operator:a b", &format!("operator:{}", "a".repeat(129))] {
            assert!(!expected_owner(bad), "{bad}");
        }
    }
}
