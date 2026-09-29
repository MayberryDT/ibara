//! Live Video (Preview): a computer's screen as H.264 in MPEG-TS, played by the
//! shell from a named pipe.
//!
//! Off unless the console's `live_video` setting is on. The shell asks for a
//! computer's video with `video open COMPUTER --width W --height H` and gets
//! back `{path, state}`: a FIFO in the private previews directory
//! (`video-<computer>-<W>x<H>-<n>.ts`, 0600), the same path for as long as the
//! stream stays open, and `starting` until bytes have reached a reader, then
//! `playing`. It must ask again within 10 s or the stream closes;
//! `video close COMPUTER` closes it at once. When the setting is off, or the
//! computer answered that it cannot stream video, the answer is
//! `{unsupported: "<plain words>"}` instead.
//!
//! The video is live: nothing is asked for or kept while no reader has the
//! FIFO open, because a player plays what it is given at the pace it was
//! recorded, so bytes kept for it would stay behind by as long as they
//! waited. Once a reader opens it, the stream polls the target's
//! `observe_video` about every 0.25 s, starting without a cursor (the target
//! then starts an encoder afresh), over a session of its own per computer,
//! separate from the one status and pictures use, so video never waits
//! behind them (or they behind it). The bytes since the last cursor go to the
//! FIFO. A new stream is needed when the target says `reset` (its encoder
//! restarted, or the cursor fell out of its buffer), when an encoder that
//! produced bytes `ended`, when the reader went away, or when the reader fell
//! more than 2 s behind: the FIFO is then closed (its reader sees the end of
//! the stream), replaced by a fresh one at the same path, and the next reader
//! gets a new stream from a new encoder's first keyframe.

use super::{Ctx, option, positional};
use super::envelope::{Fault, Handled};
use crate::error::IbaraError;
use crate::operator::directory::OperatorDirectory;
use crate::operator::pattern;
use crate::operator::sessions::OperatorSessions;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::pipe;
use tokio::sync::Notify;
use tokio::time::Instant;

/// A stream not asked for again within this long closes.
const RENEW: Duration = Duration::from_secs(10);
/// Time between polls of the target.
const TICK: Duration = Duration::from_millis(250);
/// Time before polling again after a failed poll.
const RETRY: Duration = Duration::from_secs(2);
/// How often a FIFO without a reader is tried again.
const ATTACH: Duration = Duration::from_millis(100);
/// Bytes kept for a reader that is slow.
const PENDING_CAP: usize = 4 * 1024 * 1024;
/// A reader that has not taken bytes for this long gets a new stream, live again.
const BEHIND: Duration = Duration::from_secs(2);
/// The pipe's own buffer, kept small so bytes cannot wait unseen in it for long.
const PIPE_BYTES: libc::c_int = 16 * 1024;
/// The most one poll may carry (the target sends at most 1 MiB).
const CHUNK_CAP: usize = 1024 * 1024;
/// A computer that cannot stream video is not asked again for this long.
const REFUSED_FOR: Duration = Duration::from_secs(60);
/// The largest and smallest picture a stream carries.
const LARGEST: (u32, u32) = (640, 360);
const SMALLEST: u32 = 16;

const OFF: &str = "Live Video is off in Settings.";
const UNSUPPORTED_CODE: &str = "VIDEO_UNSUPPORTED";
const CANNOT: &str = "This computer can't stream video efficiently.";

/// One poll of the target.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Ask {
    pub computer: String,
    pub width: u32,
    pub height: u32,
    pub cursor: Option<u64>,
}

/// The target's answer to one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Chunk {
    pub cursor: u64,
    pub bytes: Vec<u8>,
    pub reset: bool,
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Refusal {
    /// The computer cannot stream video; the plain words say why.
    Unsupported(String),
    /// Anything else (authorization, a closed route, Take Control): try again later.
    Failed(String),
}

type Polled = Pin<Box<dyn Future<Output = Result<Chunk, Refusal>> + Send>>;
type Source = Arc<dyn Fn(Ask) -> Polled + Send + Sync>;
type Enabled = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a stream's task and its owner share.
struct Shared {
    renewed: Mutex<Instant>,
    stopped: AtomicBool,
    stop: Notify,
    playing: AtomicBool,
}

impl Shared {
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }
}

struct Stream {
    id: u64,
    size: (u32, u32),
    path: PathBuf,
    shared: Arc<Shared>,
}

