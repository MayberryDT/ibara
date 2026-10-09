//! `ibarad --role operator`: the console service for the Omarchy plugin.
//!
//! It replaces `scripts/ibara-bridge.mjs`, which the plugin spawned once per read
//! (a 55 MB Node process each), and its `serve-previews` child. The plugin keeps
//! one connection to `$XDG_RUNTIME_DIR/ibara/ibarad.sock` (directory 0700, socket
//! 0600, same-uid peers only) and speaks newline-delimited JSON:
//!
//! - request `{id, command, args}`: the bridge's argv as `command` and `args`.
//!   Plugins from before every computer was reached over its pairing route
//!   also send `for_computer` and `station_descriptor`; both are ignored.
//! - reply `{id, envelope}`: exactly the bridge's version-2 envelope for that
//!   command, with `request_id` = `id`.
//! - event `{event: "preview", computer_id, data}`: the answer to
//!   `operator-observe`, with the preview envelope as `data`; its picture is a
//!   private PPM file at the shown size (`frames.rs`), not base64.
//!   `preview-release <file name>…`
//!   deletes every frame except those named, when the console closes.
//!
//! Every command runs in-process with [`crate::operator`], except the desktop
//! programs it launches (the viewer, terminals, the file chooser) and the
//! clipboard it shares with a computer this console controls.
//! Nothing is cached: the directory is read per request; preview frames live
//! only as the few newest files per computer, and only the last picture's
//! digest is kept, so a screen that has not changed is not sent again.

mod clipboard;
mod envelope;
mod everyday;
mod files;
pub mod fleet;
mod frames;
pub(crate) mod pairing;
mod picker;
mod process;
mod selected;
pub mod video;
mod viewer;
mod logins;
mod updates;

use crate::operator::directory::{OperatorDirectory, directory_path};
use crate::operator::sessions::OperatorSessions;
use crate::operator::{LineRead, current_uid, pattern, read_line_bounded};
use envelope::{Env, Fault, Handled, Head, failure, fault_envelope};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};

/// A request line is at most 64 KiB (the setup form is at most 8 KiB).
const REQUEST_LIMIT: usize = 64 * 1024;
/// Requests one connection may have in flight; more are refused as busy.
const IN_FLIGHT: usize = 32;
/// Answer lines queued for one connection's writer.
const OUTBOX: usize = 16;
/// Connections served at once (the shell holds one).
const CONNECTIONS: usize = 8;
/// The bridge's default command deadline (`--timeout 15`).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// State shared by every connection: the operator sessions and, for each computer
/// with a preview in flight, the gate later previews of it wait on. No reply or
/// envelope is kept.
pub struct Console {
    /// The chosen local browser. Only internal login delivery reads values;
    /// the console surface reports metadata and counts.
    chrome: Mutex<Option<Arc<crate::desktop::chrome::ChromeBridge>>>,
    login_gate: tokio::sync::Mutex<()>,
    database: PathBuf,
    sessions: OperatorSessions,
    previews: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Where preview frames are written for the shell, and the last of each (`frames.rs`).
    frames: frames::Frames,
    /// Requests to add this computer to others (`pairing.rs`).
    pairs: pairing::Pairs,
    /// Epochs, approvals and timeline marks across computers (`fleet.rs`).
    fleet: fleet::Fleet,
    /// What login sharing knows about each computer, and its background loop (`logins.rs`).
    logins: logins::Logins,
    /// The viewer this console started for each computer, ended on hand back.
    viewers: Mutex<HashMap<String, u32>>,
    standby: Mutex<Option<viewer::Standby>>,
    warmed_attention: Mutex<std::collections::HashSet<String>>,
    /// The clipboard shared with each computer this console controls, and the
    /// controller epoch it was started for.
    clipboards: Mutex<HashMap<String, (String, tokio::task::JoinHandle<()>)>>,
    /// Live Video streams, each on a second session to its computer (`video.rs`).
    videos: video::Videos,
}

impl Console {
    pub fn new(database: PathBuf, frames: PathBuf) -> Self {
        Console {
            chrome: Mutex::new(None),
            login_gate: tokio::sync::Mutex::new(()),
            sessions: OperatorSessions::new(database.clone()),
            videos: video::Videos::new(frames.clone(), database.clone()),
            database,
            previews: Mutex::new(HashMap::new()),
            frames: frames::Frames::new(frames),
            pairs: pairing::Pairs::default(),
            fleet: fleet::Fleet::default(),
            logins: logins::Logins::default(),
            viewers: Mutex::new(HashMap::new()),
            standby: Mutex::new(None),
            warmed_attention: Mutex::new(std::collections::HashSet::new()),
            clipboards: Mutex::new(HashMap::new()),
        }
    }

