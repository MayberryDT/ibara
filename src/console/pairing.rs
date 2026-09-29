//! Adding computers from this console, and answering requests to add this one
//! (the other side is `server::pairing`).
//!
//! - `tailnet`: this computer and its Linux peers, each probed on the pairing
//!   port, with the directory's pairing state; `changed` marks an added
//!   computer whose SSH host key is not the one pinned here (it was reinstalled).
//! - `pair-start NODE [INVITE_CODE]`: create `~/.ssh/ibara_agent_ed25519` when missing, have
//!   this computer's own ibara register the request (so it can vouch for it),
//!   ask NODE with a request signed by that key, and follow it in the
//!   background. Once NODE accepts, pin its SSH host key, write its verified
//!   directory row and prove the selected route with a `session` call. A
//!   computer added before that now answers as a new one (reinstalled) takes
//!   over its old row.
//! - `pair-status ID`, `pair-cancel ID`.
//! - `remove-computer --computer NAME`: forget an added computer here.
//! - `pair-requests`, `pair-answer ID accept|decline`: requests waiting for a
//!   person on this computer, through its target daemon's pairing socket.
//! - `invite-create LEVEL LASTS`, `invites`, `invite-revoke ID`: sharing this
//!   computer with a friend, through the same socket.
//!
//! Requests this console started live in memory, like the target's.

use super::envelope::{Fault, Handled};
use super::{Console, Ctx, validated_id};
use crate::operator::directory::{ComputerRecord, OperatorDirectory};
use crate::operator::{current_uid, home_dir, pattern};
use crate::sshkey::{KeyLine, ed25519_line};
use crate::tailnet::{self, CliError, Peer, Status};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// A pairing request, including the other computer enrolling this one.
const PAIR_TIMEOUT: Duration = Duration::from_secs(45);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long a new route may take to answer (its account and socket appear within seconds).
const VERIFY_FOR: Duration = Duration::from_secs(15);
const KEEP_REQUESTS: usize = 64;

/// One request this console started.
#[derive(Debug, Clone)]
struct Outgoing {
    request_id: String,
    target: SocketAddr,
    from: Option<IpAddr>,
    code: String,
    mode: String,
    state: String,
    expires_at: i64,
    computer_id: Option<String>,
    label: Option<String>,
    message: Option<String>,
}

impl Outgoing {
    fn data(&self) -> Value {
        let mut data = json!({"request_id": self.request_id, "state": self.state, "code": self.code, "mode": self.mode});
        for (key, value) in [("computer_id", &self.computer_id), ("label", &self.label), ("message", &self.message)] {
            if let Some(value) = value {
                data[key] = json!(value);
            }
        }
        data
    }
}

/// The requests this console started.
#[derive(Default)]
pub struct Pairs(Mutex<HashMap<String, Outgoing>>);

