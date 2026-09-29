//! `SSH_ORIGINAL_COMMAND=transfer-v1`: the artifact transfer line relay
//! (replaces `dist/transfer.js`).
//!
//! Each stdin line (at most 2 MiB) is one JSON request, sent as
//! `{kind:"transfer", principal, connectionId:"transfer_<32hex>", request}`;
//! the controller's reply body is printed as one line. A failed request prints
//! `DELIVERY_UNAVAILABLE`; an over-long line ends the session. At the end,
//! `{kind:"end_transfer"}` releases the connection's reader pins.

use super::{Gateway, read_line, write_line};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::BufReader;

const MAX_LINE: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(90);
const TOO_LARGE: &str = r#"{"error":{"code":"INVALID_ARGUMENT","message":"Transfer request too large"}}"#;
const FAILED: &str = r#"{"error":{"code":"DELIVERY_UNAVAILABLE","message":"Transfer failed; inspect retained state before retry"}}"#;

pub async fn run(principal: &str, gateway: &Gateway) -> i32 {
    let connection_id = crate::ids::id("transfer");
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut line = Vec::new();
    loop {
        let reply = match read_line(&mut input, MAX_LINE, &mut line).await {
            Ok(None) | Err(_) => break,
            Ok(Some(false)) => {
                let _ = write_line(&mut output, TOO_LARGE).await;
                break;
            }
            Ok(Some(true)) => match serde_json::from_slice::<Value>(&line) {
                Ok(request) => {
                    let body = json!({ "kind": "transfer", "principal": principal, "connectionId": connection_id, "request": request });
                    gateway.post(&body, TIMEOUT).await.map(|reply| reply.to_string()).unwrap_or_else(|_| FAILED.into())
                }
                Err(_) => FAILED.into(),
            },
        };
        if write_line(&mut output, &reply).await.is_err() {
            break;
        }
    }
    let end = json!({ "kind": "transfer", "principal": principal, "connectionId": connection_id, "request": { "kind": "end_transfer" } });
    let _ = gateway.post(&end, TIMEOUT).await;
    0
}
