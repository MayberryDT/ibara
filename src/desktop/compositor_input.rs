//! Optional plugin-free input for Hypoland. Pointer motion uses dispatchers,
//! never virtual-pointer motion (which would show the person's pointer).
//! The persistent virtual pointer supplies only the seat device and wheel.
//! Focus and pointer position are checked in each dispatcher request. Window
//! geometry is checked outside the compositor; surface/grab checks and real
//! keyboard interruption remain the plugin's stronger guarantees.

use super::{
    Button, Cancel, SurfaceId, Window,
    cua::Cua,
    hyprland::{Hyprland, Screens, lua_string},
};
use crate::error::{IbaraError, Result, internal};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

pub const SAFETY: &str = "Dispatcher input: reduced safety. Window geometry is checked before input; no plugin surface/grab checks or physical-keyboard interruption. Mouse movement is checked between steps.";
static DISPATCHED: AtomicU64 = AtomicU64::new(0);
pub fn dispatch_count() -> u64 {
    DISPATCHED.load(Ordering::SeqCst)
}

#[derive(Default)]
struct WheelState;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WheelState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(WheelState: ignore wl_seat::WlSeat);
delegate_noop!(WheelState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WheelState: ignore ZwlrVirtualPointerV1);

struct Wheel {
    connection: Connection,
    queue: wayland_client::EventQueue<WheelState>,
    pointer: ZwlrVirtualPointerV1,
}
impl Wheel {
    fn new() -> Result<Self> {
        let connection = Connection::connect_to_env().map_err(|e| {
            unavailable(format!(
                "The dispatcher wheel could not connect to Wayland: {e}"
            ))
        })?;
        let (globals, mut queue) = registry_queue_init::<WheelState>(&connection)
            .map_err(|e| unavailable(format!("Wayland registry: {e}")))?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|e| unavailable(format!("Wayland seat: {e}")))?;
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
            unavailable(format!(
                "The compositor has no virtual pointer for wheel input: {e}"
            ))
        })?;
        let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());
        queue
            .roundtrip(&mut WheelState)
            .map_err(|e| unavailable(format!("Wheel device: {e}")))?;
        Ok(Self {
            connection,
            queue,
            pointer,
        })
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
        // GTK ignores axes older than the preceding pointer motion. Use the
        // compositor's monotonic clock, not time since this connection began.
        let mut clock: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
            return Err(unavailable("The scroll clock is unavailable."));
        }
        let mut time = (clock.tv_sec as u64 * 1000 + clock.tv_nsec as u64 / 1_000_000) as u32;
        // Send one detent per frame. Hypoland's default discrete-scroll
        // emulation reduces a combined multi-detent frame to one notch.
        for tick in 0..dx.unsigned_abs().max(dy.unsigned_abs()) {
            for (axis, count) in [(Axis::HorizontalScroll, dx), (Axis::VerticalScroll, dy)] {
                if tick < count.unsigned_abs() {
                    self.pointer.axis_discrete(
                        time,
                        axis,
                        f64::from(count.signum()) * 15.0,
                        count.signum(),
                    );
                    self.pointer.axis_source(AxisSource::Wheel);
                }
            }
            self.pointer.frame();
            self.queue
                .roundtrip(&mut WheelState)
                .map_err(|e| unavailable(format!("Scroll input failed: {e}")))?;
            if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
                return Err(unavailable("The scroll clock is unavailable."));
            }
            time = (clock.tv_sec as u64 * 1000 + clock.tv_nsec as u64 / 1_000_000) as u32;
        }
        self.connection
            .flush()
            .map_err(|e| unavailable(format!("Wheel input failed: {e}")))
    }
}

fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", message, true).with("execution_not_started", true)
}
fn interrupted() -> IbaraError {
    IbaraError::new(
        "CAPABILITY_UNAVAILABLE",
        "A person moved the mouse; dispatcher input stopped.",
        true,
    )
    .with("reason", "interrupted")
}
fn focus_guard(id: &SurfaceId) -> String {
    format!(
        "local w=hl.get_active_window(); if not w or w.address~={} or w.pid~={} or w.class~={} then error('ibara_target_changed') end; ",
        lua_string(&id.address),
        id.pid,
        lua_string(&id.class)
    )
}
fn position_guard(at: (f64, f64)) -> String {
    format!(
        "local p=hl.get_cursor_pos(); if math.abs(p.x-{})>2 or math.abs(p.y-{})>2 then error('ibara_person_moved') end; ",
        at.0.round() as i64,
        at.1.round() as i64
    )
}
fn key_state(key: &str, mods: &str, state: &str) -> String {
    checked_dispatch(
        &format!(
            "hl.dsp.send_key_state({{mods={},key={},state={}}})",
            lua_string(mods),
            lua_string(key),
            lua_string(state)
        ),
        if state == "down" {
            "ibara_press_rejected"
        } else {
            "ibara_release_rejected"
        },
    )
}
fn checked_dispatch(dispatcher: &str, rejection: &str) -> String {
    format!(
        "do local r=hl.dispatch({dispatcher}); if not r or not r.ok then error('{rejection}: '..tostring(r and r.error)) end end; "
    )
}

