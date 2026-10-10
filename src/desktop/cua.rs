//! Cua (`cua-driver mcp`) as a private child process: agent keyboard and
//! pointer, accessibility trees and actions, and the named agent cursor.
//!
//! - One worker, started on first use and stopped after [`IDLE_STOP`] without
//!   a call, never while the agent's named cursor is drawn (it goes with the
//!   worker). It speaks MCP over inherited pipes; there is no port.
//! - Every `cua-driver` run gets [`private_env`]: telemetry off by the
//!   environment, no update check, and Cua's own state in ibara's private
//!   home. A worker never starts unless Cua confirms telemetry is off by the
//!   environment.
//! - Calls are serialised. A typing piece is never cancelled once sent: a
//!   handoff, and a person's mouse move, wait for the piece in flight.
//!   Stopping Cua's worker in the middle of a piece crashes the GTK 3 app
//!   being typed into (Mousepad: 4 of 26 moves during typing on a test
//!   computer, 27 September; step 1 saw it after a cancelled type). A
//!   worker that does not answer in time is killed; the Hyprland plugin
//!   releases its held keys when the connection closes.
//! - The agent's named cursor stays where a screen shows it: every motion
//!   call sets all the fields ibara relies on ([`motion`]), a lead stops at
//!   the nearest point a screen shows, and from one screen to another the
//!   cursor lands in one frame ([`Cua::pace`]).
//! - Input uses the plugin's exact-target foreground route; elements use
//!   AT-SPI through Cua. A refused route is reported as refused, never retried
//!   on another route. Desktop-scope keys are never sent: Cua's desktop
//!   keyboard replaces every client's keymap with a two-key one.

use super::atspi::{ElementPage, RawElement, ResolvedElement, Tree};
use super::handover::Motion;
use super::hyprland::{Rect, Screens};
use super::run::{Cancel, Cmd, clip, run};
use crate::error::{IbaraError, Result, invalid};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// The worker exits this long after its last call.
pub const IDLE_STOP: Duration = Duration::from_secs(120);
/// Cua's foreground route delivers a whole call's keys at once. A GTK file
/// chooser given a 99-character path that way ignored the next Return (Tulip0,
/// 2 of 2 at 64 and at 32 characters a piece; 3 of 3 passed at 16 with a 40 ms
/// gap). Short pieces also keep a handoff's wait short.
pub const TYPE_PIECE: usize = 16;
/// Between typing pieces.
const PIECE_GAP: Duration = Duration::from_millis(40);
/// Cua's reason when it stopped text before sending the next key: exactly
/// the keys it counts arrived ([`partial`]).
const INPUT_ENDED: &str = "text_interrupted";
/// Cua's reason when its connection failed with a key sent and unacknowledged.
const KEY_IN_FLIGHT: &str = "transport_or_protocol_error";
/// Typing goes on after [`INPUT_ENDED`] at most this often in one text, each
/// time after [`RESUME_AFTER`]: a display change comes as several of
/// Hyprland's events, and the plugin ends the input connection on each.
const RESUMES: usize = 3;
const RESUME_AFTER: Duration = Duration::from_millis(500);
/// Starting a worker, including the MCP handshake.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// One accessibility walk.
const READ_TIMEOUT: Duration = Duration::from_secs(12);
/// How long Cua may walk a window's accessibility tree. Cua 0.29 stops a
/// walk after 1 s unless asked for more, and returns the part it read:
/// Google Chrome showing a sign-up page (42 elements) took 1.9–2.4 s whole on
/// a test computer (28 September), so a 1 s read held 13–26 of them, a
/// different part each time, and a button a frame offered was missing from
/// the read made to click it. This leaves [`READ_TIMEOUT`] room for Cua's answer.
/// Cua 0.28 walks without a limit and ignores the field.
const WALK_BUDGET: Duration = Duration::from_secs(10);
/// How long the look for the field typed text goes to (before the first
/// piece) may walk the window's tree: Mousepad reads in 0.25 s. A read cut
/// short proves no field, since another editable element may be in the part
/// not read, and the text is typed with the cursor where it is.
const FIELD_LOOK: Duration = Duration::from_secs(1);
/// One input action. Only a guard against a hung worker.
const ACT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a handoff waits for the Cua action in flight.
pub const SETTLE: Duration = Duration::from_secs(20);
/// Largest accessibility snapshot requested.
const MAX_ELEMENTS: u32 = 1500;
/// Largest reply line accepted from the worker.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Cua's Wayland overlay glides to a target but reports no arrival, so its
/// action fired while the named cursor was still on the way: it was seen to
/// trail the real pointer (26 September); a fixed 260 ms glide then
/// looked inhumanly fast. Each glide now takes a person's pointing time for
/// its distance (Fitts's law, [`human_glide`]); the vendored Hyprland plugin
/// moves the real pointer over the same time and rests before pressing
/// (`vendor/cua-hyprland-plugin/IBARA.md`).
const HOVER: Duration = Duration::from_millis(140);
/// How long Cua 0.29.1 takes to draw a new session's cursor after its first
/// move: 31–36 ms on a test computer (27 September), 136 ms on another run.
pub const DRAWN: Duration = Duration::from_millis(200);
/// Cua places a never-shown cursor 140 px up and left of its first target,
/// and a move's tip lands 16 px back along 45°: a first move this far down
/// and right of the point, then one to the point, draws the cursor first
/// exactly there (probed on Cua 0.29.1).
const SEED: f64 = 140.0 + 11.313_708_498_984_761;
/// Cua 0.29's refusal of window pixels without a current picture of the
/// window taken by the same session (`element_cache.rs`); nothing was sent.
const NO_PICTURE: &str = "screenshot_context_missing";

/// How long a person takes to point `distance` logical pixels at a control:
/// 250 ms + 150 ms × log2(distance / 20 + 1), 350–1400 ms (about 0.45 s for
/// 30 px, 0.64 s for 100 px, 1.05 s across 800 px). Unknown distance: 0.7 s.
pub fn human_glide(distance: Option<f64>) -> Duration {
    let ms = match distance {
        Some(d) => 250.0 + 150.0 * (d.max(0.0) / 20.0 + 1.0).log2(),
        None => 700.0,
    };
    Duration::from_millis(ms.clamp(350.0, 1400.0) as u64)
}

/// What a call is, for session, dispatch counting and error mapping.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A read: no session.
    Read,
    /// Moves only the agent cursor: opens the session, is not an effect.
    Cursor,
    /// A picture of a window for the pointer input that follows: opens the
    /// session (Cua ties the picture to it), is not an effect.
    Picture,
    /// Input: opens the session and counts as dispatched.
    Effect,
}

static DISPATCHED: AtomicU64 = AtomicU64::new(0);

/// Mutating Cua calls sent so far (for `execution_not_started`).
pub fn dispatch_count() -> u64 {
    DISPATCHED.load(Ordering::SeqCst)
}

/// A window's only enabled editable text element, as one read found it.
#[derive(Debug, Clone)]
pub struct Field {
    /// Its box as Cua gives it: x, y, width, height.
    frame: Option<[f64; 4]>,
    /// The read was whole, so the window has no other.
    whole: bool,
}

/// Where an element click stands after [`Cua::click_element`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ElementClick {
    /// Activated through accessibility; the element's center on the
    /// screen, when Cua gives its box.
    Sent(Option<(f64, f64)>),
    /// Nothing sent: for the pointer at `at`, the element's center on the
    /// screen, inside its window.
    Pointer { at: (f64, f64) },
}

#[derive(Debug, Clone)]
pub struct CuaConfig {
    pub program: PathBuf,
    /// Private home (0700) for Cua's telemetry state and release channel.
    pub home: PathBuf,
    pub env: Arc<[(OsString, OsString)]>,
}

/// The environment of every `cua-driver` run, as Cua 0.28.2 reads it
/// (`telemetry.rs`, `version_check.rs`, `release_channel.rs`):
///
/// - `CUA_DRIVER_RS_TELEMETRY_ENABLED=false` switches telemetry off and takes
///   precedence over any saved preference, so the desktop user's own
///   `cua-driver telemetry enable` cannot turn it on for ibara.
/// - `CUA_DRIVER_RS_UPDATE_CHECK=false`: `mcp` otherwise asks GitHub's
///   releases API for a newer version as it starts (at most once per 20 h);
///   the telemetry setting does not cover that.
/// - `CUA_DRIVER_TELEMETRY_HOME` and `CUA_DRIVER_RS_HOME` keep the installation
///   ID, telemetry markers and release channel in ibara's home, not the
///   user's shared `~/.cua-driver`. (`CUA_DRIVER_HOME` only locates Cua's
///   history helper.)
pub fn private_env(home: &Path) -> [(&'static str, OsString); 4] {
    [
        ("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false".into()),
        ("CUA_DRIVER_RS_UPDATE_CHECK", "false".into()),
        ("CUA_DRIVER_TELEMETRY_HOME", home.into()),
        ("CUA_DRIVER_RS_HOME", home.into()),
    ]
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// The Cua session (agent cursor) currently open, by label.
    session: Option<String>,
}

struct Inner {
    cfg: CuaConfig,
    worker: tokio::sync::Mutex<Option<Worker>>,
    last_used: Mutex<Instant>,
    agent: Mutex<Option<String>>,
    reaper: Mutex<bool>,
    /// [`IDLE_STOP`], shorter in tests.
    idle_stop: Mutex<Duration>,
    /// Where the named cursor was last sent, for the next glide's distance.
    cursor_at: Mutex<Option<(f64, f64)>>,
    /// Input in flight that moves the real pointer (point clicks, drags,
    /// wheel notches); keys, text and accessibility actions do not.
    motion: Arc<Motion>,
    /// The agent's named cursor is shown (the agent has the screen).
    shown: AtomicBool,
    /// The person took the screen since the named cursor was last shown.
    yielded: AtomicBool,
    /// A read in flight, which a person's move may stop.
    reading: Mutex<Reading>,
    // One bounded native snapshot; actions still resolve against fresh state.
    elements_page: Mutex<Option<(i64, u64, Option<String>, Instant, Tree)>>,
}

/// A read or picture in flight, for [`Cua::hide_cursor_now`].
#[derive(Default)]
enum Reading {
    #[default]
    None,
    /// On the worker with this process ID; `session`: the agent's session
    /// (its named cursor) is open on it.
    Worker { pid: u32, session: bool },
    /// The worker was stopped to take the named cursor down.
    Stopped,
}

/// The Cua adapter. Cheap to clone.
#[derive(Clone)]
pub struct Cua {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Cua {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cua").field("program", &self.inner.cfg.program).finish()
    }
}

fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", message, true).with("execution_not_started", true)
}

/// A Cua window id is the Hyprland window address as an integer.
pub fn window_id(address: &str) -> Result<u64> {
    if let Some(id) = address.strip_prefix("gnome:") { return id.parse::<u32>().map(u64::from).map_err(|_| invalid("Invalid GNOME window token.")); }
    u64::from_str_radix(address.trim_start_matches("0x"), 16).map_err(|_| invalid("Invalid window address."))
}

impl Cua {
    pub fn new(cfg: CuaConfig) -> Cua {
        Cua {
            inner: Arc::new(Inner {
                cfg,
                worker: tokio::sync::Mutex::new(None),
                last_used: Mutex::new(Instant::now()),
                agent: Mutex::new(None),
                reaper: Mutex::new(false),
                idle_stop: Mutex::new(IDLE_STOP),
                cursor_at: Mutex::new(None),
                motion: Arc::new(Motion::default()),
                shown: AtomicBool::new(false),
                yielded: AtomicBool::new(false),
                reading: Mutex::new(Reading::None),
                elements_page: Mutex::new(None),
            }),
        }
    }

    /// The agent whose cursor subsequent actions show (`codex@vesper`), or
    /// none. Clearing it closes the cursor at the next idle check.
    pub fn set_agent(&self, label: Option<String>) {
        if self.agent() != label {
            *self.inner.elements_page.lock().unwrap_or_else(|p|p.into_inner()) = None;
        }
        *self.inner.agent.lock().unwrap_or_else(|p| p.into_inner()) = label;
    }

