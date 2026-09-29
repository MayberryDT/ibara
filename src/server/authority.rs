//! Operator authority: `operator-authority.json`, operator bearers, pairing
//! (enrol, target-local confirmation, activation, revocation) and the
//! peer-route gate (`server.ts:39-131,235-254`).
//!
//! The file is kept as JSON objects, not typed records, so unknown keys and
//! key order survive every rewrite byte-for-byte like the TypeScript spread
//! updates did.

use super::policy::{ct_eq, sha256, sha256_hex, unhex};
use super::{Engine, Paths, Server, create_exclusive, peer};
use crate::access::PairRights;
use crate::error::{IbaraError, Result};
use crate::ids::{iso_from_millis, millis_from_iso, now_iso, now_millis};
use serde_json::{Map, Value, json};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const PAIRING_WINDOW_MS: i64 = 5 * 60_000;

fn invalid(message: &str) -> IbaraError {
    IbaraError::new("INVALID_ARGUMENT", message, true)
}

fn denied(message: &str) -> IbaraError {
    IbaraError::new("PERMISSION_DENIED", message, true)
}

/// A failure that is not one of ibara's own errors (a corrupt file, a failed
/// write). It projects to the generic, sanitised `INTERNAL_ERROR`.
pub fn unknown_failure() -> IbaraError {
    IbaraError::new("INTERNAL_ERROR", "Controller operation failed. Consult sanitized operator diagnostics.", false)
        .requires_reconciliation()
}

/// Read `operator-authority.json`; a missing file is no authority at all.
pub fn load(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(unknown_failure()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(_) => Err(unknown_failure()),
    }
}

/// Atomic rewrite through `<file>.<pid>.tmp`, mode 0600.
pub fn save(path: &Path, value: &Map<String, Value>) -> Result<()> {
    let temp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let text = serde_json::to_vec(value).map_err(|_| unknown_failure())?;
    create_exclusive(&temp, &text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// `{...policy.json operator_grants (re-read), ...operator-authority.json}`
/// for the controller's per-call authorisation. Any unreadable file yields no
/// grants, which denies every operator (fail closed).
pub fn operator_grants_source(paths: &Paths) -> Rc<dyn Fn() -> Map<String, Value>> {
    let policy = paths.policy.clone();
    let authority = paths.authority();
    Rc::new(move || {
        let (Some(mut grants), Ok(records)) = (super::policy::policy_operator_grants(&policy), load(&authority)) else {
            return Map::new();
        };
        for (id, record) in records {
            grants.insert(id, record);
        }
        grants
    })
}

pub fn is_hex_lower(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn charset(s: &str, min: usize, max: usize, extra: &[u8]) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// `^[a-z][a-z0-9_-]{0,63}$`, except `owner` (the local owner, see
/// [`crate::access::OWNER`]): a stable operator or agent principal.
pub fn valid_principal(s: &str) -> bool {
    peer::lower_ident(s, 64) && s != crate::access::OWNER
}

/// `^pair_[a-f0-9]{32}$`.
pub fn valid_challenge_ref(s: &str) -> bool {
    s.strip_prefix("pair_").is_some_and(|rest| is_hex_lower(rest, 32))
}

/// `^[A-Za-z0-9_.:-]{8,128}$`.
fn valid_endpoint(s: &str) -> bool {
    charset(s, 8, 128, b"_.:-")
}

/// The reviewed identity a pairing is bound to (`pairingBinding`).
fn pairing_binding(action: &Value) -> Result<Map<String, Value>> {
    let field = |key: &str| action.get(key).and_then(Value::as_str);
    match (field("review_digest"), field("operator_endpoint_id"), field("target_endpoint_id"), field("operator_key_fingerprint")) {
        (Some(review), Some(operator), Some(target), Some(fingerprint))
            if is_hex_lower(review, 64)
                && valid_endpoint(operator)
                && valid_endpoint(target)
                && charset(fingerprint, 16, 128, b"+/_=.:-") =>
        {
            let mut binding = Map::new();
            binding.insert("review_digest".into(), review.into());
            binding.insert("operator_endpoint_id".into(), operator.into());
            binding.insert("target_endpoint_id".into(), target.into());
            binding.insert("operator_key_fingerprint".into(), fingerprint.into());
            Ok(binding)
        }
        _ => Err(invalid("Pairing requires reviewed identity, digest and key fingerprint.")),
    }
}

fn challenge_path(paths: &Paths, reference: &Value) -> Result<PathBuf> {
    match reference.as_str() {
        Some(r) if valid_challenge_ref(r) => Ok(paths.challenges().join(format!("{r}.json"))),
        _ => Err(invalid("Invalid challenge reference.")),
    }
}

fn remove_force(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// `Date.parse(x) <= Date.now()`: false for anything unparsable.
fn expired(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).and_then(millis_from_iso).is_some_and(|t| t <= now_millis())
}

/// Truthiness of an optional JSON value, as JavaScript sees it.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `a === b` for JSON values: numbers compare by value.
pub fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::Object(_) | Value::Array(_)), _) | (_, Some(Value::Object(_) | Value::Array(_))) => false,
        _ => a == b,
    }
}

