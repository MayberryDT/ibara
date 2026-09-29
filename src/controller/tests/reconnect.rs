//! An agent whose connection went away comes back over a new one, and
//! two live sessions of one agent (two `codex` sessions on one computer).
//! The case where the old session is still open, its client silent, is end
//! to end in `tests/server_entries.rs`; these need the fake clock. Failure
//! cases, written first:
//! - the agent back during its old connection's grace must wait for the
//!   grace, or its control ends when that grace runs out after it came back
//! - a different agent is let in during the grace, is told anything but that
//!   another agent controls, can use the first agent's task, or is still kept
//!   out after the grace released the control
//! - after a person took control during the drop, the agent gets control
//!   back, or is told anything but that a person has it
//! - control whose grace ran out before the agent came back is resumed
//! - a second session of the same agent, while the first still answers, is
//!   told it controls, learns the task from begin, or takes the control by
//!   naming the task, so the two sessions take turns sending input
//! - once the first session stops answering, the same agent still waits for
//!   the grace, or gets new control instead of the one it held; the first
//!   session, answering again, takes it back from the live second one

use super::stream::take;
use super::*;

async fn call_as(c: &Controller, connection_id: &str, client: &str, tool: &str, args: Value) -> Value {
    c.call("vesper", connection_id, client, tool, args, Cancel::new()).await.envelope
}

fn situation(envelope: &Value) -> &str {
    envelope["situation"].as_str().unwrap_or_default()
}

fn checkpoint(task: &str) -> Value {
    json!({ "task_ref": task, "note": "still here" })
}

#[test]
fn an_agent_back_during_the_grace_keeps_control_after_the_grace_runs_out() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        c.disconnect("vesper", "connection_a").await.unwrap();
        rig.clock.fetch_add(10_000, Ordering::SeqCst);
        let back = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(back["status"], "ok", "resumed at once: {back}");
        assert!(situation(&back).contains("you (codex@vesper) control"), "{back}");
        rig.clock.fetch_add(DISCONNECT_GRACE_MS, Ordering::SeqCst);
        c.reconcile().await.unwrap();
        let later = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(later["status"], "ok", "the old connection's grace no longer applies: {later}");
        assert_eq!(c.journal.get_active_lease().unwrap().map(|l| l.connection_id).as_deref(), Some("connection_b"));
    });
}

#[test]
fn another_agent_is_kept_out_until_the_grace_releases_control() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        c.disconnect("vesper", "connection_a").await.unwrap();
        let refused = call_as(c, "connection_c", "claude", "computer_begin", json!({ "goal": "Something else", "request_id": "other-1" })).await;
        assert_eq!((code(&refused), refused["error"]["message"].as_str()), ("BUSY", Some("Control is unavailable.")), "{refused}");
        assert!(situation(&refused).contains("another agent controls"), "{refused}");
        let theirs = call_as(c, "connection_c", "claude", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(code(&theirs), "PERMISSION_DENIED", "{theirs}");
        assert_eq!(c.journal.get_active_lease().unwrap().map(|l| l.connection_id).as_deref(), Some("connection_a"));

        rig.clock.fetch_add(DISCONNECT_GRACE_MS + 1, Ordering::SeqCst);
        let late = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(code(&late), "LEASE_EXPIRED", "control that ended is not resumed: {late}");
        let ended = c.journal.list_leases_for_task(&task).unwrap();
        assert_eq!(ended[0].reason.as_deref(), Some("disconnect_grace"));
        let admitted = call_as(c, "connection_c", "claude", "computer_begin", json!({ "goal": "Something else", "request_id": "other-2" })).await;
        assert_eq!(admitted["status"], "ok", "the grace released control: {admitted}");
    });
}

#[test]
fn an_agent_back_after_a_person_took_control_is_told_the_person_has_it() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        let task = begin(c).await;
        take(c, "vesper").await.unwrap();
        let back = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(code(&back), "HUMAN_CONTROL", "{back}");
        assert!(situation(&back).contains("a person controls"), "{back}");
        assert!(c.journal.get_active_lease().unwrap().is_none(), "nothing resumed under the person");
    });
}

#[test]
fn a_second_session_of_the_same_agent_cannot_take_a_live_sessions_control() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        c.heartbeat("vesper", "connection_a", true).await.unwrap();
        let begun = call_as(c, "connection_b", "codex", "computer_begin", json!({ "goal": "Something else", "request_id": "second-1" })).await;
        assert_eq!((code(&begun), begun["error"]["message"].as_str()), ("BUSY", Some("Control is unavailable.")), "{begun}");
        assert!(begun["error"]["task_ref"].is_null(), "the other session's task stays private: {begun}");
        assert!(situation(&begun).contains("another agent controls"), "{begun}");
        let taken = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!((code(&taken), taken["error"]["message"].as_str()), ("BUSY", Some("Control is unavailable.")), "{taken}");
        assert_eq!(c.journal.get_active_lease().unwrap().map(|l| l.connection_id).as_deref(), Some("connection_a"));
        let first = call_as(c, "connection_a", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(first["status"], "ok", "the first session keeps its control: {first}");
        assert!(situation(&first).contains("you (codex@vesper) control"), "{first}");
    });
}

#[test]
fn the_same_agent_carries_on_once_its_old_session_stops_answering() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let held = c.journal.get_active_lease().unwrap().unwrap();
        c.heartbeat("vesper", "connection_a", false).await.unwrap();
        let begun = call_as(c, "connection_b", "codex", "computer_begin", json!({ "goal": "Something else", "request_id": "back-1" })).await;
        assert_eq!((code(&begun), begun["error"]["task_ref"].as_str()), ("BUSY", Some(task.as_str())), "{begun}");
        assert!(situation(&begun).contains("you (codex@vesper) control"), "{begun}");
        let back = call_as(c, "connection_b", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!(back["status"], "ok", "carried on at once: {back}");
        let moved = c.journal.get_active_lease().unwrap().unwrap();
        assert_eq!((moved.generation, moved.connection_id.as_str()), (held.generation, "connection_b"));

        // The first session's client answers again: the live second session keeps control.
        c.heartbeat("vesper", "connection_a", true).await.unwrap();
        let again = call_as(c, "connection_a", "codex", "computer_checkpoint", checkpoint(&task)).await;
        assert_eq!((code(&again), again["error"]["message"].as_str()), ("BUSY", Some("Control is unavailable.")), "{again}");
        assert_eq!(c.journal.get_active_lease().unwrap().map(|l| l.connection_id).as_deref(), Some("connection_b"));
    });
}