pub struct CompositorInput {
    hypr: Hyprland,
    cua: Cua,
    runtime: Option<PathBuf>,
    socket: tokio::sync::Mutex<Option<PathBuf>>,
    wheel: Arc<Mutex<Option<Wheel>>>,
    held: tokio::sync::Mutex<Vec<(String, String)>>,
}
impl CompositorInput {
    pub fn new(hypr: Hyprland, cua: Cua, runtime: Option<PathBuf>) -> Self {
        Self {
            hypr,
            cua,
            runtime,
            socket: tokio::sync::Mutex::new(None),
            wheel: Arc::new(Mutex::new(None)),
            held: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Create the seat pointer before control begins, so apps on mouseless
    /// computers receive pointer focus. No motion is ever sent through it.
    pub async fn prepare(&self) -> Result<bool> {
        self.eval("assert(hl.dsp and hl.dsp.cursor and type(hl.dsp.cursor.move)=='function' and type(hl.dsp.send_key_state)=='function', 'ibara dispatchers unavailable')", false).await?;
        let wheel = self.wheel.clone();
        tokio::task::spawn_blocking(move || {
            let mut slot = wheel.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_some() {
                return Ok(false);
            }
            *slot = Some(Wheel::new()?);
            Ok(true)
        })
        .await
        .map_err(|e| internal(format!("Wheel initialization: {e}")))?
    }

    async fn request(&self, command: &str, effect: bool) -> Result<String> {
        let mut socket = self.socket.lock().await;
        if socket.is_none() {
            let runtime = self
                .runtime
                .as_ref()
                .ok_or_else(|| unavailable("No Wayland runtime directory for dispatcher input."))?;
            *socket = super::watch::socket_path(&self.hypr, runtime, ".socket.sock").await;
        }
        let path = socket
            .as_ref()
            .ok_or_else(|| unavailable("The compositor request socket is unavailable."))?;
        let request = async {
            let mut stream = tokio::net::UnixStream::connect(path).await?;
            if effect {
                DISPATCHED.fetch_add(1, Ordering::SeqCst);
            }
            stream.write_all(command.as_bytes()).await?;
            let mut bytes = Vec::new();
            stream.take(8192).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
        };
        let answer = tokio::time::timeout(Duration::from_secs(4), request).await;
        let text = match answer {
            Ok(Ok(text)) => text,
            _ => {
                *socket = None;
                return Err(IbaraError::new(
                    "SESSION_UNAVAILABLE",
                    "The compositor did not acknowledge dispatcher input.",
                    true,
                ));
            }
        };
        if text.contains("ibara_person_moved") {
            return Err(interrupted());
        }
        if text.contains("ibara_target_changed") {
            return Err(IbaraError::new(
                "STALE_TARGET",
                "The target lost focus before dispatcher input.",
                true,
            )
            .with("execution_not_started", true));
        }
        if text.to_ascii_lowercase().contains("error") || text.trim().is_empty() {
            let error = IbaraError::new(
                "CAPABILITY_UNAVAILABLE",
                format!("Dispatcher input failed: {}", super::run::clip(&text, 240)),
                true,
            );
            return Err(if !effect || text.contains("ibara_press_rejected") {
                error.with("execution_not_started", true)
            } else {
                error
            });
        }
        Ok(text)
    }

    async fn eval(&self, lua: &str, effect: bool) -> Result<String> {
        self.request(&format!("eval {lua}"), effect).await
    }

    fn ready(&self, cancel: Option<&Cancel>) -> Result<()> {
        super::cua::unless_cancelled(cancel)?;
        self.cua.screen_held()
    }

    async fn position(&self) -> Result<(f64, f64)> {
        let text = self.request("j/cursorpos", false).await?;
        let pos: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| unavailable("The compositor did not return its pointer position."))?;
        Ok((
            pos["x"]
                .as_f64()
                .ok_or_else(|| unavailable("Invalid pointer position."))?,
            pos["y"]
                .as_f64()
                .ok_or_else(|| unavailable("Invalid pointer position."))?,
        ))
    }

    async fn travel(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        let motion = self.cua.motion();
        let _moving = motion.begin();
        let start = Instant::now();
        let mut expected = from;
        loop {
            self.ready(cancel)?;
            let t = (start.elapsed().as_secs_f64() / duration.as_secs_f64().max(0.001)).min(1.0);
            let smooth = t * t * (3.0 - 2.0 * t);
            let next = (
                (from.0 + (to.0 - from.0) * smooth).round(),
                (from.1 + (to.1 - from.1) * smooth).round(),
            );
            self.eval(
                &format!(
                    "{}{}",
                    position_guard(expected),
                    checked_dispatch(
                        &format!(
                            "hl.dsp.cursor.move({{x={},y={}}})",
                            next.0 as i64, next.1 as i64
                        ),
                        "ibara_motion_rejected"
                    )
                ),
                true,
            )
            .await?;
            expected = next;
            if t >= 1.0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    }

    async fn point(
        &self,
        window: &Window,
        at: (f64, f64),
        screens: &Screens,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        self.ready(cancel)?;
        let motion = self.cua.motion();
        {
            let _moving = motion.begin();
            self.hypr.focus_address(&window.address).await?;
        }
        let from = self.position().await?;
        let duration = self
            .cua
            .pace(at.0, at.1, screens)
            .await
            .unwrap_or(Duration::ZERO);
        self.cua.move_cursor(at.0, at.1).await?;
        self.travel(from, at, duration, cancel).await?;
        tokio::time::sleep(Duration::from_millis(110)).await;
        self.check_point(window, at).await
    }

    async fn check_point(&self, window: &Window, at: (f64, f64)) -> Result<()> {
        let (windows, active, monitors) = tokio::join!(
            self.hypr.clients(),
            self.hypr.active_window(),
            self.hypr.monitors()
        );
        let live = super::window_under(at.0, at.1, active?, windows?, &monitors?)?;
        if !live.is(&window.id()) {
            return Err(unavailable(
                "A different window now covers the dispatcher input point.",
            ));
        }
        Ok(())
    }

    async fn down(&self, key: &str, guard: &str) -> Result<()> {
        self.held.lock().await.push((key.into(), String::new()));
        match self
            .eval(&format!("{guard}{}", key_state(key, "", "down")), true)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.details.get("execution_not_started") == Some(&serde_json::Value::Bool(true))
                    || e.details.get("reason").and_then(serde_json::Value::as_str)
                        == Some("interrupted")
                {
                    self.held.lock().await.clear();
                }
                Err(e)
            }
        }
    }
    pub async fn release(&self) -> Result<()> {
        let mut held = self.held.lock().await;
        if held.is_empty() {
            return Ok(());
        }
        let lua = held
            .iter()
            .map(|(k, m)| key_state(k, m, "up"))
            .collect::<String>();
        self.eval(&lua, true).await.map_err(|_| {
            IbaraError::new(
                "CONTROL_UNSETTLED",
                "Held dispatcher input could not be released.",
                false,
            )
            .requires_reconciliation()
        })?;
        held.clear();
        Ok(())
    }

