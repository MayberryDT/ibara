//! Selected operator RPC: `ibara operator …` (replaces `agent/ibara-operator.mjs`) and
//! [`OperatorSessions`], the per-computer long-lived operator transport.
//!
//! No admin secret, no mutable descriptor, no fallback. Every call rebinds from the
//! private directory; every reply must echo the pinned endpoint, the controller
//! epoch and the grant generation (`checkedReply`) or it is refused.

use super::directory::{OperatorDirectory, SelectedEnvelope, bind_fresh, directory_path};
use super::transport::{self, RouteKind};
use super::{LineRead, exit_with, fail, js, pattern, read_line_bounded, refused, runtime, unreachable_route};
use crate::error::{IbaraError, Result};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

/// A reply line (and a one-shot stdout) is at most 2 MiB (`REPLY_LIMIT`).
pub const REPLY_LIMIT: usize = 2 * 1024 * 1024;
/// The stdin action bound (ibara-operator.mjs:14).
pub const ACTION_LIMIT: usize = 1_500_000;
/// Transport deadline for ordinary operations.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Control transitions and the file operations that read a whole file: the target
/// relay allows 150 s; add SSH connect time and margin.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(170);
/// An unused session closes this long after its last reply.
pub const IDLE_CLOSE: Duration = Duration::from_secs(30);
/// A graceful close waits this long before signalling the transport's process group.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Requests queued behind one session before callers are refused.
const SESSION_QUEUE: usize = 64;

const FILE_OPS: [&str; 9] = [
    "files_roots", "files_list", "files_begin_upload", "files_resume_upload", "files_upload_chunk", "files_publish",
    "files_begin_download", "files_download_chunk", "files_status",
];
const CONTROL_OPS: [&str; 2] = ["take_control", "handback"];
/// Pause and resume take no action body but, like control transitions, may
/// wait on settlement.
const PAUSE_OPS: [&str; 2] = ["pause", "resume"];
/// Opening a viewer may start the target's stream.
const VIEWER_TICKET: &str = "viewer_ticket";
/// The shared clipboard during Take Control.
const CLIPBOARD_OPS: [&str; 2] = ["clipboard_get", "clipboard_set"];
const PINNED: [&str; 4] = ["op", "endpoint_id", "controller_epoch", "expected_authorization_generation"];
const QUALITIES: [&str; 2] = ["tile", "selected"];

fn is_control(op: &str) -> bool {
    CONTROL_OPS.contains(&op)
}

/// Operations that get their own transport and the long deadline.
fn is_transition(op: &str) -> bool {
    is_control(op)
        || PAUSE_OPS.contains(&op)
        || op == VIEWER_TICKET
        || crate::controller::SLOW_OPS.contains(&op)
        || crate::controller::SLOW_FILE_OPS.contains(&op)
}

/// Operations whose body is a JSON action (stdin for the CLI).
fn takes_action(op: &str) -> bool {
    FILE_OPS.contains(&op)
        || is_control(op)
        || matches!(op,"pairing_confirm"|"access_set"|"access_remove"|"access_unpair"|"answer_attention"|"viewer_register"|"viewer_ticket"|"observe_video")
        || CLIPBOARD_OPS.contains(&op)
        || crate::controller::EVERYDAY_OPS.contains(&op)
}

/// One validated operator request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OperatorRequest {
    pub computer: String,
    /// Absent only for `session`, which bootstraps the epoch.
    pub epoch: Option<String>,
    pub op: String,
    pub task: Option<String>,
    pub display: Option<String>,
    pub quality: Option<String>,
    pub display_revision: Option<String>,
    /// `jpeg` from a console that reads it; absent, the target sends PNG.
    pub format: Option<String>,
    /// The digest of the picture the console shows, so an unchanged screen is not sent again.
    pub previous: Option<String>,
    /// The action body of `files_*`, `take_control`, `handback`, `pairing_confirm`.
    pub action: Option<Map<String, Value>>,
}

impl OperatorRequest {
    /// The argv checks of ibara-operator.mjs:158-163.
    pub fn check_shape(&self) -> Result<()> {
        let op = self.op.as_str();
        let supported = ["status", "task_status", "observe", "session", "access", "attention"].contains(&op) || PAUSE_OPS.contains(&op) || takes_action(op);
        let epoch = self.epoch.as_deref().unwrap_or("");
        let epoch_bad = if op == "session" { !epoch.is_empty() } else { !pattern::id(epoch) };
        if self.computer.is_empty() || !supported || epoch_bad {
            return Err(fail(
                "Selected operator requires a computer, verified epoch and supported operation (session bootstraps without an epoch).",
            ));
        }
        let given = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
        if op == "observe"
            && (!pattern::id(self.display.as_deref().unwrap_or(""))
                || !QUALITIES.contains(&self.quality.as_deref().unwrap_or("")))
        {
            return Err(fail("Observation requires a named display and quality."));
        }
        if op == "observe"
            && (self.format.as_deref().is_some_and(|f| f != "jpeg")
                || self.previous.as_deref().is_some_and(|p| !pattern::lower_hex(p, 64)))
        {
            return Err(fail("Observation takes only the JPEG format and a previous picture's digest."));
        }
        if op != "observe"
            && (given(&self.display) || given(&self.quality) || given(&self.display_revision) || given(&self.format) || given(&self.previous))
        {
            return Err(fail("Only observation accepts display arguments."));
        }
        if op == "task_status" && !pattern::id(self.task.as_deref().unwrap_or("")) {
            return Err(fail("Task status requires one explicit task reference."));
        }
        if op != "task_status" && given(&self.task) {
            return Err(fail("Only task status accepts a task reference."));
        }
        Ok(())
    }

