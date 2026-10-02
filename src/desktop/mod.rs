//! The desktop layer: every effect on and observation of the local Hyprland
//! desktop, as primitives the controller composes.
//!
//! Agent input, accessibility and the agent cursor go through Cua
//! (`cua-driver`, [`cua`]). `hyprctl`, `grim`, `omarchy-toggle-idle` and the
//! approved app launchers remain; everything else is in-process. See
//! docs/internals.md "Desktop layer" for the list of primitives, the program each
//! uses and its failure codes.

pub mod apps;
pub mod atspi;
pub mod capture;
pub mod chrome;
pub mod clipboard;
pub mod compositor_input;
pub mod cua;
pub mod handover;
pub mod hyprland;
pub mod idle;
pub mod input;
pub mod run;
pub mod selection;
pub mod video;
pub mod watch;

pub use apps::App;
pub use atspi::{Element, ElementPage};
pub use capture::{ImageBudget, ImageFormat, PreviewFormat, PreviewQuality};
pub use hyprland::{Monitor, Rect, SurfaceId, Window};
pub use input::{Button, TypingCursor};
pub use run::Cancel;
pub use watch::DesktopEvent;

use crate::error::{IbaraError, Result, internal, invalid};
use hyprland::{Hyprland, Screens};
use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch as tokio_watch};

const SURFACE_CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);
const PREVIEW_DEADLINE: Duration = Duration::from_millis(2000);
/// How long a menu level may take to close after Escape.
const MENU_CLOSE: Duration = Duration::from_secs(1);
const MENU_POLL: Duration = Duration::from_millis(50);
/// Tries of a shortcut after its menu closed (at [`MENU_POLL`]).
const MENU_RELEASE_TRIES: u32 = 20;
/// Moves at most to reach a menu item (open, walk, open a submenu, …).
const MENU_MOVES: usize = 8;

/// Where the desktop layer finds its programs and state.
#[derive(Debug, Clone)]
pub struct DesktopConfig {
    pub release_root: PathBuf,
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub hyprctl: PathBuf,
    /// `hyprctl -i` instance: `0` is the oldest live Hyprland.
    pub hyprland_instance: String,
    pub grim: PathBuf,
    /// `cua-driver`, run as a private child with its home under `state_dir`.
    pub cua: PathBuf,
    pub idle_binary: PathBuf,
    /// `wf-recorder`, the live video encoder.
    pub wf_recorder: PathBuf,
    pub idle_state: PathBuf,
    pub apps: Vec<App>,
    /// Added to the daemon's environment for every helper.
    pub env: Vec<(OsString, OsString)>,
}

impl DesktopConfig {
    /// Production layout under `release_root`, `runtime_dir` and `state_dir`.
    /// `IBARA_TEST_PREVIEW_TOOLS=1` swaps in `IBARA_TEST_HYPRCTL`,
    /// `IBARA_TEST_GRIM`, `IBARA_TEST_CUA`, `IBARA_TEST_IDLE` and
    /// `IBARA_TEST_WF_RECORDER` for
    /// end-to-end tests.
    pub fn new(release_root: &Path, runtime_dir: &Path, state_dir: &Path) -> Self {
        let test_tools = std::env::var("IBARA_TEST_PREVIEW_TOOLS").as_deref() == Ok("1");
        let tool = |var: &str, default: &str| -> PathBuf {
            match std::env::var_os(var).filter(|_| test_tools) {
                Some(path) => path.into(),
                None => default.into(),
            }
        };
        let mut env = Vec::new();
        if std::env::var_os("GDK_SCALE").is_none() {
            env.push(("GDK_SCALE".into(), "1".into()));
        }
        DesktopConfig {
            release_root: release_root.to_path_buf(),
            runtime_dir: runtime_dir.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            hyprctl: tool("IBARA_TEST_HYPRCTL", "hyprctl"),
            hyprland_instance: "0".into(),
            grim: tool("IBARA_TEST_GRIM", "/usr/bin/grim"),
            cua: tool("IBARA_TEST_CUA", "/usr/bin/cua-driver"),
            idle_binary: tool("IBARA_TEST_IDLE", "/usr/bin/omarchy-toggle-idle"),
            wf_recorder: tool("IBARA_TEST_WF_RECORDER", "/usr/bin/wf-recorder"),
            idle_state: state_dir.join("idle-owned.json"),
            apps: apps::default_catalog(release_root),
            env,
        }
    }

    /// From the environment: `IBARA_RELEASE_ROOT` (else
    /// `<IBARA_INSTALL_ROOT or /opt/agent-computer>/current`),
    /// `IBARA_RUNTIME_DIR` (`/run/agent-computer`) and `IBARA_STATE_DIR`
    /// (`~/.local/state/agent-computer`).
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let install = var("IBARA_INSTALL_ROOT").unwrap_or_else(|| "/opt/agent-computer".into());
        let release = var("IBARA_RELEASE_ROOT").unwrap_or_else(|| install.join("current"));
        let runtime = var("IBARA_RUNTIME_DIR").unwrap_or_else(|| "/run/agent-computer".into());
        let state = var("IBARA_STATE_DIR").unwrap_or_else(|| {
            var("HOME").unwrap_or_else(|| "/".into()).join(".local/state/agent-computer")
        });
        Self::new(&release, &runtime, &state)
    }
}

/// Monitors, windows and focus read together.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub monitors: Vec<Monitor>,
    pub windows: Vec<Window>,
    pub active: Option<Window>,
    /// See [`hyprland::geometry_key`].
    pub geometry_key: String,
}

impl Snapshot {
    /// The focused monitor, else the first.
    pub fn focused_monitor(&self) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.focused).or_else(|| self.monitors.first())
    }
}

/// A point in logical layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Enough to act on an element again: its window and the helper's selector.
#[derive(Debug, Clone)]
pub struct ElementTarget {
    pub surface: SurfaceId,
    pub selector: Value,
    pub role: String,
    pub name: String,
    pub actions: Vec<String>,
}

impl Element {
    pub fn target(&self, surface: SurfaceId) -> ElementTarget {
        ElementTarget {
            surface,
            selector: self.selector.clone(),
            role: self.role.clone(),
            name: self.name.clone(),
            actions: self.actions.clone(),
        }
    }
}

/// What a click lands on.
#[derive(Debug, Clone)]
pub enum ClickTarget {
    /// Logical coordinates. With a surface, that surface must be focused.
    Point { x: f64, y: f64, surface: Option<SurfaceId> },
    /// Activated through accessibility, not the pointer.
    Element(ElementTarget),
}

/// A launched app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Launched {
    /// PID of the launched program (the window's PID for direct launchers).
    pub pid: u32,
}

/// An encoded agent image and what it shows.
#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    /// The logical layout rectangle the image covers.
    pub region: Rect,
    pub captured_at: String,
    pub elapsed_ms: u64,
}

impl EncodedImage {
    pub fn mime_type(&self) -> &'static str {
        self.format.mime_type()
    }
    /// Image pixel to logical layout coordinates (`image_to_logical`, §5).
    pub fn to_logical(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.region.x as f64 + x * self.region.width as f64 / self.width.max(1) as f64,
            self.region.y as f64 + y * self.region.height as f64 / self.height.max(1) as f64,
        )
    }
}

/// An operator preview frame (§11.1).
#[derive(Debug, Clone)]
pub struct Preview {
    /// Encoded in the format that was asked for.
    pub picture: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub display_revision: String,
    pub captured_at: String,
}

/// An output the operator can preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplayInfo {
    pub display_id: String,
    pub label: String,
    pub display_revision: String,
}

/// A change the headless output fallback (§12.3) makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputChange {
    /// No output is enabled: create `IbaraVirtual`.
    Create,
    /// A physical output is enabled beside `IbaraVirtual`: remove it.
    Remove,
}

