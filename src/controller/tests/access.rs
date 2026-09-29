//! Access failure cases: projection, settlement, owner recovery, how long an
//! approval to watch lasts, and pausing or resuming agents without the right
//! to control this computer. The root access helper is a fake socket.

use super::*;
use crate::access::{Access, CAPABILITIES, Grant, Identity, Pairing, Rule};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// The root access helper: records each projection and answers as told.
struct Helper {
    seen: Rc<RefCell<Vec<Value>>>,
    fail: Rc<Cell<bool>>,
}

/// A rig whose access model has the local owner and the paired computer
/// `vesper` (generation 3) with `grants`, projecting to a fake helper.
fn access_rig(viewer: bool, grants: &[(&str, Rule)]) -> (Rig, Helper) {
    let mut rig = rig(viewer);
    let path = rig.dir.join("a.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("helper socket");
    Rc::get_mut(&mut rig.controller).expect("unshared controller").access_socket = path;
    let helper = Helper { seen: Rc::default(), fail: Rc::default() };
    let (seen, fail) = (helper.seen.clone(), helper.fail.clone());
    tokio::task::spawn_local(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(read).read_line(&mut line).await.is_err() {
                continue;
            }
            seen.borrow_mut().push(serde_json::from_str(&line).unwrap_or(Value::Null));
            let reply = if fail.get() { "{\"ok\":false}\n" } else { "{\"ok\":true}\n" };
            let _ = write.write_all(reply.as_bytes()).await;
        }
    });
    let mut a = Access::empty();
    a.identities.insert("owner".into(), Identity { kind: "person".into(), computer: "owner".into(), key: "admin-sha256:0".into() });
    for cap in CAPABILITIES {
        a.import_grant("owner", cap, true, None, BTreeMap::new());
    }
    a.identities.insert("vesper".into(), Identity { kind: "computer".into(), computer: "vesper".into(), key: "SHA256:vesper".into() });
    a.pairings.insert("vesper".into(), Pairing { key: "SHA256:vesper".into(), endpoint: None, active: true, generation: 3 });
    for (cap, rule) in grants {
        let grant = Grant { subject: "vesper".into(), capability: cap.to_string(), rule: *rule, expires_at: None, effects: BTreeMap::new() };
        a.grants.insert(format!("vesper:{cap}"), grant);
    }
    a.save(&rig.controller.journal, "owner", "Test access").unwrap();
    (rig, helper)
}

fn model(c: &Controller) -> Access {
    Access::load(&c.journal).unwrap().expect("access model")
}

/// `computerctl access_set` at the current revision.
fn set(c: &Controller, fields: Value) -> Value {
    let mut action = json!({ "op": "access_set", "expected_revision": model(c).revision });
    action.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    action
}

#[test]
fn a_projection_that_failed_partway_is_sent_again_when_the_same_access_returns() {
    run(async {
        let (rig, helper) = access_rig(false, &[("watch", Rule::Allow)]);
        let c = &rig.controller;
        c.admin(json!({ "op": "access_sync" })).await.expect("first projection");
        let enabled = helper.seen.borrow().last().cloned().expect("projected");
        assert_eq!(enabled["peers"]["vesper"]["operator"], true, "{enabled}");
        helper.fail.set(true);
        let deny = set(c, json!({ "subject": "vesper", "capability": "watch", "rule": "deny" }));
        let failed = c.admin(deny).await.unwrap_err();
        assert_eq!(failed.details["access_saved"], true, "{failed:?}");
        helper.fail.set(false);
        let sent = helper.seen.borrow().len();
        let allow = set(c, json!({ "subject": "vesper", "capability": "watch", "rule": "allow" }));
        c.admin(allow).await.expect("restored access");
        let seen = helper.seen.borrow();
        assert!(seen.len() > sent, "the failed helper may have changed some peers, so the restored access must reach it again");
        assert_eq!(seen.last(), Some(&enabled));
    });
}

