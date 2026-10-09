//! Journal behaviour tests. Failure cases, written first:
//! - a retried request dispatches again instead of replaying its receipt
//! - a reused request_id with changed arguments replays instead of conflicting
//!   in its session, or conflicts in a later session once it has finished
//! - a later session's new request under an old id corrupts an unfinished
//!   one, or stops a retry of the old request from replaying
//! - old mutation rows (request_id inside the fingerprint, or no session) stop replaying
//! - a failed begin's replay answers something other than its original error
//! - a restart leaves dispatched non-job receipts `running`, or touches job-backed ones
//! - retention deletes unresolved operations, failed begins, or retained tasks' events
//! - a new intent over the metadata cap is written instead of refused
//! - an unknown schema version is modified instead of refused
//! - opening a real journal changes endpoint identity, the fingerprint secret or
//!   the base schema version, or loses rows
//! - stored fingerprints on real journals do not recompute
//! - a journal paused before pause origins existed reads as nobody's or a
//!   person's pause, the core version is not raised, or a second open fails
//! - a restart during a lease replaces a person's pause with the system's
//! - a journal from before task windows (every computer on core schema 5)
//!   cannot remember which windows a task opened

use super::*;
use crate::ids::{id, iso_from_millis};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const T0: i64 = 1_790_000_000_000;

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(id("ibara-store-test"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_at(dir: &Path, clock: Arc<AtomicI64>) -> Journal {
    let c = clock.clone();
    Journal::open(dir, JournalOptions { now: Some(Arc::new(move || c.load(Ordering::SeqCst))), ..Default::default() }).unwrap()
}

fn open(dir: &Path) -> (Journal, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(T0));
    (open_at(dir, clock.clone()), clock)
}

fn receipt(request_id: &str, task_ref: &str) -> Value {
    json!({ "kind": "receipt", "request_id": request_id, "operation_ref": "pending", "task_ref": task_ref, "epoch": "epoch_x",
            "execution": "not_started", "verification": "not_requested", "effect": "none", "evidence_refs": [], "summary": "intent" })
}

fn intent<'a>(principal: &'a str, request_id: &'a str, task_ref: Option<&'a str>, tool: &'a str, args: &'a Value, receipt: &'a Value) -> RememberIntent<'a> {
    RememberIntent {
        principal,
        request_id,
        task_ref,
        tool,
        args_fingerprint_source: args,
        receipt,
        now_iso: "2026-09-25T00:00:00.000Z",
        recovery_operation_ref: None,
        effect_class: "change",
        session: "connection_1",
    }
}

fn task(task_ref: &str, state: &str, updated_at: &str) -> TaskRecord {
    TaskRecord {
        task_ref: task_ref.into(),
        principal: "vesper".into(),
        created_at: updated_at.into(),
        updated_at: updated_at.into(),
        state: state.into(),
        goal: "g".into(),
        success_criteria: vec![],
        budgets: json!({}),
        client_flags: json!({}),
        authorization_ref: None,
        required_capabilities: vec![],
        control_started_ms: None,
        last_charge_ms: None,
        active_control_used_ms: 0,
        actions_used: 0,
        images_used: 0,
        last_checkpoint_ref: None,
        completion: None,
        contract_version: "3.0".into(),
        visibility: "private".into(),
        owner_group: None,
        deliveries: vec![],
    }
}

fn fresh(r: Remembered) -> OperationRecord {
    match r {
        Remembered::Fresh(op) => op,
        other => panic!("expected a fresh intent, got {other:?}"),
    }
}

#[test]
fn replay_of_the_same_request_returns_the_original_receipt() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "task_ref": "task_a", "action": { "kind": "type", "text": "hi" } });
    let first = fresh(j.remember_intent(intent("vesper", "req-1", Some("task_a"), "computer_act", &args, &receipt("req-1", "task_a"))).unwrap());
    let done = json!({ "kind": "receipt", "request_id": "req-1", "operation_ref": first.operation_ref, "execution": "completed", "verification": "not_requested", "effect": "local_change" });
    j.update_operation(&first.operation_ref, OperationPatch { receipt: Some(done.clone()), dispatched: Some(true), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();
    // Same arguments in a different key order.
    let again = json!({ "action": { "text": "hi", "kind": "type" }, "task_ref": "task_a" });
    match j.remember_intent(intent("vesper", "req-1", Some("task_a"), "computer_act", &again, &receipt("req-1", "task_a"))).unwrap() {
        Remembered::Replay(op) => {
            assert_eq!(op.operation_ref, first.operation_ref);
            assert_eq!(op.receipt, done);
            assert!(op.dispatched);
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

#[test]
fn reused_request_id_with_changed_arguments_in_the_same_session_is_a_request_conflict() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "task_ref": "task_a", "command": "ls" });
    fresh(j.remember_intent(intent("vesper", "req-1", Some("task_a"), "computer_exec", &args, &receipt("req-1", "task_a"))).unwrap());
    let changed = json!({ "task_ref": "task_a", "command": "rm -rf ~" });
    let err = j.remember_intent(intent("vesper", "req-1", Some("task_a"), "computer_exec", &changed, &receipt("req-1", "task_a"))).unwrap_err();
    assert_eq!(err.code, "REQUEST_CONFLICT");
    assert!(!err.retry_safe);
    // A different principal or task is a different request.
    fresh(j.remember_intent(intent("hazel", "req-1", Some("task_a"), "computer_exec", &changed, &receipt("req-1", "task_a"))).unwrap());
    fresh(j.remember_intent(intent("vesper", "req-1", Some("task_b"), "computer_exec", &changed, &receipt("req-1", "task_b"))).unwrap());
}

#[test]
fn legacy_fingerprints_that_included_request_id_still_replay() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "task_ref": "task_a", "command": "ls" });
    let op = fresh(j.remember_intent(intent("vesper", "req-old", Some("task_a"), "computer_exec", &args, &receipt("req-old", "task_a"))).unwrap());
    // Rewrite the row as an older controller stored it.
    let legacy = j.fingerprint(&json!({ "tool": "computer_exec", "args": { "task_ref": "task_a", "command": "ls", "request_id": "req-old" } }));
    j.db().execute("UPDATE operations SET fingerprint = ? WHERE operation_ref = ?", [&legacy, &op.operation_ref]).unwrap();
    assert!(matches!(
        j.remember_intent(intent("vesper", "req-old", Some("task_a"), "computer_exec", &args, &receipt("req-old", "task_a"))).unwrap(),
        Remembered::Replay(_)
    ));
    let changed = json!({ "task_ref": "task_a", "command": "pwd" });
    assert_eq!(
        j.remember_intent(intent("vesper", "req-old", Some("task_a"), "computer_exec", &changed, &receipt("req-old", "task_a"))).unwrap_err().code,
        "REQUEST_CONFLICT"
    );
}