    /// The action-body checks of ibara-operator.mjs:167-175.
    pub fn check_action(op: &str, action: Value) -> Result<Map<String, Value>> {
        let Value::Object(action) = action else {
            return Err(fail("File action must not override pinned identity fields."));
        };
        if PINNED.iter().any(|k| action.contains_key(*k)) {
            return Err(fail("File action must not override pinned identity fields."));
        }
        if is_control(op)
            && (!pattern::owner(&js::string_or(action.get("expected_owner"), ""))
                || !pattern::id(&js::string_or(action.get("expected_ownership_revision"), "")))
        {
            return Err(fail("Control requires an exact expected owner and ownership revision."));
        }
        if op == "pairing_confirm"
            && (!pattern::challenge_ref(&js::string_or(action.get("challenge_ref"), ""))
                || !pattern::lower_hex(&js::string_or(action.get("nonce"), ""), 64)
                || !pattern::key_fingerprint(&js::string_or(action.get("operator_key_fingerprint"), "")))
        {
            return Err(fail("Pairing confirmation requires an exact challenge, nonce and key fingerprint."));
        }
        if op == "viewer_register"
            && (action.len() != 1 || !pattern::lower_hex(&js::string_or(action.get("viewer_cert_sha256"), ""), 64))
        {
            return Err(fail("Viewer registration requires only the viewer certificate's SHA-256."));
        }
        if op == VIEWER_TICKET && !action.is_empty() {
            return Err(fail("A viewer ticket takes no fields."));
        }
        Ok(action)
    }

    /// A request from library fields: `observe` takes `display_id`, `quality` and
    /// optional `display_revision`, `format` and `previous`; `task_status` takes
    /// `task_ref`; action operations take their action body; `status` and
    /// `session` take nothing.
    pub fn from_fields(computer: &str, epoch: Option<&str>, op: &str, fields: Value) -> Result<Self> {
        let mut fields = match fields {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            _ => return Err(fail("Operator fields must be an object.")),
        };
        let mut request = OperatorRequest {
            computer: computer.to_string(),
            epoch: epoch.map(str::to_string),
            op: op.to_string(),
            ..OperatorRequest::default()
        };
        let mut take = |name: &str| fields.shift_remove(name).map(|v| js::string(&v));
        if takes_action(op) {
            request.check_shape()?;
            request.action = Some(Self::check_action(op, Value::Object(fields))?);
            return Ok(request);
        }
        // The argv checks then give the Node tool's messages for misplaced fields.
        request.display = take("display_id");
        request.quality = take("quality");
        request.display_revision = take("display_revision");
        request.format = take("format");
        request.previous = take("previous");
        request.task = take("task_ref");
        request.check_shape()?;
        if let Some(extra) = fields.keys().next() {
            return Err(fail(format!("Operation {op} does not accept the field {extra}.")));
        }
        Ok(request)
    }

    /// `recordId`: the display for observation, else the operation.
    pub fn record_id(&self) -> &str {
        if self.op == "observe" { self.display.as_deref().unwrap_or("") } else { &self.op }
    }

    pub fn timeout(&self) -> Duration {
        if is_transition(&self.op) { CONTROL_TIMEOUT } else { CALL_TIMEOUT }
    }

    /// The action line sent to the target (ibara-operator.mjs:182-188).
    pub fn action_for(&self, envelope: &SelectedEnvelope) -> Value {
        let mut action = self.action.clone().unwrap_or_default();
        action.insert("op".into(), json!(self.op));
        action.insert("endpoint_id".into(), json!(envelope.endpoint_id));
        action.insert("expected_authorization_generation".into(), json!(envelope.expected_authorization_generation));
        if self.op == "task_status" {
            action.insert("task_ref".into(), json!(self.task));
        }
        if self.op != "session" {
            action.insert("controller_epoch".into(), json!(self.epoch));
        }
        if self.op == "observe" {
            action.insert("display_id".into(), json!(self.display));
            action.insert("quality".into(), json!(self.quality));
            if let Some(revision) = self.display_revision.as_deref().filter(|r| !r.is_empty()) {
                action.insert("display_revision".into(), json!(revision));
            }
            for (name, value) in [("format", &self.format), ("previous", &self.previous)] {
                if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                    action.insert(name.into(), json!(value));
                }
            }
        }
        Value::Object(action)
    }
}

/// Whether the target refused (`reply.error || reply.result?.error`).
fn is_refusal(reply: &Value) -> bool {
    js::truthy(reply.get("error")) || js::truthy(reply.get("result").and_then(|r| r.get("error")))
}

