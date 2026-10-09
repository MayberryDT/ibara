//! `ibara mcp [--directory-db FILE] [--computer NAME]`: the agent's MCP server on an
//! operator computer (replaces `agent/ibara-mcp` + `agent/ibara-mcp-selected.mjs`).
//!
//! A lazy relay. `initialize`, `notifications/initialized`, `ping` and `tools/list`
//! are answered here from the contract (`crate::mcp`), so an idle harness session
//! costs only this small process. A `tools/call` goes to one computer over its
//! selected `mcp` ssh route (a *link*), opened on first use and kept for the
//! session: the link performs the MCP handshake with the harness's own
//! `initialize` parameters and relays calls with rewritten JSON-RPC ids. A route
//! that cannot be opened, or that closes under a call, becomes a
//! `SESSION_UNAVAILABLE` tool result, never a crash; the next call tries again.
//!
//! With `--computer` every call goes to that computer. Without it one session
//! covers every verified computer in the operator directory, re-read at each call
//! so a computer added mid-session is usable at once:
//! - `computer_status()` asks every computer at once and lists them all;
//!   `help:` refs are answered here.
//! - `computer_begin({computer})` picks the computer by label, node, `cmp_` or
//!   `computer_` id or endpoint id (optional with one computer) and passes its
//!   `cmp_` id on; the returned `task_ref` is remembered.
//! - A call naming a `task_ref`, or a status `ref`, goes to the computer that owns
//!   it: remembered, the only computer, or found by asking each computer.
//! - `computer_procedures` goes to the computer of the latest task, else the first.
//!
//! Every answer calls a computer by the name the person gave it in this
//! console (situation lines, the fleet list, begin's `computer`, hints), not
//! by the computer's own name; a rename reaches the next answer.
//!
//! The first successful `computer_begin` is recorded in `onboarding.json`.

use super::directory::{ComputerNames, Named, OperatorDirectory, bind_fresh, computer_choices, directory_path, pick_computer};
use super::transport::{self, RouteKind};
use super::{LineRead, onboarding, read_line_bounded, runtime, unreachable_route};
use crate::contract::{Envelope, RefStatus, StatusResult};
use crate::error::{IbaraError, Result};
use crate::mcp::CallOutcome;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Longest harness request line (the target server's bound).
const CLIENT_LINE_LIMIT: usize = 4 * 1024 * 1024;
/// Longest target line (tool results may carry images).
const UPSTREAM_LINE_LIMIT: usize = 32 * 1024 * 1024;
/// ssh connect (10 s) plus the target's MCP handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(25);
/// After the harness leaves, how long the target may take to wind down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// After the harness closes stdin, how long calls already sent may take to answer.
const DRAIN_LIMIT: Duration = Duration::from_secs(120);
/// How long a question put to every computer waits for the slowest answer.
const FAN_OUT_LIMIT: Duration = Duration::from_secs(20);
/// The relay's id for its own `initialize` on the route.
const HANDSHAKE_ID: u64 = 0;
/// The name used in situation lines of answers that cover several computers.
const FLEET_LABEL: &str = "ibara";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Where calls go.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// One computer from the operator directory, rebound at every route opening.
    Selected { database: PathBuf, computer: String },
    /// Every verified computer in the operator directory.
    Fleet { database: PathBuf },
}

/// A startup failure: message and exit status.
#[derive(Debug, PartialEq)]
pub struct StartError {
    pub status: u8,
    pub message: String,
}

fn start_error(status: u8, message: impl Into<String>) -> StartError {
    StartError { status, message: message.into() }
}

/// Parse argv: `--computer NAME` pins one computer; without it the session covers
/// the fleet. `--directory-db` must be absolute.
pub fn parse_route(args: &[String]) -> std::result::Result<Route, StartError> {
    let usage = || start_error(1, "Usage: ibara mcp [--directory-db FILE] [--computer NAME]");
    let (mut computer, mut database): (Option<String>, Option<String>) = (None, None);
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).filter(|v| !v.is_empty()).cloned();
        match args[i].as_str() {
            "--computer" if computer.is_none() && value.is_some() => computer = value,
            "--directory-db" if database.is_none() && value.is_some() => database = value,
            _ => return Err(usage()),
        }
        i += 2;
    }
    if database.as_deref().is_some_and(|d| !Path::new(d).is_absolute()) {
        return Err(start_error(1, "The directory path must be absolute."));
    }
    let database = directory_path(database.as_deref());
    Ok(match computer {
        Some(computer) => Route::Selected { database, computer },
        None => Route::Fleet { database },
    })
}

/// Check the route at startup: the pinned computer, named any way a person or
/// agent names it, becomes its directory id. Returns the route and the name for
/// situation lines. Nothing is contacted; a fleet is read at each call.
pub fn check_route(route: Route) -> std::result::Result<(Route, String), StartError> {
    match route {
        Route::Selected { database, computer } => {
            let checked = OperatorDirectory::open(&database).and_then(|directory| {
                let row = directory.resolve_computer(&computer)?;
                directory.bind_operation(&row.computer_id, None, &format!("mcp_{}", uuid::Uuid::new_v4().hyphenated()))?;
                directory.close();
                Ok(row)
            });
            let row = checked.map_err(|e| start_error(2, e.message))?;
            Ok((Route::Selected { database, computer: row.computer_id }, row.label))
        }
        Route::Fleet { .. } => Ok((route, FLEET_LABEL.to_string())),
    }
}

