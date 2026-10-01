//! Leases, settlement, pause and the viewer ownership machine
//! (`core.ts:291-490`, `644-815`, `954-1070`, `2363-2404`).

use super::ports::{StreamPort, StreamStatus};
use super::{Controller, log_event};
use crate::error::{IbaraError, Result, denied, invalid};
use crate::ids::id;
use crate::store::{ConnectionRecord, ControlPatch, ControlState, LeaseRecord, PauseOrigin, TaskRecord};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

fn fail(code: &'static str, message: &str, retry_safe: bool) -> IbaraError {
    IbaraError::new(code, message, retry_safe)
}

fn person_holds_control() -> IbaraError {
    fail("HUMAN_CONTROL", "Your person has control of the computer.", true)
        .with("next", "Your person has control; wait for Hand Back. You can keep waiting for their answer with computer_wait.")
}

/// A task's control ended because ibara restarted: the step stops here, and
/// the agent begins again once ibara has started.
pub(crate) fn restart_ended() -> IbaraError {
    fail("LEASE_EXPIRED", "ibara restarted, which ended this task's control.", false)
        .with("next", "Check what the last step did with computer_status, then begin again with computer_begin in a few seconds.")
}

/// Why agents wait while only ibara paused the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemWait {
    /// The pause every start makes until the watchdog finds the computer healthy.
    Starting,
    /// Earlier work did not finish stopping; the watchdog settles it first.
    Settling,
    /// A repair failed; a person has to fix it.
    NeedsPerson,
    /// This computer's settings leave resuming to a person.
    ResumeOff,
}

impl SystemWait {
    /// The holder in the situation line.
    pub(crate) fn holder(self) -> &'static str {
        match self {
            SystemWait::Starting => "ibara is starting",
            SystemWait::Settling => "ibara is settling",
            SystemWait::NeedsPerson => "ibara needs a person to fix it",
            SystemWait::ResumeOff => "waiting for a person to resume agents",
        }
    }

    /// Its name in operator status (`system_wait`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SystemWait::Starting => "starting",
            SystemWait::Settling => "settling",
            SystemWait::NeedsPerson => "needs_person",
            SystemWait::ResumeOff => "resume_off",
        }
    }

    /// The refusal of a begin.
    pub(crate) fn refusal(self) -> IbaraError {
        let (message, retry, next) = match self {
            SystemWait::Starting => (
                "ibara is starting and checks the computer before agents begin; try again in a few seconds.",
                true,
                "Call computer_begin again in a few seconds, with a new request_id.",
            ),
            SystemWait::Settling => (
                "ibara is settling earlier work before agents begin; try again in a minute.",
                true,
                "Call computer_begin again in a minute, with a new request_id.",
            ),
            SystemWait::NeedsPerson => (
                "ibara restarted and cannot resume agents until a person fixes a problem on this computer.",
                false,
                "Ask the person to open this computer in the ibara console and fix the problem, then begin again with a new request_id.",
            ),
            SystemWait::ResumeOff => (
                "ibara restarted, and this computer's settings leave resuming agents to a person.",
                false,
                "Ask the person to resume agents in the ibara console, then begin again with a new request_id.",
            ),
        };
        fail("BUSY", message, retry).with("next", next)
    }
}

/// How a lease ends (`leases.reason`) and the state it ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    IdleExpired,
    ControlBudgetExpired,
    DisconnectGrace,
    Finished,
    OperatorPause,
    /// ibara paused as it started or stopped.
    Restart,
    OperatorRevoke,
    AuthorityRevoked,
}

impl Release {
    fn reason(self) -> &'static str {
        match self {
            Release::IdleExpired => "idle_expired",
            Release::ControlBudgetExpired => "control_budget_expired",
            Release::DisconnectGrace => "disconnect_grace",
            Release::Finished => "finished",
            Release::OperatorPause => "operator_pause",
            Release::Restart => "controller_restart",
            Release::OperatorRevoke => "operator_revoke",
            Release::AuthorityRevoked => "authority_revoked",
        }
    }
    fn expires(self) -> bool {
        matches!(self, Release::IdleExpired | Release::ControlBudgetExpired)
    }
    fn phrase(self) -> &'static str {
        match self {
            Release::IdleExpired => "control ended: idle for 5 minutes",
            Release::ControlBudgetExpired => "control ended: control budget used up",
            Release::DisconnectGrace => "control ended: the agent disconnected",
            Release::Finished => "control released: task finished",
            Release::OperatorPause => "a person paused the computer",
            Release::Restart => "control ended: ibara restarted",
            Release::OperatorRevoke => "a person revoked control",
            Release::AuthorityRevoked => "control ended: access was revoked",
        }
    }
}

/// `activeControlLimitMs`: `None` is unlimited.
pub(crate) fn control_limit_ms(task: &TaskRecord) -> Option<f64> {
    task.budgets.get("active_control_seconds").and_then(Value::as_f64).map(|s| s * 1000.0)
}

impl Controller {
    // ---- tasks and leases ---------------------------------------------------

    /// `requireReadableTask` (`core.ts:954-961`).
    pub(crate) fn require_readable_task(&self, principal: &str, task_ref: &str) -> Result<TaskRecord> {
        if !super::is_ref(task_ref) {
            return Err(invalid("task_ref is not a valid identifier."));
        }
        self.journal
            .readable_task(principal, task_ref)?
            .ok_or_else(|| denied("Task is private or this principal has no active group grant."))
    }

    /// `redactedBusy`.
    pub(crate) fn redacted_busy(&self) -> IbaraError {
        fail("BUSY", "Control is unavailable.", true)
    }

    /// Why agents wait, while only ibara paused the computer; `None` while a
    /// person paused it or holds it, or nothing is paused.
    pub(crate) fn system_wait(&self, control: &ControlState) -> Option<SystemWait> {
        if !control.paused_by_system() || self.viewer_state.borrow().owner.is_some() {
            return None;
        }
        Some(if !self.repair_status()["needs_person"].is_null() {
            SystemWait::NeedsPerson
        } else if !crate::settings::current().bool("auto_resume") {
            SystemWait::ResumeOff
        } else if control.unsettled {
            SystemWait::Settling
        } else {
            SystemWait::Starting
        })
    }