impl OutputChange {
    /// The change these outputs need. Hyprland's `FALLBACK` placeholder is
    /// not an output, so `IbaraVirtual` also returns when the last physical
    /// output goes and only the placeholder is left.
    pub fn needed(monitors: &[Monitor]) -> Option<OutputChange> {
        let enabled = || monitors.iter().filter(|m| !m.disabled);
        let physical = enabled().any(|m| m.name != hyprland::VIRTUAL_OUTPUT && m.name != hyprland::FALLBACK_OUTPUT);
        let virtual_ = enabled().any(|m| m.name == hyprland::VIRTUAL_OUTPUT);
        match (physical, virtual_) {
            (true, true) => Some(OutputChange::Remove),
            (false, false) => Some(OutputChange::Create),
            _ => None,
        }
    }
}

/// One capability probe row (`native.ts:321-348`).
#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    pub name: &'static str,
    pub status: &'static str,
    pub backend: String,
    pub last_tested_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

type PreviewResult = Option<Result<Arc<Preview>>>;

struct PreviewFlight {
    key: (String, PreviewQuality, PreviewFormat),
    rx: tokio_watch::Receiver<PreviewResult>,
}

/// Clears the in-flight preview when the leader finishes or is dropped.
struct FlightGuard<'a> {
    slot: &'a Mutex<Option<PreviewFlight>>,
    tx: Option<tokio_watch::Sender<PreviewResult>>,
    cancel: Cancel,
}

impl FlightGuard<'_> {
    fn finish(mut self, result: Result<Arc<Preview>>) {
        take_slot(self.slot);
        if let Some(tx) = self.tx.take() {
            tx.send_replace(Some(result));
        }
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            self.cancel.cancel();
            take_slot(self.slot);
            tx.send_replace(Some(Err(IbaraError::new("TIMEOUT", "Preview capture was abandoned.", true))));
        }
    }
}

fn take_slot(slot: &Mutex<Option<PreviewFlight>>) {
    slot.lock().unwrap_or_else(|p| p.into_inner()).take();
}

fn stale(message: &str) -> IbaraError {
    IbaraError::new("STALE_TARGET", message, true)
}

fn not_on_screen() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "Surface is not on a visible workspace.", true)
        .with("reason", "not_visible")
        .with("next", "Focus the surface, then capture it again.")
}

/// Parse `/proc/pressure/memory` and `/proc/meminfo`; `Some` when memory is
/// tight: less than 15 % available, or more than 10 % of time stalled on
/// memory over the last 10 s (PSI `some avg10`).
pub fn memory_pressure_from(pressure: Option<&str>, meminfo: &str) -> Option<String> {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
    };
    let low_memory = match (field("MemTotal:"), field("MemAvailable:")) {
        (Some(total), Some(available)) if total > 0 => available * 100 < total * 15,
        _ => false,
    };
    let stalled = pressure
        .and_then(|p| p.lines().find(|l| l.starts_with("some ")))
        .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg10=")))
        .and_then(|v| v.parse::<f64>().ok())
        .is_some_and(|avg10| avg10 > 10.0);
    (low_memory || stalled).then(|| "memory tight · close tabs".to_string())
}

/// The desktop of this computer. Cheap to construct (no I/O); share it in an `Arc`.
pub struct Desktop {
    cfg: DesktopConfig,
    env: Arc<[(OsString, OsString)]>,
    hypr: Hyprland,
    cua: cua::Cua,
    compositor_input: compositor_input::CompositorInput,
    /// When text was last typed, for [`input::AFTER_TYPING`].
    typed_at: Mutex<Option<Instant>>,
    idle: idle::Idle,
    events: broadcast::Sender<DesktopEvent>,
    preview: Mutex<Option<PreviewFlight>>,
    /// Which one cursor the screen shows: the person's or the agent's.
    handover: handover::Handover,
    /// Live video streams of the displays.
    video: video::Video,
}