fn in_session<'a>(session: &'a str, intent: RememberIntent<'a>) -> RememberIntent<'a> {
    RememberIntent { session, ..intent }
}

/// Store `receipt` on `op`, as a call does when it ends.
fn settle(j: &Journal, op: &OperationRecord, receipt: Value, dispatched: bool) {
    j.update_operation(&op.operation_ref, OperationPatch { receipt: Some(receipt), dispatched: Some(dispatched), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();
}

/// A request whose whole answer was stored.
fn finished() -> Value {
    json!({ "kind": "receipt", "execution": "completed", "verification": "not_requested", "effect": "local_change", "call": { "status": "ok", "result": {} } })
}

#[test]
fn a_finished_request_id_is_a_new_request_in_a_later_session_and_its_retry_still_replays() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "task_ref": "task_a", "action": { "kind": "key", "keys": "ctrl+s" } });
    let act = |session: &str, args: &Value| j.remember_intent(in_session(session, intent("vesper", "act-1", Some("task_a"), "computer_act", args, &receipt("act-1", "task_a"))));
    let step = json!({ "index": 1, "step": { "kind": "key", "keys": "Return" } });
    let step_receipt = receipt("act-1#1", "task_a");
    let step_intent = |session: &'static str| in_session(session, intent("vesper", "act-1#1", Some("task_a"), "computer_act", &step, &step_receipt));
    let first = fresh(act("connection_1", &args).unwrap());
    let first_step = fresh(j.remember_intent(step_intent("connection_1")).unwrap());
    settle(&j, &first, finished(), true);
    settle(&j, &first_step, finished(), true);

    let changed = json!({ "task_ref": "task_a", "action": { "kind": "key", "keys": "ctrl+q" } });
    let second = fresh(act("connection_2", &changed).unwrap());
    assert_eq!(second.session.as_deref(), Some("connection_2"));
    assert_eq!(j.get_mutation_operation("vesper", "task_a", "act-1").unwrap().unwrap().operation_ref, second.operation_ref);
    // The first request moved aside with its later step, so the new act's steps start clean.
    let retired = format!("act-1~{}", first.operation_ref);
    assert_eq!(j.get_operation_by_ref(&first.operation_ref).unwrap().unwrap().request_id, retired);
    assert_eq!(j.get_operation_by_ref(&first_step.operation_ref).unwrap().unwrap().request_id, format!("{retired}#1"));
    assert_eq!(j.get_operation_by_ref(&first.operation_ref).unwrap().unwrap().receipt["execution"], "completed", "its receipt is kept");
    fresh(j.remember_intent(step_intent("connection_2")).unwrap());

    // A retry of either request replays it, from any session.
    for session in ["connection_1", "connection_2", "connection_3"] {
        for (sent, original) in [(&args, &first), (&changed, &second)] {
            match act(session, sent).unwrap() {
                Remembered::Replay(op) => assert_eq!(op.operation_ref, original.operation_ref, "{session}"),
                other => panic!("{session}: expected a replay, got {other:?}"),
            }
        }
    }
    // Both sessions that used act-1 have used it up for other arguments.
    let third = json!({ "task_ref": "task_a", "action": { "kind": "key", "keys": "ctrl+w" } });
    for session in ["connection_1", "connection_2"] {
        let err = act(session, &third).unwrap_err();
        assert_eq!((err.code, err.message.as_str()), ("REQUEST_CONFLICT", "This request_id was already used in this session with different arguments."), "{session}");
    }
}