    /// `requireLiveLease` (`core.ts:963-985`).
    pub(crate) fn require_live_lease(&self, principal: &str, connection_id: &str, task_ref: &str) -> Result<LeaseRecord> {
        self.require_task_lease(principal, connection_id, task_ref, false)
    }

    /// Attention waits read a person's answer without taking input authority.
    pub(crate) fn require_task_lease(&self, principal: &str, connection_id: &str, task_ref: &str, attention_wait: bool) -> Result<LeaseRecord> {
        self.require_readable_task(principal, task_ref)?;
        self.require_access(&self.task_subject(task_ref,principal),"agents")?;
        let control = self.journal.get_control()?;
        if control.human_control && !(attention_wait && control.pause_origin == Some(PauseOrigin::Person)) {
            let Some(wait) = self.system_wait(&control) else {
                return Err(person_holds_control());
            };
            let restarted = self.journal.list_leases_for_task(task_ref)?.first().and_then(|l| l.reason.clone()).as_deref() == Some(Release::Restart.reason());
            return Err(if restarted { restart_ended() } else { wait.refusal() });
        }
        if control.unsettled && !attention_wait {
            return Err(fail("CONTROL_UNSETTLED", "Previous controllable work has not settled.", false).requires_reconciliation());
        }
        let Some(lease) = self.journal.get_active_lease()? else {
            return Err(fail("LEASE_EXPIRED", "No active control; this task's control ended.", true));
        };
        if lease.principal != principal || lease.connection_id != connection_id {
            return Err(self.redacted_busy());
        }
        if lease.task_ref != task_ref {
            return Err(fail("LEASE_EXPIRED", "The supplied task_ref is not the task you control.", true));
        }
        if lease.epoch != self.epoch {
            return Err(fail("LEASE_EXPIRED", "The control belongs to a previous controller start.", true));
        }
        if lease.idle_expires_at_ms <= self.now_ms() {
            return Err(fail("LEASE_EXPIRED", "The control idle period expired.", true));
        }
        Ok(lease)
    }

    /// Whether `agent` at `principal`, calling over `connection_id`, holds
    /// `lease`: over that connection, or it is the agent that began the task
    /// and the connection holding the control is gone. A second live session
    /// of the same agent (two `codex` sessions on one computer) does not.
    pub(crate) fn holds_lease(&self, lease: &LeaseRecord, principal: &str, connection_id: &str, agent: &str) -> bool {
        lease.principal == principal
            && (lease.connection_id == connection_id
                || (self.task_subject(&lease.task_ref, principal) == agent && self.connection_gone(&lease.connection_id)))
    }

    /// The connection went away (disconnected, in its grace), or its client
    /// stopped answering the session's pings: a network drop this computer
    /// has not noticed, the session still open and heartbeating.
    fn connection_gone(&self, connection_id: &str) -> bool {
        self.silent.borrow().contains(connection_id)
            || self.journal.get_connection(connection_id).ok().flatten().is_none_or(|c| c.disconnected_at_ms.is_some())
    }

    /// The agent holding `task_ref`'s control calls over another connection
    /// after the old one went away (whether or not the old session is still
    /// open here): bind the control to this connection at once, instead of
    /// leaving it to the old one until its disconnect grace or idle expiry
    /// runs out. Nothing is resumed from a connection that still answers,
    /// while a person holds the computer or it is unsettled, nor control that
    /// already ended or belongs to an earlier start. Work still running for
    /// the old connection stops at its next authority check.
    pub(crate) fn resume_lease(&self, principal: &str, connection_id: &str, agent: &str, task_ref: &str) -> Result<()> {
        let Some(mut lease) = self.journal.get_active_lease()? else {
            return Ok(());
        };
        let now = self.now_ms();
        if lease.connection_id == connection_id
            || lease.task_ref != task_ref
            || lease.epoch != self.epoch
            || lease.idle_expires_at_ms <= now
            || !self.holds_lease(&lease, principal, connection_id, agent)
        {
            return Ok(());
        }
        let control = self.journal.get_control()?;
        if control.human_control || control.unsettled {
            return Ok(());
        }
        lease.connection_id = connection_id.to_string();
        lease.last_heartbeat_at = self.now_iso();
        lease.last_heartbeat_ms = now;
        lease.idle_expires_at_ms = now + self.idle_expiry_ms;
        self.journal.put_lease(&lease)
    }

    /// `context.assertAuthority()` (`core.ts:995-1003`), re-run before and after effects.
    pub(crate) fn assert_authority(&self, lease: &LeaseRecord) -> Result<()> {
        self.require_readable_task(&lease.principal, &lease.task_ref)?;
        self.require_access(&self.task_subject(&lease.task_ref,&lease.principal),"agents")?;
        let control = self.journal.get_control()?;
        if control.human_control {
            // Only a start or stop pauses for the system while an agent holds control.
            return Err(if control.paused_by_system() { restart_ended() } else { person_holds_control() });
        }
        match self.journal.get_active_lease()? {
            Some(live) if live.generation == lease.generation => {
                if live.connection_id != lease.connection_id || live.principal != lease.principal {
                    return Err(fail("LEASE_EXPIRED", "Control is no longer bound to this connection.", false));
                }
                Ok(())
            }
            _ => Err(fail("LEASE_EXPIRED", "Control changed hands.", false)),
        }
    }

