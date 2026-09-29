//! `SSH_ORIGINAL_COMMAND=mcp`: the agent's MCP server on stdio, relaying
//! every `tools/call` to `controller.sock` (replaces `dist/mcp.js`).
//!
//! `initialize`, `ping` and `tools/list` are answered locally by
//! [`crate::mcp::serve`]. Each call is sent as
//! `{kind:"call", principal, connectionId, tool, args, client_name}` with the
//! gateway bearer; the reply `{result: <envelope>, images}` becomes the MCP
//! tool result. The client is asked every 5 s whether it is still there (an
//! MCP `ping`); a heartbeat goes every 30 s and at once when the client stops
//! or starts answering again, saying whether it answers. End of input or
//! SIGTERM sends `disconnect`. Any transport failure is an envelope with
//! status `error`, never a crash.

use super::Gateway;
use crate::contract::Envelope;
use crate::error::{CODES, IbaraError};
use crate::mcp::{CallOutcome, ClientInfo, Image};
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::time::Duration;
use tokio::time::{Instant, interval_at};

const HEARTBEAT: Duration = Duration::from_secs(30);
/// How often the client is asked whether it is still there.
const PING: Duration = Duration::from_secs(5);
/// The client stopped answering once it has said nothing for this many ping
/// intervals; fewer would misjudge a slow link, or one busy carrying a large
/// result.
const UNANSWERED: u32 = 3;
/// Calls with no long wait of their own.
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// Added to every derived deadline: dispatch, the final frame and the reply.
const MARGIN_MS: u64 = 30_000;
/// Per step, for the action itself (helpers, launch, focus) before its expectation.
const STEP_MS: u64 = 15_000;
/// The longest expectation deadline the engine honours (`within_ms` is capped there).
const MAX_EXPECT_MS: u64 = 120_000;
/// An expectation without `within_ms`: the longest per-kind default (window, url).
const DEFAULT_EXPECT_MS: u64 = 5_000;
/// `computer_wait` accepts at most ten minutes.
const MAX_WAIT_MS: u64 = 600_000;
/// A foreground command without `timeout_ms` may run for storage's maximum.
const MAX_EXEC_MS: u64 = 1_800_000;

/// How long to wait for the controller's answer to one call. The deadline
/// covers the longest the call may legitimately take, from its own
/// arguments: `computer_wait`'s `deadline_ms`, each step's expectation and
/// typing, a foreground command's timeout. Everything else, and arguments
/// the controller will refuse anyway, gets the base 90 s. Overestimating
/// only delays a report about a controller that stopped answering.
pub fn call_deadline(tool: &str, args: &Value) -> Duration {
    let derived = match tool {
        "computer_wait" => args.get("deadline_ms").and_then(Value::as_u64).map(|ms| ms.min(MAX_WAIT_MS) + MARGIN_MS),
        "computer_act" => match args.get("steps") {
            None => Some(step_ms(args) + MARGIN_MS),
            Some(Value::Array(steps)) if steps.is_empty() => Some(step_ms(args) + MARGIN_MS),
            Some(Value::Array(steps)) => Some(steps.iter().take(8).map(step_ms).sum::<u64>() + MARGIN_MS),
            Some(_) => None,
        },
        "browser_act" => {
            let wait_for = args.pointer("/action/within_ms").and_then(Value::as_u64).map_or(0, |ms| ms.min(MAX_EXPECT_MS));
            Some(step_ms(args) + wait_for + MARGIN_MS)
        }
        "computer_exec" if args.get("background") != Some(&Value::Bool(true)) => {
            let timeout = args.get("timeout_ms").map_or(Some(MAX_EXEC_MS), Value::as_u64);
            timeout.map(|ms| ms.min(MAX_EXEC_MS) + MARGIN_MS)
        }
        _ => None,
    };
    derived.map_or(CALL_TIMEOUT, |ms| Duration::from_millis(ms).max(CALL_TIMEOUT))
}

/// One step (a single-step act, a `steps[]` entry or a browser action):
/// the action, its typing and its expectation.
fn step_ms(step: &Value) -> u64 {
    let text = [step.get("text"), step.pointer("/action/text")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|t| t.chars().count() as u64)
        .sum::<u64>();
    // Cua types 16-character pieces with a 40 ms gap, about 60 ms a piece.
    let typing = text * 25 + text.div_ceil(16) * 150;
    STEP_MS + typing + step.get("expect").map_or(0, expect_ms)
}

fn expect_ms(expect: &Value) -> u64 {
    let default = match expect.get("kind").and_then(Value::as_str) {
        Some("settled") => expect.get("quiet_ms").and_then(Value::as_u64).unwrap_or(0).saturating_add(3_000),
        _ => DEFAULT_EXPECT_MS,
    };
    expect.get("within_ms").and_then(Value::as_u64).unwrap_or(default).min(MAX_EXPECT_MS)
}