/// `checkedReply(envelope, epoch, op, reply)` (ibara-operator.mjs:39-46): a refusal becomes
/// `CODE: message`; a reply whose endpoint, epoch or grant generation differs from the
/// binding is refused; otherwise the binding plus the target's result.
pub fn checked_reply(envelope: &SelectedEnvelope, epoch: Option<&str>, op: &str, reply: &Value) -> Result<Value> {
    if is_refusal(reply) {
        let error = reply.get("error");
        let inner = reply.get("result").and_then(|r| r.get("error"));
        let pick = |name: &str, fallback: &str| {
            [error.and_then(|e| e.get(name)), inner.and_then(|e| e.get(name))]
                .into_iter()
                .find(|v| js::truthy(*v))
                .flatten()
                .map(js::string)
                .unwrap_or_else(|| fallback.to_string())
        };
        let code = pick("code", "OPERATOR_ERROR");
        let message = pick("message", "Target refused the operation.");
        return Err(refused(&code, format!("{code}: {message}")));
    }
    let response = reply.get("result").filter(|r| js::truthy(Some(r)));
    let identity_ok = response.is_some_and(|r| {
        let epoch_ok = if op == "session" {
            pattern::id(&js::string_or(r.get("controller_epoch"), ""))
        } else {
            matches!((r.get("controller_epoch"), epoch), (Some(Value::String(got)), Some(want)) if got == want)
        };
        r.get("endpoint_id") == Some(&json!(envelope.endpoint_id))
            && epoch_ok
            && js::same_number(r.get("authorization_generation"), envelope.expected_authorization_generation as f64)
    });
    let Some(response) = response.filter(|_| identity_ok) else {
        return Err(IbaraError::new(
            "STALE_TARGET",
            "Selected operator response identity, epoch or grant generation changed.",
            false,
        ));
    };
    let mut out = Map::new();
    out.insert("environment_id".into(), json!(envelope.environment_id));
    out.insert("computer_id".into(), json!(envelope.computer_id));
    out.insert("endpoint_id".into(), json!(envelope.endpoint_id));
    out.insert("binding_revision".into(), json!(envelope.binding_revision));
    out.insert("request_id".into(), json!(envelope.request_id));
    out.insert("expected_authorization_generation".into(), json!(envelope.expected_authorization_generation));
    out.insert("record_id".into(), json!(envelope.record_id));
    if let Some(epoch) = response.get("controller_epoch") {
        out.insert("controller_epoch".into(), epoch.clone());
    }
    out.insert("result".into(), response.clone());
    Ok(Value::Object(out))
}

fn spawn_error(error: std::io::Error) -> IbaraError {
    unreachable_route(format!("spawn {} {error}", transport::SSH))
}

/// One selected-operator transport for one action (`spawnSync(ibara-transport …)`):
/// write the line, close stdin, collect at most 2 MiB of stdout and stderr each.
async fn one_shot(envelope: &SelectedEnvelope, line: &str, deadline: Duration) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::from(transport::selected_command(RouteKind::Operator, envelope)?);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().map_err(spawn_error)?;
    let pid = child.id().unwrap_or(0);
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let work = async {
        if let Some(mut input) = stdin.take() {
            let _ = input.write_all(line.as_bytes()).await;
        }
        let (out, err) = tokio::join!(read_capped(&mut stdout), read_capped(&mut stderr));
        let status = child.wait().await;
        (out, err, status)
    };
    let (out, err, status) = match tokio::time::timeout(deadline, work).await {
        Ok(done) => done,
        Err(_) => {
            transport::kill_pid(pid, libc::SIGTERM);
            return Err(unreachable_route(format!("spawnSync {} ETIMEDOUT", transport::SSH)));
        }
    };
    let (Some(out), Some(err)) = (out, err) else {
        transport::kill_pid(pid, libc::SIGTERM);
        return Err(unreachable_route(format!("spawnSync {} ENOBUFS", transport::SSH)));
    };
    let status = status?;
    if !status.success() {
        let text = String::from_utf8_lossy(&err);
        let message = js::trim(&text);
        return Err(unreachable_route(if message.is_empty() { "Selected operator transport failed." } else { message }));
    }
    Ok(out)
}

/// Read to EOF, or `None` once more than `REPLY_LIMIT` bytes arrive (`maxBuffer`).
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut block = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut block).await {
            Ok(0) | Err(_) => return Some(out),
            Ok(n) if out.len() + n > REPLY_LIMIT => return None,
            Ok(n) => out.extend_from_slice(&block[..n]),
        }
    }
}