    /// `reconcile` (`core.ts:740-758`): expire an idle lease, end one whose
    /// control budget ran out, revoke one whose connection's grace passed.
    pub(crate) async fn reconcile(&self) -> Result<()> {
        // Revocation is enforced in-process first. Closing what it denies
        // (a lease, a viewer) and transport cleanup can fail without blocking
        // lease expiry, budgets, grace or unrelated calls; both retry next time.
        if let Err(e) = Box::pin(self.settle_access()).await {
            super::log_event("access_settle_failed", &e.to_string());
        }
        self.project_access_background().await;
        let now = self.now_ms();
        if let Some(lease) = self.journal.get_active_lease()?
            && lease.idle_expires_at_ms <= now
        {
            self.release_lease(Some(lease), Release::IdleExpired, false).await?;
        }
        let Some(active) = self.journal.get_active_lease()? else {
            return Ok(());
        };
        if let Some(task) = self.journal.get_task(&active.task_ref)?
            && self.control_remaining_ms(&task).is_some_and(|ms| ms <= 0.0)
        {
            return self.release_lease(Some(active), Release::ControlBudgetExpired, false).await;
        }
        if let Some(conn) = self.journal.get_connection(&active.connection_id)?
            && conn.disconnected_at_ms.is_some()
            && conn.grace_expires_at_ms.is_some_and(|at| at <= now)
        {
            self.release_lease(Some(active), Release::DisconnectGrace, false).await?;
        }
        Ok(())
    }

    /// `releaseLease` (`core.ts:760-797`). All release paths join one
    /// settlement: cancel jobs, browser work, input and idle inhibition, then
    /// drain the effect queue; only a clean settlement clears `unsettled`.
    pub(crate) async fn release_lease(&self, lease: Option<LeaseRecord>, how: Release, inside_effect: bool) -> Result<()> {
        if self.settling.get() {
            let mut rx = self.settled.subscribe();
            while self.settling.get() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            return Ok(());
        }
        if let Some(lease) = &lease
            && self.journal.get_active_lease()?.map(|l| l.generation) != Some(lease.generation.clone())
        {
            return Ok(());
        }
        self.settling.set(true);
        let _done = SettleGuard(self);
        let generation = lease.as_ref().map(|l| l.generation.clone()).unwrap_or_else(|| id("settlement"));
        self.journal.begin_settlement(&generation)?;
        if let Some(lease) = &lease {
            if how.expires() {
                self.journal.expire_lease(&lease.generation, how.reason())?;
            } else {
                self.journal.revoke_active_leases(how.reason(), self.now_ms())?;
                if how == Release::OperatorPause {
                    // Keep the task reserved for this agent, but fence old work
                    // and approvals, including approvals answered just before take.
                    let mut paused = lease.clone();
                    paused.generation = id("lease");
                    paused.acquired_at = self.now_iso();
                    self.journal.put_lease(&paused)?;
                }
            }
            if let Some(mut task) = self.journal.get_task(&lease.task_ref)?
                && (task.state == "active" || task.state == "created")
            {
                self.stop_charging(&mut task);
                if how != Release::OperatorPause {
                    task.state = "interrupted".into();
                }
                task.updated_at = self.now_iso();
                self.journal.put_task(&task)?;
                if how == Release::OperatorPause {
                    // Only approvals tied to a held step depend on the old
                    // screen and control. Questions and access decisions survive.
                    for item in self.journal.list_attention(Some("open"), Some(&task.task_ref), 500)? {
                        if item.kind == "approval" && item.generation.is_some() {
                            self.journal.expire_attention(Some(&item.att_ref), None, how.reason(), &task.updated_at)?;
                        }
                    }
                } else {
                    self.journal.expire_attention(None, Some(&task.task_ref), how.reason(), &task.updated_at)?;
                }
            }
            self.push_event(Some(&lease.task_ref), how.phrase());
        }
        self.abort_effects();
        let cancellation_failed = self.gather_cancellation(lease.as_ref().map(|l| l.task_ref.as_str())).await;
        let drained = self.drain_effects(inside_effect).await;
        self.journal.complete_settlement(&generation, cancellation_failed || !drained)?;
        Ok(())
    }

    /// `gatherCancellation` (`core.ts:1043-1069`): true when something may
    /// still be running.
    async fn gather_cancellation(&self, task_ref: Option<&str>) -> bool {
        let mut unsettled = false;
        match self.storage.cancel_jobs(task_ref).await {
            Ok(false) | Err(_) => unsettled = true,
            Ok(true) => {}
        }
        if self.storage.has_active_jobs(task_ref) {
            unsettled = true;
        }
        if self.desktop.browser_cancel().await.is_err() {
            unsettled = true;
        }
        // The agent goes first, so no late input can hide the person's
        // pointer again, and releasing input gives the screen back.
        self.desktop.set_agent(None);
        if let Err(e) = self.desktop.release_input().await {
            log_event("release_input_failed", &e.to_string());
            unsettled = true;
        }
        if self.desktop.set_idle_inhibited(false).await.is_err() {
            unsettled = true;
        }
        unsettled || self.storage.has_active_jobs(task_ref)
    }

    /// Remaining active-control milliseconds; `None` is unlimited.
    pub(crate) fn control_remaining_ms(&self, task: &TaskRecord) -> Option<f64> {
        let limit = control_limit_ms(task)?;
        let since = task.last_charge_ms.map_or(0, |at| (self.now_ms() - at).max(0));
        Some(limit - (task.active_control_used_ms + since) as f64)
    }

    /// `charge` (`core.ts:1105-1127`): bill control time, count actions and images.
    pub(crate) fn charge(&self, task: &mut TaskRecord, kind: Charge) -> Result<()> {
        let now = self.now_ms();
        if let Some(at) = task.last_charge_ms {
            task.active_control_used_ms += (now - at).max(0);
        }
        task.last_charge_ms = Some(now);
        match kind {
            Charge::Action => task.actions_used += 1,
            Charge::Image => task.images_used += 1,
            Charge::Control => {}
        }
        let budget = |key: &str, default: i64| task.budgets.get(key).and_then(Value::as_i64).filter(|n| *n > 0).unwrap_or(default);
        if control_limit_ms(task).is_some_and(|limit| task.active_control_used_ms as f64 >= limit) {
            self.journal.put_task(task)?;
            return Err(fail("BUDGET_EXCEEDED", "Active control budget exhausted.", false));
        }
        if kind == Charge::Action && task.actions_used > budget("max_actions", 200) {
            self.journal.put_task(task)?;
            return Err(fail("BUDGET_EXCEEDED", "Action budget exhausted.", false));
        }
        if kind == Charge::Image && task.images_used > budget("image_count", 40) {
            self.journal.put_task(task)?;
            return Err(fail("BUDGET_EXCEEDED", "Image budget exhausted.", false));
        }
        task.updated_at = self.now_iso();
        self.journal.put_task(task)
    }