impl Pairs {
    fn get(&self, id: &str) -> Option<Outgoing> {
        self.0.lock().ok()?.get(id).cloned()
    }
    fn put(&self, request: Outgoing) {
        let Ok(mut map) = self.0.lock() else { return };
        if map.len() >= KEEP_REQUESTS {
            let oldest = map.values().filter(|r| r.state != "waiting").min_by_key(|r| r.expires_at).map(|r| r.request_id.clone());
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(request.request_id.clone(), request);
    }
    fn update(&self, id: &str, change: impl FnOnce(&mut Outgoing)) {
        if let Ok(mut map) = self.0.lock()
            && let Some(request) = map.get_mut(id)
        {
            change(request);
        }
    }
}

fn tailscale_failure(ctx: &Ctx, error: &CliError) -> Value {
    match error {
        CliError::NotInstalled => ctx.failure("TAILSCALE_MISSING", "Tailscale isn't installed on this computer.", "failed", true),
        _ => ctx.failure("TAILSCALE_STOPPED", "Tailscale isn't running on this computer. Start it, then try again.", "failed", true),
    }
}

// ---------------------------------------------------------------------------
// tailnet

/// `tailnet`: this computer first, then its Linux peers by name.
pub async fn tailnet(ctx: &Ctx) -> Handled {
    if !ctx.args.is_empty() {
        return Err(Fault::plain("tailnet takes no arguments."));
    }
    let unavailable = |state: &str| json!({"state": state, "login": null, "self_node": null, "login_url": null});
    let status = match tailnet::status().await {
        Ok(status) => status,
        Err(CliError::NotInstalled) => return Ok(ctx.ready(json!({"tailscale": unavailable("not_installed"), "computers": []}))),
        Err(_) => return Ok(ctx.ready(json!({"tailscale": unavailable("stopped"), "computers": []}))),
    };
    let state = match status.backend.as_str() {
        "Running" => "running",
        "NeedsLogin" | "NeedsMachineAuth" => "logged_out",
        _ => "stopped",
    };
    let tailscale = json!({
        "state": state, "login": status.login(), "self_node": status.own.as_ref().map(|o| o.node.clone()),
        "login_url": if state == "logged_out" { json!(status.auth_url) } else { Value::Null },
    });
    if state != "running" {
        return Ok(ctx.ready(json!({"tailscale": tailscale, "computers": []})));
    }
    // Every verified computer here, with the host keys pinned for it.
    let rows: Vec<(ComputerRecord, Vec<String>)> = OperatorDirectory::open(&ctx.console.database)
        .and_then(|d| {
            let listed = d.list_computers()?;
            Ok(listed.iter().filter_map(|r| d.get_computer(&r.computer_id).ok().flatten()).filter(|r| r.trust_state == "verified").collect::<Vec<_>>())
        })
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let pins = pinned_keys(&r.known_hosts_file_ref);
            (r, pins)
        })
        .collect();
    let own = status.own.clone();
    let from = own.as_ref().and_then(Peer::address);
    let mut peers: Vec<&Peer> = status.peers.iter().filter(|p| p.os == "linux").collect();
    peers.sort_by(|a, b| a.node.cmp(&b.node));
    let listed: Vec<(&Peer, bool)> = own.iter().map(|o| (o, true)).chain(peers.into_iter().map(|p| (p, false))).collect();
    // Probe every computer at once; each answers within the probe timeout. An
    // added computer that answers also shows its SSH host key, so one that was
    // reinstalled (a new key) can be told from one this console can reach.
    let probes: Vec<_> = listed
        .iter()
        .map(|(peer, is_self)| {
            let (peer, is_self) = ((*peer).clone(), *is_self);
            let port = rows.iter().find(|(r, _)| peer.named(&r.host)).map(|(r, _)| r.port);
            tokio::spawn(async move {
                let ibara = probe(&peer, is_self, from).await;
                let key = match (ibara, port, peer.address()) {
                    ("ready", Some(port), Some(address)) => host_key_at(address, port).await,
                    _ => None,
                };
                (ibara, key)
            })
        })
        .collect();
    let mut answers = Vec::with_capacity(probes.len());
    for probe in probes {
        answers.push(probe.await.unwrap_or(("unknown", None)));
    }
    let computers: Vec<Value> = listed
        .iter()
        .zip(answers)
        .map(|((peer, is_self), (ibara, key))| {
            // Of the rows naming this computer, the one its key is pinned for.
            let named: Vec<&(ComputerRecord, Vec<String>)> = rows.iter().filter(|(r, _)| peer.named(&r.host)).collect();
            let pinned = |(_, pins): &(ComputerRecord, Vec<String>)| key.as_ref().is_some_and(|k| pins.contains(k));
            let row = named.iter().copied().find(|r| pinned(r)).or(named.first().copied());
            let changed = row.is_some_and(|r| key.is_some() && !pinned(r));
            let row = row.map(|(r, _)| r);
            let same_owner = *is_self
                || own.as_ref().is_some_and(|o| tailnet::same_owner((o.user_id, &o.tags), (peer.user_id, &peer.tags)));
            json!({
                "node": peer.node, "dns_name": peer.dns_name, "ip": peer.address().map(|a| a.to_string()),
                "online": *is_self || peer.online, "owner": status.owner(peer), "same_owner": same_owner, "is_self": is_self,
                "ibara": ibara, "paired": row.is_some(), "changed": changed, "computer_id": row.map(|r| r.computer_id.clone()),
                "label": row.map(|r| r.label.clone()),
            })
        })
        .collect();
    Ok(ctx.ready(json!({"tailscale": tailscale, "computers": computers})))
}

/// The Ed25519 host key `address` shows on `port`, if it answers in time.
async fn host_key_at(address: IpAddr, port: i64) -> Option<String> {
    let scan = tokio::process::Command::new("ssh-keyscan")
        .args(["-T", "2", "-t", "ed25519", "-p", &port.to_string(), &address.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(3), scan).await.ok()?.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
        [_, "ssh-ed25519", blob, ..] => Some(blob.to_string()),
        _ => None,
    })
}

/// The Ed25519 host keys pinned in a row's known-hosts file.
fn pinned_keys(known_hosts_ref: &str) -> Vec<String> {
    let text = known_hosts_ref.strip_prefix("file:").and_then(|path| std::fs::read_to_string(path).ok()).unwrap_or_default();
    text.lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            [_, "ssh-ed25519", blob, ..] => Some(blob.to_string()),
            _ => None,
        })
        .collect()
}