/// One operation over a fresh transport (the CLI path, and control transitions).
pub async fn call_once(database: &Path, request: &OperatorRequest) -> Result<Value> {
    let directory = OperatorDirectory::open(database)?;
    let envelope = directory.bind_operation(&request.computer, None, request.record_id());
    directory.close();
    let envelope = envelope?;
    let line = format!("{}\n", request.action_for(&envelope));
    let stdout = one_shot(&envelope, &line, request.timeout()).await?;
    let text = String::from_utf8_lossy(&stdout);
    let trimmed = js::trim(&text);
    if trimmed.split('\n').count() != 1 || stdout.len() > REPLY_LIMIT {
        return Err(unreachable_route("Selected operator reply is malformed or oversized."));
    }
    let reply = super::parse_json(trimmed)?;
    checked_reply(&envelope, request.epoch.as_deref(), &request.op, &reply)
}

// ---------------------------------------------------------------------------
// Long-lived sessions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Close {
    Open,
    Graceful,
    Force,
}

type Exchange = std::result::Result<Vec<u8>, String>;

struct Job {
    line: String,
    timeout: Duration,
    reply: oneshot::Sender<Exchange>,
}

#[derive(Clone)]
struct Session {
    id: u64,
    key: String,
    jobs: mpsc::Sender<Job>,
    close: Arc<watch::Sender<Close>>,
    closed: Arc<AtomicBool>,
    finished: watch::Receiver<bool>,
}

const CLOSED: &str = "Selected operator transport closed.";
const MALFORMED: &str = "Selected operator reply is malformed or oversized.";

impl Session {
    fn close(&self, how: Close) {
        self.close.send_if_modified(|current| {
            let escalate = matches!((*current, how), (Close::Open, _) | (Close::Graceful, Close::Force));
            if escalate {
                *current = how;
            }
            escalate
        });
    }

    async fn exchange(&self, line: String, timeout: Duration) -> Exchange {
        let (reply, answer) = oneshot::channel();
        match self.jobs.try_send(Job { line, timeout, reply }) {
            Ok(()) => answer.await.unwrap_or_else(|_| Err(CLOSED.to_string())),
            Err(mpsc::error::TrySendError::Full(_)) => Err("Selected operator session is busy.".to_string()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CLOSED.to_string()),
        }
    }

    async fn wait_finished(&self) {
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
}

struct Inner {
    database: PathBuf,
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
}

/// Per-computer long-lived selected-operator transports, shared by `ibara operator
/// --serve` and the operator-side console service.
///
/// Each call rebinds from the private directory (a forgotten or unverified computer
/// closes its session), reuses the computer's session only while the binding key
/// `[environment, computer, endpoint, binding revision, grant generation, route]` is
/// unchanged, and serialises calls per session. A timeout, EOF, oversized or
/// unrequested line, or a reply for the wrong identity force-closes the session; a
/// target refusal keeps it. Sessions close 30 s after their last reply.
/// `take_control`, `handback`, `pause` and `resume` (up to 170 s) use their own
/// transport so previews keep flowing during a control transition; so do a sent
/// file's final check and a fetched file's digest, which read the whole file.
#[derive(Clone)]
pub struct OperatorSessions {
    inner: Arc<Inner>,
}

impl OperatorSessions {
    pub fn new(database: PathBuf) -> Self {
        OperatorSessions {
            inner: Arc::new(Inner { database, sessions: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) }),
        }
    }

    /// Call `op` on `computer`. `epoch` is required except for `session`; `fields` as in
    /// [`OperatorRequest::from_fields`]. Returns the `checkedReply` object.
    pub async fn call(&self, computer: &str, epoch: Option<&str>, op: &str, fields: Value) -> Result<Value> {
        let request = OperatorRequest::from_fields(computer, epoch, op, fields)?;
        self.run(&request).await
    }

    /// Run a validated request.
    pub async fn run(&self, request: &OperatorRequest) -> Result<Value> {
        if is_transition(&request.op) {
            return call_once(&self.inner.database, request).await;
        }
        let envelope = match bind_fresh(&self.inner.database, &request.computer, request.record_id()) {
            Ok(envelope) => envelope,
            Err(error) => {
                // A forgotten or unverified computer must not keep an open route.
                if let Some(stale) = self.lock().remove(&request.computer) {
                    stale.close(Close::Force);
                }
                return Err(error);
            }
        };
        let session = self.session_for(&request.computer, &envelope)?;
        let line = format!("{}\n", request.action_for(&envelope));
        let line = session.exchange(line, request.timeout()).await.map_err(unreachable_route)?;
        let Ok(reply) = serde_json::from_slice::<Value>(&line) else {
            session.close(Close::Force);
            return Err(unreachable_route(MALFORMED));
        };
        // A target refusal leaves the session usable; a reply for the wrong identity does not.
        let checked = checked_reply(&envelope, request.epoch.as_deref(), &request.op, &reply);
        if checked.is_err() && !is_refusal(&reply) {
            session.close(Close::Force);
        }
        checked
    }

    /// Close every session gracefully and wait for their transports to go.
    pub async fn close_all(&self) {
        let sessions: Vec<Session> = self.lock().drain().map(|(_, s)| s).collect();
        for session in &sessions {
            session.close(Close::Graceful);
        }
        for session in &sessions {
            session.wait_finished().await;
        }
    }

    /// Close `computer`'s session at once: it left the directory or answers as a new computer.
    pub fn forget(&self, computer: &str) {
        if let Some(session) = self.lock().remove(computer) {
            session.close(Close::Force);
        }
    }