    /// Let go of what this console holds for `computer`, which left the
    /// directory or now answers as a new computer: its session, what the fleet
    /// kept, its video, its shared clipboard and its viewer.
    fn forget_computer(&self, computer: &str) {
        viewer::forget_standby(self, computer);
        self.sessions.forget(computer);
        self.fleet.forget(computer);
        self.logins.forget(computer);
        self.videos.close(computer);
        clipboard::stop(self, computer);
        selected::close_viewer(self, computer);
    }
}

/// One request's context (the bridge's `ctx`).
pub struct Ctx {
    pub head: Head,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub console: Arc<Console>,
}

impl Ctx {
    /// `envelope({...ctx, ...})`.
    pub fn envelope(&self, env: Env) -> Value {
        env.render(&self.head)
    }
    pub fn ready(&self, data: Value) -> Value {
        Env::ready(data).render(&self.head)
    }
    pub fn failure(&self, code: &str, message: &str, connection: &str, retry_safe: bool) -> Value {
        failure(&self.head, code, message, connection, retry_safe)
    }
}

// ---------------------------------------------------------------------------
// Argument helpers shared by the handlers (ibara-bridge.mjs:174-176).

/// `option(args, name)`: the value after the first `name`.
pub(crate) fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}

/// `positional(args)`: every argument that is not an option or its value.
pub(crate) fn positional(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2;
        } else {
            out.push(args[i].as_str());
            i += 1;
        }
    }
    out
}

/// `validatedId(value, label)`.
pub(crate) fn validated_id(value: Option<&str>, label: &str) -> Result<String, Fault> {
    let value = value.unwrap_or("");
    if pattern::id(value) { Ok(value.to_string()) } else { Err(Fault::Plain(format!("Invalid {label}."))) }
}

// ---------------------------------------------------------------------------
// Requests.

/// One parsed request line.
struct Request {
    id: String,
    command: String,
    args: Vec<String>,
}

/// A request id is echoed as the envelope's `request_id`: any short line of text.
fn usable_id(id: &str) -> bool {
    (1..=256).contains(&id.len()) && !id.chars().any(char::is_control)
}

/// Parse one line. `None` drops it: without a usable id nothing can be answered.
fn parse_request(line: &[u8]) -> Option<Result<Request, (String, String)>> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let id = value.get("id")?.as_str().filter(|id| usable_id(id))?.to_string();
    let text = |name: &str| value.get(name).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let command = text("command").unwrap_or_default();
    let args = match value.get("args") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|a| a.as_str().map(str::to_string)).collect(),
        Some(_) => None,
    };
    let known = ["id", "command", "args", "for_computer", "station_descriptor"];
    let extra = value.as_object().and_then(|o| o.keys().find(|k| !known.contains(&k.as_str())).cloned());
    let Some(args) = args.filter(|_| !command.is_empty() && extra.is_none()) else {
        let reason = match extra {
            Some(key) => format!("Unknown request field {key}."),
            None => "A request needs a command and string arguments.".to_string(),
        };
        return Some(Err((id, reason)));
    };
    Some(Ok(Request { id, command, args }))
}

/// The head of every answer: the command as named, or `unknown`.
fn fallback_head(id: &str, command: &str) -> Head {
    let named = !command.is_empty()
        && command.len() <= 64
        && command.as_bytes()[0].is_ascii_lowercase()
        && command.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    Head { request_id: id.to_string(), command: if named { command.into() } else { "unknown".into() }, target: "ibara".into() }
}

/// Build the context and run the command. Also returns the computer the
/// request names, as its `computer_` id when it could be told.
async fn answer(console: Arc<Console>, request: Request) -> (Option<String>, Value) {
    let head = fallback_head(&request.id, &request.command);
    let mut args = request.args;
    let named = name_computer(&console, &head.command, &mut args);
    let computer = option(&args, "--computer").map(str::to_string);
    if let Err(error) = named {
        return (computer, failure(&head, error.code, &error.message, "failed", false));
    }
    let ctx = Ctx { head, args, timeout: COMMAND_TIMEOUT, console };
    let envelope = match dispatch(&ctx).await {
        Ok(envelope) => envelope,
        Err(fault) => fault_envelope(&ctx.head, fault),
    };
    (computer, envelope)
}