impl Desktop {
    pub fn new(cfg: DesktopConfig) -> Desktop {
        let env: Arc<[(OsString, OsString)]> = Arc::from(cfg.env.clone());
        let hypr = Hyprland::new(&cfg.hyprctl, cfg.hyprland_instance.clone(), env.clone());
        let cua = cua::Cua::new(cua::CuaConfig { program: cfg.cua.clone(), home: cfg.state_dir.join("cua"), env: env.clone() });
        let idle = idle::Idle::new(cfg.idle_binary.clone(), cfg.idle_state.clone(), env.clone());
        let (events, _) = broadcast::channel(watch::EVENT_CAPACITY);
        // Where helpers find Hyprland's sockets.
        let runtime = cfg.env.iter().rev().find(|(k, _)| k == "XDG_RUNTIME_DIR").map(|(_, v)| PathBuf::from(v)).or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from));
        let compositor_input = compositor_input::CompositorInput::new(hypr.clone(), cua.clone(), runtime.clone());
        let handover = handover::Handover::new(hypr.clone(), cua.clone(), cfg.state_dir.join("pointer-hidden"), runtime);
        let video = video::Video::new(cfg.wf_recorder.clone(), env.clone());
        Desktop { cfg, env, hypr, cua, compositor_input, typed_at: Mutex::new(None), idle, events, preview: Mutex::new(None), handover, video }
    }

    pub fn config(&self) -> &DesktopConfig {
        &self.cfg
    }

    pub fn hyprland(&self) -> &Hyprland {
        &self.hypr
    }

    // ---- events ----

    /// Desktop events (window open/close/title, focus, monitors, display changes).
    pub fn subscribe(&self) -> broadcast::Receiver<DesktopEvent> {
        self.events.subscribe()
    }

    /// Start the Hyprland event-socket task (once, at daemon start).
    pub fn start_watch(&self) -> tokio::task::JoinHandle<()> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_default();
        tokio::spawn(watch::run_watch(self.hypr.clone(), runtime, self.events.clone()))
    }

    // ---- reading ----

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        self.hypr.monitors().await
    }

    pub async fn windows(&self) -> Result<Vec<Window>> {
        self.hypr.clients().await
    }

    pub async fn focused(&self) -> Result<Option<Window>> {
        self.hypr.active_window().await
    }

    /// The workspace the focused monitor shows (else the first monitor's).
    pub async fn active_workspace(&self) -> Result<Option<hyprland::WorkspaceRef>> {
        let monitors = self.hypr.monitors().await?;
        Ok(monitors.iter().find(|m| m.focused).or(monitors.first()).map(|m| m.active_workspace.clone()))
    }

    /// Monitors, windows and focus, read concurrently.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let (monitors, windows, active) =
            tokio::join!(self.hypr.monitors(), self.hypr.clients(), self.hypr.active_window());
        let monitors = monitors?;
        if monitors.is_empty() {
            return Err(IbaraError::new("SESSION_UNAVAILABLE", "No Hyprland output is available.", true));
        }
        Ok(Snapshot { geometry_key: hyprland::geometry_key(&monitors), monitors, windows: windows?, active: active? })
    }

    /// The window with this identity, if it still exists.
    pub async fn live(&self, id: &SurfaceId) -> Result<Option<Window>> {
        Ok(self.hypr.clients().await?.into_iter().find(|w| w.is(id)))
    }

    /// The surface exists and has keyboard focus (nothing auto-focuses):
    /// keys and text sent now reach it and no other window.
    pub async fn ensure_focused(&self, id: &SurfaceId) -> Result<Window> {
        let (live, active) = tokio::join!(self.live(id), self.hypr.active_window());
        let live = live?.ok_or_else(|| stale("Surface is gone."))?;
        match active? {
            Some(active) if active.address == id.address => Ok(live),
            active => {
                let now = active.map_or_else(|| "no window has it now".to_string(), |a| format!("{} \"{}\" has it now", a.class, a.title));
                Err(stale(&format!("{} no longer has the keyboard focus: {now}.", live.class))
                    .with("execution_not_started", true)
                    .with("next", format!("Focus the {} app again before sending keys or text to it.", live.class)))
            }
        }
    }

    /// `assertSessionReady` (§12.1): unlocked, or `HUMAN_CONTROL` /
    /// `SESSION_UNAVAILABLE`.
    pub async fn session_ready(&self) -> Result<()> {
        if !graphical_env() {
            return Err(IbaraError::new("SESSION_UNAVAILABLE", "No graphical session is available.", true));
        }
        idle::require_unlocked(self.hypr.monitors().await)
    }

    /// A person's Take Control: like [`Self::session_ready`], but a locked
    /// screen is fine, since they unlock it through the viewer.
    pub async fn control_ready(&self) -> Result<()> {
        if !graphical_env() {
            return Err(IbaraError::new("SESSION_UNAVAILABLE", "No graphical session is available.", true));
        }
        idle::require_known(self.hypr.monitors().await)
    }

    /// The screen is locked (Hyprland's session lock); false when unlocked
    /// or unreadable.
    pub async fn locked(&self) -> bool {
        graphical_env() && self.hypr.monitors_quick().await.is_ok_and(|m| idle::lock_state(&m) == idle::LockState::Locked)
    }

    /// `Some("memory tight · close tabs")` when this computer is short of memory.
    pub fn memory_pressure(&self) -> Option<String> {
        let pressure = std::fs::read_to_string("/proc/pressure/memory").ok();
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        memory_pressure_from(pressure.as_deref(), &meminfo)
    }

    /// One page of accessibility elements for a window (`available: false`
    /// when the app exposes no usable tree).
    pub async fn elements(
        &self,
        surface: &SurfaceId,
        query: Option<&str>,
        limit: u32,
        cursor: Option<u32>,
    ) -> Result<ElementPage> {
        let live = self.live(surface).await?.ok_or_else(|| stale("Requested surface is no longer present."))?;
        if live.title.is_empty() || live.pid <= 0 {
            return Ok(ElementPage::default());
        }
        self.cua.elements(live.pid, cua::window_id(&live.address)?, query, limit, cursor).await
    }

    /// The agent that holds control, whose named cursor the screen shows
    /// from now until control ends or a person takes over ([`handover`]), or
    /// none. With none, the screen goes back to the person's pointer within
    /// [`handover::POLL`], and at once through [`Desktop::release_input`].
    pub fn set_agent(&self, label: Option<String>) {
        let begins = label.is_some();
        self.cua.set_agent(label);
        if begins {
            self.handover.begin();
        }
    }

    /// Whether the surface shows a password field (replay keeps no picture).
    pub async fn has_password_field(&self, surface: &SurfaceId) -> Result<bool> {
        let live = self.live(surface).await?.ok_or_else(|| stale("Surface is gone."))?;
        self.cua.has_password_field(live.pid, cua::window_id(&live.address)?).await
    }

    /// The window a point lands on: the named surface, else the one
    /// [`window_under`] it, and what the screens show. Coordinates come
    /// back window-local. A point no screen shows is refused: the pointer
    /// cannot go there, and the agent's cursor may not.
    async fn window_at(&self, x: f64, y: f64, surface: Option<&SurfaceId>) -> Result<(Window, f64, f64, Screens)> {
        let (window, monitors) = match surface {
            Some(surface) => {
                let (window, monitors) = tokio::join!(self.ensure_focused(surface), self.hypr.monitors());
                (window?, monitors.unwrap_or_default())
            }
            None => {
                let (monitors, windows, active) = tokio::join!(self.hypr.monitors(), self.hypr.clients(), self.hypr.active_window());
                let monitors = monitors?;
                (window_under(x, y, active?, windows?, &monitors)?, monitors)
            }
        };
        let (lx, ly) = (x - window.at[0] as f64, y - window.at[1] as f64);
        if lx < 0.0 || ly < 0.0 || lx >= window.size[0] as f64 || ly >= window.size[1] as f64 {
            return Err(invalid("The point is outside the surface.").with("field", "x/y"));
        }
        let screens = Screens::of(&monitors);
        if !screens.show(x, y) {
            return Err(invalid("No screen shows that point.").with("field", "x/y"));
        }
        Ok((window, lx, ly, screens))
    }

    /// What the screens show now (anywhere, when Hyprland does not say).
    async fn screens(&self) -> Screens {
        Screens::of(&self.hypr.monitors().await.unwrap_or_default())
    }

    /// Wait out [`input::AFTER_TYPING`] before a key or button press.
    async fn after_typing(&self) {
        let typed_at = *self.typed_at.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(left) = typed_at.map(|t| input::AFTER_TYPING.saturating_sub(t.elapsed())).filter(|d| !d.is_zero()) {
            tokio::time::sleep(left).await;
        }
    }

    // ---- effects ----

    async fn uses_dispatchers(&self) -> bool {
        match crate::settings::current().text("input_backend").as_deref() {
            Some("dispatchers") => true,
            Some("plugin") => false,
            _ => self.hypr.hypoland_version().await.ok().flatten().is_some(),
        }
    }

    async fn prepare_dispatchers(&self) -> Result<()> {
        let created = self.compositor_input.prepare().await?;
        if created && self.cua.cursor_shown() {
            // Creating the seat pointer resets the compositor's hide timer.
            // Let its next cursor tick pass before sending any agent input.
            tokio::time::sleep(Duration::from_millis(650)).await;
        }
        Ok(())
    }

    /// Run an effect; on failure wait for helpers and release held input
    /// (`CONTROL_UNSETTLED` if that fails), and mark the error
    /// `execution_not_started` when no mutating helper ran (`act`, §4).
    /// Nothing starts once `cancel` has fired (control changed hands). The
    /// screen is the agent's first ([`handover`]): after a person took it,
    /// the agent's input takes it back.
    async fn effect<T>(&self, cancel: Option<&Cancel>, work: impl Future<Output = Result<T>>) -> Result<T> {
        cua::unless_cancelled(cancel)?;
        if self.uses_dispatchers().await {
            // The passive seat pointer must exist before hiding it. Launch
            // and window management still work if wheel support is missing.
            let _ = self.prepare_dispatchers().await;
        }
        self.handover.to_agent().await?;
        cua::unless_cancelled(cancel)?;
        let before = (run::mutating_spawn_count(), cua::dispatch_count(), compositor_input::dispatch_count());
        match work.await {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.details.get("reason").and_then(Value::as_str) == Some("interrupted") {
                    self.handover.person_moved().await;
                }
                let started = (run::mutating_spawn_count(), cua::dispatch_count(), compositor_input::dispatch_count()) != before
                    && error.details.get("execution_not_started") != Some(&Value::Bool(true));
                if let Err(release) = self.release_input().await {
                    return Err(if release.code == "CONTROL_UNSETTLED" {
                        release
                    } else {
                        IbaraError::new("CONTROL_UNSETTLED", "Held input could not be released.", false)
                            .requires_reconciliation()
                    });
                }
                Err(if started { error } else { error.with("execution_not_started", true) })
            }
        }
    }

    /// Launch an approved app, detached.
    pub async fn launch(&self, app_id: &str, workspace_file: Option<&Path>, cancel: Option<&Cancel>) -> Result<Launched> {
        self.effect(cancel, async {
            let cmd = apps::launch_command(&self.cfg.apps, app_id, workspace_file)?;
            let pid = run::spawn_detached(cmd.envs(&self.env).mutates(true)).await?;
            Ok(Launched { pid })
        })
        .await
    }

    pub async fn focus(&self, surface: &SurfaceId, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            self.live(surface).await?.ok_or_else(|| stale("Cannot focus a missing surface."))?;
            // Hyprland may move the pointer onto the window it focuses.
            let motion = self.cua.motion();
            let _moving = motion.begin();
            self.hypr.focus_address(&surface.address).await
        })
        .await
    }

    /// Ask a window to close (like its close button); the address **and**
    /// PID must still match. Unsaved-changes prompts are the app's.
    pub async fn close(&self, surface: &SurfaceId, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            self.live(surface).await?.ok_or_else(|| stale("Surface is gone."))?;
            // Hyprland may move the pointer onto the window focused next.
            let motion = self.cua.motion();
            let _moving = motion.begin();
            self.hypr.close_window(&surface.address, surface.pid).await
        })
        .await
    }

    /// Close a window for a person managing windows: as [`Self::close`], but
    /// no agent input goes, so nothing waits for the screen or takes it from
    /// the person.
    pub async fn close_for_person(&self, surface: &SurfaceId) -> Result<()> {
        // Hyprland may move the pointer onto the window focused next.
        let motion = self.cua.motion();
        let _moving = motion.begin();
        self.hypr.close_window(&surface.address, surface.pid).await
    }

    /// Move a window to `workspace` for a person, leaving the workspace shown
    /// as it is; the address **and** PID must still match.
    pub async fn move_for_person(&self, surface: &SurfaceId, workspace: i64) -> Result<()> {
        // Hyprland may move the pointer onto the window focused next.
        let motion = self.cua.motion();
        let _moving = motion.begin();
        self.hypr.move_window(&surface.address, surface.pid, workspace).await
    }

    /// Click, double-click or right-click. A point is clicked through Cua's
    /// exact-target route in the window under it; an element, after checking
    /// it still resolves to the same role and name, is activated through
    /// accessibility, or clicked by the pointer at its center as a point is
    /// when Cua has no accessibility route for the click
    /// ([`cua::ElementClick::Pointer`]). `cancel` stops the click until its
    /// input is sent. Returns where it clicked as a fraction of the screen
    /// showing that point, when known (a menu item chosen by keyboard has no
    /// point).
    pub async fn click(&self, target: &ClickTarget, button: Button, double: bool, cancel: Option<&Cancel>) -> Result<Option<(f64, f64)>> {
        self.effect(cancel, async {
            self.after_typing().await;
            match target {
                ClickTarget::Point { x, y, surface } => {
                    let (window, lx, ly, screens) = self.window_at(*x, *y, surface.as_ref()).await?;
                    let (gx, gy) = (window.at[0] as f64 + lx, window.at[1] as f64 + ly);
                    if self.uses_dispatchers().await {
                        self.prepare_dispatchers().await?;
                        self.compositor_input.click(&window, (gx,gy), &screens, button, double, cancel).await?;
                        return Ok(screens.fraction(gx,gy));
                    }
                    self.point_from_cursor(&window).await;
                    self.cua.pace(gx, gy, &screens).await;
                    cua::unless_cancelled(cancel)?;
                    self.cua.click_at(window.pid, cua::window_id(&window.address)?, lx, ly, button.name(), double, cancel).await?;
                    Ok(screens.fraction(gx, gy))
                }
                ClickTarget::Element(element) => {
                    let (live, screens) = tokio::join!(self.ensure_focused(&element.surface), self.screens());
                    let live = live?;
                    if matches!(button, Button::Left) && !double && menu_item(element) {
                        return self.choose_menu_item(element, live.pid, cua::window_id(&live.address)?, &screens, cancel).await.map(|_| None);
                    }
                    let tool = element_tool(&element.actions, button, double)?;
                    let window = cua::window_id(&live.address)?;
                    match self.cua.click_element(&element.selector, &element.role, &element.name, tool, live.geometry(), cancel, &screens).await? {
                        cua::ElementClick::Sent(center) => Ok(center.and_then(|(x, y)| screens.fraction(x, y))),
                        cua::ElementClick::Pointer { at: (gx, gy) } => {
                            if !screens.show(gx, gy) {
                                return Err(stale("Nothing was sent: no screen shows that element now.")
                                    .with("execution_not_started", true)
                                    .with("next", "Scroll it into view, observe again and click it."));
                            }
                            let (lx, ly) = (gx - live.at[0] as f64, gy - live.at[1] as f64);
                            if self.uses_dispatchers().await {
                                self.prepare_dispatchers().await?;
                                self.compositor_input.click(&live, (gx,gy), &screens, button, double, cancel).await?;
                                return Ok(screens.fraction(gx,gy));
                            }
                            self.point_from_cursor(&live).await;
                            self.cua.pace(gx, gy, &screens).await;
                            cua::unless_cancelled(cancel)?;
                            self.cua.click_at(live.pid, window, lx, ly, button.name(), double, cancel).await?;
                            Ok(screens.fraction(gx, gy))
                        }
                    }
                }
            }
        })
        .await
    }

    /// A left click on an item in a menu, made as a keyboard user makes it
    /// ([`cua::MenuMove`]): open its menu through accessibility, walk the
    /// highlight to it with arrow keys, and press Return once a fresh read
    /// shows it highlighted, read again after the agent's cursor reached
    /// it. The open menu holds the keyboard, so its keys go through
    /// Hyprland ([`Desktop::menu_keys`]). Each move waits up to
    /// [`MENU_CLOSE`] for the menu to show it; when the item cannot be
    /// reached, the menu is closed again and nothing is chosen.
    async fn choose_menu_item(&self, element: &ElementTarget, pid: i64, window: u64, screens: &Screens, cancel: Option<&Cancel>) -> Result<()> {
        let read = || self.cua.menu_move(&element.selector, &element.role, &element.name);
        let surface = &element.surface;
        let mut next = read().await?;
        for moved in 0..MENU_MOVES {
            // Once a move went out, a refusal no longer means nothing was sent.
            let sent = |e: IbaraError| if moved > 0 { e.with("execution_not_started", false) } else { e };
            cua::unless_cancelled(cancel).map_err(sent)?;
            match &next {
                cua::MenuMove::Choose(at) => {
                    if let Some((x, y)) = at {
                        self.cua.lead(*x, *y, screens).await;
                        if !matches!(read().await.map_err(sent)?, cua::MenuMove::Choose(_)) {
                            return Err(IbaraError::new("STALE_TARGET", "The menu no longer shows the item highlighted, so Return was not sent.", true)
                                .with("execution_not_started", moved == 0));
                        }
                    }
                    cua::unless_cancelled(cancel).map_err(sent)?;
                    return self.menu_keys(surface, "Return", 1).await.map_err(sent);
                }
                cua::MenuMove::Open(token) => self.cua.click_token(pid, window, token).await.map_err(sent)?,
                cua::MenuMove::Keys(key, count) => self.menu_keys(surface, key, *count).await.map_err(sent)?,
            }
            let done = next.clone();
            let start = Instant::now();
            loop {
                tokio::time::sleep(MENU_POLL).await;
                next = match read().await {
                    Ok(next) => next,
                    Err(error) => {
                        self.close_menus(surface, pid, window).await;
                        return Err(error);
                    }
                };
                if !same_move(&done, &next) || start.elapsed() >= MENU_CLOSE {
                    break;
                }
            }
        }
        self.close_menus(surface, pid, window).await;
        Err(IbaraError::new("BLOCKED_BY_DIALOG", "The menu never showed the item highlighted, so nothing was chosen; the menu was closed again.", true))
    }

    /// Presses of `key` into the window's open menu through Hyprland, which
    /// skips Cua's own checks. So, with nothing sent, they are refused as
    /// Cua refuses input while the person has the screen, and unless
    /// `surface` still has the keyboard focus.
    async fn menu_keys(&self, surface: &SurfaceId, key: &str, count: usize) -> Result<()> {
        self.cua.screen_held()?;
        self.ensure_focused(surface).await?;
        self.hypr.menu_keys(key, count).await
    }

    /// Close the window's open menus, as far as Escape does.
    async fn close_menus(&self, surface: &SurfaceId, pid: i64, window: u64) {
        if let Ok(open) = self.cua.open_menus(pid, window).await
            && open > 0
        {
            let _ = self.menu_keys(surface, "Escape", open).await;
        }
    }

    /// Press at `from`, move to `to` over `duration` (50..3000 ms) and release,
    /// in the window under `from`. Cua releases the button itself.
    pub async fn drag(&self, from: Point, to: Point, duration: Duration, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            let duration = duration.clamp(Duration::from_millis(50), Duration::from_millis(3000));
            let (window, fx, fy, screens) = self.window_at(from.x, from.y, None).await?;
            if !screens.show(to.x, to.y) {
                return Err(invalid("No screen shows the point to drag to.").with("field", "to"));
            }
            if self.uses_dispatchers().await {
                self.prepare_dispatchers().await?;
                return self.compositor_input.drag(&window, (from.x,from.y), (to.x,to.y), &screens, duration, cancel).await;
            }
            self.cua.lead(from.x, from.y, &screens).await;
            cua::unless_cancelled(cancel)?;
            let (tx, ty) = (to.x - window.at[0] as f64, to.y - window.at[1] as f64);
            self.cua.drag(window.pid, cua::window_id(&window.address)?, (fx, fy), (tx, ty), duration, cancel).await
        })
        .await
    }

    /// Wheel notches (−50..50 each) into the window at `at`, else the focused one.
    pub async fn scroll(&self, at: Option<Point>, dx: i32, dy: i32, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            if !(-50..=50).contains(&dx) || !(-50..=50).contains(&dy) {
                return Err(invalid("Scroll notches must be between -50 and 50.").with("field", "dx/dy"));
            }
            let window = match at {
                Some(at) => self.window_at(at.x, at.y, None).await?.0,
                None => self.hypr.active_window().await?.ok_or_else(|| stale("No window has focus."))?,
            };
            if self.uses_dispatchers().await {
                self.prepare_dispatchers().await?;
                let at = at.map(|p| (p.x,p.y)).unwrap_or((window.at[0] as f64 + window.size[0] as f64/2.0, window.at[1] as f64 + window.size[1] as f64/2.0));
                return self.compositor_input.scroll(&window,at,&self.screens().await,dx,dy,cancel).await;
            }
            self.cua.scroll(window.pid, cua::window_id(&window.address)?, dx, dy).await
        })
        .await
    }

    /// A key chord such as `ctrl+s`, into the focused surface. The
    /// surface's own open menu holds the keyboard, and Cua refuses keys
    /// meanwhile ([`Desktop::key_past_menu`]).
    pub async fn key(&self, surface: &SurfaceId, combo: &str, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            let keys = input::cua_keys(combo)?;
            let live = self.ensure_focused(surface).await?;
            self.after_typing().await;
            if self.uses_dispatchers().await {
                return self.compositor_input.key(surface,&keys,cancel).await;
            }
            let window = cua::window_id(&live.address)?;
            match self.cua.key(live.pid, window, &keys).await {
                Err(refused) if held(&refused) => self.key_past_menu(surface, live.pid, window, &keys, refused, cancel).await,
                sent => sent,
            }
        })
        .await
    }

    /// A key Cua refused because something holds the keyboard. Cua's plugin
    /// lets keys reach the window's own popups and menus, but one built
    /// before ibara 0.1.0-18 refused them too, and a loaded plugin stays
    /// until the next sign-in. When the window's own menu is open, Escape
    /// goes to the menu through Hyprland, as a person's would, and closes it
    /// (one level); a shortcut (a chord with a modifier) closes every level
    /// that way first and then goes to the window. Any other key would be
    /// meant for the menu and is refused, saying Escape closes it. Every key
    /// while something else holds the keyboard (another app's popup, a
    /// drag) stays refused, and nothing is sent.
    async fn key_past_menu(&self, surface: &SurfaceId, pid: i64, window: u64, keys: &[String], refused: IbaraError, cancel: Option<&Cancel>) -> Result<()> {
        let escape = matches!(keys, [only] if only == "Escape");
        if !escape && keys.len() == 1 {
            return Err(match self.cua.open_menus(pid, window).await {
                Ok(open) if open > 0 => IbaraError {
                    message: "This window's own menu is open and holds the keyboard, so the key was not sent: it would go to the menu. Escape closes the menu.".into(),
                    ..refused
                }
                .with("next", "Press Escape (kind key) to close the menu, then send the key again; or left-click the menu's item by its id from computer_observe."),
                _ => refused,
            });
        }
        let mut open = self.cua.open_menus(pid, window).await?;
        if open == 0 {
            return Err(refused);
        }
        for escaped in 0.. {
            self.menu_keys(surface, "Escape", 1).await.map_err(|e| if escaped > 0 { e.with("execution_not_started", false) } else { e })?;
            let was = open;
            open = self.menus_after_escape(pid, window, was).await;
            if escape || open == 0 {
                break;
            }
            if open >= was {
                return Err(IbaraError::new("BLOCKED_BY_DIALOG", "The window's menu did not close on Escape, so the shortcut was not sent.", true)
                    .with("execution_not_started", false));
            }
        }
        if escape {
            return Ok(());
        }
        cua::unless_cancelled(cancel).map_err(|e| e.with("execution_not_started", false))?;
        // Hyprland may end the menu's hold a moment after the menu closed.
        for _ in 0..MENU_RELEASE_TRIES {
            match self.cua.key(pid, window, keys).await {
                Err(refused) if held(&refused) => tokio::time::sleep(MENU_POLL).await,
                sent => return sent.map_err(|e| e.with("execution_not_started", false)),
            }
        }
        Err(refused.with("execution_not_started", false))
    }

    /// The number of the window's open menus once fewer than `was` are
    /// open, else after [`MENU_CLOSE`].
    async fn menus_after_escape(&self, pid: i64, window: u64, was: usize) -> usize {
        let start = Instant::now();
        loop {
            let open = self.cua.open_menus(pid, window).await.unwrap_or(was);
            if open < was || start.elapsed() >= MENU_CLOSE {
                return open;
            }
            tokio::time::sleep(MENU_POLL).await;
        }
    }

    /// Type text into the focused surface. ASCII goes through Cua's
    /// exact-target keyboard in short pieces that are never cancelled once
    /// started, each only while the surface still has the keyboard focus;
    /// other text is inserted through accessibility into the window's only
    /// editable element. With [`TypingCursor::ToField`] the agent's named
    /// cursor first glides to the field when Cua proves its box
    /// ([`cua::Cua::to_field`]); otherwise it stays. Cua shows its typing
    /// animation on the named cursor while the text goes in.
    pub async fn type_text(&self, surface: &SurfaceId, text: &str, cursor: TypingCursor, cancel: Option<&Cancel>) -> Result<()> {
        self.effect(cancel, async {
            input::check_text(text)?;
            let live = self.ensure_focused(surface).await?;
            let window = cua::window_id(&live.address)?;
            let to_field = cursor == TypingCursor::ToField && self.cua.cursor_shown();
            let dispatchers = self.uses_dispatchers().await;
            let ascii = text.is_ascii() && (!dispatchers || self.compositor_input.ascii_keyboard().await?);
            let result = if ascii {
                if to_field && let Some(field) = self.cua.typing_field(live.pid, window).await {
                    self.cua.to_field(&field, live.geometry(), &self.screens().await).await;
                }
                if dispatchers {
                    self.compositor_input.type_ascii(surface,text,cancel).await
                } else {
                    self.cua.type_ascii(live.pid, window, text, cancel, async || self.ensure_focused(surface).await.map(drop)).await
                }
            } else {
                async {
                    let field = self.cua.insert_field(live.pid, window).await?;
                    if to_field && let Some(field) = &field {
                        self.cua.to_field(field, live.geometry(), &self.screens().await).await;
                        cua::unless_cancelled(cancel)?;
                    }
                    self.cua.type_semantic(live.pid, window, field.as_ref(), text).await
                }
                .await
            };
            *self.typed_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            result
        })
        .await
    }

    /// Before a point click: put the real pointer where the named cursor is,
    /// so both travel the same way in the same time and the press comes after
    /// the named cursor has arrived. Only within the target window (crossing
    /// another window would move keyboard focus), and only while the agent
    /// has the screen.
    async fn point_from_cursor(&self, window: &Window) {
        let g = window.geometry();
        if let Some((x, y)) = self.cua.cursor_at()
            && x > g.x as f64 && y > g.y as f64 && x < (g.x + g.width) as f64 && y < (g.y + g.height) as f64
        {
            self.handover.pointer_to(x, y).await;
        }
    }

    /// Wait up to 2.5 s for launched helpers and for Cua's action in flight
    /// (a typing piece is never cancelled; Cua releases what it pressed).
    /// Once no agent holds control, the screen goes back to the person's
    /// pointer first, or, while Cua's call was in flight, once it has ended.
    pub async fn release_input(&self) -> Result<()> {
        let dispatcher_release = self.compositor_input.release().await;
        if !self.cua.has_agent() {
            self.handover.release().await;
        }
        let helpers = run::wait_for_helpers(run::HELPER_SETTLE).await;
        let cua = self.cua.settle(cua::SETTLE).await;
        if !self.cua.has_agent() {
            self.handover.release().await;
        }
        dispatcher_release.and(helpers).and(cua)
    }

    /// Stop the Cua worker (startup, shutdown), which takes the named cursor
    /// with it, and draw Hyprland's pointer again. The Hyprland plugin
    /// releases anything it held when the worker's connection closes.
    pub async fn reset_input(&self) -> Result<()> {
        let dispatcher_release = self.compositor_input.release().await;
        self.cua.stop().await;
        self.handover.reset().await;
        dispatcher_release
    }

    /// Keep the screen awake while an agent works (§12.1).
    pub async fn set_idle_inhibited(&self, active: bool) -> Result<()> {
        self.idle.set_inhibited(active).await
    }

    /// Keep this computer awake for good; whether stay-awake was turned on now.
    pub async fn keep_awake(&self) -> Result<bool> {
        self.idle.keep_awake().await
    }

    /// The headless fallback change (§12.3) the outputs need: remove
    /// `IbaraVirtual` when a physical output exists, create it when none does.
    pub async fn output_change(&self) -> Result<Option<OutputChange>> {
        Ok(OutputChange::needed(&self.hypr.monitors_all().await?))
    }

    /// Create or remove `IbaraVirtual`. Maintenance only.
    pub async fn apply_output_change(&self, change: OutputChange) -> Result<()> {
        match change {
            OutputChange::Remove => {
                self.hypr.output(&["remove", hyprland::VIRTUAL_OUTPUT]).await?;
            }
            OutputChange::Create => {
                self.hypr.output(&["create", "headless", hyprland::VIRTUAL_OUTPUT]).await?;
                self.hypr.eval(&virtual_monitor(&virtual_size())).await?;
            }
        }
        Ok(())
    }

    /// Give an existing `IbaraVirtual` the size in settings; whether it changed.
    pub async fn resize_virtual(&self) -> Result<bool> {
        let size = virtual_size();
        let Some((width, height)) = size.split_once('x').and_then(|(w, h)| Some((w.parse::<i64>().ok()?, h.parse::<i64>().ok()?))) else {
            return Ok(false);
        };
        let monitors = self.hypr.monitors_all().await?;
        let Some(current) = monitors.iter().find(|m| m.name == hyprland::VIRTUAL_OUTPUT && !m.disabled) else { return Ok(false) };
        if (current.width, current.height) == (width, height) {
            return Ok(false);
        }
        self.hypr.eval(&virtual_monitor(&size)).await?;
        Ok(true)
    }

    // ---- capture ----

    /// A crop of one window as WebP or JPEG within `budget`.
    pub async fn capture_surface(&self, surface: &SurfaceId, budget: &ImageBudget) -> Result<EncodedImage> {
        let (monitors, windows) = tokio::join!(self.hypr.monitors(), self.hypr.clients());
        let monitors = monitors?;
        let window = windows?.into_iter().find(|w| w.is(surface)).ok_or_else(|| stale("Surface is gone."))?;
        if !window.on_screen(&monitors) {
            return Err(not_on_screen());
        }
        let screen = monitors
            .iter()
            .find(|m| m.id == window.monitor)
            .map(Monitor::logical_rect)
            .unwrap_or(window.geometry());
        let rect = window.geometry().intersect(&screen).ok_or_else(not_on_screen)?;
        let scale = monitors.iter().map(|m| m.scale).fold(1.0f64, f64::max);
        self.capture(capture::Source::Region(rect), rect, scale, budget).await
    }

    /// A crop of a logical rectangle as WebP or JPEG within `budget`.
    pub async fn capture_region(&self, rect: Rect, budget: &ImageBudget) -> Result<EncodedImage> {
        let scale = self.hypr.monitors().await?.iter().map(|m| m.scale).fold(1.0f64, f64::max);
        self.capture(capture::Source::Region(rect), rect, scale, budget).await
    }

    /// The whole focused output, only when an agent asks for the screen.
    pub async fn capture_screen(&self, budget: &ImageBudget) -> Result<EncodedImage> {
        let monitors = self.hypr.monitors().await?;
        let monitor = monitors
            .iter()
            .find(|m| m.focused)
            .or_else(|| monitors.first())
            .ok_or_else(|| IbaraError::new("SESSION_UNAVAILABLE", "No Hyprland output is available.", true))?;
        let rect = monitor.logical_rect();
        let scale = if monitor.scale > 0.0 { monitor.scale } else { 1.0 };
        self.capture(capture::Source::Output(&monitor.name), rect, scale, budget).await
    }

    async fn capture(&self, source: capture::Source<'_>, region: Rect, scale: f64, budget: &ImageBudget) -> Result<EncodedImage> {
        let started = Instant::now();
        let captured_at = crate::ids::now_iso();
        let expected = (region.width.max(0) as f64 * scale).ceil() as u64 * (region.height.max(0) as f64 * scale).ceil() as u64;
        let frame = capture::grab(&self.cfg.grim, &self.env, source, expected, SURFACE_CAPTURE_TIMEOUT, None).await?;
        let budget = *budget;
        let encoded = tokio::task::spawn_blocking(move || {
            let encoded = capture::encode_within(frame.pixels(), frame.width, frame.height, &budget);
            encoded.map(|e| (e, frame.width, frame.height))
        })
        .await
        .map_err(|e| internal(format!("image encoding task: {e}")))??;
        let (encoded, source_width, source_height) = encoded;
        Ok(EncodedImage {
            bytes: encoded.bytes,
            format: encoded.format,
            width: encoded.width,
            height: encoded.height,
            source_width,
            source_height,
            region,
            captured_at,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// FNV-1a hash (hex) of a logical region's pixels, for change detection.
    pub async fn region_hash(&self, rect: Rect) -> Result<String> {
        let scale = self.hypr.monitors().await?.iter().map(|m| m.scale).fold(1.0f64, f64::max);
        let expected = (rect.width.max(0) as f64 * scale).ceil() as u64 * (rect.height.max(0) as f64 * scale).ceil() as u64;
        let source = capture::Source::Region(rect);
        let frame = capture::grab(&self.cfg.grim, &self.env, source, expected, SURFACE_CAPTURE_TIMEOUT, None).await?;
        Ok(format!("{:x}", capture::fnv1a32(frame.pixels())))
    }

    /// Outputs the operator can preview (`operatorOutputs`, `server.ts:144-150`).
    pub async fn outputs(&self) -> Result<Vec<DisplayInfo>> {
        let monitors = self
            .hypr
            .monitors_quick()
            .await
            .map_err(|_| IbaraError::new("CAPABILITY_UNAVAILABLE", "Display discovery failed.", true))?;
        Ok(monitors
            .iter()
            .filter(|m| !m.name.is_empty() && m.name.chars().count() <= 128 && m.width > 0 && m.height > 0 && !m.disabled)
            .take(32)
            .map(|m| DisplayInfo {
                display_id: m.name.clone(),
                label: m.name.clone(),
                display_revision: hyprland::display_revision(m),
            })
            .collect())
    }

    /// An operator preview of one output (§11.1): PNG or JPEG within the
    /// quality's bounds. At most one capture runs at a time; a request for the
    /// same display, quality and format joins it, any other is `BUDGET_EXCEEDED`
    /// (busy). The whole capture has a 2 s deadline.
    pub async fn preview(&self, display: &str, quality: PreviewQuality, format: PreviewFormat) -> Result<Arc<Preview>> {
        let key = (display.to_string(), quality, format);
        let (guard, joined) = {
            let mut slot = self.preview.lock().unwrap_or_else(|p| p.into_inner());
            match &*slot {
                Some(flight) if flight.key == key => (None, Some(flight.rx.clone())),
                Some(_) => {
                    return Err(IbaraError::new("BUDGET_EXCEEDED", "Target preview capture busy.", true));
                }
                None => {
                    let (tx, rx) = tokio_watch::channel(None);
                    *slot = Some(PreviewFlight { key, rx });
                    (Some(FlightGuard { slot: &self.preview, tx: Some(tx), cancel: Cancel::new() }), None)
                }
            }
        };
        if let Some(mut rx) = joined {
            return match rx.wait_for(Option::is_some).await {
                Ok(result) => result.clone().unwrap_or_else(|| Err(internal("Preview finished without a result."))),
                Err(_) => Err(IbaraError::new("TIMEOUT", "Preview capture was abandoned.", true)),
            };
        }
        let Some(guard) = guard else {
            return Err(internal("Preview flight missing."));
        };
        let result = match tokio::time::timeout(PREVIEW_DEADLINE, self.preview_once(display, quality, format, &guard.cancel)).await {
            Ok(result) => result.map(Arc::new),
            Err(_) => {
                guard.cancel.cancel();
                Err(IbaraError::new("TIMEOUT", "Preview deadline elapsed.", true))
            }
        };
        guard.finish(result.clone());
        result
    }

    async fn preview_once(&self, display: &str, quality: PreviewQuality, format: PreviewFormat, cancel: &Cancel) -> Result<Preview> {
        if display.is_empty() || display.chars().count() > 128 || display.starts_with('-') {
            return Err(invalid("Invalid display_id.").with("field", "display_id"));
        }
        if !graphical_env() {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "The graphical session is unavailable.", true)
                .with("reason", "locked"));
        }
        let discovery = || IbaraError::new("CAPABILITY_UNAVAILABLE", "Display discovery failed.", true);
        let monitors = self.hypr.monitors_quick().await.map_err(|_| discovery())?;
        idle::require_unlocked(Ok(monitors.clone()))?;
        let monitor = monitors
            .iter()
            .find(|m| m.name == display && m.width > 0 && m.height > 0)
            .ok_or_else(|| {
                IbaraError::new("CAPABILITY_UNAVAILABLE", "Named display unavailable.", true).with("reason", "no_display")
            })?;
        let revision = hyprland::display_revision(monitor);
        let pixels = (monitor.width * monitor.height) as u64;
        if pixels > capture::MAX_PIXELS {
            return Err(IbaraError::new("BUDGET_EXCEEDED", "Preview source exceeds geometry limit.", true));
        }
        let captured_at = crate::ids::now_iso();
        let source = capture::Source::Output(display);
        let frame = capture::grab(&self.cfg.grim, &self.env, source, pixels, PREVIEW_DEADLINE, Some(cancel)).await?;
        let after = self.hypr.monitors_quick().await.map_err(|_| discovery())?;
        if after.iter().find(|m| m.name == display).map(hyprland::display_revision) != Some(revision.clone()) {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Display changed during capture.", true)
                .with("reason", "stale_display"));
        }
        let (source_width, source_height) = (frame.width, frame.height);
        let encoded =
            tokio::task::spawn_blocking(move || capture::encode_preview(frame.pixels(), frame.width, frame.height, quality, format))
                .await
                .map_err(|e| internal(format!("preview encoding task: {e}")))??;
        Ok(Preview {
            picture: encoded.bytes,
            width: encoded.width,
            height: encoded.height,
            source_width,
            source_height,
            display_revision: revision,
            captured_at,
        })
    }

    /// The cached render node proven to encode H.264.
    pub fn vaapi_render_node(&self) -> Option<PathBuf> {
        self.video.encoder_node()
    }

    /// Whether this computer streams live video. Until the background probe
    /// finishes, the answer says it is still checking.
    pub fn video_capability(&self) -> video::VideoCapability {
        if let Some(known) = self.video.capability() {
            return known;
        }
        if graphical_env() && self.video.begin_probe() {
            let (hypr, video) = (self.hypr.clone(), self.video.clone());
            tokio::spawn(async move {
                if let Ok(display) = video_display(&hypr, None).await {
                    let _ = video.probe(&display).await;
                }
                video.end_probe();
            });
        }
        video::Video::checking()
    }

    /// Live video of `display` (else the first display) from `cursor`, on
    /// `viewer`'s own encoder under `access`; see [`video`].
    pub async fn observe_video(&self, viewer: &str, access: &str, display: Option<&str>, width: u32, height: u32, cursor: Option<u64>) -> Result<video::Chunk> {
        let display = video_display(&self.hypr, display).await?;
        self.video.observe(&video::Watch { viewer, access, display: &display, width, height }, cursor).await
    }

    /// Stop every live video encoder (lock, Take Control).
    pub fn stop_video(&self) {
        self.video.stop_all();
    }

    /// Capability rows (`native.ts:321-348`), probed now.
    pub async fn capabilities(&self) -> Vec<Capability> {
        let tested = crate::ids::now_iso();
        let row = |name: &'static str, backend: String, outcome: Result<bool>| {
            let (status, reason) = match outcome {
                Ok(true) => ("available", None),
                Ok(false) => ("unavailable", Some("Probe failed.".to_string())),
                Err(error) => ("unavailable", Some(clip_string(&error.message, 1000))),
            };
            Capability { name, status, backend, last_tested_at: tested.clone(), reason }
        };
        let (monitors, plugin, hypoland, idle_status) = tokio::join!(self.hypr.monitors(), self.hypr.cua_status(), self.hypr.hypoland_version(), self.idle.status());
        let hypr_ok = monitors.as_ref().map(|m| !m.is_empty()).map_err(Clone::clone);
        let cua_present = on_path(&self.cfg.cua);
        let input = if self.uses_dispatchers().await {
            let compositor = hypoland.ok().flatten().map(|v| format!("Hypoland {v}, without plugin support. ")).unwrap_or_default();
            let ready = self.prepare_dispatchers().await;
            let keyboard = self.hypr.devices().await.map(|d| d["keyboards"].as_array().is_some_and(|ks| ks.iter().any(|k| k["main"] == true)));
            let keyboard_reason = match &keyboard {
                Ok(true) => String::new(),
                Ok(false) => " No seat keyboard: dispatcher keys are unavailable.".into(),
                Err(error) => format!(" The seat keyboard could not be checked: {}",error.message),
            };
            Capability {
                name: "native.input",
                status: if ready.is_ok() && cua_present && matches!(keyboard, Ok(true)) { "available" } else { "unavailable" },
                backend: "cua-driver+compositor-dispatchers".into(),
                last_tested_at: tested.clone(),
                reason: Some(format!("{compositor}{}{keyboard_reason}{}", compositor_input::SAFETY, ready.err().map(|e| format!(" {}",e.message)).unwrap_or_else(|| if cua_present { String::new() } else { " cua-driver is missing.".into() }))),
            }
        } else {
            let plugin = if let Some(version) = hypoland.ok().flatten() {
                Err(IbaraError::new("CAPABILITY_UNAVAILABLE", format!("Hypoland {version} has no plugin support. Select Automatic or Compositor Dispatchers for agent input."),true))
            } else { plugin.map(|ready| ready && cua_present) };
            row("native.input", "cua-driver+hyprland-plugin".into(), plugin)
        };
        vec![
            row("hyprland", "hyprctl".into(), hypr_ok.clone()),
            row("native.hyprland", "hyprctl".into(), hypr_ok),
            row("session", "hyprland-session-lock".into(), idle::require_unlocked(monitors).map(|()| true)),
            row("native.capture", "grim".into(), Ok(on_path(&self.cfg.grim))),
            input,
            row("native.atspi", "cua-driver".into(), Ok(cua_present)),
            row("native.idle", self.cfg.idle_binary.display().to_string(), idle_status.map(|_| true)),
        ]
    }
}

