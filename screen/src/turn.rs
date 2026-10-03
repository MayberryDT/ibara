use serde_json::Value;
#[derive(Debug, PartialEq)]
pub enum Action {
    Ignore,
    Request,
    Refused,
    Deliver(Value),
}
#[derive(Default)]
pub struct TurnGate {
    pub person: bool,
    pub holder: Option<String>,
    operator: Option<String>,
    pending: Vec<Value>,
    motion: Option<Value>,
}
impl TurnGate {
    pub fn event(&mut self, cert: &str, input: bool, event: Value) -> Action {
        if !input {
            return Action::Ignore;
        }
        if event.get("release_all") == Some(&Value::Bool(true)) {
            return if self.operator.as_deref() == Some(cert) {
                self.pending.clear();
                self.motion = None;
                Action::Deliver(event)
            } else {
                Action::Ignore
            };
        }
        if let Some(op) = self.operator.as_deref() {
            if op != cert {
                return Action::Refused;
            }
        }
        if self.person && self.operator.as_deref() == Some(cert) {
            return Action::Deliver(event);
        }
        if event.get("move").is_some() {
            self.motion = Some(event);
            return Action::Ignore;
        }
        let meaningful = event
            .get("wheel")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_i64().is_some_and(|n| n != 0)))
            || ["key", "button"].iter().any(|k| {
                event
                    .get(k)
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(1))
                    .and_then(Value::as_bool)
                    == Some(true)
            });
        if !meaningful && self.operator.is_none() {
            return Action::Ignore;
        }
        if self.pending.len() >= 256 {
            return Action::Ignore;
        }
        self.pending.push(event);
        if self.operator.is_none() {
            self.operator = Some(cert.into());
            Action::Request
        } else {
            Action::Ignore
        }
    }
    pub fn grant(&mut self, person: bool, holder: Option<&str>) -> Vec<Value> {
        self.person = person;
        self.holder = holder.map(str::to_owned);
        if !person {
            self.clear();
            return vec![];
        }
        if self.pending.is_empty() {
            return vec![];
        }
        let mut events = vec![];
        if let Some(m) = self.motion.take() {
            events.push(m);
        }
        events.append(&mut self.pending);
        events
    }
    pub fn disconnect(&mut self, cert: &str) {
        if self.owns_input(cert) {
            self.operator = None;
            self.pending.clear();
            self.motion = None;
        }
    }
    pub fn answer(&mut self, cert: &str, accepted: bool, person: bool, holder: Option<&str>) -> Vec<Value> {
        if !accepted {
            self.disconnect(cert);
            self.person = person;
            self.holder = holder.map(str::to_owned);
            return vec![];
        }
        if !self.owns_input(cert) { return vec![]; }
        self.grant(person, holder)
    }
    pub fn owns_input(&self, cert: &str) -> bool {
        self.operator.as_deref() == Some(cert)
    }
    pub fn is_operator(&self, cert: &str) -> bool {
        self.person && self.operator.as_deref() == Some(cert)
    }
    pub fn clear(&mut self) {
        self.person = false;
        self.holder = None;
        self.operator = None;
        self.pending.clear();
        self.motion = None;
    }
}
