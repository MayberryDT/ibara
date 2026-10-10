//! GNOME 50 identity and compositor-bound window operations. Raw portal input
//! is intentionally unavailable until its interruption contract is qualified.
use super::{hyprland::{Monitor, Window, SurfaceId, process_start_ticks}, watch::DesktopEvent};
use crate::error::{IbaraError, Result};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;
use futures_util::StreamExt;
use base64::Engine;

const SERVICE: &str = "org.ibara.Gnome";
const PATH: &str = "/org/ibara/Gnome";
static DISPATCHED: AtomicU64 = AtomicU64::new(0);
pub fn dispatch_count() -> u64 { DISPATCHED.load(Ordering::SeqCst) }

fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("SESSION_UNAVAILABLE", message, true).with("execution_not_started", true)
}
pub fn input_unavailable() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "The maintained Mutter input guard is unavailable in this GNOME session. No input was sent.", false)
        .with("execution_not_started", true).with("reason", "gnome_guard_unavailable")
}

#[derive(Clone, Debug)]
pub struct Gnome { env: Arc<[(OsString, OsString)]> }
#[derive(Deserialize)]
pub struct State {
    #[serde(default)] pub input_readiness_api: u32,
    #[serde(default)] pub shell_input_blocked: bool,
    #[serde(default)] pub capture_api: u32,
    #[serde(default)]
    pub guarded_input_api: u32,
    #[serde(default)] pub cursor_api: u32,
    #[serde(default)] pub cursor_visible: bool,
    #[serde(default)] pub last_person_us: u64,
    #[serde(default)]
    pub input_settled: Option<bool>,
    pub epoch: String,
    pub session_generation: u64,
    pub locked: bool,
    pub monitors: Vec<Monitor>,
    pub windows: Vec<Window>,
    pub focused: Option<String>,
}
impl Gnome {
    pub fn selected(env: &[(OsString, OsString)]) -> bool {
        let desktop = env.iter().rev().find(|(key, _)| key == "XDG_CURRENT_DESKTOP").map(|(_, value)| value.to_string_lossy().into_owned())
            .or_else(|| std::env::var("XDG_CURRENT_DESKTOP").ok()).unwrap_or_default();
        desktop.split(':').any(|part| part.eq_ignore_ascii_case("gnome"))
    }
    pub fn new(env: Arc<[(OsString, OsString)]>) -> Self { Self { env } }