fn clip_string(text: &str, max: usize) -> String {
    run::clip(text, max).to_string()
}

/// The virtual screen size from this computer's settings (`WIDTHxHEIGHT`).
fn virtual_size() -> String {
    crate::settings::current().text("virtual_display_size").unwrap_or_else(|| "1920x1080".into())
}

/// The Hyprland call that gives `IbaraVirtual` its mode; `size` is one of
/// [`crate::settings::DISPLAY_SIZES`], so it is plain digits and one `x`.
fn virtual_monitor(size: &str) -> String {
    format!(r#"hl.monitor({{output="{}",mode="{size}@30",position="0x0",scale=1}})"#, hyprland::VIRTUAL_OUTPUT)
}

/// Cua refused the input because a menu, popup or drag holds the keyboard.
fn held(refused: &IbaraError) -> bool {
    refused.details.get("reason").and_then(Value::as_str) == Some("grab")
}

/// An item of a menu (not a submenu or a top-level menu), as the frame
/// showed it: its parent is a menu.
fn menu_item(element: &ElementTarget) -> bool {
    let in_menu = element.selector["identity"]["path"].as_array().and_then(|path| path.last()).is_some_and(|parent| parent[0] == "menu");
    in_menu && matches!(element.role.as_str(), "menu item" | "check menu item" | "radio menu item")
}

/// Whether a fresh read still asks for the move just made, so the menu has
/// not shown it yet: the same menu to open (its token is new each read), a
/// highlight still on its way, or the same key.
fn same_move(done: &cua::MenuMove, next: &cua::MenuMove) -> bool {
    use cua::MenuMove::{Keys, Open};
    match (done, next) {
        (Open(_), Open(_)) => true,
        (Keys("Down" | "Up", _), Keys("Down" | "Up", _)) => true,
        _ => done == next,
    }
}

/// The program exists: an absolute or relative path to a file, or a bare
/// name found on `PATH`.
fn on_path(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn graphical_env() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        && std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|v| !v.is_empty())
}