type Streams = Arc<Mutex<HashMap<String, Stream>>>;
type Refused = Arc<Mutex<HashMap<String, (String, Instant)>>>;

/// The open video streams of this console, one per computer.
pub struct Videos {
    dir: PathBuf,
    source: Source,
    enabled: Enabled,
    renew: Duration,
    tick: Duration,
    streams: Streams,
    refused: Refused,
    next: AtomicU64,
    /// The second session to each computer; `None` in tests.
    sessions: Option<OperatorSessions>,
}

/// Clamp a requested size to the largest a stream carries, in even numbers.
fn clamp(width: u32, height: u32) -> (u32, u32) {
    let even = |v: u32, max: u32| v.clamp(SMALLEST, max) & !1;
    (even(width, LARGEST.0), even(height, LARGEST.1))
}

impl Videos {
    /// Streams polled over their own session per computer, while `live_video` is on.
    pub fn new(dir: PathBuf, database: PathBuf) -> Videos {
        let sessions = OperatorSessions::new(database);
        let target = Arc::new(Target { sessions: sessions.clone(), epochs: Mutex::new(HashMap::new()) });
        let source: Source = Arc::new(move |ask| {
            let target = target.clone();
            Box::pin(async move { target.poll(ask).await })
        });
        let enabled: Enabled = Arc::new(|| crate::settings::current().bool("live_video"));
        let mut videos = Videos::with(dir, source, enabled, RENEW, TICK);
        videos.sessions = Some(sessions);
        videos
    }

    fn with(dir: PathBuf, source: Source, enabled: Enabled, renew: Duration, tick: Duration) -> Videos {
        Videos {
            dir,
            source,
            enabled,
            renew,
            tick,
            streams: Arc::new(Mutex::new(HashMap::new())),
            refused: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
            sessions: None,
        }
    }

