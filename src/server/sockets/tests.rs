//! Transport tests against real sockets with a stub engine: authentication,
//! refusals and the pairing state machine. The engine behind them is the
//! controller's business and is tested there.

use super::*;
use crate::desktop::run::Cancel;
use crate::mcp::CallOutcome;
use crate::server::Paths;
use crate::server::peer::SystemDb;
use crate::server::policy::{Keys, Policy, sha256_hex};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const GATEWAY: &str = "gateway-secret";
const ADMIN: &str = "admin-secret";
const LEGACY_OPERATOR: &str = "legacy-operator-secret";
const ENDPOINT: &str = "ibara_00000000000000000000000000000000";
const EPOCH: &str = "epoch_00000000000000000000000000000000";

#[derive(Default)]
struct Stub {
    calls: RefCell<Vec<String>>,
    /// Releases `hold` calls, which ignore cancellation like an effect in flight.
    release: tokio::sync::Notify,
}

impl Engine for Stub {
    fn epoch(&self) -> String {
        EPOCH.into()
    }
    fn endpoint_id(&self) -> String {
        ENDPOINT.into()
    }
    async fn call(&self, principal: &str, _: &str, client: &str, tool: &str, _: Value, cancel: Cancel) -> CallOutcome {
        self.calls.borrow_mut().push(format!("call {principal} {client} {tool}"));
        match tool {
            // Runs until the caller goes away (a long computer_wait).
            "until_cancelled" => {
                if tokio::time::timeout(Duration::from_secs(10), cancel.cancelled()).await.is_ok() {
                    self.calls.borrow_mut().push("cancelled".into());
                }
            }
            // Holds until the test releases it, whatever happens to the caller.
            "hold" => self.release.notified().await,
            _ => {}
        }
        CallOutcome::new(json!({ "situation": "stub", "status": "ok", "since": [], "result": {}, "error": null }))
    }
    async fn heartbeat(&self, principal: &str, _: &str, _: bool) -> Result<()> {
        self.calls.borrow_mut().push(format!("heartbeat {principal}"));
        Ok(())
    }
    async fn disconnect(&self, principal: &str, _: &str) -> Result<()> {
        self.calls.borrow_mut().push(format!("disconnect {principal}"));
        Ok(())
    }
    async fn admin(&self, action: Value) -> Result<Value> {
        self.calls.borrow_mut().push(format!("admin {}", action["op"]));
        Ok(json!({ "ok": true }))
    }
    async fn operator_call(&self, operator_id: &str, action: Value) -> Result<Value> {
        self.calls.borrow_mut().push(format!("operator {operator_id} {}", action["op"]));
        // The controller authorizes a viewer registration and names the pairing generation.
        if action["op"] == "viewer_register" {
            return Ok(json!({ "authorization_generation": 3 }));
        }
        Ok(json!({ "operator": operator_id }))
    }
    async fn transfer(&self, principal: &str, _: &str, _: Value) -> Result<Value> {
        self.calls.borrow_mut().push(format!("transfer {principal}"));
        Ok(json!({}))
    }
    async fn revoke_viewer_operator(&self, operator_id: &str) -> Result<()> {
        self.calls.borrow_mut().push(format!("revoke_viewer {operator_id}"));
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    server: Rc<Server<Stub>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.close();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("ibara-se-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["state", "data", "run", "operators"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::set_permissions(root.join("operators"), std::fs::Permissions::from_mode(0o711)).unwrap();
        std::fs::write(
            root.join("passwd"),
            "root:x:0:0::/root:/bin/bash\nibara-op-test:x:990:990::/var/lib/ibara-operator:/usr/local/sbin/ibara-op-shell\n",
        )
        .unwrap();
        std::fs::write(root.join("group"), "ibara-runtime:x:960:tulip1,ibara-agent\n").unwrap();
        let accounts = root.join("operator-accounts.json");
        std::fs::write(&accounts, r#"{"schema_version":1,"accounts":{"test":{"user":"ibara-op-test","uid":990}}}"#).unwrap();
        std::fs::set_permissions(&accounts, std::fs::Permissions::from_mode(0o644)).unwrap();
        let paths = Paths {
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            runtime_dir: root.join("run"),
            install_root: root.clone(),
            release_root: root.clone(),
            procedures_dir: root.join("procedures"),
            policy: root.join("policy.json"),
            gateway_key: root.join("gateway.key"),
            admin_hash: root.join("admin.sha256"),
            operator_accounts: accounts,
            operator_accounts_overridden: true,
            operator_socket_dir: root.join("operators"),
            ssh_host_key: root.join("host_ed25519.pub"),
            station: root.join("station.json"),
        };
        let policy = Policy::from_value(json!({
            "principals": ["test"],
            "operator_credentials": { "legacyop": sha256_hex(LEGACY_OPERATOR.as_bytes()) },
        }))
        .unwrap();
        let keys = Keys::new(GATEWAY, &sha256_hex(ADMIN.as_bytes()));
        let system = SystemDb { passwd: root.join("passwd"), group: root.join("group") };
        let server = Rc::new(Server::with_system(Rc::new(Stub::default()), paths, policy, keys, system));
        Fixture { root, server }
    }

    fn calls(&self) -> Vec<String> {
        self.server.engine.calls.borrow().clone()
    }
}

/// One raw request; returns the status code and the JSON body.
async fn raw(socket: &Path, method: &str, path: &str, bearer: Option<&str>, content_length: usize, body: &[u8]) -> (u16, Value) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {content_length}\r\n");
    if let Some(bearer) = bearer {
        head.push_str(&format!("authorization: Bearer {bearer}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await.unwrap();
    let text = String::from_utf8(reply).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

async fn post(socket: &Path, bearer: Option<&str>, body: Value) -> (u16, Value) {
    let bytes = body.to_string().into_bytes();
    raw(socket, "POST", "/v1", bearer, bytes.len(), &bytes).await
}

fn local<F: Future<Output = ()>>(test: F) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, test);
}

fn refused() -> Value {
    json!({ "error": { "code": "PERMISSION_DENIED", "message": "Unauthorized transport." } })
}

fn envelope_error(reply: &Value) -> (&str, &str) {
    let error = &reply["result"]["error"];
    assert_eq!(reply["result"]["status"], "error", "{reply}");
    (error["code"].as_str().unwrap(), error["message"].as_str().unwrap())
}

#[test]
fn wrong_credentials_get_403_and_reach_nothing() {
    local(async {
        let f = Fixture::new("auth");
        f.server.listen().await.unwrap();
        let gateway = f.server.paths.controller_socket();
        let admin = f.server.paths.admin_socket();
        let heartbeat = json!({ "kind": "heartbeat", "principal": "test", "connectionId": "connection_1" });
        for (socket, bearer) in [
            (&gateway, None),
            (&gateway, Some("wrong")),
            (&gateway, Some(ADMIN)),
            (&admin, Some(GATEWAY)),
            (&admin, Some(LEGACY_OPERATOR)),
            (&admin, None),
        ] {
            assert_eq!(post(socket, bearer, heartbeat.clone()).await, (403, refused()), "{socket:?} {bearer:?}");
        }
        let body = heartbeat.to_string();
        assert_eq!(raw(&gateway, "GET", "/v1", Some(GATEWAY), body.len(), body.as_bytes()).await, (403, refused()));
        assert_eq!(raw(&gateway, "POST", "/v2", Some(GATEWAY), body.len(), body.as_bytes()).await, (403, refused()));
        assert!(f.calls().is_empty(), "{:?}", f.calls());

        // A principal outside policy.principals, or an over-long connection id, is refused after authentication.
        let (status, reply) = post(&gateway, Some(GATEWAY), json!({ "kind": "heartbeat", "principal": "other", "connectionId": "c" })).await;
        assert_eq!(status, 200);
        assert_eq!(envelope_error(&reply), ("PERMISSION_DENIED", "Unknown principal or connection."));
        let long = "c".repeat(129);
        let (_, reply) = post(&gateway, Some(GATEWAY), json!({ "kind": "heartbeat", "principal": "test", "connectionId": long })).await;
        assert_eq!(envelope_error(&reply), ("PERMISSION_DENIED", "Unknown principal or connection."));
        // An admin body on admin.sock must be {kind:"admin", action}.
        let (_, reply) = post(&admin, Some(ADMIN), json!({ "kind": "call", "action": { "op": "status" } })).await;
        assert_eq!(envelope_error(&reply), ("INVALID_ARGUMENT", "Expected admin operation."));
        assert!(f.calls().is_empty(), "{:?}", f.calls());

        // The right credentials reach the engine.
        assert_eq!(post(&gateway, Some(GATEWAY), heartbeat.clone()).await, (200, json!({ "ok": true })));
        assert_eq!(post(&admin, Some(ADMIN), json!({ "kind": "admin", "action": { "op": "status" } })).await, (200, json!({ "result": { "ok": true } })));
        let (_, reply) = post(&gateway, Some(GATEWAY), json!({ "kind": "negotiate", "principal": "test", "connectionId": "c", "contract_version": "3.0" })).await;
        assert_eq!(reply, json!({ "contract_version": "4" }));
        assert_eq!(f.calls(), ["heartbeat test", "admin \"status\""]);
    });
}

#[test]
fn operator_bearer_cannot_use_agent_or_admin_kinds() {
    local(async {
        let f = Fixture::new("opbearer");
        f.server.listen().await.unwrap();
        let gateway = f.server.paths.controller_socket();
        let refusal = ("PERMISSION_DENIED", "Operator credential cannot call agent or admin operations.");
        for body in [
            json!({ "kind": "call", "principal": "legacyop", "connectionId": "c", "tool": "computer_status", "args": {} }),
            json!({ "kind": "call", "principal": "test", "connectionId": "c", "tool": "computer_status", "args": {} }),
            json!({ "kind": "heartbeat", "principal": "test", "connectionId": "c" }),
            json!({ "kind": "admin", "action": { "op": "pause" } }),
            json!({ "kind": "operator", "principal": "someone", "action": { "op": "status" } }),
            json!({ "kind": "operator", "principal": "legacyop", "action": null }),
        ] {
            let (status, reply) = post(&gateway, Some(LEGACY_OPERATOR), body.clone()).await;
            assert_eq!(status, 200);
            assert_eq!(envelope_error(&reply), refusal, "{body}");
        }
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        let (_, reply) = post(&gateway, Some(LEGACY_OPERATOR), json!({ "kind": "operator", "principal": "legacyop", "action": { "op": "status" } })).await;
        assert_eq!(reply, json!({ "result": { "operator": "legacyop" } }));
    });
}

#[test]
fn oversize_and_malformed_bodies_are_refused_without_reaching_the_engine() {
    local(async {
        let f = Fixture::new("oversize");
        f.server.listen().await.unwrap();
        let gateway = f.server.paths.controller_socket();
        let (status, reply) = raw(&gateway, "POST", "/v1", Some(GATEWAY), MAX_BODY + 1, b"").await;
        assert_eq!(status, 200);
        assert_eq!(envelope_error(&reply), ("INVALID_ARGUMENT", "Request too large."));
        assert_eq!(reply["result"]["contract_version"], "2.0");
        assert_eq!(reply["result"]["records"], json!([]));
        let (_, reply) = raw(&gateway, "POST", "/v1", Some(GATEWAY), 5, b"{nope").await;
        assert_eq!(envelope_error(&reply).0, "INTERNAL_ERROR");
        assert_eq!(reply["result"]["error"]["requires_reconciliation"], true);
        let (_, reply) = post(&gateway, Some(GATEWAY), json!({ "kind": "bogus", "principal": "test", "connectionId": "c" })).await;
        assert_eq!(envelope_error(&reply), ("INVALID_ARGUMENT", "Unknown RPC operation."));
        assert!(f.calls().is_empty(), "{:?}", f.calls());
    });
}

#[test]
fn an_active_socket_is_never_replaced() {
    local(async {
        let f = Fixture::new("active");
        f.server.listen().await.unwrap();
        let err = listen(&f.server, &f.server.paths.controller_socket(), 0o660, Kind::Gateway).await.err().unwrap();
        assert_eq!(err.message, "Existing controller socket is active.");
        // A stale file (nobody listening) is replaced.
        let stale = f.root.join("run/stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        listen(&f.server, &stale, 0o600, Kind::Admin).await.unwrap().close();
    });
}

#[test]
fn peer_socket_refuses_a_uid_other_than_the_operator_account() {
    local(async {
        let f = Fixture::new("peer");
        let accounts =
            peer::load_operator_accounts(&f.server.paths.operator_accounts, true, f.server.uid, &f.server.system).unwrap();
        assert_eq!(accounts["test"].uid, 990, "the fixture account must load, or the refusal proves nothing");
        let path = f.server.paths.operator_socket_dir.join("test.sock");
        let listening = serve(&f.server, UnixListener::bind(&path).unwrap(), &path, Kind::Peer("test".into()));
        let body = json!({ "kind": "operator", "principal": "test", "action": { "op": "status" } });
        assert_eq!(post(&path, None, body.clone()).await, (403, refused()));
        assert_eq!(post(&path, Some(GATEWAY), body).await, (403, refused()));
        assert!(f.calls().is_empty(), "{:?}", f.calls());
        listening.close();
        assert!(!path.exists());

        // Opening for real needs the one-user ACL; an account the system does
        // not know cannot be granted, so no socket appears (fail closed).
        f.server.sync_operator_peers().await;
        assert!(!path.exists());
        assert!(f.server.peers.borrow().is_empty());
    });
}

#[test]
fn pairing_needs_the_target_local_nonce_and_is_single_use() {
    local(async {
        let f = Fixture::new("pairing");
        f.server.listen().await.unwrap();
        let gateway = f.server.paths.controller_socket();
        let admin = f.server.paths.admin_socket();
        let review = "b".repeat(64);
        let enroll = json!({ "kind": "admin", "action": {
            "op": "enroll_operator", "operator_id": "vesper", "review_digest": review,
            "operator_endpoint_id": "host_12345678", "target_endpoint_id": ENDPOINT,
            "operator_key_fingerprint": "SHA256:abcdefghijklmnop", "observe": true, "files": true,
        } });

        // The target endpoint must be this one.
        let mut wrong_target = enroll.clone();
        wrong_target["action"]["target_endpoint_id"] = json!("ibara_somewhere_else");
        let (_, reply) = post(&admin, Some(ADMIN), wrong_target).await;
        assert_eq!(envelope_error(&reply), ("PERMISSION_DENIED", "Pairing target identity mismatch."));

        let (_, reply) = post(&admin, Some(ADMIN), enroll.clone()).await;
        let enrolled = &reply["result"];
        assert_eq!(enrolled["state"], "pending_target_local_confirmation", "{reply}");
        assert_eq!(enrolled["generation"], 1);
        assert_eq!(enrolled["endpoint_id"], ENDPOINT);
        let challenge_ref = enrolled["challenge_ref"].as_str().unwrap().to_string();
        assert!(authority::valid_challenge_ref(&challenge_ref));
        let bearer = std::fs::read_to_string(enrolled["credential_ref"].as_str().unwrap()).unwrap();
        let challenge_file = f.server.paths.challenges().join(format!("{challenge_ref}.json"));
        let challenge: Value = serde_json::from_slice(&std::fs::read(&challenge_file).unwrap()).unwrap();
        let nonce = challenge["nonce"].as_str().unwrap().to_string();

        // A second enrolment while pending is refused.
        let (_, reply) = post(&admin, Some(ADMIN), enroll).await;
        assert_eq!(envelope_error(&reply), ("INVALID_ARGUMENT", "Operator already enrolled or pending; revoke before replacement."));

        // Activation before target-local confirmation is refused.
        let activate = json!({ "kind": "admin", "action": { "op": "activate_operator", "operator_id": "vesper", "challenge_ref": challenge_ref, "review_digest": review } });
        let (_, reply) = post(&admin, Some(ADMIN), activate.clone()).await;
        assert_eq!(envelope_error(&reply), ("PERMISSION_DENIED", "Pairing confirmation missing, stale or already consumed."));

        let confirm = |nonce: &str| {
            json!({ "kind": "operator", "principal": "vesper", "action": {
                "op": "pairing_confirm", "challenge_ref": challenge_ref, "nonce": nonce,
                "endpoint_id": ENDPOINT, "controller_epoch": EPOCH, "expected_authorization_generation": 1,
                "review_digest": review, "operator_endpoint_id": "host_12345678", "target_endpoint_id": ENDPOINT,
                "operator_key_fingerprint": "SHA256:abcdefghijklmnop",
            } })
        };
        let denied = ("PERMISSION_DENIED", "Pairing challenge invalid, expired, changed or already consumed.");
        let (_, reply) = post(&gateway, Some(&bearer), confirm(&"0".repeat(64))).await;
        assert_eq!(envelope_error(&reply), denied);
        let mut changed = confirm(&nonce);
        changed["action"]["operator_key_fingerprint"] = json!("SHA256:zzzzzzzzzzzzzzzz");
        let (_, reply) = post(&gateway, Some(&bearer), changed).await;
        assert_eq!(envelope_error(&reply), denied);
        assert!(challenge_file.exists(), "a refused confirmation must not consume the challenge");

        let (_, reply) = post(&gateway, Some(&bearer), confirm(&nonce)).await;
        assert_eq!(reply["result"]["state"], "confirmed_pending_activation", "{reply}");
        assert_eq!(reply["result"]["challenge_ref"], challenge_ref.as_str());
        assert_eq!(reply["result"]["authorization_generation"], 1);
        assert!(!challenge_file.exists());
        let (_, reply) = post(&gateway, Some(&bearer), confirm(&nonce)).await;
        assert_eq!(envelope_error(&reply), denied);

        let mut wrong_digest = activate.clone();
        wrong_digest["action"]["review_digest"] = json!("c".repeat(64));
        let (_, reply) = post(&admin, Some(ADMIN), wrong_digest).await;
        assert_eq!(envelope_error(&reply).0, "PERMISSION_DENIED");
        let (_, reply) = post(&admin, Some(ADMIN), activate.clone()).await;
        assert_eq!(reply["result"]["state"], "active", "{reply}");
        assert_eq!(reply["result"]["generation"], 1);
        let (_, reply) = post(&admin, Some(ADMIN), activate).await;
        assert_eq!(envelope_error(&reply).0, "PERMISSION_DENIED");

        let (_, reply) =
            post(&admin, Some(ADMIN), json!({ "kind": "admin", "action": { "op": "revoke_operator", "operator_id": "vesper" } })).await;
        assert_eq!(reply, json!({ "result": { "operator_id": "vesper", "generation": 2, "enabled": false } }));
        let stored = authority::load(&f.server.paths.authority()).unwrap();
        assert_eq!(stored["vesper"]["enabled"], false);
        assert!(stored["vesper"].get("pending").is_none());
        // Enrolment and revocation both end any viewer access the operator held.
        assert_eq!(f.calls(), ["revoke_viewer vesper", "revoke_viewer vesper"]);
    });
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Send a request without reading the reply; the caller drops the stream to hang up.
async fn send_only(socket: &Path, bearer: &str, body: Value) -> UnixStream {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let body = body.to_string();
    let head = format!("POST /v1 HTTP/1.1\r\ncontent-length: {}\r\nauthorization: Bearer {bearer}\r\n\r\n", body.len());
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream
}

fn call_body(tool: &str) -> Value {
    json!({ "kind": "call", "principal": "test", "connectionId": "c", "tool": tool, "args": {}, "client_name": "codex" })
}

#[test]
fn a_call_is_cancelled_when_its_caller_hangs_up() {
    local(async {
        let f = Fixture::new("hangup");
        f.server.listen().await.unwrap();
        let client = send_only(&f.server.paths.controller_socket(), GATEWAY, call_body("until_cancelled")).await;
        eventually("the call to start", || f.calls().len() == 1).await;
        drop(client);
        eventually("the engine to see the cancellation", || f.calls().contains(&"cancelled".to_string())).await;
    });
}

#[test]
fn abandoned_calls_do_not_lock_out_new_connections() {
    local(async {
        let f = Fixture::new("abandoned");
        f.server.listen().await.unwrap();
        let socket = f.server.paths.controller_socket();
        let mut clients = Vec::new();
        for _ in 0..MAIN_CONNECTIONS {
            clients.push(send_only(&socket, GATEWAY, call_body("hold")).await);
        }
        eventually("every call to start", || f.calls().len() == MAIN_CONNECTIONS).await;
        drop(clients);
        // The engine is still busy with all of them, but their callers are gone.
        let heartbeat = json!({ "kind": "heartbeat", "principal": "test", "connectionId": "c2" });
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            let body = heartbeat.to_string();
            let head = format!("POST /v1 HTTP/1.1\r\ncontent-length: {}\r\nauthorization: Bearer {GATEWAY}\r\n\r\n", body.len());
            let _ = stream.write_all(format!("{head}{body}").as_bytes()).await;
            let mut reply = Vec::new();
            let _ = stream.read_to_end(&mut reply).await;
            if String::from_utf8_lossy(&reply).ends_with(r#"{"ok":true}"#) {
                break;
            }
            assert!(Instant::now() < deadline, "a new connection is still refused after its predecessors hung up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        f.server.engine.release.notify_waiters();
    });
}

#[test]
fn an_endless_request_head_is_cut_off_at_the_bound() {
    local(async {
        let f = Fixture::new("head");
        f.server.listen().await.unwrap();
        let mut stream = UnixStream::connect(f.server.paths.controller_socket()).await.unwrap();
        // One header line far past the 16 KiB bound, with no newline.
        let line = format!("POST /v1 HTTP/1.1\r\nx-filler: {}", "a".repeat(1024 * 1024));
        let (mut read, mut write) = stream.split();
        let writer = async {
            let _ = write.write_all(line.as_bytes()).await;
        };
        let reader = async {
            let mut reply = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), read.read_to_end(&mut reply)).await
        };
        let (_, closed) = tokio::join!(writer, reader);
        assert!(closed.is_ok(), "the server kept reading an over-long head instead of closing");
    });
}

#[test]
fn a_revocation_during_an_enrolment_is_not_undone() {
    local(async {
        let f = Fixture::new("race");
        let hazel = json!({ "hazel": { "digest": "0".repeat(64), "enabled": true, "observe": true, "files": true, "generation": 3 } });
        std::fs::write(f.server.paths.authority(), hazel.to_string()).unwrap();
        let enroll = json!({
            "op": "enroll_operator", "operator_id": "vesper", "review_digest": "b".repeat(64),
            "operator_endpoint_id": "host_12345678", "target_endpoint_id": ENDPOINT,
            "operator_key_fingerprint": "SHA256:abcdefghijklmnop", "observe": true, "files": true,
        });
        let revoke = json!({ "op": "revoke_operator", "operator_id": "hazel" });
        // The enrolment waits on the ACL tools while the revocation runs.
        let (enrolled, revoked) =
            tokio::join!(authority::operator_admin(&f.server, &enroll), authority::operator_admin(&f.server, &revoke));
        assert_eq!(enrolled.unwrap()["state"], "pending_target_local_confirmation");
        assert_eq!(revoked.unwrap()["generation"], 4);
        let stored = authority::load(&f.server.paths.authority()).unwrap();
        assert_eq!(stored["hazel"]["enabled"], false, "{stored:?}");
        assert_eq!(stored["hazel"]["generation"], 4);
        assert!(stored["vesper"]["pending"].is_object());
    });
}

/// Failure cases: a viewer registered for a computer not paired here; the
/// same certificate again ends the stream it opened; a new certificate leaves
/// the old one's stream running or keeps the old certificate.
#[test]
fn a_viewer_registers_on_its_pairing_record_and_a_new_one_ends_the_old_stream() {
    local(async {
        let f = Fixture::new("viewer-register");
        let register = |cert: &str| json!({ "op": "viewer_register", "viewer_cert_sha256": cert });
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let err = authority::viewer_register(&f.server, "vesper", &register(&a)).await.unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED", "{err:?}");

        let paired = json!({ "vesper": { "digest": "0".repeat(64), "enabled": true, "observe": true, "files": true, "generation": 3 } });
        std::fs::write(f.server.paths.authority(), paired.to_string()).unwrap();
        let viewer = || authority::load(&f.server.paths.authority()).unwrap()["vesper"]["viewer"].clone();
        let reply = authority::viewer_register(&f.server, "vesper", &register(&a)).await.unwrap();
        assert_eq!((reply["viewer_cert_sha256"].clone(), reply["replaced"].clone()), (json!(a), json!(false)));
        assert_eq!(viewer(), json!({ "cert_sha256": a, "generation": 3 }));
        authority::viewer_register(&f.server, "vesper", &register(&a)).await.unwrap();
        assert!(!f.calls().iter().any(|c| c.starts_with("revoke_viewer")), "the same viewer keeps its stream: {:?}", f.calls());

        let reply = authority::viewer_register(&f.server, "vesper", &register(&b)).await.unwrap();
        assert_eq!(reply["replaced"], true);
        assert_eq!(viewer(), json!({ "cert_sha256": b, "generation": 3 }));
        assert_eq!(f.calls().last().map(String::as_str), Some("revoke_viewer vesper"));
    });
}