#[test]
fn a_begin_id_is_a_new_request_in_a_later_session_once_its_begin_finished() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "goal": "Write" });
    let begin = |session: &str, args: &Value| j.remember_intent(in_session(session, intent("vesper", "begin-1", None, "computer_begin", args, &receipt("begin-1", "task_pending"))));
    let first = fresh(begin("connection_1", &args).unwrap());
    let changed = json!({ "goal": "Something else" });
    // Still beginning: its task is not known yet.
    let err = begin("connection_2", &changed).unwrap_err();
    assert_eq!((err.code, err.message.as_str()), ("REQUEST_CONFLICT", "An earlier request with this request_id and different arguments has not finished yet."));
    j.update_operation(&first.operation_ref, OperationPatch { task_ref: Some(Some("task_a".into())), receipt: Some(finished()), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();

    let second = fresh(begin("connection_2", &changed).unwrap());
    assert_eq!(j.get_begin_operation("vesper", "begin-1").unwrap().unwrap().operation_ref, second.operation_ref);
    match begin("connection_3", &args).unwrap() {
        Remembered::Replay(op) => assert_eq!(op.operation_ref, first.operation_ref),
        other => panic!("expected the first begin's replay, got {other:?}"),
    }
    assert_eq!(begin("connection_1", &json!({ "goal": "A third thing" })).unwrap_err().code, "REQUEST_CONFLICT");
}

#[test]
fn a_request_from_before_sessions_counts_as_another_sessions() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "task_ref": "task_a", "command": "ls" });
    let exec = |args: &Value| j.remember_intent(intent("vesper", "req-old", Some("task_a"), "computer_exec", args, &receipt("req-old", "task_a")));
    let op = fresh(exec(&args).unwrap());
    settle(&j, &op, finished(), true);
    // As an older build stored it: no session, and request_id inside the fingerprint.
    let legacy = j.fingerprint(&json!({ "tool": "computer_exec", "args": { "task_ref": "task_a", "command": "ls", "request_id": "req-old" } }));
    j.db().execute("UPDATE operations SET session = NULL, fingerprint = ? WHERE operation_ref = ?", [&legacy, &op.operation_ref]).unwrap();
    assert!(j.get_operation_by_ref(&op.operation_ref).unwrap().unwrap().session.is_none());

    let changed = json!({ "task_ref": "task_a", "command": "pwd" });
    let new = fresh(exec(&changed).unwrap());
    assert_eq!(new.session.as_deref(), Some("connection_1"));
    match exec(&args).unwrap() {
        Remembered::Replay(replayed) => assert_eq!(replayed.operation_ref, op.operation_ref, "the old request still replays"),
        other => panic!("expected the old request's replay, got {other:?}"),
    }
}

#[test]
fn an_unfinished_request_keeps_its_id_in_every_session() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let call = json!({ "status": "ok", "result": {} });
    let cases = [
        ("intent-only", None, false, None),
        ("running", Some(json!({ "execution": "running", "verification": "not_requested", "effect": "unknown", "call": call })), true, None),
        ("unknown", Some(json!({ "execution": "unknown", "verification": "unknown", "effect": "unknown", "call": { "status": "error", "error": { "code": "OUTCOME_UNKNOWN" } } })), true, None),
        (
            "held",
            Some(json!({ "execution": "not_started", "verification": "not_requested", "effect": "none", "held": { "attention": "att_1", "index": 0 },
                         "call": { "status": "pending", "result": {}, "held": { "index": 0, "attention": "att_1" } } })),
            false,
            None,
        ),
        ("later-step-unknown", Some(finished()), true, Some(json!({ "execution": "unknown", "verification": "unknown", "effect": "unknown" }))),
    ];
    let args = json!({ "task_ref": "task_a", "command": "ls" });
    let changed = json!({ "task_ref": "task_a", "command": "pwd" });
    for (request_id, stored, dispatched, step) in cases {
        let op = fresh(j.remember_intent(intent("vesper", request_id, Some("task_a"), "computer_exec", &args, &receipt(request_id, "task_a"))).unwrap());
        if let Some(stored) = stored {
            settle(&j, &op, stored, dispatched);
        }
        if let Some(step) = step {
            let later = format!("{request_id}#1");
            let step_op = fresh(j.remember_intent(intent("vesper", &later, Some("task_a"), "computer_exec", &json!({ "index": 1 }), &receipt(&later, "task_a"))).unwrap());
            settle(&j, &step_op, step, true);
        }
        let before = j.get_operation_by_ref(&op.operation_ref).unwrap().unwrap();
        let err = j.remember_intent(in_session("connection_2", intent("vesper", request_id, Some("task_a"), "computer_exec", &changed, &receipt(request_id, "task_a")))).unwrap_err();
        assert_eq!(err.code, "REQUEST_CONFLICT", "{request_id}");
        assert!(err.message.contains("not finished"), "{request_id}: {}", err.message);
        assert_eq!(j.get_operation_by_ref(&op.operation_ref).unwrap().unwrap(), before, "{request_id}: the unfinished request is untouched");
    }
}