fn generation_plus_one(record: Option<&Value>) -> Value {
    match record.and_then(|r| r.get("generation")) {
        Some(Value::Number(n)) if n.is_i64() => json!(n.as_i64().unwrap_or(0) + 1),
        Some(Value::Number(n)) => json!(n.as_f64().unwrap_or(f64::NAN) + 1.0),
        _ => Value::Null,
    }
}

/// `bytes` random bytes from the kernel, as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    let mut filled = 0;
    while filled < bytes {
        // SAFETY: the pointer and length describe the unfilled tail of `buf`.
        let n = unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), bytes - filled, 0) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        filled += n as usize;
    }
    Ok(crate::store::canonical::hex_encode(&buf))
}

/// The operator id whose bearer this is, over `controller.sock`: the legacy
/// `policy.operator_credentials` merged with `operator-authority.json`
/// digests (re-read per request), skipping blocked keys.
pub fn operator_for_bearer<E: Engine + 'static>(server: &Server<E>, bearer: &str) -> Option<String> {
    let authority = load(&server.paths.authority()).unwrap_or_default();
    let blocked = server.blocked.borrow();
    let digest_of = |id: &str| -> Option<String> {
        authority.get(id).filter(|_| !blocked.contains(id)).and_then(|r| r.get("digest")).map(|d| d.as_str().unwrap_or("").to_string())
    };
    let mut merged: Vec<(String, String)> = server
        .policy
        .operator_credentials()
        .iter()
        .map(|(id, digest)| (id.clone(), digest_of(id).unwrap_or_else(|| digest.clone())))
        .collect();
    for id in authority.keys() {
        if !merged.iter().any(|(known, _)| known == id)
            && let Some(digest) = digest_of(id)
        {
            merged.push((id.clone(), digest));
        }
    }
    let supplied = sha256(bearer.as_bytes());
    merged
        .into_iter()
        .find(|(_, digest)| digest.len() == 64 && unhex(digest).is_some_and(|d| ct_eq(&supplied, &d)))
        .map(|(id, _)| id)
}

/// May the peer socket for `operator_id` accept requests? (`peerRouteOpen`)
/// Open before pairing (no record), while a pairing waits for confirmation,
/// and once enabled; closed after revocation.
pub fn peer_route_open<E: Engine + 'static>(server: &Server<E>, operator_id: &str) -> bool {
    if server.blocked.borrow().contains(operator_id) {
        return false;
    }
    let Ok(authority) = load(&server.paths.authority()) else { return false };
    let Some(record) = authority.get(operator_id) else { return true };
    if truthy(record.get("enabled")) {
        return true;
    }
    let pending = record.get("pending").filter(|p| truthy(Some(p)));
    pending.is_some_and(|p| {
        !truthy(p.get("confirmed_at")) && p.get("expires_at").and_then(Value::as_str).and_then(millis_from_iso).is_some_and(|t| t > now_millis())
    })
}

