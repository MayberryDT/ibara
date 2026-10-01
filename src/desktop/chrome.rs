//! The Chrome extension bridge.
//!
//! Extension worker ⇄ Chrome native messaging (stdio) ⇄ `ibara chrome-host`
//! ⇄ `<runtime>/chrome.sock` ⇄ [`ChromeBridge`] in `ibarad`.
//!
//! Framing on both hops is Chrome's: a 4-byte little-endian length, then that
//! many bytes of UTF-8 JSON, 2 bytes to 512 KiB. The host relays whole frames
//! without parsing them. The bridge accepts one extension at a time, requires
//! `{"hello":1}` within 3 s, and keeps at most one request in flight.
//!
//! After the hello the bridge names the jobs this daemon needs (`jobs`, e.g.
//! `["pages","receive"]` on an agent computer, `["share"]` on the sharing
//! computer) and waits up to 3 s for any reply before the extension counts as
//! connected; an older extension refuses the request, which is fine. The
//! extension refuses every operation outside its jobs, and allows only
//! `pages` until told otherwise.
//!
//! A request is `{"id":"chrome_<hex>","op":…,"args":…,"deadline":<ms>}` (4 s
//! ahead; 12 s for `cookies_*`); replies are `{"id","result":{…}}` or
//! `{"id","error":{…}}`. An *effect* that is not acknowledged (error without
//! `execution_not_started`, a malformed reply, a timeout after 6 s, or a
//! disconnect) leaves the bridge unsettled: disconnected until `ibarad`
//! restarts, as today. Writing or removing cookies is an effect that is safe
//! to repeat: when unacknowledged (within 15 s) its outcome is unknown, but
//! the bridge stays usable for reading pages.

use crate::error::{IbaraError, Result, internal, invalid};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub const MAX_CHROME_FRAME: usize = 512 * 1024;
pub const CHROME_PROTOCOL: u64 = 1;
const HELLO_WITHIN: Duration = Duration::from_millis(3000);
const JOBS_WITHIN: Duration = Duration::from_millis(3000);
const REPLY_WITHIN: Duration = Duration::from_millis(6000);
const DEADLINE_AHEAD_MS: i64 = 4000;
/// Cookie bundles can hold 100+ cookies, each set and checked on its own.
const COOKIE_REPLY_WITHIN: Duration = Duration::from_millis(15000);
const COOKIE_DEADLINE_AHEAD_MS: i64 = 12000;
const CANCEL_WAIT: Duration = Duration::from_millis(6500);
const LOGIN_UNKNOWN: &str = "The login copy may be incomplete; share it again or remove it.";