    pub(super) async fn connection(&self) -> Result<zbus::Connection> {
        let address = self.env.iter().rev().find(|(key, _)| key == "DBUS_SESSION_BUS_ADDRESS").map(|(_, value)| value.to_string_lossy().into_owned())
            .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()).ok_or_else(|| unavailable("No graphical session bus."))?;
        let builder = zbus::connection::Builder::address(address.as_str()).map_err(|e| unavailable(format!("Session bus address: {e}")))?;
        builder.build().await.map_err(|e| unavailable(format!("Session bus unavailable: {e}")))
    }
    async fn owner(connection: &zbus::Connection, service: &str) -> Result<String> {
        let proxy = zbus::Proxy::new(connection, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").await.map_err(|e| unavailable(e.to_string()))?;
        proxy.call("GetNameOwner", &(service,)).await.map_err(|_| unavailable(format!("The {service} helper is unavailable. Enable its GNOME extension in this graphical session.")))
    }
    async fn trusted(&self) -> Result<(zbus::Connection, String)> {
        let connection = self.connection().await?;
        let shell = Self::owner(&connection, "org.gnome.Shell").await?;
        let helper = Self::owner(&connection, SERVICE).await?;
        if shell != helper { return Err(unavailable("GNOME helper is not owned by the running Shell.")); }
        Ok((connection, helper))
    }
    async fn state_on(connection: &zbus::Connection, owner: &str) -> Result<State> {
        crate::install::gnome::require_runtime_target().map_err(unavailable)?;
        let proxy = zbus::Proxy::new(connection, owner, PATH, SERVICE).await.map_err(|e| unavailable(e.to_string()))?;
        let text: String = proxy.call("GetState", &()).await.map_err(|e| unavailable(format!("GNOME state: {e}")))?;
        if text.len() > 1024 * 1024 { return Err(unavailable("GNOME state exceeds its limit.")); }
        let mut state: State = serde_json::from_str(&text).map_err(|_| unavailable("GNOME state is invalid."))?;
        if uuid::Uuid::parse_str(&state.epoch).is_err() || state.monitors.is_empty() || state.monitors.iter().any(|m|
            m.width <= 0 || m.height <= 0 || !m.scale.is_finite() || m.scale <= 0.0) {
            return Err(unavailable("GNOME session identity or display geometry is unavailable."));
        }
        let instance = format!("gnome|{owner}|{}", state.epoch);
        for window in &mut state.windows {
            if window.address.strip_prefix("gnome:").and_then(|id| id.parse::<u32>().ok()).is_none() || window.pid <= 0 {
                return Err(unavailable("GNOME returned an invalid window identity."));
            }
            window.compositor_instance = instance.clone();
            window.process_start_ticks = process_start_ticks(window.pid);
        }
        Ok(state)
    }
    pub async fn state(&self) -> Result<State> {
        tokio::time::timeout(Duration::from_secs(4), async {
            let (connection, owner) = self.trusted().await?;
            Self::state_on(&connection, &owner).await
        }).await.map_err(|_| unavailable("GNOME state timed out."))?
    }
    pub async fn settle_input(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let state = self.state().await?;
                if state.guarded_input_api != 1 || state.monitors.len()!=1 || state.monitors[0].scale!=1.0 { return Err(input_unavailable()); }
                if state.input_settled == Some(true) { return Ok(()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.map_err(|_| IbaraError::new("CONTROL_UNSETTLED", "GNOME input has not settled.", false).requires_reconciliation())?
    }
    pub async fn mutate(&self, method: &str, surface: &SurfaceId) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(4), async {
            let (connection, owner) = self.trusted().await?;
            let state = Self::state_on(&connection, &owner).await?;
            if state.locked { return Err(IbaraError::new("HUMAN_CONTROL", "The GNOME session is locked.", false)); }
            if !state.windows.iter().any(|window| window.is(surface)) {
                return Err(IbaraError::new("STALE_TARGET", "GNOME window/process/session identity changed.", true).with("execution_not_started", true));
            }
            let identity = json!({"epoch":state.epoch,"address":surface.address,"pid":surface.pid,"class":surface.class}).to_string();
            let proxy = zbus::Proxy::new(&connection, owner.as_str(), PATH, SERVICE).await.map_err(|e| unavailable(e.to_string()))?;
            DISPATCHED.fetch_add(1, Ordering::SeqCst);
            let accepted: bool = proxy.call(method, &(identity,)).await.map_err(|e| IbaraError::new("OUTCOME_UNKNOWN", format!("GNOME {method} was not acknowledged: {e}"), false).with("execution_not_started", false).requires_reconciliation())?;
            if !accepted { return Err(IbaraError::new("STALE_TARGET", "GNOME refused the requested window operation.", true)); }
            Ok(())
        }).await.map_err(|_| IbaraError::new("OUTCOME_UNKNOWN", "GNOME window operation was not acknowledged; reobserve before retrying.", false).with("execution_not_started", false).requires_reconciliation())?
    }
    pub async fn capture(&self, surface: Option<&SurfaceId>) -> Result<super::capture::Frame> {
        tokio::time::timeout(Duration::from_secs(8), async {
            let (connection, owner) = self.trusted().await?;
            let before = Self::state_on(&connection, &owner).await?;
            if before.locked { return Err(unavailable("The GNOME session is locked.")); }
            if let Some(surface) = surface {
                if !before.windows.iter().any(|w| w.is(surface)) { return Err(unavailable("The requested GNOME surface is stale.")); }
            } else if before.monitors.len() != 1 {
                return Err(unavailable("GNOME stage preview currently requires exactly one output."));
            }
            if before.capture_api != 1 || before.monitors.len()!=1 || before.monitors[0].scale!=1.0 {
                return Err(unavailable("GNOME snapshot requires the packaged capture helper and one output at scale1."));
            }
            let request = if let Some(surface) = surface {
                json!({"epoch":before.epoch,"generation":before.session_generation,"address":surface.address,"pid":surface.pid,"class":surface.class})
            } else {json!({"epoch":before.epoch,"generation":before.session_generation})}.to_string();
            let proxy = zbus::Proxy::new(&connection, owner.as_str(), PATH, SERVICE).await.map_err(|e|unavailable(e.to_string()))?;
            let encoded:String=proxy.call("Snapshot", &(request,)).await.map_err(|e|unavailable(format!("GNOME snapshot: {e}")))?;
            let after = Self::state_on(&connection, &owner).await?;
            let geometry_changed = if let Some(surface) = surface {
                let old = before.windows.iter().find(|w| w.is(surface));
                let new = after.windows.iter().find(|w| w.is(surface));
                match (old, new) {
                    (Some(old), Some(new)) => old.at != new.at || old.size != new.size || old.monitor != new.monitor,
                    _ => true,
                }
            } else {
                serde_json::to_value(&before.monitors).ok() != serde_json::to_value(&after.monitors).ok()
            };
            if geometry_changed || after.locked || after.epoch != before.epoch || after.session_generation != before.session_generation ||
                surface.is_some_and(|surface| !after.windows.iter().any(|w| w.is(surface))) {
                return Err(unavailable("GNOME session/display/surface changed during capture; pixels discarded."));
            }
            if encoded.len() > 44 * 1024 * 1024 { return Err(unavailable("GNOME capture exceeds its byte limit.")); }
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| unavailable("GNOME capture encoding is invalid."))?;
            let expected = if let Some(surface) = surface {
                let window = before.windows.iter().find(|w| w.is(surface)).ok_or_else(|| unavailable("The requested GNOME surface is stale."))?;
                (i64::from(window.size[0]), i64::from(window.size[1]))
            } else { (before.monitors[0].width, before.monitors[0].height) };
            let frame = tokio::task::spawn_blocking(move || super::capture::from_png(bytes)).await.map_err(|e| unavailable(e.to_string()))??;
            if expected.0 <= 0 || expected.1 <= 0 || frame.width != expected.0 as u32 || frame.height != expected.1 as u32 {
                return Err(unavailable("GNOME snapshot dimensions disagree with trusted geometry; pixels discarded."));
            }
            Ok(frame)
        }).await.map_err(|_| unavailable("GNOME capture timed out."))?
    }
    pub async fn watch(self, events: broadcast::Sender<DesktopEvent>) {
        loop {
            let result = async {
                let (connection, owner) = self.trusted().await?;
                let proxy = zbus::Proxy::new(&connection, owner.as_str(), PATH, SERVICE).await.map_err(|e| unavailable(e.to_string()))?;
                let mut changes = proxy.receive_signal("Changed").await.map_err(|e| unavailable(e.to_string()))?;
                let _ = events.send(DesktopEvent::Resync);
                let _ = events.send(DesktopEvent::DisplayChanged);
                let initial = Self::state_on(&connection, &owner).await?;
                loop {
                    // A helper can unexport without closing the session bus. Periodically
                    // check its owner and epoch rather than waiting forever for a signal.
                    let message = match tokio::time::timeout(Duration::from_secs(3), changes.next()).await {
                        Ok(Some(message)) => message,
                        Ok(None) => break,
                        Err(_) => {
                            if Self::owner(&connection, SERVICE).await? != owner ||
                                Self::owner(&connection, "org.gnome.Shell").await? != owner ||
                                Self::state_on(&connection, &owner).await?.epoch != initial.epoch {
                                break;
                            }
                            continue;
                        }
                    };
                    let (reason,): (String,) = message.body().deserialize().map_err(|e| unavailable(e.to_string()))?;
                    let _ = events.send(DesktopEvent::Resync);
                    if reason == "displays" || reason == "session" { let _ = events.send(DesktopEvent::DisplayChanged); }
                }
                Ok::<(), IbaraError>(())
            }.await;
            let _ = result;
            let _ = events.send(DesktopEvent::Resync);
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

/// Candidate mechanism connector. Public desktop input remains disabled until
/// full qualification. One transaction owns one connection; dropping it closes
/// that connection so Shell settles input even when a future is cancelled.
pub struct InputTransaction {
    persistent_cursor: bool,
    connection: Option<zbus::Connection>,
    owner: String,
    token: String,
}
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct InputReceipt {
    pub active: bool,
    pub settled: bool,
    #[serde(default)] pub accepted: u32,
    #[serde(default)] pub rejected: u32,
    #[serde(default)] pub held: u32,
    #[serde(default)] pub native_held: u32,
    #[serde(default)] pub reason: String,
    pub key_events: u32,
    pub button_events: u32,
    pub motion_events: u32,
    #[serde(default)] pub text_events: u32,
    #[serde(default)] pub scroll_events: u32,
}
fn input_unknown(message: impl Into<String>) -> IbaraError {
    IbaraError::new("OUTCOME_UNKNOWN", message, false)
        .with("execution_not_started", false).requires_reconciliation()
}
impl Gnome {
    pub async fn begin_input(&self, surface: &SurfaceId, generation: &str) -> Result<InputTransaction> {
        tokio::time::timeout(Duration::from_secs(4), async {
            let (connection, owner) = self.trusted().await?;
            begin_input_on(connection, owner, surface, generation, None).await
        }).await.map_err(|_| input_unknown("Input connection timed out; do not replay."))?
    }
}
async fn begin_input_on(connection: zbus::Connection, owner: String, surface: &SurfaceId,
                        generation: &str, cursor_lease: Option<&str>) -> Result<InputTransaction> {
    tokio::time::timeout(Duration::from_secs(4), async {
        let state = Gnome::state_on(&connection, &owner).await?;
        if state.monitors.len()!=1 || state.monitors[0].scale!=1.0 {
            return Err(unavailable("GNOME input supports one output at scale1 only."));
        }
        if state.guarded_input_api != 1 { return Err(input_unavailable()); }
        if state.locked { return Err(IbaraError::new("HUMAN_CONTROL", "The session is locked.", false)); }
        if !state.windows.iter().any(|window| window.is(surface)) {
            return Err(IbaraError::new("STALE_TARGET", "Window/process/session changed before input.", true)
                .with("execution_not_started", true));
        }
        let mut identity = json!({"epoch":state.epoch,"address":surface.address,"pid":surface.pid,"class":surface.class});
        if let Some(token) = cursor_lease { identity["cursor_lease"] = json!(token); }
        let token: String = {
            let proxy = zbus::Proxy::new(&connection, owner.as_str(), PATH, SERVICE).await
                .map_err(|e| unavailable(e.to_string()))?;
            DISPATCHED.fetch_add(1, Ordering::SeqCst);
            proxy.call("InputBegin", &(identity.to_string(), generation)).await
                .map_err(|e| input_unknown(format!("Input begin was not acknowledged: {e}")))?
        };
        if uuid::Uuid::parse_str(&token).is_err() { return Err(input_unknown("Invalid input transaction receipt.")); }
        Ok(InputTransaction { persistent_cursor: cursor_lease.is_some(), connection: Some(connection), owner, token })
    }).await.map_err(|_| input_unknown("Input begin timed out; do not replay."))?
}

impl InputTransaction {
    async fn proxy(&self) -> Result<zbus::Proxy<'_>> {
        let connection = self.connection.as_ref().ok_or_else(|| input_unknown("Input connection is closed."))?;
        zbus::Proxy::new(connection, self.owner.as_str(), PATH, SERVICE).await
            .map_err(|e| input_unknown(e.to_string()))
    }
    pub async fn receipt(&self) -> Result<InputReceipt> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let encoded: String = self.proxy().await?.call("InputState", &(self.token.as_str(),)).await
                .map_err(|e| input_unknown(format!("Input state unavailable: {e}")))?;
            if encoded.len() > 4096 { return Err(input_unknown("Input receipt exceeds its limit.")); }
            serde_json::from_str(&encoded).map_err(|_| input_unknown("Invalid input receipt."))
        }).await.map_err(|_| input_unknown("Input state timed out."))?
    }
    // Queue admission must be followed by the dispatch receipt. It is still not
    // application readback: callers must independently verify their outcome.
    async fn admitted(&self, previous: &InputReceipt, kind: &str) -> Result<InputReceipt> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let receipt = self.receipt().await?;
                if !receipt.active || receipt.rejected > previous.rejected {
                    return Err(IbaraError::new("HUMAN_CONTROL", "Guarded input stopped before completion.", false)
                        .with("reason", "interrupted").with("execution_not_started", false));
                }
                let advanced = match kind {
                    "key" => receipt.key_events > previous.key_events,
                    "button" => receipt.button_events > previous.button_events,
                    "motion" => receipt.motion_events > previous.motion_events,
                    "text" => receipt.text_events > previous.text_events,
                    "scroll" => receipt.scroll_events > previous.scroll_events,
                    _ => false,
                };
                if advanced { return Ok(receipt); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.map_err(|_| input_unknown("Queued input has no dispatch receipt; do not replay."))?
    }
    pub async fn key(&self, key: u32, pressed: bool) -> Result<InputReceipt> {
        let result = self.key_inner(key, pressed).await;
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    async fn key_inner(&self, key: u32, pressed: bool) -> Result<InputReceipt> {
        let previous = self.receipt().await?;
        let queued: bool = tokio::time::timeout(Duration::from_secs(2), async {
            DISPATCHED.fetch_add(1, Ordering::SeqCst);
            self.proxy().await?.call("InputKey", &(self.token.as_str(), key, pressed)).await
                .map_err(|e| input_unknown(format!("Key acknowledgement lost: {e}")))
        }).await.map_err(|_| input_unknown("Key acknowledgement timed out; do not replay."))??;
        if !queued { return Err(IbaraError::new("HUMAN_CONTROL", "Guarded key refused.", false).with("reason", "interrupted")); }
        self.admitted(&previous, "key").await
    }
    pub async fn scroll(&self, dx: i32, dy: i32) -> Result<InputReceipt> {
        if !(-50..=50).contains(&dx) || !(-50..=50).contains(&dy) {
            return Err(IbaraError::new("INVALID_ARGUMENT", "Scroll accepts -50 to 50 wheel notches per axis.", false)
                .with("execution_not_started", true));
        }
        let result = async {
            let mut receipt = self.receipt().await?;
            for (count, direction) in [(dx, if dx < 0 { 2u32 } else { 3 }), (dy, if dy < 0 { 0u32 } else { 1 })] {
                for _ in 0..count.unsigned_abs() {
                    let queued: bool = tokio::time::timeout(Duration::from_secs(2), async {
                        DISPATCHED.fetch_add(1, Ordering::SeqCst);
                        self.proxy().await?.call("InputScroll", &(self.token.as_str(), direction)).await
                            .map_err(|e| input_unknown(format!("Wheel acknowledgement lost: {e}")))
                    }).await.map_err(|_| input_unknown("Wheel acknowledgement timed out; do not replay."))??;
                    if !queued { return Err(IbaraError::new("HUMAN_CONTROL", "Guarded wheel refused.", false)
                        .with("reason", "interrupted").with("execution_not_started", false)); }
                    receipt = self.admitted(&receipt, "scroll").await?;
                }
            }
            Ok(receipt)
        }.await;
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    pub async fn text(&self, text: &str) -> Result<InputReceipt> {
        if text.is_empty() || text.len() > 4000 || text.contains('\0') {
            return Err(IbaraError::new("INVALID_ARGUMENT", "Text must contain 1–4000 UTF-8 bytes without NUL.", false)
                .with("execution_not_started", true));
        }
        let result = async {
            let previous = self.receipt().await?;
            let accepted: bool = tokio::time::timeout(Duration::from_secs(2), async {
                DISPATCHED.fetch_add(1, Ordering::SeqCst);
                self.proxy().await?.call("InputText", &(self.token.as_str(), text)).await
                    .map_err(|e| input_unknown(format!("Text acknowledgement lost: {e}")))
            }).await.map_err(|_| input_unknown("Text acknowledgement timed out; do not replay."))??;
            if !accepted {
                return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "The guarded target text context refused insertion.", false)
                    .with("execution_not_started", true));
            }
            self.admitted(&previous, "text").await
        }.await;
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    pub async fn motion(&self, x: f64, y: f64) -> Result<InputReceipt> {
        let result = self.motion_inner(x, y).await;
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    async fn motion_inner(&self, x: f64, y: f64) -> Result<InputReceipt> {
        let previous = self.receipt().await?;
        let queued: bool = tokio::time::timeout(Duration::from_secs(2), async {
            DISPATCHED.fetch_add(1, Ordering::SeqCst);
            self.proxy().await?.call("InputMotion", &(self.token.as_str(), x, y)).await
                .map_err(|e| input_unknown(format!("Motion acknowledgement lost: {e}")))
        }).await.map_err(|_| input_unknown("Motion acknowledgement timed out; do not replay."))??;
        if !queued { return Err(IbaraError::new("HUMAN_CONTROL", "Guarded motion refused.", false).with("reason", "interrupted")); }
        self.admitted(&previous, "motion").await
    }
    pub async fn button(&self, button: u32, pressed: bool) -> Result<InputReceipt> {
        let result = self.button_inner(button, pressed).await;
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    async fn button_inner(&self, button: u32, pressed: bool) -> Result<InputReceipt> {
        let previous = self.receipt().await?;
        let queued: bool = tokio::time::timeout(Duration::from_secs(2), async {
            DISPATCHED.fetch_add(1, Ordering::SeqCst);
            self.proxy().await?.call("InputButton", &(self.token.as_str(), button, pressed)).await
                .map_err(|e| input_unknown(format!("Button acknowledgement lost: {e}")))
        }).await.map_err(|_| input_unknown("Button acknowledgement timed out; do not replay."))??;
        if !queued { return Err(IbaraError::new("HUMAN_CONTROL", "Guarded button refused.", false).with("reason", "interrupted")); }
        self.admitted(&previous, "button").await
    }
    pub async fn finish(mut self) -> Result<InputReceipt> {
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let _: String = self.proxy().await?.call("InputEnd", &(self.token.as_str(),)).await
                .map_err(|e| input_unknown(format!("Input end acknowledgement lost: {e}")))?;
            loop {
                let receipt = self.receipt().await?;
                if receipt.settled { return Ok(receipt); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.map_err(|_| IbaraError::new("CONTROL_UNSETTLED", "Guarded input settlement timed out.", false).requires_reconciliation())?;
        if let Some(connection) = self.connection.take() {
            if result.is_err() || !self.persistent_cursor { let _ = connection.close().await; }
        }
        result
    }
}
impl Drop for InputTransaction {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { let _ = connection.close().await; });
            }
            // Without a runtime, dropping the final connection closes it. The
            // native expiry also bounds helper/process loss independently.
        }
    }
}