/// `ready` when ibara answers on the pairing port, `not_installed` when the
/// computer refuses the connection, `offline` or `unknown` otherwise.
async fn probe(peer: &Peer, is_self: bool, from: Option<IpAddr>) -> &'static str {
    if !is_self && !peer.online {
        return "offline";
    }
    let Some(address) = peer.address() else { return "unknown" };
    let request = json!({"op": "probe"});
    match tailnet::exchange(SocketAddr::new(address, tailnet::pairing_port()), from, &request, PROBE_TIMEOUT).await {
        Ok(reply) if reply.get("ibara").and_then(Value::as_str) == Some("ready") => "ready",
        Ok(_) => "unknown",
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => "not_installed",
        Err(_) => "unknown",
    }
}

// ---------------------------------------------------------------------------
// This computer's key and identity.

/// `~/.ssh/ibara_agent_ed25519`: this desktop user's ibara key, mode 0600.
fn operator_key_path() -> PathBuf {
    home_dir().join(".ssh/ibara_agent_ed25519")
}

/// [`operator_key_path`], created without a passphrase when missing.
pub(crate) async fn operator_key() -> Result<KeyLine, String> {
    let key = operator_key_path();
    let ssh = home_dir().join(".ssh");
    if std::fs::symlink_metadata(&key).is_err() {
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&ssh).map_err(|e| format!("Could not create {}: {e}", ssh.display()))?;
        let comment = format!("ibara-{}", std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim());
        let made = tokio::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(Duration::from_secs(20), made).await {
            Ok(Ok(out)) if out.status.success() => {}
            _ => return Err("This computer could not create its ibara key with ssh-keygen.".into()),
        }
    }
    let meta = std::fs::symlink_metadata(&key).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != current_uid() || meta.mode() & 0o077 != 0 {
        return Err(format!("{} must be a private file that belongs to you.", key.display()));
    }
    let public = tokio::process::Command::new("ssh-keygen").args(["-y", "-P", "", "-f"]).arg(&key).stdin(std::process::Stdio::null()).output();
    let public = match tokio::time::timeout(Duration::from_secs(10), public).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => return Err(format!("ibara can't read {} (it may have a passphrase).", key.display())),
    };
    ed25519_line(&public).ok_or_else(|| format!("{} is not an Ed25519 key.", key.display()))
}

/// `host_` and 32 hex digits of sha256(machine-id:uid): this operator environment.
fn local_endpoint_id() -> String {
    let machine = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let digest = Sha256::digest(format!("{}:{}", machine.trim(), current_uid()).as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("host_{hex}")
}

/// The `pair` request for the computer at `address`, signed with this desktop
/// user's key. For another computer, this computer's own ibara first registers
/// it over the desktop user's socket, so it can vouch that this console asked;
/// without that the other computer waits for a person, even for an own computer.
async fn signed_request(key: &KeyLine, address: IpAddr, is_self: bool, invite: Option<&str>) -> Result<Value, String> {
    use crate::server::pairing::{SIGN_NAMESPACE, ask_local, statement};
    let endpoint = local_endpoint_id();
    let nonce = match is_self {
        true => None,
        false => {
            let register = json!({"op": "outgoing", "fingerprint": key.fingerprint, "target": address.to_string()});
            ask_local(&register).await.ok().and_then(|reply| reply.get("nonce").and_then(Value::as_str).map(str::to_string))
        }
    };
    let signed_at = crate::ids::now_millis();
    let signed = statement(address, &endpoint, nonce.as_deref(), signed_at, invite);
    let signature = crate::sshkey::sign(&operator_key_path(), SIGN_NAMESPACE, signed.as_bytes())
        .await
        .ok_or("This computer could not sign its request with its ibara key.")?;
    let mut request = json!({"op": "pair", "public_key": key.line, "endpoint_id": endpoint, "signed_at": signed_at, "signature": signature});
    if let Some(nonce) = nonce {
        request["nonce"] = json!(nonce);
    }
    if let Some(invite) = invite {
        request["invite"] = json!(invite);
    }
    Ok(request)
}

// ---------------------------------------------------------------------------
// pair-start, pair-status, pair-cancel

fn one_arg<'a>(ctx: &'a Ctx, usage: &str) -> Result<&'a str, Fault> {
    match ctx.args.as_slice() {
        [one] if !one.is_empty() => Ok(one.as_str()),
        _ => Err(Fault::plain(usage)),
    }
}

/// The computer `name` names in `status`, and whether it is this computer.
fn find<'a>(status: &'a Status, name: &str) -> Option<(&'a Peer, bool)> {
    let own = status.own.as_ref().filter(|o| o.named(name)).map(|o| (o, true));
    own.or_else(|| status.peers.iter().find(|p| p.named(name)).map(|p| (p, false)))
}