#[test]
fn failed_begin_replay_returns_the_original_error() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let args = json!({ "goal": "g", "success_criteria": [], "resume_task_ref": null, "contract_version": "3.0" });
    let op = fresh(j.remember_intent(intent("vesper", "begin-1", None, "computer_begin", &args, &receipt("begin-1", "task_pending"))).unwrap());
    let mut failed = receipt("begin-1", "task_none");
    failed["error"] = json!({ "code": "HUMAN_CONTROL", "message": "A human currently owns the desktop.", "retry_safe": false, "requires_reconciliation": false, "recovery": "Wait for operator resume." });
    j.update_operation(&op.operation_ref, OperationPatch { receipt: Some(failed), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();
    match j.remember_intent(intent("vesper", "begin-1", None, "computer_begin", &args, &receipt("begin-1", "task_pending"))).unwrap() {
        Remembered::FailedBeginReplay { operation, error } => {
            assert_eq!(operation.operation_ref, op.operation_ref);
            assert_eq!(error.code, "HUMAN_CONTROL");
            assert_eq!(error.message, "A human currently owns the desktop.");
            assert_eq!(error.details.get("recovery"), Some(&json!("Wait for operator resume.")));
        }
        other => panic!("expected the original begin error, got {other:?}"),
    }
    // A begin that created its task replays normally.
    let ok = fresh(j.remember_intent(intent("vesper", "begin-2", None, "computer_begin", &args, &receipt("begin-2", "task_pending"))).unwrap());
    j.update_operation(&ok.operation_ref, OperationPatch { task_ref: Some(Some("task_new".into())), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();
    assert!(matches!(
        j.remember_intent(intent("vesper", "begin-2", None, "computer_begin", &args, &receipt("begin-2", "task_pending"))).unwrap(),
        Remembered::Replay(_)
    ));
}

#[test]
fn restart_turns_dispatched_non_job_running_receipts_unknown() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    j.put_task(&task("task_a", "active", "2026-09-25T00:00:00.000Z")).unwrap();
    j.put_lease(&LeaseRecord {
        generation: "lease_1".into(),
        task_ref: "task_a".into(),
        principal: "vesper".into(),
        connection_id: "c1".into(),
        epoch: j.get_epoch().unwrap(),
        acquired_at: "2026-09-25T00:00:00.000Z".into(),
        last_heartbeat_at: "2026-09-25T00:00:00.000Z".into(),
        last_heartbeat_ms: T0,
        idle_expires_at_ms: T0 + 300_000,
        state: "active".into(),
        reason: None,
    })
    .unwrap();
    let running = |request_id: &str, job_ref: Option<&str>| {
        let args = json!({ "task_ref": "task_a", "n": request_id });
        let op = fresh(j.remember_intent(intent("vesper", request_id, Some("task_a"), "computer_exec", &args, &receipt(request_id, "task_a"))).unwrap());
        let mut r = json!({ "kind": "receipt", "request_id": request_id, "execution": "running", "verification": "not_requested", "effect": "local_change" });
        if let Some(job) = job_ref {
            r["job_ref"] = json!(job);
        }
        j.update_operation(&op.operation_ref, OperationPatch { receipt: Some(r), dispatched: Some(true), ..OperationPatch::at("2026-09-25T00:00:01.000Z") }).unwrap();
        op.operation_ref
    };
    let plain = running("r1", None);
    let job = running("r2", Some("job_1"));
    // Not dispatched: an intent that never ran stays as it was.
    let intent_only = fresh(
        j.remember_intent(intent("vesper", "r3", Some("task_a"), "computer_act", &json!({ "task_ref": "task_a" }), &receipt("r3", "task_a"))).unwrap(),
    );
    j.put_observation("task_a", "vesper", &json!({ "observation_ref": "obs_1", "epoch": j.get_epoch().unwrap() }), false).unwrap();
    let old_epoch = j.get_epoch().unwrap();

    let epoch = j.rotate_epoch("2026-09-25T01:00:00.000Z").unwrap();
    assert_ne!(epoch, old_epoch);
    assert_eq!(j.get_epoch().unwrap(), epoch);
    let plain = j.get_operation_by_ref(&plain).unwrap().unwrap();
    assert_eq!(plain.receipt["execution"], "unknown");
    assert_eq!(plain.receipt["verification"], "unknown");
    assert_eq!(plain.receipt["effect"], "unknown");
    assert_eq!(plain.receipt["error"]["code"], "OUTCOME_UNKNOWN");
    assert_eq!(plain.receipt["error"]["requires_reconciliation"], true);
    assert_eq!(j.get_operation_by_ref(&job).unwrap().unwrap().receipt["execution"], "running");
    assert_eq!(j.get_operation_by_ref(&intent_only.operation_ref).unwrap().unwrap().receipt["execution"], "not_started");
    assert!(j.get_active_lease().unwrap().is_none());
    assert_eq!(j.get_lease("lease_1").unwrap().unwrap().reason.as_deref(), Some("controller_restart"));
    let t = j.get_task("task_a").unwrap().unwrap();
    assert_eq!(t.state, "interrupted");
    assert_eq!(t.last_charge_ms, None);
    let control = j.get_control().unwrap();
    assert!(control.human_control && control.paused && control.unsettled);
    assert!(j.get_observation("obs_1").unwrap().unwrap().expired);
}

#[test]
fn retention_never_prunes_unresolved_operations() {
    let dir = tempdir();
    let (j, clock) = open(&dir);
    let old = iso_from_millis(T0 - 40 * DAY_MS);
    let op = |task_ref: &str, request_id: &str, execution: &str, dispatched: bool| {
        let args = json!({ "task_ref": task_ref, "id": request_id });
        let o = fresh(j.remember_intent(intent("vesper", request_id, Some(task_ref), "computer_act", &args, &receipt(request_id, task_ref))).unwrap());
        let r = json!({ "execution": execution, "verification": "not_requested", "effect": if execution == "completed" { "local_change" } else { "unknown" } });
        j.update_operation(&o.operation_ref, OperationPatch { receipt: Some(r), dispatched: Some(dispatched), ..OperationPatch::at(old.clone()) }).unwrap();
        o.operation_ref
    };
    // Settled and old: pruned.
    j.put_task(&task("task_done", "completed", &old)).unwrap();
    let settled = op("task_done", "a", "completed", true);
    j.put_check("task_done", "vesper", &json!({ "check_ref": "check_1" })).unwrap();
    j.append_event(NewEvent { at: &old, kind: "step", task_ref: Some("task_done"), actor: "vesper", summary: "s", data: &json!({}) }).unwrap();
    // Old and terminal, but with an unresolved operation: everything kept.
    j.put_task(&task("task_open", "completed", &old)).unwrap();
    let running = op("task_open", "b", "running", true);
    let unknown = op("task_open", "c", "unknown", true);
    j.put_check("task_open", "vesper", &json!({ "check_ref": "check_2" })).unwrap();
    j.append_event(NewEvent { at: &old, kind: "step", task_ref: Some("task_open"), actor: "vesper", summary: "s", data: &json!({}) }).unwrap();
    // Non-terminal tasks are never pruned.
    j.put_task(&task("task_blocked", "blocked", &old)).unwrap();
    let blocked = op("task_blocked", "d", "completed", true);
    // Failed begin (task_ref NULL): never pruned.
    let begin = fresh(j.remember_intent(intent("vesper", "b1", None, "computer_begin", &json!({}), &receipt("b1", "task_none"))).unwrap());

    clock.store(T0, Ordering::SeqCst);
    j.enforce_retention(false).unwrap();
    assert!(j.get_operation_by_ref(&settled).unwrap().is_none());
    assert!(j.get_check("check_1").unwrap().is_none());
    assert!(j.events_since(0, Some("task_done"), 10).unwrap().is_empty());
    for kept in [&running, &unknown, &blocked, &begin.operation_ref] {
        assert!(j.get_operation_by_ref(kept).unwrap().is_some(), "{kept} was pruned");
    }
    assert!(j.get_check("check_2").unwrap().is_some());
    assert_eq!(j.events_since(0, Some("task_open"), 10).unwrap().len(), 1);
    assert_eq!(j.unresolved_operations("task_open").unwrap().len(), 2);
    assert!(j.get_task("task_done").unwrap().is_some(), "task rows are kept forever");
}

#[test]
fn new_intent_over_the_metadata_cap_is_refused_but_replay_works() {
    let dir = tempdir();
    let clock = Arc::new(AtomicI64::new(T0));
    let c = clock.clone();
    let j = Journal::open(&dir, JournalOptions { now: Some(Arc::new(move || c.load(Ordering::SeqCst))), ..Default::default() }).unwrap();
    let args = json!({ "task_ref": "task_a" });
    let op = fresh(j.remember_intent(intent("vesper", "r1", Some("task_a"), "computer_act", &args, &receipt("r1", "task_a"))).unwrap());
    drop(j);
    let c = clock.clone();
    let j = Journal::open(&dir, JournalOptions { max_metadata_bytes: Some(1024), now: Some(Arc::new(move || c.load(Ordering::SeqCst))), ..Default::default() }).unwrap();
    let err = j.remember_intent(intent("vesper", "r2", Some("task_a"), "computer_act", &args, &receipt("r2", "task_a"))).unwrap_err();
    assert_eq!(err.code, "BUDGET_EXCEEDED");
    assert!(j.get_operation_by_request("vesper", "r2", Some("task_a")).unwrap().is_none());
    match j.remember_intent(intent("vesper", "r1", Some("task_a"), "computer_act", &args, &receipt("r1", "task_a"))).unwrap() {
        Remembered::Replay(o) => assert_eq!(o.operation_ref, op.operation_ref),
        other => panic!("expected replay, got {other:?}"),
    }
}

#[test]
fn unsupported_schema_is_refused_without_writing() {
    let dir = tempdir();
    let path = dir.join("journal.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO meta VALUES('schema_version','9');").unwrap();
    drop(db);
    let err = Journal::open(&dir, JournalOptions::default()).err().expect("refused");
    assert_eq!(err.code, "INTERNAL_ERROR");
    let db = Connection::open(&path).unwrap();
    let tables: i64 = db.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0)).unwrap();
    assert_eq!(tables, 2, "only meta and its autoindex exist");
    // A core version newer than this build is refused too.
    db.execute("UPDATE meta SET value = '2' WHERE key = 'schema_version'", []).unwrap();
    db.execute("INSERT INTO meta VALUES('core_schema_version', '99')", []).unwrap();
    drop(db);
    assert!(Journal::open(&dir, JournalOptions::default()).is_err());
}

