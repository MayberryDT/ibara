//! The watchdog: every few seconds it repairs the stuck states found on
//! screenless computers (`.research/headless/diagnosis.json`) and resumes a
//! computer that only the system paused.
//!
//! One pass, in order:
//! - **Display missing**: the session answers but has no enabled output. Create
//!   `IbaraVirtual` through `reconcile_display`; the display loop only starts
//!   on a display event, which a computer that started without a screen may
//!   never see.
//! - **Viewer failed**: run `initialize_viewer` again, which revokes and ends
//!   any stream left behind (the stream starts on demand, so ending it is the
//!   repair). This replaces the start-up viewer retry loop and also covers
//!   faults after start.
//! - **Unsettled with nothing running** for 30 s (no lease, queued effect,
//!   job or held viewer): settle again.
//! - **Resume**: paused by the system (start, shutdown, repair, a restart
//!   during a lease), settled, no viewer fault or holder, and a display
//!   present. A person's pause, or a person holding control, is never ended here.
//!
//! Each repair writes a `repair` timeline event and each automatic resume an
//! `auto_resumed` one. A failed repair waits 10 s, doubling to 5 minutes;
//! after three failures the problem shows as `needs_person` in status until it
//! clears. The share-picker overlay the diagnosis found holding focus is not
//! handled: the desktop port does not see layer surfaces.
//!
//! This computer's settings turn the repairs (`self_repair`) and the resume
//! (`auto_resume`) off; each pass reads them, so a change applies at once. A
//! person runs a repair at once with the operator operation `repair`
//! ([`Controller::repair_now`]), whatever the settings say.

use super::control::Release;
use super::ports::OutputChange;
use super::{Controller, log_event};
use crate::error::Result;
use crate::store::{NewEvent, PauseOrigin};
use serde_json::{Value, json};
use std::rc::Weak;
use std::time::Duration;

/// Timeline kind of a repair.
pub(crate) const REPAIR: &str = "repair";
/// Timeline kind of an automatic resume.
pub(crate) const AUTO_RESUMED: &str = "auto_resumed";
const TICK: Duration = Duration::from_secs(5);
/// How long unsettled work must sit with nothing running before it is settled again.
pub(crate) const UNSETTLED_GRACE_MS: i64 = 30_000;
const BACKOFF_FIRST_MS: i64 = 10_000;
const BACKOFF_MAX_MS: i64 = 300_000;
/// Failed repairs before a problem is shown as needing a person.
pub(crate) const NEEDS_PERSON_AFTER: u32 = 3;

/// A stuck state the watchdog repairs, in the order it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    Display,
    Viewer,
    Unsettled,
}

const PROBLEMS: [Problem; 3] = [Problem::Display, Problem::Viewer, Problem::Unsettled];

impl Problem {
    fn index(self) -> usize {
        self as usize
    }