    /// Open (or renew) the video of `computer` at about `width`×`height`.
    pub fn open(&self, computer: &str, width: u32, height: u32) -> Result<Value, String> {
        if !(self.enabled)() {
            self.close(computer);
            return Ok(json!({"unsupported": OFF}));
        }
        {
            let mut refused = self.refused.lock();
            match refused.get(computer) {
                Some((message, at)) if at.elapsed() < REFUSED_FOR => return Ok(json!({"unsupported": message})),
                Some(_) => {
                    refused.remove(computer);
                }
                None => {}
            }
        }
        let size = clamp(width, height);
        let mut streams = self.streams.lock();
        if let Some(stream) = streams.get(computer) {
            if stream.size == size && !stream.shared.stopped.load(Ordering::SeqCst) {
                *stream.shared.renewed.lock() = Instant::now();
                let state = if stream.shared.playing.load(Ordering::SeqCst) { "playing" } else { "starting" };
                return Ok(json!({"path": stream.path.display().to_string(), "state": state}));
            }
            if let Some(old) = streams.remove(computer) {
                old.shared.stop();
                remove_fifo(&old.path);
            }
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("video-{computer}-{}x{}-{id}.ts", size.0, size.1));
        make_fifo(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let shared = Arc::new(Shared {
            renewed: Mutex::new(Instant::now()),
            stopped: AtomicBool::new(false),
            stop: Notify::new(),
            playing: AtomicBool::new(false),
        });
        let run = Run {
            id,
            computer: computer.to_string(),
            size,
            fifo: Fifo::new(path.clone()),
            shared: shared.clone(),
            source: self.source.clone(),
            enabled: self.enabled.clone(),
            renew: self.renew,
            tick: self.tick,
            streams: self.streams.clone(),
            refused: self.refused.clone(),
        };
        tokio::spawn(run.run());
        streams.insert(computer.to_string(), Stream { id, size, path: path.clone(), shared });
        Ok(json!({"path": path.display().to_string(), "state": "starting"}))
    }

    /// Close the video of `computer`, if open: its FIFO goes at once.
    pub fn close(&self, computer: &str) {
        if let Some(stream) = self.streams.lock().remove(computer) {
            stream.shared.stop();
            remove_fifo(&stream.path);
        }
    }

    /// Close every stream and the sessions they used.
    pub async fn close_all(&self) {
        let streams: Vec<Stream> = self.streams.lock().drain().map(|(_, s)| s).collect();
        for stream in streams {
            stream.shared.stop();
            remove_fifo(&stream.path);
        }
        if let Some(sessions) = &self.sessions {
            sessions.close_all().await;
        }
    }
}

/// Remove a FIFO. A reader still waiting in `open` (the shell's player opens
/// with a blocking open nothing interrupts) is first let through by opening and
/// closing a writer end, so it reads the end of the stream instead of hanging.
fn remove_fifo(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    drop(std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(path));
    let _ = std::fs::remove_file(path);
}

/// `mkfifo PATH` with mode 0600, replacing whatever was there.
fn make_fifo(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    remove_fifo(path);
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// The FIFO of one stream and the bytes waiting for its reader.
struct Fifo {
    path: PathBuf,
    sender: Option<pipe::Sender>,
    pending: Vec<u8>,
    /// Bytes of `pending` already written.
    head: usize,
    /// Since when bytes have waited for the reader without all of them going.
    waiting_since: Option<Instant>,
    /// Bytes of the current stream were taken in (so a restart must end it).
    dirty: bool,
    /// Bytes of the current stream reached a reader.
    written: bool,
}

/// What flushing found.
#[derive(Debug, PartialEq)]
enum Flushed {
    Fine,
    /// The reader closed the FIFO or fell behind: start a new stream.
    Restart,
}

impl Fifo {
    fn new(path: PathBuf) -> Fifo {
        Fifo { path, sender: None, pending: Vec::new(), head: 0, waiting_since: None, dirty: false, written: false }
    }

    fn playing(&self) -> bool {
        self.sender.is_some() && self.written
    }

    /// Take in bytes for the reader. `false` when that would exceed the cap.
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.head == self.pending.len() {
            self.pending.clear();
            self.head = 0;
        }
        if self.pending.len() - self.head + bytes.len() > PENDING_CAP {
            return false;
        }
        self.pending.extend_from_slice(bytes);
        self.waiting_since.get_or_insert_with(Instant::now);
        self.dirty = true;
        true
    }

    /// End the current stream: the reader sees its end, and the next reader
    /// opens a fresh FIFO at the same path. Nothing happens before any bytes came.
    fn restart(&mut self) -> std::io::Result<()> {
        self.pending.clear();
        self.head = 0;
        self.waiting_since = None;
        if !self.dirty && self.sender.is_none() {
            return Ok(());
        }
        self.sender = None;
        self.dirty = false;
        self.written = false;
        make_fifo(&self.path)
    }

    /// Wait until `deadline` for a reader to open the FIFO. `true` once one has.
    async fn attach_until(&mut self, deadline: Instant) -> bool {
        use std::os::fd::AsRawFd;
        loop {
            match pipe::OpenOptions::new().open_sender(&self.path) {
                Ok(sender) => {
                    // SAFETY: F_SETPIPE_SZ on a pipe descriptor we own; failure keeps the default size.
                    unsafe { libc::fcntl(sender.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES) };
                    self.sender = Some(sender);
                    return true;
                }
                // No reader yet (ENXIO), or the path is being replaced.
                Err(_) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    tokio::time::sleep_until(deadline.min(now + ATTACH)).await;
                }
            }
        }
    }

    /// Write waiting bytes to the reader until `deadline`.
    async fn flush_until(&mut self, deadline: Instant) -> Flushed {
        let Some(sender) = self.sender.as_mut() else { return Flushed::Restart };
        while self.head < self.pending.len() {
            match tokio::time::timeout_at(deadline, sender.write(&self.pending[self.head..])).await {
                Err(_) => break,
                Ok(Ok(n)) => {
                    self.head += n;
                    self.written = true;
                }
                // The reader went away (EPIPE): the next reader needs a stream from its start.
                Ok(Err(_)) => return Flushed::Restart,
            }
        }
        if self.head == self.pending.len() {
            self.waiting_since = None;
        } else if self.waiting_since.is_some_and(|at| at.elapsed() > BEHIND) {
            return Flushed::Restart;
        }
        tokio::time::sleep_until(deadline).await;
        Flushed::Fine
    }
}

/// One stream's polling task.
struct Run {
    id: u64,
    computer: String,
    size: (u32, u32),
    fifo: Fifo,
    shared: Arc<Shared>,
    source: Source,
    enabled: Enabled,
    renew: Duration,
    tick: Duration,
    streams: Streams,
    refused: Refused,
}

