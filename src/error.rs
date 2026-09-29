//! The one error type every ibara surface returns, and its code registry.
//!
//! Codes are the stable public vocabulary. The registry says, for each code,
//! whether retrying the same request is safe and what the caller can do next;
//! tool descriptions and documentation are generated from it.

use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fmt;

/// Every public error code, with its default next moves.
pub const CODES: &[(&str, &str)] = &[
    ("BUSY", "Another request holds the computer or the effect queue is full. Wait, then check computer_status before retrying."),
    ("LEASE_EXPIRED", "Your control ended. Start again with computer_begin; old references are not revived."),
    ("HUMAN_CONTROL", "A person holds the computer. Wait for them to hand back; do not retry input."),
    ("STALE_TARGET", "The thing you pointed at changed. Observe again and choose a fresh target."),
    ("DISPLAY_CHANGED", "The display changed. Observe again before acting."),
    ("AMBIGUOUS_TARGET", "More than one match. Choose one of the listed candidates."),
    ("BLOCKED_BY_DIALOG", "A dialog is in the way. Handle or close the dialog first."),
    ("CAPABILITY_UNAVAILABLE", "This route is not available here; the message says why. Reach the same result another way: by element, by keys, or with another tool."),
    ("REQUEST_CONFLICT", "This request_id was already used with different arguments. Use a new request_id."),
    ("OUTCOME_UNKNOWN", "ibara cannot tell whether the effect happened. Check its state with computer_status; never repeat it blindly."),
    ("POSTCONDITION_FAILED", "The action ran but its expectation was not seen. Observe, then decide."),
    ("BUDGET_EXCEEDED", "A task budget ran out. Finish, or ask a person to extend it."),
    ("DELIVERY_UNAVAILABLE", "The destination cannot receive the result. Check the delivery setup."),
    ("WRONG_TOOL", "This needs a different tool; the error names it."),
    ("PERMISSION_DENIED", "Not allowed for you here. Ask a person for access."),
    ("SESSION_UNAVAILABLE", "The computer is not reachable or its desktop is not ready. Check computer_status."),
    ("INVALID_ARGUMENT", "The request does not fit the tool. The error names the offending field."),
    ("TIMEOUT", "The deadline passed. For effects, check the operation's state before any retry."),
    ("INTERNAL_ERROR", "ibara failed. Check computer_status; report if it repeats."),
    ("CONTROL_UNSETTLED", "Input from a previous owner is not settled yet. Wait for settlement."),
    ("UNSUPPORTED_CONTRACT", "Client and computer disagree on the contract. Update the client."),
    ("VIDEO_UNSUPPORTED", "This computer can't stream video; show pictures instead."),
];

pub fn is_known_code(code: &str) -> bool {
    CODES.iter().any(|(c, _)| *c == code)
}

pub fn next_moves(code: &str) -> &'static str {
    CODES.iter().find(|(c, _)| *c == code).map(|(_, m)| *m).unwrap_or("")
}

#[derive(Debug, Clone)]
pub struct IbaraError {
    pub code: &'static str,
    pub message: String,
    pub retry_safe: bool,
    /// Extra public fields (for example `requires_reconciliation`, `recovery`, candidates).
    pub details: Map<String, Value>,
}

impl IbaraError {
    pub fn new(code: &'static str, message: impl Into<String>, retry_safe: bool) -> Self {
        debug_assert!(is_known_code(code), "unregistered error code {code}");
        IbaraError { code, message: message.into(), retry_safe, details: Map::new() }
    }
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        self.details.insert(key.to_string(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }
    pub fn requires_reconciliation(self) -> Self {
        self.with("requires_reconciliation", true)
    }
    /// The public error object.
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("code".into(), json!(self.code));
        obj.insert("message".into(), json!(self.message));
        obj.insert("retry_safe".into(), json!(self.retry_safe));
        obj.insert(
            "requires_reconciliation".into(),
            self.details.get("requires_reconciliation").cloned().unwrap_or(json!(false)),
        );
        for (k, v) in &self.details {
            if k != "requires_reconciliation" {
                obj.insert(k.clone(), v.clone());
            }
        }
        if !obj.contains_key("next") {
            let moves = next_moves(self.code);
            if !moves.is_empty() {
                obj.insert("next".into(), json!(moves));
            }
        }
        Value::Object(obj)
    }
}

impl fmt::Display for IbaraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for IbaraError {}

impl From<rusqlite::Error> for IbaraError {
    fn from(e: rusqlite::Error) -> Self {
        IbaraError::new("INTERNAL_ERROR", format!("store: {e}"), false)
    }
}

impl From<std::io::Error> for IbaraError {
    fn from(e: std::io::Error) -> Self {
        IbaraError::new("INTERNAL_ERROR", format!("io: {e}"), false)
    }
}

pub type Result<T, E = IbaraError> = std::result::Result<T, E>;

/// Shorthand constructors.
pub fn invalid(message: impl Into<String>) -> IbaraError {
    IbaraError::new("INVALID_ARGUMENT", message, true)
}
pub fn denied(message: impl Into<String>) -> IbaraError {
    IbaraError::new("PERMISSION_DENIED", message, false)
}
pub fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", message, false)
}
pub fn internal(message: impl Into<String>) -> IbaraError {
    IbaraError::new("INTERNAL_ERROR", message, false)
}