    /// `stopCharging`.
    pub(crate) fn stop_charging(&self, task: &mut TaskRecord) {
        if let Some(at) = task.last_charge_ms.take() {
            task.active_control_used_ms += (self.now_ms() - at).max(0);
        }
    }

    /// `touchConnection` (`core.ts:719-738`): a connection id stays bound to
    /// its first principal; a call cancels a pending disconnect grace, and
    /// shows its client is there.
    pub(crate) fn touch_connection(&self, principal: &str, connection_id: &str) -> Result<()> {
        if let Some(previous) = self.journal.get_connection(connection_id)?
            && previous.principal != principal
        {
            return Err(denied("Connection is bound to a different principal."));
        }
        self.grace_deadlines.borrow_mut().remove(connection_id);
        self.silent.borrow_mut().remove(connection_id);
        self.journal.put_connection(&ConnectionRecord {
            connection_id: connection_id.to_string(),
            principal: principal.to_string(),
            last_heartbeat_ms: self.now_ms(),
            disconnected_at_ms: None,
            grace_expires_at_ms: None,
        })
    }

    /// `heartbeat` (`core.ts:644-660`): extends this connection's lease by the
    /// idle expiry. `answering: false` says the session's client stopped
    /// answering; the lease stays, but the same agent may carry it on over a
    /// new connection.
    pub async fn heartbeat(&self, principal: &str, connection_id: &str, answering: bool) -> Result<()> {
        self.ensure_open()?;
        self.assert_identity(principal, connection_id)?;
        self.reconcile().await?;
        self.touch_connection(principal, connection_id)?;
        if !answering {
            self.silent.borrow_mut().insert(connection_id.to_string());
        }
        if let Some(mut lease) = self.journal.get_active_lease()?
            && lease.principal == principal
            && lease.connection_id == connection_id
        {
            let now = self.now_ms();
            lease.last_heartbeat_at = self.now_iso();
            lease.last_heartbeat_ms = now;
            lease.idle_expires_at_ms = now + self.idle_expiry_ms;
            self.journal.put_lease(&lease)?;
        }
        Ok(())
    }

    /// `disconnect` (`core.ts:662-678`): the lease survives for the grace period.
    pub async fn disconnect(&self, principal: &str, connection_id: &str) -> Result<()> {
        self.ensure_open()?;
        self.assert_identity(principal, connection_id)?;
        let at = self.now_ms();
        self.journal.put_connection(&ConnectionRecord {
            connection_id: connection_id.to_string(),
            principal: principal.to_string(),
            last_heartbeat_ms: at,
            disconnected_at_ms: Some(at),
            grace_expires_at_ms: Some(at + self.disconnect_grace_ms),
        })?;
        self.grace_deadlines.borrow_mut().insert(connection_id.to_string(), at + self.disconnect_grace_ms);
        self.sessions.borrow_mut().remove(connection_id);
        self.silent.borrow_mut().remove(connection_id);
        Ok(())
    }

    /// `revokeUnauthorizedLease` (`core.ts:1011-1016`).
    pub(crate) async fn revoke_unauthorized_lease(&self) -> Result<()> {
        let Some(lease) = self.journal.get_active_lease()? else {
            return Ok(());
        };
        if self.require_readable_task(&lease.principal, &lease.task_ref).is_err() {
            self.release_lease(Some(lease), Release::AuthorityRevoked, false).await?;
        }
        Ok(())
    }