#[test]
fn identity_secret_and_settlement_token_behave_as_before() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let identity = j.endpoint_identity().unwrap();
    assert!(identity.starts_with("ibara_") && identity.len() == 38);
    let fp = j.fingerprint(&json!({ "a": 1 }));
    drop(j);
    let (j, _) = open(&dir);
    assert_eq!(j.endpoint_identity().unwrap(), identity);
    assert_eq!(j.fingerprint(&json!({ "a": 1 })), fp);
    j.begin_settlement("settlement_1").unwrap();
    // A newer uncertain outcome cleared the token: completing the old one changes nothing.
    j.set_control(ControlPatch { settling_generation: Some(None), ..Default::default() }).unwrap();
    assert!(j.complete_settlement("settlement_1", false).unwrap().unsettled);
    j.begin_settlement("settlement_2").unwrap();
    assert!(!j.complete_settlement("settlement_2", false).unwrap().unsettled);
}

#[test]
fn a_journal_paused_before_pause_origins_migrates_as_the_systems_pause() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    j.set_control(ControlPatch { paused: Some(true), human_control: Some(true), ..Default::default() }).unwrap();
    drop(j);
    let db = Connection::open(dir.join("journal.sqlite")).unwrap();
    db.execute_batch("ALTER TABLE control_state DROP COLUMN pause_origin; UPDATE meta SET value = '4' WHERE key = 'core_schema_version';")
        .unwrap();
    drop(db);
    let (j, _) = open(&dir);
    assert_eq!(j.get_control().unwrap().pause_origin, Some(PauseOrigin::System));
    assert_eq!(meta(j.db(), "core_schema_version").as_deref(), Some("6"));

    // A person's pause survives both a reopen and a restart during a lease.
    j.set_control(ControlPatch { pause_origin: Some(Some(PauseOrigin::Person)), ..Default::default() }).unwrap();
    j.put_task(&task("task_a", "active", "2026-09-25T00:00:00.000Z")).unwrap();
    j.put_lease(&LeaseRecord {
        generation: "lease_a".into(),
        task_ref: "task_a".into(),
        principal: "vesper".into(),
        connection_id: "c".into(),
        epoch: j.get_epoch().unwrap(),
        acquired_at: "2026-09-25T00:00:00.000Z".into(),
        last_heartbeat_at: "2026-09-25T00:00:00.000Z".into(),
        last_heartbeat_ms: T0,
        idle_expires_at_ms: T0 + 300_000,
        state: "active".into(),
        reason: None,
    })
    .unwrap();
    drop(j);
    let (j, _) = open(&dir);
    j.rotate_epoch(&iso_from_millis(T0)).unwrap();
    let control = j.get_control().unwrap();
    assert!(control.paused && control.unsettled, "{control:?}");
    assert_eq!(control.pause_origin, Some(PauseOrigin::Person));
}