/// The display to stream: the one named, else the first; refused while the
/// screen is locked.
async fn video_display(hypr: &Hyprland, named: Option<&str>) -> Result<String> {
    let monitors = hypr
        .monitors_quick()
        .await
        .map_err(|_| IbaraError::new("CAPABILITY_UNAVAILABLE", "Display discovery failed.", true))?;
    idle::require_unlocked(Ok(monitors.clone()))?;
    monitors
        .iter()
        .filter(|m| !m.disabled && m.width > 0 && m.height > 0)
        .find(|m| named.is_none_or(|n| m.name == n))
        .map(|m| m.name.clone())
        .ok_or_else(|| IbaraError::new("CAPABILITY_UNAVAILABLE", "Named display unavailable.", true).with("reason", "no_display"))
}

/// Cua's tool for a click on an element: `click` activates the element's
/// first action through accessibility; a double or right click is the
/// pointer's ([`cua::Cua::click_element`]), and a right click is offered only
/// on an element with a menu action.
fn element_tool(actions: &[String], button: Button, double: bool) -> Result<&'static str> {
    match (button, double) {
        (Button::Right, _) if actions.iter().any(|a| a.to_lowercase().contains("menu")) => Ok("right_click"),
        (Button::Right, _) => Err(invalid("This element has no menu action; right-click a point on an image instead.").with("field", "target")),
        (Button::Middle, _) => Err(invalid("Elements take left or right clicks.").with("field", "button")),
        (Button::Left, true) => Ok("double_click"),
        (Button::Left, false) => Ok("click"),
    }
}