    pub async fn click(
        &self,
        window: &Window,
        at: (f64, f64),
        screens: &Screens,
        button: Button,
        double: bool,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        self.point(window, at, screens, cancel).await?;
        let key = match button {
            Button::Left => "mouse:272",
            Button::Right => "mouse:273",
            Button::Middle => "mouse:274",
        };
        for i in 0..if double { 2 } else { 1 } {
            let result = async {
                self.ready(cancel)?;
                if i > 0 {
                    self.check_point(window, at).await?;
                }
                self.down(
                    key,
                    &format!("{}{}", focus_guard(&window.id()), position_guard(at)),
                )
                .await?;
                self.release().await
            }
            .await;
            if let Err(error) = result {
                return Err(if i > 0 {
                    error.with("execution_not_started", false)
                } else {
                    error
                });
            }
        }
        Ok(())
    }

    pub async fn drag(
        &self,
        window: &Window,
        from: (f64, f64),
        to: (f64, f64),
        screens: &Screens,
        duration: Duration,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        if !window.geometry().contains(to.0, to.1) {
            return Err(unavailable(
                "Dispatcher drags must end inside the same window.",
            ));
        }
        self.point(window, from, screens, cancel).await?;
        self.ready(cancel)?;
        self.down(
            "mouse:272",
            &format!("{}{}", focus_guard(&window.id()), position_guard(from)),
        )
        .await?;
        self.cua.pace(to.0, to.1, screens).await;
        let result = async {
            self.cua.move_cursor(to.0, to.1).await?;
            self.travel(from, to, duration, cancel).await
        }
        .await;
        self.release().await?;
        result.map_err(|error| error.with("execution_not_started", false))
    }

