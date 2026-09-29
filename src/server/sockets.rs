//! The three socket kinds and their HTTP handling (`server.ts:189-306`).
//!
//! - `controller.sock` (0660): gateway bearer for agent kinds, or an operator
//!   bearer for `{kind:"operator"}` only.
//! - `admin.sock` (0600): admin bearer, `{kind:"admin", action}`.
//! - `/run/ibara-operator/<p>.sock` (0600 plus one named-user ACL): no bearer;
//!   the peer's `SO_PEERCRED` uid must be the operator account's.
//!
//! Framing is HTTP/1.1 `POST /v1` with a JSON body of at most 24 MiB, one
//! request per connection. Headers must arrive within 10 s and the whole
//! request within 90 s. Authentication failures are `403` with
//! `{"error":{"code":"PERMISSION_DENIED","message":"Unauthorized transport."}}`;
//! everything else is `200`, errors included, in the legacy error envelope.

use super::authority::{self, truthy, unknown_failure};
use super::{Engine, Server, peer};
use crate::desktop::run::Cancel;
use crate::error::{IbaraError, Result};
use crate::http::{MAX_BODY, write_response};
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

const HEADERS_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_HEAD: usize = 16 * 1024;
const MAIN_CONNECTIONS: usize = 32;
const PEER_CONNECTIONS: usize = 8;
/// The contract version the legacy error envelope names. No transport
/// connection negotiates any more, so it is always the TypeScript default.
const LEGACY_CONTRACT: &str = "2.0";

#[derive(Clone)]
pub enum Kind {
    Gateway,
    Admin,
    /// A per-operator peer socket for this principal.
    Peer(String),
}

/// A listening socket: its accept loop and the file to remove on close.
pub struct Listening {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Listening {
    /// Stop accepting (connections already accepted finish) and remove the socket file.
    pub fn close(self) {
        self.task.abort();
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Refuse a live socket at `path`; remove a stale one.
async fn clear_stale(path: &Path) -> Result<bool> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(true);
    }
    if UnixStream::connect(path).await.is_ok() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// Bind, set the mode and start accepting. Must run inside a `LocalSet`.
pub async fn listen<E: Engine + 'static>(server: &Rc<Server<E>>, path: &Path, mode: u32, kind: Kind) -> Result<Listening> {
    if !clear_stale(path).await? {
        return Err(IbaraError::new("INTERNAL_ERROR", "Existing controller socket is active.", false));
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(serve(server, listener, path, kind))
}

/// Accept connections on an already bound listener.
pub fn serve<E: Engine + 'static>(server: &Rc<Server<E>>, listener: UnixListener, path: &Path, kind: Kind) -> Listening {
    let limit = if matches!(kind, Kind::Peer(_)) { PEER_CONNECTIONS } else { MAIN_CONNECTIONS };
    let active = Rc::new(Cell::new(0usize));
    let server = server.clone();
    let task = tokio::task::spawn_local(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                // Out of descriptors or similar: back off instead of spinning.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if active.get() >= limit {
                drop(stream);
                continue;
            }
            let slot = Slot::take(&active);
            let (server, kind) = (server.clone(), kind.clone());
            tokio::task::spawn_local(async move {
                connection(&server, stream, &kind, slot).await;
            });
        }
    });
    Listening { path: path.to_path_buf(), task }
}

/// One of a socket's connection places, given back when the connection ends
/// or its caller hangs up, whichever comes first.
struct Slot(Option<Rc<Cell<usize>>>);