/// The window a screen point lands on without a named surface: the focused
/// window when it contains the point and no visible floating window lies over
/// it there (floating windows stack above tiled ones), else the one visible
/// floating window, or the one tiled window, that contains it.
fn window_under(x: f64, y: f64, active: Option<Window>, windows: Vec<Window>, monitors: &[hyprland::Monitor]) -> Result<Window> {
    let contains = |w: &Window| {
        let g = w.geometry();
        x >= g.x as f64 && y >= g.y as f64 && x < (g.x + g.width) as f64 && y < (g.y + g.height) as f64
    };
    let covered = |a: &Window| !a.floating && windows.iter().any(|w| w.floating && w.address != a.address && w.on_screen(monitors) && contains(w));
    if let Some(a) = active.filter(|a| contains(a) && !covered(a)) {
        return Ok(a);
    }
    let mut hits: Vec<Window> = windows.into_iter().filter(|w| w.on_screen(monitors) && contains(w)).collect();
    hits.sort_by_key(|w| !w.floating);
    let floating = hits.iter().filter(|w| w.floating).count();
    if hits.is_empty() || (floating != 1 && hits.len() > 1) {
        return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "No single window is under that point; name the surface to act in.", true)
            .with("execution_not_started", true));
    }
    Ok(hits.swap_remove(0))
}