    fn agent(&self) -> Option<String> {
        self.inner.agent.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Stop the worker after `idle` without a call instead of [`IDLE_STOP`].
    #[cfg(test)]
    pub fn stop_when_idle(&self, idle: Duration) {
        *self.inner.idle_stop.lock().unwrap_or_else(|p| p.into_inner()) = idle;
    }

    /// A worker is running.
    pub async fn running(&self) -> bool {
        self.inner.worker.lock().await.is_some()
    }

    /// Wait for the call in flight (a typing piece is never cancelled), up to
    /// `limit`. `CONTROL_UNSETTLED` when it does not end in time.
    pub async fn settle(&self, limit: Duration) -> Result<()> {
        match tokio::time::timeout(limit, self.inner.worker.lock()).await {
            Ok(_) => Ok(()),
            Err(_) => Err(IbaraError::new("CONTROL_UNSETTLED", "A Cua input action is still running.", false)
                .requires_reconciliation()),
        }
    }

    /// Stop the worker now. The plugin releases anything the worker held when
    /// its connection closes.
    pub async fn stop(&self) {
        let mut slot = self.inner.worker.lock().await;
        self.inner.shown.store(false, Ordering::SeqCst);
        if let Some(mut worker) = slot.take() {
            close(&mut worker).await;
        }
    }

    /// Before every worker start: Cua must report telemetry off, decided by
    /// the environment. Checked each time, so a replaced `cua-driver` that no
    /// longer honours the variable is caught at its first start.
    async fn confirm_telemetry_off(&self) -> Result<()> {
        let home = &self.inner.cfg.home;
        std::fs::create_dir_all(home).map_err(|e| unavailable(format!("Cua home unavailable: {e}")))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
        }
        let mut command = Cmd::new(&self.inner.cfg.program)
            .args(["telemetry", "status", "--json"])
            .envs(&self.inner.cfg.env)
            .timeout(Duration::from_secs(10))
            .max_output(64 * 1024);
        for (key, value) in private_env(home) {
            command = command.env(key, value);
        }
        let status = run(command).await?;
        let reported: Value = serde_json::from_slice(&status.stdout).unwrap_or(Value::Null);
        if !status.success() || reported["enabled"] != json!(false) || reported["source"] != json!("environment") {
            return Err(unavailable("Cua did not confirm that its telemetry is off; Cua was not started.")
                .with("detail", clip(&status.stdout_text(), 200)));
        }
        Ok(())
    }

