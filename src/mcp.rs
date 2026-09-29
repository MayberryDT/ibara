//! MCP over stdio: newline-delimited JSON-RPC 2.0 on any reader and writer.
//!
//! The server answers `initialize`, `ping` and `tools/list` itself and hands
//! `tools/call` to a [`ToolHandler`]. Requests are served one at a time, in
//! order; `notifications/cancelled` is ignored because no effect is ever
//! abandoned half-way. Input is read all the while, so the server can ask the
//! client whether it is still there (a `ping`) even during a long call.

use crate::contract::{Envelope, render_text, tool_definitions};
use crate::error::IbaraError;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

pub const SERVER_NAME: &str = "ibara";
/// Newest first; the first is the answer to an unknown requested version.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const INSTRUCTIONS: &str = "ibara lets you use real computers your person owns: desktop apps, a signed-in browser, the screen and files. Use it when a task needs real input, a real browser session, a visual check or work on another computer; keep code, git and tests where you are. computer_status lists the computers; computer_begin starts a task and returns a frame; computer_act does checked steps; computer_finish proves and ends the task, so finish at the first safe point. Every response starts with a situation line, and every error names the offending field and your next moves. A pending reply needs a person's approval: computer_wait for its att_, then resend the same request_id. computer_status({ref: \"help:<tool>\"}) explains any tool; the ibara skill (/usr/share/ibara/skills/ibara/SKILL.md) explains working well. If your person says you need not ask them before you send, spend or delete, call computer_checkpoint with stop_asking: true on that computer; ibara asks them once, and you can never change your own access.";

/// Longest accepted request line; longer lines are answered with a parse error.
const MAX_LINE: usize = 4 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The client as named in `initialize`; `name` becomes the agent name (`codex`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// An image part: WebP or JPEG crop, base64-encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub mime: String,
    pub base64: String,
}

/// What a tool call produced: the envelope JSON and any images.
#[derive(Debug, Clone, PartialEq)]
pub struct CallOutcome {
    pub envelope: Value,
    pub images: Vec<Image>,
}

impl CallOutcome {
    pub fn new(envelope: Value) -> Self {
        CallOutcome { envelope, images: Vec::new() }
    }
}

