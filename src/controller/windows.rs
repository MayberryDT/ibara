//! Windows for a person (the console's Windows tab): the workspaces that hold
//! windows, and the active one, with the windows on each; closing a window;
//! moving one to another workspace without showing it. A window the task
//! holding control opened is that agent's while it works: closing or moving
//! it is refused (`BUSY`, `reason: agent_window`) and nothing is sent.

use super::everyday::number;
use super::ports::{Win, WinKey, Effect, Cancel};
use crate::desktop::TypingCursor;
use super::{Controller, squash};
use crate::error::{IbaraError, Result, invalid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use std::os::fd::{OwnedFd, FromRawFd, AsRawFd};
use tokio::time::{Instant, sleep};

mod editor;
mod portal;

/// How long a closed window may take to go (an app may ask to save first).
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How long a moved window may take to report its new workspace.
const MOVE_WAIT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(100);
/// A window's title in a timeline summary or an approval.
const TITLE_CHARS: usize = 60;

/// `address` (`0x` and 1 to 16 lowercase hex digits) and `pid` (positive).
fn window_fields(action: &Value) -> Result<(String, i64)> {
    let address = action.get("address").and_then(Value::as_str).unwrap_or("");
    let hex = address.strip_prefix("0x").unwrap_or("");
    let gnome = address.strip_prefix("gnome:").is_some_and(|id| !id.is_empty() && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit()) && id.parse::<u32>().is_ok());
    if !gnome && (!(1..=16).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) {
        return Err(invalid("Expected the exact window address from the current window list."));
    }
    let pid = number(action, "pid").filter(|p| *p > 0).ok_or_else(|| invalid("Expected the window's process id."))?;
    Ok((address.to_string(), pid))
}

fn gone() -> IbaraError {
    IbaraError::new("NOT_FOUND", "That window is gone.", true)
}

/// Hyprland not finding the window it was asked about means it is gone.
fn gone_if_stale(error: IbaraError) -> IbaraError {
    if error.code == "STALE_TARGET" { gone() } else { error }
}

/// A special workspace (the scratchpad), by its name alone: a named
/// workspace (`workspace name:hdmi`) has a negative id too but is ordinary.
fn special(name: &str) -> bool {
    name.starts_with("special:")
}

/// Where a workspace is listed: numbered ones by id, then named ones by name,
/// then special ones by name.
fn order(id: i64, name: &str) -> (u8, i64, String) {
    match (special(name), id > 0) {
        (true, _) => (2, 0, name.to_string()),
        (false, true) => (0, id, String::new()),
        (false, false) => (1, 0, name.to_string()),
    }
}

/// Terminal programs by their `/proc/<pid>/comm` (at most 15 characters, so
/// `gnome-terminal-server` is `gnome-terminal-`). A window's class does not
/// tell: Omarchy and agents start terminal apps with their own app ids
/// (`foot -a fleet-btop`, `org.omarchy.btop`).
const TERMINALS: &[&str] =
    &["foot", "kitty", "alacritty", "ghostty", "wezterm-gui", "xterm", "urxvt", "konsole", "gnome-terminal-", "ptyxis", "tilix"];

/// Whether the process behind a window is a terminal; false when unreadable.
fn terminal(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| TERMINALS.contains(&comm.trim()))
}

fn same(w: &Win, address: &str, pid: i64) -> bool {
    w.address == address && w.pid == pid
}

pub(super) fn is_browser(class: &str) -> bool {
    matches!(class.to_ascii_lowercase().as_str(), "chromium" | "google-chrome" | "google-chrome-stable")
}

/// Only stable client identity: title/geometry changes must not restart failed input.
pub(super) fn reset_inventory(windows: Option<&[Win]>) -> Value {
    let Some(windows) = windows else { return json!("unavailable"); };
    let mut keys: Vec<_> = windows.iter().map(|w| (&w.address, w.pid, &w.class, w.process_start_ticks, &w.compositor_instance)).collect();
    keys.sort();
    json!(keys)
}