/// `ibara mcp …`.
pub fn main(args: Vec<String>) -> ExitCode {
    let started = parse_route(&args).and_then(check_route);
    let (route, label) = match started {
        Ok(started) => started,
        Err(error) => {
            eprintln!("{}", error.message);
            return ExitCode::from(error.status);
        }
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(relay(route, label, tokio::io::stdin(), tokio::io::stdout())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// The front end: the harness's stdin and stdout.

/// Run the relay until the harness closes stdin.
pub async fn relay<R, W>(route: Route, label: String, input: R, mut out: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (client_tx, mut client) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut reader = BufReader::new(input);
        loop {
            let read = read_line_bounded(&mut reader, CLIENT_LINE_LIMIT, true).await;
            let done = !matches!(read, Ok(LineRead::Line(_) | LineRead::Oversize));
            if done || client_tx.send(read.unwrap_or(LineRead::Eof)).await.is_err() {
                return;
            }
        }
    });
    // Everything written to the harness: replies from call tasks and notifications
    // from the links, one writer.
    let (outbox_tx, mut outbox) = mpsc::unbounded_channel::<Value>();
    let router = Arc::new(Router::new(route, label, outbox_tx.clone()));
    let tools = crate::mcp::tools_list_json();
    let running = Arc::new(AtomicUsize::new(0));
    // After the harness closes stdin, calls already sent still get their replies,
    // for at most `DRAIN_LIMIT`.
    let mut drain_until: Option<tokio::time::Instant> = None;
    loop {
        if drain_until.is_some() && running.load(Ordering::SeqCst) == 0 {
            break;
        }
        let drained = async {
            match drain_until {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            line = client.recv(), if drain_until.is_none() => match line {
                None | Some(LineRead::Eof) => drain_until = Some(tokio::time::Instant::now() + DRAIN_LIMIT),
                Some(LineRead::Oversize) => write(&mut out, &rpc_error(Value::Null, PARSE_ERROR, "request line too long")).await?,
                Some(LineRead::Line(line)) => {
                    if let Some(reply) = on_client_line(&router, &tools, &line, &outbox_tx, &running) {
                        write(&mut out, &reply).await?;
                    }
                }
            },
            Some(message) = outbox.recv() => write(&mut out, &message).await?,
            _ = drained => break,
        }
    }
    // Replies sent just before the last call task finished.
    while let Ok(message) = outbox.try_recv() {
        write(&mut out, &message).await?;
    }
    router.shutdown().await;
    Ok(())
}

async fn write<W: AsyncWrite + Unpin>(out: &mut W, message: &Value) -> std::io::Result<()> {
    let mut line = message.to_string();
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One harness line: a reply to write now, or nothing (a notification, or a
/// `tools/call` handed to its own task).
fn on_client_line(
    router: &Arc<Router>,
    tools: &Value,
    line: &[u8],
    outbox: &mpsc::UnboundedSender<Value>,
    running: &Arc<AtomicUsize>,
) -> Option<Value> {
    if line.trim_ascii().is_empty() {
        return None;
    }
    let message: Value = match serde_json::from_slice(line) {
        Ok(message) => message,
        Err(e) => return Some(rpc_error(Value::Null, PARSE_ERROR, &format!("parse error: {e}"))),
    };
    let Value::Object(message) = message else {
        return Some(rpc_error(Value::Null, INVALID_REQUEST, "expected one JSON-RPC object per line"));
    };
    // A response to a request the relay never sends to the harness.
    let method = message.get("method").and_then(Value::as_str)?.to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = message.get("id").cloned() else {
        if method == "notifications/cancelled" {
            router.cancel(&params);
        }
        return None;
    };
    match method.as_str() {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).map(str::to_string);
            *lock(&router.init) = Some(params);
            Some(json!({"jsonrpc": "2.0", "id": id, "result": crate::mcp::initialize_result(requested.as_deref())}))
        }
        "ping" => Some(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => Some(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})),
        "tools/call" => {
            let known = |name: &str| tools.as_array().is_some_and(|t| t.iter().any(|t| t.get("name").and_then(Value::as_str) == Some(name)));
            match params.get("name").and_then(Value::as_str) {
                None => Some(rpc_error(id, INVALID_PARAMS, "tools/call needs a tool name")),
                Some(name) if !known(name) => Some(rpc_error(id, INVALID_PARAMS, &format!("unknown tool: {name}"))),
                Some(_) => {
                    running.fetch_add(1, Ordering::SeqCst);
                    // Registered before the task starts, so a cancellation that follows at once is seen.
                    let call = id.to_string();
                    lock(&router.calls).insert(call.clone(), CallEntry::default());
                    let (router, outbox, running) = (router.clone(), outbox.clone(), running.clone());
                    tokio::spawn(async move {
                        let answer = router.call(&call, message).await;
                        let cancelled = lock(&router.calls).remove(&call).is_some_and(|c| c.cancelled);
                        if let Some(answer) = answer.filter(|_| !cancelled) {
                            let mut reply = Map::new();
                            reply.insert("jsonrpc".into(), json!("2.0"));
                            reply.insert("id".into(), id);
                            reply.extend(answer.into_iter().filter(|(k, _)| k != "jsonrpc" && k != "id"));
                            let _ = outbox.send(Value::Object(reply));
                        }
                        running.fetch_sub(1, Ordering::SeqCst);
                    });
                    None
                }
            }
        }
        other => Some(rpc_error(id, METHOD_NOT_FOUND, &format!("method not found: {other}"))),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Routing: which computer answers a call.

/// One computer a call can go to.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    computer_id: String,
    endpoint_id: String,
    label: String,
    host: String,
}

impl Named for Target {
    fn names(&self) -> ComputerNames<'_> {
        ComputerNames { computer_id: &self.computer_id, endpoint_id: &self.endpoint_id, label: &self.label, host: &self.host }
    }
}

impl Target {
    /// The target's own id for itself: `cmp_` plus the directory id's digest.
    fn cmp_id(&self) -> String {
        self.names().cmp_id()
    }
}

enum Mode {
    Single { database: PathBuf, target: Target },
    Fleet { database: PathBuf },
}

/// A call in progress: whether the harness cancelled it and which link calls it made.
#[derive(Default)]
struct CallEntry {
    cancelled: bool,
    sent: Vec<(String, u64)>,
}

/// What one computer said to a question put to several.
enum Answer {
    /// The computer answered with a tool result.
    Result { reply: Map<String, Value>, envelope: Option<Value> },
    /// The computer could not be asked.
    Unreachable(String),
}

struct Router {
    mode: Mode,
    label: String,
    init: Arc<Mutex<Option<Value>>>,
    outbox: mpsc::UnboundedSender<Value>,
    links: Mutex<HashMap<String, LinkHandle>>,
    next_key: AtomicU64,
    /// Refs the relay has seen a computer hand out (`task_ref` from begin), and the
    /// computers found to own refs by asking.
    owners: Mutex<HashMap<String, String>>,
    /// The computer of the latest task begun in this session.
    latest: Mutex<Option<String>>,
    calls: Mutex<HashMap<String, CallEntry>>,
}

impl Router {
    fn new(route: Route, label: String, outbox: mpsc::UnboundedSender<Value>) -> Self {
        let mode = match route {
            Route::Selected { database, computer } => {
                let target = Target { computer_id: computer, endpoint_id: String::new(), label: label.clone(), host: String::new() };
                Mode::Single { database, target }
            }
            Route::Fleet { database } => Mode::Fleet { database },
        };
        Router {
            mode,
            label,
            init: Arc::new(Mutex::new(None)),
            outbox,
            links: Mutex::new(HashMap::new()),
            next_key: AtomicU64::new(1),
            owners: Mutex::new(HashMap::new()),
            latest: Mutex::new(None),
            calls: Mutex::new(HashMap::new()),
        }
    }

    /// Drop every link and let each close its route.
    async fn shutdown(&self) {
        let links: Vec<LinkHandle> = lock(&self.links).drain().map(|(_, link)| link).collect();
        for link in links {
            drop(link.tx);
            let _ = link.task.await;
        }
    }

    /// `notifications/cancelled`: stop what the call sent and never answer it.
    fn cancel(&self, params: &Value) {
        let Some(request_id) = params.get("requestId") else { return };
        let sent = {
            let mut calls = lock(&self.calls);
            let Some(entry) = calls.get_mut(&request_id.to_string()) else { return };
            entry.cancelled = true;
            std::mem::take(&mut entry.sent)
        };
        let links = lock(&self.links);
        for (computer, key) in sent {
            if let Some(link) = links.get(&computer) {
                let _ = link.tx.send(LinkCmd::Cancel { key, params: params.clone() });
            }
        }
    }

    fn database(&self) -> &Path {
        match &self.mode {
            Mode::Single { database, .. } | Mode::Fleet { database } => database,
        }
    }

    /// The link to `target`, opened lazily.
    fn link(&self, target: &Target) -> mpsc::UnboundedSender<LinkCmd> {
        let mut links = lock(&self.links);
        let link = links.entry(target.computer_id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let link = Link::new(target.clone(), self.database().to_path_buf(), self.init.clone(), self.outbox.clone());
            LinkHandle { tx, task: tokio::spawn(link.run(rx)) }
        });
        link.tx.clone()
    }

    /// Send one `tools/call` to `target`; `None` when the harness cancelled it.
    async fn send(&self, call: &str, target: &Target, message: Map<String, Value>) -> Option<Map<String, Value>> {
        let key = self.next_key.fetch_add(1, Ordering::SeqCst);
        {
            let mut calls = lock(&self.calls);
            let entry = calls.entry(call.to_string()).or_default();
            if entry.cancelled {
                return None;
            }
            entry.sent.push((target.computer_id.clone(), key));
        }
        let (reply, answered) = oneshot::channel();
        if self.link(target).send(LinkCmd::Call { key, message, reply }).is_err() {
            let error = unreachable_route(format!("{} is not reachable: the route ended. Nothing was sent.", target.label));
            return Some(tool_reply(crate::mcp::error_tool_result(&error, &format!("{} · not reachable", target.label))));
        }
        answered.await.ok().map(|reply| in_console_words(reply, target))
    }

    /// Answer one `tools/call`; `None` when it was cancelled.
    async fn call(self: &Arc<Self>, call: &str, mut message: Map<String, Value>) -> Option<Map<String, Value>> {
        let name = message.get("params").and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or_default().to_string();
        let args = message.get("params").and_then(|p| p.get("arguments")).cloned().unwrap_or(Value::Null);
        let database = match &self.mode {
            Mode::Single { database, target } => {
                let mut target = target.clone();
                if let Some(label) = current_label(database, &target.computer_id) {
                    target.label = label;
                }
                let reply = self.send(call, &target, message).await?;
                if name == "computer_begin" {
                    self.note_begin(&target, &reply);
                }
                return Some(reply);
            }
            Mode::Fleet { database } => database.clone(),
        };
        let fleet = match fleet_targets(&database) {
            Ok(fleet) => fleet,
            Err(error) => return Some(self.local_error(&unreachable_route(format!("Your computers could not be read: {}", error.message)), 0)),
        };
        let reference = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        match name.as_str() {
            "computer_status" => match reference("ref") {
                None => self.fleet_status(call, &fleet).await,
                Some(r) if r.starts_with("help:") => Some(self.local_help(&r, fleet.len())),
                Some(r) => match self.owner(call, &r, &fleet).await? {
                    Some(target) => self.send(call, &target, message).await,
                    None => Some(self.local_ok(StatusResult::Ref(ended(&r)), fleet.len())),
                },
            },
            "computer_begin" if reference("computer").is_none_or(|s|s.trim().is_empty()) => self.begin_any(call, message, &fleet).await,
            "computer_begin" => {
                let target = match pick(&fleet, reference("computer").as_deref()) {
                    Ok(target) => target.clone(),
                    Err(error) => return Some(self.local_error(&error, fleet.len())),
                };
                if let Some(Value::Object(args)) = message.get_mut("params").and_then(|p| p.get_mut("arguments")) {
                    args.insert("computer".into(), json!(target.cmp_id()));
                }
                let reply = self.send(call, &target, message).await?;
                self.note_begin(&target, &reply);
                Some(reply)
            }
            "computer_procedures" => {
                let latest = lock(&self.latest).clone();
                let target = latest.and_then(|id| fleet.iter().find(|t| t.computer_id == id)).or(fleet.first()).cloned();
                match target {
                    Some(target) => self.send(call, &target, message).await,
                    None => Some(self.local_error(&no_computers(), 0)),
                }
            }
            _ => {
                let Some(task_ref) = reference("task_ref") else {
                    // The computer names the missing field; any computer will do.
                    return match fleet.first().cloned() {
                        Some(target) => self.send(call, &target, message).await,
                        None => Some(self.local_error(&no_computers(), 0)),
                    };
                };
                match self.owner(call, &task_ref, &fleet).await? {
                    Some(target) => self.send(call, &target, message).await,
                    None => {
                        let error = IbaraError::new(
                            "INVALID_ARGUMENT",
                            format!("task_ref: '{}' is not an open task on any of your computers; computer_status() lists them and computer_begin starts a task.", clip(&task_ref, 60)),
                            false,
                        );
                        Some(self.local_error(&error, fleet.len()))
                    }
                }
            }
        }
    }

    /// Ordinary work needs no machine name. Status probes are read-only; only one
    /// begin is sent at a time. Its route is durable before sending any effects.
    async fn begin_any(self: &Arc<Self>, call: &str, message: Map<String,Value>, fleet: &[Target]) -> Option<Map<String,Value>> {
        if fleet.is_empty() {return Some(self.local_error(&no_computers(),0));}
        let args=message["params"]["arguments"].clone();
        let request=args["request_id"].as_str().filter(|s|!s.is_empty() && s.len()<=200);
        let Some(request)=request else {return Some(self.local_error(&crate::error::invalid("request_id: give a nonempty request identity (at most 200 characters)."),fleet.len()));};
        // Client identity is part of the operator namespace; no goal or login
        // text is retained in the directory, only digests and a computer ID.
        let identity=lock(&self.init).as_ref().and_then(|i|i.pointer("/clientInfo/name")).and_then(Value::as_str).unwrap_or("agent").to_string();
        let key=format!("{:x}",Sha256::digest(format!("{identity}:{request}").as_bytes()));
        let fingerprint=format!("{:x}",Sha256::digest(crate::store::canonical::canonicalize(&args).as_bytes()));
        let route=|candidate:Option<&str>,refused:Option<&str>| -> Result<Option<String>> {
            OperatorDirectory::open(self.database())?.auto_begin_route(&key,&fingerprint,candidate,refused)
        };
        let bound=match route(None,None) {Ok(bound)=>bound,Err(e)=>return Some(self.local_error(&e,fleet.len()))};
        let answers=if bound.is_none() {self.ask_all(call,fleet,json!({})).await?} else {Vec::new()};
        let mut ready:Vec<Target>=answers.iter().filter_map(|(t,a)|match a {
            Answer::Result{envelope:Some(e),..} if e["status"]=="ok" && e.pointer("/result/access/capabilities/agents").and_then(Value::as_str)!=Some("deny")
                && e.pointer("/result/computers").and_then(Value::as_array).is_some_and(|cs|cs.iter().any(|c|c["state"]=="ready"))=>Some(t.clone()),
            _=>None,
        }).collect();
        // Stable request-specific ordering spreads unrelated agents across free
        // computers; replay goes to the durable binding, regardless of readiness.
        ready.sort_by_key(|t|Sha256::digest(format!("{key}:{}",t.computer_id).as_bytes()).to_vec());
        let mut selected=bound;
        let mut refused=None::<String>;
        let mut last=None;
        loop {
            if selected.is_none() {
                let Some(candidate)=ready.first().cloned() else {break;};
                ready.remove(0);
                selected=match route(Some(&candidate.computer_id),refused.as_deref()) {
                    Ok(r)=>r,Err(e)=>return Some(self.local_error(&e,fleet.len()))
                };
            }
            let id=selected.take().unwrap();
            ready.retain(|t|t.computer_id!=id);
            let Some(target)=fleet.iter().find(|t|t.computer_id==id) else {
                return Some(self.local_error(&unreachable_route("The original computer for this request is unavailable. Inspect its task/receipt; do not start this request elsewhere."),fleet.len()));
            };
            let mut outgoing=message.clone();
            outgoing.get_mut("params")?.get_mut("arguments")?.as_object_mut()?.insert("computer".into(),json!(target.cmp_id()));
            let reply=self.send(call,target,outgoing).await?;
            self.note_begin(target,&reply);
            let envelope=reply.get("result").and_then(envelope_of);
            let definitely_refused=envelope.as_ref().is_some_and(|e|e["status"]=="error"
                && e.pointer("/result/task_ref").is_none()
                && matches!(e.pointer("/error/code").and_then(Value::as_str),Some("BUSY"|"HUMAN_CONTROL"|"CONTROL_UNSETTLED")));
            if !definitely_refused {return Some(reply);}
            // A bound replay never selects a new host after an uncertain send.
            // These three errors are pre-acquisition refusals, retained by the target.
            refused=Some(id); last=Some(reply);
        }
        if let Some(reply)=last {return Some(reply);}
        let states=answers.iter().map(|(t,a)|{
            let state=match a {Answer::Result{envelope:Some(e),..}=>e.pointer("/result/computers/0/state").and_then(Value::as_str).unwrap_or("unavailable"),_=>"unreachable"};
            format!("{}: {state}",t.label)
        }).collect::<Vec<_>>().join(", ");
        Some(self.local_error(&IbaraError::new("BUSY",format!("No free computer right now ({states}). Continue independent work; retry computer_begin without computer with a fresh request_id. No task started."),true),fleet.len()))
    }

    /// The computer that owns `reference`: remembered, named by it, the only one, or
    /// the one that reports it open. `Err`-free: `None` from `?` means cancelled.
    async fn owner(self: &Arc<Self>, call: &str, reference: &str, fleet: &[Target]) -> Option<Option<Target>> {
        let remembered = lock(&self.owners).get(reference).cloned();
        if let Some(target) = remembered.and_then(|id| fleet.iter().find(|t| t.computer_id == id)) {
            return Some(Some(target.clone()));
        }
        if reference.starts_with("cmp_") || reference.starts_with("computer_") {
            return Some(fleet.iter().find(|t| t.names().answers_to(reference)).cloned());
        }
        if fleet.len() <= 1 {
            return Some(fleet.first().cloned());
        }
        let answers = self.ask_all(call, fleet, json!({"ref": reference})).await?;
        let open = answers.into_iter().find_map(|(target, answer)| match answer {
            Answer::Result { envelope: Some(envelope), .. }
                if envelope.get("status").and_then(Value::as_str) == Some("ok")
                    && !matches!(envelope.pointer("/result/state").and_then(Value::as_str), None | Some("ended" | "expired")) =>
            {
                Some(target)
            }
            _ => None,
        });
        if let Some(target) = &open {
            lock(&self.owners).insert(reference.to_string(), target.computer_id.clone());
        }
        Some(open)
    }

    /// `computer_status({ref})` to every computer at once; unanswered by the deadline
    /// counts as not reachable. `None` when cancelled.
    async fn ask_all(self: &Arc<Self>, call: &str, fleet: &[Target], arguments: Value) -> Option<Vec<(Target, Answer)>> {
        let deadline = tokio::time::Instant::now() + FAN_OUT_LIMIT;
        let asks: Vec<(Target, JoinHandle<Option<Map<String, Value>>>)> = fleet
            .iter()
            .map(|target| {
                let message = json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "computer_status", "arguments": arguments}});
                let Value::Object(message) = message else { unreachable!() };
                let (router, call, t) = (self.clone(), call.to_string(), target.clone());
                (target.clone(), tokio::spawn(async move { router.send(&call, &t, message).await }))
            })
            .collect();
        let mut answers = Vec::with_capacity(asks.len());
        for (target, ask) in asks {
            let answer = match tokio::time::timeout_at(deadline, ask).await {
                Ok(Ok(Some(reply))) => classify(reply),
                Ok(Ok(None)) => return None,
                Ok(Err(_)) => Answer::Unreachable("the question was lost.".into()),
                Err(_) => Answer::Unreachable("it did not answer in time.".into()),
            };
            answers.push((target, answer));
        }
        Some(answers)
    }

    /// `computer_status()`: every computer, each as it describes itself.
    async fn fleet_status(self: &Arc<Self>, call: &str, fleet: &[Target]) -> Option<Map<String, Value>> {
        if fleet.is_empty() {
            let envelope = Envelope::ok(
                format!("{FLEET_LABEL} · no computers yet · add one in the ibara console"),
                Vec::new(),
                StatusResult::Fleet { computers: Vec::new() },
            );
            return Some(tool_reply(crate::mcp::call_result(&CallOutcome::new(envelope.to_value()))));
        }
        let mut answers = self.ask_all(call, fleet, json!({})).await?;
        if answers.len() == 1
            && let (_, Answer::Result { envelope: Some(envelope), .. }) = &answers[0]
            && envelope.get("status").and_then(Value::as_str) == Some("ok")
        {
            let Some((_, Answer::Result { reply, .. })) = answers.pop() else { unreachable!() };
            return Some(reply);
        }
        let (mut computers, mut since, mut words) = (Vec::new(), Vec::new(), Vec::new());
        for (target, answer) in answers {
            let listed = match &answer {
                Answer::Result { envelope: Some(envelope), .. } if envelope.get("status").and_then(Value::as_str) == Some("ok") => {
                    envelope.pointer("/result/computers").and_then(Value::as_array).cloned().map(|items| (items, envelope))
                }
                _ => None,
            };
            match listed {
                Some((items, envelope)) => {
                    for mut item in items {
                        if let (Value::Object(fields), Some(access)) = (&mut item, envelope.pointer("/result/access")) {
                            fields.insert("access".into(), access.clone());
                        }
                        words.push(format!("{} {}", str_of(&item, "name"), str_of(&item, "state")));
                        computers.push(item);
                    }
                    for event in envelope.get("since").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                        since.push(format!("{}: {event}", target.label));
                    }
                }
                None => {
                    let why = match answer {
                        Answer::Unreachable(message) => message,
                        Answer::Result { envelope, .. } => envelope
                            .as_ref()
                            .and_then(|e| e.pointer("/error/message").and_then(Value::as_str))
                            .unwrap_or("it gave no status.")
                            .to_string(),
                    };
                    words.push(format!("{} offline", target.label));
                    computers.push(json!({
                        "id": target.cmp_id(), "name": target.label, "state": "offline",
                        "capabilities": if why.contains("not reachable") { clip(&why, 240) } else { format!("not reachable: {}", clip(&why, 220)) },
                    }));
                }
            }
        }
        let situation = clip(&format!("{FLEET_LABEL} · {} computers: {}", computers.len(), words.join(", ")), 200);
        let envelope = Envelope::ok(situation, since, json!({"computers": computers}));
        Some(tool_reply(crate::mcp::call_result(&CallOutcome::new(envelope.to_value()))))
    }

    /// Remember who owns a new task, and record the first one ever begun.
    fn note_begin(&self, target: &Target, reply: &Map<String, Value>) {
        let Some(envelope) = reply.get("result").and_then(envelope_of) else { return };
        if envelope.get("status").and_then(Value::as_str) != Some("ok") {
            return;
        }
        let Some(task_ref) = envelope.pointer("/result/task_ref").and_then(Value::as_str) else { return };
        lock(&self.owners).insert(task_ref.to_string(), target.computer_id.clone());
        *lock(&self.latest) = Some(target.computer_id.clone());
        if let Err(error) = onboarding::record_first_task(&onboarding::path()) {
            eprintln!("ibara mcp: could not record the first task: {error}");
        }
    }

    fn situation(&self, computers: usize) -> String {
        match computers {
            1 => format!("{} · 1 computer", self.label),
            n => format!("{} · {n} computers", self.label),
        }
    }

    fn local_ok(&self, result: StatusResult, computers: usize) -> Map<String, Value> {
        let envelope = Envelope::ok(self.situation(computers), Vec::new(), result);
        tool_reply(crate::mcp::call_result(&CallOutcome::new(envelope.to_value())))
    }

    fn local_error(&self, error: &IbaraError, computers: usize) -> Map<String, Value> {
        tool_reply(crate::mcp::error_tool_result(error, &self.situation(computers)))
    }

    fn local_help(&self, reference: &str, computers: usize) -> Map<String, Value> {
        let tool = reference.trim_start_matches("help:");
        match crate::contract::help(tool) {
            Some(help) => self.local_ok(StatusResult::Help { tool: tool.to_string(), help }, computers),
            None => {
                let message = format!("ref: no tool named '{}'; see the tool list", clip(tool, 60));
                self.local_error(&IbaraError::new("INVALID_ARGUMENT", message, false), computers)
            }
        }
    }
}