/// Re-seal every stored operator key at start-up (`hardenStoredOperatorKeys`).
/// A key that cannot be kept private blocks that operator's bearer.
pub async fn harden_stored_operator_keys<E: Engine + 'static>(server: &Server<E>) {
    let Ok(entries) = std::fs::read_dir(server.paths.operator_keys()) else { return };
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in names {
        let Some(id) = name.strip_suffix(".key").filter(|id| valid_principal(id)) else { continue };
        let path = server.paths.operator_keys().join(&name);
        if peer::seal_operator_credential(&path, server.uid).await {
            server.blocked.borrow_mut().remove(id);
        } else {
            server.blocked.borrow_mut().insert(id.to_string());
            eprintln!("{}", json!({ "event": "operator_credential_not_private", "operator_id": id }));
        }
    }
}

/// A fresh operator bearer at `operator-keys/<id>.key`, kept private to the
/// controller. Returns the token and its path.
async fn issue_credential<E: Engine + 'static>(server: &Server<E>, operator_id: &str) -> Result<(String, PathBuf)> {
    let token = random_hex(32)?;
    let key_dir = server.paths.operator_keys();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&key_dir)?;
    let dir_meta = std::fs::symlink_metadata(&key_dir)?;
    if !dir_meta.file_type().is_dir() || dir_meta.uid() != server.uid {
        return Err(denied("Operator credential directory is not private to the controller."));
    }
    std::fs::set_permissions(&key_dir, std::fs::Permissions::from_mode(0o700))?;
    let key_path = key_dir.join(format!("{operator_id}.key"));
    let temp = PathBuf::from(format!("{}.{}.tmp", key_path.display(), std::process::id()));
    create_exclusive(&temp, token.as_bytes())?;
    std::fs::rename(&temp, &key_path)?;
    if !peer::seal_operator_credential(&key_path, server.uid).await {
        remove_force(&key_path);
        return Err(denied("Operator credential could not be kept private to the controller."));
    }
    Ok((token, key_path))
}