#[test]
fn a_journal_from_before_task_windows_remembers_them_once_it_migrates() {
    let dir = tempdir();
    drop(open(&dir));
    let db = Connection::open(dir.join("journal.sqlite")).unwrap();
    db.execute_batch("DROP TABLE task_windows; UPDATE meta SET value = '5' WHERE key = 'core_schema_version';").unwrap();
    drop(db);
    let (j, _) = open(&dir);
    assert_eq!(meta(j.db(), "core_schema_version").as_deref(), Some("6"));
    j.own_window("task_1", "0x2", 200, "mousepad", "notes.txt - Mousepad", None, "").unwrap();
    let owned: Vec<(String, i64)> = j.task_windows("task_1").unwrap().into_iter().map(|w| (w.address, w.pid)).collect();
    assert_eq!(owned, [("0x2".to_string(), 200)]);
}

#[test]
fn attention_items_answer_once_and_appear_on_the_timeline() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let before = j.timeline_revision().unwrap();
    let options = vec!["approve".to_string(), "deny".to_string()];
    let item = j
        .raise_attention(NewAttention {
            task_ref: "task_a",
            principal: "vesper",
            kind: "approval",
            operation_ref: Some("op_1"),
            generation: Some("lease_1"),
            question: "Send report.pdf to bob@example.com?",
            details: None,
            options: &options,
            now_iso: "2026-09-25T00:00:00.000Z",
        })
        .unwrap();
    assert_eq!(j.count_open_attention(Some("task_a")).unwrap(), 1);
    assert_eq!(j.answer_attention(&item.att_ref, "maybe", "riley", "2026-09-25T00:00:01.000Z").unwrap_err().code, "INVALID_ARGUMENT");
    let answered = j.answer_attention(&item.att_ref, "approve", "riley", "2026-09-25T00:00:01.000Z").unwrap();
    assert_eq!(answered.state, "answered");
    assert_eq!(j.answer_attention(&item.att_ref, "approve", "riley", "2026-09-25T00:00:02.000Z").unwrap(), answered);
    assert!(j.answer_attention(&item.att_ref, "deny", "riley", "2026-09-25T00:00:02.000Z").is_err());
    assert_eq!(j.count_open_attention(None).unwrap(), 0);
    let kinds: Vec<String> = j.events_since(before, None, 10).unwrap().into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["attention.raised", "attention.answered"]);
}