/// The verified computers of the directory, in the order they were added. A
/// directory that does not exist yet has none (and is not created here).
fn fleet_targets(database: &Path) -> Result<Vec<Target>> {
    if !database.exists() {
        return Ok(Vec::new());
    }
    let directory = OperatorDirectory::open(database)?;
    let listed = directory.list_computers();
    directory.close();
    Ok(listed?
        .into_iter()
        .filter(|c| c.trust_state == "verified")
        .map(|c| Target { computer_id: c.computer_id, endpoint_id: c.endpoint_id, label: c.label, host: c.host })
        .collect())
}

/// The name the person gave `computer` in this console now, so a rename made
/// during a pinned session reaches its next answer.
fn current_label(database: &Path, computer: &str) -> Option<String> {
    if !database.exists() {
        return None;
    }
    let directory = OperatorDirectory::open(database).ok()?;
    let row = directory.get_computer(computer);
    directory.close();
    row.ok().flatten().map(|r| r.label).filter(|l| !l.trim().is_empty())
}

/// A computer's answer in this console's words. Each computer names itself
/// (its situation line, its row in the fleet list, begin's `computer`, the
/// hints); the name the person gave it here replaces that, so an agent calls
/// the computer what the person calls it, whichever ibara the computer runs.
fn in_console_words(mut reply: Map<String, Value>, target: &Target) -> Map<String, Value> {
    let Some(result) = reply.get("result") else { return reply };
    let Some(mut envelope) = envelope_of(result) else { return reply };
    if !call_it(&mut envelope, target) {
        return reply;
    }
    let images = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|part| {
            let text = |key: &str| part.get(key).and_then(Value::as_str).map(str::to_string);
            Some(crate::mcp::Image { base64: text("data")?, mime: text("mimeType")? })
        })
        .collect();
    reply.insert("result".into(), crate::mcp::call_result(&CallOutcome { envelope, images }));
    reply
}