    /// `availability` (`core.ts:833-843`).
    pub(crate) fn availability(&self) -> Result<&'static str> {
        let control = self.journal.get_control()?;
        if control.unsettled {
            return Ok("control_unsettled");
        }
        if control.human_control || control.paused {
            return Ok("paused");
        }
        if !self.desktop.session_available() || !control.session_hint {
            return Ok("unavailable");
        }
        if self.journal.get_active_lease()?.is_some() {
            return Ok("busy");
        }
        let degraded = self.capabilities.borrow().iter().any(|c| c.get("status").and_then(Value::as_str) == Some("degraded"));
        Ok(if degraded { "degraded" } else { "ready" })
    }

    /// `refreshCapabilities` (`core.ts:867-907`): desktop, browser and storage
    /// rows, deduplicated by name, stored in the journal.
    pub(crate) async fn refresh_capabilities(&self) -> Result<Vec<Value>> {
        let mut rows = self.desktop.capabilities().await;
        rows.push(self.browser_row());
        match self.storage.capabilities() {
            Ok(storage) => rows.extend(storage),
            Err(e) => rows.push(json!({ "name": "storage", "status": "unavailable", "backend": "storage", "reason": super::clip(&e.message, 1000) })),
        }
        let mut seen = std::collections::HashSet::new();
        let mut merged = Vec::new();
        for cap in rows {
            let Some(name) = cap.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string) else {
                continue;
            };
            if !seen.insert(name.clone()) {
                continue;
            }
            let mut row = serde_json::Map::new();
            row.insert("name".into(), json!(name));
            row.insert("status".into(), cap.get("status").cloned().unwrap_or(json!("not_tested")));
            row.insert("backend".into(), cap.get("backend").cloned().unwrap_or(json!("unknown")));
            if let Some(at) = cap.get("last_tested_at").filter(|v| !v.is_null()) {
                row.insert("last_tested_at".into(), at.clone());
            }
            if let Some(reason) = cap.get("reason").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                row.insert("reason".into(), json!(super::clip(reason, 1000)));
            }
            merged.push(Value::Object(row));
        }
        self.journal.put_capabilities(&merged)?;
        *self.capabilities.borrow_mut() = merged.clone();
        Ok(merged)
    }

    /// The `browser_semantics` row, read live: `available` while a browser
    /// with ibara's page reader is open, `not_open` while none is but the
    /// page reader is installed (it connects when one opens), `unavailable`
    /// only when it is not installed for any supported browser.
    pub(crate) fn browser_row(&self) -> Value {
        let (status, reason) = if self.desktop.browser_connected() {
            ("available", "The focused tab's top frame is read by ibara's page reader; Cua clicks and types with real input.")
        } else if self.desktop.browser_reader_installed() {
            (
                "not_open",
                "No browser is open yet. ibara's page reader is installed and connects when Chromium or Google Chrome opens; a browser that was already open when it was installed needs a restart.",
            )
        } else {
            ("unavailable", "ibara's page reader is not installed for Chromium or Google Chrome on this computer; installing ibara sets it up.")
        };
        json!({ "name": "browser_semantics", "status": status, "backend": "chrome-extension", "reason": reason })
    }

    // ---- pause and resume -----------------------------------------------------

    /// Pause (with `human_control`) for `origin`, optionally marking the
    /// controller unsettled. A system pause never replaces a person's.
    pub(crate) fn pause_control(&self, origin: PauseOrigin, unsettled: bool) -> Result<()> {
        let current = self.journal.get_control()?;
        let persons = (current.paused || current.human_control) && current.pause_origin == Some(PauseOrigin::Person);
        self.journal.set_control(ControlPatch {
            human_control: Some(true),
            paused: Some(true),
            unsettled: unsettled.then_some(true),
            pause_origin: Some(Some(if persons { PauseOrigin::Person } else { origin })),
            ..Default::default()
        })?;
        Ok(())
    }

    /// `adminPause` (`core.ts:2363-2371`), for a person or for the system.
    pub(crate) async fn admin_pause(&self, origin: PauseOrigin) -> Result<Value> {
        let holder = self.viewer_state.borrow().owner.clone();
        if let Some(holder) = holder {
            self.revoke_viewer_operator(&holder).await?;
        }
        let lease = self.journal.get_active_lease()?;
        self.abort_effects();
        self.pause_control(origin, false)?;
        let how = if origin == PauseOrigin::System { Release::Restart } else { Release::OperatorPause };
        self.release_lease(lease, how, false).await?;
        Ok(json!({ "ok": true, "availability": self.availability()?, "control": self.journal.get_control()? }))
    }

    /// `adminResume` (`core.ts:2373-2377`): also forgets who paused.
    pub(crate) async fn admin_resume(&self) -> Result<Value> {
        {
            let viewer = self.viewer_state.borrow();
            if viewer.owner.is_some() || viewer.fault {
                return Err(fail("CONTROL_UNSETTLED", "Viewer access prevents agent handback.", false));
            }
        }
        self.reconcile().await?;
        if !self.journal.get_control()?.unsettled
            && let Some(lease) = self.journal.get_active_lease()?
        {
            let mut task = self.require_readable_task(&lease.principal, &lease.task_ref)?;
            task.last_charge_ms = Some(self.now_ms());
            task.updated_at = self.now_iso();
            self.journal.put_task(&task)?;
            if let Err(e) = self.desktop.set_idle_inhibited(true).await {
                log_event("idle_inhibit_failed", &e.to_string());
            }
            self.desktop.set_agent(Some(self.task_subject(&lease.task_ref, &lease.principal)));
        }
        self.journal.set_control(ControlPatch {
            human_control: Some(false),
            paused: Some(false),
            pause_origin: Some(None),
            ..Default::default()
        })?;
        self.push_event(None, "the computer was resumed for agents");
        Ok(json!({
            "ok": true,
            "availability": self.availability()?,
            "note": "Resume lets the paused task continue when its control is still valid; it does not clear unsettled work."
        }))
    }

    /// Operator-route `pause` and `resume`. A pause is a person's: it holds
    /// across restarts and hand-backs until someone resumes. While a person
    /// holds control, pausing keeps agents paused after they hand back, and
    /// resuming is refused.
    pub(crate) async fn operator_pause(&self, pause: bool, authorize: &dyn Fn() -> Result<super::OperatorGrant>) -> Result<Value> {
        let _turn = self.viewer_lock.lock().await;
        authorize()?;
        let (held, fault) = {
            let viewer = self.viewer_state.borrow();
            (viewer.owner.is_some(), viewer.fault)
        };
        if pause && held {
            self.viewer_state.borrow_mut().pause_before_take = Some(PauseOrigin::Person);
            self.pause_control(PauseOrigin::Person, false)?;
        } else if pause {
            // No holder, so this pause revokes no viewer and needs no viewer lock.
            self.admin_pause(PauseOrigin::Person).await?;
        } else if held {
            return Err(fail("HUMAN_CONTROL", "Hand back control before resuming agents.", false));
        } else if fault {
            return Err(fail("CONTROL_UNSETTLED", "Screen sharing on this computer is being repaired; try again in a minute.", true));
        } else {
            self.admin_resume().await?;
        }
        let control = self.journal.get_control()?;
        Ok(json!({
            "paused": control.paused || control.human_control,
            "pause_origin": control.pause_origin,
            "availability": self.availability()?,
            "owner": self.viewer_owner_name()?,
            "ownership_revision": self.viewer_revision_name()?,
        }))
    }

    // ---- viewer ownership -----------------------------------------------------

    /// `viewerOwnerName` (`core.ts:291-297`).
    pub(crate) fn viewer_owner_name(&self) -> Result<String> {
        if let Some(owner) = &self.viewer_state.borrow().owner {
            return Ok(format!("operator:{owner}"));
        }
        let control = self.journal.get_control()?;
        if control.human_control || control.paused {
            return Ok("human".into());
        }
        if let Some(lease) = self.journal.get_active_lease()? {
            return Ok(format!("agent:{}:{}", lease.principal, lease.task_ref));
        }
        Ok("none".into())
    }

    /// `viewerRevisionName` (`core.ts:299-302`).
    pub(crate) fn viewer_revision_name(&self) -> Result<String> {
        let lease = self.journal.get_active_lease()?;
        let revision = self.viewer_state.borrow().revision;
        Ok(format!("{}:{}:{}", self.epoch, revision, lease.map_or_else(|| "none".to_string(), |l| l.generation)))
    }

    fn clear_viewer_owner(&self) {
        {
            let mut viewer = self.viewer_state.borrow_mut();
            viewer.owner = None;
            viewer.generation = None;
            viewer.revision += 1;
        }
        self.clipboard_stop();
    }

    /// The viewer certificate `operator_id` registered (`viewer_register`)
    /// for the pairing `grant` belongs to; none once that pairing changed.
    pub(crate) fn viewer_identity(&self, operator_id: &str, grant: &super::OperatorGrant) -> Option<String> {
        let grants = (self.operator_grants)();
        let viewer = grants.get(operator_id)?.get("viewer")?;
        let cert = viewer.get("cert_sha256")?.as_str()?;
        (is_sha256_hex(cert) && grant.generation_matches(viewer.get("generation"))).then(|| cert.to_string())
    }

    /// The stream, when this computer has one to offer.
    fn usable_stream(&self) -> Result<Rc<dyn StreamPort>> {
        match self.stream.clone() {
            Some(stream) if stream.available() && !self.viewer_state.borrow().fault => Ok(stream),
            Some(stream) if stream.available() => {
                Err(fail("CONTROL_UNSETTLED", "Screen sharing on this computer is being repaired; try again in a minute.", true))
            }
            _ => Err(fail("CAPABILITY_UNAVAILABLE", "Screen sharing is not installed on this computer.", false)),
        }
    }

    /// Close admission, wait until every key and button the viewer held is
    /// released, then end the process. A revoke that fails or is not settled
    /// still ends it, and is an error.
    async fn end_stream(&self) -> Result<()> {
        let Some(stream) = self.stream.clone() else { return Ok(()) };
        let settlement = stream.revoke().await;
        let stopped = stream.stop().await;
        match settlement? {
            Some(s) if !s.settled => Err(fail("CONTROL_UNSETTLED", "Keys held through the viewer were not released in time.", false)),
            _ => stopped,
        }
    }

    /// A fresh one-time ticket for `cert` on the stream's current generation,
    /// opening a new generation when the stream does not admit ours (it
    /// started afresh, or was revoked). The generation and the stream block
    /// for the reply.
    async fn mint_ticket(&self, stream: &Rc<dyn StreamPort>, current: Option<u64>, cert: &str) -> Result<(u64, Value)> {
        let status = stream.start().await?;
        let generation = match current {
            Some(g) if status.generation == g && !status.admission_closed => g,
            _ => {
                let g = self.stream_generation.get().max(status.generation) + 1;
                stream.open(g).await?;
                self.stream_generation.set(g);
                g
            }
        };
        let ticket = crate::server::authority::random_hex(32)?;
        stream.issue_ticket(generation, &ticket, cert, TICKET_MS).await?;
        Ok((generation, stream_block(&status, &ticket)))
    }

    /// A restarted process never inherits viewer authority: a stream an
    /// earlier `ibarad` left is revoked and ended before anyone may take control.
    pub async fn initialize_viewer(&self) -> Result<()> {
        if self.stream.is_none() {
            return Ok(());
        }
        self.pause_control(PauseOrigin::System, false)?;
        match self.end_stream().await {
            Ok(()) => {
                self.clear_viewer_owner();
                let startup = {
                    let mut viewer = self.viewer_state.borrow_mut();
                    viewer.fault = false;
                    std::mem::take(&mut viewer.startup_unsettled)
                };
                if startup {
                    self.journal.set_control(ControlPatch { unsettled: Some(false), ..Default::default() })?;
                }
                Ok(())
            }
            Err(e) => {
                let already = self.journal.get_control()?.unsettled;
                {
                    let mut viewer = self.viewer_state.borrow_mut();
                    viewer.fault = true;
                    if !already {
                        viewer.startup_unsettled = true;
                    }
                }
                self.journal.set_control(ControlPatch { unsettled: Some(true), ..Default::default() })?;
                Err(e)
            }
        }
    }

    /// End `operator_id`'s control (grant expiry, a denial, unpairing, a new
    /// viewer identity): fence the stream, wait for its input to be released
    /// and end it. The computer stays paused.
    pub async fn revoke_viewer_operator(&self, operator_id: &str) -> Result<()> {
        let _turn = self.viewer_lock.lock().await;
        if self.viewer_state.borrow().owner.as_deref() != Some(operator_id) {
            return Ok(());
        }
        self.pause_control(PauseOrigin::System, false)?;
        let outcome = self.end_stream().await;
        self.clear_viewer_owner();
        if outcome.is_err() {
            self.viewer_state.borrow_mut().fault = true;
            self.pause_control(PauseOrigin::System, true)?;
        }
        outcome
    }

    /// `sweepViewerGrant` (`core.ts:485-490`).
    pub(crate) async fn sweep_viewer_grant(&self) -> Result<()> {
        let holder = self.viewer_state.borrow().owner.clone();
        let Some(holder) = holder else {
            return Ok(());
        };
        let active = self.operator_grant(&holder).is_some_and(|g| g.active(self.now_ms()));
        if !active {
            self.revoke_viewer_operator(&holder).await?;
        }
        Ok(())
    }

    /// Take control or hand back, one at a time. Taking control fences the
    /// stream (ending any earlier holder's viewer and waiting for its keys to
    /// be released) while agents are paused and settled, then starts the
    /// stream and mints a ticket for this operator's viewer. Handing back
    /// fences and ends the stream. Once the transition starts, any failure
    /// ends the stream and fences the desktop.
    pub(crate) async fn operator_control(
        &self,
        operator_id: &str,
        take: bool,
        action: &Value,
        authorize: &dyn Fn() -> Result<super::OperatorGrant>,
    ) -> Result<Value> {
        let _turn = self.viewer_lock.lock().await;
        let mut transition_started = false;
        let outcome: Result<Value> = async {
            let grant = authorize()?;
            let stream = self.usable_stream()?;
            let expected_owner = action.get("expected_owner").and_then(Value::as_str);
            let expected_revision = action.get("expected_ownership_revision").and_then(Value::as_str);
            if expected_owner != Some(self.viewer_owner_name()?.as_str()) || expected_revision != Some(self.viewer_revision_name()?.as_str()) {
                return Err(fail("REQUEST_CONFLICT", "Desktop owner changed; refresh explicit confirmation.", true));
            }
            let holder = self.viewer_state.borrow().owner.clone();
            if !take && holder.as_deref() != Some(operator_id) {
                return Err(denied("Only the current control holder can hand back.").with_retry(true));
            }
            if take && holder.as_deref() == Some(operator_id) {
                return Err(fail("REQUEST_CONFLICT", "You already hold control.", true));
            }
            if take {
                let Some(cert) = self.viewer_identity(operator_id, &grant) else {
                    return Err(fail("CAPABILITY_UNAVAILABLE", "This computer does not know your viewer yet; choose Take Control again.", true)
                        .with("viewer_identity", "missing"));
                };
                if !self.desktop.session_available() {
                    return Err(fail("CAPABILITY_UNAVAILABLE", "Desktop session unavailable.", true));
                }
                // A locked screen is fine: the person unlocks it through the viewer.
                self.desktop.control_ready().await?;
                authorize()?;
                transition_started = true;
                // Agents settle while the stream is fenced; both must finish.
                let pause = async {
                    if holder.is_none() {
                        let control = self.journal.get_control()?;
                        self.viewer_state.borrow_mut().pause_before_take =
                            (control.human_control || control.paused).then(|| control.pause_origin.unwrap_or(PauseOrigin::System));
                        self.admin_pause(PauseOrigin::Person).await?;
                    }
                    Ok::<(), IbaraError>(())
                };
                let (fenced, paused) = tokio::join!(stream.revoke(), pause);
                paused?;
                if fenced?.is_some_and(|s| !s.settled) {
                    return Err(fail("CONTROL_UNSETTLED", "Keys held through the earlier viewer were not released in time.", false));
                }
                if holder.is_some() {
                    self.clear_viewer_owner();
                }
                if self.journal.get_control()?.unsettled {
                    return Err(fail("CONTROL_UNSETTLED", "Input settlement did not complete; viewer remains disabled.", false));
                }
                authorize()?;
                let (generation, block) = self.mint_ticket(&stream, None, &cert).await?;
                authorize()?;
                // Nothing copied before this holder's control began is theirs,
                // even when nobody held control before (a watcher left running).
                self.clipboard_stop();
                {
                    let mut viewer = self.viewer_state.borrow_mut();
                    viewer.owner = Some(operator_id.to_string());
                    viewer.generation = Some(generation);
                    viewer.revision += 1;
                }
                // Live video rests while a person holds the controls.
                self.desktop.stop_video();
                self.push_event(None, "a person took control");
                return Ok(json!({
                    "endpoint_id": self.endpoint_id,
                    "controller_epoch": self.epoch,
                    "authorization_generation": grant.generation,
                    "owner": self.viewer_owner_name()?,
                    "ownership_revision": self.viewer_revision_name()?,
                    "control_generation": generation,
                    "viewer_ready": true,
                    "stream": block,
                }));
            }
            transition_started = true;
            self.end_stream().await?;
            self.clear_viewer_owner();
            authorize()?;
            // After a person's own pause the computer stays theirs. Otherwise
            // agents resume now when settled, or the watchdog resumes them once
            // it has settled the computer.
            let before = self.viewer_state.borrow_mut().pause_before_take.take();
            if before != Some(PauseOrigin::Person) {
                if self.journal.get_control()?.unsettled {
                    self.journal.set_control(ControlPatch { pause_origin: Some(Some(PauseOrigin::System)), ..Default::default() })?;
                } else {
                    self.admin_resume().await?;
                }
            }
            self.push_event(None, "a person handed control back");
            Ok(json!({
                "endpoint_id": self.endpoint_id,
                "controller_epoch": self.epoch,
                "authorization_generation": grant.generation,
                "owner": self.viewer_owner_name()?,
                "ownership_revision": self.viewer_revision_name()?,
                // Who still pauses agents after the hand back: none (they can work
                // again), a person, or ibara while it settles the computer.
                "pause_origin": self.journal.get_control()?.pause_origin.map(|o| o.as_str()),
                "agent_resumed": !self.journal.get_control()?.human_control
                    && !self.journal.get_control()?.unsettled && self.journal.get_active_lease()?.is_some(),
            }))
        }
        .await;
        if outcome.is_err() && transition_started {
            // Nothing half-made survives: the stream ends and the computer stays
            // paused and unsettled until the watchdog has settled it.
            let _ = self.end_stream().await;
            self.viewer_state.borrow_mut().fault = true;
            self.pause_control(PauseOrigin::System, true)?;
        }
        outcome
    }

    /// `viewer_ticket` (Open Viewer): a fresh ticket for the holder's viewer,
    /// starting the stream again when closing the viewer stopped it. It
    /// changes no ownership.
    pub(crate) async fn viewer_ticket(&self, operator_id: &str, authorize: &dyn Fn() -> Result<super::OperatorGrant>) -> Result<Value> {
        let _turn = self.viewer_lock.lock().await;
        let grant = authorize()?;
        self.require_access(operator_id, "control")?;
        if self.viewer_state.borrow().owner.as_deref() != Some(operator_id) {
            return Err(denied("Take Control first; only the person who has control can open a viewer.").with_retry(true));
        }
        let stream = self.usable_stream()?;
        let Some(cert) = self.viewer_identity(operator_id, &grant) else {
            return Err(fail("CAPABILITY_UNAVAILABLE", "This computer does not know your viewer yet; choose Open Viewer again.", true)
                .with("viewer_identity", "missing"));
        };
        let current = self.viewer_state.borrow().generation;
        let (generation, block) = self.mint_ticket(&stream, current, &cert).await?;
        authorize()?;
        self.viewer_state.borrow_mut().generation = Some(generation);
        Ok(json!({
            "endpoint_id": self.endpoint_id,
            "controller_epoch": self.epoch,
            "authorization_generation": grant.generation,
            "owner": self.viewer_owner_name()?,
            "ownership_revision": self.viewer_revision_name()?,
            "control_generation": generation,
            "stream": block,
        }))
    }
}