#[test]
fn the_owner_keeps_a_permanent_administer_grant() {
    run(async {
        let (rig, _helper) = access_rig(false, &[("watch", Rule::Allow)]);
        let c = &rig.controller;
        let refused = [
            json!({ "subject": "owner", "capability": "administer", "rule": "allow", "expires_at": "2099-01-01T00:00:00Z" }),
            json!({ "subject": "owner", "capability": "administer", "rule": "deny", "grant_id": "owner:pause", "expires_at": "2099-01-01T00:00:00Z" }),
        ];
        for fields in refused {
            let err = c.admin(set(c, fields.clone())).await.expect_err("the owner would lose recovery");
            assert_eq!(err.code, "PERMISSION_DENIED", "{fields}: {err:?}");
        }
        let beside = json!({ "subject": "owner", "capability": "administer", "rule": "allow", "grant_id": "owner:extra", "expires_at": "2099-01-01T00:00:00Z" });
        c.admin(set(c, beside)).await.expect("an expiring grant beside the permanent one");
        rig.clock.store(4_100_000_000_000, Ordering::SeqCst);
        let later = set(c, json!({ "subject": "vesper", "capability": "files", "rule": "allow" }));
        c.admin(later).await.expect("the owner still administers after every expiry");
    });
}

#[test]
fn a_viewer_revocation_that_fails_blocks_neither_the_saved_edit_nor_later_calls() {
    run(async {
        let (mut rig, _helper) = access_rig(true, &[("watch", Rule::Allow), ("control", Rule::Allow), ("agents", Rule::Allow)]);
        Rc::get_mut(&mut rig.controller).expect("unshared controller").timing = Timing { drain: Duration::ZERO };
        let c = &rig.controller;
        c.viewer_state.borrow_mut().owner = Some("vesper".into());
        // The stream hangs: its revoke never answers.
        rig.stream.0.failures.set(u32::MAX);
        let deny = set(c, json!({ "subject": "vesper", "capability": "control", "rule": "deny" }));
        let err = c.admin(deny).await.expect_err("the viewer could not be revoked");
        assert_eq!(err.details.get("access_saved"), Some(&json!(true)), "the denial is saved: {err:?}");
        assert_eq!(model(c).rule("vesper", "control", c.now_ms()), Rule::Deny);
        assert!(c.journal.get_control().unwrap().human_control, "the desktop stays fenced");
        c.admin(json!({ "op": "status" })).await.expect("administration still answers");
        let status = call(c, "computer_status", json!({})).await;
        assert_eq!(status["status"], "ok", "agent calls still answer: {status}");
    });
}

/// Failure cases: a computer denied control pauses or resumes agents anyway;
/// an ask-first rule pauses before a person approves; a stale binding or an
/// unpaired computer is obeyed; the pause is not recorded as a person's.
#[test]
fn pausing_and_resuming_agents_follows_the_control_rule() {
    run(async {
        let (rig, _helper) = access_rig(false, &[("control", Rule::Deny)]);
        let c = &rig.controller;
        let err = c.operator_call("vesper", operator_action(c, "pause")).await.unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED", "{err:?}");
        assert!(!c.journal.get_control().unwrap().paused);

        let (rig, _helper) = access_rig(false, &[("control", Rule::Ask)]);
        let c = &rig.controller;
        let held = c.operator_call("vesper", operator_action(c, "pause")).await.unwrap();
        assert_eq!(held["state"], "pending_approval", "{held}");
        assert!(!c.journal.get_control().unwrap().paused, "held until a person approves");

        let (rig, _helper) = access_rig(false, &[("control", Rule::Allow)]);
        let c = &rig.controller;
        let mut stale = operator_action(c, "pause");
        stale["controller_epoch"] = json!("epoch_old");
        assert_eq!(c.operator_call("vesper", stale).await.unwrap_err().code, "PERMISSION_DENIED");
        assert_eq!(c.operator_call("hazel", operator_action(c, "pause")).await.unwrap_err().code, "PERMISSION_DENIED");
        assert!(!c.journal.get_control().unwrap().paused);
        let paused = c.operator_call("vesper", operator_action(c, "pause")).await.unwrap();
        assert_eq!((paused["paused"].clone(), paused["pause_origin"].clone()), (json!(true), json!("person")), "{paused}");
        assert_eq!(c.journal.get_control().unwrap().pause_origin, Some(crate::store::PauseOrigin::Person));
        assert_eq!(c.operator_call("hazel", operator_action(c, "resume")).await.unwrap_err().code, "PERMISSION_DENIED");
        let resumed = c.operator_call("vesper", operator_action(c, "resume")).await.unwrap();
        assert_eq!(resumed["paused"], false, "{resumed}");
        assert_eq!(resumed["controller_epoch"], json!(c.epoch()));
    });
}