/// Rename the computer in `envelope` to `target`'s label; false when it
/// already has that name. The situation line starts with the computer's own name.
fn call_it(envelope: &mut Value, target: &Target) -> bool {
    let label = target.label.trim();
    let Some(situation) = envelope.get("situation").and_then(Value::as_str) else { return false };
    let (own, rest) = match situation.split_once(" · ") {
        Some((own, rest)) => (own.to_string(), Some(rest.to_string())),
        None => (situation.to_string(), None),
    };
    if label.is_empty() || own == label {
        return false;
    }
    envelope["situation"] = json!(match rest {
        Some(rest) => format!("{label} · {rest}"),
        None => label.to_string(),
    });
    let id = target.cmp_id();
    let is_self = |v: &Value| v.get("id").and_then(Value::as_str) == Some(id.as_str()) || v.get("name").and_then(Value::as_str) == Some(own.as_str());
    let result = &mut envelope["result"];
    if let Some(rows) = result.get_mut("computers").and_then(Value::as_array_mut) {
        for row in rows.iter_mut().filter(|row| is_self(row)) {
            row["name"] = json!(label);
        }
    }
    if let Some(computer) = result.get_mut("computer").filter(|c| is_self(c)) {
        computer["name"] = json!(label);
    }
    // computer_status({ref: cmp_…}): "Name · holder · capabilities".
    if result.get("kind").and_then(Value::as_str) == Some("computer")
        && let Some(summary) = result.get("summary").and_then(Value::as_str)
        && let Some(rest) = summary.strip_prefix(&format!("{own} · "))
    {
        result["summary"] = json!(format!("{label} · {rest}"));
    }
    // Hints such as `computer_begin({computer: "Name", goal, request_id})`.
    if let Some(hints) = result.get_mut("next").and_then(Value::as_array_mut) {
        for hint in hints.iter_mut() {
            if let Some(text) = hint.as_str()
                && let Some(start) = text.find("computer: \"").map(|at| at + "computer: \"".len())
                && let Some(length) = text[start..].find('"')
            {
                *hint = json!(format!("{}{label}{}", &text[..start], &text[start + length..]));
            }
        }
    }
    true
}

