//! One cursor on the screen at every moment: Hyprland's pointer, which is the
//! person's, or the agent's named cursor while an agent holds control.
//!
//! Cua's Hyprland plugin moves the real pointer along the named cursor's path
//! so pages see a person's movement, and two cursors side by side looked
//! wrong; so did a pointer that came and went between an agent's inputs. The
//! rule: the person's pointer stays hidden for the
//! whole time an agent holds the computer, and comes back only when a person
//! takes over. The screen is handed over, never shared:
//!
//! - **To the agent** when control begins ([`Handover::begin`]), and at the
//!   agent's next input after a person took the screen: its named cursor is
//!   drawn exactly where the pointer is, and then the pointer is hidden, so
//!   one cursor seems to change color. It stays so while the agent works or
//!   thinks, however long: Cua never hides it for idleness, its worker is not
//!   stopped while it is drawn, and when the worker exits anyway the pointer
//!   is shown at once and the named cursor drawn again. If the named cursor
//!   or the pointer cannot be handed over, agent input is refused, nothing
//!   sent.
//! - **To the person** when control ends (finish, pause, Take Control,
//!   revoke, expiry, disconnect) or at once when the person moves the mouse.
//!   The pointer shows first, then the named cursor is hidden: a read in
//!   flight is stopped, but a typing piece in flight (16 characters) ends
//!   first, because stopping Cua's worker in the middle of one crashes the
//!   GTK 3 app being typed into; nothing is typed after it. At the end of
//!   control both meet in one place first: the pointer comes back on the
//!   named cursor inside the focused window, else the named cursor moves
//!   onto the pointer. After a person's move the pointer is already where
//!   they put it.
//! - Agent input goes only while the agent has the screen. Cua moving a
//!   hidden named cursor draws it again, so nothing is sent while the person
//!   has it: the agent's next input waits until the mouse has been still for
//!   [`PERSON_STILL`], takes the screen back, and is refused, nothing sent,
//!   after [`PERSON_WAIT`]; an input under way when the person takes the
//!   screen sends nothing more, and says how much of it went through.
//!
//! The pointer is hidden with `cursor:inactive_timeout` (0.1 s), not
//! `cursor:invisible`: Hyprland 0.56 applies either only on its 500 ms cursor
//! timer, but this one it undoes by itself as soon as a person moves the
//! mouse (14–49 ms), and one `hyprctl eval` that restores the person's own
//! timeout and moves the pointer draws it again within 8–26 ms. ibara's own
//! pointer moves and focus changes do not draw it (all probed on a test computer). So
//! at each take both cursors show in one place for up to 0.5 s, and the
//! agent's input waits for that to pass.
//!
//! A person's movement is seen two ways: Cua's plugin refuses an input in
//! flight when the real pointer moves (`interrupted`), and while an agent
//! holds control ibara reads the pointer's position from Hyprland every
//! [`POLL`]. A change while no agent call could have moved it is the
//! person's.
//!
//! ibara starting or stopping stops Cua, which takes the named cursor with
//! it, and shows Hyprland's pointer. A marker under the state directory keeps
//! the person's own `cursor:inactive_timeout` while ibara has the pointer
//! hidden, so a restart after a crash puts it back, but only over ibara's
//! own: a timeout a new Hyprland session or a config reload put in place is
//! the person's current one and stays. A config reload during control puts
//! the config's timeout back, which would draw the pointer again: the watch
//! sees it, keeps it as the person's, and hides the pointer again. A pointer
//! the person hid themselves (`cursor:invisible`) stays hidden.

use super::cua::Cua;
use super::hyprland::Hyprland;
use crate::error::{IbaraError, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// After the person last moved the mouse, the agent's next input takes the
/// screen only once the mouse has been still this long.
pub const PERSON_STILL: Duration = Duration::from_secs(1);
/// Longest an agent input waits for the person to leave the mouse still.
pub const PERSON_WAIT: Duration = Duration::from_secs(3);
/// How often the pointer's position is read while an agent holds control.
pub const POLL: Duration = Duration::from_millis(20);
/// Hyprland hides the pointer at its next cursor tick (every 500 ms) once it
/// has been still 0.1 s: the named cursor and the pointer stay in one place,
/// and the agent's input waits, until then.
const HIDE_TICK: Duration = Duration::from_millis(650);
/// Cua paints a move's first frame at the old place; it lands after this.
const LAND: Duration = Duration::from_millis(80);
/// A take that failed (Cua could not draw its cursor) is tried again after this.
const RETRY: Duration = Duration::from_secs(1);
/// One reading from Hyprland's request socket.
const QUERY: Duration = Duration::from_millis(250);

/// Whether a live `cursor:inactive_timeout` is the one ibara hides the
/// pointer with, not the person's (a config reload puts theirs back).
fn ibaras(timeout: f64) -> bool {
    (timeout - super::hyprland::POINTER_HIDE_TIMEOUT).abs() < 1e-6
}

pub(super) fn person_busy() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "A person is using the mouse, so ibara sent nothing. Try again once they stop.", true)
        .with("reason", "person_active")
        .with("execution_not_started", true)
}

fn cursor_unavailable() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "ibara could not show the agent's cursor, so it sent nothing. Try again.", true)
        .with("reason", "agent_cursor_unavailable")
        .with("execution_not_started", true)
}

/// Agent calls that can move the real pointer (Cua input, ibara's own pointer
/// moves and focus changes), counted so their movement is never taken for the
/// person's.
#[derive(Debug, Default)]
pub struct Motion {
    busy: AtomicUsize,
    starts: AtomicU64,
}

/// One agent call that can move the real pointer, while it runs.
pub struct Moving<'a>(&'a Motion);

impl Motion {
    /// `busy` is raised before `starts`, and a reading takes `starts` before
    /// `busy`: a call that begins during a reading makes it void.
    pub fn begin(&self) -> Moving<'_> {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.starts.fetch_add(1, Ordering::SeqCst);
        Moving(self)
    }

    fn now(&self) -> (u64, usize) {
        let starts = self.starts.load(Ordering::SeqCst);
        (starts, self.busy.load(Ordering::SeqCst))
    }
}

impl Drop for Moving<'_> {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why the screen goes back to the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// Control ended.
    Released,
    /// The person moved the mouse (or Cua refused an input because they
    /// used the mouse or keyboard): the pointer stays where they put it.
    Person,
    /// Cua's worker exited and took the named cursor with it: the pointer
    /// until the agent has it again, at once.
    Lost,
}

/// How a take ended.
#[derive(Debug, PartialEq, Eq)]
enum Taken {
    Yes,
    /// The person moved the mouse meanwhile: the screen stays theirs.
    PersonMoved,
    Failed,
}

#[derive(Debug, Default)]
struct State {
    /// The named cursor is drawn and Hyprland's pointer hidden (by ibara, or
    /// by the person's own `cursor:invisible`).
    agent_holds: bool,
    viewer_person: bool,
    /// When the person was last seen moving the mouse.
    person_at: Option<Instant>,
    /// The pointer's last reading while no agent call could move it, with
    /// the count of such calls then.
    seen: Option<(u64, (f64, f64))>,
    /// The watch task runs.
    watching: bool,
    /// Take the screen back for the agent, not before then (its named cursor
    /// was lost, or could not be drawn at the start of control).
    retake: Option<Instant>,
}

impl State {
    fn person_active(&self) -> bool {
        self.viewer_person || self.person_at.is_some_and(|t| t.elapsed() < PERSON_STILL)
    }
}

struct Inner {
    hypr: Hyprland,
    cua: Cua,
    motion: Arc<Motion>,
    marker: PathBuf,
    runtime: Option<PathBuf>,
    /// One handover at a time.
    turn: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// Hyprland's request socket, once found.
    socket: Mutex<Option<PathBuf>>,
}

/// The screen's one cursor. Cheap to clone.
#[derive(Clone)]
pub struct Handover {
    inner: Arc<Inner>,
}