    /// The number of open sessions.
    pub fn open_sessions(&self) -> usize {
        self.lock().values().filter(|s| !s.closed.load(Ordering::SeqCst)).count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.inner.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session_for(&self, computer: &str, envelope: &SelectedEnvelope) -> Result<Session> {
        let key = envelope.session_key();
        let mut sessions = self.lock();
        if let Some(existing) = sessions.get(computer) {
            if !existing.closed.load(Ordering::SeqCst) && existing.key == key {
                return Ok(existing.clone());
            }
            existing.close(Close::Force);
            sessions.remove(computer);
        }
        let session = self.open(computer, key, envelope)?;
        sessions.insert(computer.to_string(), session.clone());
        Ok(session)
    }

    fn open(&self, computer: &str, key: String, envelope: &SelectedEnvelope) -> Result<Session> {
        let mut command = tokio::process::Command::from(transport::selected_command(RouteKind::Operator, envelope)?);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let mut child = command.spawn().map_err(spawn_error)?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (lines_tx, lines) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_line_bounded(&mut reader, REPLY_LIMIT, false).await {
                    Ok(LineRead::Line(line)) => {
                        if lines_tx.send(LineRead::Line(line)).await.is_err() {
                            return;
                        }
                    }
                    Ok(LineRead::Oversize) => {
                        let _ = lines_tx.send(LineRead::Oversize).await;
                        return;
                    }
                    Ok(LineRead::Eof) | Err(_) => return,
                }
            }
        });
        let tail = Arc::new(Mutex::new(String::new()));
        let tail_writer = tail.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut block = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut block).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail_writer.lock().unwrap_or_else(|p| p.into_inner());
                tail.push_str(&String::from_utf8_lossy(&block[..n]));
                let excess = tail.chars().count().saturating_sub(400);
                if excess > 0 {
                    let cut = tail.char_indices().nth(excess).map_or(tail.len(), |(i, _)| i);
                    tail.drain(..cut);
                }
            }
        });
        let (jobs_tx, jobs) = mpsc::channel(SESSION_QUEUE);
        let (close_tx, close) = watch::channel(Close::Open);
        let (finished_tx, finished) = watch::channel(false);
        let closed = Arc::new(AtomicBool::new(false));
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let actor = Actor {
            child,
            stdin,
            lines,
            tail,
            jobs,
            close,
            closed: closed.clone(),
            owner: Arc::downgrade(&self.inner),
            computer: computer.to_string(),
            id,
        };
        tokio::spawn(async move {
            actor.run().await;
            let _ = finished_tx.send(true);
        });
        Ok(Session { id, key, jobs: jobs_tx, close: Arc::new(close_tx), closed, finished })
    }
}

struct Actor {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<LineRead>,
    tail: Arc<Mutex<String>>,
    jobs: mpsc::Receiver<Job>,
    close: watch::Receiver<Close>,
    closed: Arc<AtomicBool>,
    owner: Weak<Inner>,
    computer: String,
    id: u64,
}

impl Actor {
    fn close_request(close: &watch::Receiver<Close>) -> Close {
        *close.borrow()
    }