/// `enroll_operator`, `activate_operator` and `revoke_operator` over `admin.sock`.
pub async fn operator_admin<E: Engine + 'static>(server: &Rc<Server<E>>, action: &Value) -> Result<Value> {
    let operator_id = action.get("operator_id").and_then(Value::as_str).unwrap_or("").to_string();
    if !valid_principal(&operator_id) {
        return Err(invalid("Invalid stable operator ID."));
    }
    let paths = &server.paths;
    // One authority change at a time: each reads, modifies and rewrites the
    // whole file, and must not overwrite another's result.
    let _serial = server.authority_lock.lock().await;
    let mut current = load(&paths.authority())?;
    let already = |current: &Map<String, Value>| {
        current.get(&operator_id).is_some_and(|r|
            (truthy(r.get("enabled")) && server.engine.access_principal(&operator_id)!=Some(false))
            || r.get("pending").is_some_and(|p|truthy(Some(p)) && !expired(p.get("expires_at")) && p["controller_epoch"].as_str()==Some(server.engine.epoch().as_str())))
    };
    match action.get("op").and_then(Value::as_str) {
        Some("enroll_operator") => {
            if already(&current) {
                return Err(invalid("Operator already enrolled or pending; revoke before replacement."));
            }
            let binding = pairing_binding(action)?;
            let target = binding["target_endpoint_id"].as_str().unwrap_or("");
            if target != server.engine.endpoint_id() || binding["operator_endpoint_id"] == binding["target_endpoint_id"] {
                return Err(denied("Pairing target identity mismatch."));
            }
            let nonce = random_hex(32)?;
            let challenge_ref = crate::ids::id("pair");
            let expires_at = iso_from_millis(now_millis() + PAIRING_WINDOW_MS);
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(paths.challenges())?;
            let challenge = challenge_path(paths, &json!(challenge_ref))?;
            let (token, key_path) = issue_credential(server, &operator_id).await?;
            let challenge_body = json!({
                "nonce": nonce,
                "challenge_ref": challenge_ref,
                "operator_id": operator_id,
                "binding": binding,
                "expires_at": expires_at,
            });
            create_exclusive(&challenge, challenge_body.to_string().as_bytes())?;
            // The ACL tools were awaited: read the file again, and change and
            // save it with no await in between.
            current = match load(&paths.authority()) {
                Ok(fresh) if !already(&fresh) => fresh,
                fresh => {
                    remove_force(&challenge);
                    return Err(fresh.err().unwrap_or_else(|| invalid("Operator already enrolled or pending; revoke before replacement.")));
                }
            };
            let existing = current.get(&operator_id);
            // `(current[id]?.generation || 0) + 1`
            let generation =
                if truthy(existing.and_then(|r| r.get("generation"))) { generation_plus_one(existing) } else { json!(1) };
            current.insert(
                operator_id.clone(),
                json!({
                    "digest": sha256_hex(token.as_bytes()),
                    "enabled": false,
                    "observe": false,
                    "files": false,
                    "generation": generation,
                    "pending": {
                        "challenge_ref": challenge_ref,
                        "nonce_digest": sha256_hex(nonce.as_bytes()),
                        "expires_at": expires_at,
                        "controller_epoch": server.engine.epoch(),
                        "binding": binding,
                        "observe": action.get("observe") == Some(&Value::Bool(true)),
                        "files": action.get("files") == Some(&Value::Bool(true)),
                    },
                }),
            );
            if let Err(e) = save(&paths.authority(), &current) {
                remove_force(&challenge);
                return Err(e);
            }
            // Re-enrolment disables the old grant, so it also ends any viewer
            // access it holds; a failure there does not block enrolment.
            let _ = server.engine.revoke_viewer_operator(&operator_id).await;
            server.engine.access_sync().await?;
            server.sync_operator_peers().await;
            Ok(json!({
                "operator_id": operator_id,
                "generation": current[&operator_id]["generation"],
                "endpoint_id": server.engine.endpoint_id(),
                "controller_epoch": server.engine.epoch(),
                "credential_ref": key_path.to_string_lossy(),
                "challenge_ref": challenge_ref,
                "expires_at": expires_at,
                "state": "pending_target_local_confirmation",
                "authority": "target_operator_only",
            }))
        }
        Some("activate_operator") => {
            let record = current.get(&operator_id).cloned();
            let pending = record.as_ref().and_then(|r| r.get("pending")).filter(|p| truthy(Some(p)));
            let fresh = pending.is_some_and(|p| {
                truthy(p.get("confirmed_at"))
                    && strict_eq(p.get("challenge_ref"), action.get("challenge_ref"))
                    && strict_eq(p.get("binding").and_then(|b| b.get("review_digest")), action.get("review_digest"))
                    && !expired(p.get("expires_at"))
                    && p.get("controller_epoch").and_then(Value::as_str) == Some(server.engine.epoch().as_str())
            });
            let (Some(Value::Object(mut record)), Some(pending), true) = (record.clone(), pending.cloned(), fresh) else {
                return Err(denied("Pairing confirmation missing, stale or already consumed."));
            };
            let binding=pending.get("binding").cloned().unwrap_or(Value::Null);
            let paired_generation=record.get("generation").and_then(Value::as_u64).ok_or_else(||invalid("Invalid pairing generation."))?;
            let rights=crate::access::PairRights::Reviewed { observe: pending.get("observe")==Some(&Value::Bool(true)), files: pending.get("files")==Some(&Value::Bool(true)) };
            server.engine.access_pair(&operator_id,&binding,paired_generation,&rights).await?;
            record.insert("operator_endpoint_id".into(),binding["operator_endpoint_id"].clone());
            record.insert("operator_key_fingerprint".into(),binding["operator_key_fingerprint"].clone());
            record.insert("enabled".into(), json!(true));
            record.insert("observe".into(), pending.get("observe").cloned().unwrap_or(Value::Null));
            record.insert("files".into(), pending.get("files").cloned().unwrap_or(Value::Null));
            record.shift_remove("pending");
            let generation = record.get("generation").cloned().unwrap_or(Value::Null);
            current.insert(operator_id.clone(), Value::Object(record));
            save(&paths.authority(), &current)?;
            server.engine.access_sync().await?;
            server.sync_operator_peers().await;
            Ok(json!({
                "operator_id": operator_id,
                "generation": generation,
                "endpoint_id": server.engine.endpoint_id(),
                "controller_epoch": server.engine.epoch(),
                "credential_ref": paths.operator_keys().join(format!("{operator_id}.key")).to_string_lossy(),
                "state": "active",
            }))
        }
        Some("revoke_operator") => {
            server.engine.access_unpair(&operator_id).await?;
            let Some(Value::Object(mut record)) = current.get(&operator_id).cloned() else {
                return Err(invalid("Operator is not enrolled."));
            };
            if let Some(pending) = record.get("pending").filter(|p| truthy(Some(p))) {
                remove_force(&challenge_path(paths, pending.get("challenge_ref").unwrap_or(&Value::Null))?);
            }
            let generation = generation_plus_one(Some(&Value::Object(record.clone())));
            record.insert("enabled".into(), json!(false));
            record.insert("generation".into(), generation.clone());
            record.shift_remove("pending");
            current.insert(operator_id.clone(), Value::Object(record));
            save(&paths.authority(), &current)?;
            server.engine.revoke_viewer_operator(&operator_id).await?;
            server.engine.access_sync().await?;
            server.sync_operator_peers().await;
            Ok(json!({ "operator_id": operator_id, "generation": generation, "enabled": false }))
        }
        _ => Err(invalid("Unknown operator administration.")),
    }
}