impl Handover {
    /// `runtime` is the `XDG_RUNTIME_DIR` Hyprland's sockets are under.
    pub fn new(hypr: Hyprland, cua: Cua, marker: PathBuf, runtime: Option<PathBuf>) -> Handover {
        let motion = cua.motion();
        Handover {
            inner: Arc::new(Inner {
                hypr,
                cua,
                motion,
                marker,
                runtime,
                turn: tokio::sync::Mutex::new(()),
                state: Mutex::new(State::default()),
                socket: Mutex::new(None),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Control begins: the screen goes to the agent now, not at its first
    /// input. A person using the mouse keeps it until the agent's first input.
    pub fn begin(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.to_agent().await
                && e.details.get("reason").and_then(serde_json::Value::as_str) == Some("agent_cursor_unavailable")
            {
                me.state().retake = Some(Instant::now() + RETRY);
            }
        });
    }

    /// Before an agent input: the screen is the agent's already, or it takes
    /// it back. Agent input goes only while the agent has the screen, so
    /// while the person moves the mouse it waits for the mouse to be still,
    /// and after [`PERSON_WAIT`] it is refused, nothing sent; it is refused
    /// too when the named cursor or the pointer cannot be handed over. With
    /// no agent set, the person's pointer stays.
    pub fn viewer_turn(&self, person: bool) {
        self.state().viewer_person = person;
        if person {
            self.inner.cua.yield_to_person();
            let me = self.clone();
            tokio::spawn(async move {
                let _turn = me.inner.turn.lock().await;
                if me.state().viewer_person { me.back(Why::Person).await; }
            });
        }
    }

    pub async fn to_agent(&self) -> Result<()> {
        if self.state().viewer_person { return Err(person_busy()); }
        if !self.inner.cua.has_agent() {
            return Ok(());
        }
        // The first time, nothing has read the pointer lately: read it twice,
        // [`POLL`] apart, to see a person moving it now. The watch reads it
        // from here on.
        if self.keep_watching() {
            self.moved_by_person().await;
            tokio::time::sleep(POLL).await;
        }
        // A move the watch has not read yet.
        if self.moved_by_person().await {
            self.person_moved().await;
        }
        let asked = Instant::now();
        loop {
            while self.state().person_active() {
                if asked.elapsed() >= PERSON_WAIT {
                    return Err(person_busy());
                }
                tokio::time::sleep(POLL).await;
            }
            let _turn = self.inner.turn.lock().await;
            let cua = &self.inner.cua;
            if !cua.has_agent() {
                return Ok(());
            }
            let holds = {
                let state = self.state();
                if state.person_active() {
                    continue;
                }
                state.agent_holds
            };
            if holds {
                if cua.cursor_shown() && !cua.cursor_lost() {
                    return Ok(());
                }
                // Cua lost the named cursor: the pointer first, then take it again.
                if !self.back(Why::Lost).await {
                    return Err(cursor_unavailable());
                }
            }
            match self.take().await {
                Taken::Yes => {
                    self.state().retake = None;
                    return Ok(());
                }
                Taken::PersonMoved => continue,
                Taken::Failed => return Err(cursor_unavailable()),
            }
        }
    }

    /// Draw the named cursor where the pointer is and, once Cua has drawn
    /// it, hide the pointer, and wait until Hyprland has: both in one place
    /// meanwhile, never neither. Holds `turn`.
    async fn take(&self) -> Taken {
        let (hypr, cua) = (&self.inner.hypr, &self.inner.cua);
        // A marker left over (a failed show, ibara 0.1.0-5's) is settled
        // first, so the settings read next are the person's own.
        if !self.show_pointer(None).await {
            return Taken::Failed;
        }
        let (Ok((invisible, timeout)), Some((x, y))) = (hypr.pointer_settings().await, self.pointer_at().await) else {
            return Taken::Failed;
        };
        let drawn = match cua.show_cursor(x, y).await {
            Ok(drawn) => drawn,
            Err(e) => {
                crate::controller::log_event("agent_cursor_show_failed", &e.message);
                cua.hide_cursor_now().await;
                return Taken::Failed;
            }
        };
        if !self.still_for(drawn).await {
            // The person moved the mouse meanwhile: it stays theirs.
            cua.hide_cursor_now().await;
            return Taken::PersonMoved;
        }
        if !invisible {
            let kept = self.keep(timeout);
            if !kept || hypr.hide_pointer().await.is_err() {
                self.show_pointer(None).await;
                cua.hide_cursor_now().await;
                return Taken::Failed;
            }
            self.state().agent_holds = true;
            if !self.still_for(HIDE_TICK).await {
                self.back(Why::Person).await;
                return Taken::PersonMoved;
            }
        }
        self.state().agent_holds = true;
        Taken::Yes
    }

    /// Wait `time`; false as soon as the person moves the mouse meanwhile.
    async fn still_for(&self, time: Duration) -> bool {
        let until = Instant::now() + time;
        loop {
            if self.state().person_active() {
                return false;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            tokio::time::sleep(left.min(POLL)).await;
        }
    }

    /// Start the watch task unless it runs; whether it was started.
    fn keep_watching(&self) -> bool {
        {
            let mut state = self.state();
            if state.watching {
                return false;
            }
            state.watching = true;
            // A reading from before the watch stopped is stale.
            state.seen = None;
        }
        tokio::spawn(self.clone().watch());
        true
    }

    /// Control ended: the person's pointer again, where the named cursor was.
    pub async fn release(&self) {
        let _turn = self.inner.turn.lock().await;
        if self.state().agent_holds {
            self.back(Why::Released).await;
        } else {
            // A pointer a failed show left hidden.
            self.show_pointer(None).await;
        }
    }

    /// Cua refused an input because the person used the mouse or keyboard.
    pub async fn person_moved(&self) {
        self.state().person_at = Some(Instant::now());
        self.keep_watching();
        let _turn = self.inner.turn.lock().await;
        if self.state().agent_holds {
            self.back(Why::Person).await;
        }
    }

    /// ibara starts or stops: nothing of the agent's is on the screen (its
    /// worker was stopped), so Hyprland's pointer and the person's own
    /// settings come back if ibara's marker says it hid the pointer.
    pub async fn reset(&self) {
        let _turn = self.inner.turn.lock().await;
        self.show_pointer(None).await;
        let mut state = self.state();
        state.agent_holds = false;
        state.retake = None;
    }

    /// ibara's own move of the pointer onto the named cursor before a point
    /// click, only while the agent holds the screen: once the person has it,
    /// their pointer stays where they put it.
    pub async fn pointer_to(&self, x: f64, y: f64) {
        let _turn = self.inner.turn.lock().await;
        if !(self.state().agent_holds && self.inner.cua.cursor_shown()) {
            return;
        }
        let _moving = self.inner.motion.begin();
        let _ = self.inner.hypr.move_cursor(x, y).await;
    }

    /// Give the screen to the person: the pointer first, then the named
    /// cursor hidden without waiting for Cua's call in flight. Whether the
    /// agent's hold ended; it goes on when the pointer could not be shown
    /// (the named cursor stays: a person's move shows the pointer only until
    /// it is still again while ibara's timeout is set), and the watch tries
    /// again. Holds `turn`.
    async fn back(&self, why: Why) -> bool {
        let cua = &self.inner.cua;
        // After a person's move the pointer is where they put it; a key press
        // left it hidden, and the show draws it there.
        let at = if why == Why::Person { None } else { self.meeting_point(why).await };
        if !self.show_pointer(at).await {
            return false;
        }
        if why == Why::Person {
            cua.yield_to_person();
        }
        cua.hide_cursor_now().await;
        self.state().agent_holds = false;
        true
    }

    /// Where the pointer comes back so that both cursors meet in one place:
    /// on the named cursor inside the focused window (moving the pointer
    /// elsewhere could move the keyboard focus); else where it is, and at the
    /// end of control the named cursor moves onto it first.
    async fn meeting_point(&self, why: Why) -> Option<(f64, f64)> {
        let cua = &self.inner.cua;
        let (x, y) = cua.cursor_at()?;
        let now = self.pointer_at().await?;
        if (now.0 - x).abs() < 1.0 && (now.1 - y).abs() < 1.0 {
            return None;
        }
        let hypr = &self.inner.hypr;
        let (monitors, windows, active) = tokio::join!(hypr.monitors(), hypr.clients(), hypr.active_window());
        if let (Ok(monitors), Ok(windows), Ok(Some(active))) = (monitors, windows, active) {
            let focused = active.address.clone();
            if super::window_under(x, y, Some(active), windows, &monitors).ok().map(|w| w.address) == Some(focused) {
                return Some((x, y));
            }
        }
        if why == Why::Released && cua.cursor_to_now(now.0, now.1).await {
            tokio::time::sleep(LAND).await;
        }
        None
    }

    /// Show Hyprland's pointer (at `at`, else where it is) and put the
    /// person's own settings back, if ibara's marker says it hid the pointer;
    /// false when that failed.
    async fn show_pointer(&self, at: Option<(f64, f64)>) -> bool {
        let Ok(saved) = std::fs::read_to_string(&self.inner.marker) else {
            return true;
        };
        // ibara's own move of the pointer onto the named cursor.
        let _moving = at.map(|_| self.inner.motion.begin());
        let hypr = &self.inner.hypr;
        let shown = match saved.trim().strip_prefix("inactive_timeout ").and_then(|t| t.parse::<f64>().ok()) {
            // ibara 0.1.0-5 and earlier left an empty marker and hid the
            // pointer with `cursor:invisible`.
            None => hypr.show_pointer(None, at, true).await,
            // The person's own timeout back only over ibara's: a config
            // reload or a new Hyprland session since (Hyprland exited while
            // an agent held control) put the person's current one in place.
            Some(own) => match self.live_timeout().await {
                Some(live) => hypr.show_pointer(ibaras(live).then_some(own), at, false).await,
                None => Err(crate::error::internal("cursor:inactive_timeout could not be read")),
            },
        };
        match shown {
            Ok(()) => {
                let _ = std::fs::remove_file(&self.inner.marker);
                true
            }
            Err(e) => {
                crate::controller::log_event("pointer_show_failed", &e.message);
                false
            }
        }
    }

    /// Keep the person's own `timeout` in the marker while ibara has the
    /// pointer hidden.
    fn keep(&self, timeout: f64) -> bool {
        std::fs::write(&self.inner.marker, format!("inactive_timeout {timeout}\n")).is_ok()
    }

    /// The person's timeout a config reload put in place of ibara's, which
    /// draws the pointer again at Hyprland's next cursor tick, while the
    /// marker says ibara has it hidden.
    async fn reloaded(&self) -> Option<f64> {
        if !self.inner.marker.exists() {
            return None;
        }
        let live = self.ask(b"j/getoption cursor:inactive_timeout").await?["float"].as_f64()?;
        (!ibaras(live)).then_some(live)
    }

    /// The live `cursor:inactive_timeout`.
    async fn live_timeout(&self) -> Option<f64> {
        match self.ask(b"j/getoption cursor:inactive_timeout").await {
            Some(option) => option["float"].as_f64(),
            None => self.inner.hypr.pointer_settings().await.ok().map(|(_, timeout)| timeout),
        }
    }

    /// While an agent holds control, or ibara still has the pointer hidden:
    /// the screen back to the person at once when they move the mouse or
    /// control ends, the pointer at once when Cua loses the named cursor and
    /// the named cursor again right after, and a failed show tried again.
    async fn watch(self) {
        loop {
            tokio::time::sleep(POLL).await;
            let cua = &self.inner.cua;
            {
                let mut state = self.state();
                if !state.agent_holds && !cua.has_agent() && !self.inner.marker.exists() {
                    state.watching = false;
                    state.retake = None;
                    return;
                }
            }
            // The timeout alongside the position: a config reload puts the
            // person's back in place of ibara's.
            let holding = self.state().agent_holds;
            let (moved, reloaded) = tokio::join!(self.moved_by_person(), async {
                if holding { self.reloaded().await } else { None }
            });
            if moved {
                self.state().person_at = Some(Instant::now());
            }
            let (holds, active, retake) = {
                let state = self.state();
                (state.agent_holds, state.person_active(), state.retake.is_some_and(|t| t <= Instant::now()))
            };
            let due = if holds {
                active || !cua.has_agent() || !cua.cursor_shown() || cua.cursor_lost() || reloaded.is_some()
            } else {
                self.inner.marker.exists() || (retake && cua.has_agent() && !active)
            };
            if due {
                self.tick().await;
            }
        }
    }

    /// The watch's one step, decided again under `turn`.
    async fn tick(&self) {
        let _turn = self.inner.turn.lock().await;
        let cua = &self.inner.cua;
        let (holds, active, retake) = {
            let state = self.state();
            (state.agent_holds, state.person_active(), state.retake.is_some_and(|t| t <= Instant::now()))
        };
        if holds {
            if active {
                self.back(Why::Person).await;
            } else if !cua.has_agent() {
                self.back(Why::Released).await;
            } else if !cua.cursor_shown() || cua.cursor_lost() {
                crate::controller::log_event("agent_cursor_lost", "the Cua worker exited; its cursor is drawn again");
                if self.back(Why::Lost).await {
                    self.state().retake = Some(Instant::now());
                }
            } else if let Some(timeout) = self.reloaded().await {
                // A config reload (`hyprctl reload`, a theme or font change, a
                // saved config file): its timeout is the person's now, and
                // the pointer is hidden again before Hyprland's next tick
                // draws it.
                crate::controller::log_event("pointer_hidden_again", "a Hyprland config reload put the person's cursor:inactive_timeout back");
                if !self.keep(timeout) {
                    crate::controller::log_event("pointer_marker_failed", "the reloaded cursor:inactive_timeout could not be kept");
                }
                if let Err(e) = self.inner.hypr.hide_pointer().await {
                    crate::controller::log_event("pointer_hide_failed", &e.message);
                }
            }
        } else if self.inner.marker.exists() {
            // A pointer a failed show left hidden.
            self.show_pointer(None).await;
        } else if retake && cua.has_agent() && !active {
            let taken = self.take().await;
            self.state().retake = (taken == Taken::Failed).then(|| Instant::now() + RETRY);
        }
    }

    /// Whether the pointer moved since the last reading while no agent call
    /// could move it. A reading during or across such a call starts again.
    async fn moved_by_person(&self) -> bool {
        let (starts, busy) = self.inner.motion.now();
        let at = self.pointer_at().await;
        let mut state = self.state();
        let Some(at) = at.filter(|_| busy == 0 && self.inner.motion.now().0 == starts) else {
            state.seen = None;
            return false;
        };
        let moved = matches!(state.seen, Some((seen, before)) if seen == starts && before != at);
        state.seen = Some((starts, at));
        moved
    }

    /// Where Hyprland's pointer is, in layout coordinates (`j/cursorpos`).
    async fn pointer_at(&self) -> Option<(f64, f64)> {
        let at = self.ask(b"j/cursorpos").await?;
        Some((at["x"].as_f64()?, at["y"].as_f64()?))
    }

    /// One JSON reading from Hyprland's request socket; the socket is found
    /// again after a failure.
    async fn ask(&self, request: &[u8]) -> Option<serde_json::Value> {
        let known = self.inner.socket.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let socket = match known {
            Some(socket) => socket,
            None => {
                let runtime = self.inner.runtime.as_ref()?;
                let found = super::watch::socket_path(&self.inner.hypr, runtime, ".socket.sock").await?;
                *self.inner.socket.lock().unwrap_or_else(|p| p.into_inner()) = Some(found.clone());
                found
            }
        };
        let reply = socket_request(&socket, request).await;
        if reply.is_none() {
            self.inner.socket.lock().unwrap_or_else(|p| p.into_inner()).take();
        }
        reply
    }
}

async fn socket_request(socket: &std::path::Path, request: &[u8]) -> Option<serde_json::Value> {
    let ask = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        stream.write_all(request).await.ok()?;
        let mut reply = Vec::with_capacity(96);
        stream.take(4096).read_to_end(&mut reply).await.ok()?;
        serde_json::from_slice(&reply).ok()
    };
    tokio::time::timeout(QUERY, ask).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    //! Through a real `Desktop`, with stand-ins for `hyprctl`, Hyprland's
    //! request socket and `cua-driver` that write every change to what the
    //! screen shows into one log. Failure cases, written first:
    //! - control begins and the person's pointer stays beside the agent's
    //!   cursor, or the agent's cursor starts anywhere but at the pointer;
    //! - both cursors show in different places, or neither shows, at the
    //!   start of control, around an input, a person's move, or its end;
    //! - the person's pointer comes back while the agent is idle, however
    //!   long (Cua's idle hide, the worker stopped for idleness);
    //! - Cua's worker exits and neither cursor shows, or the agent's cursor
    //!   does not come back;
    //! - a person's move does not bring their pointer back at once, or the
    //!   agent's cursor stays until a long read ends;
    //! - the agent's own pointer movement is taken for a person's;
    //! - agent input is sent while the person moves the mouse, or a person
    //!   who keeps moving it makes the agent wait forever; after they stop,
    //!   the agent's next input does not take the screen back where they left
    //!   the pointer;
    //! - after control ends (finish, pause, Take Control, revoke, expiry:
    //!   all clear the agent and release input), a late input is sent or
    //!   hides the pointer again, or the pointer comes back somewhere other
    //!   than where the agent's cursor was;
    //! - after a restart the pointer stays hidden, or the person's own
    //!   `cursor:inactive_timeout` is lost, or one the person set since ibara
    //!   hid the pointer is overridden with the old one; after control
    //!   begins over ibara 0.1.0-5's marker, ibara's timeout stays;
    //! - a Hyprland config reload while the agent holds control brings the
    //!   person's pointer back beside the agent's cursor, or loses the
    //!   timeout the reloaded config gave the person;
    //! - the pointer ibara moves onto the agent's cursor when it gives the
    //!   screen back is taken for the person's move, and delays the agent;
    //! - when showing the person's pointer fails, neither cursor shows;
    //! - a person's move while the agent types stops Cua's worker in the
    //!   middle of a piece (which crashes GTK 3 apps), lets the typing go on
    //!   past the piece in flight, or does not tell the agent how much was
    //!   typed;
    //! - the agent's cursor is drawn where no screen shows it: past a
    //!   screen's edge on its way to a point (a wide turn), at an element
    //!   off the screen, or between two screens on its way from one to the
    //!   other;
    //! - the agent's cursor fades while the agent is idle after a click;
    //! - keys or text reach a window other than the one they were sent to:
    //!   another window took the keyboard focus before they went out, or
    //!   between two typing pieces;
    //! - a menu item read from a GTK 3 window (whose labels end in spaces)
    //!   cannot be chosen, with its menu closed or open; or it is chosen
    //!   through accessibility's own click, which GTK 3 runs the item in (a
    //!   dialog it opens leaves the app's accessibility unanswered until it
    //!   closes); or Return goes before the item is highlighted, or after the
    //!   person took the screen, another window took the keyboard focus or
    //!   the menu closed while the agent's cursor went to the item;
    //! - the window's own open menu, which holds the keyboard, refuses
    //!   Escape, or a shortcut such as ctrl+s; or a key goes anywhere while
    //!   something else holds the keyboard (another app's popup, a drag).
    //! - typing or a key moves the real pointer; a key moves the agent's
    //!   cursor; typing moves it to a field no box proves, or sends the first
    //!   key before it arrives at a field whose box is known.
    use super::super::{ClickTarget, Desktop, DesktopConfig, ElementTarget, SurfaceId, TypingCursor};
    use super::*;
    use crate::desktop::input::Button;
    use crate::desktop::run::Cancel;
    use std::path::Path;

    const WINDOW: &str = r#"{"address":"0x1","mapped":true,"hidden":false,"at":[0,0],"size":[1000,800],"workspace":{"id":1,"name":"1"},"class":"app","title":"App","pid":100,"monitor":0}"#;
    /// A viewer of another computer, which takes the keyboard focus when it maps.
    const VIEWER: &str = r#"{"address":"0x2","mapped":true,"hidden":false,"at":[0,0],"size":[1920,1080],"workspace":{"id":1,"name":"1"},"class":"ibara-view","title":"Viewer","pid":200,"monitor":0}"#;
    const MONITORS: &str = r#"[{"id":0,"name":"HDMI-A-1","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"focused":true,"activeWorkspace":{"id":1,"name":"1"}}]"#;

    fn now_ns() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    /// An event with Cua's JSON numbers (`500.0`) written as Hyprland's (`500`).
    fn whole(event: &str) -> String {
        event.split(' ').map(|w| w.parse::<f64>().map_or_else(|_| w.to_string(), |n| n.to_string())).collect::<Vec<_>>().join(" ")
    }

    fn note_in(dir: &Path, what: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("screen.log"))?;
        writeln!(log, "{} {what}", now_ns())
    }

    struct Screen {
        dir: PathBuf,
        desktop: Desktop,
        surface: SurfaceId,
        _socket: tokio::task::JoinHandle<()>,
        /// No other test keeps a mutating helper in flight meanwhile.
        _helpers: tokio::sync::RwLockReadGuard<'static, ()>,
    }

    impl Screen {
        /// A desktop with one focused window at 0,0 (1000×800) and the
        /// person's pointer at `pointer`; an agent begins control.
        async fn new(pointer: (i64, i64)) -> Screen {
            let screen = Screen::idle(pointer).await;
            screen.note(&format!("person {} {}", pointer.0, pointer.1));
            screen.desktop.set_agent(Some("codex@test".into()));
            screen
        }

        /// The same desktop with no agent.
        async fn idle(pointer: (i64, i64)) -> Screen {
            let helpers = crate::desktop::run::HELPERS.read().await;
            // Short: a socket path has at most 107 bytes.
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!("/tmp/ibh-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
            std::fs::create_dir_all(dir.join("state")).unwrap();
            std::fs::create_dir_all(dir.join("hypr/sig")).unwrap();
            std::fs::write(dir.join("pos"), format!("{} {}", pointer.0, pointer.1)).unwrap();
            let d = dir.display();
            // Hyprland 0.56.2 as probed: cursor settings apply at its next
            // cursor tick (here 0.3 s later), which hides the pointer only if
            // it has been still 0.1 s; a pointer hidden by `inactive_timeout`
            // is drawn again at once by the one-eval show, and by a person's
            // move (`Screen::person_moves`), not by warps.
            super::super::write_script(
                &dir.join("hyprctl"),
                &format!(
                    r#"#!/bin/sh
dir='{d}'
log() {{ echo "$(date +%s%N) $*" >>"$dir/screen.log"; }}
later() {{ ( sleep 0.3; "$@" ) </dev/null >/dev/null 2>&1 & }}
# A hide is logged before it happens and a show after, so the log never
# claims a pointer state another writer (a person's move) has undone.
hide() {{ [ -f "$dir/invisible" ] || {{ log pointer hidden; touch "$dir/invisible"; }}; }}
show() {{ [ -f "$dir/invisible" ] && {{ rm -f "$dir/invisible"; log pointer shown; }}; }}
still() {{ [ $(( $(date +%s%N) - $(cat "$dir/person_at" 2>/dev/null || echo 0) )) -gt 100000000 ]; }}
hide_if_timeout() {{ [ "$(cat "$dir/timeout" 2>/dev/null)" = 0.1 ] && still && hide; }}
[ "$1" = -i ] && shift 2
case "$*" in
  "-j instances") echo '[{{"instance":"sig","pid":1}}]';;
  "-j getoption cursor:invisible") log settings read; if [ -f "$dir/own_invisible" ] || [ -f "$dir/set_invisible" ]; then b=true; else b=false; fi; echo "{{\"option\":\"cursor:invisible\",\"bool\":$b,\"set\":true}}";;
  "-j getoption cursor:inactive_timeout") echo "{{\"option\":\"cursor:inactive_timeout\",\"float\":$(cat "$dir/timeout" 2>/dev/null || echo 0),\"set\":true}}";;
  *"inactive_timeout = 0.1 "*) echo 0.1 >"$dir/timeout"; later hide_if_timeout;;
  *get_cursor_pos*)
    if [ -f "$dir/fail_show" ]; then rm -f "$dir/fail_show"; log pointer show failed; echo "error: hyprctl timed out"; exit 1; fi
    if [ "$(date +%s%N)" -lt "$(cat "$dir/fail_show_until" 2>/dev/null || echo 0)" ]; then log pointer show failed; echo "error: hyprctl timed out"; exit 1; fi
    t=$(printf '%s' "$*" | sed -n 's/.*inactive_timeout = \([0-9.]*\).*/\1/p'); [ -n "$t" ] && echo "$t" >"$dir/timeout"
    case "$*" in *"invisible = false"*) rm -f "$dir/set_invisible"; log invisible off;; esac
    xy=$(printf '%s' "$*" | sed -n 's/.*cursor.move({{ x = \(-*[0-9][0-9]*\), y = \(-*[0-9][0-9]*\).*/\1 \2/p')
    [ -n "$xy" ] && {{ echo "$xy" >"$dir/pos"; log warp $xy; }}
    show;;
  *"invisible = true"*) touch "$dir/set_invisible"; later hide;;
  *"invisible = false"*) rm -f "$dir/set_invisible"; later show;;
  *cursor.move*) xy=$(printf '%s' "$*" | sed -n 's/.*x = \(-*[0-9]*\), y = \(-*[0-9]*\).*/\1 \2/p'); echo "$xy" >"$dir/pos"; log warp $xy;;
  "-j clients") w=$(cat "$dir/window" 2>/dev/null || echo '{WINDOW}'); if [ -f "$dir/focus" ]; then echo "[$w,$(cat "$dir/focus")]"; else echo "[$w]"; fi;;
  "-j activewindow") if [ -f "$dir/focus" ]; then cat "$dir/focus"; else cat "$dir/window" 2>/dev/null || echo '{WINDOW}'; fi;;
  "-j monitors") if [ -f "$dir/monitors" ]; then cat "$dir/monitors"; else echo '{MONITORS}'; fi;;
  *send_shortcut*)
    # Into whatever holds the keyboard: the app's model (`menu.sh`) of an open menu.
    k=$(printf '%s' "$*" | sed -n "s/.*key = .\([A-Za-z_]*\).*/\1/p")
    n=$(printf '%s' "$*" | grep -o send_shortcut | wc -l)
    i=0; while [ "$i" -lt "$n" ]; do i=$((i + 1)); log hyprland key "$k"; done
    [ -f "$dir/menu.sh" ] && sh "$dir/menu.sh" key "$k" "$n";;
  eval*) ;;
  *) echo '[]';;