/// Failure cases: a ticket is minted, or the computer paused, before a person
/// approves an ask-first take-control, or for a computer denied control.
#[test]
fn take_control_waits_for_approval_and_a_denial_mints_no_ticket() {
    run(async {
        let (rig, _helper) = access_rig(true, &[("control", Rule::Ask)]);
        let c = &rig.controller;
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        let mut take = operator_action(c, "take_control");
        take["expected_owner"] = status["owner"].clone();
        take["expected_ownership_revision"] = status["ownership_revision"].clone();
        let held = c.operator_call("vesper", take.clone()).await.unwrap();
        assert_eq!(held["state"], "pending_approval", "{held}");
        assert!(rig.stream.calls().is_empty(), "no stream before approval");
        assert!(!c.journal.get_control().unwrap().paused, "held until a person approves");
        c.journal.answer_attention(held["attention"].as_str().unwrap(), "approve", "owner", &c.now_iso()).unwrap();
        let taken = c.operator_call("vesper", take).await.unwrap();
        assert!(taken["stream"]["ticket"].is_string(), "approved: {taken}");
        assert_eq!(rig.stream.calls().last().map(String::as_str), Some("ticket 1 c0ffee"));

        let (rig, _helper) = access_rig(true, &[("control", Rule::Deny)]);
        let c = &rig.controller;
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        assert_eq!(status["interactive_control"], "unsupported_without_verified_viewer_adapter");
        let mut take = operator_action(c, "take_control");
        take["expected_owner"] = status["owner"].clone();
        take["expected_ownership_revision"] = status["ownership_revision"].clone();
        assert_eq!(c.operator_call("vesper", take).await.unwrap_err().code, "PERMISSION_DENIED");
        assert!(rig.stream.calls().is_empty());
    });
}

/// Failure cases: an approval to watch never ends, or it ends while the
/// computer is still watching (a picture every few minutes).
#[test]
fn an_approval_to_watch_lasts_until_five_minutes_without_watching() {
    run(async {
        let (rig, _helper) = access_rig(false, &[("watch", Rule::Ask)]);
        let c = &rig.controller;
        let picture = || {
            let mut action = operator_action(c, "observe");
            action["display_id"] = json!("HDMI-A-1");
            action["quality"] = json!("tile");
            c.operator_call("vesper", action)
        };
        let asked = picture().await.unwrap();
        assert_eq!(asked["state"], "pending_approval", "{asked}");
        c.journal.answer_attention(asked["attention"].as_str().unwrap(), "approve", "owner", &c.now_iso()).unwrap();
        // The fake desktop has no picture to give: an error past the gate.
        let start = rig.clock.load(Ordering::SeqCst);
        for minutes in [0, 4, 8, 12] {
            rig.clock.store(start + minutes * 60_000, Ordering::SeqCst);
            let passed = picture().await.expect_err("past the gate");
            assert_eq!(passed.message, "no preview", "minute {minutes}: {passed:?}");
        }
        rig.clock.store(start + 17 * 60_000, Ordering::SeqCst);
        let again = picture().await.unwrap();
        assert_eq!(again["state"], "pending_approval", "five minutes without watching end it: {again}");
    });
}