impl Slot {
    fn take(active: &Rc<Cell<usize>>) -> Slot {
        active.set(active.get() + 1);
        Slot(Some(active.clone()))
    }
    fn release(&mut self) {
        if let Some(active) = self.0.take() {
            active.set(active.get() - 1);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.release();
    }
}

/// Resolves when the caller closes the connection. Nothing is expected after
/// the body (callers read the reply on the same socket without half-closing),
/// so stray bytes are discarded.
async fn hung_up(stream: &UnixStream) {
    let mut scratch = [0u8; 256];
    loop {
        if stream.readable().await.is_err() {
            return;
        }
        match stream.try_read(&mut scratch) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: usize,
}

/// One head line, reading no more than the head's remaining budget. `None`
/// when the budget runs out before the line ends.
async fn bounded_line<R: AsyncBufReadExt + Unpin>(reader: &mut R, line: &mut String, total: &mut usize) -> std::io::Result<Option<usize>> {
    line.clear();
    let budget = MAX_HEAD.saturating_sub(*total) as u64;
    if budget == 0 {
        return Ok(None);
    }
    let n = (&mut *reader).take(budget).read_line(line).await?;
    *total += n;
    if n > 0 && !line.ends_with('\n') && *total >= MAX_HEAD {
        return Ok(None);
    }
    Ok(Some(n))
}

async fn read_head<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> std::io::Result<Option<Head>> {
    let mut line = String::new();
    let mut total = 0;
    match bounded_line(reader, &mut line, &mut total).await? {
        None | Some(0) => return Ok(None),
        Some(_) => {}
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut head = Head { method, path, authorization: None, content_length: 0 };
    loop {
        let Some(n) = bounded_line(reader, &mut line, &mut total).await? else { return Ok(None) };
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => head.authorization = Some(value.to_string()),
                "content-length" => head.content_length = value.parse().unwrap_or(usize::MAX),
                _ => {}
            }
        }
    }
    Ok(Some(head))
}

fn unauthorized() -> Value {
    json!({ "error": { "code": "PERMISSION_DENIED", "message": "Unauthorized transport." } })
}

/// `protocolError` for the legacy envelope: code, message (≤1000 characters),
/// retry_safe, requires_reconciliation (default `!retry_safe`) and recovery.
pub fn legacy_error(err: &IbaraError) -> Value {
    let mut out = Map::new();
    out.insert("code".into(), json!(err.code));
    let message: String = err.message.chars().take(1000).collect();
    let message = if message.is_empty() { unknown_failure().message } else { message };
    out.insert("message".into(), json!(message));
    out.insert("retry_safe".into(), json!(err.retry_safe));
    let reconcile = match err.details.get("requires_reconciliation") {
        Some(v) if !v.is_null() => truthy(Some(v)),
        _ => !err.retry_safe,
    };
    out.insert("requires_reconciliation".into(), json!(reconcile));
    if let Some(recovery) = err.details.get("recovery").filter(|r| truthy(Some(r))) {
        let text = recovery.as_str().map(str::to_string).unwrap_or_else(|| recovery.to_string());
        out.insert("recovery".into(), json!(text.chars().take(1000).collect::<String>()));
    }
    // Authority changed but generated transport cleanup did not finish: the
    // caller must not read this as a failed edit (docs/security-and-access.md).
    if err.details.get("access_saved") == Some(&json!(true)) {
        out.insert("access_saved".into(), json!(true));
    }
    Value::Object(out)
}

/// The error body every non-authentication failure gets (`server.ts:222-225`).
pub fn error_envelope(err: &IbaraError) -> Value {
    json!({ "result": {
        "kind": "response",
        "contract_version": LEGACY_CONTRACT,
        "status": "error",
        "records": [],
        "error": legacy_error(err),
    } })
}

enum Body {
    Json(Value),
    Failed(IbaraError),
}

async fn read_body<R: AsyncReadExt + Unpin>(reader: &mut R, head: &Head, deadline: Instant) -> Option<Body> {
    if head.content_length > MAX_BODY {
        return Some(Body::Failed(IbaraError::new("INVALID_ARGUMENT", "Request too large.", true)));
    }
    let mut bytes = vec![0u8; head.content_length];
    timeout_at(deadline, reader.read_exact(&mut bytes)).await.ok()?.ok()?;
    Some(match serde_json::from_slice(&bytes) {
        Ok(body) => Body::Json(body),
        Err(_) => Body::Failed(unknown_failure()),
    })
}

async fn connection<E: Engine + 'static>(server: &Rc<Server<E>>, mut stream: UnixStream, kind: &Kind, mut slot: Slot) {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let peer_uid = stream.peer_cred().ok().map(|cred| cred.uid());
    let mut reader = BufReader::new(&mut stream);
    let Ok(Ok(Some(head))) = timeout(HEADERS_TIMEOUT, read_head(&mut reader)).await else { return };
    let is_v1 = head.method == "POST" && head.path == "/v1";