esac
exit 0
"#
                ),
            );
            // Cua as probed on Cua 0.29.1: a new session's cursor is drawn
            // where its last move put it once it is enabled; moving a drawn
            // or hidden cursor draws it there, and so does input with its
            // session. A point click moves the real pointer to its point (the
            // Hyprland plugin); an element click does not. The named cursor
            // goes with the process (the overlay is its own Wayland surface).
            // A read waits `read_delay` seconds and answers `elements`. Text
            // is typed key by key, `type_gap` seconds apart.
            //
            // Its motion as Cua 0.29.1 on Wayland has it: every field a
            // `set_agent_cursor_motion` leaves out takes Cua's default (glide
            // by speed, idle hide after 20 s, here 1.5 s, turns of 80 px), not
            // the session's current value. A glide (anything but 1 ms, which
            // lands in one frame) first runs on along the cursor's heading
            // for about its turn radius, as the first turn of Cua's path does,
            // then straight to its point; each point on the way is logged.
            super::super::write_script(
                &dir.join("cua-driver"),
                &format!(
                    r#"#!/bin/sh
dir='{d}'
log() {{ echo "$(date +%s%N) $*" >>"$dir/screen.log"; }}
drawn() {{
  [ -f "$dir/named" ] || {{ touch "$dir/named"; log named on; }}
  idle=$(cat "$dir/idle" 2>/dev/null || echo 1500); [ "$idle" = 0 ] && return
  stamp=$(date +%s%N); echo "$stamp" >"$dir/active"
  ( sleep "$(awk -v ms="$idle" 'BEGIN {{ print ms / 1000 }}')"; [ "$(cat "$dir/active")" = "$stamp" ] && hidden ) 5>&- </dev/null >/dev/null 2>&1 &
}}
hidden() {{ [ -f "$dir/named" ] && {{ rm -f "$dir/named"; log named off; }}; }}
arg() {{ printf '%s' "$line" | sed -n "s/.*\"$1\":\([-0-9.e]*\).*/\1/p"; }}
num() {{ awk -v v="$1" 'BEGIN {{ print v + 0 }}'; }}
go() {{
  from=$(cat "$dir/named_xy" 2>/dev/null)
  if [ -n "$from" ] && [ "$(cat "$dir/glide" 2>/dev/null || echo 0)" != 1 ]; then
    awk -v from="$from" -v prev="$(cat "$dir/prev_xy" 2>/dev/null)" -v to="$1 $2" -v r="$(cat "$dir/turn" 2>/dev/null || echo 80)" 'BEGIN {{
      split(from, f, " "); split(to, t, " ")
      if (prev != "" && r > 1) {{
        split(prev, p, " "); dx = f[1] - p[1]; dy = f[2] - p[2]; d = sqrt(dx * dx + dy * dy)
        if (d > 0) print f[1] + r * dx / d, f[2] + r * dy / d
      }}
      for (i = 1; i < 4; i++) print f[1] + (t[1] - f[1]) * i / 4, f[2] + (t[2] - f[2]) * i / 4
    }}' | while read -r px py; do log named at $px $py; done
  fi
  [ -n "$from" ] && echo "$from" >"$dir/prev_xy"
  echo "$1 $2" >"$dir/named_xy"
  log named at $1 $2
}}
case "$1" in
telemetry) echo '{{"enabled":false,"source":"environment"}}';;
mcp)
  echo $$ >"$dir/worker.pid"
  alive="$dir/alive.$$"; mkfifo "$alive"
  ( read -r _ <"$alive"; hidden ) >/dev/null 2>&1 &
  exec 5>"$alive"; rm -f "$alive"
  while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    [ -n "$id" ] || continue
    tool=$(printf '%s' "$line" | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p')
    reply='"result":{{"content":[],"isError":false}}'
    case "$tool" in
      start_session) rm -f "$dir/appeared" "$dir/moved" "$dir/enabled" "$dir/glide" "$dir/idle" "$dir/turn";;
      set_agent_cursor_motion)
        g=$(arg glide_duration_ms); i=$(arg idle_hide_ms); r=$(arg turn_radius)
        num "${{g:-0}}" >"$dir/glide"; num "${{i:-1500}}" >"$dir/idle"; num "${{r:-80}}" >"$dir/turn";;
      set_agent_cursor_enabled)
        case "$line" in
          *'"enabled":true'*) touch "$dir/enabled"; [ -f "$dir/moved" ] && {{ touch "$dir/appeared"; drawn; }};;
          *) rm -f "$dir/enabled"; hidden;;
        esac;;
      end_session) rm -f "$dir/enabled" "$dir/appeared"; hidden;;
      move_cursor)
        xy=$(printf '%s' "$line" | sed -n 's/.*"x":\([-0-9.]*\),"y":\([-0-9.]*\).*/\1 \2/p')
        go $xy
        touch "$dir/moved"
        [ -f "$dir/appeared" ] && drawn;;
      get_window_state)
        log read
        [ -f "$dir/read_delay" ] && sleep "$(cat "$dir/read_delay")" 5>&- </dev/null >/dev/null 2>&1
        [ -f "$dir/elements" ] && reply='"result":{{"content":[],"isError":false,"structuredContent":'"$(cat "$dir/elements")"'}}';;
      type_text)
        # With the agent's session Cua shows its typing animation.
        case "$line" in *'"session"'*) log cue text;; esac
        text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
        gap=$(cat "$dir/type_gap" 2>/dev/null || echo 0)
        # What arrived goes to `field`. A `stop_at` line "N why" stops Cua
        # once the field holds N characters, as Cua 0.29.1 stops: `display`
        # when Hyprland's displays change (its plugin ends the input
        # connection before the next key), `transport` when the connection
        # fails with a key in flight (that key arrives unacknowledged),
        # `grab` when a popup takes the keyboard (the plugin refuses the next
        # key). `focus_on_stop` then takes the keyboard focus.
        at='' why=''; [ -s "$dir/stop_at" ] && read -r at why <"$dir/stop_at"
        i=0
        while [ "$i" -lt "${{#text}}" ]; do
          if [ -n "$at" ] && [ "$(wc -c <"$dir/field" 2>/dev/null || echo 0)" -ge "$at" ]; then
            sed -i 1d "$dir/stop_at"
            [ -f "$dir/focus_on_stop" ] && mv "$dir/focus_on_stop" "$dir/focus"
            detail='Hyprland input connection failed'
            case "$why" in
              transport)
                printf '%s' "$text" | cut -c$((i + 1)) | tr -d '\n' >>"$dir/field"; log typed
                reason=transport_or_protocol_error;;
              grab) reason=primary_target_busy detail=foreground_grab;;
              *) reason=text_interrupted;;
            esac
            if [ "$i" -gt 0 ]; then
              reply='"result":{{"isError":true,"content":[{{"type":"text","text":"foreground_unavailable ('$reason'): '"$detail"'"}}],"structuredContent":{{"ok":false,"code":"foreground_unavailable","reason":"'$reason'","detail":"'"$detail"'","effect":"partial","delivery":{{"mode":"foreground","delivered_count":'$i'}},"route":"global_input","verified":false}}}}'
            else
              reply='"result":{{"isError":true,"content":[{{"type":"text","text":"foreground_unavailable: Hyprland input connection failed"}}],"structuredContent":{{"ok":false,"code":"foreground_unavailable","reason":"transport_or_protocol_error","detail":"Hyprland input connection failed","effect":"unverifiable"}}}}'
            fi
            break
          fi
          [ "$gap" = 0 ] || sleep "$gap" 5>&-
          printf '%s' "$text" | cut -c$((i + 1)) | tr -d '\n' >>"$dir/field"
          i=$((i + 1)); log typed
        done;;
      press_key|hotkey)
        case "$line" in *'"session"'*) log cue key;; esac
        if [ -f "$dir/grab" ]; then
          # A menu or popup holds the keyboard: Cua 0.28.2's plugin refuses.
          reply='"result":{{"isError":true,"content":[{{"type":"text","text":"foreground_unavailable (primary_target_busy): foreground_grab"}}],"structuredContent":{{"code":"foreground_unavailable","reason":"primary_target_busy","detail":"foreground_grab","effect":"refused","ok":false}}}}'
        else
          log key begins
          [ -f "$dir/key_delay" ] && sleep "$(cat "$dir/key_delay")"
          if [ -f "$dir/interrupt" ]; then
            reply='"result":{{"isError":true,"content":[{{"type":"text","text":"refused"}}],"structuredContent":{{"code":"foreground_unavailable","reason":"primary_target_busy","detail":"foreground_interrupted","effect":"refused","ok":false}}}}'
          else
            log key
          fi
        fi;;
      click|double_click|right_click)
        case "$line" in
          *'"element_token"'*)
            case "$line" in *'"session"'*) [ -f "$dir/appeared" ] && drawn; log named at $(cat "$dir/named_xy" 2>/dev/null || echo 0 0);; esac
            token=$(printf '%s' "$line" | sed -n 's/.*"element_token":"\([^"]*\)".*/\1/p')
            echo "$token" >>"$dir/clicked"
            # `refuse` holds Cua's refusal of the accessibility route (its structured content).
            if [ -f "$dir/refuse" ]; then
              reply='"result":{{"isError":true,"content":[{{"type":"text","text":"refused"}}],"structuredContent":'"$(cat "$dir/refuse")"'}}'
            else
              [ -f "$dir/menu.sh" ] && sh "$dir/menu.sh" click "$token"
              log click
            fi;;
          *)
            xy=$(printf '%s' "$line" | sed -n 's/.*"x":\([-0-9.]*\),"y":\([-0-9.]*\).*/\1 \2/p')
            case "$line" in *'"session"'*) [ -f "$dir/appeared" ] && drawn; go $xy;; esac
            for step in 1 2 3; do sleep 0.05; echo "$xy" >"$dir/pos"; log plugin $xy; done
            log pointer $tool $xy
            log click;;
        esac;;
    esac
    printf '{{"jsonrpc":"2.0","id":%s,%s}}\n' "$id" "$reply"
  done;;
