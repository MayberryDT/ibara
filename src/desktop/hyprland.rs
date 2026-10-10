//! Hyprland through `hyprctl`.
//!
//! Every call is `hyprctl -i <instance> …` (instance `0`, the oldest live
//! Hyprland), default timeout 4 s; `eval`, `output` and `keyword` count as
//! mutating. Errors: output mentioning the session, Wayland, a connection or
//! the instance is `SESSION_UNAVAILABLE`; any other failure `INTERNAL_ERROR`;
//! unparseable JSON `SESSION_UNAVAILABLE` "Hyprland <label> was not JSON.".
//!
//! Hyprland 0.56 `hyprctl eval` prints Lua errors on stdout and exits 7, so
//! the focus and close scripts are judged by their text before the exit code
//! (today's controller turned a vanished window into `INTERNAL_ERROR` there).

use super::run::{Cmd, Output, clip, run};
use crate::error::{IbaraError, Result, internal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(8);

/// Name of the headless output ibara creates when no physical output exists (§12.3).
pub const VIRTUAL_OUTPUT: &str = "IbaraVirtual";
/// Hyprland's placeholder output while no real one exists. It is not a display:
/// only `IbaraVirtual` gives a computer without one a stable output.
pub const FALLBACK_OUTPUT: &str = "FALLBACK";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

/// A `hyprctl -j monitors` entry (fields read today, plus the workspaces
/// needed to tell whether a window is on screen).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub transform: i64,
    pub focused: bool,
    pub disabled: bool,
    pub reserved: Vec<i64>,
    #[serde(default, deserialize_with = "explicit_solitary_blockers")]
    pub solitary_blocked_by: Option<Vec<String>>,
    pub active_workspace: WorkspaceRef,
    pub special_workspace: WorkspaceRef,
}

// Hyprland emits JSON null when its blocker bitmask is zero. Serde's ordinary
// Option conflates that explicit evidence with an absent/unsupported field.
fn explicit_solitary_blockers<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vec<String>>, D::Error> {
    Option::<Vec<String>>::deserialize(d).map(|value| Some(value.unwrap_or_default()))
}

impl Monitor {
    /// The monitor's rectangle in logical layout coordinates.
    pub fn logical_rect(&self) -> Rect {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let (w, h) = if self.transform % 2 == 1 { (self.height, self.width) } else { (self.width, self.height) };
        Rect {
            x: self.x as i32,
            y: self.y as i32,
            width: (w as f64 / scale).round() as i32,
            height: (h as f64 / scale).round() as i32,
        }
    }
}

/// A `hyprctl -j clients` / `activewindow` entry: one window.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Window {
    pub address: String,
    pub mapped: bool,
    pub hidden: bool,
    pub visible: bool,
    pub accepts_input: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    /// Trusted compositor content bounds for AT-SPI WINDOW coordinates.
    pub client_rect: Option<[i32; 4]>,
    pub workspace: WorkspaceRef,
    pub floating: bool,
    pub monitor: i64,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub initial_title: String,
    pub pid: i64,
    pub fullscreen: i64,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
    pub stable_id: Option<String>,
    /// Local /proc and compositor provenance, never accepted from client JSON.
    #[serde(skip_deserializing)]
    pub process_start_ticks: Option<u64>,
    #[serde(skip_deserializing)]
    pub compositor_instance: String,
}

impl Window {
    /// The identity a surface keeps while it lives: a change of
    /// class or PID at the same address is a different surface.
    pub fn id(&self) -> SurfaceId {
        SurfaceId { address: self.address.clone(), pid: self.pid, class: self.class.clone(), process_start_ticks: self.process_start_ticks, compositor_instance: self.compositor_instance.clone() }
    }
    pub fn is(&self, id: &SurfaceId) -> bool {
        id.process_start_ticks.is_some() && !id.compositor_instance.is_empty()
            && self.address == id.address && self.pid == id.pid && self.class == id.class
            && self.process_start_ticks == id.process_start_ticks && self.compositor_instance == id.compositor_instance
    }
    pub fn geometry(&self) -> Rect {
        Rect { x: self.at[0], y: self.at[1], width: self.size[0], height: self.size[1] }
    }
    /// Mapped, not hidden, and on a workspace some monitor is showing.
    pub fn on_screen(&self, monitors: &[Monitor]) -> bool {
        if !self.mapped || self.hidden {
            return false;
        }
        monitors.iter().any(|m| {
            !m.disabled
                && (m.active_workspace.id == self.workspace.id
                    || (m.special_workspace.id != 0 && m.special_workspace.id == self.workspace.id))
        })
    }
}