/// Target-local pairing confirmation over the operator route (`operatorPairingConfirm`).
pub async fn pairing_confirm<E: Engine + 'static>(server: &Server<E>, operator_id: &str, action: &Value) -> Result<Value> {
    let paths = &server.paths;
    let _serial = server.authority_lock.lock().await;
    let mut current = load(&paths.authority())?;
    let binding = pairing_binding(action)?;
    let epoch = server.engine.epoch();
    let endpoint = server.engine.endpoint_id();
    let record = current.get(operator_id);
    let pending = record.and_then(|r| r.get("pending")).filter(|p| truthy(Some(p)));
    let nonce = action.get("nonce").and_then(Value::as_str).filter(|n| is_hex_lower(n, 64));
    let valid = match (record, pending, nonce) {
        (Some(record), Some(pending), Some(nonce)) => {
            !truthy(pending.get("confirmed_at"))
                && strict_eq(pending.get("challenge_ref"), action.get("challenge_ref"))
                && pending.get("controller_epoch").and_then(Value::as_str) == Some(epoch.as_str())
                && !expired(pending.get("expires_at"))
                && action.get("endpoint_id").and_then(Value::as_str) == Some(endpoint.as_str())
                && action.get("controller_epoch").and_then(Value::as_str) == Some(epoch.as_str())
                && strict_eq(action.get("expected_authorization_generation"), record.get("generation"))
                && pending.get("binding").and_then(Value::as_object).is_some_and(|b| {
                    ["review_digest", "operator_endpoint_id", "target_endpoint_id", "operator_key_fingerprint"]
                        .iter()
                        .all(|k| strict_eq(b.get(*k), binding.get(*k)))
                })
                && pending
                    .get("nonce_digest")
                    .and_then(Value::as_str)
                    .and_then(unhex)
                    .is_some_and(|digest| ct_eq(&digest, &sha256(nonce.as_bytes())))
        }
        _ => false,
    };
    if !valid {
        return Err(denied("Pairing challenge invalid, expired, changed or already consumed."));
    }
    let generation = current[operator_id].get("generation").cloned().unwrap_or(Value::Null);
    let pending = current
        .get_mut(operator_id)
        .and_then(|r| r.get_mut("pending"))
        .and_then(Value::as_object_mut)
        .ok_or_else(unknown_failure)?;
    pending.insert("confirmed_at".into(), json!(now_iso()));
    let challenge_ref = pending.get("challenge_ref").cloned().unwrap_or(Value::Null);
    save(&paths.authority(), &current)?;
    remove_force(&challenge_path(paths, &challenge_ref)?);
    Ok(json!({
        "operator_id": operator_id,
        "challenge_ref": challenge_ref,
        "endpoint_id": endpoint,
        "controller_epoch": epoch,
        "authorization_generation": generation,
        "state": "confirmed_pending_activation",
    }))
}