    async fn run(mut self) {
        let mut failure: Option<String> = None;
        let mut unrequested = false;
        let mut idle: Option<Instant> = None;
        let force = loop {
            let requested = Self::close_request(&self.close);
            if requested != Close::Open {
                break requested == Close::Force;
            }
            let idle_at = idle;
            tokio::select! {
                biased;
                changed = self.close.changed() => {
                    if changed.is_err() {
                        break false;
                    }
                }
                job = self.jobs.recv() => {
                    let Some(job) = job else { break false };
                    if unrequested {
                        let message = "Selected operator sent an unrequested reply.".to_string();
                        let _ = job.reply.send(Err(message.clone()));
                        failure = Some(message);
                        break true;
                    }
                    let stdin = &mut self.stdin;
                    let lines = &mut self.lines;
                    let exchange = async {
                        if let Some(input) = stdin.as_mut() {
                            let _ = input.write_all(job.line.as_bytes()).await;
                        }
                        lines.recv().await
                    };
                    tokio::select! {
                        event = exchange => match event {
                            Some(LineRead::Line(line)) => {
                                let _ = job.reply.send(Ok(line));
                                idle = Some(Instant::now() + IDLE_CLOSE);
                            }
                            Some(LineRead::Oversize) => {
                                failure = Some(MALFORMED.to_string());
                                let _ = job.reply.send(Err(MALFORMED.to_string()));
                                break true;
                            }
                            Some(LineRead::Eof) | None => {
                                let _ = job.reply.send(Err(failure.clone().unwrap_or_else(|| CLOSED.to_string())));
                                break false;
                            }
                        },
                        _ = tokio::time::sleep(job.timeout) => {
                            let tail = self.tail.lock().map(|t| js::trim(&t).to_string()).unwrap_or_default();
                            let message = if tail.is_empty() {
                                "Selected operator transport timed out.".to_string()
                            } else {
                                format!("Selected operator transport timed out. {tail}")
                            };
                            let _ = job.reply.send(Err(message.clone()));
                            failure = Some(message);
                            break true;
                        }
                        changed = self.close.changed() => {
                            let force = changed.is_err() || Self::close_request(&self.close) == Close::Force;
                            let _ = job.reply.send(Err(CLOSED.to_string()));
                            break force;
                        }
                    }
                }
                event = self.lines.recv() => match event {
                    // Kept until the next request, which then fails (ibara-operator.mjs:88).
                    Some(LineRead::Line(_)) => unrequested = true,
                    Some(LineRead::Oversize) => {
                        failure = Some(MALFORMED.to_string());
                        break true;
                    }
                    Some(LineRead::Eof) | None => break false,
                },
                _ = async {
                    match idle_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => break false,
            }
        };
        self.closed.store(true, Ordering::SeqCst);
        if let Some(owner) = self.owner.upgrade() {
            let mut sessions = owner.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if sessions.get(&self.computer).is_some_and(|s| s.id == self.id) {
                sessions.remove(&self.computer);
            }
        }
        self.jobs.close();
        while let Ok(job) = self.jobs.try_recv() {
            let _ = job.reply.send(Err(failure.clone().unwrap_or_else(|| CLOSED.to_string())));
        }
        // End stdin; signal the process group now when forced, else after the grace period.
        drop(self.stdin.take());
        let pid = self.child.id().unwrap_or(0);
        if force || tokio::time::timeout(CLOSE_GRACE, self.child.wait()).await.is_err() {
            transport::kill_group(pid, libc::SIGTERM);
        }
        if tokio::time::timeout(CLOSE_GRACE, self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}

// ---------------------------------------------------------------------------
// The CLI.

const USAGE: &str = "Usage: ibara-operator --computer NAME --epoch EPOCH --op status|task_status|observe|files_*";

/// `explicit || $IBARA_OPERATOR_DIRECTORY_DB || default`, which must be absolute.
fn absolute_directory(explicit: Option<&str>) -> Result<PathBuf> {
    let db = directory_path(explicit);
    if !db.is_absolute() {
        return Err(fail("Operator directory path must be absolute."));
    }
    Ok(db)
}

/// `boundedFileAction()`: stop at the protocol bound instead of buffering all of stdin.
fn bounded_stdin_action() -> Result<Value> {
    use std::io::Read;
    let mut data = Vec::new();
    std::io::stdin().lock().take(ACTION_LIMIT as u64 + 1).read_to_end(&mut data)?;
    if data.len() > ACTION_LIMIT {
        return Err(fail("File action exceeds operator message bound."));
    }
    super::parse_json(&String::from_utf8_lossy(&data))
}

/// Parse `ibara operator` argv (ibara-operator.mjs:148-153).
fn parse_request(argv: &[String]) -> Result<(OperatorRequest, Option<String>)> {
    const KEYS: [&str; 8] =
        ["--computer", "--directory-db", "--epoch", "--op", "--task", "--display", "--quality", "--display-revision"];
    let mut options: HashMap<&str, String> = HashMap::new();
    let mut i = 0;
    while i < argv.len() {
        let key = argv[i].as_str();
        let value = argv.get(i + 1).filter(|v| !v.is_empty());
        let Some(value) = value.filter(|_| KEYS.contains(&key) && !options.contains_key(key)) else {
            return Err(fail(USAGE));
        };
        options.insert(key, value.clone());
        i += 2;
    }
    let mut get = |k: &str| options.remove(k);
    let request = OperatorRequest {
        computer: get("--computer").unwrap_or_default(),
        epoch: get("--epoch"),
        op: get("--op").unwrap_or_default(),
        task: get("--task"),
        display: get("--display"),
        quality: get("--quality"),
        display_revision: get("--display-revision"),
        format: None,
        previous: None,
        action: None,
    };
    Ok((request, get("--directory-db")))
}

fn run_op(argv: &[String]) -> Result<Value> {
    let (mut request, database) = parse_request(argv)?;
    request.check_shape()?;
    if takes_action(&request.op) {
        request.action = Some(OperatorRequest::check_action(&request.op, bounded_stdin_action()?)?);
    }
    let database = absolute_directory(database.as_deref())?;
    let directory = OperatorDirectory::open(&database)?;
    let named = directory.resolve_computer(&request.computer);
    directory.close();
    request.computer = named?.computer_id;
    runtime()?.block_on(call_once(&database, &request))
}

/// `ibara operator ARGS…`.
pub fn main(args: Vec<String>) -> ExitCode {
    if args.first().map(String::as_str) == Some("--serve") {
        let rest = &args[1..];
        if !(rest.is_empty() || (rest.len() == 2 && rest[0] == "--directory-db")) {
            return exit_with(&fail("Usage: ibara-operator --serve [--directory-db FILE]"));
        }
        let database = match absolute_directory(rest.get(1).map(String::as_str)) {
            Ok(db) => db,
            Err(error) => return exit_with(&error),
        };
        return match runtime() {
            Ok(rt) => rt.block_on(serve(database)),
            Err(error) => exit_with(&error.into()),
        };
    }
    match run_op(&args) {
        Ok(reply) => {
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(error) => exit_with(&error),
    }
}

/// The preview service line protocol (ibara-operator.mjs:117-138): one read-only
/// observation per request line, answered as `{id, ok, reply|error}` lines.
pub async fn serve(database: PathBuf) -> ExitCode {
    let sessions = OperatorSessions::new(database);
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut input = BufReader::new(tokio::io::stdin());
    let mut pending = tokio::task::JoinSet::new();
    loop {
        let line = match read_line_bounded(&mut input, 64 * 1024, false).await {
            Ok(LineRead::Line(line)) => line,
            Ok(LineRead::Oversize) => continue,
            Ok(LineRead::Eof) | Err(_) => break,
        };
        while pending.try_join_next().is_some() {}
        if line.is_empty() {
            continue;
        }
        let Some((id, request)) = accept(&line) else { continue };
        let out = out.clone();
        match request {
            Err(message) => emit(&out, json!({"id": id, "ok": false, "error": message})).await,
            Ok(request) => {
                let sessions = sessions.clone();
                pending.spawn(async move {
                    let reply = match sessions.run(&request).await {
                        Ok(reply) => json!({"id": id, "ok": true, "reply": reply}),
                        Err(error) => json!({"id": id, "ok": false, "error": error.message}),
                    };
                    emit(&out, reply).await;
                });
            }
        }
    }
    while pending.join_next().await.is_some() {}
    sessions.close_all().await;
    ExitCode::SUCCESS
}

/// Parse one preview request. `None` drops the line silently (bad JSON or id).
fn accept(line: &[u8]) -> Option<(String, std::result::Result<OperatorRequest, String>)> {
    let request: Value = serde_json::from_slice(line).ok()?;
    let id = js::string_or(request.get("id"), "");
    if !pattern::id(&id) {
        return None;
    }
    let text = |name: &str| js::string_or(request.get(name), "");
    let quality_ok = matches!(request.get("quality"), Some(Value::String(q)) if QUALITIES.contains(&q.as_str()));
    if !js::has_exact_keys(&request, &["computer", "display", "epoch", "id", "quality"])
        || !pattern::id(&text("computer"))
        || !pattern::id(&text("epoch"))
        || !pattern::id(&text("display"))
        || !quality_ok
    {
        return Some((
            id,
            Err("The preview service accepts only one read-only observation with a computer, epoch, display and quality."
                .to_string()),
        ));
    }
    Some((
        id,
        Ok(OperatorRequest {
            computer: text("computer"),
            epoch: Some(text("epoch")),
            op: "observe".into(),
            display: Some(text("display")),
            quality: Some(text("quality")),
            ..OperatorRequest::default()
        }),
    ))
}

async fn emit(out: &tokio::sync::Mutex<tokio::io::Stdout>, value: Value) {
    let mut line = value.to_string();
    line.push('\n');
    let mut out = out.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}

#[cfg(test)]
mod tests {
    use super::super::directory::SelectedRoute;
    use super::*;

    fn envelope() -> SelectedEnvelope {
        SelectedEnvelope {
            environment_id: "operator_e".into(),
            computer_id: "computer_a".into(),
            endpoint_id: "ibara_0123456789".into(),
            binding_revision: 2,
            request_id: "request_r".into(),
            expected_authorization_generation: 3,
            record_id: "status".into(),
            route: SelectedRoute {
                host: "tulip1".into(),
                user: "vesper".into(),
                port: 2222,
                identity_file_ref: "file:/k".into(),
                known_hosts_file_ref: "file:/h".into(),
            },
        }
    }

    fn reply(endpoint: &str, epoch: Value, generation: Value) -> Value {
        json!({"result": {"endpoint_id": endpoint, "controller_epoch": epoch, "authorization_generation": generation, "ok": true}})
    }

    const CHANGED: &str = "Selected operator response identity, epoch or grant generation changed.";

    #[test]
    fn checked_reply_rejects_a_different_endpoint() {
        let err = checked_reply(&envelope(), Some("epoch_1"), "status", &reply("ibara_other000", json!("epoch_1"), json!(3)));
        assert_eq!(err.err().unwrap().message, CHANGED);
    }

    #[test]
    fn checked_reply_rejects_a_different_epoch() {
        let err = checked_reply(&envelope(), Some("epoch_1"), "status", &reply("ibara_0123456789", json!("epoch_2"), json!(3)));
        assert_eq!(err.err().unwrap().message, CHANGED);
        // `session` accepts any valid epoch, but not an invalid one.
        assert!(checked_reply(&envelope(), None, "session", &reply("ibara_0123456789", json!("epoch_2"), json!(3))).is_ok());
        let err = checked_reply(&envelope(), None, "session", &reply("ibara_0123456789", json!("bad epoch"), json!(3)));
        assert_eq!(err.err().unwrap().message, CHANGED);
    }

    #[test]
    fn checked_reply_rejects_a_different_grant_generation() {
        for generation in [json!(4), json!("3"), Value::Null] {
            let err = checked_reply(&envelope(), Some("epoch_1"), "status", &reply("ibara_0123456789", json!("epoch_1"), generation));
            assert_eq!(err.err().unwrap().message, CHANGED);
        }
    }

    #[test]
    fn checked_reply_reports_refusals_with_their_code() {
        let err = checked_reply(&envelope(), Some("e"), "status", &json!({"error": {"code": "PERMISSION_DENIED", "message": "no"}}));
        let err = err.err().unwrap();
        assert_eq!((err.code, err.message.as_str()), ("PERMISSION_DENIED", "PERMISSION_DENIED: no"));
        let err = checked_reply(&envelope(), Some("e"), "status", &json!({"result": {"error": {"message": "busy"}}}));
        assert_eq!(err.err().unwrap().message, "OPERATOR_ERROR: busy");
    }

    #[test]
    fn checked_reply_output_keeps_the_node_key_order() {
        let ok = checked_reply(&envelope(), Some("epoch_1"), "status", &reply("ibara_0123456789", json!("epoch_1"), json!(3))).unwrap();
        assert_eq!(
            ok.to_string(),
            r#"{"environment_id":"operator_e","computer_id":"computer_a","endpoint_id":"ibara_0123456789","binding_revision":2,"request_id":"request_r","expected_authorization_generation":3,"record_id":"status","controller_epoch":"epoch_1","result":{"endpoint_id":"ibara_0123456789","controller_epoch":"epoch_1","authorization_generation":3,"ok":true}}"#
        );
    }

    #[test]
    fn requests_that_break_the_argv_rules_are_refused() {
        let base = OperatorRequest { computer: "c".into(), epoch: Some("epoch_1".into()), op: "status".into(), ..Default::default() };
        let shape = |r: OperatorRequest| r.check_shape().err().map(|e| e.message);
        assert!(shape(OperatorRequest { epoch: None, ..base.clone() }).unwrap().starts_with("Selected operator requires"));
        assert!(shape(OperatorRequest { op: "session".into(), ..base.clone() }).unwrap().starts_with("Selected operator requires"));
        assert_eq!(shape(OperatorRequest { op: "session".into(), epoch: None, ..base.clone() }), None);
        assert_eq!(
            shape(OperatorRequest { op: "observe".into(), display: Some("d".into()), quality: Some("huge".into()), ..base.clone() }).unwrap(),
            "Observation requires a named display and quality."
        );
        assert_eq!(shape(OperatorRequest { quality: Some("tile".into()), ..base.clone() }).unwrap(), "Only observation accepts display arguments.");
        assert_eq!(shape(OperatorRequest { task: Some("t".into()), ..base.clone() }).unwrap(), "Only task status accepts a task reference.");
        let pinned = OperatorRequest::check_action("files_list", json!({"endpoint_id": "x"})).err().unwrap();
        assert_eq!(pinned.message, "File action must not override pinned identity fields.");
        let control = OperatorRequest::check_action("take_control", json!({"expected_owner": "root", "expected_ownership_revision": "1"}));
        assert_eq!(control.err().unwrap().message, "Control requires an exact expected owner and ownership revision.");
        let pairing = OperatorRequest::check_action("pairing_confirm", json!({"challenge_ref": "pair_x", "nonce": "0", "operator_key_fingerprint": "SHA256:abc"}));
        assert_eq!(pairing.err().unwrap().message, "Pairing confirmation requires an exact challenge, nonce and key fingerprint.");
    }

    #[test]
    fn action_line_matches_the_node_field_order() {
        let request = OperatorRequest {
            computer: "c".into(),
            epoch: Some("epoch_1".into()),
            op: "observe".into(),
            display: Some("DP-1".into()),
            quality: Some("tile".into()),
            ..Default::default()
        };
        assert_eq!(
            request.action_for(&envelope()).to_string(),
            r#"{"op":"observe","endpoint_id":"ibara_0123456789","expected_authorization_generation":3,"controller_epoch":"epoch_1","display_id":"DP-1","quality":"tile"}"#
        );
        let files = OperatorRequest::from_fields("c", Some("epoch_1"), "files_list", json!({"root_id": "home", "relative_path": "."})).unwrap();
        assert_eq!(
            files.action_for(&envelope()).to_string(),
            r#"{"root_id":"home","relative_path":".","op":"files_list","endpoint_id":"ibara_0123456789","expected_authorization_generation":3,"controller_epoch":"epoch_1"}"#
        );
    }

    #[test]
    fn preview_requests_need_exactly_the_five_keys() {
        assert!(accept(br#"{"id":"bad id","computer":"c"}"#).is_none());
        assert!(accept(b"not json").is_none());
        let (id, refused) = accept(br#"{"id":"1","computer":"c","epoch":"e","display":"d","quality":"tile","x":1}"#).unwrap();
        assert_eq!(id, "1");
        assert!(refused.is_err());
        let (_, ok) = accept(br#"{"id":"2","computer":"c","epoch":"e","display":"d","quality":"selected"}"#).unwrap();
        assert_eq!(ok.unwrap().record_id(), "d");
    }
}