/// Stable identity of a window surface: Hyprland address, PID and class.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct SurfaceId {
    pub address: String,
    pub pid: i64,
    pub class: String,
    #[serde(default)]
    pub process_start_ticks: Option<u64>,
    #[serde(default)]
    pub compositor_instance: String,
}

/// A rectangle in logical layout coordinates (or pixels, where stated).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.width) as f64
            && y < (self.y + self.height) as f64
    }
}

/// What the monitors show, in logical layout coordinates: where the agent's
/// named cursor may be drawn. With none known, anywhere.
#[derive(Debug, Clone, Default)]
pub struct Screens(Vec<Rect>);

impl Screens {
    pub fn of(monitors: &[Monitor]) -> Screens {
        Screens(monitors.iter().filter(|m| !m.disabled).map(Monitor::logical_rect).filter(|r| r.width > 0 && r.height > 0).collect())
    }

    fn at(&self, x: f64, y: f64) -> Option<usize> {
        self.0.iter().position(|r| r.contains(x, y))
    }

    /// Whether a screen shows `(x, y)`.
    pub fn show(&self, x: f64, y: f64) -> bool {
        self.0.is_empty() || self.at(x, y).is_some()
    }

    /// `(x, y)`, or the nearest point a screen shows.
    pub fn nearest(&self, x: f64, y: f64) -> (f64, f64) {
        if self.show(x, y) {
            return (x, y);
        }
        let inside = |r: &Rect| (x.clamp(r.x as f64, (r.x + r.width - 1) as f64), y.clamp(r.y as f64, (r.y + r.height - 1) as f64));
        let away = |(px, py): (f64, f64)| (px - x).powi(2) + (py - y).powi(2);
        self.0.iter().map(inside).min_by(|a, b| away(*a).total_cmp(&away(*b))).unwrap_or((x, y))
    }

    /// Whether one screen shows both points, and so the straight way
    /// between them.
    pub fn one_shows(&self, a: (f64, f64), b: (f64, f64)) -> bool {
        self.0.is_empty() || self.at(a.0, a.1).is_some_and(|i| self.at(b.0, b.1) == Some(i))
    }

    /// `(x, y)` as a fraction of the width and height of the screen that
    /// shows it; `None` when no known screen does.
    pub fn fraction(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let r = &self.0[self.at(x, y)?];
        Some(((x - r.x as f64) / r.width as f64, (y - r.y as f64) / r.height as f64))
    }
}

/// One `hyprctl -j instances` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Instance {
    pub instance: String,
    pub pid: i64,
}

/// `geometryKey` (§4): the output layout a coordinate is valid for. Frames
/// bound to one key are refused under another (`DISPLAY_CHANGED`).
pub fn geometry_key(monitors: &[Monitor]) -> String {
    monitors
        .iter()
        .map(|m| format!("{}:{},{},{}x{}@{}:{}", m.name, m.x, m.y, m.width, m.height, m.scale, m.transform))
        .collect::<Vec<_>>()
        .join("|")
}

/// `display_revision` of one output (§11.1): the JSON array
/// `[name,width,height,scale,transform,x,y]`, numbers formatted as JavaScript
/// does (`1`, not `1.0`) so revisions match what the console already holds.
pub fn display_revision(m: &Monitor) -> String {
    format!(
        "[{},{},{},{},{},{},{}]",
        serde_json::Value::String(m.name.clone()),
        m.width,
        m.height,
        m.scale,
        m.transform,
        m.x,
        m.y
    )
}