/// `pair-start NODE [INVITE_CODE]`: with a code, someone else's computer that
/// shared itself with this one adds it at once.
pub async fn pair_start(ctx: &Ctx) -> Handled {
    const USAGE: &str = "Usage: pair-start COMPUTER [INVITE_CODE]";
    let (name, invite) = match ctx.args.as_slice() {
        [name] if !name.is_empty() => (name.as_str(), None),
        [name, code] if !name.is_empty() => match crate::server::invites::normalize(code) {
            Some(code) => (name.as_str(), Some(code)),
            None => {
                let message = "An invite code has 8 letters and numbers, like 4H7K-92QX. Check it, then try again.";
                return Ok(ctx.failure("INVITE_REFUSED", message, "failed", false));
            }
        },
        _ => return Err(Fault::plain(USAGE)),
    };
    let status = match tailnet::status().await {
        Ok(status) if status.running() => status,
        Ok(_) => return Ok(tailscale_failure(ctx, &CliError::Invalid)),
        Err(error) => return Ok(tailscale_failure(ctx, &error)),
    };
    let Some((peer, is_self)) = find(&status, name) else {
        return Ok(ctx.failure("UNKNOWN_COMPUTER", &format!("Tailscale doesn't list a computer called {name}."), "failed", true));
    };
    if !is_self && !peer.online {
        return Ok(ctx.failure("COMPUTER_OFFLINE", &format!("{} is offline. Turn it on, then try again.", peer.node), "offline", true));
    }
    if peer.os != "linux" {
        return Ok(ctx.failure("UNSUPPORTED_COMPUTER", &format!("{} isn't a Linux computer, so it can't run ibara.", peer.node), "failed", false));
    }
    let Some(address) = peer.address() else {
        return Ok(ctx.failure("UNKNOWN_COMPUTER", &format!("Tailscale lists no address for {}.", peer.node), "failed", true));
    };
    let key = match operator_key().await {
        Ok(key) => key,
        Err(message) => return Ok(ctx.failure("KEY_UNAVAILABLE", &message, "failed", false)),
    };
    let target = SocketAddr::new(address, tailnet::pairing_port());
    let from = status.own.as_ref().and_then(Peer::address);
    let request = match signed_request(&key, address, is_self, invite.as_deref()).await {
        Ok(request) => request,
        Err(message) => return Ok(ctx.failure("KEY_UNAVAILABLE", &message, "failed", false)),
    };
    // This computer is added over its desktop user's own socket: the pairing port
    // refuses requests from this computer's own address.
    let asked = if is_self {
        crate::server::pairing::ask_local(&request).await
    } else {
        tailnet::exchange(target, from, &request, PAIR_TIMEOUT).await
    };
    let reply = match asked {
        Ok(reply) => reply,
        Err(_) if is_self => {
            let message = "ibara isn't running on this computer yet. Start it, then try again.".to_string();
            return Ok(ctx.failure("IBARA_NOT_INSTALLED", &message, "failed", true));
        }
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            let message = format!("{} doesn't have ibara, or has an older ibara. Install or update ibara on it, then try again.", peer.node);
            return Ok(ctx.failure("IBARA_NOT_INSTALLED", &message, "failed", true));
        }
        Err(_) => {
            let message = format!("{} isn't answering. Check that ibara is running on it, then try again.", peer.node);
            return Ok(ctx.failure("PEER_UNAVAILABLE", &message, "offline", true));
        }
    };
    if reply.get("ok") != Some(&json!(true)) {
        let error = reply.get("error");
        let code = error.and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("PAIRING_REFUSED");
        let message = error.and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("The other computer refused the request.");
        return Ok(ctx.failure(code, message, "failed", true));
    }
    let text = |key: &str| reply.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let outgoing = Outgoing {
        request_id: text("request_id"),
        target,
        from,
        code: text("code"),
        mode: text("mode"),
        state: "waiting".into(),
        expires_at: reply.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
        computer_id: None,
        label: None,
        message: None,
    };
    if !pattern::id(&outgoing.request_id) {
        return Ok(ctx.failure("INVALID_RESPONSE", "The other computer answered with an invalid request.", "failed", true));
    }
    let id = outgoing.request_id.clone();
    ctx.console.pairs.put(outgoing);
    // Settled already (the same person's computer, or a key it already knows): finish now.
    settle(&ctx.console, &id, target.ip(), &reply).await;
    if ctx.console.pairs.get(&id).is_some_and(|r| r.state == "waiting") {
        tokio::spawn(follow(ctx.console.clone(), id.clone()));
    }
    let data = ctx.console.pairs.get(&id).map(|r| r.data()).unwrap_or(Value::Null);
    Ok(ctx.ready(data))
}