/// The computer a request names (`--computer NAME`, or `wake NAME`), named any
/// way a person or agent names one (`pick_computer`), becomes its `computer_`
/// id here. The plugin already names computers by that id, which is taken as
/// it is without reading the directory. Login commands name All Computers
/// as `all`, which is not a computer.
fn name_computer(console: &Console, command: &str, args: &mut [String]) -> crate::error::Result<()> {
    let slot = if command == "wake" {
        let mut at = 0;
        while args.get(at).is_some_and(|a| a.starts_with("--")) {
            at += 2;
        }
        at
    } else {
        match args.iter().position(|a| a == "--computer") {
            Some(at) => at + 1,
            None => return Ok(()),
        }
    };
    let all = command.starts_with("login-") && args.get(slot).is_some_and(|a| a == "all");
    let Some(name) = args.get_mut(slot).filter(|name| !all && !name.starts_with("computer_")) else { return Ok(()) };
    let directory = OperatorDirectory::open(&console.database)?;
    let row = directory.resolve_computer(name);
    directory.close();
    *name = row?.computer_id;
    Ok(())
}

/// `dispatch(ctx)` (ibara-bridge.mjs:734-813).
async fn dispatch(ctx: &Ctx) -> Handled {
    let command = ctx.head.command.as_str();
    match command {
        "login-settings" | "login-browser-status" | "login-on" | "login-not-now" | "login-off" | "login-rule" | "login-rows"
        | "login-answer" | "login-assist" | "login-share-with" | "login-sync" | "login-remove" | "login-probe" | "login-share" | "login-test-seed" => {
            logins::command(ctx).await
        }
        "directory" => selected::directory(ctx),
        "rename-computer" => selected::rename_computer(ctx),
        "remove-computer" => pairing::remove_computer(ctx),
        "connect-prompt" => Ok(ctx.ready(crate::operator::onboarding::connect_prompt_json())),
        "tailnet" => pairing::tailnet(ctx).await,
        "pair-start" => pairing::pair_start(ctx).await,
        "pair-status" => pairing::pair_status(ctx).await,
        "pair-cancel" => pairing::pair_cancel(ctx).await,
        "pair-requests" => pairing::pair_requests(ctx).await,
        "pair-answer" => pairing::pair_answer(ctx).await,
        "invite-create" => pairing::invite_create(ctx).await,
        "invites" => pairing::invites(ctx).await,
        "invite-revoke" => pairing::invite_revoke(ctx).await,
        "operator-session" => selected::operator_session(ctx).await,
        "operator-status" | "operator-task-status" | "operator-files" => selected::operator_read(ctx).await,
        "operator-observe" => selected::observe(ctx).await,
        "video" => video::command(ctx).await,
        "preview-release" => {
            ctx.console.frames.release(&ctx.args);
            Ok(ctx.ready(json!({"released": true})))
        }
        "operator-file-send" | "operator-file-receive" | "operator-file-resume" => files::human_files(ctx).await,
        "operator-control" => selected::selected_control(ctx).await,
        "open-viewer" => selected::open_viewer(ctx).await,
        "pick-file" | "pick-folder" => picker::pick_local(ctx).await,
        "operator-logs" | "operator-health" | "operator-power" | "operator-settings" | "operator-answer-attention" | "operator-repair"
        | "operator-tasks" | "operator-task" | "operator-artifacts" | "operator-procedures" | "operator-procedure"
        | "operator-task-extend" | "operator-task-revoke" | "operator-procedure-review" | "operator-access" | "operator-access-set"
        | "operator-access-remove" | "operator-access-unpair" | "operator-windows" | "operator-window-close" | "operator-window-move" => {
            everyday::per_computer(ctx).await
        }
        "operator-artifact-save" => everyday::artifact_save(ctx).await,
        "operator-theme" => fleet::operator_theme(ctx).await,
        "open-terminal" => everyday::open_terminal(ctx).await,
        "fleet-attention" => fleet::fleet_attention(ctx).await,
        "away" => fleet::away(ctx).await,
        "away-seen" => fleet::away_seen(ctx).await,
        "theme-fleet" => fleet::theme_fleet(ctx).await,
        "wake" => fleet::wake(ctx).await,
        "settings" => fleet::console_settings(ctx),
        "whats-new" => Ok(ctx.ready(crate::install::update::whats_new())),
        "update-check" => {
            if ctx.args.iter().any(|a| a == "--refresh") {
                updates::refresh().await.map(|v| ctx.ready(v)).map_err(Fault::Plain)
            } else { Ok(ctx.ready(updates::cached())) }
        }
        "whats-new-seen" => crate::install::update::whats_new_seen().map(|data| ctx.ready(data)).map_err(Fault::Plain),
        "unattended-boot" => Ok(ctx.ready(crate::install::unattended_boot::status_json())),
        "unattended-boot-lock" => crate::install::unattended_boot::console_lock(&ctx.args).map(|data| ctx.ready(data)).map_err(Fault::Plain),
        // Every computer is reached over its own pairing route; there is no
        // station. Plugins that still poll the station hear that it is absent.
        "status" => Ok(ctx.ready(json!({"station_configured": false}))),
        _ => Err(Fault::Plain(format!("Unknown command {command}."))),
    }
}