impl Run {
    fn expired(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst) || self.shared.renewed.lock().elapsed() > self.renew || !(self.enabled)()
    }

    async fn run(mut self) {
        let mut cursor: Option<u64> = None;
        // The last failure logged, so a computer refusing for minutes logs once.
        let mut failing: Option<String> = None;
        while !self.expired() {
            // Nothing is asked for before a reader is there: it starts live, without a cursor.
            if self.fifo.sender.is_none() {
                cursor = None;
                let attached = tokio::select! {
                    attached = self.fifo.attach_until(Instant::now() + self.tick) => attached,
                    _ = self.shared.stop.notified() => break,
                };
                if !attached {
                    continue;
                }
            }
            let mut next = Instant::now() + self.tick;
            let ask = Ask { computer: self.computer.clone(), width: self.size.0, height: self.size.1, cursor };
            let polled = tokio::select! {
                polled = (self.source)(ask) => polled,
                _ = self.shared.stop.notified() => break,
            };
            // Bytes past the cap end the stream at once; an encoder that ended, after this flush.
            let mut restart = false;
            let mut ends = false;
            match polled {
                // The reader's stream broke off: it starts over, live, with its next open.
                Ok(chunk) if chunk.reset && self.fifo.dirty => restart = true,
                Ok(chunk) => {
                    failing = None;
                    restart = !self.fifo.push(&chunk.bytes);
                    if chunk.ended {
                        // Held after it ended: a stream that showed something ends with it.
                        cursor = None;
                        ends = self.fifo.dirty;
                    } else {
                        cursor = Some(chunk.cursor);
                    }
                }
                Err(Refusal::Unsupported(message)) => {
                    self.refused.lock().insert(self.computer.clone(), (message, Instant::now()));
                    break;
                }
                Err(Refusal::Failed(message)) => {
                    if failing.as_deref() != Some(message.as_str()) {
                        eprintln!("{}", json!({"event": "video_poll_failed", "computer_id": self.computer, "detail": message}));
                        failing = Some(message);
                    }
                    next = Instant::now() + RETRY.max(self.tick);
                }
            }
            if !restart {
                tokio::select! {
                    flushed = self.fifo.flush_until(next) => restart = flushed == Flushed::Restart || ends,
                    _ = self.shared.stop.notified() => break,
                }
            }
            if restart {
                cursor = None;
                if self.fifo.restart().is_err() {
                    break;
                }
            }
            self.shared.playing.store(self.fifo.playing(), Ordering::SeqCst);
        }
        self.shared.stopped.store(true, Ordering::SeqCst);
        // Closed by its owner, the entry and FIFO are gone already; otherwise both go here,
        // under the lock so a new stream of this computer never loses its FIFO.
        let mut streams = self.streams.lock();
        if streams.get(&self.computer).is_some_and(|s| s.id == self.id) {
            streams.remove(&self.computer);
            remove_fifo(&self.fifo.path);
        }
    }
}

// ---------------------------------------------------------------------------
// The target.

/// Polls `observe_video` over the console's second session to each computer.
struct Target {
    sessions: OperatorSessions,
    /// The controller epoch each computer's session was bound with.
    epochs: Mutex<HashMap<String, String>>,
}

fn refusal(error: IbaraError) -> Refusal {
    let prefix = format!("{UNSUPPORTED_CODE}: ");
    if error.code == UNSUPPORTED_CODE || error.message.starts_with(&prefix) {
        let words = error.message.strip_prefix(&prefix).unwrap_or(&error.message).trim();
        Refusal::Unsupported(if words.is_empty() { CANNOT.to_string() } else { words.to_string() })
    } else {
        Refusal::Failed(error.message)
    }
}

/// The target's `observe_video` result as a chunk.
fn chunk(result: &Value) -> Result<Chunk, Refusal> {
    let malformed = || Refusal::Failed("The video answer was malformed.".to_string());
    let cursor = result.get("cursor").and_then(Value::as_u64).ok_or_else(malformed)?;
    let data = result.get("data").and_then(Value::as_str).unwrap_or("");
    if data.len() > CHUNK_CAP.div_ceil(3) * 4 {
        return Err(Refusal::Failed("The video answer was larger than allowed.".to_string()));
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| malformed())?;
    let flag = |name: &str| result.get(name).and_then(Value::as_bool).unwrap_or(false);
    Ok(Chunk { cursor, bytes, reset: flag("reset"), ended: flag("ended") })
}