// Failure cases for the details behind an approval's plain words:
// 1. The details do not come back as they were raised (a Details view shows nothing).
// 2. A journal from before details keeps its open approvals, which were asked
//    in words that show JSON, window addresses and process ids.
// 3. Adding the column expires an agent's open question too, which a person
//    can still answer.
#[test]
fn approvals_keep_their_details_and_older_open_approvals_expire_once() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    let options = vec!["approve".to_string(), "deny".to_string()];
    let raise = |j: &Journal, kind: &str, question: &str, details: Option<&Value>| {
        j.raise_attention(NewAttention {
            task_ref: "task_a",
            principal: "vesper",
            kind,
            operation_ref: None,
            generation: None,
            question,
            details,
            options: if kind == "approval" { &options[..] } else { &[] },
            now_iso: "2026-09-25T00:00:00.000Z",
        })
        .unwrap()
    };
    let details = json!({ "step": { "action": { "kind": "key", "keys": "Return" } }, "target": { "window": { "address": "0x1", "pid": 100 } } });
    let raised = raise(&j, "approval", "codex@vesper wants to press Return in Mousepad.", Some(&details));
    assert_eq!(j.get_attention(&raised.att_ref).unwrap().unwrap().details, details);
    drop(j);

    // A journal from before details: an open approval in the old words, and an agent's question.
    let db = Connection::open(dir.join("journal.sqlite")).unwrap();
    db.execute_batch(
        r#"DELETE FROM attention_items;
           ALTER TABLE attention_items DROP COLUMN details;
           INSERT INTO attention_items(att_ref, task_ref, principal, kind, question, options, state, created_at) VALUES
             ('att_old', 'task_a', 'vesper', 'approval', 'Approve computer_act send step: key Return in chromium · target {"pid":3015645}?', '["approve","deny"]', 'open', '2026-09-25T00:00:00.000Z'),
             ('att_ask', 'task_a', 'vesper', 'question', 'Which file?', '[]', 'open', '2026-09-25T00:00:00.000Z');"#,
    )
    .unwrap();
    drop(db);
    let (j, _) = open(&dir);
    let open_now: Vec<String> = j.list_attention(Some("open"), None, 10).unwrap().into_iter().map(|i| i.att_ref).collect();
    assert_eq!(open_now, ["att_ask"], "the older approval expired; the question is still open");
    assert_eq!(j.get_attention("att_old").unwrap().unwrap().details, Value::Null);
    let again = raise(&j, "approval", "codex@vesper wants to press Return in Mousepad.", Some(&details));
    drop(j);
    let (j, _) = open(&dir);
    assert_eq!(j.get_attention(&again.att_ref).unwrap().unwrap().state, "open", "only the first open after the column is added expires approvals");
}

// Failure cases for login requests on a journal made by an older build:
// 1. Its attention table's kind check still refuses `login`.
// 2. Rebuilding the table loses an item, its details or its answer.
#[test]
fn an_older_attention_table_takes_login_requests_and_keeps_its_items() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    drop(j);
    let db = Connection::open(dir.join("journal.sqlite")).unwrap();
    db.execute_batch(
        r#"DROP TABLE attention_items;
           CREATE TABLE attention_items (att_ref TEXT PRIMARY KEY, task_ref TEXT NOT NULL, principal TEXT NOT NULL,
             kind TEXT NOT NULL DEFAULT 'question' CHECK (kind IN ('question','approval')), operation_ref TEXT, generation TEXT,
             question TEXT NOT NULL, options TEXT NOT NULL DEFAULT '[]', state TEXT NOT NULL CHECK (state IN ('open','answered','expired')),
             answer TEXT, answered_by TEXT, created_at TEXT NOT NULL, answered_at TEXT);
           ALTER TABLE attention_items ADD COLUMN details TEXT;
           INSERT INTO attention_items(att_ref, task_ref, principal, kind, question, options, state, answer, answered_by, created_at, answered_at, details) VALUES
             ('att_done', 'task_a', 'vesper', 'approval', 'codex@vesper wants to send.', '["approve","deny"]', 'answered', 'approve', 'riley', '2026-09-25T00:00:00.000Z', '2026-09-25T00:00:01.000Z', '{"effect":"send"}');"#,
    )
    .unwrap();
    assert!(db.execute("INSERT INTO attention_items(att_ref, task_ref, principal, kind, question, state, created_at) VALUES ('att_x', 't', 'p', 'login', 'q', 'open', 'now')", []).is_err());
    drop(db);
    let (j, _) = open(&dir);
    let kept = j.get_attention("att_done").unwrap().unwrap();
    assert_eq!((kept.state.as_str(), kept.answer.as_deref(), kept.details.clone()), ("answered", Some("approve"), json!({"effect": "send"})));
    let login = j
        .raise_attention(NewAttention {
            task_ref: "task_b",
            principal: "vesper",
            kind: "login",
            operation_ref: None,
            generation: None,
            question: "codex@vesper, working on “Renew the license” on Tulip1, wants your login for example.org",
            details: Some(&json!({ "sites": [{ "site": "example.org" }] })),
            options: &[],
            now_iso: "2026-09-25T00:00:02.000Z",
        })
        .unwrap();
    assert_eq!(j.count_open_attention(Some("task_b")).unwrap(), 1, "{login:?}");
}

#[test]
fn app_notes_count_confirmations_per_fact() {
    let dir = tempdir();
    let (j, _) = open(&dir);
    j.record_app_note("mousepad", "0.6", "dialog:Save As", "semantics", &json!({ "available": false }), "2026-09-25T00:00:00.000Z").unwrap();
    let note = j.record_app_note("mousepad", "0.6", "dialog:Save As", "semantics", &json!({ "available": false }), "2026-09-25T00:00:01.000Z").unwrap();
    assert_eq!(note.count, 2);
    j.record_app_note("mousepad", "0.6", "dialog:Save As", "route:keyboard", &json!({ "outcome": "worked" }), "2026-09-25T00:00:02.000Z").unwrap();
    assert_eq!(j.list_app_notes("mousepad", None, Some("dialog:Save As"), 10).unwrap().len(), 2);
    assert!(j.list_app_notes("gedit", None, None, 10).unwrap().is_empty());
}

// ---- real journals (fixtures/private, never committed) -----------------------