/// Runs one tool call. Implemented for any `Fn(name, arguments, client) -> impl Future<Output = CallOutcome>`.
pub trait ToolHandler {
    fn call(&self, name: String, arguments: Value, client: ClientInfo) -> impl Future<Output = CallOutcome>;
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(String, Value, ClientInfo) -> Fut,
    Fut: Future<Output = CallOutcome>,
{
    fn call(&self, name: String, arguments: Value, client: ClientInfo) -> impl Future<Output = CallOutcome> {
        self(name, arguments, client)
    }
}

/// The `tools/list` array.
pub fn tools_list_json() -> Value {
    Value::Array(tool_definitions().iter().map(|t| t.to_mcp()).collect())
}

/// The `initialize` result, echoing the client's protocol version when supported.
pub fn initialize_result(requested_protocol: Option<&str>) -> Value {
    let version = requested_protocol
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// The MCP `CallToolResult` for an outcome. The text part renders the
/// envelope; with images the full envelope follows as a second text part and
/// `structuredContent` is omitted, because Codex drops images next to it.
pub fn call_result(outcome: &CallOutcome) -> Value {
    let text = match Envelope::deserialize(&outcome.envelope) {
        Ok(envelope) => render_text(&envelope),
        Err(_) => outcome.envelope.to_string(),
    };
    let is_error = outcome.envelope.get("status").and_then(Value::as_str) == Some("error");
    let mut content = vec![json!({ "type": "text", "text": text })];
    let structured = if outcome.images.is_empty() {
        Some(outcome.envelope.clone())
    } else {
        for image in &outcome.images {
            content.push(json!({ "type": "image", "data": image.base64, "mimeType": image.mime }));
        }
        content.push(json!({ "type": "text", "text": outcome.envelope.to_string() }));
        None
    };
    let mut result = Map::new();
    result.insert("content".into(), Value::Array(content));
    if let Some(envelope) = structured {
        result.insert("structuredContent".into(), envelope);
    }
    result.insert("isError".into(), is_error.into());
    Value::Object(result)
}

/// A `CallToolResult` for an error raised before any tool ran.
pub fn error_tool_result(err: &IbaraError, situation: &str) -> Value {
    call_result(&CallOutcome::new(Envelope::error(situation, Vec::new(), err).to_value()))
}

/// Serve MCP until the reader ends. Every `ping` the client is asked whether
/// it is still there; `heard` is set whenever it sends anything, so a client
/// that stopped answering shows as a `heard` that stopped moving.
pub async fn serve<R, W, H>(reader: R, writer: W, handler: H, ping: Duration, heard: &Cell<Instant>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: ToolHandler,
{
    let writer = Mutex::new(writer);
    let (queue, mut requests) = mpsc::channel::<Incoming>(QUEUE);
    let reading = async move {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = read_line(&mut reader, &mut line).await?;
            if matches!(read, Line::Eof) {
                return Ok::<(), std::io::Error>(());
            }
            heard.set(Instant::now());
            let incoming = if matches!(read, Line::TooLong) {
                Incoming::Refused(error_response(Value::Null, PARSE_ERROR, "request line too long"))
            } else if line.trim_ascii().is_empty() {
                continue;
            } else {
                match serde_json::from_slice::<Value>(&line) {
                    Err(e) => Incoming::Refused(error_response(Value::Null, PARSE_ERROR, &format!("parse error: {e}"))),
                    // The client's answer to one of our pings.
                    Ok(Value::Object(m)) if !m.contains_key("method") && (m.contains_key("result") || m.contains_key("error")) => continue,
                    Ok(message) => Incoming::Message(message),
                }
            };
            if queue.send(incoming).await.is_err() {
                return Ok(());
            }
        }
    };
    let handling = async {
        let mut server = Server { client: ClientInfo::default() };
        while let Some(incoming) = requests.recv().await {
            let reply = match incoming {
                Incoming::Refused(reply) => Some(reply),
                Incoming::Message(message) => server.handle(message, &handler).await,
            };
            if let Some(reply) = reply {
                write_line(&writer, &reply).await?;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    let pinging = async {
        let mut tick = tokio::time::interval_at(Instant::now() + ping, ping);
        for n in 1u64.. {
            tick.tick().await;
            write_line(&writer, &json!({ "jsonrpc": "2.0", "id": format!("ibara-ping-{n}"), "method": "ping" })).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {
        served = async { tokio::try_join!(reading, handling).map(|_| ()) } => served,
        failed = pinging => failed,
    }
}

/// Requests read ahead while one is being served.
const QUEUE: usize = 64;

/// One line from the client, in order: a message to serve, or the error
/// answer to a line that was not one.
enum Incoming {
    Message(Value),
    Refused(Value),
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &Mutex<W>, message: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(message).unwrap_or_default();
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

enum Line {
    Eof,
    Complete,
    TooLong,
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Line> {
    let n = (&mut *reader).take(MAX_LINE as u64 + 1).read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.len() > MAX_LINE && buf.last() != Some(&b'\n') {
        // Discard the rest of the oversized line.
        let mut skipped = Vec::new();
        loop {
            skipped.clear();
            let n = (&mut *reader).take(64 * 1024).read_until(b'\n', &mut skipped).await?;
            if n == 0 || skipped.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(Line::TooLong);
    }
    Ok(Line::Complete)
}

struct Server {
    client: ClientInfo,
}

impl Server {
    async fn handle<H: ToolHandler>(&mut self, message: Value, handler: &H) -> Option<Value> {
        let Value::Object(mut message) = message else {
            return Some(error_response(Value::Null, INVALID_REQUEST, "expected one JSON-RPC object per line"));
        };
        let id = message.shift_remove("id");
        let Some(Value::String(method)) = message.shift_remove("method") else {
            // A message that is neither a request nor an answer: answer only if it has an id.
            return id.map(|id| error_response(id, INVALID_REQUEST, "missing method"));
        };
        let params = message.shift_remove("params").unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notifications (initialized, cancelled, anything else) get no reply.
            return None;
        };
        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools_list_json() })),
            "tools/call" => self.call(params, handler).await,
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let info = &params["clientInfo"];
        self.client = ClientInfo {
            name: info["name"].as_str().unwrap_or_default().to_string(),
            version: info["version"].as_str().unwrap_or_default().to_string(),
        };
        initialize_result(params["protocolVersion"].as_str())
    }

    async fn call<H: ToolHandler>(&self, params: Value, handler: &H) -> Result<Value, (i64, String)> {
        let Value::Object(mut params) = params else {
            return Err((INVALID_PARAMS, "tools/call needs params {name, arguments}".into()));
        };
        let Some(Value::String(name)) = params.shift_remove("name") else {
            return Err((INVALID_PARAMS, "tools/call needs a tool name".into()));
        };
        if !tool_definitions().iter().any(|t| t.name == name) {
            return Err((INVALID_PARAMS, format!("unknown tool: {name}")));
        }
        let arguments = match params.shift_remove("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(args @ Value::Object(_)) => args,
            Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".into())),
        };
        let outcome = handler.call(name, arguments, self.client.clone()).await;
        Ok(call_result(&outcome))
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    struct Client {
        tx: DuplexStream,
        rx: tokio::io::Lines<BufReader<DuplexStream>>,
    }

    impl Client {
        async fn send(&mut self, line: &str) {
            self.tx.write_all(line.as_bytes()).await.unwrap();
            self.tx.write_all(b"\n").await.unwrap();
        }
        async fn recv(&mut self) -> Value {
            let line = self.rx.next_line().await.unwrap().expect("server closed");
            serde_json::from_str(&line).unwrap()
        }
        async fn request(&mut self, line: &str) -> Value {
            self.send(line).await;
            self.recv().await
        }
    }

    /// A server whose handler echoes the tool name and client, with an image
    /// when the arguments ask for one.
    fn start() -> Client {
        let (client_tx, server_rx) = tokio::io::duplex(1 << 16);
        let (server_tx, client_rx) = tokio::io::duplex(1 << 16);
        let handler = |name: String, args: Value, client: ClientInfo| async move {
            let status = if args.get("fail").is_some() { "error" } else { "ok" };
            let envelope = json!({
                "situation": format!("Tulip1 · {} called {name}", client.name),
                "status": status,
                "since": [],
                "result": { "agent": client.name },
                "error": if status == "error" { json!({"code": "BUSY", "message": "busy"}) } else { Value::Null },
            });
            let images = if args.get("image").is_some() {
                vec![Image { mime: "image/webp".into(), base64: "UklGRg==".into() }]
            } else {
                Vec::new()
            };
            CallOutcome { envelope, images }
        };
        let serving = async move {
            let heard = Cell::new(Instant::now());
            serve(server_rx, server_tx, handler, Duration::from_secs(3600), &heard).await.unwrap()
        };
        tokio::task::spawn_local(serving);
        Client { tx: client_tx, rx: BufReader::new(client_rx).lines() }
    }

    async fn run(test: impl Future<Output = ()>) {
        tokio::task::LocalSet::new().run_until(test).await;
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"codex","version":"0.9"}}}"#;

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_echoes_supported_version_and_falls_back_otherwise() {
        run(async {
            let mut c = start();
            let r = c.request(INIT).await;
            assert_eq!(r["id"], 1);
            assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
            assert_eq!(r["result"]["serverInfo"]["name"], "ibara");
            assert!(r["result"]["capabilities"]["tools"].is_object());
            let r = c
                .request(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#)
                .await;
            assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notifications_get_no_reply_and_ping_does() {
        run(async {
            let mut c = start();
            c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
            c.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#).await;
            let r = c.request(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).await;
            assert_eq!(r, json!({"jsonrpc": "2.0", "id": "p", "result": {}}));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tools_list_has_eleven_tools_within_budget() {
        run(async {
            let mut c = start();
            let r = c.request(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).await;
            let tools = r["result"]["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 11);
            assert!(serde_json::to_vec(tools).unwrap().len() <= 10 * 1024);
            assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_without_images_carries_structured_content_and_client_name() {
        run(async {
            let mut c = start();
            c.request(INIT).await;
            let r = c
                .request(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"computer_status","arguments":{}}}"#)
                .await;
            let result = &r["result"];
            assert_eq!(result["isError"], false);
            assert_eq!(result["structuredContent"]["result"]["agent"], "codex");
            assert_eq!(result["content"].as_array().unwrap().len(), 1);
            assert!(result["content"][0]["text"].as_str().unwrap().starts_with("Tulip1 · codex called computer_status"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_with_images_sends_envelope_as_text_and_no_structured_content() {
        run(async {
            let mut c = start();
            let r = c
                .request(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"computer_observe","arguments":{"image":true,"fail":true}}}"#)
                .await;
            let result = &r["result"];
            assert!(result.get("structuredContent").is_none());
            assert_eq!(result["isError"], true);
            let content = result["content"].as_array().unwrap();
            assert_eq!(content.len(), 3);
            assert_eq!(content[1], json!({"type": "image", "data": "UklGRg==", "mimeType": "image/webp"}));
            let envelope: Value = serde_json::from_str(content[2]["text"].as_str().unwrap()).unwrap();
            assert_eq!(envelope["status"], "error");
            assert!(content[0]["text"].as_str().unwrap().contains("error BUSY: busy"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_line_unknown_method_and_unknown_tool_are_errors_and_serving_continues() {
        run(async {
            let mut c = start();
            let r = c.request("{not json").await;
            assert_eq!(r["error"]["code"], PARSE_ERROR);
            assert_eq!(r["id"], Value::Null);
            let r = c.request(r#"{"jsonrpc":"2.0","id":6,"method":"resources/list"}"#).await;
            assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
            let r = c
                .request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"computer_teleport"}}"#)
                .await;
            assert_eq!(r["error"]["code"], INVALID_PARAMS);
            let r = c.request(r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#).await;
            assert_eq!(r["id"], 8);
        })
        .await;
    }
}
