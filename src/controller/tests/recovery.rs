//! Auto-resume and self-repair, through `start`, restarts on the same state
//! directory and the watchdog pass its loop runs. Failure cases, written first:
//! - every start leaves the computer paused until someone resumes it
//! - a start or shutdown pause, or a restart during an agent's lease, turns a
//!   person's pause into the system's (so the watchdog would end it)
//! - the watchdog ends a person's pause, or a person's control, after a restart
//! - handing back after a start leaves the computer paused forever; handing
//!   back after a person's own pause, or a pause made while holding control,
//!   resumes agents
//! - a viewer that failed at start is never retried, or is retried every pass
//! - a computer that started without a screen waits for a display event that
//!   never comes, or resumes before it has a screen
//! - unsettled work with nothing running stays unsettled forever, or is
//!   settled while a person holds control or before the grace ends
//! - a display change whose output change waited for a task's control to end
//!   runs while that task's finish settles, and leaves the computer
//!   unsettled, so the next agents are refused until the watchdog settles it
//! - a repair or automatic resume leaves no timeline event
//! - a repair that keeps failing is never shown as needing a person, or stays
//!   shown after it works
//! - after a restart, finishing a task leaves the apps it opened open without
//!   saying so (the apps outlive ibarad; whose windows they are must too), or
//!   closes one a person used before the restart
//! - while only ibara paused the computer (starting, settling), an agent is
//!   told a person holds it, is refused for good, or is told to retry the same
//!   request_id (a failed begin replays its refusal); a person's pause or
//!   Take Control no longer says a person holds it
//! - a step cut by a restart blames a person instead of the restart

use super::*;
use crate::store::{ControlState, PauseOrigin, TimelineEvent};
use std::time::Duration;

fn quick(controller: &mut Rc<Controller>) {
    Rc::get_mut(controller).expect("unshared controller").timing = Timing { drain: Duration::from_millis(50) };
}

fn control(c: &Controller) -> ControlState {
    c.journal.get_control().unwrap()
}

fn events(c: &Controller, kind: &str) -> Vec<TimelineEvent> {
    let mut events: Vec<TimelineEvent> = c.journal.recent_events(None, 500).unwrap().into_iter().filter(|e| e.kind == kind).collect();
    events.reverse();
    events
}

fn assert_resumed(c: &Controller) {
    let state = control(c);
    assert!(!state.paused && !state.human_control && state.pause_origin.is_none(), "{state:?}");
    assert_eq!(c.availability().unwrap(), "ready");
}

fn advance(rig: &Rig, ms: i64) {
    rig.clock.fetch_add(ms, Ordering::SeqCst);
}

async fn operator(c: &Controller, op: &str) -> Value {
    c.operator_call("vesper", operator_action(c, op)).await.unwrap_or_else(|e| panic!("{op}: {e:?}"))
}

/// Take control or hand back at the current owner and revision.
async fn transition(c: &Controller, op: &str) -> Value {
    let status = operator(c, "status").await;
    let mut action = operator_action(c, op);
    action["expected_owner"] = status["owner"].clone();
    action["expected_ownership_revision"] = status["ownership_revision"].clone();
    c.operator_call("vesper", action).await.unwrap_or_else(|e| panic!("{op}: {e:?}"))
}

/// A started controller on `dir` with `stream`.
async fn start_with(dir: &Path, clock: &Arc<AtomicI64>, desktop: &Rc<FakeDesktop>, stream: &FakeStream) -> Rc<Controller> {
    let mut controller = open_with(dir, clock, desktop, Some(stream.clone()));
    quick(&mut controller);
    controller.start().await.unwrap();
    controller
}