/// `viewer_register {viewer_cert_sha256}` over the operator route: the
/// SHA-256 of the certificate this operator's viewer presents, with no PIN and
/// no restart. The controller authorizes the call first (an active pairing,
/// the pinned endpoint and epoch, a well-formed hash); the certificate is then
/// kept on the operator's record as `viewer: {cert_sha256, generation}`, for
/// that pairing generation only, so a new pairing starts without one. A
/// different certificate ends any stream the old one holds.
pub async fn viewer_register<E: Engine + 'static>(server: &Server<E>, operator_id: &str, action: &Value) -> Result<Value> {
    let mut reply = server.engine.operator_call(operator_id, action.clone()).await?;
    let cert = action.get("viewer_cert_sha256").and_then(Value::as_str).filter(|c| is_hex_lower(c, 64)).ok_or_else(|| invalid("Expected the viewer certificate's SHA-256."))?;
    let entry = json!({"cert_sha256": cert, "generation": reply.get("authorization_generation").cloned().unwrap_or(Value::Null)});
    let replaced = {
        let _serial = server.authority_lock.lock().await;
        let mut current = load(&server.paths.authority())?;
        let Some(Value::Object(record)) = current.get_mut(operator_id) else {
            return Err(denied("This computer is not paired here; pair it again."));
        };
        let before = record.get("viewer").cloned();
        if before.as_ref() != Some(&entry) {
            record.insert("viewer".into(), entry);
            save(&server.paths.authority(), &current)?;
        }
        before.as_ref().and_then(|b| b.get("cert_sha256")).and_then(Value::as_str).is_some_and(|old| old != cert)
    };
    if replaced {
        server.engine.revoke_viewer_operator(operator_id).await?;
    }
    if let Some(out) = reply.as_object_mut() {
        out.insert("viewer_cert_sha256".into(), json!(cert));
        out.insert("replaced".into(), json!(replaced));
    }
    Ok(reply)
}

/// A computer that asked to be added, as Tailscale identified it, with the key
/// it will sign in with (and has proved it holds). A person accepted it, its
/// own desktop user's console vouched for it on another of the same person's
/// computers, it brought a valid invite, or it is this computer adding itself.
pub struct Enrollment<'a> {
    pub key: &'a crate::sshkey::KeyLine,
    /// What the other computer calls itself (`host_<…>`): recorded, not trusted.
    pub operator_endpoint_id: &'a str,
    /// Its Tailscale node name, host name, stable node ID and login (from `tailscale whois`).
    pub node: &'a str,
    pub host_name: &'a str,
    pub stable_id: &'a str,
    pub login: &'a str,
    pub own_computer: bool,
    /// The invite it redeemed: its level and expiry replace whatever it had here.
    pub invite: Option<&'a super::invites::Redeemed>,
}