/// A ticket lets one viewer connect within this long.
const TICKET_MS: u64 = 60_000;

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a viewer needs to connect: the ticket, the certificate to pin, the
/// ports, and the picture it asks for (the software cap when encoding is in
/// software, else 1920×1080 at 30 frames a second).
fn stream_block(status: &StreamStatus, ticket: &str) -> Value {
    let capped = status.software_cap.as_deref().and_then(|cap| {
        let (size, fps) = cap.split_once('@')?;
        let (width, height) = size.split_once('x')?;
        Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?, fps.parse::<u32>().ok()?))
    });
    let (width, height, fps, bitrate_kbps) = match capped {
        Some((w, h, f)) => (w, h, f, 4000),
        None => (1920, 1080, 30, 10_000),
    };
    json!({
        "ticket": ticket,
        "expires_in_ms": TICKET_MS,
        "server_cert_sha256": status.server_cert_sha256,
        "http_port": status.http_port,
        "https_port": status.https_port,
        "width": width,
        "height": height,
        "fps": fps,
        "bitrate_kbps": bitrate_kbps,
        "encoder": status.encoder,
        "software_cap": status.software_cap,
    })
}

/// What `charge` bills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charge {
    Action,
    Image,
    Control,
}

struct SettleGuard<'a>(&'a Controller);