/// A started rig whose stream fails its next `failures` calls, as one an
/// earlier ibarad left hanging does.
async fn flaky_rig(failures: u32) -> (Rig, FakeStream) {
    let dir = std::env::temp_dir().join(id("ibara-controller-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = Arc::new(AtomicI64::new(1_790_000_000_000));
    let desktop = FakeDesktop::new();
    let stream = FakeStream::default();
    stream.0.failures.set(failures);
    let controller = start_with(&dir, &clock, &desktop, &stream).await;
    (Rig { dir, clock, desktop, stream: stream.clone(), controller }, stream)
}

#[test]
fn a_restart_after_a_system_pause_resumes_by_itself() {
    run(async {
        let mut rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = rig.controller.clone();
        let started = control(&c);
        assert!(started.paused && started.human_control, "every start pauses first: {started:?}");
        assert_eq!(started.pause_origin, Some(PauseOrigin::System));
        c.watchdog_tick().await;
        assert_resumed(&c);
        let resumed = events(&c, "auto_resumed");
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].summary, "Resumed after restart; nothing needed a person.");
        assert_eq!(resumed[0].actor, "ibarad");

        // A clean stop pauses for the system; the next start resumes again.
        c.system_pause().await.unwrap();
        c.shutdown().await;
        drop(c);
        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        assert_eq!(control(&c).pause_origin, Some(PauseOrigin::System), "the shutdown pause is kept");
        c.start().await.unwrap();
        c.watchdog_tick().await;
        assert_resumed(&c);
        assert_eq!(events(&c, "auto_resumed").len(), 2);

        // A crash while an agent held control is the system's pause too.
        begin(&c).await;
        drop(c);
        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        let crashed = control(&c);
        assert!(crashed.paused && crashed.unsettled && crashed.pause_origin == Some(PauseOrigin::System), "{crashed:?}");
        c.start().await.unwrap();
        c.watchdog_tick().await;
        assert_resumed(&c);
    });
}

#[test]
fn apps_an_agent_opened_outlive_a_restart_and_its_task_still_closes_only_its_own() {
    run(async {
        let mut rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = rig.controller.clone();
        c.watchdog_tick().await;
        let task = begin(&c).await;
        for (address, pid, class, title, app) in [("0x2", 200, "mousepad", "notes.txt - Mousepad", "editor"), ("0x3", 201, "foot", "shell", "terminal")] {
            rig.desktop.spawn.replace(Some(win(address, pid, class, title, false, false)));
            rig.desktop.launch_pid.set(Some(pid as u32));
            let env = call(&c, "computer_act", json!({ "task_ref": task, "request_id": format!("open-{app}"), "action": { "kind": "launch", "app": app } })).await;
            assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        }
        // A person goes to the terminal while no agent step runs.
        advance(&rig, 10_000);
        c.on_desktop_event(DesktopEvent::Focus { address: Some("0x3".into()) });

        // ibara restarts (an update); both apps keep running with their windows.
        c.system_pause().await.unwrap();
        c.shutdown().await;
        drop(c);
        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        c.start().await.unwrap();
        c.watchdog_tick().await;
        assert_resumed(&c);

        let finish = call(&c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "complete", "summary": "Saved the note." })).await;
        assert_eq!(finish["status"], "ok", "{finish}");
        let cleanup = &finish["result"]["cleanup"];
        assert_eq!(cleanup["closed"], json!(["mousepad \"notes.txt - Mousepad\""]), "the task's editor closes: {finish}");
        assert_eq!(cleanup["left"], json!([{ "surface": "foot \"shell\"", "reason": "a person used it" }]), "{finish}");
        let closes: Vec<String> = rig.desktop.acts.borrow().iter().filter(|a| a.starts_with("Close")).cloned().collect();
        assert!(closes.len() == 1 && closes[0].contains("0x2"), "only the task's own window is asked to close: {closes:?}");
        let open: Vec<String> = rig.desktop.windows.borrow().iter().map(|w| w.address.clone()).collect();
        assert_eq!(open, ["0x1", "0x3"], "the person's windows stay");
    });
}

#[test]
fn a_restart_after_a_persons_pause_stays_paused() {
    run(async {
        let mut rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = rig.controller.clone();
        c.watchdog_tick().await;
        let paused = operator(&c, "pause").await;
        assert_eq!(paused["paused"], true, "{paused}");
        assert_eq!(paused["pause_origin"], "person");

        c.system_pause().await.unwrap();
        c.shutdown().await;
        drop(c);
        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        c.start().await.unwrap();
        c.watchdog_tick().await;
        advance(&rig, 3_600_000);
        c.watchdog_tick().await;
        let state = control(&c);
        assert!(state.paused && state.pause_origin == Some(PauseOrigin::Person), "a person's pause outlives restarts: {state:?}");
        assert_eq!(events(&c, "auto_resumed").len(), 1, "only the first start resumed by itself");
        let begun = call(&c, "computer_begin", json!({ "goal": "Write", "request_id": "r1" })).await;
        assert_eq!(code(&begun), "HUMAN_CONTROL");

        let resumed = operator(&c, "resume").await;
        assert_eq!(resumed["paused"], false, "{resumed}");
        assert_resumed(&c);
    });
}