esac
"#
                ),
            );
            let socket = std::os::unix::net::UnixListener::bind(dir.join("hypr/sig/.socket.sock")).unwrap();
            socket.set_nonblocking(true).unwrap();
            let socket = tokio::net::UnixListener::from_std(socket).unwrap();
            let pos = dir.join("pos");
            let timeout = dir.join("timeout");
            let serve = tokio::spawn(async move {
                while let Ok((mut stream, _)) = socket.accept().await {
                    let (pos, timeout) = (pos.clone(), timeout.clone());
                    tokio::spawn(async move {
                        let mut ask = [0u8; 64];
                        let n = stream.read(&mut ask).await.unwrap_or(0);
                        let reply = match &ask[..n] {
                            b"j/cursorpos" => {
                                let text = std::fs::read_to_string(&pos).unwrap_or_default();
                                let mut xy = text.split_whitespace().map(|v| v.parse::<f64>().unwrap_or(0.0));
                                let (x, y) = (xy.next().unwrap_or(0.0), xy.next().unwrap_or(0.0));
                                format!("{{\"x\": {x}, \"y\": {y}}}")
                            }
                            b"j/getoption cursor:inactive_timeout" => {
                                let t = std::fs::read_to_string(&timeout).ok().and_then(|t| t.trim().parse::<f64>().ok()).unwrap_or(0.0);
                                format!("{{\"option\": \"cursor:inactive_timeout\", \"float\": {t:.6}, \"set\": true }}")
                            }
                            _ => return,
                        };
                        let _ = stream.write_all(reply.as_bytes()).await;
                    });
                }
            });
            let mut cfg = DesktopConfig::new(&dir, &dir, &dir.join("state"));
            cfg.hyprctl = dir.join("hyprctl");
            cfg.cua = dir.join("cua-driver");
            cfg.env.push(("XDG_RUNTIME_DIR".into(), dir.clone().into()));
            let desktop = Desktop::new(cfg);
            let surface = SurfaceId { address: "0x1".into(), pid: 100, class: "app".into() };
            Screen { dir, desktop, surface, _socket: serve, _helpers: helpers }
        }

        /// The person moves the mouse to `(x, y)`. Hyprland draws a pointer
        /// hidden by ibara's `inactive_timeout` again at once, and hides it
        /// again at a tick once the mouse is still if the timeout is still
        /// ibara's.
        fn person_moves(&self, x: i64, y: i64) {
            let at = now_ns().to_string();
            std::fs::write(self.dir.join("person_at"), &at).unwrap();
            std::fs::write(self.dir.join("pos"), format!("{x} {y}")).unwrap();
            self.note(&format!("person {x} {y}"));
            let ours = |dir: &Path| std::fs::read_to_string(dir.join("timeout")).is_ok_and(|t| t.trim() == "0.1");
            if ours(&self.dir) && std::fs::remove_file(self.dir.join("invisible")).is_ok() {
                self.note("pointer shown");
            }
            let dir = self.dir.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                let still = std::fs::read_to_string(dir.join("person_at")).is_ok_and(|p| p == at);
                if still && ours(&dir) && !dir.join("invisible").exists() {
                    // Logged before it happens, as the `hyprctl` stand-in does.
                    let _ = note_in(&dir, "pointer hidden");
                    let _ = std::fs::write(dir.join("invisible"), "");
                }
            });
        }

        /// Hyprland loads its config again (`hyprctl reload`, a theme or
        /// font change, a saved config file) and it says `timeout`: that
        /// replaces ibara's, and at its next cursor tick Hyprland draws a
        /// pointer hidden by ibara's timeout again unless ibara's is back.
        fn reload(&self, timeout: &str) {
            std::fs::write(self.dir.join("timeout"), timeout).unwrap();
            self.note("reload");
            let dir = self.dir.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                let ours = std::fs::read_to_string(dir.join("timeout")).is_ok_and(|t| t.trim() == "0.1");
                if !ours && std::fs::remove_file(dir.join("invisible")).is_ok() {
                    let _ = note_in(&dir, "pointer shown");
                }
            });
        }

        fn timeout(&self) -> String {
            std::fs::read_to_string(self.dir.join("timeout")).unwrap().trim().to_string()
        }

        fn marker(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.join("state/pointer-hidden")).ok()
        }

        fn note(&self, what: &str) {
            note_in(&self.dir, what).unwrap();
        }

        fn log(&self) -> Vec<(u128, String)> {
            let mut lines: Vec<(u128, String)> = std::fs::read_to_string(self.dir.join("screen.log"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.split_once(' ').and_then(|(t, e)| Some((t.parse().ok()?, whole(e)))))
                .collect();
            lines.sort_by_key(|(t, _)| *t);
            lines
        }

        fn pointer_shown(&self) -> bool {
            !self.dir.join("invisible").exists()
        }

        fn named_shown(&self) -> bool {
            self.dir.join("named").exists()
        }

        /// Only the agent's cursor.
        fn agents(&self) -> bool {
            !self.pointer_shown() && self.named_shown()
        }

        /// Only the person's pointer.
        fn persons(&self) -> bool {
            self.pointer_shown() && !self.named_shown()
        }

        fn events(&self, name: &str) -> usize {
            self.log().iter().filter(|(_, e)| e == name).count()
        }

        /// Where the named cursor was last sent.
        fn named_at(&self) -> String {
            self.log().iter().rev().find_map(|(_, e)| e.strip_prefix("named at ").map(str::to_string)).unwrap_or_default()
        }

        fn pointer_at(&self) -> String {
            std::fs::read_to_string(self.dir.join("pos")).unwrap().trim().to_string()
        }

        async fn key(&self, cancel: Option<&Cancel>) -> crate::error::Result<()> {
            self.desktop.key(&self.surface, "ctrl+s", cancel).await
        }

        /// Another window opens and takes the keyboard focus, as a viewer
        /// of another computer does when it maps.
        fn focus_moves_away(&self) {
            std::fs::write(self.dir.join("focus"), VIEWER).unwrap();
            self.note("focus moved");
        }

        /// What arrived in the focused field, in order.
        fn field(&self) -> String {
            std::fs::read_to_string(self.dir.join("field")).unwrap_or_default()
        }

        /// What ending control does (finish, pause, Take Control, revoke,
        /// expiry): clear the agent, then release input. Its settle waits
        /// for every input helper in this process.
        async fn end_control(&self) {
            self.desktop.set_agent(None);
            self.desktop.release_input().await.unwrap();
        }

        /// Wait up to `limit` for `done`; how long it took.
        async fn until(&self, limit: Duration, done: impl Fn(&Screen) -> bool) -> Option<Duration> {
            let start = Instant::now();
            while start.elapsed() < limit {
                if done(self) {
                    return Some(start.elapsed());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            None
        }

        /// Control has begun: the agent's cursor alone.
        async fn taken(&self) {
            if self.until(Duration::from_secs(5), Screen::agents).await.is_none() {
                panic!("the agent's cursor alone: {:#?}", self.log());
            }
        }

        /// When `name` was logged, in order.
        fn times(&self, name: &str) -> Vec<u128> {
            self.log().into_iter().filter(|(_, e)| e == name).map(|(t, _)| t).collect()
        }

        /// Cua's next reads answer one button, "Save", at `x`,300 (80×30).
        fn one_button(&self, x: i64) -> ElementTarget {
            self.button_at(x, 300)
        }

        /// Cua's next reads answer one button, "Save", at `x`,`y` (80×30).
        fn button_at(&self, x: i64, y: i64) -> ElementTarget {
            let elements = format!(r#"{{"elements":[{{"element_index":0,"role":"push button","label":"Save","element_token":"t1","frame":{{"x":{x},"y":{y},"w":80,"h":30}},"actions":["click"]}}],"elements_complete":true}}"#);
            std::fs::write(self.dir.join("elements"), elements).unwrap();
            ElementTarget {
                surface: self.surface.clone(),
                selector: serde_json::json!({"pid": 100, "window_id": 1, "identity": {"path": [], "role": "push button", "label": "Save", "ordinal": 1}}),
                role: "push button".into(),
                name: "Save".into(),
                actions: vec!["click".into()],
            }
        }

        /// Mousepad's File menu as Cua 0.28.2 read it on a test computer,
        /// answering as GTK 3 does (`menu.sh`, which Cua's and Hyprland's
        /// stand-ins run): each label ends in spaces, and only an open
        /// menu's items have bounds. Accessibility's click on File opens it
        /// with its first item highlighted; accessibility's click on an item
        /// chooses it. An open menu holds the keyboard (Cua refuses keys),
        /// and keys sent through Hyprland reach it: Down and Up walk the
        /// highlight, Return chooses the highlighted item, Escape closes it.
        /// A chosen item is logged as `activated <label>`, with `by
        /// accessibility` when accessibility's click chose it.
        fn mousepad_menu(&self, open: bool) {
            let menu = self.dir.join("menu.sh");
            super::super::write_script(
                &menu,
                &format!(
                    r#"#!/bin/sh
dir='{d}'
log() {{ echo "$(date +%s%N) $*" >>"$dir/screen.log"; }}
label() {{ case "$1" in 1) echo New;; 2) echo Save;; 3) echo 'Save As...';; esac; }}
close() {{ rm -f "$dir/menu" "$dir/grab"; sel=0; }}
sel=$(cat "$dir/selected" 2>/dev/null || echo 0)
case "$1" in
  click)
    case "$2" in
      t0) touch "$dir/menu" "$dir/grab"; sel=1;;
      t[1-3]) log activated "$(label "${{2#t}}")" by accessibility; close;;
    esac;;
  key)
    if [ -f "$dir/menu" ]; then
      case "$2" in
        Escape) close;;
        Down) sel=$((sel + $3)); [ "$sel" -gt 3 ] && sel=3;;
        Up) sel=$((sel - $3)); [ "$sel" -lt 1 ] && sel=1;;
        Return) log activated "$(label "$sel")"; close;;
      esac
    fi;;
