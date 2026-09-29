//! Adding this computer to another computer's ibara.
//!
//! The pairing listener answers on each of this computer's Tailscale addresses,
//! TCP port [`tailnet::PAIRING_PORT`] (24247). Tailscale's WireGuard has
//! already authenticated the other node, so `tailscale whois` on the
//! connection's source address says which computer and which Tailscale login
//! is asking. Nothing the other computer says about itself is trusted except
//! its public key, which it proves it holds and which is enrolled for exactly
//! that computer.
//!
//! Tailscale names the computer, not the account on it, and any account can
//! make a new key. So an own computer (the same Tailscale login, neither
//! tagged) is accepted without a person only when that computer's own ibara,
//! which runs as its desktop user, vouches that its desktop user's console
//! made the request. A program running as that desktop user counts as that
//! person.
//!
//! One JSON line each way per connection:
//! - `{op:"probe"}` → `{ok, ibara:"ready", protocol}`: cheap, no identity needed.
//! - `{op:"pair", public_key, endpoint_id, signed_at, signature, nonce?}` →
//!   `{ok, request_id, code, mode, state, expires_at, route?, message?}`.
//!   `signature` is `ssh-keygen -Y sign -n ibara-pair` with that key over
//!   [`statement`]: this computer's address as the asker dialed it, the
//!   endpoint ID, the nonce and `signed_at` (milliseconds, within two minutes
//!   of this computer's clock). For an own computer this computer then asks
//!   the asker's pairing port `{op:"vouch", fingerprint, nonce}`; `vouched:
//!   true` means accept at once (`own_computer`). Anything else waits five
//!   minutes for a person here (`needs_approval`); accepting an own computer
//!   then gives it what a vouched one gets. A key already paired here, asking
//!   from the computer it was paired for, is accepted at once: it gains
//!   nothing new. A request may carry `invite`, a code this computer's owner
//!   made to share it with a friend (signed too, see [`statement`]): a good
//!   one from someone else's computer is accepted at once for exactly the
//!   invite's level (`mode:"invite"`, see [`super::invites`]).
//! - `{op:"status", request_id}` and `{op:"cancel", request_id}`: only from the
//!   computer that asked. `route` (once `paired`) is what that computer needs to
//!   reach this one: principal, endpoint, epoch, grant generation, the SSH
//!   host key, port and this computer's name.
//! - `{op:"vouch", fingerprint, nonce}` → `{ok, vouched}`: yes once, and only to
//!   the computer this computer's console registered the nonce for.
//!
//! This computer's own answers (its console, `ibara join`) use
//! `<runtime>/pairing.sock`, mode 0600, for this desktop user or root:
//! `{op:"requests"}` → `{ok, requests:[{request_id, from_owner, from_computer,
//! code, expires_at}]}`; `{op:"answer", request_id, answer:"accept"|"decline"}`
//! → `{ok, request_id, state}`; `{op:"pair", …}` adds this computer to its own
//! console (signed like any request, to this computer's own address);
//! `{op:"outgoing", fingerprint, target}` → `{ok, nonce}`: the console is about
//! to ask `target` to add this computer with that key, and this computer will
//! vouch for it once, for two minutes. `{op:"invite", level, lasts}`,
//! `{op:"invites"}` and `{op:"invite_revoke", id}` make, list and revoke
//! invites.
//!
//! Requests live only in memory: a restart forgets them, like the codes on screen.

use super::authority::{self, Enrollment};
use super::invites::{Invites, Redeemed, Used};
use super::{Engine, Server};
use crate::sshkey::{KeyLine, ed25519_line};
use crate::tailnet::{self, LINE_LIMIT, Whois};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpSocket, UnixListener};
use tokio::task::JoinHandle;

/// Pairing protocol version answered to probes.
pub const PROTOCOL: u32 = 1;
/// The `ssh-keygen -Y` namespace a pairing request is signed in.
pub const SIGN_NAMESPACE: &str = "ibara-pair";
/// The restricted SSH entry a paired computer uses.
pub const SSH_PORT: u16 = 2222;
const WINDOW_MS: i64 = 5 * 60_000;
/// Finished requests stay readable this long, for the computer that asked.
const KEEP_FINISHED_MS: i64 = 10 * 60_000;
/// How far a request's signing time may be from this computer's clock.
const SKEW_MS: i64 = 2 * 60_000;
/// How long this computer vouches for a request its console registered.
const VOUCH_FOR_MS: i64 = 2 * 60_000;
const MAX_WAITING: usize = 16;
const MAX_WAITING_PER_COMPUTER: usize = 2;
const MAX_OUTGOING: usize = 16;
const MAX_CONNECTIONS: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const VOUCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A whole exchange, including an accepted pairing's enrolment.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(40);
const REBIND_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Waiting,
    /// Accepted and being enrolled; shown as waiting.
    Accepting,
    Paired,
    Declined,
    Expired,
    Canceled,
    Failed,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Waiting | State::Accepting => "waiting",
            State::Paired => "paired",
            State::Declined => "declined",
            State::Expired => "expired",
            State::Canceled => "canceled",
            State::Failed => "failed",
        }
    }
}