    // Authenticate before reading the body.
    let caller = match kind {
        Kind::Gateway | Kind::Admin => {
            let bearer = head.authorization.as_deref().unwrap_or("");
            let bearer = bearer.strip_prefix("Bearer ").unwrap_or(bearer);
            let operator = matches!(kind, Kind::Gateway).then(|| authority::operator_for_bearer(server, bearer)).flatten();
            let permitted = match kind {
                Kind::Admin => server.keys.admin_ok(bearer),
                _ => server.keys.gateway_ok(bearer),
            };
            if !is_v1 || (!permitted && operator.is_none()) {
                None
            } else {
                Some(operator)
            }
        }
        Kind::Peer(principal) => {
            let accounts = peer::load_operator_accounts(
                &server.paths.operator_accounts,
                server.paths.operator_accounts_overridden,
                server.uid,
                &server.system,
            )
            .unwrap_or_default();
            let identified = peer_uid
                .is_some_and(|uid| peer::peer_identity_allowed(principal, uid, &accounts, server.uid, &server.system));
            if !is_v1 || head.authorization.as_deref().is_some_and(|a| !a.is_empty()) || !identified {
                None
            } else if !authority::peer_route_open(server, principal) {
                drop(reader);
                let _ = write_response(&mut stream, 403, &unauthorized()).await;
                close_operator_peer(server, principal);
                return;
            } else {
                Some(Some(principal.clone()))
            }
        }
    };
    let Some(operator) = caller else {
        drop(reader);
        let _ = write_response(&mut stream, 403, &unauthorized()).await;
        return;
    };
    let Some(body) = read_body(&mut reader, &head, deadline).await else { return };
    drop(reader);
    // While the request runs, watch for the caller hanging up (its own
    // deadline passed, or it died). Then the work is told to stop at its next
    // safe point and polled to the end, since dropping it mid-effect would
    // leave the effect's outcome unrecorded; the connection's place is given
    // back at once, so abandoned requests cannot lock out new callers.
    let cancel = Cancel::new();
    let mut work = std::pin::pin!(respond(server, kind, operator, body, &cancel));
    let reply = tokio::select! {
        reply = &mut work => Some(reply),
        () = hung_up(&stream) => None,
    };
    match reply {
        Some(reply) => {
            let _ = write_response(&mut stream, 200, &reply).await;
        }
        None => {
            cancel.cancel();
            drop(stream);
            slot.release();
            let _ = work.await;
        }
    }
}

async fn respond<E: Engine + 'static>(server: &Rc<Server<E>>, kind: &Kind, operator: Option<String>, body: Body, cancel: &Cancel) -> Value {
    let reply = match body {
        Body::Failed(err) => Err(err),
        Body::Json(body) => match (kind, operator) {
            (Kind::Peer(principal), _) => operator_request(server, principal, &body, "Operator peer cannot call agent or admin operations.").await,
            (_, Some(operator)) => {
                operator_request(server, &operator, &body, "Operator credential cannot call agent or admin operations.").await
            }
            (Kind::Admin, None) => admin_request(server, &body).await,
            (Kind::Gateway, None) => gateway_request(server, &body, cancel).await,
        },
    };
    reply.unwrap_or_else(|err| error_envelope(&err))
}

async fn operator_request<E: Engine + 'static>(server: &Rc<Server<E>>, operator_id: &str, body: &Value, refusal: &str) -> Result<Value> {
    let action = body.get("action").filter(|a| a.is_object());
    let (Some("operator"), Some(true), Some(action)) = (
        body.get("kind").and_then(Value::as_str),
        body.get("principal").map(|p| p.as_str() == Some(operator_id)),
        action,
    ) else {
        return Err(IbaraError::new("PERMISSION_DENIED", refusal, true));
    };
    let result = match action.get("op").and_then(Value::as_str) {
        Some("pairing_confirm") => authority::pairing_confirm(server, operator_id, action).await?,
        Some("viewer_register") => authority::viewer_register(server, operator_id, action).await?,
        _ => server.engine.operator_call(operator_id, action.clone()).await?,
    };
    Ok(json!({ "result": result }))
}

async fn admin_request<E: Engine + 'static>(server: &Rc<Server<E>>, body: &Value) -> Result<Value> {
    let action = body.get("action").filter(|a| truthy(Some(a)));
    let (Some("admin"), Some(action)) = (body.get("kind").and_then(Value::as_str), action) else {
        return Err(IbaraError::new("INVALID_ARGUMENT", "Expected admin operation.", true));
    };
    let result = match action.get("op").and_then(Value::as_str) {
        Some("enroll_operator" | "activate_operator" | "revoke_operator") => authority::operator_admin(server, action).await?,
        _ => server.engine.admin(action.clone()).await?,
    };
    Ok(json!({ "result": result }))
}