/// Apply one reply from the other computer to a request still waiting here.
async fn settle(console: &Arc<Console>, id: &str, host: IpAddr, reply: &Value) {
    let state = reply.get("state").and_then(Value::as_str).unwrap_or("");
    match state {
        "waiting" | "" => {}
        "paired" => {
            let route = reply.get("route").cloned().unwrap_or(Value::Null);
            let outcome = finish(console, host, &route).await;
            console.pairs.update(id, |r| match outcome {
                Ok((computer_id, label)) => {
                    r.state = "paired".into();
                    r.computer_id = Some(computer_id);
                    r.label = Some(label);
                }
                Err(message) => {
                    r.state = "failed".into();
                    r.message = Some(message);
                }
            });
        }
        other => {
            let message = reply.get("message").and_then(Value::as_str).map(str::to_string);
            let state = if matches!(other, "declined" | "expired" | "canceled") { other } else { "failed" };
            console.pairs.update(id, |r| {
                r.state = state.into();
                r.message = message.or_else(|| (state == "failed").then(|| "The other computer could not add this one.".into()));
            });
        }
    }
}

/// Follow a waiting request until the other computer settles it.
async fn follow(console: Arc<Console>, id: String) {
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let Some(request) = console.pairs.get(&id).filter(|r| r.state == "waiting") else { return };
        let asked = tailnet::exchange(request.target, request.from, &json!({"op": "status", "request_id": id}), STATUS_TIMEOUT).await;
        match asked {
            Ok(reply) if reply.get("ok") == Some(&json!(true)) => settle(&console, &id, request.target.ip(), &reply).await,
            Ok(_) => console.pairs.update(&id, |r| {
                r.state = "failed".into();
                r.message = Some("The other computer forgot this request (it may have restarted). Try again.".into());
            }),
            // Unreachable for now: keep asking until the request would have expired.
            Err(_) if crate::ids::now_millis() > request.expires_at + 15_000 => console.pairs.update(&id, |r| r.state = "expired".into()),
            Err(_) => {}
        }
    }
}

/// Whether a directory row's host (an address, or the computer's Tailscale
/// name) is the computer at `address`, which Tailscale calls `node`.
fn answered_by(row_host: &str, address: IpAddr, node: Option<&str>) -> bool {
    if row_host.parse::<IpAddr>().ok().map(|ip| ip.to_canonical()) == Some(address.to_canonical()) {
        return true;
    }
    let first = row_host.split('.').next().unwrap_or("").to_ascii_lowercase();
    node.is_some_and(|node| !first.is_empty() && first == node.to_ascii_lowercase())
}

/// What to reach the computer at `address` by: its Tailscale name when this
/// computer resolves that name to that address (MagicDNS), else the address.
async fn route_host(address: IpAddr, node: Option<&str>) -> String {
    if let Some(node) = node.filter(|n| pattern::node(n)) {
        let resolved = tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((node, 0))).await;
        if let Ok(Ok(mut found)) = resolved
            && found.any(|a| a.ip().to_canonical() == address.to_canonical())
        {
            return node.to_string();
        }
    }
    address.to_string()
}