/// The computer `computer_begin` names; optional with exactly one computer.
fn pick<'a>(fleet: &'a [Target], name: Option<&str>) -> std::result::Result<&'a Target, IbaraError> {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    match (fleet, name) {
        ([], _) => Err(no_computers()),
        ([only], None) => Ok(only),
        (_, None) => Err(IbaraError::new("INVALID_ARGUMENT", format!("computer: name one of your computers: {}", computer_choices(fleet)), false)),
        (_, Some(name)) => pick_computer(fleet, name).map_err(|mut e| {
            e.message = format!("computer: {}", e.message);
            e
        }),
    }
}

fn no_computers() -> IbaraError {
    IbaraError::new("SESSION_UNAVAILABLE", "No computers are added yet. Add one in the ibara console, then call computer_status().", true)
}

/// The answer for a ref no computer reports: ended, with where to look instead.
fn ended(reference: &str) -> RefStatus {
    let kind = match reference.split('_').next().unwrap_or_default() {
        "task" => "task",
        "op" => "op",
        "att" => "attention",
        "art" | "artifact" => "artifact",
        "frame" => "frame",
        "cmp" | "computer" => "computer",
        _ => "reference",
    };
    RefStatus {
        details: None,
        reference: reference.to_string(),
        kind: kind.into(),
        state: "ended".into(),
        summary: Some("Not found on any of your computers.".into()),
        parent: None,
        children: Vec::new(),
        next: vec!["computer_status() lists your computers".into()],
    }
}

