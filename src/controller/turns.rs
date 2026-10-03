//! Person turns, independent of agent leases and explicit Pause.

use crate::error::{IbaraError, Result};

#[derive(Debug)]
pub(crate) struct PersonTurn {
    pub operator: String,
    pub since_ms: i64,
    pub last_input_ms: i64,
    held: u64,
    gone_ms: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct Turns {
    pub person: Option<PersonTurn>,
    previous: Option<(i64, i64)>,
}

impl Turns {
    pub fn request(&mut self, operator: &str, now: i64) -> bool {
        if let Some(person) = &self.person { return person.operator == operator; }
        self.person = Some(PersonTurn { operator: operator.into(), since_ms: now, last_input_ms: now, held: 0, gone_ms: None });
        true
    }
    pub fn input(&mut self, held: u64, now: i64) {
        if let Some(p) = &mut self.person { p.held = held; p.last_input_ms = now; }
    }
    pub fn viewers(&mut self, count: u64, now: i64) {
        if let Some(p) = &mut self.person {
            if count == 0 { p.gone_ms.get_or_insert(now); } else { p.gone_ms = None; }
        }
    }
    pub fn idle(&self, now: i64, timeout: Option<i64>) -> bool {
        self.person.as_ref().is_some_and(|p| {
            p.gone_ms.is_some_and(|at| now - at >= 1_000)
                || (p.held == 0 && timeout.is_some_and(|ms| now - p.last_input_ms >= ms))
        })
    }
    pub fn end(&mut self, now: i64) {
        if let Some(p) = self.person.take() { self.previous = Some((p.since_ms, now)); }
    }
    #[cfg(test)]
    pub fn observed(&mut self) { if self.person.is_none() { self.previous = None; } }
    pub fn observed_since(&mut self, started_ms: i64) {
        if self.person.is_none() && self.previous.is_some_and(|(_, ended)| started_ms >= ended) { self.previous = None; }
    }
    pub fn before_action(&mut self) -> Result<()> {
        let error = |reason: &str, message: String| IbaraError::new("CAPABILITY_UNAVAILABLE", message, true)
            .with("reason", reason).with("execution_not_started", true);
        if self.person.is_some() { return Err(error("person_active", "A person is using this computer. Wait until their turn ends.".into())
            .with("next", "Wait until the person's turn ends, then use computer_observe before acting.")); }
        if let Some((start, end)) = self.previous.take() {
            let time = |ms| crate::ids::iso_from_millis(ms);
            return Err(error("person_was_here", format!("A person used this computer from {} to {}. Look at the screen again before acting.", time(start), time(end)))
                .with("next", "Use computer_observe to look at the screen again before acting."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_person_input_and_held_idle() {
        let mut turns = Turns::default();
        assert!(turns.request("tyler", 100));
        assert!(!turns.request("other", 200));
        turns.input(1, 300);
        assert!(!turns.idle(9_000, Some(5_000)));
        turns.input(0, 9_001);
        assert!(!turns.idle(14_000, Some(5_000)));
        assert!(turns.idle(14_001, Some(5_000)));
    }

    #[test]
    fn handback_requires_one_fresh_observation_or_refusal() {
        let mut turns = Turns::default();
        turns.request("tyler", 100);
        turns.observed();
        turns.end(200);
        let error = turns.before_action().unwrap_err();
        assert_eq!(error.details["reason"], "person_was_here");
        assert_eq!(error.details["execution_not_started"], true);
        assert!(turns.before_action().is_ok());
        turns.request("tyler", 300);
        assert_eq!(turns.before_action().unwrap_err().details["reason"], "person_active");
        turns.end(400);
        turns.observed();
        assert!(turns.before_action().is_ok());
    }

    #[test]
    fn manual_idle_and_disconnect() {
        let mut turns = Turns::default();
        turns.request("tyler", 100);
        assert!(!turns.idle(1_000_000, None));
        turns.viewers(0, 200);
        assert!(!turns.idle(1_199, None));
        assert!(turns.idle(1_200, None));
    }
}

use super::Controller;
use serde_json::{Value, json};
use std::rc::Weak;
use std::time::Duration;

impl Controller {
    pub(crate) fn own_screen(&self) -> bool {
        // Keep the active Sunshine takeover on its existing ticket/Hand Back path.
        if self.screen_generation.get() == 0 && self.viewer_state.borrow().owner.is_some()
            && self.journal.get_control().is_ok_and(|control| control.human_control) { return false; }
        if self.fallback_reason.borrow().is_some() && self.now_ms() >= self.fallback_until.get()
            && self.turns.borrow().person.is_none() && self.viewer_state.borrow().owner.is_none() {
            self.fallback_reason.borrow_mut().take();
        }
        let setting = crate::settings::current().text("screen_stream");
        (setting.as_deref() == Some("ibara") || (setting.as_deref() == Some("auto")
            && self.screen.as_ref().is_some_and(|s| s.hardware_encoder() != Some(false))))
            && self.fallback_reason.borrow().is_none()
    }
    pub(crate) fn screen_fallback_reason(&self) -> Option<String> {
        let setting = crate::settings::current().text("screen_stream");
        if setting.as_deref() == Some("auto") && self.screen.is_none() {
            return Some("ibara's screen sender isn't installed on this computer, so Join uses Sunshine.".into());
        }
        if setting.as_deref() == Some("auto")
            && self.screen.as_ref().is_some_and(|s| s.hardware_encoder() == Some(false)) {
            if self.screen.as_ref().is_some_and(|s| s.encoder().as_deref() == Some("nvenc")) {
                return Some("NVIDIA encoding is still in testing, so Join uses Sunshine. Choose ibara in Screen Stream to try it.".into());
            }
            return Some("This computer's graphics can't encode ibara's stream yet, so Join uses Sunshine.".into());
        }
        self.fallback_reason.borrow().clone().or_else(|| {
            (setting.as_deref() == Some("sunshine")).then(|| "Screen Stream is set to Sunshine.".into())
        }).or_else(|| {
            (self.viewer_state.borrow().owner.is_some() && !self.own_screen())
                .then(|| "Sunshine is already open on this computer.".into())
        })
    }
    fn screen_fallback_reply(&self, generation: &Value) -> Result<Value> {
        Ok(json!({"endpoint_id":self.endpoint_id,"controller_epoch":self.epoch,"authorization_generation":generation,
            "owner":self.viewer_owner_name()?,"ownership_revision":self.viewer_revision_name()?,
            "screen_engine":"sunshine","fallback_reason":self.screen_fallback_reason(),"fallback_required":true}))
    }
    pub(crate) fn screen_engine(&self) -> &'static str { if self.own_screen() { "ibara" } else { "sunshine" } }

    pub(crate) fn screen_input_allowed(&self, operator: &str) -> bool {
        crate::access::Access::load(&self.journal).ok().is_some_and(|access| {
            access.is_none_or(|a| a.rule(operator, "control", self.now_ms()) == super::Rule::Allow)
        })
    }
    pub(crate) async fn screen_fallback(&self, reason: &str) {
        *self.fallback_reason.borrow_mut() = Some(reason.chars().take(300).collect());
        self.fallback_until.set(self.now_ms().saturating_add(30_000));
        if let Some(screen) = &self.screen { screen.stop().await; }
        self.screen_generation.set(0);
        self.screen_admissions.borrow_mut().clear();
        self.turns.borrow_mut().end(self.now_ms());
        self.desktop.person_turn(false).await;
        if self.viewer_state.borrow().owner.is_some() { self.clear_turn_owner(); }
    }
    fn clear_turn_owner(&self) {
        let mut viewer = self.viewer_state.borrow_mut();
        viewer.owner = None;
        viewer.revision += 1;
    }
    pub(crate) async fn end_person_turn(&self) -> Result<()> {
        if self.turns.borrow().person.is_none() { return Ok(()); }
        if let Some(screen) = &self.screen {
            screen.settle().await?;
            screen.command(json!({"t":"turn","person":false,"holder":null})).await?;
        }
        self.turns.borrow_mut().end(self.now_ms());
        self.clear_turn_owner();
        self.desktop.person_turn(false).await;
        self.push_event(None, "a person handed back");
        Ok(())
    }
    pub(crate) async fn join_screen(&self, operator: &str, action: &Value, authorize: &dyn Fn() -> Result<super::OperatorGrant>) -> Result<Value> {
        let started = std::time::Instant::now();
        let grant = authorize()?;
        if !self.own_screen() {
            let mut reply = self.operator_control(operator, true, action, authorize).await?;
            reply["screen_engine"] = json!("sunshine");
            reply["fallback_reason"] = json!(self.screen_fallback_reason());
            return Ok(reply);
        }
        let _lock = self.viewer_lock.lock().await;
        let cert = self.viewer_identity(operator, &grant).ok_or_else(|| IbaraError::new("CAPABILITY_UNAVAILABLE", "This computer does not know your viewer yet; choose Join again.", true))?;
        // A live capture worker already proves the compositor and output.
        // Cold/failed senders still perform the ordinary desktop readiness check.
        if !self.screen.as_ref().is_some_and(|screen| screen.running() && screen.failure().is_none()) {
            self.desktop.control_ready().await?;
        }
        join_trace("desktop_ready", started);
        let start = match &self.screen {
            Some(screen) => screen.start().await,
            None => Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "ibara-screen is not installed on this computer.", true)),
        };
        let ready = match start {
            Ok(ready) => ready,
            Err(error) => {
                self.screen_fallback(&error.message).await;
                return self.screen_fallback_reply(&grant.generation);
            }
        };
        if !self.own_screen() {
            self.screen.as_ref().unwrap().stop().await;
            authorize()?;
            return self.screen_fallback_reply(&grant.generation);
        }
        join_trace("sender_ready", started);
        authorize()?;
        let screen = self.screen.as_ref().unwrap();
        let generation = if self.screen_generation.get() == 0 {
            let generation = self.stream_generation.get() + 1;
            screen.command(json!({"t":"open","generation":generation})).await?;
            self.screen_generation.set(generation);
            self.stream_generation.set(generation);
            generation
        } else { self.screen_generation.get() };
        let input = self.screen_input_allowed(operator);
        let ticket = crate::server::authority::random_hex(32)?;
        screen.command(json!({"t":"ticket","ticket":ticket,"client_cert_sha256":cert,
            "generation":generation,"expires_ms":self.now_ms()+60_000,"input":input})).await?;
        if let Err(error) = authorize() {
            screen.command(json!({"t":"revoke"})).await?;
            self.stream_generation.set(self.stream_generation.get().saturating_add(1));
            self.screen_admissions.borrow_mut().clear();
            self.screen_generation.set(0);
            self.end_person_turn().await?;
            return Err(error);
        }
        join_trace("ticket_ready", started);
        self.screen_admissions.borrow_mut().insert(operator.into(), (cert, grant.generation.clone()));
        self.request_stream_warm(90_000);
        Ok(json!({"endpoint_id":self.endpoint_id,"controller_epoch":self.epoch,"authorization_generation":grant.generation,
            "owner":self.viewer_owner_name()?,"ownership_revision":self.viewer_revision_name()?,"viewer_ready":true,
            "control_generation":generation,"screen_engine":"ibara","fallback_reason":null,
            "stream":{"engine":"ibara","port":47910,"server_cert_sha256":ready["cert_sha256"],
                "ticket":ticket,"input":input,"expires_in_ms":60_000}}))
    }

    async fn sender_event(&self, event: Value) -> Result<()> {
        let screen = self.screen.as_ref().unwrap();
        match event["t"].as_str().or_else(|| event["type"].as_str()) {
            Some("turn_request") => {
                let cert = event["operator_cert_sha256"].as_str().unwrap_or("");
                let operator = (self.operator_grants)().keys().find(|id| {
                    self.screen_admissions.borrow().get(*id).is_some_and(|(pin, _)| pin == cert)
                        && self.operator_grant(id).filter(|g| g.active(self.now_ms()))
                        .is_some_and(|g| self.viewer_identity(id, &g).as_deref() == Some(cert))
                        && self.screen_input_allowed(id)
                }).cloned();
                let accepted = operator.as_deref().is_some_and(|id| self.turns.borrow_mut().request(id, self.now_ms()));
                if accepted {
                    let operator = operator.unwrap();
                    self.abort_effects();
                    // Fence new agent input immediately; cursor restoration runs beside the sender turn switch.
                    self.desktop.person_turn(true).await;
                    {
                        let mut viewer = self.viewer_state.borrow_mut();
                        if viewer.owner.as_deref() != Some(&operator) { viewer.revision += 1; }
                        viewer.owner = Some(operator);
                    }
                    self.push_event(None, "a person's turn started");
                }
                let holder = self.turns.borrow().person.as_ref().map(|p| p.operator.clone());
                // The certificate binds this response to the held first input.
                screen.command(json!({"t":"turn","person":holder.is_some(),"holder":holder,
                    "operator_cert_sha256":cert,"accepted":accepted})).await?;
            }
            Some("person_input") => {
                let held = event["held"].as_u64().ok_or_else(|| IbaraError::new("CAPABILITY_UNAVAILABLE", "The screen sender returned an invalid held-input count.", true))?;
                self.turns.borrow_mut().input(held, self.now_ms());
            }
            Some("viewer") => {
                let count = event["count"].as_u64().unwrap_or(0);
                let cert = event["operator_cert_sha256"].as_str();
                let holder = self.turns.borrow().person.as_ref().map(|p| p.operator.clone());
                let for_holder = holder.as_ref().is_some_and(|id| {
                    self.screen_admissions.borrow().get(id).is_some_and(|(pin, _)| Some(pin.as_str()) == cert)
                });
                // Fleet count alone cannot tell whether the turn holder is still watching.
                if count == 0 || cert.is_none() {
                    self.turns.borrow_mut().viewers(count, self.now_ms());
                } else if for_holder {
                    if let Some(remaining) = event["operator_viewers"].as_u64() {
                        self.turns.borrow_mut().viewers(remaining, self.now_ms());
                    }
                }
                if count == 0 { self.request_stream_warm(90_000); }
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) async fn screen_watch(me: Weak<Controller>) {
    let mut last_status = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() { return; }
        let Some(screen) = &controller.screen else { return };
        if !screen.running() { continue; }
        if !controller.own_screen() {
            if let Ok(_lock) = controller.viewer_lock.try_lock() {
                if let Err(error) = controller.end_person_turn().await {
                    controller.screen_fallback(&error.message).await;
                } else {
                    screen.stop().await;
                }
                controller.screen_admissions.borrow_mut().clear();
                controller.screen_generation.set(0);
            }
            continue;
        }
        if let Ok(_lock) = controller.viewer_lock.try_lock() {
            if let Some(reason) = screen.failure() {
                // A normally expired idle child is stopped, not a stream failure.
                if screen.status().is_some_and(|s| s["viewers"] == 0)
                    && controller.stream_warm_until.get() <= controller.now_ms()
                    && controller.turns.borrow().person.is_none()
                {
                    screen.stop().await;
                    controller.screen_generation.set(0);
                    controller.screen_admissions.borrow_mut().clear();
                } else { controller.screen_fallback(&reason).await; }
                continue;
            }
            for event in screen.events() {
                if let Err(error) = controller.sender_event(event).await {
                    controller.screen_fallback(&error.message).await;
                    break;
                }
            }
            let timeout = crate::settings::current().text("hand_back_seconds").and_then(|s| s.parse::<i64>().ok()).map(|s| s * 1000);
            let idle = controller.turns.borrow().idle(controller.now_ms(), timeout);
            if idle {
                if let Err(error) = controller.end_person_turn().await { controller.screen_fallback(&error.message).await; }
            }
            if last_status.elapsed() >= Duration::from_secs(1) {
                last_status = std::time::Instant::now();
                let _ = screen.command(json!({"t":"status"})).await;
                if screen.status().is_some_and(|s| s["viewers"] == 0) && controller.stream_warm_until.get() <= controller.now_ms() {
                    screen.stop().await;
                    controller.screen_generation.set(0);
                    controller.screen_admissions.borrow_mut().clear();
                }
            }
        }
    }
}

fn join_trace(stage: &str, started: std::time::Instant) {
    if std::env::var_os("IBARA_SCREEN_TRACE").is_some() {
        eprintln!("join_stage {stage} elapsed_us={}", started.elapsed().as_micros());
    }
}