/// The answer line for one request: a reply, or a preview event for `operator-observe`.
fn answer_line(request_command: &str, computer: Option<String>, envelope: Value) -> String {
    let message = if request_command == "operator-observe" {
        json!({"event": "preview", "computer_id": computer.unwrap_or_default(), "data": envelope})
    } else {
        let id = envelope.get("request_id").cloned().unwrap_or(Value::Null);
        json!({"id": id, "envelope": envelope})
    };
    let mut line = message.to_string();
    line.push('\n');
    line
}

// ---------------------------------------------------------------------------
// The socket.

/// `$XDG_RUNTIME_DIR/ibara/ibarad.sock`.
pub fn socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    match runtime {
        Some(dir) if dir.is_absolute() => Ok(dir.join("ibara").join("ibarad.sock")),
        _ => Err("XDG_RUNTIME_DIR must name an absolute directory.".into()),
    }
}

/// Create (or tighten) the private socket directory: ours, a real directory, 0700.
fn prepare_directory(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))
        }
        Err(e) => Err(format!("{}: {e}", dir.display())),
        Ok(meta) if !meta.is_dir() || meta.uid() != current_uid() => {
            Err(format!("{} is not a directory owned by this user.", dir.display()))
        }
        Ok(meta) if meta.mode() & 0o777 != 0o700 => std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", dir.display())),
        Ok(_) => Ok(()),
    }
}

/// Bind the socket with mode 0600. A live socket means another operator service is
/// running; a stale one (nothing listening) is replaced.
fn bind(path: &Path) -> Result<UnixListener, String> {
    let dir = path.parent().ok_or("The socket path has no directory.")?;
    prepare_directory(dir)?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() || meta.uid() != current_uid() {
            return Err(format!("{} exists and is not this user's socket.", path.display()));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!("Another ibarad already serves {}.", path.display()));
        }
        std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // SAFETY: umask(2) only swaps the process mask; the runtime has no other threads yet.
    let previous = unsafe { libc::umask(0o177) };
    let listener = std::os::unix::net::UnixListener::bind(path);
    // SAFETY: as above, restoring the previous mask.
    unsafe { libc::umask(previous) };
    let listener = listener.map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", path.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    UnixListener::from_std(listener).map_err(|e| e.to_string())
}

/// Serve one connection until the peer closes it. Requests run concurrently;
/// answers are written in completion order.
async fn serve_connection(console: Arc<Console>, stream: UnixStream) {
    if stream.peer_cred().map(|c| c.uid()).ok() != Some(current_uid()) {
        return;
    }
    let (read, mut write) = stream.into_split();
    let (tx, mut outbox) = mpsc::channel::<String>(OUTBOX);
    tokio::spawn(async move {
        while let Some(line) = outbox.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    let permits = Arc::new(Semaphore::new(IN_FLIGHT));
    let mut reader = BufReader::new(read);
    loop {
        let line = match read_line_bounded(&mut reader, REQUEST_LIMIT, false).await {
            Ok(LineRead::Line(line)) => line,
            Ok(LineRead::Oversize) => continue,
            Ok(LineRead::Eof) | Err(_) => break,
        };
        let request = match parse_request(&line) {
            None => continue,
            Some(Err((id, reason))) => {
                let envelope = failure(&fallback_head(&id, ""), "INVALID_ARGUMENT", &reason, "failed", true);
                let _ = tx.send(answer_line("", None, envelope)).await;
                continue;
            }
            Some(Ok(request)) => request,
        };
        let command = request.command.clone();
        let computer = option(&request.args, "--computer").map(str::to_string);
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            let head = Head { request_id: request.id, command: fallback_head("", &command).command, target: "ibara".into() };
            let busy = failure(&head, "BUSY", "Too many requests are in flight on this connection.", "failed", true);
            let _ = tx.send(answer_line(&command, computer, busy)).await;
            continue;
        };
        let tx = tx.clone();
        let console = console.clone();
        // A request whose connection closed still runs to its own deadline, so an
        // effect is never cut short; only its answer is dropped.
        tokio::spawn(async move {
            let (computer, envelope) = answer(console, request).await;
            let _ = tx.send(answer_line(&command, computer, envelope)).await;
            drop(permit);
        });
    }
}

/// Accept connections until SIGTERM or SIGINT, then close every operator session.
pub async fn serve(
    listener: UnixListener,
    console: Arc<Console>,
    mut term: tokio::signal::unix::Signal,
    mut interrupt: tokio::signal::unix::Signal,
) {
    let connections = Arc::new(Semaphore::new(CONNECTIONS));
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = interrupt.recv() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                    let console = console.clone();
                    tokio::spawn(async move {
                        serve_connection(console, stream).await;
                        drop(permit);
                    });
                }
                Err(error) => {
                    eprintln!("ibarad: accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
        }
    }
    console.logins.stop();
    console.videos.close_all().await;
    console.sessions.close_all().await;
}

