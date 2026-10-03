//! `SSH_ORIGINAL_COMMAND=operator-v1`: the operator line relay (replaces
//! `dist/operator.js`).
//!
//! Newline-delimited JSON on stdio, one request at a time. Each line must be
//! an object with a string `op`; it is sent as
//! `{kind:"operator", principal, action}` over the principal's peer socket
//! (production: no bearer, the controller checks `SO_PEERCRED`) or, with an
//! operator key path, over `controller.sock` with that bearer (test/admin
//! route). `pairing_confirm` must carry the fingerprint pinned by the forced
//! command. Deadlines: 150 s for `take_control`, `handback`, `pause`, `resume`,
//! `viewer_ticket` (which may start the stream), the slow everyday
//! operations (`theme_apply`, `repair`) and the file operations that read a
//! whole file (`files_publish`, `files_begin_download`), 6 s otherwise.

use super::{Gateway, read_line, write_line};
use crate::server::policy::js_trim;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::BufReader;

const MAX_LINE: usize = 2 * 1024 * 1024;
const TOO_LARGE: &str = r#"{"error":{"code":"INVALID_ARGUMENT","message":"Operator request exceeds bound."}}"#;
const INVALID_JSON: &str = r#"{"error":{"code":"OPERATOR_TRANSPORT_UNAVAILABLE","message":"Invalid operator JSON."}}"#;
const FAILED: &str = r#"{"error":{"code":"OPERATOR_TRANSPORT_UNAVAILABLE","message":"Operator request failed; inspect target status."}}"#;

#[derive(Debug, Clone)]
pub enum Route {
    /// Production: the per-principal peer socket.
    Peer(PathBuf),
    /// Test/admin: `controller.sock` with an operator bearer file.
    Bearer(Gateway),
}

impl Route {
    /// The peer socket must be an absolute path without NUL or `..`; the
    /// bearer route needs a key path (`operator.ts:5-11`).
    pub fn acceptable(&self) -> bool {
        match self {
            Route::Peer(socket) => {
                let text = socket.to_string_lossy();
                socket.is_absolute() && !text.contains('\0') && !Path::new(&*text).components().any(|c| c == Component::ParentDir)
            }
            Route::Bearer(gateway) => !gateway.key.as_os_str().is_empty(),
        }
    }

    async fn post(&self, body: &Value, timeout: Duration) -> crate::error::Result<Value> {
        match self {
            Route::Peer(socket) => crate::http::post(socket, None, body, timeout).await,
            Route::Bearer(gateway) => gateway.post(body, timeout).await,
        }
    }
}

/// The body for one line, or the fixed error line to print instead.
fn request(principal: &str, line: &[u8], fingerprint: &str) -> Result<(Value, Duration), &'static str> {
    let mut action: Value = serde_json::from_slice(line).map_err(|_| INVALID_JSON)?;
    let op = action.get("op").and_then(Value::as_str).map(str::to_string);
    let (Some(op), true) = (op, action.is_object()) else { return Err(FAILED) };
    if op == "pairing_confirm" {
        if fingerprint.is_empty() || action.get("operator_key_fingerprint").and_then(Value::as_str) != Some(fingerprint) {
            return Err(FAILED);
        }
        action["operator_key_fingerprint"] = json!(fingerprint);
    }
    let slow = matches!(op.as_str(), "join" | "screen_failed" | "take_control" | "handback" | "pause" | "resume" | "viewer_ticket" | "warm")
        || crate::controller::SLOW_OPS.contains(&op.as_str())
        || crate::controller::SLOW_FILE_OPS.contains(&op.as_str());
    let deadline = if slow { 150_000 } else { 6_000 };
    Ok((json!({ "kind": "operator", "principal": principal, "action": action }), Duration::from_millis(deadline)))
}

pub async fn run(principal: &str, route: &Route, fingerprint: &str) -> i32 {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut line = Vec::new();
    loop {
        let reply = match read_line(&mut input, MAX_LINE, &mut line).await {
            Ok(None) | Err(_) => break,
            Ok(Some(false)) => TOO_LARGE.to_string(),
            Ok(Some(true)) if js_trim(&String::from_utf8_lossy(&line)).is_empty() => continue,
            Ok(Some(true)) => match request(principal, &line, fingerprint) {
                Ok((body, deadline)) => route.post(&body, deadline).await.map(|r| r.to_string()).unwrap_or_else(|_| FAILED.into()),
                Err(fixed) => fixed.to_string(),
            },
        };
        if write_line(&mut output, &reply).await.is_err() {
            break;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_confirm_needs_the_pinned_fingerprint() {
        let line = br#"{"op":"pairing_confirm","operator_key_fingerprint":"SHA256:other"}"#;
        assert_eq!(request("vesper", line, "SHA256:pinned").unwrap_err(), FAILED);
        assert_eq!(request("vesper", line, "").unwrap_err(), FAILED);
        let line = br#"{"op":"pairing_confirm"}"#;
        assert_eq!(request("vesper", line, "SHA256:pinned").unwrap_err(), FAILED);
        assert_eq!(request("vesper", b"{", "").unwrap_err(), INVALID_JSON);
        assert_eq!(request("vesper", br#"["op"]"#, "").unwrap_err(), FAILED);
        assert_eq!(request("vesper", br#"{"op":5}"#, "").unwrap_err(), FAILED);
        let (body, deadline) = request("vesper", br#"{"op":"handback"}"#, "").unwrap();
        assert_eq!(body, json!({ "kind": "operator", "principal": "vesper", "action": { "op": "handback" } }));
        assert_eq!(deadline, Duration::from_secs(150));
    }

    #[test]
    fn peer_socket_path_must_be_plain_and_absolute() {
        assert!(!Route::Peer("relative/p.sock".into()).acceptable());
        assert!(!Route::Peer("/run/ibara-operator/../agent-computer/controller.sock".into()).acceptable());
        assert!(Route::Peer("/run/ibara-operator/vesper.sock".into()).acceptable());
    }
}