/// Where agents may only work with approval, a task waits for a person before
/// it begins. Failure cases: the pending begin says nothing on what to do in
/// either form, or points at a wait the agent cannot make without a task;
/// sent again once approved, it does not begin.
#[test]
fn a_begin_waiting_for_approval_says_to_send_it_again() {
    run(async {
        let (rig, _helper) = access_rig(false, &[("agents", Rule::Ask)]);
        let c = &rig.controller;
        let args = json!({ "goal": "Save a note in the editor", "request_id": "b-1" });
        let outcome = c.call("vesper", "connection_a", "codex", "computer_begin", args.clone(), Cancel::new()).await;
        let held = &outcome.envelope;
        assert_eq!(held["status"], "pending", "{held}");
        let att = held["result"]["attention"].as_str().unwrap();
        let next = held["result"]["next"].as_str().unwrap_or_else(|| panic!("a waiting begin says what to do next: {held}"));
        assert!(next.contains(att) && next.contains("send this same computer_begin request again (same request_id)"), "{next}");
        assert!(next.contains("Don't end your turn or ask your user") && !next.contains("computer_wait"), "{next}");
        let text = crate::mcp::call_result(&outcome)["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.lines().any(|line| line == format!("next: {next}")), "the text form carries the whole next: {text}");

        c.journal.answer_attention(att, "approve", "owner", &c.now_iso()).unwrap();
        let begun = call(c, "computer_begin", args).await;
        assert_eq!(begun["status"], "ok", "sent again once approved, it begins: {begun}");
    });
}

/// One live video read of 1920×1080 by `vesper`.
async fn video(c: &Controller, cursor: Value) -> crate::error::Result<Value> {
    let mut action = operator_action(c, "observe_video");
    action["width"] = json!(1920);
    action["height"] = json!(1080);
    action["cursor"] = cursor;
    c.operator_call("vesper", action).await
}

/// Live video is watched under the preview's grant, checked on every read.
/// Failure cases: a computer denied watching, one still waiting for approval,
/// one whose watching was revoked between reads, one with a stale grant, or
/// any while a person holds control or the screen is locked, gets bytes, or
/// even starts the encoder.
#[test]
fn live_video_is_refused_without_bytes_whenever_watching_is() {
    run(async {
        let (rig, _helper) = access_rig(false, &[("watch", Rule::Deny)]);
        assert_eq!(video(&rig.controller, Value::Null).await.unwrap_err().code, "PERMISSION_DENIED");
        assert_eq!(rig.desktop.video_reads.get(), 0, "denied");

        let (rig, _helper) = access_rig(false, &[("watch", Rule::Ask)]);
        let asked = video(&rig.controller, Value::Null).await.unwrap();
        assert_eq!(asked["state"], "pending_approval", "{asked}");
        assert!(asked.get("data").is_none() && rig.desktop.video_reads.get() == 0, "waiting for approval: {asked}");

        let (rig, _helper) = access_rig(false, &[("watch", Rule::Allow)]);
        let c = &rig.controller;
        let first = video(c, Value::Null).await.unwrap();
        assert_eq!((first["data"].as_str(), first["cursor"].as_u64()), (Some("R2FiYw=="), Some(4)), "{first}");
        assert_eq!((first["width"].as_u64(), first["height"].as_u64()), (Some(640), Some(360)), "clamped: {first}");
        assert_eq!(first["authorization_generation"], json!(3));

        let mut stale = operator_action(c, "observe_video");
        stale["width"] = json!(640);
        stale["height"] = json!(360);
        stale["expected_authorization_generation"] = json!(2);
        assert_eq!(c.operator_call("vesper", stale).await.unwrap_err().code, "PERMISSION_DENIED");

        c.viewer_state.borrow_mut().owner = Some("vesper".into());
        let held = video(c, first["cursor"].clone()).await.unwrap_err();
        assert_eq!(held.details["reason"], "control_held", "{held:?}");
        c.viewer_state.borrow_mut().owner = None;

        rig.desktop.session.set(false);
        assert_eq!(video(c, first["cursor"].clone()).await.unwrap_err().code, "CAPABILITY_UNAVAILABLE");
        rig.desktop.session.set(true);

        let mut revoked = model(c);
        revoked.grants.get_mut("vesper:watch").unwrap().rule = Rule::Deny;
        revoked.save(&c.journal, "owner", "Stop watching").unwrap();
        assert_eq!(video(c, first["cursor"].clone()).await.unwrap_err().code, "PERMISSION_DENIED", "revoked between reads");
        assert_eq!(rig.desktop.video_reads.get(), 1, "only the allowed read reached the encoder");
    });
}
