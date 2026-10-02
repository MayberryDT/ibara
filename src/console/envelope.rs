//! The bridge's version-2 envelope (ibara-bridge.mjs:42-49) and its top-level
//! error mapping (ibara-bridge.mjs:866-877). `StatusModel.js` parses these
//! unchanged, so the key order and every field are the bridge's own.

use crate::error::IbaraError;
use crate::operator::js;
use serde_json::{Map, Value, json};

pub const VERSION: u32 = 2;

/// What every envelope repeats about its request.
#[derive(Debug, Clone)]
pub struct Head {
    pub request_id: String,
    pub command: String,
    /// The station node the command was routed to (`ctx.target`), or `ibara`
    /// when the request failed before a station was chosen.
    pub target: String,
}

/// Commands whose lost reply is safe to repeat (ibara-bridge.mjs:872).
const READ_ONLY: [&str; 22] = [
    "directory", "tailnet", "pair-status", "pair-requests", "invites", "operator-session", "operator-status",
    "operator-task-status", "operator-observe", "status", "operator-logs", "operator-health", "operator-tasks",
    "operator-task", "operator-artifacts", "operator-procedures", "operator-procedure", "operator-access",
    "fleet-attention", "away", "operator-windows", "update-check",
];

/// A handler failure that the bridge threw rather than returned.
#[derive(Debug)]
pub enum Fault {
    /// `new BridgeError(code, message)`.
    Coded(&'static str, String),
    /// A helper passed its deadline (`spawnSync … ETIMEDOUT`).
    Timeout(String),
    /// Anything else thrown (`new Error(message)`).
    Plain(String),
}

impl Fault {
    pub fn plain(message: impl Into<String>) -> Self {
        Fault::Plain(message.into())
    }
}

impl From<IbaraError> for Fault {
    fn from(error: IbaraError) -> Self {
        Fault::Plain(error.message)
    }
}

impl From<std::io::Error> for Fault {
    fn from(error: std::io::Error) -> Self {
        Fault::Plain(error.to_string())
    }
}

/// A handler's outcome: the finished envelope, or a fault for [`fault_envelope`].
pub type Handled = Result<Value, Fault>;

/// `new Date().toISOString()` without milliseconds.
pub fn iso_now() -> String {
    let now = crate::ids::now_iso();
    match now.rfind('.') {
        Some(dot) if now.ends_with('Z') => format!("{}Z", &now[..dot]),
        _ => now,
    }
}

/// `clip(value, limit)`: NUL removed, whitespace runs collapsed, trimmed, at most
/// `limit` UTF-16 units with an ellipsis.
pub fn clip(value: &str, limit: usize) -> String {
    let mut text = String::with_capacity(value.len());
    let mut space = false;
    for c in value.chars().filter(|c| *c != '\0') {
        if js::is_js_space(c) {
            space = true;
        } else {
            if space && !text.is_empty() {
                text.push(' ');
            }
            space = false;
            text.push(c);
        }
    }
    if js::length(&text) <= limit {
        return text;
    }
    let mut cut = js::slice_units(&text, limit.saturating_sub(1));
    cut.push('…');
    cut
}

/// The parts of an envelope that vary; `render` adds the head and the time.
pub struct Env {
    pub connection: Value,
    pub desktop: Value,
    pub owner: Value,
    pub capabilities: Value,
    pub stale: bool,
    pub data: Value,
    pub error: Value,
}

impl Env {
    /// `envelope({...ctx, connection, data})`.
    pub fn new(connection: &str, data: Value) -> Self {
        Env {
            connection: json!(connection),
            desktop: Value::Null,
            owner: json!({}),
            capabilities: json!({}),
            stale: false,
            data,
            error: Value::Null,
        }
    }

    pub fn ready(data: Value) -> Self {
        Env::new("ready", data)
    }