struct Request {
    id: String,
    code: String,
    /// Both computers belong to the same person: accepting gives the rights an
    /// own computer gets.
    own_computer: bool,
    /// Accepted without a person: this computer adding itself, an own computer
    /// its console vouched for, a key already paired for that computer, or a
    /// friend's computer with a valid invite.
    automatic: bool,
    state: State,
    expires_at: i64,
    finished_at: i64,
    who: Whois,
    key: KeyLine,
    endpoint_id: String,
    /// This computer's name, as the other computer will show it.
    label: String,
    route: Option<Value>,
    message: Option<String>,
    /// The friend's invite this request uses (claimed until it finishes).
    invite: Option<Redeemed>,
}

impl Request {
    fn reply(&self) -> Value {
        let mode = if self.own_computer && self.automatic {
            "own_computer"
        } else if self.invite.is_some() {
            "invite"
        } else {
            "needs_approval"
        };
        let mut reply = json!({
            "ok": true, "request_id": self.id, "code": self.code, "mode": mode,
            "state": self.state.name(), "expires_at": self.expires_at,
        });
        if let Some(route) = &self.route {
            reply["route"] = route.clone();
        }
        if let Some(message) = &self.message {
            reply["message"] = json!(message);
        }
        reply
    }
}

fn refusal(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

pub(super) fn log(event: &str, detail: Value) {
    let mut line = json!({"event": event});
    if let (Some(line), Value::Object(detail)) = (line.as_object_mut(), detail) {
        line.extend(detail);
    }
    eprintln!("{line}");
}

/// Read one request line of at most [`LINE_LIMIT`] bytes.
async fn read_request<R: AsyncRead + Unpin>(reader: R) -> Option<Value> {
    let mut line = Vec::new();
    let mut limited = BufReader::new(reader).take(LINE_LIMIT as u64 + 1);
    tokio::time::timeout(READ_TIMEOUT, limited.read_until(b'\n', &mut line)).await.ok()?.ok()?;
    if line.len() > LINE_LIMIT {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

/// `^[A-Za-z0-9_.:-]{8,128}$`
fn endpoint_id(s: &str) -> bool {
    authority::charset(s, 8, 128, b"_.:-")
}

/// An OpenSSH Ed25519 `SHA256:` fingerprint: 43 unpadded base64 characters.
fn ed25519_fingerprint(s: &str) -> bool {
    s.strip_prefix("SHA256:").is_some_and(|rest| rest.len() == 43 && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'))
}

/// What a computer signs to ask the computer at `target` to add it: the
/// protocol, the address it dialed, its endpoint ID, the nonce its own ibara
/// vouches for (`-` for none), when it signed, in milliseconds, and the invite
/// code it brings, if any (that line only then).
pub fn statement(target: IpAddr, endpoint_id: &str, nonce: Option<&str>, signed_at: i64, invite: Option<&str>) -> String {
    let mut text = format!("ibara-pair-v1\ntarget {target}\nendpoint {endpoint_id}\nnonce {}\nsigned_at {signed_at}\n", nonce.unwrap_or("-"));
    if let Some(invite) = invite {
        text.push_str(&format!("invite {invite}\n"));
    }
    text
}

/// A nonce this computer hands its console: `pv_` and 32 hex digits.
fn nonce(s: &str) -> bool {
    s.strip_prefix("pv_").is_some_and(|rest| authority::is_hex_lower(rest, 32))
}

/// How a pairing request reached this computer.
#[derive(Debug, Clone, Copy)]
enum Via {
    /// The desktop user's own socket: this computer adding itself at `address`.
    Local { address: IpAddr },
    /// The pairing port: `from` dialed this computer's address `to`.
    Tailnet { from: IpAddr, to: IpAddr },
}

impl Via {
    /// This computer's address as the asker dialed it: what it signed.
    fn target(self) -> IpAddr {
        match self {
            Via::Local { address } => address,
            Via::Tailnet { to, .. } => to,
        }
    }
}

/// A request this computer's console is about to send, which this computer
/// vouches for once when `target` asks.
struct Outgoing {
    fingerprint: String,
    target: IpAddr,
    nonce: String,
    expires_at: i64,
}

pub struct Pairing<E: Engine + 'static> {
    server: Rc<Server<E>>,
    requests: RefCell<Vec<Request>>,
    outgoing: RefCell<Vec<Outgoing>>,
    window_ms: i64,
    port: u16,
    tasks: RefCell<Vec<JoinHandle<()>>>,
    listeners: RefCell<BTreeMap<IpAddr, JoinHandle<()>>>,
    /// The last bind failure logged for each address, so a retry logs only changes.
    refused: RefCell<BTreeMap<IpAddr, String>>,
    connections: Cell<usize>,
    invites: Invites,
}

impl<E: Engine + 'static> Pairing<E> {
    /// Start answering on this computer's Tailscale addresses (re-read every 30
    /// seconds, so a Tailscale that starts later is picked up) and on the local
    /// socket. Needs a `LocalSet`.
    pub fn start(server: Rc<Server<E>>) -> Rc<Pairing<E>> {
        let window_ms = std::env::var("IBARA_TEST_PAIRING_WINDOW_MS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(WINDOW_MS);
        let invites = Invites::new(server.paths.invites());
        let pairing = Rc::new(Pairing {
            server,
            requests: RefCell::new(Vec::new()),
            outgoing: RefCell::new(Vec::new()),
            window_ms,
            port: tailnet::pairing_port(),
            tasks: RefCell::new(Vec::new()),
            listeners: RefCell::new(BTreeMap::new()),
            refused: RefCell::new(BTreeMap::new()),
            connections: Cell::new(0),
            invites,
        });
        let socket = pairing.server.paths.pairing_socket();
        match bind_local(&socket) {
            Ok(listener) => {
                let task = tokio::task::spawn_local(pairing.clone().serve_local(listener));
                pairing.tasks.borrow_mut().push(task);
            }
            Err(error) => log("pairing_local_socket_failed", json!({"path": socket, "error": error.to_string()})),
        }
        let task = tokio::task::spawn_local(pairing.clone().keep_listening());
        pairing.tasks.borrow_mut().push(task);
        pairing
    }

    /// Stop every listener and remove the local socket.
    pub fn close(&self) {
        for task in self.tasks.borrow_mut().drain(..) {
            task.abort();
        }
        for (_, task) in std::mem::take(&mut *self.listeners.borrow_mut()) {
            task.abort();
        }
        let socket = self.server.paths.pairing_socket();
        if std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&socket);
        }
    }

    async fn keep_listening(self: Rc<Self>) {
        let mut last_error = None;
        loop {
            match tailnet::status().await {
                Ok(status) => {
                    last_error = None;
                    let wanted = status.own.map(|own| own.ips).unwrap_or_default();
                    self.listen_on(&wanted).await;
                }
                Err(error) => {
                    if last_error.as_ref() != Some(&error) {
                        log("pairing_tailscale_unavailable", json!({"error": format!("{error:?}")}));
                    }
                    last_error = Some(error);
                }
            }
            tokio::time::sleep(REBIND_EVERY).await;
        }
    }

    /// Listen on exactly `wanted`: new addresses are bound, gone ones closed.
    async fn listen_on(self: &Rc<Self>, wanted: &[IpAddr]) {
        let mut changed = false;
        self.listeners.borrow_mut().retain(|ip, task| {
            let keep = wanted.contains(ip);
            if !keep {
                task.abort();
                changed = true;
            }
            keep
        });
        for ip in wanted {
            if self.listeners.borrow().contains_key(ip) {
                continue;
            }
            match bind_tcp(SocketAddr::new(*ip, self.port)) {
                Ok(listener) => {
                    self.refused.borrow_mut().remove(ip);
                    let task = tokio::task::spawn_local(self.clone().serve_tcp(listener));
                    self.listeners.borrow_mut().insert(*ip, task);
                    changed = true;
                }
                Err(error) => {
                    let error = error.to_string();
                    if self.refused.borrow().get(ip) != Some(&error) {
                        log("pairing_listen_failed", json!({"address": ip.to_string(), "port": self.port, "error": error}));
                        self.refused.borrow_mut().insert(*ip, error);
                    }
                }
            }
        }
        if changed {
            let addresses: Vec<String> = self.listeners.borrow().keys().map(IpAddr::to_string).collect();
            if !addresses.is_empty() {
                log("pairing_listening", json!({"addresses": addresses, "port": self.port}));
            }
        }
    }

    async fn serve_tcp(self: Rc<Self>, listener: TcpListener) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if self.connections.get() >= MAX_CONNECTIONS {
                continue;
            }
            self.connections.set(self.connections.get() + 1);
            let this = self.clone();
            tokio::task::spawn_local(async move {
                let to = stream.local_addr().map(|a| a.ip().to_canonical());
                let (read, mut write) = stream.into_split();
                let exchange = async {
                    let reply = match (read_request(read).await, to) {
                        (Some(request), Ok(to)) => this.answer_peer(&request, peer.ip().to_canonical(), to).await,
                        _ => refusal("INVALID_ARGUMENT", "Expected one JSON request line."),
                    };
                    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
                };
                let _ = tokio::time::timeout(EXCHANGE_TIMEOUT, exchange).await;
                this.connections.set(this.connections.get() - 1);
            });
        }
    }

    /// One request from `from`, which dialed this computer's address `to`.
    async fn answer_peer(self: &Rc<Self>, request: &Value, from: IpAddr, to: IpAddr) -> Value {
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        if op == "probe" {
            return json!({"ok": true, "ibara": "ready", "protocol": PROTOCOL});
        }
        if op == "vouch" {
            return json!({"ok": true, "vouched": self.vouch(request, from)});
        }
        if !matches!(op, "pair" | "status" | "cancel") {
            return refusal("INVALID_ARGUMENT", "Unknown pairing request.");
        }
        let who = match tailnet::whois(from).await {
            Ok(who) => who,
            Err(_) => return refusal("NOT_ON_TAILNET", "Tailscale could not tell which computer is asking."),
        };
        if op == "pair" {
            // A connection from this computer's own address could come from any
            // local account; this computer adds itself over the desktop user's
            // local socket instead.
            let own = tailnet::status().await.ok().and_then(|s| s.own);
            if own.is_some_and(|o| o.ips.contains(&from)) {
                return refusal("USE_LOCAL", "Add this computer from its own console.");
            }
            return self.pair(request, who, Via::Tailnet { from, to }).await;
        }
        let id = request.get("request_id").and_then(Value::as_str).unwrap_or("");
        self.expire();
        let mut requests = self.requests.borrow_mut();
        let Some(found) = requests.iter_mut().find(|r| r.id == id && r.who.stable_id == who.stable_id) else {
            return refusal("PAIRING_UNKNOWN", "This computer has no pairing request with that ID.");
        };
        if op == "cancel" && found.state == State::Waiting {
            found.state = State::Canceled;
            found.finished_at = crate::ids::now_millis();
        }
        found.reply()
    }

    async fn pair(self: &Rc<Self>, request: &Value, who: Whois, via: Via) -> Value {
        let Some(key) = request.get("public_key").and_then(Value::as_str).and_then(ed25519_line) else {
            return refusal("INVALID_ARGUMENT", "A pairing request needs one ssh-ed25519 public key.");
        };
        let Some(endpoint) = request.get("endpoint_id").and_then(Value::as_str).filter(|e| endpoint_id(e)) else {
            return refusal("INVALID_ARGUMENT", "A pairing request needs the asking computer's endpoint ID.");
        };
        let nonce = match request.get("nonce") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_str().filter(|n| nonce(n)) {
                Some(n) => Some(n),
                None => return refusal("INVALID_ARGUMENT", "A pairing request's nonce is malformed."),
            },
        };
        let invite_code = match request.get("invite") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_str().filter(|c| c.len() <= 32 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b' ')) {
                Some(c) => Some(c),
                None => return refusal("INVALID_ARGUMENT", "A pairing request's invite code is malformed."),
            },
        };
        if let Some(refused) = self.unproven(request, &key, endpoint, nonce, invite_code, via.target()).await {
            log("pairing_refused", json!({"from_computer": who.node, "from_owner": who.login, "reason": refused["error"]["code"]}));
            return refused;
        }
        let status = match tailnet::status().await {
            Ok(status) if status.running() => status,
            _ => return refusal("TAILSCALE_UNAVAILABLE", "Tailscale isn't running on this computer."),
        };
        let own = status.own.as_ref();
        let local = matches!(via, Via::Local { .. });
        let own_computer = local || own.is_some_and(|o| tailnet::same_owner((o.user_id, &o.tags), (Some(who.user_id), &who.tags)));
        let label = self.label(own.map(|o| o.host_name.as_str()).filter(|h| !h.is_empty()));
        // A key already paired here, asking from the computer it was paired for,
        // gains nothing new.
        let known = authority::enrolled_key(&self.server, &key.fingerprint).is_some_and(|e| e.same_computer(&who.stable_id));
        let automatic = local
            || known
            || match via {
                Via::Tailnet { from, to } if own_computer => self.vouched(from, to, &key, nonce).await,
                _ => false,
            };
        // Someone else's computer with an invite code: accepted at once when the
        // code is good. An own computer is added as always, whatever it sends.
        let mut invite = match invite_code.filter(|_| !own_computer) {
            None => None,
            Some(code) => match self.invites.redeem(code, &who.stable_id) {
                Ok(redeemed) => Some(redeemed),
                Err(refused) => {
                    log("pairing_refused", json!({"from_computer": who.node, "from_owner": who.login, "reason": refused["error"]["code"]}));
                    return refused;
                }
            },
        };
        let automatic = automatic || invite.is_some();
        let release = |invite: &Option<Redeemed>| {
            if let Some(invite) = invite {
                self.invites.release(&invite.id);
            }
        };
        self.expire();
        let id = {
            let mut requests = self.requests.borrow_mut();
            let asking = |r: &Request| matches!(r.state, State::Waiting | State::Accepting) && r.who.stable_id == who.stable_id;
            // The same computer asking again with the same key: the same request,
            // accepted now if this time it needs no person.
            if let Some(same) = requests.iter_mut().find(|r| asking(r) && r.key.blob == key.blob) {
                if !automatic || same.state != State::Waiting {
                    release(&invite);
                    return same.reply();
                }
                same.automatic = true;
                if invite.is_some() {
                    same.invite = invite.take();
                }
                same.id.clone()
            } else {
                let waiting = requests.iter().filter(|r| matches!(r.state, State::Waiting | State::Accepting)).count();
                if waiting >= MAX_WAITING || requests.iter().filter(|r| asking(r)).count() >= MAX_WAITING_PER_COMPUTER {
                    release(&invite);
                    return refusal("BUSY", "Too many computers are waiting to be added here. Try again in a few minutes.");
                }
                let now = crate::ids::now_millis();
                let code = fresh_code(&requests);
                let id = crate::ids::id("pr");
                log(
                    "pairing_requested",
                    json!({"request_id": id, "from_computer": who.node, "from_owner": who.login,
                           "own_computer": own_computer, "automatic": automatic, "invite": invite.as_ref().map(|i| &i.id)}),
                );
                requests.push(Request {
                    id: id.clone(),
                    code,
                    own_computer,
                    automatic,
                    state: State::Waiting,
                    expires_at: now + self.window_ms,
                    finished_at: 0,
                    who,
                    key: key.clone(),
                    endpoint_id: endpoint.to_string(),
                    label,
                    route: None,
                    message: None,
                    invite: invite.take(),
                });
                id
            }
        };
        if automatic {
            // Its own task: a caller that hangs up never leaves it half done.
            let _ = tokio::task::spawn_local(self.clone().accept(id.clone())).await;
        }
        self.reply_for(&id)
    }

    /// Why a request is refused unless it is signed, within two minutes of this
    /// computer's clock, by the key it offers, for this computer's address as
    /// the asker dialed it (and with the invite code it brings).
    async fn unproven(&self, request: &Value, key: &KeyLine, endpoint: &str, nonce: Option<&str>, invite: Option<&str>, target: IpAddr) -> Option<Value> {
        let signed_at = request.get("signed_at").and_then(Value::as_i64);
        let signature = request.get("signature").and_then(Value::as_str);
        let (Some(signed_at), Some(signature)) = (signed_at, signature) else {
            let message = "The computer asking didn't prove it holds its ibara key. Update ibara on it, then try again.";
            return Some(refusal("UNSIGNED", message));
        };
        if crate::ids::now_millis().abs_diff(signed_at) > SKEW_MS as u64 {
            let message = "This request is more than two minutes old, or the two computers' clocks differ by more than two minutes. \
                           Check the time on both, then try again.";
            return Some(refusal("STALE_REQUEST", message));
        }
        let statement = statement(target, endpoint, nonce, signed_at, invite);
        if !crate::sshkey::verify(key, SIGN_NAMESPACE, statement.as_bytes(), signature, &self.server.paths.runtime_dir).await {
            return Some(refusal("BAD_SIGNATURE", "The request isn't signed with the key it offers, so this computer refused it."));
        }
        None
    }

    /// Whether the asking computer's own ibara vouches that its desktop user's
    /// console registered this request, with this key, for this computer.
    async fn vouched(&self, from: IpAddr, to: IpAddr, key: &KeyLine, nonce: Option<&str>) -> bool {
        let Some(nonce) = nonce else { return false };
        let ask = json!({"op": "vouch", "fingerprint": key.fingerprint, "nonce": nonce});
        match tailnet::exchange(SocketAddr::new(from, self.port), Some(to), &ask, VOUCH_TIMEOUT).await {
            Ok(reply) => reply.get("vouched") == Some(&json!(true)),
            Err(_) => false,
        }
    }

    /// Vouch once for a request this computer's console registered for `from`.
    fn vouch(&self, request: &Value, from: IpAddr) -> bool {
        let fingerprint = request.get("fingerprint").and_then(Value::as_str).unwrap_or("");
        let nonce = request.get("nonce").and_then(Value::as_str).unwrap_or("");
        let now = crate::ids::now_millis();
        let mut outgoing = self.outgoing.borrow_mut();
        outgoing.retain(|o| o.expires_at > now);
        let found = outgoing
            .iter()
            .position(|o| o.target == from && o.fingerprint == fingerprint && super::policy::ct_eq(o.nonce.as_bytes(), nonce.as_bytes()));
        found.map(|at| outgoing.remove(at)).is_some()
    }

    /// Remember a request this computer's console is about to send to `target`.
    fn register(&self, request: &Value) -> Value {
        let fingerprint = request.get("fingerprint").and_then(Value::as_str).filter(|f| ed25519_fingerprint(f));
        let target = request.get("target").and_then(Value::as_str).and_then(|t| t.parse::<IpAddr>().ok());
        let (Some(fingerprint), Some(target)) = (fingerprint, target) else {
            return refusal("INVALID_ARGUMENT", "Give the key's fingerprint and the address being asked.");
        };
        let nonce = crate::ids::id("pv");
        let now = crate::ids::now_millis();
        let mut outgoing = self.outgoing.borrow_mut();
        outgoing.retain(|o| o.expires_at > now);
        if outgoing.len() >= MAX_OUTGOING {
            outgoing.remove(0);
        }
        outgoing.push(Outgoing {
            fingerprint: fingerprint.to_string(),
            target: target.to_canonical(),
            nonce: nonce.clone(),
            expires_at: now + VOUCH_FOR_MS,
        });
        json!({"ok": true, "nonce": nonce})
    }

    fn reply_for(&self, id: &str) -> Value {
        match self.requests.borrow().iter().find(|r| r.id == id) {
            Some(request) => request.reply(),
            None => refusal("PAIRING_UNKNOWN", "This computer has no pairing request with that ID."),
        }
    }

    /// This computer's name for others: the name in its settings, else the
    /// reviewed station label, else its Tailscale host name.
    fn label(&self, host_name: Option<&str>) -> String {
        if let Some(name) = crate::settings::current().text("name") {
            return name;
        }
        let station: Option<Value> = std::fs::read(&self.server.paths.station).ok().and_then(|b| serde_json::from_slice(&b).ok());
        station
            .as_ref()
            .and_then(|s| s.get("display_label"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.chars().count() <= 80)
            .or(host_name)
            .unwrap_or("This computer")
            .to_string()
    }

    /// Waiting requests past their five minutes expire; long-finished ones are forgotten.
    fn expire(&self) {
        let now = crate::ids::now_millis();
        let mut requests = self.requests.borrow_mut();
        for request in requests.iter_mut().filter(|r| r.state == State::Waiting && r.expires_at <= now) {
            request.state = State::Expired;
            request.finished_at = now;
        }
        requests.retain(|r| r.finished_at == 0 || now - r.finished_at < KEEP_FINISHED_MS);
    }

    /// Enrol a waiting request (once); returns its state afterwards.
    async fn accept(self: Rc<Self>, id: String) -> &'static str {
        self.expire();
        let claimed = {
            let mut requests = self.requests.borrow_mut();
            let Some(request) = requests.iter_mut().find(|r| r.id == id) else { return "unknown" };
            if request.state != State::Waiting {
                // Ended before it could be accepted: its invite can be used again.
                if matches!(request.state, State::Expired | State::Declined | State::Canceled)
                    && let Some(invite) = request.invite.take()
                {
                    self.invites.release(&invite.id);
                }
                return request.state.name();
            }
            request.state = State::Accepting;
            (request.key.clone(), request.endpoint_id.clone(), request.who.clone(), request.own_computer, request.label.clone(), request.invite.clone())
        };
        let (key, endpoint, who, own_computer, label, invite) = claimed;
        let enrolled = async {
            let host_key = std::fs::read_to_string(&self.server.paths.ssh_host_key)
                .ok()
                .and_then(|text| ed25519_line(&text))
                .ok_or_else(|| crate::error::unavailable("This computer's SSH host key is unavailable."))?;
            let enrollment = Enrollment {
                key: &key,
                operator_endpoint_id: &endpoint,
                node: &who.node,
                host_name: &who.host_name,
                stable_id: &who.stable_id,
                login: &who.login,
                own_computer,
                invite: invite.as_ref(),
            };
            let (principal, generation) = authority::pair_operator(&self.server, &enrollment).await?;
            Ok::<_, crate::error::IbaraError>(json!({
                "principal": principal,
                "endpoint_id": self.server.engine.endpoint_id(),
                "controller_epoch": self.server.engine.epoch(),
                "authorization_generation": generation,
                "host_key": format!("ssh-ed25519 {}", host_key.blob),
                "port": SSH_PORT,
                "label": label,
            }))
        }
        .await;
        if let Some(invite) = &invite {
            match &enrolled {
                Ok(route) => self.invites.use_up(
                    &invite.id,
                    Used {
                        at: crate::ids::now_millis(),
                        principal: route["principal"].as_str().unwrap_or("").to_string(),
                        fingerprint: key.fingerprint.clone(),
                        login: who.login.clone(),
                        computer: who.host_name.clone(),
                    },
                ),
                Err(_) => self.invites.release(&invite.id),
            }
        }
        let mut requests = self.requests.borrow_mut();
        let Some(request) = requests.iter_mut().find(|r| r.id == id) else { return "unknown" };
        request.finished_at = crate::ids::now_millis();
        match enrolled {
            Ok(route) => {
                log(
                    "pairing_accepted",
                    json!({"request_id": id, "principal": route["principal"], "own_computer": own_computer, "invite": invite.as_ref().map(|i| &i.id)}),
                );
                request.state = State::Paired;
                request.route = Some(route);
            }
            Err(error) => {
                log("pairing_failed", json!({"request_id": id, "error": error.message}));
                request.state = State::Failed;
                request.message =
                    Some(format!("{label} accepted, but could not finish adding this computer. Update ibara on {label}, then try again."));
            }
        }
        request.state.name()
    }

    async fn serve_local(self: Rc<Self>, listener: UnixListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let uid = stream.peer_cred().map(|c| c.uid()).ok();
            if uid != Some(self.server.uid) && uid != Some(0) {
                continue;
            }
            let this = self.clone();
            tokio::task::spawn_local(async move {
                let (read, mut write) = stream.into_split();
                let reply = match read_request(read).await {
                    Some(request) => this.answer_local(&request).await,
                    None => refusal("INVALID_ARGUMENT", "Expected one JSON request line."),
                };
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            });
        }
    }

    async fn answer_local(self: &Rc<Self>, request: &Value) -> Value {
        self.expire();
        match request.get("op").and_then(Value::as_str) {
            Some("pair") => {
                let own = tailnet::status().await.ok().and_then(|s| s.own);
                let Some(address) = own.as_ref().and_then(|o| o.address()) else {
                    return refusal("TAILSCALE_UNAVAILABLE", "Tailscale isn't running on this computer.");
                };
                match tailnet::whois(address).await {
                    Ok(who) => self.pair(request, who, Via::Local { address }).await,
                    Err(_) => refusal("NOT_ON_TAILNET", "Tailscale could not tell which computer this is."),
                }
            }
            Some("outgoing") => self.register(request),
            Some("invite") => self.invites.create(request),
            Some("invites") => self.invites.list(),
            Some("invite_revoke") => {
                let id = request.get("id").and_then(Value::as_str).unwrap_or("");
                match self.invites.revoke(id) {
                    Err(refused) => refused,
                    Ok(None) => {
                        log("invite_revoked", json!({"invite": id, "ended": false}));
                        json!({"ok": true, "id": id, "ended": false})
                    }
                    // Used: that friend's pairing ends now, unless it was already
                    // replaced by another pairing. A later invite redeemed by the
                    // same computer pairs the same key under the same name, so
                    // only the invite recorded on its current pairing tells them
                    // apart. The lock keeps a new pairing from landing in between.
                    Ok(Some(used)) => {
                        let engine = &self.server.engine;
                        let serial = self.server.authority_lock.lock().await;
                        let made_by_this = authority::load(&self.server.paths.authority())
                            .ok()
                            .and_then(|records| records.get(&used.principal)?.pointer("/invite/id")?.as_str().map(|i| i == id))
                            .unwrap_or(false);
                        let ended = if made_by_this && engine.access_pairing_key(&used.principal).as_deref() == Some(used.fingerprint.as_str()) {
                            if let Err(error) = engine.access_unpair(&used.principal).await {
                                log("invite_revoke_failed", json!({"invite": id, "principal": used.principal, "error": error.message}));
                                return refusal("REVOKE_FAILED", "ibara revoked the invite but couldn't end the pairing. Remove it on the Access tab.");
                            }
                            true
                        } else {
                            false
                        };
                        drop(serial);
                        log("invite_revoked", json!({"invite": id, "ended": ended, "principal": used.principal}));
                        json!({"ok": true, "id": id, "ended": ended})
                    }
                }
            }
            Some("requests") => {
                let requests: Vec<Value> = self
                    .requests
                    .borrow()
                    .iter()
                    .filter(|r| r.state == State::Waiting && !r.automatic)
                    .map(|r| {
                        json!({"request_id": r.id, "from_owner": r.who.login, "from_computer": r.who.node,
                               "code": r.code, "expires_at": r.expires_at})
                    })
                    .collect();
                json!({"ok": true, "requests": requests})
            }
            Some("answer") => {
                let id = request.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
                let state = match request.get("answer").and_then(Value::as_str) {
                    Some("accept") => tokio::task::spawn_local(self.clone().accept(id.clone())).await.unwrap_or("failed"),
                    Some("decline") => {
                        let mut requests = self.requests.borrow_mut();
                        match requests.iter_mut().find(|r| r.id == id) {
                            Some(r) if r.state == State::Waiting => {
                                r.state = State::Declined;
                                r.finished_at = crate::ids::now_millis();
                                log("pairing_declined", json!({"request_id": id}));
                                "declined"
                            }
                            Some(r) => r.state.name(),
                            None => "unknown",
                        }
                    }
                    _ => return refusal("INVALID_ARGUMENT", "Answer accept or decline."),
                };
                if state == "unknown" {
                    return refusal("PAIRING_UNKNOWN", "This computer has no pairing request with that ID.");
                }
                json!({"ok": true, "request_id": id, "state": state})
            }
            _ => refusal("INVALID_ARGUMENT", "Unknown pairing request."),
        }
    }
}