/// An enabled pairing for a key: its principal, grant generation and the
/// Tailscale computer it was enrolled for (none for pairings made before
/// tailnet pairing).
pub struct Enrolled {
    pub principal: String,
    pub generation: Value,
    stable_id: Option<String>,
}

impl Enrolled {
    /// Whether the computer Tailscale calls `stable_id` is the one this key was
    /// enrolled for. A pairing with no recorded computer only needs the key,
    /// which the request has already proved it holds.
    pub fn same_computer(&self, stable_id: &str) -> bool {
        self.stable_id.as_deref().is_none_or(|recorded| recorded == stable_id)
    }
}

/// The enabled pairing for `fingerprint`.
pub fn enrolled_key<E: Engine + 'static>(server: &Server<E>, fingerprint: &str) -> Option<Enrolled> {
    let current = load(&server.paths.authority()).ok()?;
    current
        .iter()
        .find(|(id, record)| {
            truthy(record.get("enabled")) && holds_key(server, id, record, fingerprint) && server.engine.access_principal(id) != Some(false)
        })
        .map(|(id, record)| Enrolled {
            principal: id.clone(),
            generation: record.get("generation").cloned().unwrap_or(Value::Null),
            stable_id: record.pointer("/tailscale/stable_id").and_then(Value::as_str).map(str::to_string),
        })
}

/// Whether `record` (principal `id`) is paired with this key. Pairings made
/// before tailnet pairing keep the key only in the access model.
fn holds_key<E: Engine + 'static>(server: &Server<E>, id: &str, record: &Value, fingerprint: &str) -> bool {
    match record.get("operator_key_fingerprint").and_then(Value::as_str) {
        Some(recorded) => recorded == fingerprint,
        None => server.engine.access_pairing_key(id).as_deref() == Some(fingerprint),
    }
}

/// `^[a-z][a-z0-9_-]{0,max-1}$` from a Tailscale node name.
fn principal_from(node: &str, max: usize) -> String {
    let mut name: String = node
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '-' })
        .collect();
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name.insert(0, 'c');
    }
    name.truncate(max);
    let name = name.trim_end_matches(['-', '_']);
    if name.is_empty() { "computer".into() } else { name.to_string() }
}

/// The principal for a requesting computer: the name its key already has here,
/// else its node name, or `name-2` … `name-9` when another computer already holds
/// that name or the name is the local owner's. Its own earlier pairing (same
/// key, or same Tailscale node) keeps its name and is replaced.
fn choose_principal<E: Engine + 'static>(server: &Server<E>, current: &Map<String, Value>, request: &Enrollment<'_>) -> Result<String> {
    // The same key, whatever the computer is called now: its earlier name, so root
    // never holds one key under two names.
    if let Some((principal, _)) = current.iter().find(|(p, r)| holds_key(server, p, r, &request.key.fingerprint)) {
        return Ok(principal.clone());
    }
    for n in 1..=9 {
        let candidate = if n == 1 { principal_from(request.node, 23) } else { format!("{}-{n}", principal_from(request.node, 21)) };
        if !valid_principal(&candidate) {
            continue;
        }
        let ours = match current.get(&candidate) {
            Some(record) => {
                record.pointer("/tailscale/stable_id").and_then(Value::as_str) == Some(request.stable_id)
                    || holds_key(server, &candidate, record, &request.key.fingerprint)
            }
            None => server.engine.access_pairing_key(&candidate).is_none_or(|key| key == request.key.fingerprint),
        };
        if ours {
            return Ok(candidate);
        }
    }
    Err(denied("Too many computers here already use this name. Rename this computer in Tailscale, then try again."))
}