/// Quote a string for Lua as today's `luaString` does.
pub fn lua_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n").replace('\r', "\\r");
    format!("'{escaped}'")
}

/// A Hyprland window address as `hyprctl` prints it: `0x` plus hex digits.
pub fn valid_address(address: &str) -> bool {
    address
        .strip_prefix("0x")
        .is_some_and(|hex| !hex.is_empty() && hex.len() <= 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn session_gone(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["session", "wayland", "connect", "no such", "instance"].iter().any(|w| lower.contains(w))
}

fn ctl_failure(out: &Output) -> IbaraError {
    let err = out.failure_text("hyprctl");
    if session_gone(&err) {
        return IbaraError::new("SESSION_UNAVAILABLE", "Hyprland session is unavailable.", true).with("detail", clip(&err, 1000));
    }
    internal(format!("hyprctl failed: {}", clip(&err, 240)))
}

/// Linux stat field 22; comm may itself contain spaces or parentheses.
pub(super) fn process_start_ticks(pid: i64) -> Option<u64> {
    if pid <= 0 { return None; }
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    raw.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// The `hyprctl` client.
#[derive(Clone, Debug)]
pub struct Hyprland {
    hyprctl: OsString,
    gnome: Option<super::gnome::Gnome>,
    instance: String,
    env: Arc<[(OsString, OsString)]>,
}

impl Hyprland {
    pub fn new(hyprctl: impl Into<OsString>, instance: impl Into<String>, env: Arc<[(OsString, OsString)]>) -> Self {
        let gnome = super::gnome::Gnome::selected(&env).then(|| super::gnome::Gnome::new(env.clone()));
        Hyprland { hyprctl: hyprctl.into(), instance: instance.into(), env, gnome }
    }

    pub fn gnome(&self) -> Option<super::gnome::Gnome> { self.gnome.clone() }

    fn cmd(&self, args: &[&str]) -> Cmd {
        let mutates = matches!(args.first(), Some(&"eval" | &"output" | &"keyword"));
        Cmd::new(&self.hyprctl)
            .arg("-i")
            .arg(&self.instance)
            .args(args.iter().copied())
            .envs(&self.env)
            .timeout(DEFAULT_TIMEOUT)
            .mutates(mutates)
    }

    async fn text(&self, cmd: Cmd) -> Result<String> {
        if self.gnome.is_some() { return Err(super::gnome::input_unavailable()); }
        let out = run(cmd).await?;
        if !out.success() {
            return Err(ctl_failure(&out));
        }
        Ok(out.stdout_text())
    }

    async fn json<T: DeserializeOwned>(&self, cmd: Cmd, label: &str) -> Result<T> {
        let raw = self.text(cmd).await?;
        serde_json::from_str(&raw).map_err(|_| {
            IbaraError::new("SESSION_UNAVAILABLE", format!("Hyprland {label} was not JSON."), true)
        })
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        if let Some(gnome) = &self.gnome { return Ok(gnome.state().await?.monitors); }
        self.json(self.cmd(&["-j", "monitors"]), "monitors").await
    }

    /// `monitors` for the operator path: 1 s and 128 KiB (§3.1, `server.ts:145-170`).
    pub async fn monitors_quick(&self) -> Result<Vec<Monitor>> {
        if let Some(gnome) = &self.gnome { return Ok(gnome.state().await?.monitors); }
        let cmd = self.cmd(&["-j", "monitors"]).timeout(Duration::from_secs(1)).max_output(128 * 1024);
        self.json(cmd, "monitors").await
    }

    /// Every output including disabled ones; falls back to `monitors`.
    pub async fn monitors_all(&self) -> Result<Vec<Monitor>> {
        match self.json(self.cmd(&["-j", "monitors", "all"]), "monitors all").await {
            Ok(monitors) => Ok(monitors),
            Err(_) => self.monitors().await,
        }
    }

    pub async fn clients(&self) -> Result<Vec<Window>> {
        if let Some(gnome) = &self.gnome { return Ok(gnome.state().await?.windows); }
        let instance = self.instance_signature().await?;
        let bound = Self { instance: instance.clone(), ..self.clone() };
        let mut windows: Vec<Window> = bound.json(bound.cmd(&["-j", "clients"]), "clients").await?;
        for window in &mut windows {
            window.process_start_ticks = process_start_ticks(window.pid);
            window.compositor_instance = instance.clone();
        }
        Ok(windows)
    }

    pub async fn devices(&self) -> Result<serde_json::Value> {
        self.json(self.cmd(&["-j", "devices"]), "devices").await
    }

    pub async fn hypoland_version(&self) -> Result<Option<String>> {
        if self.gnome.is_some() { return Ok(None); }
        let version: serde_json::Value = self.json(self.cmd(&["-j", "version"]), "version").await?;
        Ok(version["hypolandVersion"].as_str().filter(|v| !v.is_empty()).map(str::to_owned))
    }

    /// The focused window, or `None` when nothing is focused.
    pub async fn active_window(&self) -> Result<Option<Window>> {
        if let Some(gnome) = &self.gnome { let state = gnome.state().await?; return Ok(state.windows.into_iter().find(|w| Some(&w.address) == state.focused.as_ref())); }
        let instance = self.instance_signature().await?;
        let bound = Self { instance: instance.clone(), ..self.clone() };
        let value: serde_json::Value = bound.json(bound.cmd(&["-j", "activewindow"]), "activewindow").await?;
        if value.get("address").and_then(|a| a.as_str()).is_none_or(str::is_empty) {
            return Ok(None);
        }
        let mut window: Window = serde_json::from_value(value)
            .map_err(|_| IbaraError::new("SESSION_UNAVAILABLE", "Hyprland activewindow was not JSON.", true))?;
        window.process_start_ticks = process_start_ticks(window.pid);
        window.compositor_instance = instance;
        Ok(Some(window))
    }

    /// Cua's Hyprland plugin is loaded for this compositor's ABI and its input
    /// transport is open (`hyprctl -j cua:status`).
    pub async fn cua_status(&self) -> Result<bool> {
        if self.gnome.is_some() { return Err(super::gnome::input_unavailable()); }
        let out = run(self.cmd(&["-j", "cua:status"])).await?;
        if !out.success() && session_gone(&out.failure_text("hyprctl")) { return Err(ctl_failure(&out)); }
        let raw = out.stdout_text();
        let status: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
            IbaraError::new("CAPABILITY_UNAVAILABLE", "Cua's Hyprland plugin is not loaded. Sign in again after setup, or select dispatcher input in ibara's agent settings.", true)
                .with("reason", "plugin_not_loaded")
        })?;
        if status["abi"]["match"] != serde_json::json!(true) {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Cua's plugin does not match the running Hyprland. Rebuild it with ibara setup and sign in again, or select dispatcher input.",true).with("reason","plugin_abi_mismatch"));
        }
        if status["transport"]["ready"] != serde_json::json!(true) || status["state"] != serde_json::json!("input_v3_candidate") {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Cua's Hyprland plugin input transport is unavailable. Sign in again after setup, or select dispatcher input.",true).with("reason","plugin_transport_unavailable"));
        }
        Ok(true)
    }

    /// Live Hyprland instances, in the order `-i <n>` indexes them.
    pub async fn instances(&self) -> Result<Vec<Instance>> {
        if let Some(gnome) = &self.gnome { let state = gnome.state().await?; return Ok(vec![Instance { instance: state.windows.first().map(|w|w.compositor_instance.clone()).unwrap_or_else(||format!("gnome|{}",state.epoch)), pid: 0 }]); }
        let cmd = Cmd::new(&self.hyprctl).args(["-j", "instances"]).envs(&self.env).timeout(DEFAULT_TIMEOUT);
        self.json(cmd, "instances").await
    }

    /// The instance signature `-i <instance>` resolves to (for the event socket).
    pub async fn instance_signature(&self) -> Result<String> {
        let instances = self.instances().await?;
        let index: usize = self.instance.parse().unwrap_or(0);
        if let Some(found) = instances.iter().find(|i| i.instance == self.instance) {
            return Ok(found.instance.clone());
        }
        instances
            .get(index)
            .map(|i| i.instance.clone())
            .ok_or_else(|| IbaraError::new("SESSION_UNAVAILABLE", "No Hyprland instance is running.", true))
    }

    /// Run Lua through `hyprctl eval` and return its exit status and text.
    async fn eval_raw(&self, lua: &str) -> Result<(bool, String)> {
        if self.gnome.is_some() { return Err(super::gnome::input_unavailable()); }
        let out = run(self.cmd(&["eval", lua])).await?;
        let text = out.failure_text("hyprctl");
        Ok((out.success(), text))
    }

    /// `hyprctl eval <lua>`; any Lua error is a failure.
    pub async fn eval(&self, lua: &str) -> Result<String> {
        let (ok, text) = self.eval_raw(lua).await?;
        if !ok || text.to_ascii_lowercase().contains("error:") {
            if session_gone(&text) && !text.to_ascii_lowercase().contains("error:") {
                return Err(IbaraError::new("SESSION_UNAVAILABLE", "Hyprland session is unavailable.", true)
                    .with("detail", clip(&text, 1000)));
            }
            return Err(internal(format!("hyprctl eval failed: {}", clip(&text, 240))));
        }
        Ok(text)
    }

    /// `hyprctl output <args…>`, 8 s (headless fallback, §12.3).
    pub async fn output(&self, args: &[&str]) -> Result<String> {
        let mut all = vec!["output"];
        all.extend_from_slice(args);
        self.text(self.cmd(&all).timeout(OUTPUT_TIMEOUT)).await
    }

    /// Run a script that selects one window and dispatches on it; "window not
    /// found" in the output means the target vanished.
    async fn window_dispatch(&self, lua: String, address: &str, verb: &str) -> Result<()> {
        let (ok, text) = self.eval_raw(&lua).await?;
        let lower = text.to_ascii_lowercase();
        if lower.contains("not found") {
            return Err(IbaraError::new("STALE_TARGET", format!("Window is no longer present for {verb}."), true)
                .with("address", address));
        }
        if !ok || lower.contains("error:") {
            if session_gone(&text) && !lower.contains("error:") {
                return Err(IbaraError::new("SESSION_UNAVAILABLE", "Hyprland session is unavailable.", true)
                    .with("detail", clip(&text, 1000)));
            }
            return Err(internal(format!("Window {verb} failed: {}", clip(&text, 240))));
        }
        Ok(())
    }

    /// Focus a window by address (Hyprland 0.56 Lua dispatch, §3.1).
    pub async fn focus_address(&self, address: &str) -> Result<()> {
        if !valid_address(address) {
            return Err(IbaraError::new("STALE_TARGET", "Window is no longer present for focus.", true).with("address", address));
        }
        let lua = format!(
            "local target; for _, w in ipairs(hl.get_windows()) do if w.address == {} then target = w break end end; \
             if not target then error('window not found') end; hl.dispatch(hl.dsp.focus({{ window = target }}))",
            lua_string(address)
        );
        self.window_dispatch(lua, address, "focus").await
    }

    /// Refuse stale/unknown process identity and bind effects to the observed
    /// compositor signature, never its replaceable numeric selection index.
    async fn surface_session(&self, surface: &SurfaceId) -> Result<Self> {
        if !valid_address(&surface.address) || surface.compositor_instance.is_empty()
            || !self.instances().await?.iter().any(|i| i.instance == surface.compositor_instance)
            || !surface.process_start_ticks.is_some_and(|start| Some(start) == process_start_ticks(surface.pid)) {
            return Err(IbaraError::new("STALE_TARGET", "Window process or compositor identity changed or is unavailable.", true)
                .with("execution_not_started", true));
        }
        Ok(Self { instance: surface.compositor_instance.clone(), ..self.clone() })
    }

    /// Graceful close with process incarnation/session guard, then an atomic
    /// compositor-side address/PID/class check. Never fall back to a new session.
    pub async fn close_surface(&self, surface: &SurfaceId) -> Result<()> {
        if let Some(gnome) = &self.gnome { return gnome.mutate("Close", surface).await; }
        let bound = self.surface_session(surface).await?;
        let lua = format!(
            "local target; for _, w in ipairs(hl.get_windows()) do if w.address == {} and w.pid == {} and w.class == {} then target = w break end end; \
             if not target then error('window not found') end; hl.dispatch(hl.dsp.window.close({{ window = target }}))",
            lua_string(&surface.address), surface.pid, lua_string(&surface.class)
        );
        bound.window_dispatch(lua, &surface.address, "close").await
    }

    /// Move a window to workspace `workspace` without following it (the
    /// `movetoworkspacesilent` of Hyprland's string dispatchers, in 0.56's Lua
    /// as `hl.dsp.window.move({ workspace, follow = false })`). The window
    /// must still be the same process: address **and** PID must match.
    pub async fn move_surface(&self, surface: &SurfaceId, workspace: i64) -> Result<()> {
        let bound = self.surface_session(surface).await?;
        let lua = format!(
            "local target; for _, w in ipairs(hl.get_windows()) do if w.address == {} and w.pid == {} and w.class == {} then target = w break end end; \
             if not target then error('window not found') end; hl.dispatch(hl.dsp.window.move({{ window = target, workspace = {workspace}, follow = false }}))",
            lua_string(&surface.address), surface.pid, lua_string(&surface.class)
        );
        bound.window_dispatch(lua, &surface.address, "move").await
    }

    /// `count` presses of `key` (an xkb name such as `Down` or `Escape`)
    /// into whatever holds the keyboard now, through Hyprland: an open
    /// menu, which Cua's plugin sends no keys past (see `Desktop::key`).
    pub async fn menu_keys(&self, key: &str, count: usize) -> Result<()> {
        let press = format!("hl.dispatch(hl.dsp.send_shortcut({{ mods = '', key = {} }}))", lua_string(key));
        self.eval(&vec![press; count.max(1)].join("; ")).await.map(drop)
    }

    /// Move the pointer to logical layout coordinates.
    pub async fn move_cursor(&self, x: f64, y: f64) -> Result<()> {
        let lua = format!("hl.dispatch(hl.dsp.cursor.move({{ x = {}, y = {} }}))", x.round() as i64, y.round() as i64);
        let (ok, text) = self.eval_raw(&lua).await?;
        if !ok || text.to_ascii_lowercase().contains("error:") {
            return Err(internal(format!("Cursor move failed: {}", clip(&text, 240))));
        }
        Ok(())
    }

    /// The person's own pointer settings the handover changes: whether the
    /// pointer is hidden for good (`cursor:invisible`), and after how many
    /// seconds still it hides (`cursor:inactive_timeout`, 0 never).
    pub async fn pointer_settings(&self) -> Result<(bool, f64)> {
        let option = |name: &'static str| async move {
            let text = self.text(self.cmd(&["-j", "getoption", name])).await?;
            serde_json::from_str::<serde_json::Value>(&text).map_err(|e| internal(format!("getoption {name}: {e}")))
        };
        let (invisible, timeout) = tokio::join!(option("cursor:invisible"), option("cursor:inactive_timeout"));
        let timeout = timeout?.get("float").and_then(serde_json::Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0).unwrap_or(0.0);
        Ok((invisible?.get("bool").and_then(serde_json::Value::as_bool).unwrap_or(false), timeout))
    }

    /// Hide the pointer while it is still ([`POINTER_HIDE_TIMEOUT`]). Hyprland
    /// applies cursor settings on a 500 ms timer, so it goes within 0.5 s; it
    /// draws the pointer again by itself as soon as a person moves the mouse,
    /// and ibara's own pointer moves and focus changes do not show it (probed
    /// on Hyprland 0.56.2). Not input: `execution_not_started` stays exact.
    pub async fn hide_pointer(&self) -> Result<()> {
        self.cursor_eval(&format!("hl.config({{ cursor = {{ inactive_timeout = {POINTER_HIDE_TIMEOUT} }} }})")).await
    }

    /// Draw the pointer again at once: the person's own `timeout` back, and
    /// the pointer moved (to `at`, else onto itself), which Hyprland draws
    /// 8–26 ms later instead of at its next cursor tick. `visible` also
    /// clears `cursor:invisible`, which ibara 0.1.0-5 and earlier set.
    pub async fn show_pointer(&self, timeout: Option<f64>, at: Option<(f64, f64)>, visible: bool) -> Result<()> {
        let mut settings = Vec::new();
        if let Some(timeout) = timeout {
            settings.push(format!("inactive_timeout = {}", if timeout.is_finite() && timeout >= 0.0 { timeout } else { 0.0 }));
        }
        if visible {
            settings.push("invisible = false".to_string());
        }
        let config = if settings.is_empty() { String::new() } else { format!("hl.config({{ cursor = {{ {} }} }}); ", settings.join(", ")) };
        let to = match at {
            Some((x, y)) => format!("{{ x = {}, y = {} }}", x.round() as i64, y.round() as i64),
            None => "{ x = p.x, y = p.y }".into(),
        };
        self.cursor_eval(&format!("{config}local p = hl.get_cursor_pos(); hl.dispatch(hl.dsp.cursor.move({to}))")).await
    }

    async fn cursor_eval(&self, lua: &str) -> Result<()> {
        let out = run(self.cmd(&["eval", lua]).mutates(false)).await?;
        let text = out.failure_text("hyprctl");
        if !out.success() || text.to_ascii_lowercase().contains("error:") {
            return Err(internal(format!("hyprctl eval failed: {}", clip(&text, 240))));
        }
        Ok(())
    }
}