    pub async fn key(
        &self,
        surface: &SurfaceId,
        keys: &[String],
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        self.ready(cancel)?;
        let key = keys
            .last()
            .ok_or_else(|| unavailable("No dispatcher key."))?;
        let mods = keys[..keys.len() - 1].join(" ").to_ascii_uppercase();
        self.held.lock().await.push((key.clone(), mods.clone()));
        let sent = self
            .eval(
                &format!(
                    "{}{}{}",
                    focus_guard(surface),
                    key_state(key, &mods, "down"),
                    key_state(key, &mods, "up")
                ),
                true,
            )
            .await;
        if sent.is_ok()
            || sent.as_ref().is_err_and(|e| {
                e.details.get("execution_not_started") == Some(&serde_json::Value::Bool(true))
            })
        {
            self.held.lock().await.clear();
        }
        sent.map(drop)
    }

    /// ASCII translation is deliberately limited to the same plain US
    /// layout the plugin supports. Other text/layouts use Cua's AT-SPI insert.
    pub async fn ascii_keyboard(&self) -> Result<bool> {
        let devices = self.hypr.devices().await?;
        Ok(devices["keyboards"].as_array().is_some_and(|ks| {
            ks.iter().any(|k| {
                k["main"] == true
                    && k["layout"] == "us"
                    && k["variant"] == ""
                    && k["capsLock"] == false
            })
        }))
    }
    pub async fn type_ascii(
        &self,
        surface: &SurfaceId,
        text: &str,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        for (sent, ch) in text.chars().enumerate() {
            let (key, shift) = ascii(ch);
            let mut keys = Vec::new();
            if shift {
                keys.push("shift".into());
            }
            keys.push(key);
            self.key(surface, &keys, cancel).await.map_err(|e| {
                let not_started = sent == 0 && e_not_started(&e);
                e.with("characters_typed", sent)
                    .with("execution_not_started", not_started)
            })?;
            // Give the app a frame to process each key, including fields opened by typing.
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        Ok(())
    }

    pub async fn scroll(
        &self,
        window: &Window,
        at: (f64, f64),
        screens: &Screens,
        dx: i32,
        dy: i32,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        self.point(window, at, screens, cancel).await?;
        self.ready(cancel)?;
        self.eval(
            &format!(
                "{}{}return 'ok'",
                focus_guard(&window.id()),
                position_guard(at)
            ),
            false,
        )
        .await?;
        let wheel = self.wheel.clone();
        DISPATCHED.fetch_add(1, Ordering::SeqCst);
        tokio::task::spawn_blocking(move || {
            let mut slot = wheel.lock().unwrap_or_else(|p| p.into_inner());
            let result = slot
                .as_mut()
                .ok_or_else(|| unavailable("The dispatcher wheel is unavailable."))?
                .scroll(dx, dy);
            if result.is_err() {
                *slot = None;
            }
            result.map_err(|e| e.with("execution_not_started", false))
        })
        .await
        .map_err(|e| internal(format!("Wheel input: {e}")))?
    }
}

fn e_not_started(e: &IbaraError) -> bool {
    e.details.get("execution_not_started") == Some(&serde_json::Value::Bool(true))
}
fn ascii(ch: char) -> (String, bool) {
    let plain = match ch {
        '\n' | '\r' => "Return",
        '\t' => "Tab",
        ' ' => "space",
        '-' | '_' => "minus",
        '=' | '+' => "equal",
        '[' | '{' => "bracketleft",
        ']' | '}' => "bracketright",
        '\\' | '|' => "backslash",
        ';' | ':' => "semicolon",
        '\'' | '"' => "apostrophe",
        ',' | '<' => "comma",
        '.' | '>' => "period",
        '/' | '?' => "slash",
        '`' | '~' => "grave",
        _ => "",
    };
    let shifted = "!@#$%^&*()";
    if let Some(i) = shifted.find(ch) {
        return ((b"1234567890"[i] as char).to_string(), true);
    }
    if !plain.is_empty() {
        return (plain.into(), "_+{}|:\"<>?~".contains(ch));
    }
    (ch.to_ascii_lowercase().to_string(), ch.is_ascii_uppercase())
}