/// Enrol an accepted computer as an operator of this one: the bearer, the
/// authority record and the access pairing `enroll_operator` and
/// `activate_operator` produce, with today's pairing rights (watch and files),
/// control, agents and administer as well when both computers belong to the
/// same person, or exactly an invite's level until it ends. The record keeps
/// the public key so the access projection can give the computer its SSH
/// account. An enabled pairing for the same key is kept as it is, unless an
/// invite replaces it; it gains administer as the owner's own computer only when
/// the same Tailscale computer asks again (or the pairing predates tailnet
/// pairing and so recorded none). Returns the principal and the grant generation.
pub async fn pair_operator<E: Engine + 'static>(server: &Rc<Server<E>>, request: &Enrollment<'_>) -> Result<(String, Value)> {
    let paths = &server.paths;
    let serial = server.authority_lock.lock().await;
    if request.invite.is_none()
        && let Some(found) = enrolled_key(server, &request.key.fingerprint)
    {
        drop(serial);
        if request.own_computer && found.same_computer(request.stable_id) {
            server.engine.access_own_computer(&found.principal)?;
        }
        server.engine.access_sync().await?;
        server.sync_operator_peers().await;
        return Ok((found.principal, found.generation));
    }
    let previous = load(&paths.authority())?;
    let principal = choose_principal(server, &previous, request)?;
    let previous = previous.get(&principal).cloned();
    // The computer's earlier pairing ends before the new one starts.
    if server.engine.access_pairing_key(&principal).is_some() {
        server.engine.access_unpair(&principal).await?;
    }
    let _ = server.engine.revoke_viewer_operator(&principal).await;
    if let Some(pending) = previous.as_ref().and_then(|r| r.get("pending")).filter(|p| truthy(Some(p)))
        && let Ok(challenge) = challenge_path(paths, pending.get("challenge_ref").unwrap_or(&Value::Null))
    {
        remove_force(&challenge);
    }
    let generation =
        if truthy(previous.as_ref().and_then(|r| r.get("generation"))) { generation_plus_one(previous.as_ref()) } else { json!(1) };
    let paired_generation = generation.as_u64().ok_or_else(|| invalid("Invalid pairing generation."))?;
    let (token, _) = issue_credential(server, &principal).await?;
    let binding = json!({
        "operator_key_fingerprint": request.key.fingerprint,
        "operator_endpoint_id": request.operator_endpoint_id,
    });
    let rights = match request.invite {
        Some(invite) => PairRights::Invite {
            level: invite.level,
            expires_at: invite.expires_at.map(iso_from_millis),
            summary: super::invites::joined_summary(request.login, request.host_name, invite),
        },
        None if request.own_computer => PairRights::OwnComputer,
        None => PairRights::Reviewed { observe: true, files: true },
    };
    server.engine.access_pair(&principal, &binding, paired_generation, &rights).await?;
    let mut current = load(&paths.authority())?;
    // What the record says matters only where no access model exists yet: an
    // invite's level and end hold there too.
    let files = request.invite.is_none_or(|i| i.level.rule("files") == crate::access::Rule::Allow);
    let mut record = json!({
        "digest": sha256_hex(token.as_bytes()),
        "enabled": true,
        "observe": true,
        "files": files,
        "generation": generation,
        "operator_endpoint_id": request.operator_endpoint_id,
        "operator_key_fingerprint": request.key.fingerprint,
        "operator_public_key": request.key.line,
        "tailscale": {"node": request.node, "host_name": request.host_name, "stable_id": request.stable_id, "login": request.login},
        "own_computer": request.own_computer,
        "paired_at": now_iso(),
    });
    if let Some(invite) = request.invite {
        record["invite"] = json!({"id": invite.id, "level": invite.level.name()});
        if let Some(at) = invite.expires_at {
            record["expires_at"] = json!(iso_from_millis(at));
        }
    }
    current.insert(principal.clone(), record);
    save(&paths.authority(), &current)?;
    drop(serial);
    server.engine.access_sync().await?;
    server.sync_operator_peers().await;
    Ok((principal, generation))
}