impl Target {
    async fn poll(&self, ask: Ask) -> Result<Chunk, Refusal> {
        let known = self.epochs.lock().get(&ask.computer).cloned();
        let epoch = match known {
            Some(epoch) => epoch,
            None => {
                let data = self.sessions.call(&ask.computer, None, "session", Value::Null).await.map_err(refusal)?;
                let epoch = data.get("controller_epoch").and_then(Value::as_str).filter(|e| pattern::id(e));
                let epoch = epoch.ok_or_else(|| Refusal::Failed("The session did not name its epoch.".to_string()))?.to_string();
                self.epochs.lock().insert(ask.computer.clone(), epoch.clone());
                epoch
            }
        };
        let fields = json!({"width": ask.width, "height": ask.height, "cursor": ask.cursor});
        match self.sessions.call(&ask.computer, Some(&epoch), "observe_video", fields).await {
            Ok(data) => chunk(data.get("result").unwrap_or(&Value::Null)),
            Err(error) => {
                // The epoch may have moved on (Take Control, a restart): bind again next time.
                self.epochs.lock().remove(&ask.computer);
                Err(refusal(error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The console command.

/// `video open COMPUTER --width W --height H` or `video close COMPUTER`.
pub async fn command(ctx: &Ctx) -> Handled {
    let words = positional(&ctx.args);
    let (verb, name) = match words.as_slice() {
        [verb, name] => (*verb, *name),
        _ => return Err(Fault::Plain(USAGE.to_string())),
    };
    let computer = if name.starts_with("computer_") && pattern::id(name) {
        name.to_string()
    } else {
        let directory = OperatorDirectory::open(&ctx.console.database).map_err(|e| Fault::Plain(e.message))?;
        let row = directory.resolve_computer(name);
        directory.close();
        row.map_err(|e| Fault::Plain(e.message))?.computer_id
    };
    let videos = &ctx.console.videos;
    match verb {
        "open" => {
            let side = |flag: &str| option(&ctx.args, flag).and_then(|v| v.parse::<u32>().ok());
            let (Some(width), Some(height)) = (side("--width"), side("--height")) else {
                return Err(Fault::Plain(USAGE.to_string()));
            };
            videos.open(&computer, width, height).map(|data| ctx.ready(data)).map_err(Fault::Plain)
        }
        "close" => {
            videos.close(&computer);
            Ok(ctx.ready(json!({"closed": true})))
        }
        _ => Err(Fault::Plain(USAGE.to_string())),
    }
}

const USAGE: &str = "Usage: ibara video open COMPUTER --width W --height H | ibara video close COMPUTER";

// ---------------------------------------------------------------------------
// `ibara video`.

/// `ibara video open|close …`: asks the console service, prints its answer as
/// JSON. Exit 0, 1 on failure, 64 on bad usage.
pub fn main(args: Vec<std::ffi::OsString>) -> i32 {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let usable = match positional(&args).as_slice() {
        ["open", _] => option(&args, "--width").is_some() && option(&args, "--height").is_some(),
        ["close", _] => true,
        _ => false,
    };
    if !usable {
        eprintln!("{USAGE}");
        return 64;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return 1 };
    match runtime.block_on(ask_console(args)) {
        Ok(data) => {
            println!("{data}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// One `video` request over the console socket; its envelope's data.
async fn ask_console(args: Vec<String>) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let socket = super::socket_path()?;
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| format!("The ibara console service is not running ({}: {e}).", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let mut line = json!({"id": "video", "command": "video", "args": args}).to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut reply = String::new();
    let read = tokio::time::timeout(Duration::from_secs(20), BufReader::new(read).read_line(&mut reply)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {}
        _ => return Err("The ibara console service did not answer.".to_string()),
    }
    let reply: Value = serde_json::from_str(&reply).map_err(|_| "The ibara console service answered badly.".to_string())?;
    let envelope = &reply["envelope"];
    if let Some(error) = envelope.get("error").filter(|e| !e.is_null()) {
        return Err(error.get("message").and_then(Value::as_str).unwrap_or("The video could not be opened.").to_string());
    }
    Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    //! Failure cases these catch: an open that starts a second stream (or
    //! returns another path) instead of renewing; a stream that outlives its
    //! renewals or leaves its FIFO behind; a stream opened while the setting
    //! is off; bytes asked for or kept before a reader is there, which a
    //! player would then play late forever; a reset or a reader that fell
    //! behind that keeps feeding the old reader instead of ending its stream,
    //! so the next reader starts live, without a cursor, at the same path.
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::sync::atomic::AtomicUsize;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ibara-video-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A target answering each poll from `answers` in turn, then with nothing new.
    fn scripted(answers: Vec<Chunk>, asks: Arc<Mutex<Vec<Ask>>>) -> Source {
        let at = Arc::new(AtomicUsize::new(0));
        Arc::new(move |ask| {
            asks.lock().push(ask.clone());
            let n = at.fetch_add(1, Ordering::SeqCst);
            let answer = answers.get(n).cloned().unwrap_or_else(|| Chunk { cursor: ask.cursor.unwrap_or(0), ..Chunk::default() });
            Box::pin(async move { Ok(answer) })
        })
    }

    fn on() -> Enabled {
        Arc::new(|| true)
    }

    fn fifo_at(value: &Value) -> PathBuf {
        PathBuf::from(value["path"].as_str().expect("a path"))
    }

    /// A reader that never blocks: `read` gives `WouldBlock` while the writer has nothing, 0 at the end.
    fn open_reader(path: &Path) -> std::fs::File {
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).unwrap()
    }

    /// Everything `reader` gets within `limit`, and whether its stream ended.
    async fn read_for(reader: &mut std::fs::File, limit: Duration) -> (Vec<u8>, bool) {
        let until = Instant::now() + limit;
        let mut got = Vec::new();
        while Instant::now() < until {
            let mut block = [0u8; 64 * 1024];
            match reader.read(&mut block) {
                Ok(0) => return (got, true),
                Ok(n) => got.extend_from_slice(&block[..n]),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        (got, false)
    }

    #[tokio::test]
    async fn open_is_idempotent_and_closes_without_renewal() {
        let dir = scratch("renew");
        let asks = Arc::new(Mutex::new(Vec::new()));
        let videos = Videos::with(dir.clone(), scripted(vec![], asks.clone()), on(), Duration::from_millis(1000), Duration::from_millis(50));
        let first = videos.open("computer_a", 1920, 1081).unwrap();
        assert_eq!(first["state"], "starting");
        let path = fifo_at(&first);
        assert!(path.starts_with(&dir));
        assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
        // Nothing is asked for before a reader opens the FIFO; then it starts without a cursor.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(asks.lock().is_empty());
        let _reader = open_reader(&path);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(asks.lock().first().map(|a| a.cursor), Some(None));
        // Asked again within the window: the same stream, the same path.
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(fifo_at(&videos.open("computer_a", 1920, 1081).unwrap()), path);
        }
        assert!(asks.lock().iter().all(|a| (a.width, a.height) == (640, 360)), "clamped to 640x360, even");
        // Not asked again: the stream stops polling and its FIFO goes.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!path.exists());
        assert!(videos.streams.lock().is_empty());
        let polls = asks.lock().len();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(asks.lock().len(), polls, "no polls after the stream closed");
        // Asked afresh: a new stream at a new path.
        let again = fifo_at(&videos.open("computer_a", 640, 360).unwrap());
        assert_ne!(again, path);
        videos.close("computer_a");
        assert!(!again.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn setting_off_answers_unsupported_and_opens_nothing() {
        let dir = scratch("off");
        let asks = Arc::new(Mutex::new(Vec::new()));
        let enabled = Arc::new(AtomicBool::new(true));
        let flag = enabled.clone();
        let videos = Videos::with(
            dir.clone(),
            scripted(vec![], asks.clone()),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            Duration::from_secs(10),
            Duration::from_millis(50),
        );
        let open = fifo_at(&videos.open("computer_a", 640, 360).unwrap());
        enabled.store(false, Ordering::SeqCst);
        let answer = videos.open("computer_a", 640, 360).unwrap();
        assert_eq!(answer, json!({"unsupported": OFF}));
        assert!(!open.exists(), "turning it off closes the open stream");
        assert!(videos.open("computer_b", 640, 360).unwrap().get("path").is_none());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let polls = asks.lock().len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(asks.lock().len(), polls);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsupported_computer_is_answered_as_such() {
        let dir = scratch("unsupported");
        let source: Source = Arc::new(|_| Box::pin(async { Err(Refusal::Unsupported(CANNOT.to_string())) }));
        let videos = Videos::with(dir.clone(), source, on(), Duration::from_secs(10), Duration::from_millis(50));
        let path = fifo_at(&videos.open("computer_a", 640, 360).unwrap());
        let _reader = open_reader(&path);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!path.exists());
        assert_eq!(videos.open("computer_a", 640, 360).unwrap(), json!({"unsupported": CANNOT}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reset_ends_the_stream_and_the_next_reader_starts_live() {
        let dir = scratch("reset");
        let asks = Arc::new(Mutex::new(Vec::new()));
        let answers = vec![
            Chunk { cursor: 5, bytes: b"first".to_vec(), ..Chunk::default() },
            Chunk { cursor: 7, bytes: b"-1".to_vec(), ..Chunk::default() },
            // Held back until the first reader has read, so the order is certain.
            Chunk { cursor: 3, bytes: b"stale".to_vec(), reset: true, ended: false },
            Chunk { cursor: 20, bytes: b"live".to_vec(), ..Chunk::default() },
        ];
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let inner = scripted(answers, asks.clone());
        let held = gate.clone();
        let source: Source = Arc::new(move |ask| {
            let polled = inner(ask.clone());
            let held = held.clone();
            Box::pin(async move {
                if ask.cursor == Some(7) {
                    held.acquire().await.unwrap().forget();
                }
                polled.await
            })
        });
        let videos = Videos::with(dir.clone(), source, on(), Duration::from_secs(10), Duration::from_millis(50));
        let path = fifo_at(&videos.open("computer_a", 640, 360).unwrap());
        // The first reader gets the first stream, then its end.
        let mut first = open_reader(&path);
        // A FIFO reads as ended until its writer opens it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (got, ended) = read_for(&mut first, Duration::from_millis(400)).await;
        assert_eq!((got.as_slice(), ended), (&b"first-1"[..], false));
        assert_eq!(videos.open("computer_a", 640, 360).unwrap()["state"], "playing");
        gate.add_permits(1);
        let (got, ended) = read_for(&mut first, Duration::from_secs(3)).await;
        assert_eq!((got.as_slice(), ended), (&b""[..], true), "the broken stream ends without its stale bytes");
        // Nothing is asked for until the next reader; it starts without a cursor.
        assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(asks.lock().len(), 3);
        let mut second = open_reader(&path);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (got, _) = read_for(&mut second, Duration::from_millis(400)).await;
        assert_eq!(got, b"live");
        let cursors: Vec<Option<u64>> = asks.lock().iter().take(4).map(|a| a.cursor).collect();
        assert_eq!(cursors, [None, Some(5), Some(7), None]);
        videos.close("computer_a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_reader_that_falls_behind_gets_a_new_stream() {
        let dir = scratch("behind");
        let asks = Arc::new(Mutex::new(Vec::new()));
        let log = asks.clone();
        let source: Source = Arc::new(move |ask| {
            log.lock().push(ask.clone());
            let cursor = ask.cursor.unwrap_or(0) + 8192;
            Box::pin(async move { Ok(Chunk { cursor, bytes: vec![0x47; 8192], ..Chunk::default() }) })
        });
        let videos = Videos::with(dir.clone(), source, on(), Duration::from_secs(10), Duration::from_millis(50));
        let path = fifo_at(&videos.open("computer_a", 640, 360).unwrap());
        // A reader that stops reading: after BEHIND its stream ends, and nothing more is kept for it.
        let mut stuck = open_reader(&path);
        tokio::time::sleep(BEHIND + Duration::from_millis(800)).await;
        let (got, ended) = read_for(&mut stuck, Duration::from_millis(500)).await;
        assert!(ended, "the stream of a reader that fell behind ends");
        assert!(got.len() <= PIPE_BYTES as usize, "only what the small pipe held reached it: {}", got.len());
        let polls = asks.lock().len();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(asks.lock().len(), polls, "nothing is asked for without a reader");
        // The next reader starts live.
        let _next = open_reader(&path);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(asks.lock().get(polls).map(|a| a.cursor), Some(None));
        videos.close("computer_a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusals_tell_unsupported_from_passing_failures() {
        let unsupported = IbaraError::new("SESSION_UNAVAILABLE", "VIDEO_UNSUPPORTED: No hardware encoder.", false);
        assert_eq!(refusal(unsupported), Refusal::Unsupported("No hardware encoder.".to_string()));
        let held = IbaraError::new("SESSION_UNAVAILABLE", "CAPABILITY_UNAVAILABLE: control_held", false);
        assert!(matches!(refusal(held), Refusal::Failed(_)));
    }
}