fn classify(reply: Map<String, Value>) -> Answer {
    if let Some(error) = reply.get("error") {
        return Answer::Unreachable(error.get("message").and_then(Value::as_str).unwrap_or("it refused the question.").to_string());
    }
    let envelope = reply.get("result").and_then(envelope_of);
    Answer::Result { reply, envelope }
}

/// The contract envelope of a tool result: `structuredContent`, or, when the result
/// carries images, the text part that holds the whole envelope.
pub(crate) fn envelope_of(result: &Value) -> Option<Value> {
    if let Some(structured) = result.get("structuredContent").filter(|s| s.is_object()) {
        return Some(structured.clone());
    }
    result.get("content")?.as_array()?.iter().rev().find_map(|part| {
        let text = part.get("text").and_then(Value::as_str)?;
        let value: Value = serde_json::from_str(text).ok()?;
        value.get("status").and_then(Value::as_str).is_some().then_some(value)
    })
}

fn tool_reply(result: Value) -> Map<String, Value> {
    let mut reply = Map::new();
    reply.insert("jsonrpc".into(), json!("2.0"));
    reply.insert("result".into(), result);
    reply
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("?")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Links: one computer's route for the session.

enum LinkCmd {
    Call { key: u64, message: Map<String, Value>, reply: oneshot::Sender<Map<String, Value>> },
    Cancel { key: u64, params: Value },
}

struct LinkHandle {
    tx: mpsc::UnboundedSender<LinkCmd>,
    task: JoinHandle<()>,
}

/// An open route: the ssh child and its line stream.
struct Upstream {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<LineRead>,
}

impl Upstream {
    async fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await
    }

    /// End stdin so the target session winds down, then make sure ssh is gone.
    async fn shutdown(self) {
        let Upstream { mut child, stdin, events } = self;
        drop(stdin);
        drop(events);
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

enum State {
    Idle,
    Opening(oneshot::Receiver<Result<Upstream>>),
    Open(Upstream),
}

enum Event {
    Cmd(LinkCmd),
    Opened(Result<Upstream>),
    Upstream(Option<LineRead>),
}

type Reply = oneshot::Sender<Map<String, Value>>;

struct Link {
    target: Target,
    database: PathBuf,
    init: Arc<Mutex<Option<Value>>>,
    outbox: mpsc::UnboundedSender<Value>,
    /// Calls waiting for the route to open.
    queued: Vec<(u64, Map<String, Value>, Reply)>,
    /// Relayed calls: route id → (call key, reply).
    inflight: HashMap<u64, (u64, Reply)>,
    next_id: u64,
}

impl Link {
    fn new(target: Target, database: PathBuf, init: Arc<Mutex<Option<Value>>>, outbox: mpsc::UnboundedSender<Value>) -> Self {
        Link { target, database, init, outbox, queued: Vec::new(), inflight: HashMap::new(), next_id: HANDSHAKE_ID + 1 }
    }

    fn situation(&self) -> String {
        format!("{} · not reachable", self.target.label)
    }

    fn fail(&self, reply: Reply, error: &IbaraError) {
        let _ = reply.send(tool_reply(crate::mcp::error_tool_result(error, &self.situation())));
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<LinkCmd>) {
        let mut state = State::Idle;
        loop {
            let event = {
                let upstream = async {
                    match &mut state {
                        State::Idle => std::future::pending().await,
                        State::Opening(opened) => {
                            Event::Opened(opened.await.unwrap_or_else(|_| Err(unreachable_route("The route task ended."))))
                        }
                        State::Open(up) => Event::Upstream(up.events.recv().await),
                    }
                };
                tokio::select! {
                    cmd = cmds.recv() => match cmd {
                        Some(cmd) => Event::Cmd(cmd),
                        None => break,
                    },
                    event = upstream => event,
                }
            };
            match event {
                Event::Cmd(LinkCmd::Call { key, message, reply }) => self.call(key, message, reply, &mut state).await,
                Event::Cmd(LinkCmd::Cancel { key, params }) => self.cancel(key, params, &mut state).await,
                Event::Opened(Ok(upstream)) => {
                    state = State::Open(upstream);
                    for (key, message, reply) in std::mem::take(&mut self.queued) {
                        self.forward(key, message, reply, &mut state).await;
                    }
                }
                Event::Opened(Err(error)) => {
                    state = State::Idle;
                    let error = unreachable_route(format!("{} is not reachable: {} Nothing was sent.", self.target.label, error.message));
                    for (_, _, reply) in std::mem::take(&mut self.queued) {
                        self.fail(reply, &error);
                    }
                }
                Event::Upstream(Some(LineRead::Line(line))) => self.on_upstream_line(&line, &mut state).await,
                Event::Upstream(_) => self.route_lost(&mut state),
            }
        }
        if let State::Open(upstream) = state {
            upstream.shutdown().await;
        }
    }

    async fn call(&mut self, key: u64, message: Map<String, Value>, reply: Reply, state: &mut State) {
        match state {
            State::Open(_) => self.forward(key, message, reply, state).await,
            State::Opening(_) => self.queued.push((key, message, reply)),
            State::Idle => {
                self.queued.push((key, message, reply));
                let (done, opened) = oneshot::channel();
                let (database, computer) = (self.database.clone(), self.target.computer_id.clone());
                let params = lock(&self.init).clone().unwrap_or_else(default_initialize_params);
                tokio::spawn(async move {
                    let _ = done.send(open(&database, &computer, params).await);
                });
                *state = State::Opening(opened);
            }
        }
    }

    async fn forward(&mut self, key: u64, mut message: Map<String, Value>, reply: Reply, state: &mut State) {
        let State::Open(upstream) = state else {
            self.queued.push((key, message, reply));
            return;
        };
        let route_id = self.next_id;
        self.next_id += 1;
        message.insert("id".into(), json!(route_id));
        self.inflight.insert(route_id, (key, reply));
        if upstream.send(&Value::Object(message)).await.is_err() {
            self.route_lost(state);
        }
    }

    /// Drop a queued call, or pass the cancellation on with the route's id. The
    /// dropped reply tells the call it was cancelled; a late reply is ignored.
    async fn cancel(&mut self, key: u64, params: Value, state: &mut State) {
        self.queued.retain(|(k, _, _)| *k != key);
        let route_id = self.inflight.iter().find(|(_, (k, _))| *k == key).map(|(route_id, _)| *route_id);
        if let Some(route_id) = route_id {
            self.inflight.remove(&route_id);
            if let State::Open(upstream) = state {
                let mut params = params;
                params["requestId"] = json!(route_id);
                let _ = upstream.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": params})).await;
            }
        }
    }

    async fn on_upstream_line(&mut self, line: &[u8], state: &mut State) {
        let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(line) else { return };
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        match (method, message.get("id").cloned()) {
            // A request from the target: answer ping, refuse the rest.
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    rpc_error(id, METHOD_NOT_FOUND, &format!("method not found: {method}"))
                };
                if let State::Open(upstream) = state
                    && upstream.send(&reply).await.is_err()
                {
                    self.route_lost(state);
                }
            }
            // A notification (progress, log, list changed): pass it on.
            (Some(_), None) => {
                let _ = self.outbox.send(Value::Object(message));
            }
            (None, Some(route_id)) => {
                if let Some((_, reply)) = route_id.as_u64().and_then(|r| self.inflight.remove(&r)) {
                    message.remove("id");
                    let _ = reply.send(message);
                }
            }
            (None, None) => {}
        }
    }

    /// The route closed: every relayed call without a reply may or may not have run.
    fn route_lost(&mut self, state: &mut State) {
        if let State::Open(upstream) = std::mem::replace(state, State::Idle) {
            tokio::spawn(upstream.shutdown());
        }
        let error = IbaraError::new(
            "SESSION_UNAVAILABLE",
            format!("The connection to {} closed before the reply; the call may have run.", self.target.label),
            false,
        )
        .requires_reconciliation();
        let mut lost: Vec<(u64, (u64, Reply))> = self.inflight.drain().collect();
        lost.sort_by_key(|(route_id, _)| *route_id);
        for (_, (_, reply)) in lost {
            self.fail(reply, &error);
        }
    }
}