    fn code(self) -> &'static str {
        match self {
            Problem::Display => "display_missing",
            Problem::Viewer => "viewer_unavailable",
            Problem::Unsettled => "work_unsettled",
        }
    }

    fn repaired(self) -> &'static str {
        match self {
            Problem::Display => "Added a virtual screen because no screen was connected.",
            Problem::Viewer => "Restarted screen sharing after it stopped working.",
            Problem::Unsettled => "Finished stopping earlier work so agents can continue.",
        }
    }

    fn needs_person(self, since_ms: Option<i64>) -> Value {
        let (message, fix) = match self {
            Problem::Display => ("This computer has no screen, and ibara could not add a virtual one.", "reconnect_display"),
            Problem::Viewer => ("Screen sharing on this computer stopped working, and restarting it did not help.", "restart_viewer"),
            Problem::Unsettled => ("Earlier work on this computer did not finish stopping, so agents are held back.", "restart_ibara"),
        };
        json!({ "code": self.code(), "message": message, "fix": fix, "at": since_ms.map(crate::ids::iso_from_millis) })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Attempts {
    failures: u32,
    next_at_ms: i64,
    /// When the problem started needing a person.
    needs_person_since_ms: Option<i64>,
}

/// Memory-only watchdog state, except `last`, which starts from the timeline.
#[derive(Debug, Default)]
pub(crate) struct Watchdog {
    attempts: [Attempts; 3],
    unsettled_since_ms: Option<i64>,
    /// The newest repair: when, and its summary.
    last: Option<(String, String)>,
    /// The computer ran unpaused at some point since this start.
    ran: bool,
}

impl Watchdog {
    pub(crate) fn new(last: Option<(String, String)>) -> Self {
        Watchdog { last, ..Default::default() }
    }
}

impl Controller {
    /// Status `repair`: the newest repair, and what needs a person, if anything.
    pub(crate) fn repair_status(&self) -> Value {
        let watchdog = self.watchdog.borrow();
        let needs = PROBLEMS.into_iter().find(|p| watchdog.attempts[p.index()].failures >= NEEDS_PERSON_AFTER);
        json!({
            "last": watchdog.last.as_ref().map(|(at, summary)| json!({ "at": at, "summary": summary })),
            "needs_person": needs.map(|p| p.needs_person(watchdog.attempts[p.index()].needs_person_since_ms)),
        })
    }

    /// One watchdog pass; passes never overlap.
    pub(crate) async fn watchdog_tick(&self) {
        let _turn = self.watchdog_turn.lock().await;
        if self.closed.get() {
            return;
        }
        self.apply_display_setting().await;
        let settings = crate::settings::current();
        if settings.bool("self_repair") {
            if let Err(e) = self.repair_display().await {
                log_event("watchdog_failed", &e.to_string());
            }
            if let Err(e) = self.repair_viewer().await {
                log_event("watchdog_failed", &e.to_string());
            }
            if let Err(e) = self.repair_unsettled().await {
                log_event("watchdog_failed", &e.to_string());
            }
        }
        if settings.bool("auto_resume")
            && let Err(e) = self.resume_if_healthy().await
        {
            log_event("watchdog_failed", &e.to_string());
        }
        // A login removed while the browser was closed goes once it connects.
        self.login_deferred_removals().await;
    }

    /// A person's repair (`needs_person.fix`), now: `fixed`, `still_broken`
    /// or, for `restart_ibara`, `restarting` (ibarad exits a second later and
    /// its unit starts it again).
    pub(crate) async fn repair_now(&self, fix: &str) -> Result<Value> {
        let answer = |state: &str, message: &str| json!({ "fix": fix, "state": state, "message": message });
        match fix {
            "reconnect_display" => {
                let _turn = self.watchdog_turn.lock().await;
                match self.display_missing().await {
                    None => return Ok(answer("still_broken", "The desktop on this computer is not running, so no screen can be added.")),
                    Some(false) => {
                        self.forget(Problem::Display);
                        return Ok(answer("fixed", "This computer has a screen."));
                    }
                    Some(true) => {}
                }
                self.admin(json!({ "op": "reconcile_display" })).await?;
                if self.display_missing().await == Some(false) {
                    self.repaired(Problem::Display);
                    Ok(answer("fixed", "Added a virtual screen."))
                } else {
                    Ok(answer("still_broken", "The virtual screen did not appear. Restart this computer."))
                }
            }
            "restart_viewer" => {
                if !self.stream.as_ref().is_some_and(|s| s.available()) {
                    return Ok(answer("still_broken", "Screen sharing is not set up on this computer."));
                }
                if self.viewer_state.borrow().owner.is_some() {
                    return Ok(answer("still_broken", "Someone has control of this computer. Hand back first, then try again."));
                }
                let _turn = self.watchdog_turn.lock().await;
                let _viewer = self.viewer_lock.lock().await;
                // Screen sharing starts on demand; this ends one left behind.
                match self.initialize_viewer().await {
                    Ok(()) => {
                        self.repaired(Problem::Viewer);
                        Ok(answer("fixed", "Restarted screen sharing."))
                    }
                    Err(e) => Ok(answer("still_broken", &format!("Screen sharing did not start again: {}", e.message))),
                }
            }
            "restart_ibara" => {
                self.record(REPAIR, "Restarting ibara at a person's request.", json!({ "problem": "restart_requested" }));
                if let Some(me) = self.me.borrow().upgrade() {
                    tokio::task::spawn_local(async move {
                        // Time for this answer to reach the person first.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        me.restart.notify_one();
                    });
                } else {
                    self.restart.notify_one();
                }
                Ok(answer("restarting", "ibara is restarting on this computer and is back in a few seconds."))
            }
            _ => Err(crate::error::invalid("Unknown repair. Use reconnect_display, restart_viewer or restart_ibara.")),
        }
    }

    /// Whether the session answers without any enabled output (`None`: it cannot tell).
    async fn display_missing(&self) -> Option<bool> {
        if !self.desktop.session_available() {
            return None;
        }
        self.desktop.output_change().await.ok().map(|change| change == Some(OutputChange::Create))
    }

    async fn repair_display(&self) -> Result<()> {
        match self.display_missing().await {
            Some(true) => {}
            Some(false) => {
                self.forget(Problem::Display);
                return Ok(());
            }
            None => return Ok(()),
        }
        if !self.due(Problem::Display) {
            return Ok(());
        }
        match self.admin(json!({ "op": "reconcile_display" })).await {
            // Hyprland can accept the command without the output appearing.
            Ok(v) if v["changed"] == json!(true) => match self.display_missing().await {
                Some(true) => self.failed(Problem::Display, "the virtual screen did not appear"),
                _ => self.repaired(Problem::Display),
            },
            // A lease, a person's control or another reconcile holds it back; that is not a failure.
            Ok(v) if v["deferred"] == json!(true) && v.get("reason").is_none() => {}
            Ok(v) if v["deferred"] == json!(false) => self.forget(Problem::Display),
            Ok(v) => self.failed(Problem::Display, &v.to_string()),
            Err(e) => self.failed(Problem::Display, &e.to_string()),
        }
        Ok(())
    }

    async fn repair_viewer(&self) -> Result<()> {
        if self.stream.is_none() || !self.viewer_state.borrow().fault {
            self.forget(Problem::Viewer);
            return Ok(());
        }
        if !self.due(Problem::Viewer) {
            return Ok(());
        }
        // A take-control, hand-back or revoke in progress owns the viewer.
        let Ok(_turn) = self.viewer_lock.try_lock() else { return Ok(()) };
        match self.initialize_viewer().await {
            Ok(()) => self.repaired(Problem::Viewer),
            Err(e) => self.failed(Problem::Viewer, &e.to_string()),
        }
        Ok(())
    }

    async fn repair_unsettled(&self) -> Result<()> {
        let unsettled = self.journal.get_control()?.unsettled;
        let busy = self.journal.get_active_lease()?.is_some()
            || self.settling.get()
            || self.queue_depth.get() > 0
            || self.storage.has_active_jobs(None)
            || {
                let viewer = self.viewer_state.borrow();
                viewer.fault || viewer.owner.is_some()
            };
        if !unsettled || busy {
            self.watchdog.borrow_mut().unsettled_since_ms = None;
            if !unsettled {
                self.forget(Problem::Unsettled);
            }
            return Ok(());
        }
        let now = self.now_ms();
        let since = *self.watchdog.borrow_mut().unsettled_since_ms.get_or_insert(now);
        if now - since < UNSETTLED_GRACE_MS || !self.due(Problem::Unsettled) {
            return Ok(());
        }
        // Without a lease, settlement only cancels leftover work, releases input and drains.
        self.release_lease(None, Release::Finished, false).await?;
        if self.journal.get_control()?.unsettled {
            self.failed(Problem::Unsettled, "settlement left work unsettled");
        } else {
            self.watchdog.borrow_mut().unsettled_since_ms = None;
            self.repaired(Problem::Unsettled);
        }
        Ok(())
    }

    /// Resume when only the system paused and the computer is healthy.
    async fn resume_if_healthy(&self) -> Result<()> {
        let control = self.journal.get_control()?;
        if !control.paused && !control.human_control {
            self.watchdog.borrow_mut().ran = true;
            return Ok(());
        }
        if control.pause_origin == Some(PauseOrigin::Person) || control.unsettled || self.journal.get_active_lease()?.is_some() {
            return Ok(());
        }
        if self.display_missing().await != Some(false) {
            return Ok(());
        }
        // A take-control or hand-back in progress decides for itself.
        let Ok(_turn) = self.viewer_lock.try_lock() else { return Ok(()) };
        {
            let viewer = self.viewer_state.borrow();
            if viewer.owner.is_some() || viewer.fault {
                return Ok(());
            }
        }
        // Checked again: the display check awaited.
        let control = self.journal.get_control()?;
        if control.pause_origin == Some(PauseOrigin::Person) || control.unsettled || !(control.paused || control.human_control) {
            return Ok(());
        }
        self.admin_resume().await?;
        let summary = if self.watchdog.borrow().ran {
            "Resumed once the computer was healthy again; nothing needed a person."
        } else {
            "Resumed after restart; nothing needed a person."
        };
        self.watchdog.borrow_mut().ran = true;
        self.record(AUTO_RESUMED, summary, json!({ "pause_origin": control.pause_origin }));
        Ok(())
    }

    fn due(&self, problem: Problem) -> bool {
        self.now_ms() >= self.watchdog.borrow().attempts[problem.index()].next_at_ms
    }

    /// The problem is gone, whoever fixed it.
    fn forget(&self, problem: Problem) {
        self.watchdog.borrow_mut().attempts[problem.index()] = Attempts::default();
    }

    fn failed(&self, problem: Problem, why: &str) {
        let now = self.now_ms();
        let failures = {
            let mut watchdog = self.watchdog.borrow_mut();
            let attempts = &mut watchdog.attempts[problem.index()];
            attempts.failures += 1;
            attempts.next_at_ms = now + (BACKOFF_FIRST_MS << (attempts.failures - 1).min(5)).min(BACKOFF_MAX_MS);
            if attempts.failures >= NEEDS_PERSON_AFTER && attempts.needs_person_since_ms.is_none() {
                attempts.needs_person_since_ms = Some(now);
            }
            attempts.failures
        };
        log_event("repair_failed", &format!("{} (attempt {failures}): {why}", problem.code()));
    }

    fn repaired(&self, problem: Problem) {
        let failures = self.watchdog.borrow().attempts[problem.index()].failures;
        self.forget(problem);
        let at = self.record(REPAIR, problem.repaired(), json!({ "problem": problem.code(), "failed_attempts": failures }));
        self.watchdog.borrow_mut().last = Some((at, problem.repaired().to_string()));
    }

    /// Write a timeline event by ibarad; returns its time.
    fn record(&self, kind: &str, summary: &str, data: Value) -> String {
        let at = self.now_iso();
        let event = NewEvent { at: &at, kind, task_ref: None, actor: "ibarad", summary, data: &data };
        if let Err(e) = self.journal.append_event(event) {
            log_event("timeline_write_failed", &e.to_string());
        }
        at
    }
}

/// One watchdog pass every 5 s while the controller runs.
pub(crate) async fn watchdog_loop(me: Weak<Controller>) {
    loop {
        tokio::time::sleep(TICK).await;
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() {
            return;
        }
        controller.watchdog_tick().await;
    }
}