/// Each computer's state dir under fixtures/private, whatever it is named.
fn fixtures() -> Vec<(String, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/private");
    let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
        .filter(|(_, dir)| dir.join("journal.sqlite").is_file())
        .collect();
    dirs.sort();
    dirs
}

/// Copy a fixture state dir (journal + storage) with the backup API.
fn copy_fixture(src: &Path) -> PathBuf {
    let dir = tempdir();
    for name in ["journal.sqlite", "storage.sqlite"] {
        if let Some(db) = migrate::snapshot(&src.join(name)).unwrap() {
            let mut out = Connection::open(dir.join(name)).unwrap();
            rusqlite::backup::Backup::new(&db, &mut out).unwrap().run_to_completion(1024, std::time::Duration::ZERO, None).unwrap();
        }
    }
    dir
}

fn meta(db: &Connection, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    db.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).optional().unwrap()
}

#[test]
fn real_journals_open_migrate_additively_and_keep_identity() {
    let hosts = fixtures();
    if hosts.is_empty() {
        eprintln!("skipped: no fixtures in fixtures/private");
        return;
    }
    for (host, src) in hosts {
        let dir = copy_fixture(&src);
        let before = Connection::open(dir.join("journal.sqlite")).unwrap();
        let identity = meta(&before, "endpoint_identity").expect("fixture has an endpoint identity");
        let secret = meta(&before, "fingerprint_secret").expect("fixture has a fingerprint secret");
        let counts = |db: &Connection| -> Vec<i64> {
            ["tasks", "operations", "leases", "connections", "artifacts", "grants", "checks", "observations", "jobs"]
                .iter()
                .map(|t| db.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap())
                .collect()
        };
        let rows_before = counts(&before);
        let stale_before: i64 = before
            .query_row("SELECT COUNT(*) FROM operations WHERE dispatched = 1 AND json_extract(receipt, '$.execution') = 'running' AND json_extract(receipt, '$.job_ref') IS NOT NULL", [], |r| r.get(0))
            .unwrap();
        // Pin the clock just after the snapshot so the retention cutoff is stable over time.
        let latest: String = before.query_row("SELECT MAX(updated_at) FROM tasks", [], |r| r.get(0)).unwrap();
        let at = crate::ids::millis_from_iso(&latest).unwrap() + 60_000;
        drop(before);

        // Default retention and capacity, as ibarad would open it.
        let j = Journal::open(&dir, JournalOptions { now: Some(Arc::new(move || at)), ..Default::default() })
            .unwrap_or_else(|e| panic!("{host}: {e}"));
        assert_eq!(j.endpoint_identity().unwrap(), identity, "{host}: endpoint identity changed");
        assert_eq!(j.migration().stale_running_receipts_settled as i64, stale_before, "{host}");
        // Every task, lease and operation row still decodes.
        for t in j.list_tasks(None, 500).unwrap() {
            j.list_operations_for_task(&t.task_ref, 100, None).unwrap();
            j.list_leases_for_task(&t.task_ref).unwrap();
        }
        j.rotate_epoch(&iso_from_millis(at)).unwrap();
        drop(j);

        let after = Connection::open(dir.join("journal.sqlite")).unwrap();
        assert_eq!(meta(&after, "endpoint_identity").as_deref(), Some(identity.as_str()), "{host}");
        assert_eq!(meta(&after, "fingerprint_secret").as_deref(), Some(secret.as_str()), "{host}: secret changed");
        assert_eq!(meta(&after, "schema_version").as_deref(), Some("2"), "{host}: base version must stay Node-compatible");
        assert_eq!(meta(&after, "core_schema_version").as_deref(), Some("6"), "{host}");
        assert_eq!(counts(&after), rows_before, "{host}: rows changed (no retention cutoff reached in fixtures)");
        let running: i64 = after
            .query_row("SELECT COUNT(*) FROM operations WHERE dispatched = 1 AND json_extract(receipt, '$.execution') = 'running'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(running, 0, "{host}: no dispatched receipt may stay running after cutover and restart");
        let classes: i64 = after.query_row("SELECT COUNT(*) FROM operations WHERE effect_class = 'change'", [], |r| r.get(0)).unwrap();
        assert_eq!(classes, rows_before[1], "{host}");
        // Reopening is a no-op migration.
        drop(after);
        let j = Journal::open(&dir, JournalOptions { now: Some(Arc::new(move || at)), ..Default::default() }).unwrap();
        assert_eq!(j.migration().stale_running_receipts_settled, 0);
        assert_eq!(j.endpoint_identity().unwrap(), identity);
    }
}

#[test]
fn real_journal_fingerprints_recompute() {
    let hosts: Vec<PathBuf> = fixtures().into_iter().map(|(_, dir)| dir).collect();
    if hosts.is_empty() {
        eprintln!("skipped: no fixtures in fixtures/private");
        return;
    }
    let mut sampled = 0;
    for src in hosts {
        let report = migrate::check(&src).unwrap();
        assert!(report.schema_supported && report.endpoint_identity_present && report.fingerprint_secret_well_formed, "{report:?}");
        for (tool, s) in &report.fingerprint_samples {
            if s.exact {
                assert_eq!(s.matched, s.sampled, "{}: {tool} fingerprints did not recompute: {report:?}", src.display());
            }
            sampled += s.matched;
        }
        assert!(report.fingerprint_pass_rate.is_none_or(|r| r == 1.0), "{report:?}");
    }
    assert!(sampled > 0, "no stored fingerprint recomputed");
}