/// Connection-owned GNOME cursor lease. A paint acknowledgement precedes input;
/// closing this connection restores only this lease's visibility inhibitors.
pub struct CursorLease {
    connection: Option<zbus::Connection>,
    owner: String,
    token: String,
}
impl Gnome {
    pub async fn begin_cursor(&self, label: &str) -> Result<CursorLease> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (connection, owner) = self.trusted().await?;
            let state = Self::state_on(&connection, &owner).await?;
            if state.monitors.len()!=1 || state.monitors[0].scale!=1.0 {
                return Err(unavailable("GNOME input supports one output at scale1 only."));
            }
            if state.cursor_api != 1 || state.locked { return Err(input_unavailable()); }
            let token: String = {
                let proxy = zbus::Proxy::new(&connection, owner.as_str(), PATH, SERVICE).await
                    .map_err(|e| unavailable(e.to_string()))?;
                proxy.call("CursorBegin", &(label,)).await
                    .map_err(|e| input_unknown(format!("Cursor takeover was not acknowledged: {e}")))?
            };
            if uuid::Uuid::parse_str(&token).is_err() { return Err(input_unknown("Invalid cursor lease receipt.")); }
            Ok(CursorLease { connection: Some(connection), owner, token })
        }).await.map_err(|_| input_unknown("Cursor takeover timed out; input must not start."))?
    }
}
impl CursorLease {
    pub async fn active(&self) -> Result<bool> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let connection = self.connection.as_ref().ok_or_else(|| input_unknown("Cursor connection is closed."))?;
            if connection.is_closed() { return Ok(false); }
            let state = Gnome::state_on(connection, &self.owner).await?;
            Ok(state.cursor_visible && !state.locked && state.monitors.len()==1 && state.monitors[0].scale==1.0)
        }).await.map_err(|_| input_unknown("Cursor state timed out."))?
    }
    async fn proxy(&self) -> Result<zbus::Proxy<'_>> {
        let connection = self.connection.as_ref().ok_or_else(|| input_unknown("Cursor connection is closed."))?;
        zbus::Proxy::new(connection, self.owner.as_str(), PATH, SERVICE).await
            .map_err(|e| input_unknown(e.to_string()))
    }
    pub async fn begin_input(&self, surface: &SurfaceId, generation: &str) -> Result<InputTransaction> {
        let connection = self.connection.as_ref().ok_or_else(|| input_unknown("Cursor connection is closed."))?;
        let result = begin_input_on(connection.clone(), self.owner.clone(), surface, generation, Some(&self.token)).await;
        if result.is_err() { let _ = connection.clone().close().await; }
        result
    }
    pub async fn move_to(&self, x: f64, y: f64) -> Result<()> {
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let painted: bool = self.proxy().await?.call("CursorMove", &(self.token.as_str(), x, y)).await
                .map_err(|e| input_unknown(format!("Cursor paint was not acknowledged: {e}")))?;
            if painted { Ok(()) } else { Err(input_unknown("Cursor paint was refused; input must not start.")) }
        }).await.unwrap_or_else(|_| Err(input_unknown("Cursor paint timed out; input must not start.")));
        if result.is_err() {
            if let Some(connection) = &self.connection { let _ = connection.clone().close().await; }
        }
        result
    }
    pub async fn finish(mut self) -> Result<()> {
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let released: bool = self.proxy().await?.call("CursorEnd", &(self.token.as_str(),)).await
                .map_err(|e| input_unknown(format!("Cursor release was not acknowledged: {e}")))?;
            if !released { return Err(input_unknown("Cursor release was refused.")); }
            loop {
                let connection = self.connection.as_ref().ok_or_else(|| input_unknown("Cursor connection is closed."))?;
                let state = Gnome::state_on(connection, &self.owner).await?;
                if state.input_settled == Some(true) { return Ok(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.map_err(|_| input_unknown("Cursor release timed out."));
        if let Some(connection) = self.connection.take() { let _ = connection.close().await; }
        result?
    }
}
impl Drop for CursorLease {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { let _ = connection.close().await; });
            }
        }
    }
}