struct Session<'a> {
    principal: &'a str,
    connection_id: String,
    gateway: &'a Gateway,
}

impl Session<'_> {
    async fn send(&self, kind: &str) {
        let body = json!({ "kind": kind, "principal": self.principal, "connectionId": self.connection_id });
        let _ = self.gateway.post(&body, CALL_TIMEOUT).await;
    }

    /// `answering`: whether the client answered lately. A session whose client
    /// stopped answering (its network dropped) may have its control taken over
    /// by the same agent on a new connection.
    async fn heartbeat(&self, answering: bool) {
        let body = json!({ "kind": "heartbeat", "principal": self.principal, "connectionId": self.connection_id, "answering": answering });
        let _ = self.gateway.post(&body, CALL_TIMEOUT).await;
    }

    async fn call(&self, tool: String, args: Value, client: ClientInfo) -> CallOutcome {
        let tool_name = tool.clone();
        let body = json!({
            "kind": "call",
            "principal": self.principal,
            "connectionId": self.connection_id,
            "tool": tool,
            "args": args,
            "client_name": client.name,
        });
        let deadline = call_deadline(&tool_name, &body["args"]);
        match self.gateway.post(&body, deadline).await {
            Ok(reply) => outcome(reply),
            Err(err) if err.code == "TIMEOUT" => failure(&err.requires_reconciliation()),
            Err(err) if crate::http::not_reached(&err) => starting(&tool_name),
            Err(_) => failure(&unavailable()),
        }
    }
}

/// The call may have reached the controller before it failed.
fn unavailable() -> IbaraError {
    IbaraError::new("SESSION_UNAVAILABLE", "Controller unavailable; reconcile pending effects after reconnect.", false)
        .requires_reconciliation()
}

/// Nothing listens on the controller's socket yet (ibarad is starting or
/// restarting), so nothing was sent and the same call is safe to repeat.
fn starting(tool: &str) -> CallOutcome {
    let host = host();
    let err = IbaraError::new("SESSION_UNAVAILABLE", format!("ibara on {host} is starting; try again in a few seconds."), true).with(
        "next",
        format!(
            "Nothing was sent. Call {tool} again in a few seconds with the same arguments; if this lasts more than a minute, ask a person to check ibara on {host}."
        ),
    );
    CallOutcome::new(Envelope::error(format!("{host} · ibara is starting"), Vec::new(), &err).to_value())
}

fn host() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let host = host.trim();
    if host.is_empty() { "this computer".into() } else { host.into() }
}

/// The situation line when the controller did not answer.
fn situation() -> String {
    format!("{} · controller unavailable", host())
}

fn failure(err: &IbaraError) -> CallOutcome {
    CallOutcome::new(Envelope::error(situation(), Vec::new(), err).to_value())
}

/// Rebuild an error the controller or transport returned as JSON.
fn error_from_json(value: &Value) -> IbaraError {
    let code = value.get("code").and_then(Value::as_str).unwrap_or("");
    let Some((code, _)) = CODES.iter().find(|(c, _)| *c == code) else { return unavailable() };
    let message = value.get("message").and_then(Value::as_str).unwrap_or("");
    let retry_safe = value.get("retry_safe").and_then(Value::as_bool).unwrap_or(false);
    let mut err = IbaraError::new(code, message, retry_safe);
    if let Value::Object(fields) = value {
        for (key, field) in fields {
            if !matches!(key.as_str(), "code" | "message" | "retry_safe") {
                err.details.insert(key.clone(), field.clone());
            }
        }
    }
    err
}

/// `{result: <envelope>, images}` → the outcome; anything else is an error envelope.
fn outcome(reply: Value) -> CallOutcome {
    let Value::Object(mut reply) = reply else { return failure(&unavailable()) };
    match reply.shift_remove("result") {
        Some(envelope) if envelope.get("situation").is_some() && envelope.get("status").is_some() => {
            let images = match reply.shift_remove("images") {
                Some(Value::Array(images)) => images.into_iter().filter_map(image).collect(),
                _ => Vec::new(),
            };
            CallOutcome { envelope, images }
        }
        result => {
            let error = result.as_ref().and_then(|r| r.get("error")).or_else(|| reply.get("error")).filter(|e| e.is_object());
            failure(&error.map(error_from_json).unwrap_or_else(unavailable))
        }
    }
}

fn image(value: Value) -> Option<Image> {
    let Value::Object(mut part) = value else { return None };
    let take = |part: &mut Map<String, Value>, key: &str| match part.shift_remove(key) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    };
    Some(Image { base64: take(&mut part, "data")?, mime: take(&mut part, "mimeType")? })
}