/// Pin the other computer's SSH host key, write its verified directory row
/// and prove the route answers. Returns the computer id and its name here.
async fn finish(console: &Arc<Console>, host: IpAddr, route: &Value) -> Result<(String, String), String> {
    let text = |key: &str| route.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let (principal, endpoint, label) = (text("principal"), text("endpoint_id"), text("label"));
    let host_key = ed25519_line(&text("host_key"));
    let port = route.get("port").and_then(Value::as_u64).filter(|p| (1..=65535).contains(p));
    let generation = route.get("authorization_generation").and_then(Value::as_u64);
    let (Some(host_key), Some(port), Some(generation)) = (host_key, port, generation) else {
        return Err("The other computer's answer was incomplete. Try again.".into());
    };
    if !pattern::principal(&principal) || !pattern::endpoint_id(&endpoint) || label.trim().is_empty() {
        return Err("The other computer's answer was incomplete. Try again.".into());
    }
    let node = tailnet::whois(host).await.ok().map(|w| w.node);
    let node = node.as_deref();
    let unwritable = |e: String| format!("This computer could not save the new computer: {e}");
    // The row this computer gets, and the name it keeps when it takes over a row.
    let (computer_id, kept_label) = {
        let rows = OperatorDirectory::open(&console.database).and_then(|d| d.list_computers()).map_err(|e| unwritable(e.message))?;
        let mut verified = rows.iter().filter(|r| r.trust_state == "verified");
        match verified.clone().find(|r| r.endpoint_id == endpoint) {
            // A computer already here keeps its row only when that row points at the
            // computer that answered: an answer naming another computer's endpoint
            // must not repoint it.
            Some(row) if answered_by(&row.host, host, node) => (row.computer_id.clone(), None),
            Some(_) => return Err("That computer answered as another computer already added here. Nothing was changed.".into()),
            // Added here before and now answering as a new computer: it was
            // reinstalled. Having passed the same checks as a first add, it
            // takes over its old row and the name it has here, so the fleet
            // keeps one card, under one name, for it.
            None => match verified.find(|r| answered_by(&r.host, host, node)) {
                Some(row) => (row.computer_id.clone(), Some(row.label.clone())),
                None => (format!("computer_{}", Sha256::digest(endpoint.as_bytes()).iter().take(12).map(|b| format!("{b:02x}")).collect::<String>()), None),
            },
        }
    };
    // A name another computer here already has would make two cards read the
    // same: add its Tailscale name, which it cannot choose itself.
    let label = label.trim();
    let taken = OperatorDirectory::open(&console.database).and_then(|d| d.label_taken(&computer_id, label)).unwrap_or(false);
    let label = if let Some(kept) = kept_label {
        kept
    } else if taken {
        let node = node.map(str::to_string).unwrap_or_else(|| host.to_string());
        let room = crate::operator::directory::LABEL_LIMIT.saturating_sub(node.encode_utf16().count() + 3);
        format!("{} ({node})", crate::operator::js::slice_units(label, room).trim_end())
    } else {
        label.to_string()
    };
    let route_host = route_host(host, node).await;
    let known_hosts = pin_host(&computer_id, &endpoint, &host_key).map_err(|e| unwritable(e.to_string()))?;
    let identity = operator_key_path();
    let mut directory = OperatorDirectory::open(&console.database).map_err(|e| unwritable(e.message))?;
    let current = directory.get_computer(&computer_id).map_err(|e| unwritable(e.message))?;
    let mut record: Map<String, Value> = Map::new();
    for (key, value) in [
        ("computer_id", json!(computer_id)),
        ("endpoint_id", json!(endpoint)),
        ("label", json!(label)),
        ("transport", json!("ssh")),
        ("host", json!(route_host)),
        ("user", json!(principal)),
        ("port", json!(port)),
        ("identity_file_ref", json!(format!("file:{}", identity.display()))),
        ("known_hosts_file_ref", json!(format!("file:{}", known_hosts.display()))),
        ("trust_state", json!("verified")),
        ("authorization_generation", json!(generation)),
    ] {
        record.insert(key.into(), value);
    }
    let expected = current.as_ref().map(|c| c.binding_revision).unwrap_or(0);
    let changed = current.as_ref().is_none_or(|c| {
        c.endpoint_id != endpoint
            || c.host != route_host
            || c.user != principal
            || c.port != port as i64
            || c.identity_file_ref != format!("file:{}", identity.display())
            || c.known_hosts_file_ref != format!("file:{}", known_hosts.display())
    });
    record.insert("binding_revision".into(), json!(if current.is_some() { expected + i64::from(changed) } else { 1 }));
    let stored = directory.register_verified_computer(&Value::Object(record), &endpoint, expected).map_err(|e| unwritable(e.message))?;
    directory.close();
    if let Some(previous) = &current {
        if previous.endpoint_id != endpoint {
            // Nothing this console kept for the computer it was applies to it now.
            console.forget_computer(&computer_id);
        }
        if previous.known_hosts_file_ref != stored.known_hosts_file_ref {
            unpin_host(&console.database, &previous.known_hosts_file_ref);
        }
    }
    // The other computer projects the new account and opens its socket within
    // seconds; a route that never answers is not kept as a new computer.
    let deadline = tokio::time::Instant::now() + VERIFY_FOR;
    loop {
        match console.sessions.call(&computer_id, None, "session", Value::Null).await {
            Ok(_) => return Ok((computer_id, stored.label)),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                eprintln!("{}", json!({"event": "pairing_route_unverified", "computer_id": computer_id, "error": error.message}));
                if current.is_none()
                    && let Ok(mut directory) = OperatorDirectory::open(&console.database)
                {
                    let _ = directory.forget_new_computer(&computer_id);
                }
                return Err(format!("{label} accepted, but this computer can't reach it yet. Make sure ibara is up to date on {label}, then try again."));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Where this console pins the host keys of the computers it adds.
fn known_hosts_dir() -> PathBuf {
    crate::operator::directory::default_operator_directory_path().with_file_name("known-hosts")
}

/// `~/.local/state/ibara/known-hosts/<computer>.known_hosts`: `<endpoint> ssh-ed25519 <blob>`, 0600.
fn pin_host(computer_id: &str, endpoint: &str, host_key: &KeyLine) -> std::io::Result<PathBuf> {
    let dir = known_hosts_dir();
    std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let file = dir.join(format!("{computer_id}.known_hosts"));
    let temp = dir.join(format!(".{computer_id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    std::io::Write::write_all(&mut out, format!("{endpoint} ssh-ed25519 {}\n", host_key.blob).as_bytes())?;
    std::fs::rename(&temp, &file)?;
    Ok(file)
}

/// Delete a host key pin this console wrote once no computer here uses it,
/// never a file anywhere else.
fn unpin_host(database: &Path, known_hosts_ref: &str) {
    let Some(path) = known_hosts_ref.strip_prefix("file:").map(Path::new) else { return };
    if path.parent() != Some(known_hosts_dir().as_path()) {
        return;
    }
    let in_use = OperatorDirectory::open(database).and_then(|d| {
        let listed = d.list_computers()?;
        Ok(listed.iter().any(|r| d.get_computer(&r.computer_id).ok().flatten().is_some_and(|c| c.known_hosts_file_ref == known_hosts_ref)))
    });
    if matches!(in_use, Ok(false)) {
        let _ = std::fs::remove_file(path);
    }
}

/// `remove-computer --computer NAME`: take a computer out of this console's
/// fleet: its directory row, its pinned host key and what this console holds
/// for it. The computer itself is not asked; adding it again pairs it as a new
/// computer, with the same checks as the first time.
pub fn remove_computer(ctx: &Ctx) -> Handled {
    let computer = match ctx.args.as_slice() {
        [flag, id] if flag == "--computer" => validated_id(Some(id), "computer_id")?,
        _ => return Err(Fault::plain("Usage: remove-computer --computer NAME")),
    };
    let removed = match OperatorDirectory::open(&ctx.console.database).and_then(|mut d| d.remove_computer(&computer)) {
        Ok(removed) => removed,
        Err(error) => return Ok(ctx.failure("REMOVE_REFUSED", &error.message, "failed", false)),
    };
    unpin_host(&ctx.console.database, &removed.known_hosts_file_ref);
    ctx.console.forget_computer(&computer);
    Ok(ctx.ready(json!({"removed": {"computer_id": removed.computer_id, "label": removed.label}})))
}

/// `pair-status ID`.
pub async fn pair_status(ctx: &Ctx) -> Handled {
    let id = one_arg(ctx, "Usage: pair-status REQUEST_ID")?;
    match ctx.console.pairs.get(id) {
        Some(request) => Ok(ctx.ready(request.data())),
        None => Ok(ctx.failure("PAIRING_UNKNOWN", "No pairing request has that ID.", "failed", false)),
    }
}

/// `pair-cancel ID`: the other computer drops the request too.
pub async fn pair_cancel(ctx: &Ctx) -> Handled {
    let id = one_arg(ctx, "Usage: pair-cancel REQUEST_ID")?;
    let Some(request) = ctx.console.pairs.get(id) else {
        return Ok(ctx.failure("PAIRING_UNKNOWN", "No pairing request has that ID.", "failed", false));
    };
    if request.state == "waiting" {
        let asked = tailnet::exchange(request.target, request.from, &json!({"op": "cancel", "request_id": id}), STATUS_TIMEOUT).await;
        match asked {
            // Settled before the cancel arrived: the follower records it.
            Ok(reply) if matches!(reply.get("state").and_then(Value::as_str), Some("paired" | "declined" | "expired")) => {
                settle(&ctx.console, id, request.target.ip(), &reply).await;
            }
            Ok(reply) if reply.get("state").and_then(Value::as_str) == Some("canceled") => {
                ctx.console.pairs.update(id, |r| r.state = "canceled".into());
            }
            // Still being accepted there, or no answer: nothing is known to have
            // stopped, so the request keeps being followed until it settles.
            _ => {}
        }
    }
    let state = ctx.console.pairs.get(id).map(|r| r.state).unwrap_or_default();
    Ok(ctx.ready(json!({"request_id": id, "state": state})))
}

// ---------------------------------------------------------------------------
// pair-requests, pair-answer (this computer being added)

fn not_hosting(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
}

fn local_failure(ctx: &Ctx, reply: &Value) -> Value {
    let error = reply.get("error");
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("PAIRING_REFUSED");
    let message = error.and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("ibara on this computer refused the answer.");
    ctx.failure(code, message, "failed", false)
}

/// `pair-requests`: computers waiting for a person here to add them.
pub async fn pair_requests(ctx: &Ctx) -> Handled {
    if !ctx.args.is_empty() {
        return Err(Fault::plain("pair-requests takes no arguments."));
    }
    match crate::server::pairing::ask_local(&json!({"op": "requests"})).await {
        Ok(reply) if reply.get("ok") == Some(&json!(true)) => Ok(ctx.ready(json!({"requests": reply["requests"]}))),
        Ok(reply) => Ok(local_failure(ctx, &reply)),
        // This computer does not host ibara, so nobody can ask to add it.
        Err(error) if not_hosting(&error) => Ok(ctx.ready(json!({"requests": []}))),
        Err(_) => Ok(ctx.failure("PAIRING_UNAVAILABLE", "ibara on this computer isn't answering.", "failed", true)),
    }
}

/// `pair-answer ID accept|decline`.
pub async fn pair_answer(ctx: &Ctx) -> Handled {
    let [id, answer] = ctx.args.as_slice() else { return Err(Fault::plain("Usage: pair-answer REQUEST_ID accept|decline")) };
    if !pattern::id(id) || !matches!(answer.as_str(), "accept" | "decline") {
        return Err(Fault::plain("Usage: pair-answer REQUEST_ID accept|decline"));
    }
    match crate::server::pairing::ask_local(&json!({"op": "answer", "request_id": id, "answer": answer})).await {
        Ok(reply) if reply.get("ok") == Some(&json!(true)) => Ok(ctx.ready(json!({"request_id": id, "state": reply["state"]}))),
        Ok(reply) => Ok(local_failure(ctx, &reply)),
        Err(error) if not_hosting(&error) => {
            Ok(ctx.failure("NOT_HOSTING", "This computer isn't set up to be added to other computers yet.", "failed", false))
        }
        Err(_) => Ok(ctx.failure("PAIRING_UNAVAILABLE", "ibara on this computer isn't answering.", "failed", true)),
    }
}

// ---------------------------------------------------------------------------
// invite-create, invites, invite-revoke (sharing this computer with a friend)

/// One request to this computer's own ibara: its ok reply, or the failure envelope.
async fn ask_this_computer(ctx: &Ctx, request: &Value) -> Result<Value, Value> {
    match crate::server::pairing::ask_local(request).await {
        Ok(reply) if reply.get("ok") == Some(&json!(true)) => Ok(reply),
        Ok(reply) => Err(local_failure(ctx, &reply)),
        Err(error) if not_hosting(&error) => {
            Err(ctx.failure("NOT_HOSTING", "ibara isn't running on this computer, so it can't be shared yet.", "failed", false))
        }
        Err(_) => Err(ctx.failure("PAIRING_UNAVAILABLE", "ibara on this computer isn't answering.", "failed", true)),
    }
}

/// `invite-create LEVEL LASTS`: a single-use code for a friend. LEVEL is
/// `watch`, `use_with_approval` or `take_control`; LASTS is `hour`, `day`,
/// `week` or `never` (until revoked). The code is shown this once.
pub async fn invite_create(ctx: &Ctx) -> Handled {
    let [level, lasts] = ctx.args.as_slice() else {
        return Err(Fault::plain("Usage: invite-create watch|use_with_approval|take_control hour|day|week|never"));
    };
    match ask_this_computer(ctx, &json!({"op": "invite", "level": level, "lasts": lasts})).await {
        Ok(reply) => Ok(ctx.ready(json!({"invite": reply["invite"]}))),
        Err(failure) => Ok(failure),
    }
}

/// `invites`: this computer's invites, newest first, and the page in the
/// Tailscale admin console where its owner shares it (`tailscale` is null
/// while Tailscale isn't running here).
pub async fn invites(ctx: &Ctx) -> Handled {
    if !ctx.args.is_empty() {
        return Err(Fault::plain("invites takes no arguments."));
    }
    let reply = match ask_this_computer(ctx, &json!({"op": "invites"})).await {
        Ok(reply) => reply,
        Err(failure) => return Ok(failure),
    };
    let own = tailnet::status().await.ok().filter(Status::running).and_then(|s| s.own);
    let address = own.as_ref().and_then(|o| o.ips.iter().find(|ip| ip.is_ipv4()).copied());
    let tailscale = address.map(|ip| json!({"address": ip.to_string(), "share_url": format!("https://login.tailscale.com/admin/machines/{ip}")}));
    Ok(ctx.ready(json!({"invites": reply["invites"], "tailscale": tailscale})))
}

/// `invite-revoke ID`: an unused invite is deleted; a used one also ends that
/// friend's pairing at once (`ended`).
pub async fn invite_revoke(ctx: &Ctx) -> Handled {
    let id = one_arg(ctx, "Usage: invite-revoke INVITE_ID")?;
    if !pattern::id(id) {
        return Err(Fault::plain("Usage: invite-revoke INVITE_ID"));
    }
    match ask_this_computer(ctx, &json!({"op": "invite_revoke", "id": id})).await {
        Ok(reply) => Ok(ctx.ready(json!({"id": id, "ended": reply["ended"]}))),
        Err(failure) => Ok(failure),
    }
}