/// `ibarad --role operator`. Returns the exit code.
pub fn main() -> i32 {
    let path = match socket_path() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("ibarad: {message}");
            return 1;
        }
    };
    let database = directory_path(None);
    if !database.is_absolute() {
        eprintln!("ibarad: Operator directory path must be absolute.");
        return 1;
    }
    // A preview frame's buffers (base64, PNG, pixels, the scaled copy) are each
    // 0.1 to 2.8 MB. glibc raises its mmap threshold after the first such buffer is
    // freed, and then keeps later ones in its heap: +9 MiB PSS that never went back
    // after 250 frames. A fixed threshold maps every block of 128 KiB or more on
    // its own and returns it when freed (`frames.rs`).
    #[cfg(target_env = "gnu")]
    // SAFETY: mallopt only changes this process's allocator settings, before any thread starts.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ibarad: {error}");
            return 1;
        }
    };
    runtime.block_on(async {
        // A visible socket can be stopped immediately, even while startup continues.
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(term), Ok(interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            eprintln!("ibarad: cannot install signal handlers");
            return 1;
        };
        let listener = match bind(&path) {
            Ok(listener) => listener,
            Err(message) => {
                eprintln!("ibarad: {message}");
                return 1;
            }
        };
        let Some(frames) = frames::directory(&path) else { return 1 };
        if let Err(message) = frames::reset(&frames) {
            eprintln!("ibarad: {message}");
            return 1;
        }
        // The sharing computer reads cookies; gated disposable test seeding also writes them.
        let jobs: &'static [&'static str] = if std::env::var("IBARA_LOGIN_TESTS").as_deref() == Ok("1") { &["share", "receive"] } else { &["share"] };
        let chrome = match crate::desktop::chrome::ChromeBridge::listen(&path.with_file_name("chrome.sock"), jobs).await {
            Ok(chrome) => chrome,
            Err(_) => { eprintln!("ibarad: could not start the local browser bridge"); return 1; }
        };
        let console = Arc::new(Console::new(database, frames.clone()));
        *console.chrome.lock().unwrap_or_else(|e| e.into_inner()) = Some(chrome.clone());
        logins::start(&console);
        updates::start();
        if crate::install::system::station_owner().is_none() { crate::install::user::start_shell_retry(); }
        eprintln!("ibarad: operator console serving {}", path.display());
        serve(listener, console, term, interrupt).await;
        chrome.close().await;
        let _ = frames::reset(&frames);
        // Only the service that bound the socket removes it.
        let _ = std::fs::remove_file(&path);
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_without_an_id_are_dropped_and_malformed_ones_are_answered() {
        assert!(parse_request(b"not json").is_none());
        assert!(parse_request(br#"{"command":"status"}"#).is_none());
        assert!(parse_request(b"{\"id\":\"a\\u0001\",\"command\":\"status\"}").is_none());
        assert!(matches!(parse_request(br#"{"id":"r1","args":[]}"#), Some(Err((id, _))) if id == "r1"));
        assert!(matches!(parse_request(br#"{"id":"r1","command":"tasks","args":[1]}"#), Some(Err(_))));
        assert!(matches!(parse_request(br#"{"id":"r1","command":"tasks","argv":[]}"#), Some(Err((_, m))) if m == "Unknown request field argv."));
        let Some(Ok(request)) = parse_request(br#"{"id":"read-3-operator-files-list:c1#gen=2","command":"status","for_computer":"c1"}"#) else {
            panic!("a request id with # is still usable, and an old plugin's for_computer is ignored");
        };
        assert_eq!((request.command.as_str(), request.args.len()), ("status", 0));
    }
}