impl Drop for SettleGuard<'_> {
    fn drop(&mut self) {
        self.0.settling.set(false);
        self.0.settled.send_modify(|n| *n = n.wrapping_add(1));
    }
}

trait RetrySafe {
    fn with_retry(self, retry_safe: bool) -> Self;
}

impl RetrySafe for IbaraError {
    fn with_retry(mut self, retry_safe: bool) -> Self {
        self.retry_safe = retry_safe;
        self
    }
}

/// Grant expiry is otherwise noticed only on the holder's next request.
pub(crate) async fn sweep_viewer(me: Weak<Controller>) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() {
            return;
        }
        if let Err(e) = controller.sweep_viewer_grant().await {
            log_event("viewer_sweep_failed", &e.to_string());
        }
    }
}

/// Closing the viewer ends its stream: checked every second, an idle
/// `ibara-stream` (no live ticket, no launch waiting, no stream) is stopped. The holder
/// keeps control and the computer stays paused; Open Viewer starts it again.
pub(crate) async fn stream_watch(me: Weak<Controller>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() {
            return;
        }
        controller.stop_idle_stream().await;
    }
}

impl Controller {
    /// Stop the stream when idle. A transition in progress (it holds the
    /// viewer lock) decides for itself.
    pub(crate) async fn stop_idle_stream(&self) {
        let Some(stream) = self.stream.clone() else { return };
        if !stream.status().await.ok().flatten().is_some_and(|s| s.idle) {
            return;
        }
        let Ok(_turn) = self.viewer_lock.try_lock() else { return };
        if stream.status().await.ok().flatten().is_some_and(|s| s.idle) {
            match stream.stop().await {
                Ok(()) => log_event("stream_stopped", "nobody is viewing"),
                Err(e) => log_event("stream_stop_failed", &e.to_string()),
            }
        }
    }
}