#[test]
fn handing_back_resumes_a_system_pause_but_never_a_persons() {
    run(async {
        let (mut rig, stream) = flaky_rig(0).await;
        let c = rig.controller.clone();
        assert_eq!(control(&c).pause_origin, Some(PauseOrigin::System));

        // Right after a start, before the watchdog: status says ibara is starting,
        // and hand back resumes agents.
        let status = operator(&c, "status").await;
        assert_eq!(status["system_wait"], "starting", "{status}");
        transition(&c, "take_control").await;
        let back = transition(&c, "handback").await;
        assert_eq!((back["owner"].clone(), back["pause_origin"].clone()), (json!("none"), Value::Null), "{back}");
        assert_resumed(&c);
        assert_eq!(operator(&c, "status").await["system_wait"], Value::Null);

        // A person's own pause stays through take control and hand back.
        operator(&c, "pause").await;
        transition(&c, "take_control").await;
        let back = transition(&c, "handback").await;
        assert_eq!((back["owner"].clone(), back["pause_origin"].clone()), (json!("human"), json!("person")), "{back}");
        assert_eq!(control(&c).pause_origin, Some(PauseOrigin::Person));
        operator(&c, "resume").await;
        assert_resumed(&c);

        // So does a pause made while holding control; resuming then is refused.
        transition(&c, "take_control").await;
        let refused = c.operator_call("vesper", operator_action(&c, "resume")).await.unwrap_err();
        assert_eq!(refused.code, "HUMAN_CONTROL");
        assert_eq!(operator(&c, "pause").await["paused"], true);
        transition(&c, "handback").await;
        assert!(control(&c).paused && control(&c).pause_origin == Some(PauseOrigin::Person));
        operator(&c, "resume").await;

        // A person holding control when ibarad stops is still in charge after the restart.
        transition(&c, "take_control").await;
        drop(c);
        rig.controller = start_with(&rig.dir, &rig.clock, &rig.desktop, &stream).await;
        let c = rig.controller.clone();
        c.watchdog_tick().await;
        let state = control(&c);
        assert!(state.paused && state.pause_origin == Some(PauseOrigin::Person), "{state:?}");
    });
}

#[test]
fn a_failed_viewer_is_restarted_by_the_watchdog_and_the_computer_resumes() {
    run(async {
        let (rig, _) = flaky_rig(1).await;
        let c = &rig.controller;
        assert!(c.viewer_state.borrow().fault, "the viewer failed at start");
        assert_eq!(c.availability().unwrap(), "control_unsettled");
        c.watchdog_tick().await;
        assert!(!c.viewer_state.borrow().fault);
        assert_resumed(c);
        let repairs = events(c, "repair");
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].summary, "Restarted screen sharing after it stopped working.");
        assert!(events(c, "auto_resumed")[0].id > repairs[0].id, "it resumed only after the repair");
        let status = operator(c, "status").await;
        assert_eq!(status["repair"]["last"], json!({ "at": repairs[0].at, "summary": repairs[0].summary }));
        assert_eq!(status["repair"]["needs_person"], Value::Null);
    });
}