esac
echo "$sel" >"$dir/selected"
item() {{
  frame=; selected=false
  if [ -f "$dir/menu" ]; then
    frame=",\"frame\":{{\"x\":18,\"y\":$((150 + 27 * $1)),\"w\":317,\"h\":27}}"
    [ "$sel" = "$1" ] && selected=true
  fi
  printf '{{"element_index":%s,"parent_index":0,"depth":5,"role":"menu item","label":"%s      ","element_token":"t%s","actions":["click"],"enabled":true,"selected":%s%s}}' "$1" "$(label "$1")" "$1" "$selected" "$frame"
}}
printf '{{"elements":[{{"element_index":0,"depth":4,"role":"menu","label":"File","element_token":"t0","frame":{{"x":12,"y":38,"w":42,"h":27}},"actions":["click"],"enabled":true}},%s,%s,%s,{{"element_index":4,"depth":3,"role":"page tab","element_token":"t4","enabled":true,"selected":true}},{{"element_index":5,"parent_index":4,"depth":5,"role":"text","label":"hello","element_token":"t5","frame":{{"x":13,"y":68,"w":900,"h":700}},"enabled":true}}],"elements_complete":true}}' "$(item 1)" "$(item 2)" "$(item 3)" >"$dir/elements.new"
mv "$dir/elements.new" "$dir/elements"
"#,
                    d = self.dir.display()
                ),
            );
            for held in ["grab", "menu"] {
                if open {
                    std::fs::write(self.dir.join(held), "").unwrap();
                } else {
                    let _ = std::fs::remove_file(self.dir.join(held));
                }
            }
            std::fs::write(self.dir.join("selected"), if open { "1" } else { "0" }).unwrap();
            assert!(std::process::Command::new("sh").arg(&menu).arg("show").status().unwrap().success());
        }

        /// Keys sent through Hyprland, in order.
        fn hyprland_keys(&self) -> Vec<String> {
            self.log().into_iter().filter_map(|(_, e)| e.strip_prefix("hyprland key ").map(str::to_string)).collect()
        }

        /// The agent's cursor is never drawn where no screen shows it: on
        /// its way anywhere, or at rest.
        fn assert_on_screen(&self) {
            let monitors = std::fs::read_to_string(self.dir.join("monitors")).unwrap_or_else(|_| MONITORS.into());
            let monitors: Vec<crate::desktop::hyprland::Monitor> = serde_json::from_str(&monitors).unwrap();
            let screens: Vec<_> = monitors.iter().map(|m| m.logical_rect()).collect();
            let shown = |(x, y): (f64, f64)| {
                screens.iter().any(|r| x >= r.x as f64 && y >= r.y as f64 && x <= (r.x + r.width) as f64 && y <= (r.y + r.height) as f64)
            };
            let log = self.log();
            let (mut drawn, mut at) = (false, None::<(f64, f64)>);
            for (t, event) in &log {
                match event.as_str() {
                    "named on" => drawn = true,
                    "named off" => drawn = false,
                    e if e.starts_with("named at ") => {
                        let mut v = e["named at ".len()..].split_whitespace().map(|n| n.parse::<f64>().unwrap());
                        at = Some((v.next().unwrap(), v.next().unwrap()));
                    }
                    _ => continue,
                }
                if let (true, Some(p)) = (drawn, at) {
                    assert!(shown(p), "the agent's cursor drawn at {p:?}, where no screen shows it ({t}): {log:#?}");
                }
            }
        }

        /// Exactly one cursor at every moment: never neither, except for
        /// `crash` after a note "crash" (a Cua worker that died took its
        /// cursor with it); both only in one place while Hyprland's tick
        /// hides the pointer, or apart only for the moment after a person's
        /// move before the named cursor goes.
        fn assert_one_cursor_but(&self, crash: Duration) {
            let (mut pointer, mut named) = (true, false);
            let (mut at, mut named_at) = ((0.0f64, 0.0f64), None::<(f64, f64)>);
            let (mut since, mut person, mut crashed) = (0u128, 0u128, 0u128);
            let log = self.log();
            let xy = |rest: &str| {
                let mut v = rest.split_whitespace().map(|n| n.parse::<f64>().unwrap());
                (v.next().unwrap(), v.next().unwrap())
            };
            let ms = |a: u128, b: u128| Duration::from_nanos(a.saturating_sub(b) as u64);
            for (t, event) in &log {
                let lasted = ms(*t, since);
                let apart = named_at.is_none_or(|n| (n.0 - at.0).abs() > 1.0 || (n.1 - at.1).abs() > 1.0);
                if !pointer && !named && since != 0 && !lasted.is_zero() {
                    assert!(crashed != 0 && ms(*t, crashed) <= crash, "no cursor for {lasted:?} before {event} at {t}: {log:#?}");
                }
                if pointer && named && !lasted.is_zero() {
                    let fine = if apart { person != 0 && ms(*t, person) <= Duration::from_millis(1000) } else { lasted <= Duration::from_millis(1200) };
                    assert!(fine, "two cursors {:?} for {lasted:?} before {event} at {t}: {log:#?}", if apart { "apart" } else { "together" });
                }
                match event.as_str() {
                    "pointer hidden" => pointer = false,
                    "pointer shown" => pointer = true,
                    "named on" => named = true,
                    "named off" => named = false,
                    "crash" => crashed = *t,
                    e if e.starts_with("named at ") => named_at = Some(xy(&e["named at ".len()..])),
                    e if e.starts_with("warp ") => at = xy(&e["warp ".len()..]),
                    e if e.starts_with("plugin ") => at = xy(&e["plugin ".len()..]),
                    e if e.starts_with("person ") => {
                        at = xy(&e["person ".len()..]);
                        person = *t;
                    }
                    _ => continue,
                }
                since = *t;
            }
        }

        fn assert_one_cursor(&self) {
            self.assert_one_cursor_but(Duration::ZERO);
        }
    }

    impl Drop for Screen {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn control_begins_with_the_agents_cursor_on_the_pointer_and_keeps_it_while_the_agent_is_idle() {
        let screen = Screen::idle((500, 400)).await;
        screen.note("person 500 400");
        screen.desktop.cua.stop_when_idle(Duration::from_millis(800));
        screen.desktop.set_agent(Some("codex@test".into()));
        // No input yet: control alone hands the screen over.
        screen.taken().await;
        assert_eq!(screen.named_at(), "500 400", "it starts where the person's pointer is");
        screen.key(None).await.unwrap();
        // The agent thinks for longer than Cua's idle stop.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(screen.agents(), "the agent's cursor alone while it thinks: {:#?}", screen.log());
        assert_eq!(screen.events("named off"), 0, "never hidden meanwhile");
        assert!(screen.desktop.cua.running().await, "the worker that draws it keeps running");
        screen.key(None).await.unwrap();
        assert_eq!(screen.events("pointer shown"), 0, "the pointer never came back: {:#?}", screen.log());
        assert_eq!(screen.events("key"), 2);
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn when_cuas_worker_exits_the_pointer_shows_at_once_and_the_agents_cursor_comes_back_on_it() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let pid: i32 = std::fs::read_to_string(screen.dir.join("worker.pid")).unwrap().trim().parse().unwrap();
        screen.note("crash");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        screen.until(Duration::from_secs(2), |s| !s.named_shown()).await.expect("the overlay went with the worker");
        let back = screen.until(Duration::from_secs(2), Screen::pointer_shown).await;
        assert!(back.is_some(), "the pointer at once: {:#?}", screen.log());
        screen.until(Duration::from_secs(10), |s| s.agents() && s.events("named on") == 2).await.expect("the agent's cursor again");
        assert_eq!(screen.named_at(), "500 400", "drawn again where the pointer is");
        screen.key(None).await.unwrap();
        screen.assert_one_cursor_but(Duration::from_secs(2));
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn when_cuas_worker_exits_away_from_the_pointer_its_cursor_comes_back_without_waiting_for_a_person() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let save = screen.one_button(200);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        assert_eq!(screen.named_at(), "240 315");
        let pid: i32 = std::fs::read_to_string(screen.dir.join("worker.pid")).unwrap().trim().parse().unwrap();
        screen.note("crash");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        screen.until(Duration::from_secs(10), |s| s.agents() && s.events("named on") == 2).await.expect("the agent's cursor again");
        // ibara moved the pointer onto the agent's cursor: not a person's
        // move, so the take again (it reads the person's settings first)
        // does not wait for the mouse to be still. Starting Cua's worker
        // after that is not timed: it is slow under load.
        let warp = screen.times("warp 240 315")[0];
        let retake = screen.times("settings read").into_iter().find(|t| *t > warp).expect("taken again");
        let waited = Duration::from_nanos((retake - warp) as u64);
        assert!(waited < PERSON_STILL, "taken again {waited:?} after the pointer came back: {:#?}", screen.log());
        assert_eq!(screen.named_at(), "240 315", "drawn again where the pointer came back");
        screen.key(None).await.unwrap();
        screen.assert_one_cursor_but(Duration::from_secs(2));
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_config_reload_while_the_agent_holds_control_keeps_the_pointer_hidden_and_the_persons_new_timeout() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let save = screen.one_button(200);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        // The agent's cursor is at the button and the hidden pointer where
        // the person left it. The person's config now says 3 s.
        screen.reload("3");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(screen.agents(), "the agent's cursor alone after the reload: {:#?}", screen.log());
        assert_eq!(screen.events("pointer shown"), 0, "{:#?}", screen.log());
        assert_eq!(screen.timeout(), "0.1");
        assert_eq!(screen.marker().as_deref(), Some("inactive_timeout 3\n"), "the reloaded config's timeout is the person's now");
        screen.key(None).await.unwrap();
        screen.end_control().await;
        assert!(screen.persons(), "{:#?}", screen.log());
        assert_eq!(screen.timeout(), "3", "the person's timeout from the reloaded config");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_who_moves_the_mouse_gets_their_pointer_at_once_and_the_agents_next_input_takes_it_back_there() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.key(None).await.unwrap();
        // Between the agent's inputs, the person moves the mouse.
        screen.person_moves(620, 450);
        let back = screen.until(Duration::from_secs(2), Screen::persons).await;
        assert!(back.is_some_and(|b| b < Duration::from_millis(1000)), "the person's pointer at once: {back:?}");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(screen.persons(), "and it stays theirs, still: {:#?}", screen.log());
        // The agent's next input waits while the person keeps moving it.
        let keys = screen.events("key");
        let moving = async {
            for i in 0..6 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                screen.person_moves(630 + i * 10, 450);
            }
        };
        let started = Instant::now();
        let (sent, ()) = tokio::join!(screen.key(None), moving);
        sent.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1500), "sent only once the mouse was still for a second: {:?}", started.elapsed());
        assert_eq!(screen.events("key"), keys + 1);
        assert_eq!(screen.named_at(), "680 450", "the agent's cursor comes back where the person left the pointer");
        assert!(screen.agents(), "and the pointer is hidden again: {:#?}", screen.log());
        screen.assert_one_cursor();

        // A person who keeps moving it: refused after a few seconds, nothing sent.
        let keys = screen.events("key");
        screen.person_moves(700, 450);
        let busy = async {
            for i in 0..45 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                screen.person_moves(700 + i, 460);
            }
        };
        let (refused, ()) = tokio::join!(screen.key(None), busy);
        let refused = refused.unwrap_err();
        assert_eq!(refused.details["execution_not_started"], serde_json::json!(true), "{refused:?}");
        assert!(refused.message.contains("person is using the mouse"), "{}", refused.message);
        assert_eq!(screen.events("key"), keys, "nothing sent while the person had the mouse");
        assert!(screen.persons());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn the_agents_own_clicks_move_the_pointer_without_giving_the_screen_back() {
        let screen = Screen::new((100, 100)).await;
        screen.taken().await;
        for (x, y) in [(400.0, 300.0), (700.0, 500.0), (200.0, 650.0)] {
            let target = ClickTarget::Point { x, y, surface: Some(screen.surface.clone()) };
            screen.desktop.click(&target, Button::Left, false, None).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(screen.events("click"), 3);
        assert_eq!(screen.events("pointer shown"), 0, "the agent's clicks are not a person's move: {:#?}", screen.log());
        assert!(screen.agents());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn when_control_ends_mid_input_the_persons_pointer_returns_and_no_late_input_is_sent() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.key(None).await.unwrap();
        std::fs::write(screen.dir.join("key_delay"), "0.5").unwrap();
        let cancel = Cancel::new();
        let steps = async {
            screen.key(Some(&cancel)).await.unwrap();
            // The step goes on after the key it was waiting for.
            screen.key(Some(&cancel)).await
        };
        let takeover = async {
            // Once the second key is on its way. What ending control does
            // (finish, pause, Take Control, revoke, expiry): cancel, clear the
            // agent, release input.
            screen.until(Duration::from_secs(10), |s| s.events("key begins") == 2).await.expect("the key started");
            cancel.cancel();
            screen.end_control().await;
        };
        let (late, ()) = tokio::join!(steps, takeover);
        assert_eq!(late.unwrap_err().details["execution_not_started"], serde_json::json!(true));
        assert_eq!(screen.events("key"), 2, "only the key already on its way");
        assert!(screen.persons(), "{:#?}", screen.log());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(screen.persons(), "and it stays so");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn when_control_ends_the_pointer_comes_back_where_the_agents_cursor_was() {
        // Inside the focused window: the pointer comes back on the named cursor.
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let save = screen.one_button(200);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        assert_eq!(screen.named_at(), "240 315", "the agent's cursor went to the button");
        screen.end_control().await;
        assert!(screen.persons(), "{:#?}", screen.log());
        assert_eq!(screen.pointer_at(), "240 315", "the pointer where the agent's cursor was");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
        drop(screen);

        // Outside it (moving the pointer there could move the keyboard
        // focus): the named cursor goes to the pointer first.
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let save = screen.one_button(1400);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        assert_eq!(screen.named_at(), "1440 315");
        screen.end_control().await;
        assert!(screen.persons(), "{:#?}", screen.log());
        assert_eq!(screen.pointer_at(), "500 400", "the pointer is not moved");
        let met = screen.times("named at 500 400");
        let shown = screen.times("pointer shown");
        assert!(met.last().zip(shown.last()).is_some_and(|(m, s)| m < s), "the agent's cursor on the pointer before it shows: {:#?}", screen.log());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_interrupting_a_key_gets_their_pointer_where_they_put_it() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.key(None).await.unwrap();
        // Cua's plugin refuses the next key: the person moved the mouse.
        std::fs::write(screen.dir.join("interrupt"), "").unwrap();
        screen.person_moves(520, 410);
        let refused = screen.key(None).await.unwrap_err();
        assert_eq!(refused.details.get("reason"), Some(&serde_json::json!("interrupted")), "{refused:?}");
        screen.until(Duration::from_secs(2), Screen::persons).await.expect("the person's pointer");
        assert_eq!(screen.events("warp 500 400"), 0, "the person's pointer is not moved");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_who_moves_the_mouse_during_an_element_clicks_lead_gets_no_click_and_no_agent_cursor() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let save = screen.one_button(200);
        screen.key(None).await.unwrap();
        // The named cursor glides to the button (about 1 s); the person moves
        // the mouse on its way.
        let target = ClickTarget::Element(save);
        let click = screen.desktop.click(&target, Button::Left, false, None);
        let person = async {
            screen.until(Duration::from_secs(10), |s| s.named_at() == "240 315").await.expect("the lead started");
            screen.person_moves(620, 450);
        };
        let (clicked, ()) = tokio::join!(click, person);
        let refused = clicked.unwrap_err();
        assert_eq!(refused.details.get("reason"), Some(&serde_json::json!("person_active")), "{refused:?}");
        assert_eq!(refused.details["execution_not_started"], serde_json::json!(true));
        assert_eq!(screen.events("click"), 0, "nothing sent once the person had the screen");
        assert!(screen.persons(), "{:#?}", screen.log());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(screen.persons(), "and it stays so: {:#?}", screen.log());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_who_moves_the_mouse_before_a_point_click_keeps_their_pointer_and_gets_no_click() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.desktop.type_text(&screen.surface, "hello", TypingCursor::ToField, None).await.unwrap();
        // A click right after typing waits a moment first; the person moves
        // the mouse then.
        let target = ClickTarget::Point { x: 300.0, y: 200.0, surface: Some(screen.surface.clone()) };
        let click = screen.desktop.click(&target, Button::Left, false, None);
        let person = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            screen.person_moves(620, 450);
        };
        let (clicked, ()) = tokio::join!(click, person);
        let refused = clicked.unwrap_err();
        assert_eq!(refused.details.get("reason"), Some(&serde_json::json!("person_active")), "{refused:?}");
        assert_eq!(screen.events("click"), 0);
        let moved = screen.times("person 620 450")[0];
        assert!(screen.log().iter().all(|(t, e)| *t < moved || !e.starts_with("warp ")), "the person's pointer is not moved: {:#?}", screen.log());
        screen.until(Duration::from_secs(2), Screen::persons).await.expect("the person's pointer");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_who_moves_the_mouse_during_a_long_read_gets_their_pointer_and_loses_the_agents_cursor_at_once() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.one_button(200);
        screen.key(None).await.unwrap();
        // Right after the key, a read of the window takes 5 s (a step's
        // check, or the agent's own read).
        std::fs::write(screen.dir.join("read_delay"), "5").unwrap();
        let read = screen.desktop.elements(&screen.surface, None, 20, None);
        let person = async {
            screen.until(Duration::from_secs(10), |s| s.events("read") == 1).await.expect("the read started");
            std::fs::remove_file(screen.dir.join("read_delay")).unwrap();
            screen.person_moves(620, 450);
            screen.until(Duration::from_secs(6), Screen::persons).await
        };
        let (page, back) = tokio::join!(read, person);
        assert!(back.is_some_and(|b| b < Duration::from_secs(2)), "only the person's pointer at once, not after the read: {back:?} {:#?}", screen.log());
        let page = page.expect("the read still answers");
        assert_eq!(page.elements.len(), 1, "{page:?}");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_who_moves_the_mouse_while_the_agent_types_lets_the_piece_in_flight_end_and_stops_the_typing_there() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // Cua types key by key (here 20 ms apart), 16 characters a piece.
        std::fs::write(screen.dir.join("type_gap"), "0.02").unwrap();
        let typing = screen.desktop.type_text(&screen.surface, "abcdefghijklmnopqrstuvwxyz0123456789", TypingCursor::ToField, None);
        let person = async {
            // Three characters into the second piece.
            screen.until(Duration::from_secs(20), |s| s.events("typed") >= 19).await.expect("typing");
            screen.person_moves(620, 450);
        };
        let (typed, ()) = tokio::join!(typing, person);
        // The second piece ends whole (stopping Cua's worker in the middle
        // of one crashes GTK 3 apps), and no third piece starts.
        assert_eq!(screen.events("typed"), 32, "the piece in flight ends whole, nothing after it: {:#?}", screen.log());
        let moved = screen.times("person 620 450")[0];
        assert!(screen.times("named off").into_iter().any(|t| t > moved), "the agent's cursor went: {:#?}", screen.log());
        // Both pieces went through; the step says so.
        let stopped = typed.unwrap_err();
        assert_eq!(stopped.details["reason"], serde_json::json!("person_active"), "{stopped:?}");
        assert_eq!(stopped.details["typed_chars"], serde_json::json!(32), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(0), "{stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(screen.events("typed"), 32, "nothing typed after it stopped");
        assert!(screen.persons(), "{:#?}", screen.log());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn keys_and_text_are_not_sent_once_another_window_has_the_keyboard_focus() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.focus_moves_away();
        for refused in [screen.desktop.type_text(&screen.surface, "hello", TypingCursor::ToField, None).await, screen.key(None).await] {
            let refused = refused.unwrap_err();
            assert_eq!(refused.code, "STALE_TARGET", "{refused:?}");
            assert_eq!(refused.details["execution_not_started"], serde_json::json!(true), "{refused:?}");
            assert!(refused.message.contains("ibara-view \"Viewer\""), "names where the focus went: {refused:?}");
            assert!(refused.details["next"].as_str().is_some_and(|n| n.contains("app")), "{refused:?}");
        }
        assert_eq!((screen.events("typed"), screen.events("key begins")), (0, 0), "nothing sent: {:#?}", screen.log());
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_window_that_takes_the_focus_while_the_agent_types_stops_the_typing_before_the_next_piece() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        std::fs::write(screen.dir.join("type_gap"), "0.02").unwrap();
        let typing = screen.desktop.type_text(&screen.surface, "abcdefghijklmnopqrstuvwxyz0123456789", TypingCursor::ToField, None);
        let viewer = async {
            // Three characters into the second piece.
            screen.until(Duration::from_secs(20), |s| s.events("typed") >= 19).await.expect("typing");
            screen.focus_moves_away();
        };
        let (typed, ()) = tokio::join!(typing, viewer);
        assert_eq!(screen.events("typed"), 32, "the piece in flight ends whole, nothing after it: {:#?}", screen.log());
        let stopped = typed.unwrap_err();
        assert_eq!(stopped.code, "STALE_TARGET", "{stopped:?}");
        assert!(stopped.message.contains("ibara-view \"Viewer\""), "{stopped:?}");
        assert_eq!(stopped.details["typed_chars"], serde_json::json!(32), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(0), "{stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(screen.events("typed"), 32, "nothing typed after it stopped");
        screen.desktop.reset_input().await.unwrap();
    }

    /// A Save As path into a folder of tasks, 99 characters.
    const SAVE_AS: &str = "/home/nova/.local/share/agent-computer/workspaces/task_1258d4784f5d4357a35f0ab3c64b3969/dogfood.txt";

    #[tokio::test]
    async fn typing_goes_on_where_it_stopped_when_a_display_change_ends_cuas_input_connection() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // A display is plugged in 9 characters into the fourth piece.
        std::fs::write(screen.dir.join("stop_at"), "57 display\n").unwrap();
        screen.desktop.type_text(&screen.surface, SAVE_AS, TypingCursor::ToField, None).await.expect("typed whole");
        assert_eq!(screen.field(), SAVE_AS, "every character once, in order: {:#?}", screen.log());
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn typing_stopped_by_a_display_change_does_not_go_on_into_a_window_that_took_the_focus() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        std::fs::write(screen.dir.join("stop_at"), "57 display\n").unwrap();
        std::fs::write(screen.dir.join("focus_on_stop"), VIEWER).unwrap();
        let stopped = screen.desktop.type_text(&screen.surface, SAVE_AS, TypingCursor::ToField, None).await.unwrap_err();
        assert_eq!(stopped.code, "STALE_TARGET", "{stopped:?}");
        assert_eq!(stopped.details["typed_chars"], serde_json::json!(57), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(0), "{stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(screen.field(), SAVE_AS[..57], "nothing typed after it stopped");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn typing_through_a_desktop_that_keeps_changing_stops_and_says_exactly_what_arrived() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        std::fs::write(screen.dir.join("stop_at"), "20 display\n30 display\n40 display\n50 display\n60 display\n").unwrap();
        let stopped = screen.desktop.type_text(&screen.surface, SAVE_AS, TypingCursor::ToField, None).await.unwrap_err();
        let typed = stopped.details["typed_chars"].as_u64().expect("typed_chars") as usize;
        assert!(typed < SAVE_AS.len(), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(0), "{stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        assert_eq!(screen.field(), SAVE_AS[..typed], "exactly what it says arrived");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn typing_is_not_resumed_when_cuas_connection_failed_with_a_key_in_flight() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        std::fs::write(screen.dir.join("stop_at"), "57 transport\n").unwrap();
        let stopped = screen.desktop.type_text(&screen.surface, SAVE_AS, TypingCursor::ToField, None).await.unwrap_err();
        assert_eq!(stopped.details["typed_chars"], serde_json::json!(57), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(1), "the key in flight: {stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(screen.field(), SAVE_AS[..58], "nothing typed after it");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_popup_that_takes_the_keyboard_part_way_stops_the_typing_with_exactly_what_arrived() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        std::fs::write(screen.dir.join("stop_at"), "57 grab\n").unwrap();
        let stopped = screen.desktop.type_text(&screen.surface, SAVE_AS, TypingCursor::ToField, None).await.unwrap_err();
        assert_eq!(stopped.code, "BLOCKED_BY_DIALOG", "{stopped:?}");
        assert_eq!(stopped.details["typed_chars"], serde_json::json!(57), "{stopped:?}");
        assert_eq!(stopped.details["unsure_chars"], serde_json::json!(0), "{stopped:?}");
        assert_eq!(stopped.details["execution_not_started"], serde_json::json!(false), "{stopped:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(screen.field(), SAVE_AS[..57], "nothing typed past the popup");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn the_agents_cursor_stays_on_the_screen_on_its_way_to_a_click() {
        // The person left the pointer near the screen's left edge: the take
        // there leaves Cua's cursor heading up and to the left.
        let screen = Screen::new((30, 300)).await;
        screen.taken().await;
        let target = ClickTarget::Point { x: 400.0, y: 300.0, surface: Some(screen.surface.clone()) };
        screen.desktop.click(&target, Button::Left, false, None).await.unwrap();
        assert_eq!(screen.named_at(), "400 300");
        screen.assert_on_screen();
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn the_agents_cursor_stays_while_the_agent_is_idle_after_a_click() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let target = ClickTarget::Point { x: 300.0, y: 200.0, surface: Some(screen.surface.clone()) };
        screen.desktop.click(&target, Button::Left, false, None).await.unwrap();
        let save = screen.one_button(200);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        // The agent thinks for longer than Cua's idle hide.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(screen.agents(), "the agent's cursor alone while it thinks: {:#?}", screen.log());
        assert_eq!(screen.events("named off"), 0, "never hidden meanwhile: {:#?}", screen.log());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn an_element_off_the_screen_takes_the_agents_cursor_to_the_screens_edge_not_past_it() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // Scrolled out of sight, left of the screen; its action still works.
        let save = screen.one_button(-300);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        assert_eq!(screen.events("click"), 1);
        assert_eq!(screen.named_at(), "0 315");
        screen.assert_on_screen();
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    /// A window at 100,50 whose app gives its boxes in the window's own
    /// coordinates, as GTK apps do through Cua 0.29.1 on Hyprland: the
    /// window's top element at 0,0 and a folder at `x`,300 (80×30), with a
    /// menu. Cua refuses the folder's accessibility click with `refusal`
    /// (its structured content).
    fn gtk_folder(screen: &Screen, x: i64, refusal: &str) -> ElementTarget {
        let window = r#"{"address":"0x1","mapped":true,"hidden":false,"at":[100,50],"size":[1000,800],"workspace":{"id":1,"name":"1"},"class":"app","title":"App","pid":100,"monitor":0}"#;
        std::fs::write(screen.dir.join("window"), window).unwrap();
        let elements = format!(
            r#"{{"elements":[{{"element_index":0,"role":"frame","label":"App","element_token":"t0","frame":{{"x":0,"y":0,"w":1000,"h":800}}}},{{"element_index":1,"parent_index":0,"role":"grid cell","label":"Documents","element_token":"t1","frame":{{"x":{x},"y":300,"w":80,"h":30}},"actions":["listitem.scroll-to","view.popup-menu"]}}],"elements_complete":true}}"#
        );
        std::fs::write(screen.dir.join("elements"), elements).unwrap();
        std::fs::write(screen.dir.join("refuse"), refusal).unwrap();
        ElementTarget {
            surface: screen.surface.clone(),
            selector: serde_json::json!({"pid": 100, "window_id": 1, "identity": {"path": [["frame", "App"]], "role": "grid cell", "label": "Documents", "ordinal": 1}}),
            role: "grid cell".into(),
            name: "Documents".into(),
            actions: vec!["listitem.scroll-to".into(), "view.popup-menu".into()],
        }
    }

    /// Cua 0.29.1's refusal of an element click it would send only as
    /// background input, which it offers to no app ibara drives.
    const NOT_QUALIFIED: &str = r#"{"ok":false,"code":"background_unavailable","reason":"client_not_qualified","detail":"client_not_qualified","route":"synthetic_events","verified":false,"effect":"refused"}"#;

    fn tried_by_accessibility(screen: &Screen) -> usize {
        std::fs::read_to_string(screen.dir.join("clicked")).unwrap_or_default().lines().count()
    }

    #[tokio::test]
    async fn an_element_cua_will_not_click_through_accessibility_is_clicked_once_by_the_pointer_at_its_center() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let folder = ClickTarget::Element(gtk_folder(&screen, 200, NOT_QUALIFIED));
        // The folder's center: 240,315 in the window, 340,365 on the screen.
        let at = Some((340.0 / 1920.0, 365.0 / 1080.0));
        assert_eq!(screen.desktop.click(&folder, Button::Left, false, None).await.unwrap(), at);
        assert_eq!(tried_by_accessibility(&screen), 1, "accessibility is asked once");
        assert_eq!(screen.events("pointer click 240 315"), 1, "{:#?}", screen.log());
        // Cua has no accessibility route for these: the pointer alone.
        assert_eq!(screen.desktop.click(&folder, Button::Left, true, None).await.unwrap(), at);
        assert_eq!(screen.desktop.click(&folder, Button::Right, false, None).await.unwrap(), at);
        assert_eq!(tried_by_accessibility(&screen), 1);
        assert_eq!(screen.events("pointer double_click 240 315"), 1, "{:#?}", screen.log());
        assert_eq!(screen.events("pointer right_click 240 315"), 1, "{:#?}", screen.log());
        assert_eq!(screen.events("click"), 3, "one click each");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn an_element_click_refused_for_another_reason_or_outside_its_window_is_not_sent_by_the_pointer() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let held = r#"{"ok":false,"code":"foreground_unavailable","reason":"primary_target_busy","detail":"foreground_constraint","effect":"refused"}"#;
        for (case, x, refusal, reason) in [("another reason", 200, held, "constraint"), ("outside its window", 1200, NOT_QUALIFIED, "client_not_qualified")] {
            let folder = ClickTarget::Element(gtk_folder(&screen, x, refusal));
            let refused = screen.desktop.click(&folder, Button::Left, false, None).await.unwrap_err();
            assert_eq!(refused.details.get("reason"), Some(&serde_json::json!(reason)), "{case}: {refused:?}");
            assert_eq!(refused.details["execution_not_started"], serde_json::json!(true), "{case}: {refused:?}");
        }
        assert_eq!(tried_by_accessibility(&screen), 2);
        assert_eq!(screen.events("click"), 0, "the pointer sent nothing: {:#?}", screen.log());
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn the_agents_cursor_goes_to_another_screen_without_crossing_what_no_screen_shows() {
        let screen = Screen::new((1900, 1000)).await;
        // A second screen to the right, 600 px lower: below the first one's
        // bottom right corner, neither shows anything.
        let screens = r#"[{"id":0,"name":"HDMI-A-1","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"focused":true,"activeWorkspace":{"id":1,"name":"1"}},
            {"id":1,"name":"DP-1","width":1920,"height":1080,"x":1920,"y":600,"scale":1.0,"focused":false,"activeWorkspace":{"id":2,"name":"2"}}]"#;
        std::fs::write(screen.dir.join("monitors"), screens).unwrap();
        screen.taken().await;
        let save = screen.button_at(1910, 1635);
        screen.desktop.click(&ClickTarget::Element(save), Button::Left, false, None).await.unwrap();
        assert_eq!(screen.named_at(), "1950 1650");
        screen.assert_on_screen();
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_person_already_moving_the_mouse_when_control_begins_keeps_their_pointer_until_they_stop() {
        let screen = Screen::idle((500, 400)).await;
        screen.note("person 500 400");
        let moving = async {
            for i in 0..25 {
                screen.person_moves(510 + i * 4, 400);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let begin = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            screen.desktop.set_agent(Some("codex@test".into()));
            tokio::time::sleep(Duration::from_millis(100)).await;
            screen.key(None).await
        };
        let (sent, ()) = tokio::join!(begin, moving);
        sent.unwrap();
        let last_move = *screen.times("person 606 400").last().unwrap();
        let after = |t: u128| Duration::from_nanos(t.saturating_sub(last_move) as u64);
        let hidden = *screen.times("pointer hidden").last().unwrap();
        let key_at = *screen.times("key").last().unwrap();
        assert!(after(hidden) >= Duration::from_millis(900), "the person keeps their pointer while they move it: hidden {:?} after their last move {:#?}", after(hidden), screen.log());
        assert!(after(key_at) >= Duration::from_millis(900), "the key waits for the mouse to be still: {:?}", after(key_at));
        assert_eq!(screen.named_at(), "606 400");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn when_the_persons_pointer_cannot_be_shown_the_agents_cursor_stays_until_it_is() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // Showing the pointer fails once when control ends.
        std::fs::write(screen.dir.join("fail_show"), "").unwrap();
        screen.end_control().await;
        let back = screen.until(Duration::from_secs(3), Screen::persons).await;
        assert!(back.is_some(), "tried again: {:#?}", screen.log());
        assert_eq!(screen.events("pointer show failed"), 1);
        // And when the person moves the mouse: Hyprland shows it, but it
        // would hide again with ibara's timeout left in place.
        screen.desktop.set_agent(Some("codex@test".into()));
        screen.taken().await;
        std::fs::write(screen.dir.join("fail_show"), "").unwrap();
        screen.person_moves(620, 450);
        screen.until(Duration::from_secs(2), |s| s.events("pointer show failed") == 2).await.expect("the show failed");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(screen.persons(), "tried again at once, and the pointer stays: {:#?}", screen.log());
        assert_eq!(std::fs::read_to_string(screen.dir.join("timeout")).unwrap().trim(), "0", "the person's own timeout is back");
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_restart_shows_the_pointer_and_puts_the_persons_own_settings_back() {
        // An ibarad that stopped while an agent held control (a crash): the
        // pointer hidden with ibara's timeout, the person's own 2.5 s kept.
        let screen = Screen::idle((500, 400)).await;
        std::fs::write(screen.dir.join("timeout"), "0.1").unwrap();
        std::fs::write(screen.dir.join("invisible"), "").unwrap();
        std::fs::write(screen.dir.join("state/pointer-hidden"), "inactive_timeout 2.5\n").unwrap();
        screen.desktop.reset_input().await.unwrap();
        assert!(screen.pointer_shown());
        assert_eq!(std::fs::read_to_string(screen.dir.join("timeout")).unwrap().trim(), "2.5");
        assert!(!screen.dir.join("state/pointer-hidden").exists());

        // ibara 0.1.0-5's marker: it hid the pointer with `cursor:invisible`.
        std::fs::write(screen.dir.join("set_invisible"), "").unwrap();
        std::fs::write(screen.dir.join("state/pointer-hidden"), "").unwrap();
        screen.desktop.reset_input().await.unwrap();
        assert_eq!(screen.events("invisible off"), 1);
        assert_eq!(std::fs::read_to_string(screen.dir.join("timeout")).unwrap().trim(), "2.5", "the timeout untouched");
        assert!(!screen.dir.join("state/pointer-hidden").exists());

        // No marker: the person's own hidden pointer stays hidden, and
        // control begins without touching it.
        std::fs::write(screen.dir.join("own_invisible"), "").unwrap();
        screen.desktop.reset_input().await.unwrap();
        assert_eq!(screen.events("invisible off"), 1);
        screen.desktop.set_agent(Some("codex@test".into()));
        screen.until(Duration::from_secs(5), Screen::named_shown).await.expect("the agent's cursor");
        screen.key(None).await.unwrap();
        assert_eq!(std::fs::read_to_string(screen.dir.join("timeout")).unwrap().trim(), "2.5", "the person's settings untouched");
        screen.end_control().await;
        assert!(!screen.named_shown());
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_restart_keeps_a_timeout_the_person_set_since_ibara_hid_the_pointer() {
        // Hyprland exited while an agent held control (a logout): the
        // marker kept 2.5 s, and the person's new session loaded 4 s since.
        let screen = Screen::idle((500, 400)).await;
        std::fs::write(screen.dir.join("timeout"), "4").unwrap();
        std::fs::write(screen.dir.join("state/pointer-hidden"), "inactive_timeout 2.5\n").unwrap();
        screen.desktop.reset_input().await.unwrap();
        assert!(screen.pointer_shown());
        assert_eq!(screen.timeout(), "4", "the person's own timeout now stays");
        assert_eq!(screen.marker(), None);
    }

    #[tokio::test]
    async fn control_that_begins_over_ibara_0_1_0_5s_marker_puts_the_persons_timeout_back_at_its_end() {
        // ibara 0.1.0-5 hid the pointer with `cursor:invisible` and left an
        // empty marker; the start's show failed (Hyprland not answering
        // yet), and still fails when an agent begins.
        let screen = Screen::idle((500, 400)).await;
        std::fs::write(screen.dir.join("timeout"), "2.5").unwrap();
        std::fs::write(screen.dir.join("set_invisible"), "").unwrap();
        std::fs::write(screen.dir.join("invisible"), "").unwrap();
        std::fs::write(screen.dir.join("state/pointer-hidden"), "").unwrap();
        let until = now_ns() + Duration::from_millis(800).as_nanos();
        std::fs::write(screen.dir.join("fail_show_until"), until.to_string()).unwrap();
        screen.desktop.set_agent(Some("codex@test".into()));
        screen.until(Duration::from_secs(5), |s| s.agents() && s.timeout() == "0.1").await.expect("the agent's cursor");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(screen.agents(), "{:#?}", screen.log());
        screen.end_control().await;
        assert!(screen.persons(), "{:#?}", screen.log());
        assert_eq!(screen.timeout(), "2.5", "the person's own timeout is back: {:#?}", screen.log());
        assert_eq!(screen.marker(), None);
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_menu_item_read_from_a_gtk_window_is_chosen_as_a_keyboard_user_chooses_it_with_its_menu_closed_or_open() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        for (open, name) in [(false, "Save As..."), (true, "Save")] {
            screen.mousepad_menu(open);
            let page = screen.desktop.elements(&screen.surface, None, 50, None).await.unwrap();
            let item = page.elements.iter().find(|e| e.role == "menu item" && e.name == name).unwrap_or_else(|| panic!("{name} is offered: {:#?}", page.elements));
            let target = ClickTarget::Element(item.target(screen.surface.clone()));
            let menu = if open { "open" } else { "closed" };
            screen.desktop.click(&target, Button::Left, false, None).await.unwrap_or_else(|e| panic!("{name} with its menu {menu}: {e:?}"));
            assert_eq!(screen.events(&format!("activated {name}")), 1, "{name} with its menu {menu} was chosen: {:#?}", screen.log());
            assert!(!screen.dir.join("menu").exists(), "the menu closed");
        }
        // GTK 3 runs an item inside accessibility's own click, and one that
        // opens a dialog leaves the app's accessibility unanswered until the
        // dialog closes: only File was clicked that way, to open it.
        let clicked = std::fs::read_to_string(screen.dir.join("clicked")).unwrap_or_default();
        assert_eq!(clicked.lines().collect::<Vec<_>>(), ["t0"]);
        assert_eq!(screen.hyprland_keys(), ["Down", "Down", "Return", "Down", "Return"], "Return only once the item was highlighted");
        screen.desktop.reset_input().await.unwrap();
    }

    /// Choose Mousepad's "Save As..." (menu closed) while `meanwhile` runs
    /// once the agent's cursor has reached the item (176.5, 244.5), just
    /// before Return would go.
    async fn choose_save_as_while(screen: &Screen, meanwhile: impl Fn(&Screen)) -> crate::error::IbaraError {
        screen.mousepad_menu(false);
        // As in the element click's lead test: after an input, the agent's cursor glides (about 1 s).
        screen.key(None).await.unwrap();
        let page = screen.desktop.elements(&screen.surface, None, 50, None).await.unwrap();
        let item = page.elements.iter().find(|e| e.name == "Save As...").expect("offered");
        let target = ClickTarget::Element(item.target(screen.surface.clone()));
        let click = screen.desktop.click(&target, Button::Left, false, None);
        let other = async {
            screen.until(Duration::from_secs(20), |s| s.named_at() == "176.5 244.5").await.expect("the agent's cursor went to the item");
            meanwhile(screen);
        };
        let (chosen, ()) = tokio::join!(click, other);
        let refused = chosen.err().unwrap_or_else(|| panic!("Return went: {:#?}", screen.log()));
        assert!(!screen.hyprland_keys().iter().any(|k| k == "Return"), "no Return: {:#?}", screen.log());
        assert_eq!(screen.events("activated Save As..."), 0);
        assert_ne!(refused.details.get("execution_not_started"), Some(&serde_json::json!(true)), "the menu was opened and walked: {refused:?}");
        refused
    }

    #[tokio::test]
    async fn a_person_who_takes_the_screen_while_the_agents_cursor_goes_to_a_menu_item_gets_no_return() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let refused = choose_save_as_while(&screen, |s| s.person_moves(620, 450)).await;
        assert_eq!(refused.details.get("reason"), Some(&serde_json::json!("person_active")), "{refused:?}");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_window_that_takes_the_focus_while_the_agents_cursor_goes_to_a_menu_item_gets_no_return() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let refused = choose_save_as_while(&screen, Screen::focus_moves_away).await;
        assert_eq!(refused.code, "STALE_TARGET", "{refused:?}");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn a_menu_that_closes_while_the_agents_cursor_goes_to_its_item_gets_no_return() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // The app closes its menu (say, a person's Escape): Return would reach the document.
        let closes = |s: &Screen| assert!(std::process::Command::new("sh").arg(s.dir.join("menu.sh")).args(["key", "Escape", "1"]).status().unwrap().success());
        let refused = choose_save_as_while(&screen, closes).await;
        assert_eq!(refused.code, "STALE_TARGET", "{refused:?}");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn the_windows_open_menu_closes_on_escape_and_a_shortcut_closes_it_and_reaches_the_window() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.mousepad_menu(true);
        // Return or an arrow means the menu: after closing it, it would reach the document instead.
        let other = screen.desktop.key(&screen.surface, "Return", None).await.unwrap_err();
        assert_eq!((other.code, &other.details["execution_not_started"]), ("BLOCKED_BY_DIALOG", &serde_json::json!(true)));
        assert!(other.message.contains("Escape"), "it says how to close the menu: {}", other.message);
        assert!(screen.hyprland_keys().is_empty() && screen.dir.join("menu").exists(), "nothing sent: {:#?}", screen.log());

        screen.desktop.key(&screen.surface, "Escape", None).await.unwrap();
        assert_eq!(screen.hyprland_keys(), ["Escape"], "Escape went to the menu");
        assert!(!screen.dir.join("menu").exists(), "the menu closed");
        assert_eq!(screen.events("key"), 0, "nothing reached the window");

        screen.mousepad_menu(true);
        screen.key(None).await.unwrap();
        assert_eq!(screen.hyprland_keys(), ["Escape", "Escape"], "the shortcut closed the menu first");
        let (closed, key) = (*screen.times("hyprland key Escape").last().unwrap(), screen.times("key"));
        assert!(key.len() == 1 && key[0] > closed, "then ctrl+s reached the window: {:#?}", screen.log());
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn while_something_else_holds_the_keyboard_every_key_is_refused_and_nothing_is_sent() {
        // Another app's popup, or a drag: the window shows no open menu.
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        screen.mousepad_menu(false);
        std::fs::write(screen.dir.join("grab"), "").unwrap();
        for combo in ["Escape", "ctrl+s"] {
            let refused = screen.desktop.key(&screen.surface, combo, None).await.unwrap_err();
            assert_eq!((refused.code, &refused.details["execution_not_started"]), ("BLOCKED_BY_DIALOG", &serde_json::json!(true)), "{combo}");
        }
        assert!(screen.hyprland_keys().is_empty(), "{:#?}", screen.log());
        assert_eq!(screen.events("key"), 0);
        screen.desktop.reset_input().await.unwrap();
    }

    /// Cua's next reads answer `elements` (Cua's own element list) as a
    /// whole read unless `whole` is false.
    fn read_answers(screen: &Screen, elements: &str, whole: bool) {
        std::fs::write(screen.dir.join("elements"), format!(r#"{{"elements":[{elements}],"elements_complete":{whole}}}"#)).unwrap();
    }

    /// A text box as Cua gives one, at `x`,`y` (`w`×`h`).
    fn text_box(token: &str, x: i64, y: i64, w: i64, h: i64) -> String {
        format!(r#"{{"element_index":0,"role":"text","label":"","element_token":"{token}","enabled":true,"frame":{{"x":{x},"y":{y},"w":{w},"h":{h}}}}}"#)
    }

    impl Screen {
        /// Since `since`: where the named cursor was sent, and whether
        /// anything moved the real pointer (a warp, or the plugin's travel).
        fn moves_since(&self, since: u128) -> (Vec<String>, bool) {
            let later: Vec<String> = self.log().into_iter().filter(|(t, _)| *t > since).map(|(_, e)| e).collect();
            let named = later.iter().filter_map(|e| e.strip_prefix("named at ").map(str::to_string)).collect();
            (named, later.iter().any(|e| e.starts_with("warp ") || e.starts_with("plugin ")))
        }
    }

    #[tokio::test]
    async fn typing_glides_the_agents_cursor_to_a_field_with_a_known_box_first_and_leaves_the_pointer_alone() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        read_answers(&screen, &text_box("t1", 100, 200, 300, 40), true);
        let since = now_ns();
        screen.desktop.type_text(&screen.surface, "hello", TypingCursor::ToField, None).await.unwrap();
        assert_eq!(screen.field(), "hello");
        let (named, pointer_moved) = screen.moves_since(since);
        assert_eq!(named.last().map(String::as_str), Some("250 220"), "to the middle of the field: {:#?}", screen.log());
        let (arrived, typed) = (*screen.times("named at 250 220").last().unwrap(), screen.times("typed")[0]);
        assert!(arrived < typed, "there before the first key: {:#?}", screen.log());
        assert!(!pointer_moved && screen.pointer_at() == "500 400", "the real pointer stays: {:#?}", screen.log());
        assert_eq!(screen.events("cue text"), 1, "Cua shows its typing animation: {:#?}", screen.log());

        // Typed into again: the agent's cursor is in the field already.
        let since = now_ns();
        screen.desktop.type_text(&screen.surface, " again", TypingCursor::ToField, None).await.unwrap();
        assert_eq!(screen.moves_since(since), (vec![], false), "{:#?}", screen.log());
        screen.assert_one_cursor();
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn typing_where_no_box_proves_the_field_leaves_both_cursors_where_they_are() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        let cases = [
            ("two fields", format!("{},{}", text_box("t1", 100, 200, 300, 40), text_box("t2", 100, 300, 300, 40)), true),
            ("a read cut short", text_box("t1", 100, 200, 300, 40), false),
            // Past the window's right edge (1000): not the window's own box.
            ("a box outside the window", text_box("t1", 900, 200, 300, 40), true),
            ("no box", r#"{"element_index":0,"role":"text","label":"","element_token":"t1","enabled":true}"#.to_string(), true),
        ];
        for (case, elements, whole) in &cases {
            read_answers(&screen, elements, *whole);
            let since = now_ns();
            screen.desktop.type_text(&screen.surface, "hi", TypingCursor::ToField, None).await.unwrap();
            assert_eq!(screen.moves_since(since), (vec![], false), "{case}: {:#?}", screen.log());
        }
        // A page field the step's click located: no read, no move.
        read_answers(&screen, &text_box("t1", 100, 200, 300, 40), true);
        let (since, reads) = (now_ns(), screen.events("read"));
        screen.desktop.type_text(&screen.surface, "hi", TypingCursor::Stays, None).await.unwrap();
        assert_eq!(screen.moves_since(since), (vec![], false), "{:#?}", screen.log());
        assert_eq!(screen.events("read"), reads, "nothing read");
        assert_eq!(screen.field(), "hihihihihi", "all typed");
        assert_eq!(screen.events("cue text"), 5, "each with Cua's typing animation");
        assert_eq!(screen.pointer_at(), "500 400");
        screen.desktop.reset_input().await.unwrap();
    }

    #[tokio::test]
    async fn keys_move_neither_the_agents_cursor_nor_the_pointer() {
        let screen = Screen::new((500, 400)).await;
        screen.taken().await;
        // Even with a field whose box is known.
        read_answers(&screen, &text_box("t1", 100, 200, 300, 40), true);
        let since = now_ns();
        screen.key(None).await.unwrap();
        screen.desktop.key(&screen.surface, "Return", None).await.unwrap();
        assert_eq!(screen.events("key"), 2);
        assert_eq!(screen.moves_since(since), (vec![], false), "{:#?}", screen.log());
        assert_eq!(screen.pointer_at(), "500 400");
        assert_eq!(screen.events("cue key"), 2, "Cua shows its key animation: {:#?}", screen.log());
        screen.desktop.reset_input().await.unwrap();
    }
}