/// Lease idle expiry, control budgets and disconnect grace are reconciled
/// every two seconds (the TypeScript armed one timer per deadline).
pub(crate) async fn lease_ticks(me: Weak<Controller>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() {
            return;
        }
        let has_grace = !controller.grace_deadlines.borrow().is_empty();
        let lease = controller.journal.get_active_lease().ok().flatten();
        if lease.is_some() || has_grace || crate::access::Access::load(&controller.journal).ok().flatten().is_some() {
            if let Err(e) = controller.reconcile().await {
                log_event("reconcile_failed", &e.to_string());
            }
            let now = controller.now_ms();
            controller.grace_deadlines.borrow_mut().retain(|_, at| *at > now);
        }
        // Capture/PNG work frees large temporary allocations, but glibc can
        // retain their pages indefinitely. Return only allocator-owned free
        // pages once no task holds the desktop; live objects remain intact.
        #[cfg(target_env = "gnu")]
        if controller.journal.get_active_lease().ok().flatten().is_none() {
            // SQLite also retains pages after large captures and history reads.
            let _ = controller.journal.db().execute_batch("PRAGMA shrink_memory");
            // SAFETY: malloc_trim acts only on this process's free heap pages.
            unsafe { libc::malloc_trim(0); }
        }
    }
}

/// Parse `policy.operator_viewer_clients` (Moonlight enrolments), for the
/// one-time import of existing access.
pub(crate) fn viewer_clients_from_policy(policy: &Value) -> BTreeMap<String, String> {
    if policy.get("viewer_control_enabled") != Some(&Value::Bool(true)) {
        return BTreeMap::new();
    }
    policy
        .get("operator_viewer_clients")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|u| (k.clone(), u.to_string()))).collect())
        .unwrap_or_default()
}