#[test]
fn a_repair_that_keeps_failing_needs_a_person_until_it_works() {
    run(async {
        let (rig, stream) = flaky_rig(u32::MAX).await;
        let revokes = || stream.0.revokes.get();
        let c = &rig.controller;
        let before = revokes();
        c.watchdog_tick().await;
        assert_eq!(revokes(), before + 1, "the first pass retries");
        c.watchdog_tick().await;
        assert_eq!(revokes(), before + 1, "a failed repair waits before the next try");
        assert_eq!(operator(c, "status").await["repair"]["needs_person"], Value::Null, "one failure is not yet a person's problem");
        advance(&rig, 10_000);
        c.watchdog_tick().await;
        advance(&rig, 20_000);
        c.watchdog_tick().await;
        assert_eq!(revokes(), before + 3);
        let status = operator(c, "status").await;
        let since = status["repair"]["needs_person"]["at"].clone();
        assert!(since.is_string(), "when it started needing a person: {status}");
        assert_eq!(
            status["repair"]["needs_person"],
            json!({
                "code": "viewer_unavailable",
                "message": "Screen sharing on this computer stopped working, and restarting it did not help.",
                "fix": "restart_viewer",
                "at": since,
            }),
            "{status}"
        );
        assert_eq!(status["repair"]["last"], Value::Null);
        assert!(control(c).paused, "an unhealthy computer is not resumed");

        stream.0.failures.set(0);
        advance(&rig, 40_000);
        c.watchdog_tick().await;
        let status = operator(c, "status").await;
        assert_eq!(status["repair"]["needs_person"], Value::Null, "{status}");
        assert_eq!(status["repair"]["last"]["summary"], "Restarted screen sharing after it stopped working.");
        assert_resumed(c);
    });
}

#[test]
fn a_computer_without_a_screen_gets_a_virtual_one_before_it_resumes() {
    run(async {
        let rig = rig(false);
        rig.desktop.output_change.set(Some(OutputChange::Create));
        rig.controller.start().await.unwrap();
        let c = &rig.controller;
        // Another reconcile holds the display: nothing is added, and no resume without a screen.
        c.display_maintenance.set(true);
        c.watchdog_tick().await;
        assert!(rig.desktop.output_changes.borrow().is_empty());
        assert!(control(c).paused);
        assert!(events(c, "repair").is_empty());
        c.display_maintenance.set(false);
        c.watchdog_tick().await;
        assert_eq!(*rig.desktop.output_changes.borrow(), [OutputChange::Create]);
        let repairs = events(c, "repair");
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].summary, "Added a virtual screen because no screen was connected.");
        assert_eq!(repairs[0].data["problem"], "display_missing");
        assert_resumed(c);
        assert!(events(c, "auto_resumed")[0].id > repairs[0].id);
        c.watchdog_tick().await;
        assert_eq!(rig.desktop.output_changes.borrow().len(), 1, "a present display is left alone");
    });
}

#[test]
fn unsettled_work_with_nothing_running_is_settled_again() {
    run(async {
        let rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = &rig.controller;
        c.watchdog_tick().await;
        assert_resumed(c);
        // A settlement that could not release input left the computer unsettled.
        c.journal.set_control(crate::store::ControlPatch { unsettled: Some(true), ..Default::default() }).unwrap();
        c.watchdog_tick().await;
        advance(&rig, 29_000);
        c.watchdog_tick().await;
        assert!(control(c).unsettled, "settled again before the grace ended");
        // A person holding control keeps it as it is, and restarts the grace.
        c.viewer_state.borrow_mut().owner = Some("vesper".into());
        advance(&rig, 5_000);
        c.watchdog_tick().await;
        assert!(control(c).unsettled, "settled under a person's control");
        c.viewer_state.borrow_mut().owner = None;
        c.watchdog_tick().await;
        advance(&rig, 29_000);
        c.watchdog_tick().await;
        assert!(control(c).unsettled);
        advance(&rig, 2_000);
        c.watchdog_tick().await;
        assert!(!control(c).unsettled);
        assert_eq!(c.availability().unwrap(), "ready");
        let repairs = events(c, "repair");
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].summary, "Finished stopping earlier work so agents can continue.");
    });
}

#[test]
fn a_display_change_waiting_for_a_finish_does_not_leave_the_computer_unsettled() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        // A display was plugged in during the task; its output change waits for control to end.
        rig.desktop.output_change.set(Some(OutputChange::Remove));
        let hold = Rc::new(Notify::new());
        rig.desktop.release_hold.replace(Some(hold.clone()));
        let finish = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "blocked", "summary": "stop" }));
        let display = async {
            rig.desktop.entered.notified().await;
            // The display loop's next try comes while the finish settles.
            let release = async {
                tokio::task::yield_now().await;
                hold.notify_one();
            };
            tokio::join!(c.admin(json!({ "op": "reconcile_display" })), release).0
        };
        let (finish, _) = tokio::join!(finish, display);
        assert_eq!(finish["status"], "ok", "{finish}");
        assert!(!control(c).unsettled, "{finish}");
        // The display loop's next try makes the change, and the next agent begins.
        assert_eq!(c.admin(json!({ "op": "reconcile_display" })).await.unwrap()["changed"], true);
        assert_eq!(*rig.desktop.output_changes.borrow(), [OutputChange::Remove]);
        begin(c).await;
    });
}