async fn gateway_request<E: Engine + 'static>(server: &Rc<Server<E>>, body: &Value, cancel: &Cancel) -> Result<Value> {
    let kind = body.get("kind").and_then(Value::as_str);
    if kind == Some("reconcile_display") {
        return Ok(json!({ "result": server.engine.admin(json!({ "op": "reconcile_display" })).await? }));
    }
    let principal = body.get("principal").and_then(Value::as_str).filter(|p| server.engine.access_principal(p).unwrap_or_else(||server.policy.allows_principal(p)));
    let connection = body.get("connectionId").and_then(Value::as_str).filter(|c| c.encode_utf16().count() <= 128);
    let (Some(principal), Some(connection)) = (principal, connection) else {
        return Err(IbaraError::new("PERMISSION_DENIED", "Unknown principal or connection.", true));
    };
    match kind {
        Some("heartbeat") => {
            // Sessions from before `answering` existed count as answering.
            let answering = body.get("answering").and_then(Value::as_bool).unwrap_or(true);
            server.engine.heartbeat(principal, connection, answering).await?;
            Ok(json!({ "ok": true }))
        }
        Some("disconnect") => {
            server.engine.disconnect(principal, connection).await?;
            Ok(json!({ "ok": true }))
        }
        // Contract 4 has no negotiation; any request is answered with "4".
        Some("negotiate") => Ok(json!({ "contract_version": crate::contract::CONTRACT_VERSION.to_string() })),
        Some("transfer") => {
            let request = body.get("request").cloned().unwrap_or(Value::Null);
            Ok(json!({ "result": server.engine.transfer(principal, connection, request).await? }))
        }
        Some("call") => {
            let tool = body.get("tool").and_then(Value::as_str).unwrap_or("");
            let args = body.get("args").cloned().filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
            let client = body.get("client_name").and_then(Value::as_str).unwrap_or("");
            let outcome = server.engine.call(principal, connection, client, tool, args, cancel.clone()).await;
            let images: Vec<Value> =
                outcome.images.iter().map(|i| json!({ "data": i.base64, "mimeType": i.mime })).collect();
            Ok(json!({ "result": outcome.envelope, "images": images }))
        }
        _ => Err(IbaraError::new("INVALID_ARGUMENT", "Unknown RPC operation.", true)),
    }
}

// ---- per-operator peer sockets (`syncOperatorPeers`, `openOperatorPeer`, `closeOperatorPeer`) ----

/// Open a socket for every account whose route is open and identity valid;
/// close every other. Runs at start-up and after enrol, activate and revoke.
pub async fn sync_operator_peers<E: Engine + 'static>(server: &Rc<Server<E>>) {
    let paths = &server.paths;
    let accounts = peer::load_operator_accounts(&paths.operator_accounts, paths.operator_accounts_overridden, server.uid, &server.system)
        .unwrap_or_default();
    let mut wanted = Vec::new();
    for (principal, account) in &accounts {
        let identity = peer::peer_identity_allowed(principal, account.uid, &accounts, server.uid, &server.system);
        if !authority::peer_route_open(server, principal) || !identity {
            continue;
        }
        wanted.push(principal.clone());
        if !server.peers.borrow().contains_key(principal) {
            open_operator_peer(server, principal, account).await;
        }
    }
    let open: Vec<String> = server.peers.borrow().keys().cloned().collect();
    for principal in open {
        if !wanted.contains(&principal) {
            close_operator_peer(server, &principal);
        }
    }
}

fn log_peer(event: &str, principal: &str) {
    eprintln!("{}", json!({ "event": event, "principal": principal }));
}

async fn open_operator_peer<E: Engine + 'static>(server: &Rc<Server<E>>, principal: &str, account: &peer::Account) {
    let dir = &server.paths.operator_socket_dir;
    if server.peers.borrow().contains_key(principal) || !peer::controller_owns_socket_dir(dir, server.uid) {
        return;
    }
    let Some(path) = peer::socket_path_for(dir, principal) else { return };
    match clear_stale(&path).await {
        Ok(true) => {}
        Ok(false) => return log_peer("operator_peer_socket_busy", principal),
        Err(_) => return,
    }
    let armed = match UnixListener::bind(&path) {
        Ok(listener) => {
            let ok = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_ok()
                && peer::arm_principal_socket(dir, principal, account, server.uid, &server.system).await;
            ok.then_some(listener)
        }
        Err(_) => None,
    };
    // A concurrent sync finds this socket live and reports it busy, so no
    // second listener can be registered for the principal.
    let Some(listener) = armed else {
        let _ = peer::disarm_principal_socket(dir, principal);
        return log_peer("operator_peer_refused", principal);
    };
    let listening = serve(server, listener, &path, Kind::Peer(principal.to_string()));
    server.peers.borrow_mut().insert(principal.to_string(), listening);
}

pub fn close_operator_peer<E: Engine + 'static>(server: &Server<E>, principal: &str) {
    let listening = server.peers.borrow_mut().remove(principal);
    if let Some(listening) = listening {
        listening.task.abort();
    }
    if peer::disarm_principal_socket(&server.paths.operator_socket_dir, principal).is_err() {
        log_peer("operator_peer_disarm_failed", principal);
    }
}

#[cfg(test)]
mod tests;