/// Read one frame. `Ok(None)` at end of stream (a trailing partial frame is
/// discarded); `InvalidData` for a length outside 2..=512 KiB.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let size = u32::from_le_bytes(header) as usize;
    if !(2..=MAX_CHROME_FRAME).contains(&size) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Chrome frame size out of range"));
    }
    let mut frame = vec![0u8; size];
    match reader.read_exact(&mut frame).await {
        Ok(_) => Ok(Some(frame)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

fn framed(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn disconnected() -> IbaraError {
    IbaraError::new(
        "CAPABILITY_UNAVAILABLE",
        "Chrome semantic extension is disconnected. Use native input or reconnect the extension.",
        true,
    )
    .with("execution_not_started", true)
}

/// The active tab as the extension reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChromeTab {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub focused: bool,
}

/// What an unanswered request leaves behind.
#[derive(Clone, Copy, PartialEq)]
enum Dispatch {
    Read,
    /// Unacknowledged, it leaves the bridge unsettled.
    Effect,
    /// An effect safe to repeat (cookies): unknown, but the bridge stays usable.
    Repeatable,
}

impl Dispatch {
    fn of(op: &str, effect: bool) -> Self {
        match (effect, op.starts_with("cookies_")) {
            (false, _) => Dispatch::Read,
            (true, false) => Dispatch::Effect,
            (true, true) => Dispatch::Repeatable,
        }
    }

    /// The error for a dispatched request whose outcome is not known.
    fn unknown(self, message: &str) -> IbaraError {
        match self {
            Dispatch::Read | Dispatch::Effect => IbaraError::new("OUTCOME_UNKNOWN", message, false).requires_reconciliation(),
            Dispatch::Repeatable => IbaraError::new("OUTCOME_UNKNOWN", LOGIN_UNKNOWN, true),
        }
    }
}

struct Pending {
    id: String,
    dispatch: Dispatch,
    deadline: Instant,
    reply: oneshot::Sender<Result<Value>>,
}

#[derive(Default)]
struct State {
    peer: Option<(u64, mpsc::Sender<Vec<u8>>)>,
    ready: bool,
    generation: u64,
    pending: Option<Pending>,
    unsettled: bool,
}

impl State {
    fn connected(&self) -> bool {
        self.ready && self.peer.is_some() && !self.unsettled
    }
    /// A request whose caller went away still times out.
    fn expire(&mut self) {
        if let Some(pending) = &self.pending
            && pending.deadline <= Instant::now()
        {
            if pending.dispatch == Dispatch::Effect {
                self.unsettled = true;
            }
            self.pending = None;
        }
    }
    fn is_current(&self, generation: u64) -> bool {
        self.peer.as_ref().is_some_and(|(g, _)| *g == generation)
    }
    /// The peer of `generation` may take requests.
    fn open(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.ready = true;
        }
    }
}

/// The controller end of the bridge.
pub struct ChromeBridge {
    state: Arc<Mutex<State>>,
    path: PathBuf,
    accept: Mutex<Option<JoinHandle<()>>>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

impl ChromeBridge {
    /// Listen on `path` (mode 0600). Refuses if another listener answers
    /// there; removes a stale socket file. Each extension that connects is
    /// told `jobs` after its hello (nothing is sent when `jobs` is empty).
    pub async fn listen(path: &Path, jobs: &'static [&'static str]) -> Result<Arc<ChromeBridge>> {
        if tokio::fs::symlink_metadata(path).await.is_ok() {
            if UnixStream::connect(path).await.is_ok() {
                return Err(internal(format!("Chrome bridge socket already active: {}", path.display())));
            }
            tokio::fs::remove_file(path).await?;
        }
        let listener = UnixListener::bind(path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let bridge = Arc::new(ChromeBridge { state: Arc::default(), path: path.to_path_buf(), accept: Mutex::new(None) });
        let state = bridge.state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let (tx, rx) = mpsc::channel(4);
                let generation = {
                    let mut s = lock(&state);
                    if s.peer.is_some() {
                        // One extension at a time; later connections are closed.
                        continue;
                    }
                    s.generation += 1;
                    s.ready = false;
                    s.peer = Some((s.generation, tx));
                    s.generation
                };
                tokio::spawn(serve_peer(state.clone(), stream, generation, rx, jobs));
            }
        });
        *bridge.accept.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
        Ok(bridge)
    }

    /// Hello received, a peer attached, and no unacknowledged effect.
    pub fn connected(&self) -> bool {
        let mut state = lock(&self.state);
        state.expire();
        state.connected()
    }

    /// Increments on every new extension connection; tab and element
    /// references from an older generation are void.
    pub fn generation(&self) -> u64 {
        lock(&self.state).generation
    }

    /// Send one request and wait for its reply (6 s; 15 s for `cookies_*`).
    /// Writing or removing cookies never leaves the bridge unsettled.
    pub async fn call(&self, op: &str, args: Value, effect: bool) -> Result<Value> {
        let dispatch = Dispatch::of(op, effect);
        let (ahead_ms, within) =
            if op.starts_with("cookies_") { (COOKIE_DEADLINE_AHEAD_MS, COOKIE_REPLY_WITHIN) } else { (DEADLINE_AHEAD_MS, REPLY_WITHIN) };
        let (reply, id, deadline) = {
            let mut state = lock(&self.state);
            state.expire();
            if !state.connected() {
                return Err(disconnected());
            }
            if state.pending.is_some() {
                return Err(IbaraError::new("BUSY", "Chrome request already in flight.", true).with("execution_not_started", true));
            }
            let id = crate::ids::id("chrome");
            let message = json!({"id": id, "op": op, "args": args, "deadline": crate::ids::now_millis() + ahead_ms});
            let payload = serde_json::to_vec(&message).map_err(|e| internal(format!("chrome request: {e}")))?;
            if payload.len() > MAX_CHROME_FRAME {
                return Err(invalid("Chrome request too large.").with("execution_not_started", true));
            }
            let Some((_, peer)) = &state.peer else {
                return Err(disconnected());
            };
            if peer.try_send(framed(&payload)).is_err() {
                return Err(disconnected());
            }
            let (tx, rx) = oneshot::channel();
            let deadline = Instant::now() + within;
            state.pending = Some(Pending { id: id.clone(), dispatch, deadline, reply: tx });
            (rx, id, deadline)
        };
        match tokio::time::timeout_at(deadline, reply).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => {
                let mut state = lock(&self.state);
                if state.pending.as_ref().is_some_and(|p| p.id == id) {
                    state.pending = None;
                }
                if dispatch == Dispatch::Effect {
                    state.unsettled = true;
                }
                Err(if dispatch == Dispatch::Read {
                    IbaraError::new("CAPABILITY_UNAVAILABLE", "Chrome request timed out.", true).with("execution_not_started", true)
                } else {
                    dispatch.unknown("Chrome request timed out.")
                })
            }
        }
    }

    /// `tabs`: the active tab of the last focused window (none for
    /// incognito or non-web pages).
    pub async fn tabs(&self) -> Result<Vec<ChromeTab>> {
        let result = self.call("tabs", json!({}), false).await?;
        Ok(result
            .get("tabs")
            .and_then(Value::as_array)
            .map(|tabs| {
                tabs.iter()
                    .filter_map(|t| {
                        Some(ChromeTab {
                            id: t.get("id")?.as_i64()?,
                            title: t.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                            url: t.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                            focused: t.get("focused").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Drop the page reader's connection so it connects afresh, and wait
    /// (≤`within`) until it has: its host exits when the socket closes, and
    /// the extension reconnects 2 s after its port goes. Nothing is dropped
    /// while a request is in flight or an effect is unsettled. True once
    /// connected again.
    pub async fn reconnect(&self, within: Duration) -> bool {
        {
            let mut state = lock(&self.state);
            state.expire();
            if state.pending.is_some() || state.unsettled {
                return false;
            }
            // Dropping the only sender ends the writer, which closes the
            // socket's write half; the host sees the end and exits.
            state.peer = None;
            state.ready = false;
        }
        let end = Instant::now() + within;
        while Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.connected() {
                return true;
            }
        }
        false
    }

    /// Wait (≤6.5 s) for an in-flight request; `CONTROL_UNSETTLED` if one is
    /// still pending or an effect was never acknowledged. A cookie write or
    /// removal in flight does not hold it up: its own caller gets its outcome.
    pub async fn cancel(&self) -> Result<()> {
        let end = Instant::now() + CANCEL_WAIT;
        loop {
            {
                let mut state = lock(&self.state);
                state.expire();
                if state.pending.as_ref().is_none_or(|p| p.dispatch == Dispatch::Repeatable) {
                    if state.unsettled {
                        break;
                    }
                    return Ok(());
                }
            }
            if Instant::now() >= end {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(IbaraError::new("CONTROL_UNSETTLED", "Chrome dispatch was not acknowledged; reconcile before resuming.", false)
            .requires_reconciliation())
    }

    /// Stop listening, drop the peer and remove the socket.
    pub async fn close(&self) {
        if let Some(task) = self.accept.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
        {
            let mut state = lock(&self.state);
            state.peer = None;
            state.ready = false;
        }
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

fn settle_reply(state: &Mutex<State>, message: &Value) {
    let Some(id) = message.get("id").and_then(Value::as_str) else {
        return;
    };
    let mut s = lock(state);
    if !s.pending.as_ref().is_some_and(|p| p.id == id) {
        return;
    }
    let Some(pending) = s.pending.take() else {
        return;
    };
    let error = message.get("error").filter(|e| !e.is_null() && **e != Value::Bool(false));
    let result = if let Some(error) = error {
        let not_started = error.get("execution_not_started") == Some(&Value::Bool(true));
        if not_started {
            Err(IbaraError::new("STALE_TARGET", "Chrome refused dispatch; observe again.", true).with("execution_not_started", true))
        } else {
            if pending.dispatch == Dispatch::Effect {
                s.unsettled = true;
            }
            Err(pending.dispatch.unknown("Chrome operation needs reconciliation.").with("execution_not_started", false))
        }
    } else if let Some(result) = message.get("result").filter(|r| r.is_object()) {
        Ok(result.clone())
    } else {
        if pending.dispatch == Dispatch::Effect {
            s.unsettled = true;
        }
        Err(pending.dispatch.unknown("Invalid Chrome reply."))
    };
    drop(s);
    let _ = pending.reply.send(result);
}

async fn serve_peer(
    state: Arc<Mutex<State>>,
    stream: UnixStream,
    generation: u64,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
    jobs: &'static [&'static str],
) {
    let (mut reader, mut writer) = stream.into_split();
    let writing = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let hello = tokio::time::timeout(HELLO_WITHIN, read_frame(&mut reader)).await;
    let greeted = matches!(
        &hello,
        Ok(Ok(Some(frame))) if serde_json::from_slice::<Value>(frame).ok().and_then(|m| m.get("hello").and_then(Value::as_u64)) == Some(CHROME_PROTOCOL)
    );
    let mut opening = None;
    if greeted {
        // Any reply to `jobs` (an older extension refuses it) or 3 s of
        // silence opens the bridge. The timer never interrupts a frame read.
        let jobs_id = if jobs.is_empty() { None } else { send_jobs(&state, generation, jobs) };
        match &jobs_id {
            None => lock(&state).open(generation),
            Some(_) => {
                let state = state.clone();
                opening = Some(tokio::spawn(async move {
                    tokio::time::sleep(JOBS_WITHIN).await;
                    lock(&state).open(generation);
                }));
            }
        }
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
                break;
            };
            if jobs_id.is_some() && message.get("id").and_then(Value::as_str) == jobs_id.as_deref() {
                lock(&state).open(generation);
                continue;
            }
            settle_reply(&state, &message);
        }
    }
    if let Some(opening) = opening {
        opening.abort();
    }
    // A peer dropped by `reconnect` is no longer current; a request in
    // flight then belongs to its successor.
    let pending = {
        let mut s = lock(&state);
        if !s.is_current(generation) {
            None
        } else {
            s.peer = None;
            s.ready = false;
            let pending = s.pending.take();
            if pending.as_ref().is_some_and(|p| p.dispatch == Dispatch::Effect) {
                s.unsettled = true;
            }
            pending
        }
    };
    if let Some(pending) = pending {
        let error = match pending.dispatch {
            Dispatch::Read => disconnected(),
            dispatch => dispatch.unknown("Chrome disconnected during dispatch."),
        };
        let _ = pending.reply.send(Err(error));
    }
    writing.abort();
}

/// Send the `jobs` request to the peer of `generation`; its id, or `None`
/// when it could not be queued.
fn send_jobs(state: &Mutex<State>, generation: u64, jobs: &[&str]) -> Option<String> {
    let id = crate::ids::id("chrome");
    let message = json!({"id": id, "op": "jobs", "args": {"jobs": jobs}, "deadline": crate::ids::now_millis() + DEADLINE_AHEAD_MS});
    let s = lock(state);
    let (g, peer) = s.peer.as_ref()?;
    (*g == generation && peer.try_send(framed(message.to_string().as_bytes())).is_ok()).then_some(id)
}

/// The origin Chrome passed: the argument after `chrome-host`, or argv[1]
/// when the binary is the host itself.
pub fn host_origin(args: &[String]) -> Option<&str> {
    match args.iter().position(|a| a == "chrome-host") {
        Some(i) => args.get(i + 1),
        None => args.get(1),
    }
    .map(String::as_str)
}

/// Relay whole frames: Chrome (`input`/`output`) ⇄ `socket`. Returns the exit
/// code: 0 when the controller closes the socket, 1 on an invalid frame or
/// an I/O error. End of input half-closes the socket and waits for its end.
pub async fn relay<I, O>(mut input: I, mut output: O, socket: UnixStream) -> i32
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let (mut from_peer, mut to_peer) = socket.into_split();
    let upstream = async {
        loop {
            match read_frame(&mut input).await {
                Ok(Some(frame)) => {
                    if to_peer.write_all(&framed(&frame)).await.is_err() {
                        return false;
                    }
                }
                Ok(None) => {
                    let _ = to_peer.shutdown().await;
                    return true;
                }
                Err(_) => return false,
            }
        }
    };
    let downstream = async {
        loop {
            match read_frame(&mut from_peer).await {
                Ok(Some(frame)) => {
                    if output.write_all(&framed(&frame)).await.is_err() || output.flush().await.is_err() {
                        return 1;
                    }
                }
                Ok(None) => return 0,
                Err(_) => return 1,
            }
        }
    };
    tokio::pin!(upstream, downstream);
    let mut input_open = true;
    loop {
        tokio::select! {
            code = &mut downstream => return code,
            ok = &mut upstream, if input_open => {
                if !ok {
                    return 1;
                }
                input_open = false;
            }
        }
    }
}

/// `ibara chrome-host <origin>`: Chrome's native messaging host
/// (`io.ibara.chrome`), replacing `dist/helpers/chrome-native.mjs`.
///
/// Exits 1 unless `<origin>` is in `allowed_origins` of
/// `<install root>/browser/native-host.json` (written by `ibara browser-setup`
/// with this computer's extension id), then relays frames to
/// `<runtime>/chrome.sock`. Returns the exit code; the caller must exit the
/// process with it at once (a blocked stdin read would otherwise hold the
/// runtime open until Chrome closes the pipe). Payloads are never logged.
pub async fn chrome_host_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let Some(origin) = host_origin(&args) else {
        return 1;
    };
    let install = std::env::var_os("IBARA_INSTALL_ROOT").map(PathBuf::from).unwrap_or_else(|| "/opt/agent-computer".into());
    let manifest = install.join("browser/native-host.json");
    let allowed = std::fs::read(&manifest)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|m| m.get("allowed_origins").and_then(Value::as_array).cloned())
        .is_some_and(|origins| origins.iter().any(|o| o.as_str() == Some(origin)));
    if !allowed {
        return 1;
    }
    if let Ok(raw)=std::fs::read(install.join("browser/selection.json")) {
        let Ok(selection)=serde_json::from_slice::<Value>(&raw) else {return 1;};
        let Some(root)=selection["root"].as_str() else {return 1;};
        if crate::logins::check_profile(Path::new(root)).is_err() {return 1;}
        let parent=unsafe {libc::getppid()};
        let expected=selection["executable"].as_str().map(PathBuf::from);
        if std::fs::read_link(format!("/proc/{parent}/exe")).ok()!=expected {return 1;}
        let Ok(cmd)=std::fs::read(format!("/proc/{parent}/cmdline")) else {return 1;};
        let args:Vec<_>=cmd.split(|b|*b==0).filter_map(|b|std::str::from_utf8(b).ok()).collect();
        for (i,arg) in args.iter().enumerate() {
            let data=arg.strip_prefix("--user-data-dir=").or_else(||(*arg=="--user-data-dir").then(||args.get(i+1).copied()).flatten());
            if data.is_some_and(|d|Path::new(d)!=Path::new(root)) || *arg=="--incognito" || arg.strip_prefix("--profile-directory=").is_some_and(|p|p!="Default") {return 1;}
        }
    }
    let runtime = std::env::var_os("IBARA_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| "/run/agent-computer".into());
    let Ok(socket) = UnixStream::connect(runtime.join("chrome.sock")).await else {
        return 1;
    };
    relay(tokio::io::stdin(), tokio::io::stdout(), socket).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, value: Value) {
        w.write_all(&framed(&serde_json::to_vec(&value).unwrap())).await.unwrap();
    }

    async fn read_json<R: AsyncRead + Unpin>(r: &mut R) -> Value {
        serde_json::from_slice(&read_frame(r).await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn frames_outside_two_bytes_to_512_kib_are_refused() {
        let mut one = &[1u8, 0, 0, 0, b'x'][..];
        assert!(read_frame(&mut one).await.is_err());
        let big = ((MAX_CHROME_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut &big[..]).await.is_err());
        let mut ok = &framed(b"{}")[..];
        assert_eq!(read_frame(&mut ok).await.unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_frame(&mut ok).await.unwrap(), None);
    }

    #[tokio::test]
    async fn host_relays_frames_verbatim_and_exits_with_the_socket() {
        let (host_side, mut controller) = UnixStream::pair().unwrap();
        let (mut chrome_in, host_in) = tokio::io::duplex(1 << 16);
        let (host_out, mut chrome_out) = tokio::io::duplex(1 << 16);
        let host = tokio::spawn(relay(host_in, host_out, host_side));
        chrome_in.write_all(&framed(br#"{"hello":1}"#)).await.unwrap();
        assert_eq!(read_frame(&mut controller).await.unwrap().unwrap(), br#"{"hello":1}"#);
        controller.write_all(&framed(br#"{"id":"chrome_1","op":"tabs"}"#)).await.unwrap();
        assert_eq!(read_frame(&mut chrome_out).await.unwrap().unwrap(), br#"{"id":"chrome_1","op":"tabs"}"#);
        drop(controller);
        assert_eq!(host.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn host_fails_on_an_oversized_frame_from_chrome() {
        let (host_side, _controller) = UnixStream::pair().unwrap();
        let (mut chrome_in, host_in) = tokio::io::duplex(64);
        let host = tokio::spawn(relay(host_in, tokio::io::sink(), host_side));
        chrome_in.write_all(&((MAX_CHROME_FRAME + 1) as u32).to_le_bytes()).await.unwrap();
        assert_eq!(host.await.unwrap(), 1);
    }

    #[test]
    fn origin_comes_after_the_subcommand() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(host_origin(&args(&["ibara", "chrome-host", "chrome-extension://x/"])), Some("chrome-extension://x/"));
        assert_eq!(host_origin(&args(&["ibara-chrome-native", "chrome-extension://y/"])), Some("chrome-extension://y/"));
        assert_eq!(host_origin(&args(&["ibara", "chrome-host"])), None);
    }

    #[tokio::test]
    async fn bridge_hello_replies_refusals_and_unacknowledged_effects() {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-chrome-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chrome.sock");
        let bridge = ChromeBridge::listen(&path, &[]).await.unwrap();
        assert_eq!(bridge.call("tabs", json!({}), false).await.unwrap_err().code, "CAPABILITY_UNAVAILABLE");

        let mut ext = UnixStream::connect(&path).await.unwrap();
        write_json(&mut ext, json!({"hello": 1})).await;
        let mut second = UnixStream::connect(&path).await.unwrap();
        assert_eq!(read_frame(&mut second).await.unwrap(), None, "a second extension is closed");
        for _ in 0..100 {
            if bridge.connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(bridge.connected());

        // A read resolves with the result.
        let call = bridge.call("tabs", json!({}), false);
        let ext_side = async {
            let request = read_json(&mut ext).await;
            let keys: Vec<&str> = request.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(keys, ["id", "op", "args", "deadline"]);
            let id = request["id"].clone();
            write_json(&mut ext, json!({"id": id, "result": {"tabs": [{"id": 7, "title": "T", "url": "https://x/", "focused": true}]}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap()["tabs"][0]["id"], json!(7));

        // A refused effect is retry-safe and leaves the bridge usable.
        let call = bridge.call("click", json!({}), true);
        let ext_side = async {
            let id = read_json(&mut ext).await["id"].clone();
            write_json(&mut ext, json!({"id": id, "error": {"execution_not_started": true}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap_err().code, "STALE_TARGET");
        assert!(bridge.connected());
        bridge.cancel().await.unwrap();

        // A disconnect during an effect: unknown, and the bridge stays unsettled.
        let call = bridge.call("click", json!({}), true);
        let ext_side = async {
            read_json(&mut ext).await;
            drop(ext);
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap_err().code, "OUTCOME_UNKNOWN");
        let mut again = UnixStream::connect(&path).await.unwrap();
        write_json(&mut again, json!({"hello": 1})).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!bridge.connected());
        assert_eq!(bridge.cancel().await.unwrap_err().code, "CONTROL_UNSETTLED");
        // Probing a live socket refuses a second listener (the probe itself
        // briefly occupies the single peer slot, so it runs last).
        assert!(ChromeBridge::listen(&path, &[]).await.is_err());
        bridge.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn reconnect_closes_the_reader_and_waits_for_it_to_come_back() {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-chrome-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chrome.sock");
        let bridge = ChromeBridge::listen(&path, &[]).await.unwrap();
        let mut first = UnixStream::connect(&path).await.unwrap();
        write_json(&mut first, json!({"hello": 1})).await;
        while !bridge.connected() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let generation = bridge.generation();
        // The page reader, as the extension does: once its host is closed,
        // connect again and say hello.
        let path_again = path.clone();
        let reader = async move {
            assert_eq!(read_frame(&mut first).await.unwrap(), None, "the old connection is closed");
            let mut second = UnixStream::connect(&path_again).await.unwrap();
            write_json(&mut second, json!({"hello": 1})).await;
            second
        };
        let (back, mut second) = tokio::join!(bridge.reconnect(Duration::from_secs(3)), reader);
        assert!(back, "connected again");
        assert_eq!(bridge.generation(), generation + 1);
        let call = bridge.call("tabs", json!({}), false);
        let ext_side = async {
            let id = read_json(&mut second).await["id"].clone();
            write_json(&mut second, json!({"id": id, "result": {"tabs": []}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap()["tabs"], json!([]));
        // Nobody comes back: false within the bound, still disconnected.
        assert!(!bridge.reconnect(Duration::from_millis(300)).await);
        assert!(!bridge.connected());
        drop(second);
        bridge.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Catches: `jobs` not sent, sent with the wrong list, or sent before the
    // hello; the bridge counting as connected before the extension answered
    // it; an older extension's refusal of `jobs` keeping the bridge closed;
    // the `jobs` reply being mistaken for, or blocking, the next request's.
    #[tokio::test]
    async fn jobs_are_named_after_the_hello_and_any_reply_opens_the_bridge() {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-chrome-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chrome.sock");
        let bridge = ChromeBridge::listen(&path, &["pages", "receive"]).await.unwrap();
        let mut ext = UnixStream::connect(&path).await.unwrap();
        write_json(&mut ext, json!({"hello": 1})).await;
        let request = read_json(&mut ext).await;
        assert_eq!(request["op"], "jobs");
        assert_eq!(request["args"], json!({"jobs": ["pages", "receive"]}));
        assert!(request["deadline"].is_i64());
        assert!(!bridge.connected(), "not connected before the extension answers");
        // An older extension refuses the operation it does not know.
        write_json(&mut ext, json!({"id": request["id"], "error": {"execution_not_started": true}})).await;
        for _ in 0..100 {
            if bridge.connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(bridge.connected());
        let call = bridge.call("tabs", json!({}), false);
        let ext_side = async {
            let request = read_json(&mut ext).await;
            assert_eq!(request["op"], "tabs");
            write_json(&mut ext, json!({"id": request["id"], "result": {"tabs": []}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap()["tabs"], json!([]));
        drop(ext);
        bridge.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Catches: a failed cookie write fencing the page reader (the bridge left
    // unsettled until ibarad restarts); a cookie failure reported as done or
    // as not started; cookie requests keeping the short 4 s deadline; cancel
    // refusing after a cookie failure.
    #[tokio::test]
    async fn a_failed_cookie_write_is_unknown_and_pages_still_work() {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-chrome-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chrome.sock");
        let bridge = ChromeBridge::listen(&path, &[]).await.unwrap();
        let mut ext = UnixStream::connect(&path).await.unwrap();
        write_json(&mut ext, json!({"hello": 1})).await;
        while !bridge.connected() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let call = bridge.call("cookies_write", json!({"site": "example.test", "cookies": []}), true);
        let ext_side = async {
            let request = read_json(&mut ext).await;
            let ahead = request["deadline"].as_i64().unwrap() - crate::ids::now_millis();
            assert!(ahead > 10_000, "cookie requests get the longer deadline: {ahead} ms");
            write_json(&mut ext, json!({"id": request["id"], "error": {"execution_not_started": false}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        let error = result.unwrap_err();
        assert_eq!(error.code, "OUTCOME_UNKNOWN");
        assert_eq!(error.message, LOGIN_UNKNOWN);
        assert!(bridge.connected(), "the page reader stays usable");
        bridge.cancel().await.unwrap();

        let call = bridge.call("tabs", json!({}), false);
        let ext_side = async {
            let request = read_json(&mut ext).await;
            write_json(&mut ext, json!({"id": request["id"], "result": {"tabs": []}})).await;
        };
        let (result, ()) = tokio::join!(call, ext_side);
        assert_eq!(result.unwrap()["tabs"], json!([]));
        drop(ext);
        bridge.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