/// How often the client is asked whether it is still there;
/// `IBARA_TEST_CLIENT_PING_MS` shortens it for tests.
fn ping_interval() -> Duration {
    std::env::var("IBARA_TEST_CLIENT_PING_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(PING, Duration::from_millis)
}

/// Serve MCP on stdio until end of input or SIGTERM. Always exits 0, like `mcp.js`.
pub async fn run(principal: &str, gateway: &Gateway) -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let session = Session { principal, connection_id: crate::ids::id("connection"), gateway };
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    let ping = ping_interval();
    let heard = Cell::new(Instant::now());
    let heartbeat = async {
        let mut tick = interval_at(Instant::now() + ping, ping);
        let (mut answering, mut sent) = (true, Instant::now());
        loop {
            tick.tick().await;
            let now = heard.get().elapsed() < ping * UNANSWERED;
            if now != answering || sent.elapsed() + ping / 2 >= HEARTBEAT {
                answering = now;
                session.heartbeat(answering).await;
                sent = Instant::now();
            }
        }
    };
    let serve = crate::mcp::serve(tokio::io::stdin(), tokio::io::stdout(), |tool, args, client| session.call(tool, args, client), ping, &heard);
    tokio::select! {
        _ = serve => {}
        _ = terminate => {}
        _ = heartbeat => {}
    }
    session.send("disconnect").await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_refusals_become_error_envelopes() {
        // A 403 from the gateway (wrong bearer, unknown path).
        let out = outcome(json!({ "error": { "code": "PERMISSION_DENIED", "message": "Unauthorized transport." } }));
        assert_eq!(out.envelope["status"], "error");
        assert_eq!(out.envelope["error"]["code"], "PERMISSION_DENIED");
        assert_eq!(out.envelope["error"]["message"], "Unauthorized transport.");
        // The legacy error envelope (unknown principal).
        let out = outcome(json!({ "result": { "kind": "response", "contract_version": "2.0", "status": "error", "records": [],
            "error": { "code": "PERMISSION_DENIED", "message": "Unknown principal or connection.", "retry_safe": true, "requires_reconciliation": false } } }));
        assert_eq!(out.envelope["error"]["message"], "Unknown principal or connection.");
        assert_eq!(out.envelope["error"]["retry_safe"], true);
        assert!(out.envelope["situation"].as_str().is_some_and(|s| s.contains("controller unavailable")));
        // Garbage.
        let out = outcome(json!([1, 2]));
        assert_eq!(out.envelope["error"]["code"], "SESSION_UNAVAILABLE");
        let out = outcome(json!({ "result": { "error": { "code": "NOT_A_CODE", "message": "x" } } }));
        assert_eq!(out.envelope["error"]["code"], "SESSION_UNAVAILABLE");
    }

    fn secs(tool: &str, args: Value) -> u64 {
        call_deadline(tool, &args).as_secs()
    }

    #[test]
    fn long_waits_get_a_deadline_past_their_own() {
        // computer_wait may wait up to ten minutes.
        assert!(secs("computer_wait", json!({ "deadline_ms": 600_000 })) > 600);
        assert!(secs("computer_wait", json!({ "deadline_ms": 200_000 })) > 200);
        // Eight steps, each with the longest expectation the engine allows (120 s).
        let step = json!({ "action": { "kind": "key", "keys": "ctrl+s" }, "expect": { "kind": "window", "within_ms": 120_000 } });
        assert!(secs("computer_act", json!({ "steps": vec![step; 8] })) > 8 * 120);
        // A within_ms beyond the engine's cap does not stretch the deadline past it.
        let huge = json!({ "action": { "kind": "key", "keys": "a" }, "expect": { "kind": "window", "within_ms": 9_000_000 } });
        assert!(secs("computer_act", huge) < 400);
        // Typing 6000 characters takes time on its own.
        let typing = json!({ "action": { "kind": "type", "text": "x".repeat(6000) } });
        assert!(secs("computer_act", typing) > 120);
        let settled = json!({ "action": { "kind": "wait_for", "text": "Done", "within_ms": 100_000 }, "expect": { "kind": "settled", "quiet_ms": 100_000 } });
        assert!(secs("browser_act", settled) > 200);
        // A foreground command gets its own timeout; a background one does not wait.
        assert!(secs("computer_exec", json!({ "command": ["sleep", "300"], "timeout_ms": 300_000 })) > 300);
        assert_eq!(secs("computer_exec", json!({ "command": ["x"], "timeout_ms": 300_000, "background": true })), 90);
        // Everything else, and malformed arguments (the controller rejects those), keeps the base deadline.
        assert_eq!(secs("computer_status", json!({})), 90);
        assert_eq!(secs("computer_wait", json!({ "deadline_ms": "soon" })), 90);
        assert_eq!(secs("computer_act", json!({ "steps": 5 })), 90);
        // Never below the base deadline.
        assert!(secs("computer_act", json!({ "action": { "kind": "key", "keys": "a" } })) >= 90);
    }
}