/// Write a test stand-in's executable from a child process. A file this
/// process writes can be open in another test's child just forked, and
/// starting the script then fails with "Text file busy".
#[cfg(test)]
pub(crate) fn write_script(path: &Path, body: &str) {
    use std::io::Write;
    let mut sh = std::process::Command::new("/bin/sh")
        .args(["-c", "cat >\"$1\" && chmod 700 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(sh.wait().unwrap().success(), "could not write {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(address: &str, floating: bool, at: [i32; 2], size: [i32; 2]) -> Window {
        Window { address: address.into(), mapped: true, floating, at, size, ..Window::default() }
    }

    #[test]
    fn a_floating_window_over_the_focused_tiled_one_takes_the_point() {
        let monitors = [hyprland::Monitor::default()];
        let tiled = window("0x1", false, [0, 0], [1000, 800]);
        let dialog = window("0x2", true, [100, 100], [300, 200]);
        let windows = vec![tiled.clone(), dialog.clone()];
        let under = |x, y| window_under(x, y, Some(tiled.clone()), windows.clone(), &monitors).map(|w| w.address);
        assert_eq!(under(150.0, 150.0).unwrap(), "0x2", "the dialog is on top there");
        assert_eq!(under(600.0, 600.0).unwrap(), "0x1", "the focused window elsewhere");
        // A floating window on a workspace no monitor shows covers nothing.
        let mut hidden = dialog.clone();
        hidden.workspace.id = 7;
        assert_eq!(window_under(150.0, 150.0, Some(tiled.clone()), vec![tiled.clone(), hidden], &monitors).unwrap().address, "0x1");
        // Two floating windows over the point: which is on top is unknown.
        let other = window("0x3", true, [120, 120], [100, 100]);
        let err = window_under(150.0, 150.0, Some(tiled), vec![windows[0].clone(), dialog, other], &monitors).unwrap_err();
        assert_eq!(err.code, "CAPABILITY_UNAVAILABLE");
    }

    #[test]
    fn the_headless_output_returns_when_only_hyprlands_placeholder_is_left() {
        let output = |name: &str, disabled: bool| Monitor { name: name.into(), disabled, ..Monitor::default() };
        let needed = |names: &[(&str, bool)]| OutputChange::needed(&names.iter().map(|(n, d)| output(n, *d)).collect::<Vec<_>>());
        assert_eq!(needed(&[]), Some(OutputChange::Create));
        assert_eq!(needed(&[("FALLBACK", false)]), Some(OutputChange::Create), "the last physical output was unplugged");
        assert_eq!(needed(&[("HDMI-A-1", true)]), Some(OutputChange::Create), "a disabled output shows nothing");
        assert_eq!(needed(&[("IbaraVirtual", false)]), None);
        assert_eq!(needed(&[("HDMI-A-1", false), ("IbaraVirtual", false)]), Some(OutputChange::Remove));
        assert_eq!(needed(&[("HDMI-A-1", false)]), None);
    }
}