/// Six random digits shown `NNN NNN`, unlike any code still waiting.
fn fresh_code(requests: &[Request]) -> String {
    loop {
        let n = uuid::Uuid::new_v4().as_u128() % 1_000_000;
        let code = format!("{:03} {:03}", n / 1000, n % 1000);
        if !requests.iter().any(|r| r.state == State::Waiting && r.code == code) {
            return code;
        }
    }
}

fn bind_tcp(address: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if address.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.bind(address)?;
    socket.listen(64)
}

/// Bind `<runtime>/pairing.sock` mode 0600, replacing a stale socket but never a live one.
fn bind_local(path: &Path) -> std::io::Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() || std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "pairing socket is in use"));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// This computer's pairing socket, for its console and `ibara join`.
pub fn local_socket() -> PathBuf {
    super::Paths::from_env().pairing_socket()
}

/// One request to this computer's pairing socket.
pub async fn ask_local(request: &Value) -> std::io::Result<Value> {
    let work = async {
        let stream = tokio::net::UnixStream::connect(local_socket()).await?;
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{request}\n").as_bytes()).await?;
        let mut line = Vec::new();
        BufReader::new(read).take(1024 * 1024).read_until(b'\n', &mut line).await?;
        serde_json::from_slice::<Value>(&line).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not a pairing reply"))
    };
    tokio::time::timeout(EXCHANGE_TIMEOUT, work).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}