    pub fn render(self, head: &Head) -> Value {
        let mut out = Map::new();
        out.insert("version".into(), json!(VERSION));
        out.insert("request_id".into(), json!(head.request_id));
        out.insert("command".into(), json!(head.command));
        out.insert("target".into(), json!(head.target));
        out.insert("observed_at".into(), json!(iso_now()));
        out.insert("connection".into(), self.connection);
        out.insert("desktop".into(), self.desktop);
        out.insert("owner".into(), self.owner);
        out.insert("capabilities".into(), self.capabilities);
        out.insert("stale".into(), json!(self.stale));
        out.insert("data".into(), self.data);
        out.insert("error".into(), self.error);
        Value::Object(out)
    }
}

/// The error object: `{code, message: clip(message), retry_safe, ...details}`.
pub fn error_object(code: &str, message: &str, retry_safe: bool, details: &[(&str, Value)]) -> Value {
    let mut error = Map::new();
    error.insert("code".into(), json!(code));
    error.insert("message".into(), json!(clip(message, 400)));
    error.insert("retry_safe".into(), json!(retry_safe));
    for (key, value) in details {
        error.insert((*key).to_string(), value.clone());
    }
    Value::Object(error)
}

/// `failure(ctx, code, message, connection, retry_safe, details)`.
pub fn failure_with(
    head: &Head,
    code: &str,
    message: &str,
    connection: &str,
    retry_safe: bool,
    details: &[(&str, Value)],
) -> Value {
    let mut env = Env::new(connection, Value::Null);
    env.error = error_object(code, message, retry_safe, details);
    env.render(head)
}

pub fn failure(head: &Head, code: &str, message: &str, connection: &str, retry_safe: bool) -> Value {
    failure_with(head, code, message, connection, retry_safe, &[])
}

/// The bridge's catch block: a coded error keeps its code; a helper timeout is
/// `TIMEOUT` (offline, and a lost reply for anything that is not a read); anything
/// else is `INVALID_ARGUMENT`.
pub fn fault_envelope(head: &Head, fault: Fault) -> Value {
    let read_only = READ_ONLY.contains(&head.command.as_str());
    match fault {
        Fault::Coded(code, message) => {
            let connection = if code == "MISSING_DEPENDENCY" { "missing-dependency" } else { "failed" };
            failure(head, code, &message, connection, true)
        }
        Fault::Timeout(message) => {
            let message = if read_only {
                message
            } else {
                "The operator reply was lost. Inspect controller state and the original operation before repeating this action."
                    .to_string()
            };
            failure(head, "TIMEOUT", &message, "offline", read_only)
        }
        Fault::Plain(message) => failure(head, "INVALID_ARGUMENT", &message, "failed", true),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> Head {
        Head { request_id: "r1".into(), command: "tasks".into(), target: "tulip0".into() }
    }

    #[test]
    fn clip_collapses_whitespace_and_bounds_utf16_length() {
        assert_eq!(clip("  a\n\tb\0c  ", 400), "a bc");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("😀😀😀", 4), "😀\u{fffd}…");
    }

    #[test]
    fn a_lost_mutation_reply_is_not_retry_safe_but_a_lost_read_is() {
        let lost = fault_envelope(
            &Head { command: "pause".into(), ..head() },
            Fault::Timeout("spawnSync tailscale ETIMEDOUT".into()),
        );
        assert_eq!(lost["connection"], "offline");
        assert_eq!(lost["error"]["code"], "TIMEOUT");
        assert_eq!(lost["error"]["retry_safe"], false);
        assert!(lost["error"]["message"].as_str().unwrap().starts_with("The operator reply was lost."));
        let read = fault_envelope(&Head { command: "operator-tasks".into(), ..head() }, Fault::Timeout("spawnSync tailscale ETIMEDOUT".into()));
        assert_eq!(read["error"]["retry_safe"], true);
        assert_eq!(read["error"]["message"], "spawnSync tailscale ETIMEDOUT");
    }

    #[test]
    fn coded_faults_keep_their_code() {
        let missing = fault_envelope(&head(), Fault::Coded("MISSING_DEPENDENCY", "x".into()));
        assert_eq!(missing["connection"], "missing-dependency");
        let plain = fault_envelope(&head(), Fault::plain("Invalid computer_id."));
        assert_eq!((plain["error"]["code"].as_str(), plain["error"]["retry_safe"].as_bool()), (Some("INVALID_ARGUMENT"), Some(true)));
    }

    #[test]
    fn envelope_keys_follow_the_bridge_order() {
        let value = Env::ready(json!({"ok": true})).render(&head());
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["version", "request_id", "command", "target", "observed_at", "connection", "desktop", "owner", "capabilities", "stale", "data", "error"]
        );
        assert!(value["observed_at"].as_str().unwrap().ends_with('Z') && !value["observed_at"].as_str().unwrap().contains('.'));
    }
}