fn default_initialize_params() -> Value {
    json!({
        "protocolVersion": crate::mcp::PROTOCOL_VERSIONS[0],
        "capabilities": {},
        "clientInfo": {"name": "ibara-mcp", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Open the computer's route and complete the MCP handshake with the harness's parameters.
async fn open(database: &Path, computer: &str, params: Value) -> Result<Upstream> {
    let record = format!("mcp_{}", uuid::Uuid::new_v4().hyphenated());
    let envelope = bind_fresh(database, computer, &record)?;
    let command = transport::selected_command(RouteKind::Mcp, &envelope)?;
    let mut command = tokio::process::Command::from(command);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| unreachable_route(format!("spawn {} {e}.", transport::SSH)))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (lines_tx, events) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let read = read_line_bounded(&mut reader, UPSTREAM_LINE_LIMIT, false).await;
            let last = !matches!(read, Ok(LineRead::Line(_)));
            if lines_tx.send(read.unwrap_or(LineRead::Eof)).await.is_err() || last {
                return;
            }
        }
    });
    // ssh diagnostics go to our stderr (the harness log) and the last few to the error.
    let tail = Arc::new(Mutex::new(String::new()));
    let tail_writer = tail.clone();
    let diagnostics = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut block = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut block).await {
            if n == 0 {
                break;
            }
            let _ = tokio::io::stderr().write_all(&block[..n]).await;
            let mut tail = lock(&tail_writer);
            tail.push_str(&String::from_utf8_lossy(&block[..n]));
            if tail.len() > 400 {
                let cut = tail.len() - 400;
                let cut = (cut..tail.len()).find(|i| tail.is_char_boundary(*i)).unwrap_or(tail.len());
                tail.drain(..cut);
            }
        }
    });
    let mut upstream = Upstream { child, stdin, events };
    let handshake = async {
        upstream
            .send(&json!({"jsonrpc": "2.0", "id": HANDSHAKE_ID, "method": "initialize", "params": params}))
            .await
            .map_err(|_| "the route closed during the MCP handshake.".to_string())?;
        loop {
            match upstream.events.recv().await {
                Some(LineRead::Line(line)) => {
                    let Ok(reply) = serde_json::from_slice::<Value>(&line) else { continue };
                    if reply.get("method").is_some() || reply.get("id") != Some(&json!(HANDSHAKE_ID)) {
                        continue;
                    }
                    if let Some(error) = reply.get("error") {
                        let message = error.get("message").and_then(Value::as_str).unwrap_or("refused");
                        return Err(format!("the target refused the MCP handshake ({message})."));
                    }
                    break;
                }
                _ => return Err("the route closed during the MCP handshake.".to_string()),
            }
        }
        upstream
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await
            .map_err(|_| "the route closed during the MCP handshake.".to_string())
    };
    let outcome = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(outcome) => outcome,
        Err(_) => Err("the MCP handshake timed out.".to_string()),
    };
    match outcome {
        Ok(()) => Ok(upstream),
        Err(reason) => {
            upstream.shutdown().await;
            // Let the stderr task drain what ssh said last.
            let _ = tokio::time::timeout(Duration::from_millis(500), diagnostics).await;
            let tail = lock(&tail).clone();
            Err(unreachable_route(match route_trouble(&tail) {
                Some(plain) => format!("{reason} {plain}"),
                None => reason,
            }))
        }
    }
}

/// What ssh's last words mean for a person, in plain words, or its last whole
/// line. The kept tail can start mid-line, so a partial first line is dropped.
fn route_trouble(tail: &str) -> Option<String> {
    if tail.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || tail.contains("Host key verification failed") {
        return Some(
            "Its identity changed since it was added, as after ibara was reinstalled there. Remove it from the fleet in the console (Remove Computer) and add it again from Add Computer."
                .into(),
        );
    }
    let whole = if tail.len() >= 400 { tail.split_once('\n').map_or("", |(_, rest)| rest) } else { tail };
    whole.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('@')).next_back().map(|l| clip(l, 200))
}