impl Controller {
    // Chromium beforeunload is native browser chrome, never a page form.
    async fn leave_browser_cleanup_prompt(&self, key: &WinKey, cancel: &Cancel,
        allowed: &impl Fn() -> Result<bool>) -> Result<bool> {
        use super::ports::Button;
        if !allowed()? || cancel.is_cancelled() { return Ok(false); }
        let native = |e: &crate::desktop::atspi::Element, name: &str| {
            e.name == name && matches!(e.role.as_str(), "button" | "push button")
                && e.states.iter().any(|s| s == "enabled")
                && matches!(e.context.as_slice(), [c] if matches!(c.as_str(), r#"dialog "Leave site?""# | r#"alert "Leave site?""#))
        };
        // An unreadable tree proves no prompt; tab-close verification still guards.
        let Ok(leave) = self.desktop.elements(key, Some("Leave"), 60, None).await else { return Ok(false) };
        if !leave.available || leave.next_cursor.is_some() { return Ok(false); }
        let buttons: Vec<_> = leave.elements.iter().filter(|e| native(e, "Leave")).collect();
        if buttons.len() != 1 { return Ok(false); }
        let Ok(cancel_button) = self.desktop.elements(key, Some("Cancel"), 60, None).await else { return Ok(false) };
        if !cancel_button.available || cancel_button.next_cursor.is_some()
            || cancel_button.elements.iter().filter(|e| native(e, "Cancel")).count() != 1 { return Ok(false); }
        if !allowed()? || cancel.is_cancelled()
            || !self.desktop.windows().await?.iter().any(|w| w.key() == *key && w.focused) { return Ok(false); }
        self.desktop.act(&Effect::ClickElement { surface:key.clone(), element:Box::new((*buttons[0]).clone()),
            button:Button::Left, double:false }, cancel).await?;
        if !allowed()? || cancel.is_cancelled() {
            return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser cleanup authority changed after confirmation.", false));
        }
        self.timeline("desktop_dialog_cleanup", None, "ibara", "Confirmed Chromium's native leave-site prompt during disposable cleanup.",
            json!({"window":key.address,"dialog":"Leave site?","action":"Leave","basis":"native alert/dialog; exact Leave and Cancel; outside web document; reset authority"}));
        Ok(true)
    }

    /// The reader sees exactly one ordinary window holding one blank tab, so a
    /// graceful close leaves no session to restore. Unreadable means no.
    async fn browser_only_blank(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        let read = loop {
            match self.desktop.browser_call("lifecycle_inventory", json!({}), false).await {
                Err(e) if e.details.get("execution_not_started") == Some(&Value::Bool(true)) && Instant::now() < deadline => sleep(POLL).await,
                result => break result,
            }
        };
        let Ok(read) = read else { return false };
        matches!(read["windows"].as_array().map(Vec::as_slice), Some([w])
            if w["type"] == "normal" && w["incognito"] != true
                && matches!(w["tabs"].as_array().map(Vec::as_slice), Some([t]) if t["blank"] == true))
    }

    /// Native retirement is owned by the serialized reset, never an expired task.
    async fn retire_browser(&self, key: &WinKey, cancel: &Cancel, allowed: &impl Fn() -> Result<bool>) -> Result<()> {
        // A reader update can reconnect between native input and its read-only
        // verification. Resume that observation, never replay the input. Allow
        // the worker's 2 s reconnect plus both 3 s native handshake phases.
        let inventory = async || {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if !allowed()? || cancel.is_cancelled() {
                    return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false));
                }
                match self.desktop.browser_call("lifecycle_inventory", json!({}), false).await {
                    Err(e) if matches!(e.code, "CAPABILITY_UNAVAILABLE" | "STALE_TARGET")
                        && e.details.get("execution_not_started") == Some(&Value::Bool(true))
                        && Instant::now() < deadline => sleep(POLL).await,
                    result => return result,
                }
            }
        };
        let send = async |effect| {
            if !allowed()? || cancel.is_cancelled() {
                return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false));
            }
            let step = match &effect {
                Effect::Focus(_) => "focus".to_string(),
                Effect::Key { combo, .. } => format!("key {combo}"),
                Effect::Type { .. } => "type blank address".to_string(),
                _ => "native input".to_string(),
            };
            self.desktop.act(&effect, cancel).await.map_err(|e| e.with("retirement_step", step))?;
            if !allowed()? || cancel.is_cancelled() { return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false)); }
            Ok(())
        };
        send(Effect::Focus(key.clone())).await?;
        if !self.desktop.windows().await?.iter().any(|w| w.key() == *key && w.focused) {
            return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser retirement window focus changed.", false));
        }
        self.leave_browser_cleanup_prompt(key, cancel, allowed).await?;
        // Full-screen Chromium can hide/retract browser chrome while the next
        // native chord is delivered. Retire in an ordinary visible window and
        // independently observe that transition before using the address bar.
        if self.desktop.windows().await?.iter().any(|w| w.key()==*key && w.fullscreen) {
            send(Effect::Key{surface:key.clone(),combo:"F11".into()}).await?;
            let deadline=Instant::now()+CLOSE_WAIT;
            let mut geometry = None;
            let mut stable_since = Instant::now();
            loop {
                if !allowed()? || cancel.is_cancelled() {return Err(IbaraError::new("CONTROL_UNSETTLED","Reset authority changed.",false));}
                let current = self.desktop.windows().await?.iter()
                    .find(|w| w.key()==*key && w.focused && !w.fullscreen)
                    .map(|w| w.rect);
                // The flag changes before Chromium's surface resize finishes.
                // Do not bind native typing to that transient geometry.
                if current.is_none() || current != geometry {
                    geometry = current;
                    stable_since = Instant::now();
                } else if stable_since.elapsed() >= Duration::from_millis(500) {
                    break;
                }
                if Instant::now()>=deadline {return Err(IbaraError::new("CONTROL_UNSETTLED","Browser did not settle after leaving full-screen mode for cleanup.",false));}
                sleep(POLL).await;
            }
        }
        let focus_deadline = Instant::now() + CLOSE_WAIT;
        let focused = loop {
            if !allowed()? || cancel.is_cancelled() { return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false)); }
            let observed = inventory().await?;
            let windows = observed.get("windows").and_then(Value::as_array)
                .ok_or_else(|| IbaraError::new("CAPABILITY_UNAVAILABLE", "Browser retirement inventory unavailable.", false))?;
            if let Some(w) = windows.iter().find(|w| w["focused"] == true) { break w.clone(); }
            if Instant::now() >= focus_deadline { return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Browser retirement focus unavailable.", false)); }
            sleep(POLL).await;
        };
        if focused["type"] != "normal" || focused["incognito"] == true {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Browser retirement requires an ordinary profile window.", false));
        }
        let window_id = focused["id"].clone();
        let count = focused["tabs"].as_array().map_or(0, Vec::len);
        if count == 0 || count > 64 { return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Browser retirement tab bound exceeded.", false)); }
        send(Effect::Key { surface:key.clone(), combo:"ctrl+t".into() }).await?;
        // Dispatch completion precedes Chromium's tab/focus transition. Observe
        // the new active tab on two consecutive reads before another chord.
        let tab_deadline = Instant::now() + CLOSE_WAIT;
        let old_ids: Vec<_> = focused["tabs"].as_array().unwrap().iter().map(|t| t["id"].clone()).collect();
        let mut observed_new = Value::Null;
        loop {
            if !allowed()? || cancel.is_cancelled() { return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false)); }
            let read = inventory().await?;
            let tabs = read["windows"].as_array().and_then(|ws| ws.iter().find(|w| w["id"] == window_id && w["focused"] == true))
                .and_then(|w| w["tabs"].as_array());
            let active = tabs.filter(|ts| ts.len() == count + 1)
                .and_then(|ts| ts.iter().find(|t| t["active"] == true && !old_ids.contains(&t["id"])))
                .map(|t| t["id"].clone()).unwrap_or(Value::Null);
            if !active.is_null() && active == observed_new { break; }
            observed_new = active;
            if Instant::now() >= tab_deadline { return Err(IbaraError::new("CONTROL_UNSETTLED", "Native new tab readiness was not verified.", false)); }
            sleep(POLL).await;
        }
        send(Effect::Key { surface:key.clone(), combo:"ctrl+l".into() }).await?;
        send(Effect::Type { surface:key.clone(), cursor:TypingCursor::Stays, text:"about:blank".into() }).await?;
        send(Effect::Key { surface:key.clone(), combo:"Return".into() }).await?;
        let created_deadline = Instant::now() + CLOSE_WAIT;
        loop {
            if !allowed()? || cancel.is_cancelled() { return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false)); }
            let created = inventory().await?;
            let tabs = created["windows"].as_array().and_then(|ws| ws.iter().find(|w| w["id"] == window_id && w["focused"] == true))
                .and_then(|w| w["tabs"].as_array());
            if tabs.is_some_and(|ts| ts.len() == count + 1 && ts.iter().any(|t| t["active"] == true && t["blank"] == true)) { break; }
            if Instant::now() >= created_deadline { return Err(IbaraError::new("CONTROL_UNSETTLED", "Native blank tab creation was not verified.", false)); }
            sleep(POLL).await;
        }
        for remaining in (1..=count).rev() {
            send(Effect::Key { surface:key.clone(), combo:"ctrl+1".into() }).await?;
            send(Effect::Key { surface:key.clone(), combo:"ctrl+w".into() }).await?;
            let deadline = Instant::now() + CLOSE_WAIT;
            let mut confirmed = false;
            loop {
                if !allowed()? || cancel.is_cancelled() { return Err(IbaraError::new("CONTROL_UNSETTLED", "Reset authority changed.", false)); }
                if !confirmed { confirmed = self.leave_browser_cleanup_prompt(key, cancel, allowed).await?; }
                let read = inventory().await?;
                let w = read["windows"].as_array().and_then(|ws| ws.iter().find(|w| w["id"] == window_id && w["focused"] == true));
                if let Some(tabs) = w.and_then(|w| w["tabs"].as_array()) {
                    if tabs.len() == remaining {
                        if remaining == 1 && tabs[0]["blank"] != true {
                            return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser retirement did not leave a blank tab.", false));
                        }
                        break;
                    }
                }
                if Instant::now() >= deadline { return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser tab close was not verified; recover any prompt.", false)); }
                sleep(POLL).await;
            }
        }
        Ok(())
    }

    /// Called only inside shared settlement, after input/jobs/effects have drained.
    /// No ownership exceptions on a designated disposable desktop. Graceful
    /// retirement first; whatever it leaves open is then terminated, so a
    /// disposable desktop can recover while settlement authority remains held.
    pub(crate) async fn reset_desktop_windows(&self) -> Result<bool> {
        let before = self.journal.get_control()?;
        let verified = self.reset_desktop_windows_gracefully().await?;
        if verified { return Ok(true); }
        let graceful = self.journal.desktop_reset()?;
        if graceful["input_settled"] == false { return Ok(false); }
        self.mark_effect(Some("desktop-reset-termination"));
        let result = self.terminate_leftover_windows(&before).await;
        self.mark_effect(None);
        let terminated = result?;
        if terminated && self.journal.browser_retirement_pending()? {
            // Process exit is not browser-session retirement. Reconcile the
            // retained restore state natively before another task is admitted.
            let force = self.journal.desktop_reset()?;
            let result = self.reset_desktop_windows_gracefully().await;
            let mut receipt = self.journal.desktop_reset()?;
            receipt["force_cleanup"] = force.clone();
            if let Some(closed) = force["cleanup"]["closed"].as_array() {
                let mut all = closed.clone();
                if let Some(retired) = receipt["cleanup"]["closed"].as_array() { all.extend(retired.iter().cloned()); }
                receipt["cleanup"]["closed"] = json!(all);
            }
            self.journal.put_desktop_reset(&receipt)?;
            result
        } else {
            Ok(terminated)
        }
    }

    /// Ends the processes behind every window graceful cleanup left on a
    /// disposable desktop: SIGTERM, then SIGKILL, then a zero-window check.
    /// Never while a lease, a persons pause or a persons control is held.
    async fn terminate_leftover_windows(&self, before: &crate::store::ControlState) -> Result<bool> {
        let allowed = || -> Result<bool> {
            let now = self.journal.get_control()?;
            Ok((self.disposable_desktop)() && before.settling_generation.is_some()
                && self.journal.get_active_lease()?.is_none()
                && self.viewer_state.borrow().owner.is_none()
                && now.settling_generation == before.settling_generation && now.epoch == before.epoch
                && now.human_control == before.human_control && now.paused == before.paused
                && now.pause_origin == before.pause_origin
                && now.pause_origin != Some(crate::store::PauseOrigin::Person)
                && (!now.human_control || now.pause_origin == Some(crate::store::PauseOrigin::System)))
        };
        if !allowed()? { return Ok(false); }
        let Ok(windows) = self.desktop.all_windows().await else { return Ok(false) };
        if !allowed()? { return Ok(false); }
        let me = std::process::id() as i64;
        let mut pids: Vec<(i64, u64, OwnedFd)> = Vec::new();
        for window in &windows {
            let Some(start) = window.process_start_ticks else { return Ok(false) };
            if window.pid <= 1 || window.pid == me || window.compositor_instance.is_empty() {
                return Ok(false);
            }
            if pids.iter().any(|(pid, ticks, _)| *pid == window.pid && *ticks == start) { continue; }
            // Open first, then check incarnation: a reused numeric PID cannot
            // redirect either later signal to a replacement process.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, window.pid as libc::pid_t, 0) };
            if raw < 0 { return Ok(false); }
            // SAFETY: successful pidfd_open returned a new owned descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
            let ticks = std::fs::read_to_string(format!("/proc/{}/stat", window.pid)).ok()
                .and_then(|stat| stat.rsplit_once(')').and_then(|(_, tail)| tail.split_whitespace().nth(19)?.parse::<u64>().ok()));
            if ticks != Some(start) { return Ok(false); }
            pids.push((window.pid, start, fd));
        }
        let terminated: Vec<String> = windows.iter().map(|w| format!("{} {}", w.class, w.address)).collect();
        let mut remaining = Some(windows.len());
        let mut intent = self.journal.desktop_reset()?;
        intent["force_signals"] = json!([]);
        intent["force_state"] = json!("incomplete");
        self.journal.put_desktop_reset(&intent)?;
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            for (pid, start, fd) in &pids {
                if !allowed()? { return Ok(false); }
                let mut receipt = self.journal.desktop_reset()?;
                receipt["force_signals"].as_array_mut().unwrap().push(json!({
                    "pid":pid,"process_start_ticks":start,"signal":signal,"state":"intent"}));
                // Durable intent survives a crash or failed post-signal write;
                // it is never relabelled as an effect that did not start.
                self.journal.put_desktop_reset(&receipt)?;
                if !allowed()? { return Ok(false); }
                // SAFETY: the descriptor binds the verified process incarnation;
                // null siginfo requests a normal signal, with flags zero.
                let sent = unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0) };
                let error = (sent < 0).then(|| std::io::Error::last_os_error().raw_os_error());
                let state = if sent >= 0 { "sent" } else if error == Some(Some(libc::ESRCH)) { "gone" } else { "failed" };
                receipt["force_signals"].as_array_mut().unwrap().last_mut().unwrap()["state"] = json!(state);
                self.journal.put_desktop_reset(&receipt)?;
                if state == "failed" { return Ok(false); }
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if !allowed()? { return Ok(false); }
                remaining = self.desktop.all_windows().await.ok().map(|w| w.len());
                if !allowed()? { return Ok(false); }
                if remaining == Some(0) || Instant::now() >= deadline { break; }
                sleep(POLL).await;
            }
            if remaining == Some(0) { break; }
        }
        if !allowed()? { return Ok(false); }
        let verified = remaining == Some(0);
        super::log_event("desktop_reset_terminated", &format!("{} window(s), {} process(es), verified {verified}", terminated.len(), pids.len()));
        if verified {
            self.timeline("desktop_reset_terminated", None, "ibara", "Closed windows that did not close on request.",
                json!({"terminated":terminated,"processes":pids.len()}));
        }
        let mut receipt = self.journal.desktop_reset()?;
        receipt["graceful_cleanup"] = receipt.clone();
        receipt["force_state"] = json!(if verified { "zero_windows_verified" } else { "incomplete" });
        receipt["state"] = json!(if verified && self.journal.browser_retirement_pending()? { "unsettled" }
            else if verified { "verified" } else { "blocked" });
        receipt["reason"] = if verified { Value::Null } else { json!("windows survived termination") };
        receipt["remaining"] = json!(remaining);
        receipt["terminated"] = json!(terminated);
        receipt["browser_retirement_pending"] = json!(self.journal.browser_retirement_pending()?);
        if verified {
            let closed: Vec<Value> = terminated.iter().map(|t| json!(format!("{t} (force-closed)"))).collect();
            receipt["cleanup"]["left"] = json!([]);
            match receipt["cleanup"]["closed"].as_array_mut() {
                Some(list) => list.extend(closed),
                None => receipt["cleanup"]["closed"] = json!(closed),
            }
        } else {
            let inventory = self.desktop.all_windows().await.ok();
            receipt["blocked_inventory"] = reset_inventory(inventory.as_deref());
        }
        self.journal.put_desktop_reset(&receipt)?;
        Ok(verified)
    }

    async fn reset_desktop_windows_gracefully(&self) -> Result<bool> {
        // Every release path (watchdog, admission, finish and reconciliation) comes
        // here. Do not replay uncertain native effects against unchanged clients.
        // A changed client inventory or explicit operator handback permits a new try.
        let previous = self.journal.desktop_reset()?;
        if previous["state"] == "blocked" {
            let inventory = self.desktop.all_windows().await.ok();
            if previous["blocked_inventory"] == reset_inventory(inventory.as_deref()) {
                return Ok(false);
            }
        }
        let started = Instant::now();
        let (cancel, _) = self.abort_handles();
        self.mark_effect(Some("desktop-reset"));
        let reset = tokio::time::timeout(Duration::from_secs(60), async {
            let verified = self.reset_desktop_windows_inner().await?;
            // One bounded second pass retires restore state after an unreadable
            // browser was gracefully closed; never report ready between passes.
            if !verified && self.journal.desktop_reset()?["state"] == "unsettled"
                && self.journal.browser_retirement_pending()?
            {
                self.reset_desktop_windows_inner().await
            } else {
                Ok(verified)
            }
        }).await;
        self.mark_effect(None);
        let result = match reset {
            Ok(Ok(verified)) => Ok(verified),
            other => {
                cancel.cancel();
                self.desktop.set_agent(None);
                // Dropping an effect future does not stop supervised helpers: explicitly cancel and drain.
                let input = self.desktop.release_input().await;
                let reason = match other {
                    Ok(Err(e)) => format!("window reset failed: {}: {}", e.code, e.message),
                    Err(_) => "window reset exceeded its 60 second deadline".into(),
                    _ => unreachable!(),
                };
                self.journal.put_desktop_reset(&json!({"state":"blocked","reason":reason,
                    "remaining":Value::Null,"at":self.now_iso(),"input_settled":input.is_ok(),
                    "cleanup":{"closed":[],"left":[{"surface":"desktop","reason":reason}]}}))?;
                Ok(false)
            }
        };
        let mut receipt = self.journal.desktop_reset()?;
        receipt["duration_ms"] = json!(started.elapsed().as_millis());
        receipt["browser_retirement_pending"] = json!(self.journal.browser_retirement_pending()?);
        if receipt["state"] == "blocked" {
            let inventory = self.desktop.all_windows().await.ok();
            receipt["blocked_inventory"] = reset_inventory(inventory.as_deref());
        }
        self.journal.put_desktop_reset(&receipt)?;
        result
    }

    async fn reset_desktop_windows_inner(&self) -> Result<bool> {
        let mut cleanup = crate::contract::Cleanup::default();
        let (cancel, _) = self.abort_handles();
        let before = self.journal.get_control()?;
        let allowed = || -> Result<bool> {
            let now = self.journal.get_control()?;
            Ok((self.disposable_desktop)() && before.settling_generation.is_some() && self.journal.get_active_lease()?.is_none()
                && self.viewer_state.borrow().owner.is_none()
                && now.settling_generation == before.settling_generation && now.epoch == before.epoch
                && (!now.human_control || now.pause_origin == Some(crate::store::PauseOrigin::System)) && now.human_control == before.human_control && now.paused == before.paused
                && now.pause_origin == before.pause_origin
                && now.pause_origin != Some(crate::store::PauseOrigin::Person))
        };
        self.journal.put_desktop_reset(&json!({"state":"resetting","at":self.now_iso()}))?;
        if !allowed()? {
            self.journal.put_desktop_reset(&json!({"state":"blocked","reason":"control is reserved","at":self.now_iso()}))?;
            return Ok(false);
        }
        let mut browser_generation = self.journal.browser_window_generation()?;
        let mut windows = match self.desktop.all_windows().await {
            Ok(w) => w,
            Err(_) => {
                self.journal.put_desktop_reset(&json!({"state":"blocked","reason":"window inventory unavailable","at":self.now_iso()}))?;
                return Ok(false);
            }
        };
        if self.journal.browser_window_generation()? != browser_generation {
            return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser changed while inventory was read; fresh reconciliation required.", false));
        }
        if windows.iter().any(|w| is_browser(&w.class)) {
            self.journal.set_browser_retirement_pending(true)?;
        } else if self.journal.browser_retirement_pending()? {
            // A crashed or manually closed browser can leave a restore session behind with zero windows.
            // Reopen under settlement authority, retire it natively, then close before any new lease.
            if !allowed()? || cancel.is_cancelled() { return Ok(false); }
            // GNOME input needs the reset to own control, as for its closes.
            self.desktop.set_agent(Some("ibara desktop reset".into()));
            let launched = self.desktop.act(&Effect::Launch { app_id:"browser".into() }, &cancel).await;
            self.desktop.set_agent(None);
            self.desktop.release_input().await?;
            launched?;
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut mapped: Option<(WinKey, Instant)> = None;
            loop {
                if !allowed()? || cancel.is_cancelled() { return Ok(false); }
                windows = self.desktop.all_windows().await?;
                // Incorporate exactly our launched window's opening event before retirement.
                // Missing/racing events leave it open for the next fresh reset, never a blind close/reopen loop.
                let generation = self.journal.browser_window_generation()?;
                let address = self.journal.last_browser_window_opened()?;
                if generation == browser_generation + 1 && windows.iter().any(|w| is_browser(&w.class) && Some(&w.address) == address.as_ref()) {
                    browser_generation = generation;
                    break;
                }
                // Without open events (GNOME), accept only exactly one browser
                // window in a reset that started with none, once it has stayed
                // mapped for a second (focus right after mapping is refused).
                if !self.desktop.emits_window_open_events() && generation == browser_generation {
                    let browsers: Vec<_> = windows.iter().filter(|w| is_browser(&w.class)).map(|w| w.key()).collect();
                    match (&browsers[..], &mapped) {
                        ([key], Some((seen, since))) if seen == key && since.elapsed() >= Duration::from_secs(1) => break,
                        ([key], Some((seen, _))) if seen == key => {}
                        ([key], _) => mapped = Some((key.clone(), Instant::now())),
                        _ => mapped = None,
                    }
                }
                if generation > browser_generation + 1 {
                    return Err(IbaraError::new("CONTROL_UNSETTLED", "Another browser opened during preparation; fresh reconciliation required.", false));
                }
                if Instant::now() >= deadline {
                    return Err(IbaraError::new("CONTROL_UNSETTLED", "Browser session preparation did not produce an observable window.", false));
                }
                sleep(POLL).await;
            }
        }
        // A native file chooser blocks its browser's shortcuts. Cancel only a
        // recognized task-owned chooser before retiring tabs, and refresh the
        // inventory. Other app windows keep their original tiling for input.
        let mut automatic_dialogs = Vec::new();
        self.desktop.set_agent(Some("ibara desktop reset".into()));
        let portals = self.retire_portal_choosers(&windows, &cancel, &allowed, &mut automatic_dialogs).await;
        self.desktop.set_agent(None);
        self.desktop.release_input().await?;
        portals?;
        if !automatic_dialogs.is_empty() { windows = self.desktop.all_windows().await?; }
        // Keep the original tiling intact while sending browser keys. Closing a
        // neighboring app changes focus/layout and can revoke Cua's input route.
        // Retire all browser sessions first; graceful closes happen only afterward.
        let mut asked = Vec::new();
        let mut retired = Vec::new();
        let mut unreadable_browser_closed = false;
        let browsers = windows.iter().filter(|w| is_browser(&w.class)).count();
        for window in windows.iter().filter(|w| is_browser(&w.class)) {
            if !allowed()? || cancel.is_cancelled() { break; }
            let name = format!("{} {}", window.class, window.address);
            if browsers == 1 && self.browser_only_blank().await {
                retired.push(window.key());
                continue;
            }
            self.desktop.set_agent(Some("ibara desktop reset".into()));
            let retirement = self.retire_browser(&window.key(), &cancel, &allowed).await;
            self.desktop.set_agent(None);
            // Unsettled input forbids every later effect, including window closes.
            // The reset wrapper cancels and drains again and retains a blocker.
            self.desktop.release_input().await?;
            if let Err(e) = retirement {
                super::log_event("browser_retirement_refused", &format!("{name}: {}: {} at {}", e.code, e.message,
                    e.details.get("retirement_step").and_then(Value::as_str).unwrap_or("verification")));
                // A refused step sent nothing (focus unavailable, or a native field
                // that exposes no accessibility object). Close only this exact browser
                // under reset authority, then require fresh normal-session retirement
                // before any new admission. Uncertain input never takes this path.
                let refused = e.code == "CAPABILITY_UNAVAILABLE" && (e.message == "Browser retirement focus unavailable."
                    || e.details.get("execution_not_started") == Some(&Value::Bool(true)));
                if refused && allowed()? && !cancel.is_cancelled()
                {
                    self.desktop.set_agent(Some("ibara desktop reset".into()));
                    let closed = self.desktop.act(&Effect::Close(window.key()), &cancel).await;
                    self.desktop.set_agent(None);
                    self.desktop.release_input().await?;
                    if closed.is_ok() {
                        unreadable_browser_closed = true;
                        asked.push((window.key(), name));
                        continue;
                    }
                }
                cleanup.left.push(crate::contract::LeftOpen { surface:name, reason:format!("native browser retirement refused at {}: {}: {} ({})", e.details.get("retirement_step").and_then(Value::as_str).unwrap_or("verification"), e.code, e.message, squash(e.details.get("detail").and_then(Value::as_str).unwrap_or("no native detail"), 300)) });
            } else {
                retired.push(window.key());
            }
        }
        // Resolve only the known editor's owned work, before the generic close
        // pass can cancel a dialog and strand its parent in another prompt.
        let mut editor_processes = Vec::new();
        for window in &windows {
            if editor_processes.contains(&window.pid) || !self.owned_editor_process(window, &windows)? { continue; }
            editor_processes.push(window.pid);
            self.desktop.set_agent(Some("ibara desktop reset".into()));
            let result = self.retire_editor(window, &windows, &cancel, &allowed, &mut automatic_dialogs).await;
            self.desktop.set_agent(None);
            self.desktop.release_input().await?;
            match result {
                Ok(()) => cleanup.closed.extend(windows.iter().filter(|w| w.pid == window.pid).map(|w| format!("{} {}", w.class, w.address))),
                Err(e) => cleanup.left.push(crate::contract::LeftOpen { surface:format!("{} {}",window.class,window.address), reason:format!("owned editor cleanup: {}: {}",e.code,e.message) }),
            }
        }
        let mut stale = Vec::new();
        let mut already_gone = Vec::new();
        for window in windows {
            if !allowed()? || cancel.is_cancelled() { break; }
            if editor_processes.contains(&window.pid) { continue; }
            let name = format!("{} {}", window.class, window.address);
            if is_browser(&window.class) && !retired.contains(&window.key()) { continue; }
            // Releasing input can yield to Take Control. Closing belongs to
            // this reset too; never use the unfenced person-window route.
            if !allowed()? || cancel.is_cancelled() { break; }
            self.desktop.set_agent(Some("ibara desktop reset".into()));
            let result = self.desktop.act(&Effect::Close(window.key()), &cancel).await;
            self.desktop.set_agent(None);
            self.desktop.release_input().await?;
            match result {
                Ok(_) => asked.push((window.key(), name)),
                Err(e) if e.code == "STALE_TARGET" => stale.push((window.key(), name)),
                Err(e) => cleanup.left.push(crate::contract::LeftOpen { surface:name, reason:format!("close refused: {}", e.code) }),
            }
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        let mut remaining = None;
        loop {
            if !allowed()? { break; }
            match self.desktop.all_windows().await {
                Ok(now) => {
                    asked.retain(|(key,name)| {
                        if now.iter().any(|w| w.key() == *key) { true }
                        else { cleanup.closed.push(name.clone()); false }
                    });
                    // A child can exit when its parent closes. A stale close
                    // remains unresolved until this independent full inventory
                    // proves that exact identity gone. Never replay the close.
                    stale.retain(|(key, name)| {
                        if now.iter().any(|w| w.key() == *key) { true }
                        else { already_gone.push(name.clone()); false }
                    });
                    remaining = Some(now.len());
                    if now.is_empty() { break; }
                }
                Err(_) => { remaining = None; break; }
            }
            if Instant::now() >= deadline { break; }
            sleep(POLL).await;
        }
        cleanup.left.extend(asked.into_iter().map(|(_,surface)| crate::contract::LeftOpen {
            surface, reason:"still open after a close request; ended by termination if it stays".into()
        }));
        cleanup.left.extend(stale.into_iter().map(|(_, surface)| crate::contract::LeftOpen {
            surface, reason:"close refused: STALE_TARGET; disappearance not verified".into()
        }));
        let generation_settled = self.journal.browser_window_generation()? == browser_generation;
        if !generation_settled {
            cleanup.left.push(crate::contract::LeftOpen { surface:"browser session".into(), reason:"a browser opened during retirement; fresh reconciliation required".into() });
        }
        let verified = allowed()? && !cancel.is_cancelled() && remaining == Some(0) && cleanup.left.is_empty() && generation_settled && !unreadable_browser_closed;
        if verified { self.journal.set_browser_retirement_pending(false)?; }
        if !automatic_dialogs.is_empty() {
            // The latest reset can be replaced by an empty startup pass; retain
            // meaningful recovery evidence in the existing bounded timeline.
            self.timeline("desktop_dialog_cleanup", None, "ibara", "Resolved task-owned native dialogs.",
                json!({"verified":verified,"remaining":remaining,"automatic_dialogs":automatic_dialogs}));
        }
        self.journal.put_desktop_reset(&json!({"state":if verified {"verified"} else if unreadable_browser_closed && remaining == Some(0) && cleanup.left.is_empty() {"unsettled"} else {"blocked"},
            "reason":if verified { Value::Null } else { json!(if remaining.is_none() { "window inventory unavailable or control changed" } else { "windows remain open or control changed" }) },
            "remaining":remaining,"at":self.now_iso(),"cleanup":cleanup,"already_gone":already_gone,"automatic_dialogs":automatic_dialogs}))?;
        Ok(verified)
    }

    /// `windows`: the workspaces holding windows, and the active one even
    /// when empty (numbered, then named, then special), each window with
    /// whether a terminal shows it and the task that opened it, if any; and
    /// the task holding control now.
    pub(super) async fn op_windows(&self) -> Result<Value> {
        let (windows, active) = tokio::join!(self.desktop.windows(), self.desktop.active_workspace());
        let (mut windows, active) = (windows?, active?);
        let running = self.journal.get_active_lease()?.map(|l| l.task_ref);
        let owned = self.journal.owned_windows()?;
        let mut goals: HashMap<String, String> = HashMap::new();
        let mut goal = |task_ref: &str| -> Result<String> {
            if let Some(known) = goals.get(task_ref) {
                return Ok(known.clone());
            }
            let read = self.journal.get_task(task_ref)?.map(|t| squash(&t.goal, 120)).unwrap_or_default();
            goals.insert(task_ref.to_string(), read.clone());
            Ok(read)
        };
        windows.sort_by_key(|w| (w.rect.y, w.rect.x));
        let mut spaces: Vec<(i64, String)> = windows.iter().map(|w| (w.workspace_id, w.workspace.clone())).collect();
        spaces.extend(active.iter().map(|a| (a.id, a.name.clone())));
        spaces.sort_by_cached_key(|(id, name)| (order(*id, name), *id));
        spaces.dedup_by_key(|(id, _)| *id);
        let mut workspaces = Vec::with_capacity(spaces.len());
        for (id, name) in spaces {
            let mut listed = Vec::new();
            for w in windows.iter().filter(|w| w.workspace_id == id) {
                let task = match owned.iter().find(|o| o.address == w.address && o.pid == w.pid) {
                    Some(o) => json!({"task_ref": o.task_ref, "goal": goal(&o.task_ref)?, "running": running.as_deref() == Some(o.task_ref.as_str())}),
                    None => Value::Null,
                };
                listed.push(json!({
                    "address": w.address, "pid": w.pid, "class": w.class, "title": w.title,
                    "floating": w.floating, "fullscreen": w.fullscreen, "focused": w.focused, "terminal": terminal(w.pid), "task": task
                }));
            }
            let is_active = active.as_ref().is_some_and(|a| a.id == id);
            workspaces.push(json!({"id": id, "special": special(&name), "name": name, "active": is_active, "windows": listed}));
        }
        let agent = match &running {
            Some(task_ref) => json!({"task_ref": task_ref, "goal": goal(task_ref)?}),
            None => Value::Null,
        };
        Ok(json!({"workspaces": workspaces, "agent": agent}))
    }

    /// The live window a person named, unless it is gone or the task holding
    /// control opened it.
    async fn window_for_person(&self, address: &str, pid: i64) -> Result<Win> {
        let win = self.desktop.windows().await?.into_iter().find(|w| same(w, address, pid)).ok_or_else(gone)?;
        if let Some(lease) = self.journal.get_active_lease()?
            && self.journal.owned_windows()?.iter().any(|o| o.task_ref == lease.task_ref && o.address == address && o.pid == pid)
        {
            return Err(IbaraError::new("BUSY", "An agent is using this window. Stop its task first.", true)
                .with("reason", "agent_window")
                .with("task_ref", lease.task_ref));
        }
        Ok(win)
    }

    /// `window_close`: ask the window to close, as its close button does, and
    /// wait up to 2 s for it to go (`closed: false` when the app is still
    /// open, perhaps asking to save).
    pub(super) async fn op_window_close(&self, operator_id: &str, action: &Value) -> Result<Value> {
        let (address, pid) = window_fields(action)?;
        let win = self.window_for_person(&address, pid).await?;
        self.desktop.close_window(&win.key()).await.map_err(gone_if_stale)?;
        let deadline = Instant::now() + CLOSE_WAIT;
        let closed = loop {
            if !self.desktop.windows().await?.iter().any(|w| same(w, &address, pid)) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            sleep(POLL).await;
        };
        let title = squash(&win.title, TITLE_CHARS);
        let (kind, summary) = if closed {
            ("window_closed", format!("Closed {} “{title}”", win.class))
        } else {
            ("window_close_asked", format!("Asked {} “{title}” to close; it is still open", win.class))
        };
        self.timeline(kind, None, operator_id, &summary, json!({"address": address, "pid": pid, "class": win.class}));
        Ok(json!({"closed": closed}))
    }

    /// `window_move`: move the window to workspace 1 to 10, leaving the shown
    /// workspace as it is, and wait up to 1 s for it to report that workspace.
    pub(super) async fn op_window_move(&self, operator_id: &str, action: &Value) -> Result<Value> {
        let (address, pid) = window_fields(action)?;
        let workspace = number(action, "workspace").filter(|n| (1..=10).contains(n)).ok_or_else(|| invalid("Choose a workspace from 1 to 10."))?;
        let win = self.window_for_person(&address, pid).await?;
        if win.workspace_id == workspace {
            return Ok(json!({"moved": true, "workspace": workspace}));
        }
        self.desktop.move_window(&win.key(), workspace).await.map_err(gone_if_stale)?;
        let deadline = Instant::now() + MOVE_WAIT;
        loop {
            if self.desktop.windows().await?.iter().any(|w| same(w, &address, pid) && w.workspace_id == workspace) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(IbaraError::new("OUTCOME_UNKNOWN", "The window did not move.", false));
            }
            sleep(POLL).await;
        }
        let summary = format!("Moved {} “{}” to workspace {workspace}", win.class, squash(&win.title, TITLE_CHARS));
        self.timeline("window_moved", None, operator_id, &summary, json!({"address": address, "pid": pid, "class": win.class, "workspace": workspace}));
        Ok(json!({"moved": true, "workspace": workspace}))
    }

    /// Before the access gate of a `window_close` or `window_move`: the named
    /// window's live title, for the words of an approval (the gate writes them
    /// without waiting on the desktop). Nothing when it cannot be read.
    pub(super) async fn note_window_title(&self, action: &Value) {
        let Ok((address, pid)) = window_fields(action) else { return };
        let title = self.desktop.windows().await.ok().and_then(|all| all.into_iter().find(|w| same(w, &address, pid))).map(|w| w.title);
        *self.window_title.borrow_mut() = title.map(|t| (address, t));
    }

    /// The title [`Self::note_window_title`] read for the window at `address`.
    pub(super) fn noted_window_title(&self, address: &str) -> Option<String> {
        self.window_title.borrow().as_ref().filter(|(a, t)| a == address && !t.trim().is_empty()).map(|(_, t)| squash(t, TITLE_CHARS))
    }
}