    async fn spawn(&self) -> Result<Worker> {
        self.confirm_telemetry_off().await?;
        let cfg = &self.inner.cfg;
        let mut command = tokio::process::Command::new(&cfg.program);
        command
            .arg("mcp")
            .envs(cfg.env.iter().map(|(k, v)| (k, v)))
            .envs(private_env(&cfg.home))
            .env("CUA_DRIVER_RS_ENABLE_WAYLAND", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| unavailable(format!("Cua could not start: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(unavailable("Cua pipes unavailable."));
        };
        let mut worker = Worker { child, stdin, stdout: BufReader::new(stdout), next_id: 0, session: None };
        let hello = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "ibarad", "version": env!("CARGO_PKG_VERSION")},
        });
        let started = tokio::time::timeout(START_TIMEOUT, async {
            if let Err(rejection) = request(&mut worker, "initialize", hello).await? {
                return Err(invalid(format!("Cua rejected initialize: {}", rejection.message)));
            }
            notify(&mut worker, "notifications/initialized").await
        })
        .await;
        match started {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(e)) => {
                close(&mut worker).await;
                Err(unavailable(format!("Cua did not start: {}", e.message)))
            }
            Err(_) => {
                close(&mut worker).await;
                Err(unavailable("Cua did not answer its start handshake."))
            }
        }
    }

    fn start_reaper(&self) {
        let mut started = self.inner.reaper.lock().unwrap_or_else(|p| p.into_inner());
        if *started {
            return;
        }
        *started = true;
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let stop_after = weak.upgrade().map_or(IDLE_STOP, |inner| *inner.idle_stop.lock().unwrap_or_else(|p| p.into_inner()));
                tokio::time::sleep((stop_after / 4).min(Duration::from_secs(10))).await;
                let Some(inner) = weak.upgrade() else { return };
                let idle = inner.last_used.lock().unwrap_or_else(|p| p.into_inner()).elapsed();
                let wanted = inner.agent.lock().unwrap_or_else(|p| p.into_inner()).clone();
                let Ok(mut slot) = inner.worker.try_lock() else { continue };
                let Some(worker) = slot.as_mut() else { continue };
                // The named cursor goes with the worker: kept while it is drawn,
                // however long the agent thinks.
                if idle >= stop_after && !inner.shown.load(Ordering::SeqCst) {
                    let mut worker = slot.take().expect("checked");
                    close(&mut worker).await;
                } else if worker.session.is_some() && worker.session != wanted {
                    // The task that owned this cursor ended; take its cursor down.
                    inner.shown.store(false, Ordering::SeqCst);
                    let label = worker.session.take().unwrap_or_default();
                    let _ = tokio::time::timeout(READ_TIMEOUT, call_tool(worker, "end_session", json!({"session": label}))).await;
                }
            }
        });
    }

    /// One tool call. Input and pictures carry the agent's session while its
    /// named cursor is shown, and are not sent while it is hidden.
    async fn call(&self, tool: &str, args: Value, mutates: bool, limit: Duration) -> Result<Value> {
        self.call_as(tool, args, if mutates { Kind::Effect } else { Kind::Read }, limit).await
    }

    /// The running worker, started when there is none.
    async fn worker<'a>(&self, slot: &'a mut Option<Worker>) -> Result<&'a mut Worker> {
        self.start_reaper();
        *self.inner.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        if slot.as_mut().is_some_and(|w| matches!(w.child.try_wait(), Ok(Some(_)))) {
            slot.take();
            self.inner.shown.store(false, Ordering::SeqCst);
        }
        if slot.is_none() {
            *slot = Some(self.spawn().await?);
        }
        Ok(slot.as_mut().expect("worker"))
    }

    async fn call_as(&self, tool: &str, mut args: Value, kind: Kind, limit: Duration) -> Result<Value> {
        let mutates = kind == Kind::Effect;
        let stoppable = matches!(kind, Kind::Read | Kind::Picture);
        // A read stopped by a person's move is taken once more.
        let mut again = kind == Kind::Read;
        let mut slot = self.inner.worker.lock().await;
        loop {
            let worker = self.worker(&mut slot).await?;
            let session = self.agent().filter(|label| worker.session.as_deref() == Some(label.as_str()));
            let shown = self.inner.shown.load(Ordering::SeqCst);
            match (kind, session) {
                (Kind::Read, _) => {}
                // Moving a hidden cursor draws it again (Cua 0.29.1), and so
                // does input: while the person has the screen, the agent's
                // cursor is left alone and its input is not sent.
                (Kind::Cursor, _) if !shown => return Err(unavailable("The agent's cursor is not shown.")),
                (Kind::Effect | Kind::Picture, _) if !shown && self.has_agent() => return Err(self.not_held()),
                (_, Some(label)) => args["session"] = json!(label),
                (_, None) => {}
            }
            if let Some(pid) = worker.child.id().filter(|_| stoppable) {
                *self.reading() = Reading::Worker { pid, session: worker.session.is_some() };
            }
            if mutates {
                DISPATCHED.fetch_add(1, Ordering::SeqCst);
            }
            let sent = if again { args.clone() } else { std::mem::take(&mut args) };
            let reply = tokio::time::timeout(limit, call_tool(worker, tool, sent)).await;
            *self.inner.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
            if stoppable && matches!(std::mem::take(&mut *self.reading()), Reading::Stopped) {
                // Stopped to take the named cursor down; its session went too.
                self.inner.shown.store(false, Ordering::SeqCst);
                if let Some(mut dead) = slot.take() {
                    dead.session = None;
                    close(&mut dead).await;
                }
                match reply {
                    Ok(Ok(Ok(outcome))) => return answer(tool, outcome, mutates),
                    _ if kind == Kind::Picture => return Err(self.not_held()),
                    _ if std::mem::take(&mut again) => continue,
                    _ => return Err(unavailable(format!("Cua stopped during {tool}."))),
                }
            }
            let outcome = match reply {
                Ok(Ok(Ok(result))) => result,
                // Cua answered, so the worker is still in step; only the call failed.
                Ok(Ok(Err(rejection))) => return Err(rejection.error(tool, mutates)),
                Ok(Err(error)) => {
                    // The pipe broke: the worker is gone or unusable.
                    self.inner.shown.store(false, Ordering::SeqCst);
                    let mut dead = slot.take().expect("worker");
                    close(&mut dead).await;
                    return Err(if mutates {
                        IbaraError::new("OUTCOME_UNKNOWN", format!("Cua stopped during {tool}: {}", error.message), false)
                            .requires_reconciliation()
                    } else {
                        unavailable(format!("Cua stopped during {tool}."))
                    });
                }
                Err(_) => {
                    self.inner.shown.store(false, Ordering::SeqCst);
                    let mut hung = slot.take().expect("worker");
                    close(&mut hung).await;
                    return Err(if mutates {
                        IbaraError::new("OUTCOME_UNKNOWN", format!("Cua did not answer {tool}; it was stopped."), false)
                            .requires_reconciliation()
                    } else {
                        IbaraError::new("TIMEOUT", format!("Cua did not answer {tool}."), true)
                    });
                }
            };
            return answer(tool, outcome, mutates);
        }
    }

    fn reading(&self) -> std::sync::MutexGuard<'_, Reading> {
        self.inner.reading.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Why agent input is not sent while its named cursor is hidden.
    fn not_held(&self) -> IbaraError {
        if self.inner.yielded.load(Ordering::SeqCst) {
            super::handover::person_busy()
        } else {
            unavailable("The agent's cursor is not on the screen, so ibara sent nothing. Try again.").with("reason", "agent_cursor_hidden")
        }
    }

    // ---- accessibility ----

    async fn snapshot(&self, pid: i64, window: u64) -> Result<Value> {
        self.snapshot_within(pid, window, WALK_BUDGET).await
    }

    async fn snapshot_within(&self, pid: i64, window: u64, walk: Duration) -> Result<Value> {
        let reply = self
            .call(
                "get_window_state",
                json!({"pid": pid, "window_id": window, "include_screenshot": false, "max_elements": MAX_ELEMENTS, "timeout_ms": walk.as_millis() as u64}),
                false,
                READ_TIMEOUT,
            )
            .await?;
        Ok(reply.get("structuredContent").cloned().unwrap_or(Value::Null))
    }

    /// The window's elements as the compact tree the controller already reads.
    pub async fn tree(&self, pid: i64, window: u64) -> Result<Tree> {
        let snapshot = self.snapshot(pid, window).await?;
        Ok(tree_from_snapshot(pid, window, &snapshot))
    }

    /// One page of compact, ranked elements.
    pub async fn elements(&self, pid: i64, window: u64, query: Option<&str>, limit: u32, cursor: Option<u32>) -> Result<ElementPage> {
        let tree = if cursor.is_some() {
            self.inner.elements_page.lock().unwrap_or_else(|p|p.into_inner()).as_ref()
                .filter(|(p,w,q,at,_)|*p==pid && *w==window && q.as_deref()==query && at.elapsed()<Duration::from_secs(30))
                .map(|(_,_,_,_,tree)|tree.clone())
                .ok_or_else(||stale("Native observation snapshot expired or changed; observe without cursor."))?
        } else {
            let tree=self.tree(pid, window).await?;
            *self.inner.elements_page.lock().unwrap_or_else(|p|p.into_inner())=Some((pid,window,query.map(str::to_owned),Instant::now(),tree.clone()));
            tree
        };
        Ok(page(tree, query, limit, cursor))
    }

    /// The element again, by identity, in a fresh snapshot, while it is
    /// still unique there; else `STALE_TARGET`. With it, the box of the
    /// window's own top element in that snapshot, which says how the
    /// snapshot's boxes sit on the screen ([`screen_center`]).
    async fn resolve_fresh(&self, selector: &Value) -> Result<(Value, ResolvedElement, Option<[f64; 4]>)> {
        let gone = || stale("Semantic element no longer resolves uniquely.");
        let pid = selector["pid"].as_i64().unwrap_or(0);
        let window = selector["window_id"].as_u64().unwrap_or(0);
        if pid <= 0 || window == 0 {
            return Err(gone());
        }
        let snapshot = self.snapshot(pid, window).await?;
        let rows = identities(&snapshot);
        let matches: Vec<&(Value, Value)> = rows.iter().filter(|(identity, _)| *identity == selector["identity"]).collect();
        let element = match matches.as_slice() {
            [(_, element)] => element,
            [] if snapshot["truncated"] == true => {
                return Err(stale("Nothing was sent: ibara could read only part of this window's elements, and that element was not in the part read this time.")
                    .with("reason", "read_in_part")
                    .with("next", "For a web page, use browser_act with the page's elements (computer_observe with surface \"tab\"); otherwise click a point from computer_observe with view \"image\"."));
            }
            _ => return Err(gone()),
        };
        // Named as the frame named it: a GTK 3 menu item's label ends in spaces.
        let (name, text) = name_and_value(element);
        let resolved = ResolvedElement {
            role: element["role"].as_str().unwrap_or("").to_string(),
            name,
            states: states_of(element),
            actions: strings(&element["actions"]),
            text,
        };
        let top = snapshot["elements"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["parent_index"].is_null() && matches!(e["role"].as_str(), Some("frame" | "window" | "dialog" | "alert" | "file chooser")))
            .and_then(frame_of);
        Ok((element.clone(), resolved, top))
    }

    /// How many of the window's menus are open (see [`open_menus`]).
    pub async fn open_menus(&self, pid: i64, window: u64) -> Result<usize> {
        Ok(open_menus(&self.snapshot(pid, window).await?))
    }

    /// The next move towards choosing the menu item `selector` (see
    /// [`MenuMove`]), from a fresh read of its window.
    pub async fn menu_move(&self, selector: &Value, role: &str, name: &str) -> Result<MenuMove> {
        let snapshot = self.snapshot(selector["pid"].as_i64().unwrap_or(0), selector["window_id"].as_u64().unwrap_or(0)).await?;
        menu_move(&snapshot, &selector["identity"], role, name)
    }

    /// Click an element through accessibility by its token from the latest
    /// read of its window (a menu, which opens).
    pub async fn click_token(&self, pid: i64, window: u64, token: &str) -> Result<()> {
        self.call("click", json!({"pid": pid, "window_id": window, "element_token": token}), true, ACT_TIMEOUT).await.map(drop)
    }

    /// Click an element after checking it still resolves uniquely to the
    /// same role and name. A left click activates it through accessibility:
    /// the named cursor goes to the element first, and `cancel` is honoured
    /// once it has arrived. Cua has no accessibility action for a double or
    /// right click: on Hyprland it would send them as background pointer
    /// input, which it does not offer to ibara, and its failure there comes
    /// after dispatch. Those, and a left click Cua refused for the same
    /// reason before sending anything ([`background_refused`]), are for the
    /// pointer at the element's center instead ([`ElementClick::Pointer`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn click_element(
        &self,
        selector: &Value,
        role: &str,
        name: &str,
        tool: &str,
        window: Rect,
        cancel: Option<&Cancel>,
        screens: &Screens,
    ) -> Result<ElementClick> {
        let (element, live, top) = self.resolve_fresh(selector).await?;
        if live.role != role || live.name != name {
            return Err(stale("Semantic identity changed."));
        }
        let center = frame_of(&element).map(|frame| screen_center(frame, top, window));
        let inside = center.filter(|&(x, y)| {
            x >= window.x as f64 && y >= window.y as f64 && x < (window.x + window.width) as f64 && y < (window.y + window.height) as f64
        });
        let pointer = |refused: Option<IbaraError>| match inside {
            Some(at) => Ok(ElementClick::Pointer { at }),
            None => Err(refused.unwrap_or_else(|| {
                unavailable("Nothing was sent: that element is not inside its window on the screen now, so the pointer cannot reach it.")
                    .with("next", "Scroll it into view, observe again and click it; or click a point from computer_observe with view \"image\".")
            })),
        };
        if tool != "click" {
            return pointer(None);
        }
        let token = element["element_token"].as_str().ok_or_else(|| stale("Element has no token."))?;
        if let Some((x, y)) = center {
            self.lead(x, y, screens).await;
        }
        unless_cancelled(cancel)?;
        let args = json!({"pid": selector["pid"], "window_id": selector["window_id"], "element_token": token});
        match self.call(tool, args, true, ACT_TIMEOUT).await {
            Ok(_) => Ok(ElementClick::Sent(center)),
            Err(refused) if background_refused(&refused) => pointer(Some(refused)),
            Err(refused) => Err(refused),
        }
    }

    // ---- input (exact-target foreground) ----

    fn target(pid: i64, window: u64) -> Value {
        json!({"pid": pid, "window_id": window, "delivery_mode": "foreground"})
    }

    /// A left, right or middle click, or a double click, at window-local
    /// pixels. `cancel` stops it before a second try (see `at_point`).
    #[allow(clippy::too_many_arguments)]
    pub async fn click_at(&self, pid: i64, window: u64, x: f64, y: f64, button: &str, double: bool, cancel: Option<&Cancel>) -> Result<()> {
        let mut args = Self::target(pid, window);
        args["x"] = json!(x);
        args["y"] = json!(y);
        let tool = match (button, double) {
            ("left", true) => "double_click",
            ("right", false) => "right_click",
            ("left", false) => "click",
            ("middle", false) => {
                args["button"] = json!("middle");
                "click"
            }
            _ => return Err(invalid("Double click uses the left button.").with("field", "button")),
        };
        self.at_point(tool, pid, window, args, ACT_TIMEOUT, cancel).await
    }

    pub async fn drag(&self, pid: i64, window: u64, from: (f64, f64), to: (f64, f64), duration: Duration, cancel: Option<&Cancel>) -> Result<()> {
        let mut args = Self::target(pid, window);
        args["from_x"] = json!(from.0);
        args["from_y"] = json!(from.1);
        args["to_x"] = json!(to.0);
        args["to_y"] = json!(to.1);
        args["duration_ms"] = json!(duration.as_millis().min(10_000) as u64);
        self.at_point("drag", pid, window, args, ACT_TIMEOUT + duration, cancel).await
    }

    /// Pointer input at window pixels. Cua 0.29 refuses window pixels
    /// ([`NO_PICTURE`]) until this session's latest look at the window
    /// includes a picture, and ibara's element reads take none. Then ibara
    /// takes one and sends the input once more, unless `cancel` fired
    /// meanwhile. Cua 0.28 never asks, so no picture is taken there.
    async fn at_point(&self, tool: &str, pid: i64, window: u64, args: Value, limit: Duration, cancel: Option<&Cancel>) -> Result<()> {
        // The plugin moves the real pointer to the point.
        let _moving = self.inner.motion.begin();
        match self.call(tool, args.clone(), true, limit).await {
            Err(refused) if refused.details.get("reason") == Some(&json!(NO_PICTURE)) => {
                if let Err(e) = self.picture(pid, window).await {
                    crate::controller::log_event("cua_window_picture_failed", &e.message);
                    // The person took the screen: say so rather than Cua's refusal.
                    return Err(if e.details.get("reason") == Some(&json!("person_active")) { e } else { refused });
                }
                unless_cancelled(cancel)?;
                self.call(tool, args, true, limit).await.map(|_| ())
            }
            sent => sent.map(|_| ()),
        }
    }

    /// Have Cua take a picture of the window at its own size, so window
    /// pixels stay window pixels (a smaller picture would scale them). Cua
    /// writes it to `/dev/null`: ibara needs Cua to have taken it, not the
    /// picture, and none is kept or sent over the pipe.
    async fn picture(&self, pid: i64, window: u64) -> Result<()> {
        let args = json!({
            "pid": pid,
            "window_id": window,
            "include_accessibility_tree": false,
            "include_screenshot": true,
            "max_image_dimension": 0,
            "screenshot_out_file": "/dev/null",
        });
        self.call_as("get_window_state", args, Kind::Picture, READ_TIMEOUT).await.map(|_| ())
    }

    /// Wheel notches into the window's focused region.
    pub async fn scroll(&self, pid: i64, window: u64, dx: i32, dy: i32) -> Result<()> {
        // Wheel input may bring the real pointer into the window first.
        let _moving = self.inner.motion.begin();
        let mut sent = false;
        for (amount, direction) in [(dy, if dy > 0 { "down" } else { "up" }), (dx, if dx > 0 { "right" } else { "left" })] {
            if amount == 0 {
                continue;
            }
            let mut args = Self::target(pid, window);
            args["direction"] = json!(direction);
            args["amount"] = json!(amount.unsigned_abs().min(50));
            if let Err(error) = self.call("scroll", args, true, ACT_TIMEOUT).await {
                // The other direction's notches went through.
                return Err(if sent { error.with("execution_not_started", false) } else { error });
            }
            sent = true;
        }
        Ok(())
    }

    /// A chord such as `ctrl+s`, already validated by [`super::input::cua_keys`].
    pub async fn key(&self, pid: i64, window: u64, keys: &[String]) -> Result<()> {
        let mut args = Self::target(pid, window);
        let tool = if let [only] = keys {
            args["key"] = json!(only);
            "press_key"
        } else {
            args["keys"] = json!(keys);
            "hotkey"
        };
        self.call(tool, args, true, ACT_TIMEOUT).await.map(|_| ())
    }

    /// ASCII text in pieces that are never cancelled once sent: a handoff
    /// and a person's move let the piece in flight end, and `cancel` takes
    /// effect between pieces. `focused` is asked between pieces too, so a
    /// window that took the keyboard focus meanwhile gets none of the rest.
    /// When Cua stops a piece before its next key ([`INPUT_ENDED`], as on a
    /// display change), exactly the keys it counts arrived, and the rest goes
    /// on from there past the same checks, a few times at most.
    /// A failure says how many characters were typed for sure
    /// (`typed_chars`) and how many more may have been (`unsure_chars`: the
    /// piece in flight once it went out, or only its key in flight when Cua
    /// counts the keys before it).
    pub async fn type_ascii(&self, pid: i64, window: u64, text: &str, cancel: Option<&Cancel>, focused: impl AsyncFn() -> Result<()>) -> Result<()> {
        debug_assert!(text.is_ascii(), "one byte a character");
        let (mut typed, mut resumes, mut pause) = (0usize, 0usize, PIECE_GAP);
        while typed < text.len() {
            let piece = &text[typed..text.len().min(typed + TYPE_PIECE)];
            let stopped = |error: IbaraError| {
                error.with("typed_chars", typed).with("unsure_chars", 0).with("no_input_held", true).with("execution_not_started", typed == 0)
            };
            // After the gap, not beside it: a focus change during the gap must stop the next piece.
            let focus = if typed > 0 {
                tokio::time::sleep(pause).await;
                focused().await
            } else {
                Ok(())
            };
            if cancel.is_some_and(Cancel::is_cancelled) {
                return Err(stopped(IbaraError::new("TIMEOUT", "Typing cancelled between pieces.", false)));
            }
            focus.map_err(stopped)?;
            let mut args = Self::target(pid, window);
            args["text"] = json!(piece);
            let Err(error) = self.call("type_text", args, true, ACT_TIMEOUT).await else {
                typed += piece.len();
                pause = PIECE_GAP;
                continue;
            };
            if let Some(delivered) = error.details.get("delivered").and_then(Value::as_u64) {
                typed += piece.len().min(delivered as usize);
                let reason = error.details.get("reason").and_then(Value::as_str);
                if reason == Some(INPUT_ENDED) && resumes < RESUMES {
                    resumes += 1;
                    pause = RESUME_AFTER;
                    continue;
                }
                let unsure = usize::from(reason == Some(KEY_IN_FLIGHT));
                return Err(error.with("typed_chars", typed).with("unsure_chars", unsure));
            }
            let sent = error.details.get("execution_not_started") != Some(&json!(true));
            let unsure = if sent { piece.len() } else { 0 };
            let error = error.with("typed_chars", typed).with("unsure_chars", unsure);
            return Err(if typed + unsure > 0 { error.with("execution_not_started", false) } else { error });
        }
        Ok(())
    }

    /// The window's only enabled editable text element, from a read that
    /// walks its tree for up to `walk`; none when it has none or several.
    async fn only_field(&self, pid: i64, window: u64, walk: Duration) -> Result<Option<Field>> {
        let snapshot = self.snapshot_within(pid, window, walk).await?;
        let editable: Vec<&Value> = snapshot["elements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| editable_role(e["role"].as_str().unwrap_or("")) && e["enabled"].as_bool() != Some(false))
            .collect();
        let [element] = editable.as_slice() else { return Ok(None) };
        let frame = &element["frame"];
        Ok(Some(Field {
            frame: match (frame["x"].as_f64(), frame["y"].as_f64(), frame["w"].as_f64(), frame["h"].as_f64()) {
                (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some([x, y, w, h]),
                _ => None,
            },
            whole: snapshot["elements_complete"].as_bool() != Some(false),
        }))
    }

    /// Where ASCII text typed into the window goes, before the first piece:
    /// its only editable text element, when a whole read shows exactly one.
    /// Keys go to the focus, and one editable element is the only place in
    /// the window that takes them as text. A failed read proves nothing.
    pub async fn typing_field(&self, pid: i64, window: u64) -> Option<Field> {
        self.only_field(pid, window, FIELD_LOOK).await.ok().flatten().filter(|f| f.whole)
    }

    /// The sole editable field for native Unicode paste, from a read of
    /// the whole window.
    pub async fn insert_field(&self, pid: i64, window: u64) -> Result<Option<Field>> {
        self.only_field(pid, window, WALK_BUDGET).await
    }

    /// Move only the agent's cursor, in the overlay's (screen) coordinates;
    /// the user's pointer is untouched. Not an effect.
    pub async fn move_cursor(&self, x: f64, y: f64) -> Result<()> {
        self.call_as("move_cursor", json!({"x": x, "y": y}), Kind::Cursor, ACT_TIMEOUT).await.map(|_| ())
    }

    /// Whether an agent holds control (its named cursor may be shown).
    pub fn has_agent(&self) -> bool {
        self.agent().is_some()
    }

    /// Whether the agent's named cursor is shown (it goes with a stopped worker).
    pub fn cursor_shown(&self) -> bool {
        self.inner.shown.load(Ordering::SeqCst)
    }

    /// Refused as Cua's own input is while the agent's cursor is not shown
    /// (the person has the screen): for input ibara sends another way.
    pub fn screen_held(&self) -> Result<()> {
        if !self.cursor_shown() && self.has_agent() {
            return Err(self.not_held());
        }
        Ok(())
    }

    /// Input in flight that moves the real pointer.
    pub fn motion(&self) -> Arc<Motion> {
        self.inner.motion.clone()
    }

    /// Where the named cursor was last sent, if anywhere.
    pub fn cursor_at(&self) -> Option<(f64, f64)> {
        *self.inner.cursor_at.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Draw the agent's named cursor exactly at `(x, y)`, where the person's
    /// pointer is when the screen is handed to the agent. Each time is a new
    /// Cua session under the agent's label: a hidden cursor drawn again
    /// shows at its old place for 50–80 ms first, a new session's first
    /// cursor, placed through [`SEED`], never does (probed on Cua 0.29.1).
    /// Glides land in one frame and turn little (the default swung a short
    /// glide 46 px past its point), and Cua never hides it for idleness:
    /// only the handover does ([`motion`]). Returns how long Cua takes to
    /// draw it.
    pub async fn show_cursor(&self, x: f64, y: f64) -> Result<Duration> {
        let label = self.agent().ok_or_else(|| unavailable("No agent holds control."))?;
        let mut slot = self.inner.worker.lock().await;
        let worker = self.worker(&mut slot).await?;
        let shown = tokio::time::timeout(READ_TIMEOUT, async {
            if let Some(old) = worker.session.take() {
                answered(call_tool(worker, "end_session", json!({"session": old})).await?)?;
            }
            answered(call_tool(worker, "start_session", json!({"session": label})).await?)?;
            worker.session = Some(label.clone());
            answered(call_tool(worker, "set_agent_cursor_motion", motion(Some(&label), 1.0)).await?)?;
            answered(call_tool(worker, "move_cursor", json!({"session": label, "x": x + SEED, "y": y + SEED})).await?)?;
            answered(call_tool(worker, "move_cursor", json!({"session": label, "x": x, "y": y})).await?)?;
            answered(call_tool(worker, "set_agent_cursor_enabled", json!({"session": label, "enabled": true})).await?)
        })
        .await;
        match shown {
            Ok(Ok(())) => {
                self.inner.yielded.store(false, Ordering::SeqCst);
                self.inner.shown.store(true, Ordering::SeqCst);
                *self.inner.cursor_at.lock().unwrap_or_else(|p| p.into_inner()) = Some((x, y));
                Ok(DRAWN)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                let mut hung = slot.take().expect("worker");
                close(&mut hung).await;
                Err(unavailable("Cua did not show the agent's cursor; it was stopped."))
            }
        }
    }

    /// Hide the named cursor now (Cua takes 30–70 ms): a read in flight (a
    /// snapshot takes up to 12 s) is stopped with its worker, which takes the
    /// cursor with it, and is taken again on a new worker. An input in
    /// flight is waited for: the plugin refuses a click as soon as a person
    /// moves the pointer, a typing piece is short, and control's end or a
    /// person's move stops what follows it. If Cua does not hide it, the
    /// worker is stopped. Starts no worker.
    pub async fn hide_cursor_now(&self) {
        self.inner.shown.store(false, Ordering::SeqCst);
        let mut slot = loop {
            self.stop_read();
            if let Ok(slot) = tokio::time::timeout(super::handover::POLL, self.inner.worker.lock()).await {
                break slot;
            }
        };
        self.hide_in(&mut slot).await;
    }

    /// Move the drawn named cursor to `(x, y)` in one frame, if no Cua call is
    /// in flight; whether it was sent. Cua paints the move's first frame at
    /// the old place, so it lands about 50 ms later.
    pub async fn cursor_to_now(&self, x: f64, y: f64) -> bool {
        let Ok(mut slot) = self.inner.worker.try_lock() else { return false };
        let Some(worker) = slot.as_mut() else { return false };
        let Some(label) = worker.session.clone().filter(|_| self.inner.shown.load(Ordering::SeqCst)) else { return false };
        let moved = tokio::time::timeout(ACT_TIMEOUT, async {
            answered(call_tool(worker, "set_agent_cursor_motion", motion(Some(&label), 1.0)).await?)?;
            answered(call_tool(worker, "move_cursor", json!({"session": label, "x": x, "y": y})).await?)
        })
        .await;
        if matches!(moved, Ok(Ok(()))) {
            *self.inner.cursor_at.lock().unwrap_or_else(|p| p.into_inner()) = Some((x, y));
            return true;
        }
        false
    }

    /// The named cursor should be drawn, but nothing draws it: its worker
    /// exited (a crash, or stopped). Never waits for a call in flight (one
    /// that meets a dead worker says so itself).
    pub fn cursor_lost(&self) -> bool {
        if !self.inner.shown.load(Ordering::SeqCst) {
            return false;
        }
        let Ok(mut slot) = self.inner.worker.try_lock() else { return false };
        let gone = slot.as_mut().is_none_or(|w| matches!(w.child.try_wait(), Ok(Some(_))));
        if gone {
            slot.take();
            self.inner.shown.store(false, Ordering::SeqCst);
        }
        gone
    }

    /// The person took the screen: no agent input or cursor move until the
    /// named cursor is shown again (input is refused as `person_active`).
    /// A typing piece in flight ends first: stopping its worker would crash
    /// the GTK 3 app it types into.
    pub fn yield_to_person(&self) {
        self.inner.yielded.store(true, Ordering::SeqCst);
        self.inner.shown.store(false, Ordering::SeqCst);
    }

    /// Stop a read in flight on a worker with the agent's session open: its
    /// named cursor goes with the worker at once.
    fn stop_read(&self) {
        let mut reading = self.reading();
        if let Reading::Worker { pid, session: true } = *reading {
            // The call in flight holds the worker, so it has not been waited
            // for and `pid` is still this child's.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            *reading = Reading::Stopped;
        }
    }

    async fn hide_in(&self, slot: &mut Option<Worker>) {
        self.inner.shown.store(false, Ordering::SeqCst);
        let Some(worker) = slot.as_mut() else { return };
        let Some(label) = worker.session.clone() else { return };
        let hidden = tokio::time::timeout(READ_TIMEOUT, call_tool(worker, "set_agent_cursor_enabled", json!({"session": label, "enabled": false}))).await;
        let hidden = match hidden {
            Ok(Ok(answer)) => answered(answer).is_ok(),
            _ => false,
        };
        if !hidden {
            crate::controller::log_event("agent_cursor_hide_failed", "the Cua worker was stopped instead");
            let mut worker = slot.take().expect("checked");
            close(&mut worker).await;
        }
    }

    /// Set the named cursor's next glide to a person's pointing time for the
    /// way to `(x, y)` and remember the point. For a point click nothing more
    /// is needed: Cua sends the cursor there as the click starts, and ibara's
    /// Hyprland plugin moves the real pointer over the same time before it
    /// presses. From one screen to another the cursor lands in one frame
    /// instead: the straight way between them may cross what no screen
    /// shows. Returns the glide, or `None` when the agent's cursor is not
    /// shown.
    pub async fn pace(&self, x: f64, y: f64, screens: &Screens) -> Option<Duration> {
        if !self.inner.shown.load(Ordering::SeqCst) {
            return None;
        }
        let from = self.inner.cursor_at.lock().unwrap_or_else(|p| p.into_inner()).replace((x, y));
        let glide = match from {
            Some(from) if !screens.one_shows(from, (x, y)) => None,
            _ => Some(human_glide(from.map(|(fx, fy)| ((x - fx).powi(2) + (y - fy).powi(2)).sqrt()))),
        };
        let ms = glide.map_or(1.0, |g| g.as_millis() as f64);
        let paced = self.call_as("set_agent_cursor_motion", motion(None, ms), Kind::Cursor, ACT_TIMEOUT).await;
        Some(match glide {
            Some(glide) if from.is_some() && paced.is_ok() => glide,
            _ => Duration::ZERO,
        })
    }

    /// Bring the named cursor to a screen point at a person's pace and wait
    /// until it has arrived and rested, for actions whose input does not move
    /// the real pointer there first (accessibility actions, a drag's start).
    /// A point no screen shows (an element scrolled out of sight) is led to
    /// at the nearest point one does. A failed move does not stop the
    /// action; it only costs the visual. The wait (up to 1.54 s) holds no
    /// worker, so a handoff meanwhile settles at once: check
    /// [`unless_cancelled`] before sending the input.
    pub async fn lead(&self, x: f64, y: f64, screens: &Screens) {
        let (x, y) = screens.nearest(x, y);
        let Some(travel) = self.pace(x, y, screens).await else { return };
        match self.move_cursor(x, y).await {
            Ok(()) => tokio::time::sleep(travel + HOVER).await,
            Err(e) => crate::controller::log_event("cua_cursor_lead_failed", &e.message),
        }
    }

    /// Before typing: bring the named cursor to the middle of the field the
    /// text goes to, at a person's pace, as [`Cua::lead`] does, unless it is
    /// in the field already. Only a box Cua gives wholly inside `window` on
    /// the screen proves where the field is: Cua 0.29.1 on Hyprland gives a
    /// GTK 3 window's text boxes in the window's own coordinates (Mousepad's
    /// text at 1,30 in a window at 12,38), and those are not followed. Moves
    /// only the named cursor: the real pointer, the keyboard focus and the
    /// windows' order stay as they are.
    pub async fn to_field(&self, field: &Field, window: Rect, screens: &Screens) {
        let Some([x, y, w, h]) = field.frame else { return };
        let (left, top) = (window.x as f64, window.y as f64);
        let inside = x >= left && y >= top && x + w <= left + window.width as f64 && y + h <= top + window.height as f64;
        let there = self.cursor_at().is_some_and(|(cx, cy)| cx >= x && cy >= y && cx <= x + w && cy <= y + h);
        if inside && !there {
            self.lead(x + w / 2.0, y + h / 2.0, screens).await;
        }
    }

    /// Whether the window exposes a password field (replay keeps no picture).
    pub async fn has_password_field(&self, pid: i64, window: u64) -> Result<bool> {
        let snapshot = self.snapshot(pid, window).await?;
        Ok(snapshot["elements"].as_array().into_iter().flatten().any(|e| e["role"].as_str() == Some("password text")))
    }
}

fn editable_role(role: &str) -> bool {
    matches!(role, "text" | "entry" | "password text" | "editbar" | "terminal")
}

fn stale(message: &str) -> IbaraError {
    IbaraError::new("STALE_TARGET", message, true).with("execution_not_started", true)
}

/// An element's box as Cua gives it (x, y, width, height), when it has one.
fn frame_of(element: &Value) -> Option<[f64; 4]> {
    let frame = &element["frame"];
    match (frame["x"].as_f64(), frame["y"].as_f64(), frame["w"].as_f64(), frame["h"].as_f64()) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some([x, y, w, h]),
        _ => None,
    }
}

/// The center of `frame` on the screen. Cua gives an element's box as its
/// toolkit reports it: on the screen for most apps, in the window's own
/// coordinates for some (Cua 0.29.1 on Hyprland gave Mousepad's text at
/// 1,30 in a window at 12,38). When the read has the window's top element,
/// `top` (the window itself), a box is placed as far from the window's
/// corner on the screen as it is from `top`'s. Without one (Mousepad's read
/// has none), a box that fits the window only in the window's own
/// coordinates is taken as given in them.
fn screen_center(frame: [f64; 4], top: Option<[f64; 4]>, window: Rect) -> (f64, f64) {
    let [x, y, w, h] = frame;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (wx, wy, width, height) = (window.x as f64, window.y as f64, window.width as f64, window.height as f64);
    let fits = |x: f64, y: f64| x >= 0.0 && y >= 0.0 && x + w <= width && y + h <= height;
    match top {
        Some([tx, ty, _, _]) => (wx + cx - tx, wy + cy - ty),
        None if !fits(x - wx, y - wy) && fits(x, y) => (wx + cx, wy + cy),
        None => (cx, cy),
    }
}

/// Cua refused, before sending anything, to act on an element as
/// background input: it offers that only to the apps it has qualified
/// (`client_not_qualified`), or not on this desktop at all
/// (`background_unavailable`).
fn background_refused(error: &IbaraError) -> bool {
    error.details.get("execution_not_started") == Some(&json!(true))
        && matches!(error.details.get("reason").and_then(Value::as_str), Some("client_not_qualified" | "background_unavailable"))
}

/// The named cursor's motion, every field ibara relies on, with a glide of
/// `glide_ms` (1: it lands in one frame): no idle hide, turns of 1 px. Cua
/// 0.29.1 on Wayland fills each field a call leaves out with its own
/// default, not the session's current value (it reads them from its X11
/// overlay, which Wayland never feeds): a glide set alone brought back turns
/// of 80 px, which swung the cursor past the screen's edge on its way to a
/// point, and a hide after 20 s idle.
fn motion(session: Option<&str>, glide_ms: f64) -> Value {
    let mut motion = json!({"glide_duration_ms": glide_ms, "idle_hide_ms": 0.0, "turn_radius": 1.0});
    if let Some(session) = session {
        motion["session"] = json!(session);
    }
    motion
}

/// Stop before sending input once `cancel` has fired (pause, take control,
/// lease expiry): the handoff has already been told nothing is running.
pub fn unless_cancelled(cancel: Option<&Cancel>) -> Result<()> {
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(IbaraError::new("TIMEOUT", "Cancelled before the input was sent.", true).with("execution_not_started", true));
    }
    Ok(())
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn states_of(element: &Value) -> Vec<String> {
    let mut states = Vec::new();
    if element["enabled"].as_bool() != Some(false) {
        states.push("enabled".to_string());
    }
    if element["selected"].as_bool() == Some(true) {
        states.push("selected".to_string());
    }
    if editable_role(element["role"].as_str().unwrap_or("")) {
        states.push("editable".to_string());
    }
    states
}

/// Each element's identity (its role and label, and its ancestors' roles and
/// labels, plus its rank among identical siblings) and the element itself.
fn identities(snapshot: &Value) -> Vec<(Value, Value)> {
    let elements = snapshot["elements"].as_array().cloned().unwrap_or_default();
    let by_index: std::collections::HashMap<u64, &Value> =
        elements.iter().filter_map(|e| e["element_index"].as_u64().map(|i| (i, e))).collect();
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    elements
        .iter()
        .map(|e| {
            let mut path = Vec::new();
            let mut parent = e["parent_index"].as_u64();
            let mut guard = 0;
            while let Some(p) = parent.and_then(|i| by_index.get(&i)) {
                path.push(json!([p["role"], p["label"]]));
                parent = p["parent_index"].as_u64();
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
            path.reverse();
            let key = json!([path, e["role"], e["label"]]).to_string();
            let ordinal = seen.entry(key).or_insert(0);
            *ordinal += 1;
            (json!({"path": path, "role": e["role"], "label": e["label"], "ordinal": *ordinal}), e.clone())
        })
        .collect()
}

/// An element's name and value as ibara shows them: the label without the
/// spaces GTK 3 ends a menu item's in, and a text view's contents (its
/// label) as its value.
fn name_and_value(e: &Value) -> (String, String) {
    let label = e["label"].as_str().unwrap_or("").trim_end().to_string();
    match e["value"].as_str() {
        Some(v) => (label, v.to_string()),
        None if editable_role(e["role"].as_str().unwrap_or("")) => (String::new(), label),
        None => (label, String::new()),
    }
}

/// Roles of the entries of a menu.
fn menu_entry(e: &Value) -> bool {
    matches!(e["role"].as_str(), Some("menu item" | "check menu item" | "radio menu item" | "menu"))
}

/// Whether Cua reports bounds for the element: GTK 3 lists every menu's
/// items, and gives a closed menu's none.
fn on_screen(e: &Value) -> bool {
    let frame = &e["frame"];
    frame["w"].as_f64().is_some_and(|w| w > 0.0) && frame["h"].as_f64().is_some_and(|h| h > 0.0)
}

/// How many menus are open in the window: menus whose entries are on the
/// screen.
fn open_menus(snapshot: &Value) -> usize {
    let elements = snapshot["elements"].as_array().map(Vec::as_slice).unwrap_or_default();
    let menus: std::collections::HashSet<u64> =
        elements.iter().filter(|e| e["role"] == "menu").filter_map(|e| e["element_index"].as_u64()).collect();
    elements
        .iter()
        .filter(|e| menu_entry(e) && on_screen(e))
        .filter_map(|e| e["parent_index"].as_u64().filter(|p| menus.contains(p)))
        .collect::<std::collections::HashSet<u64>>()
        .len()
}

/// One move towards choosing a menu item as a keyboard user does. Not
/// through accessibility's own action: GTK 3 runs the item inside that
/// call, so an item that opens a dialog (Save As) leaves the app's
/// accessibility unanswered until the dialog closes.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuMove {
    /// Open the item's top-level menu through accessibility (by its token).
    Open(String),
    /// Presses of a key into the open menu: `Down` and `Up` walk the
    /// highlight, `Right` opens the highlighted submenu, `Escape` closes a
    /// menu that is in the way.
    Keys(&'static str, usize),
    /// The item is highlighted: `Return` chooses it. Its centre on the
    /// screen, for the agent's cursor.
    Choose(Option<(f64, f64)>),
}

/// The next [`MenuMove`] for the menu item with `identity` in `snapshot`,
/// which must still be the item the frame showed.
fn menu_move(snapshot: &Value, identity: &Value, role: &str, name: &str) -> Result<MenuMove> {
    let rows = identities(snapshot);
    let found: Vec<&Value> = rows.iter().filter(|(id, _)| id == identity).map(|(_, e)| e).collect();
    let [item] = found.as_slice() else { return Err(stale("Semantic element no longer resolves uniquely.")) };
    if item["role"].as_str() != Some(role) || name_and_value(item).0 != name {
        return Err(stale("Semantic identity changed."));
    }
    if item["enabled"].as_bool() == Some(false) {
        return Err(stale("The menu item is disabled now; observe again."));
    }
    let elements = snapshot["elements"].as_array().map(Vec::as_slice).unwrap_or_default();
    let by_index: std::collections::HashMap<u64, &Value> =
        elements.iter().filter_map(|e| e["element_index"].as_u64().map(|i| (i, e))).collect();
    // The menus the item is in, outermost first.
    let mut chain: Vec<&Value> = Vec::new();
    let mut at: &Value = item;
    while let Some(menu) = at["parent_index"].as_u64().and_then(|i| by_index.get(&i).copied()).filter(|p| p["role"] == "menu") {
        chain.push(menu);
        at = menu;
        if chain.len() > 16 {
            break;
        }
    }
    chain.reverse();
    // A menu's entries a keyboard can highlight, top to bottom.
    let entries = |menu: &Value| {
        let mut shown: Vec<&Value> = elements
            .iter()
            .filter(|e| e["parent_index"] == menu["element_index"] && menu_entry(e) && on_screen(e) && e["enabled"].as_bool() != Some(false))
            .collect();
        shown.sort_by(|a, b| a["frame"]["y"].as_f64().unwrap_or(0.0).total_cmp(&b["frame"]["y"].as_f64().unwrap_or(0.0)));
        shown
    };
    let Some(level) = chain.iter().rposition(|menu| !entries(menu).is_empty()) else {
        if open_menus(snapshot) > 0 {
            return Ok(MenuMove::Keys("Escape", 1));
        }
        let top = chain.first().ok_or_else(|| stale("The element is not in a menu."))?;
        let token = top["element_token"].as_str().ok_or_else(|| stale("Element has no token."))?;
        return Ok(MenuMove::Open(token.to_string()));
    };
    // Highlight the next step: the submenu on the way, or the item itself.
    let step = chain.get(level + 1).copied().unwrap_or(*item);
    let shown = entries(chain[level]);
    let want = shown
        .iter()
        .position(|e| e["element_index"] == step["element_index"])
        .ok_or_else(|| stale("The menu item is not shown in its open menu; observe again."))?;
    Ok(match shown.iter().position(|e| e["selected"] == true) {
        Some(have) if have == want && level + 1 < chain.len() => MenuMove::Keys("Right", 1),
        Some(have) if have == want => {
            let f = &step["frame"];
            let centre = |at: &str, size: &str| f[at].as_f64().zip(f[size].as_f64()).map(|(a, s)| a + s / 2.0);
            MenuMove::Choose(centre("x", "w").zip(centre("y", "h")))
        }
        Some(have) if have < want => MenuMove::Keys("Down", want - have),
        Some(have) => MenuMove::Keys("Up", have - want),
        None => MenuMove::Keys("Down", want + 1),
    })
}

/// Cua's snapshot as the element tree the controller reads. The ancestor path
/// uses the helper's `role:name/…` form, so context and ranking are unchanged.
fn tree_from_snapshot(pid: i64, window: u64, snapshot: &Value) -> Tree {
    let Some(list) = snapshot["elements"].as_array() else {
        return Tree::default();
    };
    let rows = identities(snapshot);
    let by_index: std::collections::HashMap<u64, &Value> =
        list.iter().filter_map(|e| e["element_index"].as_u64().map(|i| (i, e))).collect();
    let mut text = String::new();
    let elements: Vec<RawElement> = rows
        .into_iter()
        .map(|(identity, e)| {
            let mut ancestors = Vec::new();
            let mut parent = e["parent_index"].as_u64();
            let mut guard = 0;
            while let Some(p) = parent.and_then(|i| by_index.get(&i)) {
                ancestors.push(format!("{}:{}", p["role"].as_str().unwrap_or(""), p["label"].as_str().unwrap_or("").trim()));
                parent = p["parent_index"].as_u64();
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
            ancestors.reverse();
            let role = e["role"].as_str().unwrap_or("").to_string();
            let (name, value) = name_and_value(&e);
            if text.len() < 12_000 && !value.is_empty() {
                text.push_str(clip(&value, 12_000 - text.len()));
                text.push('\n');
            }
            RawElement {
                selector: json!({"pid": pid, "window_id": window, "identity": identity}),
                role,
                name,
                states: states_of(&e),
                ancestor: ancestors.join("/"),
                actions: strings(&e["actions"]),
                text: value,
            }
        })
        .collect();
    let total = elements.len() as u64;
    // An app without accessibility (a terminal such as foot) comes back as
    // one unnamed element, the window itself: that is no tree.
    let only_the_window = matches!(elements.as_slice(), [only] if only.ancestor.is_empty() && only.name.is_empty() && only.text.is_empty());
    Tree {
        available: !elements.is_empty() && !only_the_window,
        truncated: snapshot["elements_complete"].as_bool() == Some(false),
        returned_count: total,
        available_count: Some(total),
        elements,
        text,
    }
}

/// Query filter, ranking and paging over a whole snapshot.
pub(super) fn page(tree: Tree, query: Option<&str>, limit: u32, cursor: Option<u32>) -> ElementPage {
    if !tree.available {
        return ElementPage::default();
    }
    let query = query.map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
    let text = tree.text;
    let truncated = tree.truncated;
    let mut elements = super::atspi::compact(tree.elements, 0);
    if let Some(q) = &query {
        elements.retain(|e| {
            e.name.to_lowercase().contains(q.as_str())
                || e.value.as_deref().is_some_and(|v| v.to_lowercase().contains(q.as_str()))
                || e.role.to_lowercase().contains(q.as_str())
                || e.context.iter().chain(&e.parent).any(|c| c.to_lowercase().contains(q.as_str()))
        });
    }
    super::atspi::rank(&mut elements, query.as_deref());
    let total = elements.len();
    let offset = (cursor.unwrap_or(0) as usize).min(total);
    let limit = limit.clamp(1, super::atspi::MAX_LIMIT) as usize;
    let end = (offset + limit).min(total);
    let next = (end < total).then_some(end as u32);
    ElementPage {
        available: true,
        elements: elements.drain(offset..end).collect(),
        truncated: truncated || next.is_some(),
        total: Some(total as u64),
        next_cursor: next,
        text: clip(&text, 12_000).to_string(),
    }
}

/// Cua's outcome of a call, or its refusal as an ibara error.
fn answer(tool: &str, outcome: Value, mutates: bool) -> Result<Value> {
    if outcome.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(refusal(tool, &outcome, mutates));
    }
    Ok(outcome)
}

/// What stopped a refused input. For every foreground refusal before
/// dispatch Cua reports `reason: primary_target_busy` and puts the plugin's
/// cause in `detail` (`foreground_keyboard_locked`), so a `foreground_…`
/// detail decides, without its prefix. Other refusals name their cause in
/// `reason` (`unsupported_layout`), or only in `code` ([`NO_PICTURE`]).
fn cause(sc: &Value) -> &str {
    if sc["code"].as_str() == Some(NO_PICTURE) {
        return NO_PICTURE;
    }
    let reason = sc["reason"].as_str().unwrap_or("");
    let detail = sc["detail"].as_str().unwrap_or("");
    match detail.strip_prefix("foreground_") {
        Some(cause) if !matches!(cause, "" | "none" | "unknown" | "partial_unknown") => cause,
        _ if !reason.is_empty() => reason,
        _ => detail,
    }
}

/// A Cua refusal or failure as an ibara error. A route Cua refused before
/// dispatch is `execution_not_started`; anything else after an effect call
/// may have acted, except that Cua sends text key by key and says how many
/// keys went out when it stops part way ([`partial`]).
fn refusal(tool: &str, outcome: &Value, mutates: bool) -> IbaraError {
    let sc = &outcome["structuredContent"];
    let text: String = outcome["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let refused = sc["effect"].as_str() == Some("refused")
        || matches!(sc["code"].as_str(), Some("tool_invocation_failed" | NO_PICTURE))
        || text.contains("no evdev mapping");
    let detail = clip(&text, 300).to_string();
    if let Some(error) = partial(tool, sc, &text) {
        return error.with("detail", detail);
    }
    if !refused && mutates {
        return IbaraError::new("OUTCOME_UNKNOWN", format!("Cua {tool} failed after dispatch."), false)
            .with("detail", detail)
            .requires_reconciliation();
    }
    refused_by(tool, sc, &text).with("detail", detail).with("execution_not_started", true)
}

/// Cua's text stopped part way (`effect: partial`), with `delivered` the
/// keys that went out before it stopped. [`INPUT_ENDED`]: Cua stopped
/// before sending the next key, so exactly those arrived; Hyprland's plugin
/// ends every input connection when a display is added or removed.
/// [`KEY_IN_FLIGHT`]: the connection failed with the next key sent, which
/// may have arrived too. Otherwise the plugin refused the next key, for a
/// cause as before any input. Other input (a drag) keeps "failed after
/// dispatch".
fn partial(tool: &str, sc: &Value, text: &str) -> Option<IbaraError> {
    if tool != "type_text" || sc["effect"].as_str() != Some("partial") {
        return None;
    }
    let delivered = sc["delivery"]["delivered_count"].as_u64()?;
    let error = match sc["reason"].as_str() {
        Some(INPUT_ENDED) => unavailable("Cua's connection to the desktop ended part way, as it does when a display is added or removed.")
            .with("reason", INPUT_ENDED),
        Some(KEY_IN_FLIGHT) => IbaraError::new("OUTCOME_UNKNOWN", format!("Cua {tool} failed after dispatch."), false)
            .requires_reconciliation()
            .with("reason", KEY_IN_FLIGHT),
        _ => refused_by(tool, sc, text),
    };
    Some(error.with("delivered", delivered).with("execution_not_started", false))
}

/// What an agent can use when Cua refuses pointer input.
const KEYS_INSTEAD: &str = "Use keys instead (kind key, in computer_act or browser_act): Tab and shift+Tab move between controls, Return or space presses the focused one, Page_Down and Page_Up scroll, and text typed without a target goes to the focused field. In the browser, browser_act navigate opens an address directly.";
/// What an agent can use when Cua refuses keyboard input.
const POINTER_INSTEAD: &str = "Use the pointer instead: click the element that does it (a button, a menu item), and write a file's text with computer_files instead of typing it.";
/// What an agent can use while something outside the window holds the
/// pointer and keyboard: an accessibility action needs neither.
const GRAB_INSTEAD: &str = "Left-click an element by its id from computer_observe instead (an element with an accessibility action needs neither the pointer nor the keyboard), or close what holds them in its own app, or wait a moment for a drag to end or a person to close it; then observe again and try again.";

/// A refusal by its cause ([`cause`]), with that cause as `reason` and, for a
/// route that stays closed for now, what the agent can use instead as `next`.
fn refused_by(tool: &str, sc: &Value, text: &str) -> IbaraError {
    let cause = cause(sc);
    let instead = match tool {
        "press_key" | "hotkey" | "type_text" => POINTER_INSTEAD,
        "click" | "double_click" | "right_click" | "drag" | "scroll" => KEYS_INSTEAD,
        _ => "Observe again with computer_observe, then act on an element by its id, or use keys.",
    };
    let (error, next) = match cause {
        "unsupported_layout" | "keyboard_group" => (
            unavailable("Cua refused: this computer's current keyboard layout is not a plain US layout."),
            Some(format!("{POINTER_INSTEAD} Typing works again once a person switches this computer's keyboard to the English (US) layout.")),
        ),
        // The plugin lets keys and points reach the target's own popup (a
        // popover, a menu), so a grab here is another app's popup, a panel or
        // a drag, which refuses every key and point to this window alike.
        "grab" | "dnd" => (
            IbaraError::new(
                "BLOCKED_BY_DIALOG",
                "Another app's menu or popup, a panel such as a launcher, or a drag holds this computer's pointer and keyboard, so Cua sent nothing: keys and points to this window are refused until it lets go.",
                true,
            ),
            Some(GRAB_INSTEAD.to_string()),
        ),
        "keyboard_locked" => (
            unavailable("Cua refused: Caps Lock (or another lock key; NumLock is fine) is on at this computer's keyboard. A person needs to turn it off."),
            Some(format!("{POINTER_INSTEAD} Typing works again once a person turns Caps Lock off.")),
        ),
        "keyboard_depressed" | "physical_keys" => (
            unavailable("Cua refused: a key is held down on this computer's keyboard."),
            Some("Wait a moment for the key to be released (a person may be typing), observe, then try again.".into()),
        ),
        "keyboard_latched" => (
            unavailable("Cua refused: a modifier key is latched (sticky keys) on this computer's keyboard."),
            Some(format!("{POINTER_INSTEAD} Typing works again once a person releases the latched key.")),
        ),
        "physical_buttons" => (
            unavailable("Cua refused: a mouse button is held down on this computer."),
            Some("Wait a moment for the button to be released, observe, then try again.".into()),
        ),
        "interrupted" => (
            unavailable("A person used the mouse or keyboard, so Cua stopped before sending the input."),
            Some("Wait until the person stops, observe again, then try again.".into()),
        ),
        "constraint" => (
            unavailable("Cua refused: the pointer constraint does not belong to this exact focused target, changed during input, or does not support this action."),
            Some("Observe the focused window again. Keys and clicks work only with that window's own pointer lock; confinement and locked dragging are unsupported. If the owner cannot be safely identified, finish partial and ask the person to release the constraint. Do not retry through another input route.".into()),
        ),
        "primary_binding" => (
            unavailable("Cua could not verify this window's primary input resources, or they changed during the action. This does not establish that another person or task owns the computer."),
            Some("Observe again before retrying. If the same binding refusal persists, finish partial and report it for input diagnostics; switching between keys and clicks cannot repair it. A task-owned app opened before the input plugin loaded may need to be closed and reopened after preserving its work.".into()),
        ),
        "physical_pointer" => (
            unavailable("Cua refused: this computer has no mouse, and its desktop session started before ibara's own pointer was installed."),
            Some(format!("{KEYS_INSTEAD} The pointer works after this computer's next sign-in or restart.")),
        ),
        "pointer_target" => (
            unavailable("That point is on a popup, video or overlay inside the window, not the window itself. Click the element instead, or another point."),
            Some("Click the element instead (its id from computer_observe), or another point in the window.".into()),
        ),
        NO_PICTURE => (
            unavailable(
                "Cua would not use a point in this window: it needs a current picture of the window and could not take one. Click the element instead, or try again.",
            ),
            Some("Click the element instead (its id from computer_observe), or try the point again.".into()),
        ),
        "exact_root" | "keyboard_focus" | "pointer_focus" => {
            (IbaraError::new("STALE_TARGET", "The window moved, resized or lost focus before Cua could act.", true), None)
        }
        _ if text.contains("no evdev mapping") => (
            invalid("Cua cannot send that key on Hyprland (supported: letters, digits, punctuation, Return, Tab, Escape, BackSpace, Delete, arrows, Home, End, Page_Up, Page_Down, F1-F12).")
                .with("field", "keys"),
            None,
        ),
        _ => (unavailable(format!("Cua refused {tool}.")), Some(instead.to_string())),
    };
    let error = error.with("reason", cause);
    match next {
        Some(next) => error.with("next", next),
        None => error,
    }
}

async fn write_line(worker: &mut Worker, message: &Value) -> Result<()> {
    let mut line = message.to_string();
    line.push('\n');
    worker
        .stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| unavailable(format!("Cua pipe: {e}")))?;
    worker.stdin.flush().await.map_err(|e| unavailable(format!("Cua pipe: {e}")))
}

async fn notify(worker: &mut Worker, method: &str) -> Result<()> {
    write_line(worker, &json!({"jsonrpc": "2.0", "method": method})).await
}

/// Cua's JSON-RPC error for a request: Cua rejected it and answered, so the
/// worker is still in step.
struct Rejection {
    code: i64,
    message: String,
}

impl Rejection {
    /// JSON-RPC's own codes, and Cua's unsupported protocol version and
    /// in-flight limit, reject a request before any tool runs (Cua 0.28.2
    /// `mcp_wire.rs`, `mcp_envelope.rs`). Another code may follow a tool that
    /// ran, for example a daemon transport error.
    fn before_any_tool(&self) -> bool {
        matches!(self.code, -32700 | -32600 | -32601 | -32602 | -32022 | -32029)
    }

    fn error(&self, tool: &str, mutates: bool) -> IbaraError {
        if mutates && !self.before_any_tool() {
            return IbaraError::new("OUTCOME_UNKNOWN", format!("Cua answered {tool} with an error: {}", self.message), false)
                .with("rpc_code", self.code)
                .requires_reconciliation();
        }
        unavailable(format!("Cua rejected {tool}: {}", self.message)).with("rpc_code", self.code)
    }
}

/// Cua's result for a request, or its JSON-RPC error.
type Answer = std::result::Result<Value, Rejection>;

/// One request. `Err` when the pipe failed (the worker is unusable);
/// `Ok(Err(_))` when Cua answered with a JSON-RPC error.
async fn request(worker: &mut Worker, method: &str, params: Value) -> Result<Answer> {
    worker.next_id += 1;
    let id = worker.next_id;
    write_line(worker, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
    let mut line = String::new();
    loop {
        line.clear();
        let read = (&mut worker.stdout)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .await
            .map_err(|e| unavailable(format!("Cua pipe: {e}")))?;
        if read == 0 {
            return Err(unavailable("Cua closed its pipe."));
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            if read >= MAX_LINE {
                return Err(unavailable("Cua reply too large."));
            }
            continue;
        };
        if message["id"].as_u64() != Some(id) {
            // Notifications and log lines; ibara reads events from Hyprland.
            continue;
        }
        if let Some(error) = message.get("error") {
            let text = error["message"].as_str().map(str::to_string).unwrap_or_else(|| error["message"].to_string());
            return Ok(Err(Rejection { code: error["code"].as_i64().unwrap_or(0), message: clip(&text, 200).to_string() }));
        }
        return Ok(Ok(message["result"].clone()));
    }
}

async fn call_tool(worker: &mut Worker, tool: &str, args: Value) -> Result<Answer> {
    request(worker, "tools/call", json!({"name": tool, "arguments": args})).await
}

/// A cursor call's answer: whether Cua did it.
fn answered(answer: Answer) -> Result<()> {
    match answer {
        Ok(result) if result.get("isError").and_then(Value::as_bool) != Some(true) => Ok(()),
        Ok(result) => Err(unavailable(format!("Cua refused a cursor call: {}", clip(&result["content"].to_string(), 200)))),
        Err(rejection) => Err(unavailable(format!("Cua rejected a cursor call: {}", rejection.message))),
    }
}

async fn close(worker: &mut Worker) {
    if let Some(label) = worker.session.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), call_tool(worker, "end_session", json!({"session": label}))).await;
    }
    let _ = worker.stdin.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(3), worker.child.wait()).await.is_err() {
        let _ = worker.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-cua-test"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in `cua-driver`. `telemetry status --json` answers as Cua
    /// 0.28.2 does: the environment decides when `honours_env`, else the
    /// default (on). `mcp` logs each start to `starts.log`, and an update
    /// check to `updates.log` unless `CUA_DRIVER_RS_UPDATE_CHECK` is false,
    /// as Cua's `mcp` asks GitHub at start. Each tool called goes to
    /// `calls.log`, answered with the JSON-RPC member (`"result":…` or
    /// `"error":…`) that `replies` gives for its name (shell `case` arms
    /// setting `reply`), else an empty success.
    fn fake_cua(dir: &Path, honours_env: bool, replies: &str) -> PathBuf {
        let script = dir.join("cua-driver");
        let honours = if honours_env { "yes" } else { "no" };
        super::super::write_script(
            &script,
            &format!(
                r#"#!/bin/sh
dir='{dir}'
case "$1" in
telemetry)
  if [ {honours} = yes ] && [ "$CUA_DRIVER_RS_TELEMETRY_ENABLED" = false ]; then
    echo '{{"enabled":false,"source":"environment"}}'
  else
    echo '{{"enabled":true,"source":"default"}}'
  fi;;
mcp)
  echo start >>"$dir/starts.log"
  [ "$CUA_DRIVER_RS_UPDATE_CHECK" = false ] || echo github >>"$dir/updates.log"
  while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    [ -n "$id" ] || continue
    tool=$(printf '%s' "$line" | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p')
    [ -n "$tool" ] && echo "$tool" >>"$dir/calls.log"
    reply='"result":{{"content":[],"isError":false}}'
    case "$tool" in
{replies}
    esac
    printf '{{"jsonrpc":"2.0","id":%s,%s}}\n' "$id" "$reply"
  done;;
esac
"#,
                dir = dir.display()
            ),
        );
        script
    }

    fn cua(dir: &Path, program: PathBuf) -> Cua {
        Cua::new(CuaConfig { program, home: dir.join("home"), env: Arc::from(Vec::new()) })
    }

    fn lines(path: PathBuf) -> Vec<String> {
        std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect()
    }

    #[tokio::test]
    async fn a_worker_starts_only_with_telemetry_off_by_the_environment_and_no_update_check() {
        let dir = temp_dir();
        let ignored = cua(&dir, fake_cua(&dir, false, ""));
        let refused = ignored.tree(1, 1).await.unwrap_err();
        assert_eq!(refused.code, "CAPABILITY_UNAVAILABLE");
        assert!(lines(dir.join("starts.log")).is_empty(), "no worker may start with telemetry on");

        let honoured = cua(&dir, fake_cua(&dir, true, ""));
        honoured.tree(1, 1).await.unwrap();
        assert_eq!(lines(dir.join("starts.log")).len(), 1);
        assert!(lines(dir.join("updates.log")).is_empty(), "the worker must not ask GitHub for updates");
        honoured.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Cua's reply to a refused foreground click, as step 1 recorded it.
    fn refused(reason: &str, detail: &str) -> Value {
        json!({
            "isError": true,
            "content": [{"type": "text", "text": format!("foreground_unavailable ({reason}): {detail}")}],
            "structuredContent": {"code": "foreground_unavailable", "reason": reason, "detail": detail, "effect": "refused", "ok": false},
        })
    }

    #[test]
    fn foreground_refusals_are_reported_by_their_cause_not_cuas_shared_reason() {
        let caps_lock = refusal("press_key", &refused("primary_target_busy", "foreground_keyboard_locked"), true);
        assert_eq!((caps_lock.code, &caps_lock.details["reason"]), ("CAPABILITY_UNAVAILABLE", &json!("keyboard_locked")));
        assert!(caps_lock.message.contains("Caps Lock"), "{}", caps_lock.message);
        assert_eq!(caps_lock.details["execution_not_started"], json!(true));

        let menu = refusal("click", &refused("primary_target_busy", "foreground_grab"), true);
        assert_eq!(menu.code, "BLOCKED_BY_DIALOG");

        // Another app's grab refuses every key and point to the window, so
        // what the agent is told to do next is neither: no Escape, no keys.
        for tool in ["click", "drag", "scroll", "press_key", "hotkey", "type_text"] {
            for detail in ["foreground_grab", "foreground_dnd"] {
                let held = refusal(tool, &refused("primary_target_busy", detail), true).to_json();
                let (message, next) = (held["message"].as_str().unwrap(), held["next"].as_str().unwrap());
                assert!(next.contains("element by its id"), "{tool} {detail}: {next}");
                for refused_way in ["Escape", "kind key", "Use keys", "Use the pointer"] {
                    assert!(!message.contains(refused_way) && !next.contains(refused_way), "{tool} {detail} advises {refused_way}: {message} / {next}");
                }
            }
        }

        for detail in ["foreground_physical_keys", "foreground_physical_buttons", "foreground_constraint", "foreground_pointer_target"] {
            let error = refusal("click", &refused("primary_target_busy", detail), true);
            assert_eq!(error.code, "CAPABILITY_UNAVAILABLE", "{detail} is not a dialog");
        }
        let moved = refusal("click", &refused("primary_target_busy", "foreground_keyboard_focus"), true);
        assert_eq!(moved.code, "STALE_TARGET");

        let layout = refusal("type_text", &refused("unsupported_layout", "unsupported_layout"), true);
        assert_eq!((layout.code, &layout.details["reason"]), ("CAPABILITY_UNAVAILABLE", &json!("unsupported_layout")));
    }

    /// Every cause the plugin reports before any input (its
    /// `ForegroundFailureReason`s) and Cua's own refusals, for pointer and
    /// keyboard tools: the agent is told what it can do instead, not the
    /// registry's general words, which name no way.
    #[test]
    fn a_refused_route_names_the_ways_that_remain() {
        let details = [
            "lease", "client_dead", "exact_root", "primary_binding", "peer_conflict", "physical_keys", "physical_buttons", "grab", "dnd",
            "constraint", "keyboard_focus", "pointer_focus", "session_unavailable", "unsupported_layout", "lease_expired",
            "physical_keyboard", "keyboard_state", "physical_pointer", "pointer_target", "seat_resource", "pointer_resources",
            "keyboard_resources", "keyboard_depressed", "keyboard_latched", "keyboard_locked", "keyboard_group", "interrupted",
        ];
        let mut outcomes: Vec<Value> = details.iter().map(|d| refused("primary_target_busy", &format!("foreground_{d}"))).collect();
        outcomes.extend(["primary_target_busy", "agent_target_busy", "unsupported_layout"].map(|r| refused(r, r)));
        outcomes.push(json!({"isError": true, "content": [], "structuredContent": {"code": NO_PICTURE, "effect": "refused"}}));
        for tool in ["click", "double_click", "right_click", "drag", "scroll", "press_key", "hotkey", "type_text"] {
            for outcome in &outcomes {
                let error = refusal(tool, outcome, true);
                let next = error.to_json()["next"].as_str().unwrap_or_default().to_string();
                let why = format!("{tool} refused for {}", error.details["reason"]);
                assert!(!next.is_empty(), "{why}: no next");
                if error.code == "CAPABILITY_UNAVAILABLE" {
                    assert_ne!(next, crate::error::next_moves(error.code), "{why}: only the general words");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_json_rpc_error_keeps_the_worker_and_says_whether_the_input_could_have_run() {
        let dir = temp_dir();
        let replies = r#"      click) reply='"error":{"code":-32602,"message":"Invalid params: unknown field `x`"}';;
      scroll) reply='"error":{"code":-32603,"message":"daemon transport error forwarding `scroll`"}';;"#;
        let cua = cua(&dir, fake_cua(&dir, true, replies));

        let rejected = cua.click_at(1, 1, 5.0, 5.0, "left", false, None).await.unwrap_err();
        assert_eq!(rejected.code, "CAPABILITY_UNAVAILABLE");
        assert_eq!(rejected.details["execution_not_started"], json!(true));
        assert!(rejected.message.contains("unknown field"), "Cua's reason is kept: {}", rejected.message);
        assert!(cua.running().await, "a rejection is an answer, not a dead worker");

        let unknown = cua.scroll(1, 1, 0, 3).await.unwrap_err();
        assert_eq!(unknown.code, "OUTCOME_UNKNOWN", "an error that can follow a tool that ran is not a refusal");

        cua.tree(1, 1).await.unwrap();
        assert_eq!(lines(dir.join("starts.log")).len(), 1, "one worker served every call");
        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Case arms for [`fake_cua`] that answer pointer input as Cua 0.29.1
    /// does (`element_cache.rs`, and probed on a test computer): once a window has a
    /// snapshot, a click or drag at window pixels is refused with
    /// `screenshot_context_missing` unless the latest `get_window_state` of it
    /// took a picture under the same `session` as the input. A plain read
    /// takes the picture's place. A picture smaller than the window moves the
    /// point (Cua scales pixels by the picture's size); each dispatched input
    /// goes to `landed.log` with `native` or `scaled`, and a picture file
    /// named by `screenshot_out_file` is written as Cua writes it.
    const CUA_029_POINTER: &str = r#"      get_window_state)
        case "$line" in
        *'"include_screenshot":true'*)
          owner=$(printf '%s' "$line" | sed -n 's/.*"session":"\([^"]*\)".*/\1/p')
          case "$line" in *'"max_image_dimension":0'*) size=native;; *) size=scaled;; esac
          file=$(printf '%s' "$line" | sed -n 's/.*"screenshot_out_file":"\([^"]*\)".*/\1/p')
          [ -z "$file" ] || [ "$file" = /dev/null ] || echo png >"$file"
          echo "${owner:-none} $size" >"$dir/picture";;
        *) rm -f "$dir/picture";;
        esac;;
      click|drag)
        owner=$(printf '%s' "$line" | sed -n 's/.*"session":"\([^"]*\)".*/\1/p')
        if [ -f "$dir/picture" ] && [ "$(cut -d' ' -f1 "$dir/picture")" = "${owner:-none}" ]; then
          echo "$tool $(cut -d' ' -f2 "$dir/picture")" >>"$dir/landed.log"
        else
          reply='"result":{"isError":true,"content":[{"type":"text","text":"The latest snapshot for this window does not contain a screenshot owned by this session. Call get_window_state with a screenshot on the same connection before using pixels."}],"structuredContent":{"code":"screenshot_context_missing","pid":1,"window_id":1}}'
        fi;;"#;

    #[tokio::test]
    async fn an_agent_clicks_and_drags_at_window_points_after_reading_its_elements_on_cua_0_29() {
        let dir = temp_dir();
        let cua = cua(&dir, fake_cua(&dir, true, CUA_029_POINTER));
        cua.set_agent(Some("codex@test".into()));
        // The agent has the screen: its input goes only then.
        cua.show_cursor(0.0, 0.0).await.unwrap();

        cua.tree(1, 1).await.unwrap();
        cua.click_at(1, 1, 40.0, 40.0, "left", false, None).await.unwrap();
        cua.drag(1, 1, (30.0, 30.0), (60.0, 40.0), Duration::from_millis(50), None).await.unwrap();
        cua.tree(1, 1).await.unwrap();
        cua.drag(1, 1, (30.0, 30.0), (60.0, 40.0), Duration::from_millis(50), None).await.unwrap();
        assert_eq!(
            lines(dir.join("landed.log")),
            ["click native", "drag native", "drag native"],
            "each input is sent once and lands at the window point asked for"
        );
        let pictures = std::fs::read_to_string(dir.join("calls.log")).unwrap().matches("get_window_state").count();
        assert_eq!(pictures, 4, "a picture only when Cua asked for one (two reads, two pictures)");

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_point_cua_still_refuses_after_a_picture_is_refused_before_any_input() {
        let dir = temp_dir();
        // A Cua that never accepts the picture (no window picture on this
        // desktop): the click is refused both times.
        let replies = CUA_029_POINTER.replace(r#"echo "${owner:-none} $size" >"$dir/picture""#, ":");
        let cua = cua(&dir, fake_cua(&dir, true, &replies));
        cua.set_agent(Some("codex@test".into()));
        cua.show_cursor(0.0, 0.0).await.unwrap();

        cua.tree(1, 1).await.unwrap();
        let refused = cua.click_at(1, 1, 40.0, 40.0, "left", false, None).await.unwrap_err();
        assert_eq!(refused.code, "CAPABILITY_UNAVAILABLE", "{}", refused.message);
        assert_eq!(refused.details["execution_not_started"], json!(true));
        let clicks = lines(dir.join("calls.log")).iter().filter(|c| *c == "click").count();
        assert_eq!(clicks, 2, "one more try, not a loop");
        assert!(lines(dir.join("landed.log")).is_empty());

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_person_who_takes_the_computer_before_the_second_try_gets_no_click() {
        let dir = temp_dir();
        let cua = cua(&dir, fake_cua(&dir, true, CUA_029_POINTER));
        cua.set_agent(Some("codex@test".into()));
        cua.show_cursor(0.0, 0.0).await.unwrap();
        let taken = Cancel::new();
        taken.cancel();

        cua.tree(1, 1).await.unwrap();
        let stopped = cua.click_at(1, 1, 40.0, 40.0, "left", false, Some(&taken)).await.unwrap_err();
        assert_eq!(stopped.details["execution_not_started"], json!(true), "{}", stopped.message);
        assert!(lines(dir.join("landed.log")).is_empty(), "no input after the handoff");

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cua_0_28_clicks_without_a_picture() {
        let dir = temp_dir();
        let cua = cua(&dir, fake_cua(&dir, true, ""));
        cua.set_agent(Some("codex@test".into()));
        cua.show_cursor(0.0, 0.0).await.unwrap();

        cua.tree(1, 1).await.unwrap();
        cua.click_at(1, 1, 40.0, 40.0, "left", false, None).await.unwrap();
        cua.drag(1, 1, (30.0, 30.0), (60.0, 40.0), Duration::from_millis(50), None).await.unwrap();
        let calls = lines(dir.join("calls.log"));
        assert_eq!(calls.iter().filter(|c| *c == "get_window_state").count(), 1, "no picture is taken: {calls:?}");
        assert_eq!(calls.iter().filter(|c| matches!(c.as_str(), "click" | "drag")).count(), 2);

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Case arms for [`fake_cua`] that read a slow window as Cua 0.29.1
    /// read Google Chrome showing a sign-up page on a test computer (28 September):
    /// a whole walk took 1.9–2.4 s, and one given less (Cua's own 1 s unless
    /// `timeout_ms` says otherwise) came back cut short, a different part
    /// each time. Here a walk given at least `whole_ms` reads the whole
    /// window; a shorter one reaches the form's button on odd reads and not
    /// on even ones. Each element clicked goes to `clicked.log` by token.
    fn slow_chrome(whole_ms: u64) -> String {
        let element = |i: u64, parent: Option<u64>, role: &str, label: &str| {
            let mut e = json!({
                "element_index": i, "role": role, "label": label, "enabled": true, "actions": ["doDefault"],
                "element_token": format!("s1:{i}"), "frame": {"x": 10 * i, "y": 10 * i, "w": 80, "h": 20},
            });
            if let Some(p) = parent {
                e["parent_index"] = json!(p);
            }
            e
        };
        let tree = [
            element(0, None, "frame", "Create your account - Google Chrome"),
            element(1, Some(0), "panel", "\u{fffc}\u{fffc}"),
            element(2, Some(1), "document web", "Create your account"),
            element(3, Some(2), "form", "\u{fffc}\u{fffc}\u{fffc}"),
            element(4, Some(3), "entry", "Full name"),
            element(5, Some(3), "button", "Create account"),
            element(6, Some(3), "link", "Terms"),
        ];
        let read = |n: usize| {
            let sc = if n == tree.len() {
                json!({"elements_complete": true, "truncated": false, "elements": tree})
            } else {
                json!({"elements_complete": false, "truncated": true, "truncation_reason": "timeout", "elements": &tree[..n]})
            };
            format!("'\"result\":{}'", json!({"isError": false, "content": [], "structuredContent": sc}))
        };
        format!(
            r#"      get_window_state)
        budget=$(printf '%s' "$line" | sed -n 's/.*"timeout_ms":\([0-9]*\).*/\1/p')
        reads=$(( $(cat "$dir/reads" 2>/dev/null || echo 0) + 1 )); echo "$reads" >"$dir/reads"
        if [ "${{budget:-1000}}" -ge {whole_ms} ]; then reply={whole}
        elif [ $((reads % 2)) = 1 ]; then reply={with_button}
        else reply={without_button}
        fi;;
      click) printf '%s' "$line" | sed -n 's/.*"element_token":"\([^"]*\)".*/\1/p' >>"$dir/clicked.log";;"#,
            whole = read(tree.len()),
            with_button = read(6),
            without_button = read(5),
        )
    }

    /// The element a frame offers is clicked in a window that takes longer to
    /// read than Cua's own 1 s walk. Before, the frame came from one part of
    /// Chrome's window and the click's fresh read from another, without the
    /// button, and the click was refused as "no longer resolves uniquely".
    #[tokio::test]
    async fn an_element_offered_in_a_slow_window_is_clicked() {
        let dir = temp_dir();
        let cua = cua(&dir, fake_cua(&dir, true, &slow_chrome(3000)));
        let page = cua.elements(1, 1, None, 40, None).await.unwrap();
        let button = page.elements.iter().find(|e| e.name == "Create account").expect("the frame offers the button");
        cua.click_element(&button.selector, &button.role, &button.name, "click", Rect::default(), None, &Screens::of(&[])).await.unwrap();
        assert_eq!(lines(dir.join("clicked.log")), ["s1:5"], "the button, once");
        assert!(page.elements.iter().any(|e| e.name == "Terms"), "the frame shows the whole window: {:?}", page.elements);

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A window too slow to read whole even in ibara's time: an element
    /// missing from the part read at click time is not clicked, and the
    /// refusal says the window was read in part, not that the element changed.
    #[tokio::test]
    async fn an_element_missing_from_a_window_read_in_part_is_refused_as_such() {
        let dir = temp_dir();
        let cua = cua(&dir, fake_cua(&dir, true, &slow_chrome(1_000_000)));
        let page = cua.elements(1, 1, None, 40, None).await.unwrap();
        let button = page.elements.iter().find(|e| e.name == "Create account").expect("the first part holds the button");
        let refused = cua.click_element(&button.selector, &button.role, &button.name, "click", Rect::default(), None, &Screens::of(&[])).await.unwrap_err();
        assert_eq!(refused.code, "STALE_TARGET", "{}", refused.message);
        assert_eq!(refused.details.get("reason"), Some(&json!("read_in_part")), "{}", refused.message);
        assert_eq!(refused.details["execution_not_started"], json!(true));
        assert!(lines(dir.join("clicked.log")).is_empty(), "nothing was clicked");

        cua.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_window_whose_only_element_is_its_unnamed_frame_has_no_accessibility_tree() {
        // foot, as Cua 0.29.1 and 0.28.2 both reported it (27 September).
        let foot = json!({
            "degraded": true,
            "element_count": 1,
            "elements_complete": false,
            "elements": [{"actions": ["activate"], "depth": 0, "element_index": 0, "element_token": "s00000001:0", "role": "window"}],
        });
        let tree = tree_from_snapshot(1, 1, &foot);
        assert!(!tree.available, "foot reads as a window without a tree");
        assert!(!page(tree, None, 20, None).available);

        // An unnamed frame that holds real elements is a tree.
        let editor = json!({
            "elements": [
                {"element_index": 0, "role": "window", "actions": ["activate"]},
                {"element_index": 1, "parent_index": 0, "role": "push button", "label": "Save", "actions": ["press"]},
            ],
        });
        assert!(tree_from_snapshot(1, 1, &editor).available);
        // So is a lone element with a name.
        let named = json!({"elements": [{"element_index": 0, "role": "frame", "label": "Untitled - Mousepad"}]});
        assert!(tree_from_snapshot(1, 1, &named).available);
    }

    /// Boxes as Cua gave them on test computers: Files (GTK 4) with its
    /// window element, on the screen through Cua 0.28.2; Mousepad (GTK 3),
    /// whose read has no window element, on the screen through 0.28.2 and in
    /// the window's own coordinates through 0.29.1.
    #[test]
    fn an_elements_center_is_placed_on_the_screen_however_its_app_gives_its_box() {
        let files = Rect { x: 967, y: 38, width: 941, height: 1030 };
        let documents = [1170.0, 84.0, 144.0, 132.0];
        assert_eq!(screen_center(documents, Some([967.0, 38.0, 941.0, 1030.0]), files), (1242.0, 150.0));
        assert_eq!(screen_center([203.0, 46.0, 144.0, 132.0], Some([0.0, 0.0, 941.0, 1030.0]), files), (1242.0, 150.0), "the same, given in the window");
        let mousepad = Rect { x: 12, y: 38, width: 941, height: 1030 };
        assert_eq!(screen_center([13.0, 68.0, 939.0, 997.0], None, mousepad), (482.5, 566.5), "on the screen");
        assert_eq!(screen_center([1.0, 30.0, 939.0, 997.0], None, mousepad), (482.5, 566.5), "in the window");
    }
}