fn assert_waits_for_ibara(envelope: &Value, word: &str) {
    let error = &envelope["error"];
    assert_eq!((error["code"].as_str(), error["retry_safe"].as_bool()), (Some("BUSY"), Some(true)), "{envelope}");
    assert!(error["message"].as_str().unwrap().contains(&format!("ibara is {word}")), "{envelope}");
    assert!(error["next"].as_str().unwrap().contains("new request_id"), "{envelope}");
    let situation = envelope["situation"].as_str().unwrap();
    assert!(situation.contains(&format!("ibara is {word}")), "{envelope}");
    assert!(!envelope.to_string().contains("person"), "nobody touched the computer: {envelope}");
}

#[test]
fn agents_are_told_ibara_is_starting_or_settling_and_only_a_persons_pause_is_a_persons() {
    run(async {
        let mut rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = rig.controller.clone();
        let starting = call(&c, "computer_begin", json!({ "goal": "Write", "request_id": "r1" })).await;
        assert_waits_for_ibara(&starting, "starting");
        c.watchdog_tick().await;
        let task = begin(&c).await;
        call(&c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "complete", "summary": "Done." })).await;

        // A person's pause, and a person holding control, stay a person's.
        operator(&c, "pause").await;
        let paused = call(&c, "computer_begin", json!({ "goal": "Write", "request_id": "r2" })).await;
        assert_eq!((code(&paused), paused["error"]["message"].as_str()), ("HUMAN_CONTROL", Some("A person currently holds the computer.")), "{paused}");
        assert!(paused["situation"].as_str().unwrap().contains("paused for a person"), "{paused}");
        operator(&c, "resume").await;

        // A crash while an agent held control: the start settles its work.
        begin(&c).await;
        drop(c);
        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        c.start().await.unwrap();
        let crashed = call(&c, "computer_begin", json!({ "goal": "Write", "request_id": "r3" })).await;
        assert_waits_for_ibara(&crashed, "starting");
        // A settlement that could not release input leaves work to settle first.
        c.journal.set_control(crate::store::ControlPatch { unsettled: Some(true), ..Default::default() }).unwrap();
        let settling = call(&c, "computer_begin", json!({ "goal": "Write", "request_id": "r4" })).await;
        assert_waits_for_ibara(&settling, "settling");
    });
}

#[test]
fn a_step_cut_by_a_restart_says_ibara_restarted() {
    run(async {
        let mut rig = rig(false);
        rig.controller.start().await.unwrap();
        let c = rig.controller.clone();
        c.watchdog_tick().await;
        let task = begin(&c).await;
        rig.desktop.until_cancelled.replace(Some(Ok(Done::default())));
        let agent = c.clone();
        let typed = task.clone();
        let typing = tokio::task::spawn_local(async move {
            call(&agent, "computer_act", json!({ "task_ref": typed, "request_id": "type-1", "action": { "kind": "type", "text": "a line that takes a while" } })).await
        });
        rig.desktop.entered.notified().await;
        // ibarad stops (an update or a restart of its unit).
        c.system_pause().await.unwrap();
        let cut = typing.await.unwrap();
        let step = cut["result"]["steps"][0].to_string();
        assert!(step.contains("ibara restarted") && !step.contains("person"), "{cut}");
        c.shutdown().await;
        drop(c);

        rig.controller = open(&rig.dir, &rig.clock, &rig.desktop, false);
        let c = rig.controller.clone();
        c.start().await.unwrap();
        let old = call(&c, "computer_act", json!({ "task_ref": task, "request_id": "type-2", "action": { "kind": "key", "keys": "Return" } })).await;
        assert_eq!(code(&old), "LEASE_EXPIRED", "{old}");
        assert!(old["error"]["message"].as_str().unwrap().contains("ibara restarted") && !old.to_string().contains("person"), "{old}");
        c.watchdog_tick().await;
        begin(&c).await;
    });
}