/// `cursor:inactive_timeout` while an agent holds the screen, in seconds.
pub const POINTER_HIDE_TIMEOUT: f64 = 0.1;

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, scale: f64) -> Monitor {
        Monitor { name: name.into(), width: 1920, height: 1080, scale, ..Monitor::default() }
    }

    #[test]
    fn revisions_format_numbers_like_javascript() {
        assert_eq!(display_revision(&monitor("HDMI-A-1", 1.0)), r#"["HDMI-A-1",1920,1080,1,0,0,0]"#);
        assert_eq!(geometry_key(&[monitor("eDP-1", 1.25)]), "eDP-1:0,0,1920x1080@1.25:0");
    }

    #[test]
    fn lua_quoting_cannot_break_out_of_the_string() {
        assert_eq!(lua_string("a'b\\c\nd"), r"'a\'b\\c\nd'");
        assert!(valid_address("0x612017a016f0"));
        assert!(!valid_address("0x61' end; os.exit() --"));
        assert!(!valid_address("612017a016f0"));
    }

    #[test]
    fn windows_on_hidden_workspaces_are_not_on_screen() {
        let mut m = monitor("eDP-1", 1.0);
        m.active_workspace.id = 2;
        let mut w = Window { mapped: true, ..Window::default() };
        w.workspace.id = 2;
        assert!(w.on_screen(std::slice::from_ref(&m)));
        w.workspace.id = -95; // special:minimized, not shown
        assert!(!w.on_screen(std::slice::from_ref(&m)));
        m.special_workspace.id = -95;
        assert!(w.on_screen(std::slice::from_ref(&m)));
    }

    #[test]
    fn rotated_and_scaled_outputs_have_logical_size() {
        let mut m = monitor("DP-1", 2.0);
        m.transform = 1;
        assert_eq!(m.logical_rect(), Rect { x: 0, y: 0, width: 540, height: 960 });
    }
}
