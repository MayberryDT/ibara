//! Written before implementation; failures are in task62/failure-cases.md.
//! Real journal/control paths, scripted desktop. Live MCP acceptance is separate.
use super::*;

fn disposable(rig: &mut Rig) {
    Rc::get_mut(&mut rig.controller).unwrap().disposable_desktop = Rc::new(|| true);
}

#[test]
fn disposable_entry_and_finish_clear_unowned_windows_but_active_work_survives() {
    run(async {
        let mut rig = rig(false);
        disposable(&mut rig);
        let c = &rig.controller;
        let task = begin(c).await;
        assert!(rig.desktop.windows.borrow().is_empty(), "entry baseline");
        rig.desktop.windows.borrow_mut().push(win("0x9", 900, "foot", "old task", true, false));
        c.reconcile().await.unwrap();
        assert_eq!(rig.desktop.windows.borrow().len(), 1, "active work protected");
        let other = call_in(c, "other", "computer_begin", json!({"goal":"compete","request_id":"b"})).await;
        assert_eq!(code(&other), "BUSY");
        assert_eq!(rig.desktop.windows.borrow().len(), 1);
        let done = call(c, "computer_finish", json!({"task_ref":task,"request_id":"f","outcome":"complete","summary":"saved"})).await;
        assert!(rig.desktop.windows.borrow().is_empty(), "unowned windows reset: {done}");
        assert!(!c.journal.get_control().unwrap().unsettled);
    });
}

#[test]
fn disposable_idle_late_window_is_closed_and_refusal_never_reports_ready() {
    run(async {
        let mut rig = rig(false);
        disposable(&mut rig);
        let c = &rig.controller;
        c.reconcile().await.unwrap();
        rig.desktop.windows.borrow_mut().push(win("0x9", 900, "mousepad", "*unsaved", true, false));
        rig.desktop.ignores_close.borrow_mut().push("0x9".into());
        c.reconcile().await.unwrap();
        assert!(c.journal.get_control().unwrap().unsettled);
        let denied = call(c, "computer_begin", json!({"request_id":"b","goal":"work"})).await;
        assert_eq!(code(&denied), "CONTROL_UNSETTLED", "{denied}");
        rig.desktop.ignores_close.borrow_mut().clear();
        // An unchanged refusal stays blocked until the operator requests recovery.
        c.admin_resume().await.unwrap();
        c.reconcile().await.unwrap();
        assert!(rig.desktop.windows.borrow().is_empty());
        assert!(!c.journal.get_control().unwrap().unsettled);
    });
}

#[test]
fn disposable_bridge_heartbeat_does_not_keep_abandoned_work_alive() {
    run(async {
        let mut rig = rig(false);
        disposable(&mut rig);
        let c = &rig.controller;
        let _task = begin(c).await;
        rig.desktop.windows.borrow_mut().push(win("0x9", 900, "foot", "abandoned", true, false));
        for _ in 0..11 {
            rig.clock.fetch_add(30_000, Ordering::SeqCst);
            c.heartbeat("vesper", "connection_a", true).await.unwrap();
        }
        c.reconcile().await.unwrap();
        assert!(c.journal.get_active_lease().unwrap().is_none());
        assert!(rig.desktop.windows.borrow().is_empty());
    });
}

#[test]
fn disposable_meaningful_observation_renews_work_and_explicit_close_accepts_old_window() {
    run(async {
        let mut rig = rig(false);
        disposable(&mut rig);
        let c = &rig.controller;
        let task = begin(c).await;
        rig.desktop.windows.borrow_mut().push(win("0x9", 900, "foot", "untracked", true, false));
        rig.clock.fetch_add(240_000, Ordering::SeqCst);
        let observed = call(c, "computer_observe", json!({"task_ref":task})).await;
        assert_eq!(observed["status"], "ok", "{observed}");
        rig.clock.fetch_add(120_000, Ordering::SeqCst);
        c.reconcile().await.unwrap();
        assert!(c.journal.get_active_lease().unwrap().is_some());
        let closed = call(c,"computer_act",json!({"task_ref":task,"request_id":"close","action":{"kind":"close","surface":"foot"}})).await;
        assert_eq!(closed["status"],"ok","{closed}");
        assert!(rig.desktop.windows.borrow().is_empty());
    });
}

#[test]
fn personal_mode_never_sweeps_old_windows() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let before = rig.desktop.windows.borrow().len();
        c.reconcile().await.unwrap();
        assert_eq!(rig.desktop.windows.borrow().len(), before);
        let task = begin(c).await;
        let closed = call(c,"computer_act",json!({"task_ref":task,"request_id":"close","action":{"kind":"close","surface":"w1"}})).await;
        assert_eq!(code(&closed),"PERMISSION_DENIED");
        call(c,"computer_finish",json!({"task_ref":task,"request_id":"f","outcome":"partial","summary":"stop"})).await;
        assert_eq!(rig.desktop.windows.borrow().len(), before);
    });
}

#[test]
fn person_pause_while_retirement_releases_input_prevents_final_close() {
    run(async {
        let mut rig = rig(false);
        disposable(&mut rig);
        let c = &rig.controller;
        rig.desktop.windows.replace(vec![win("0x9", 900, "chromium", "synthetic", true, false)]);
        let inventory = |tabs| json!({"windows":[{"id":7,"focused":true,"type":"normal","incognito":false,"tabs":tabs}]});
        rig.desktop.reader_replies.borrow_mut().insert("lifecycle_inventory".into(), [
            inventory(json!([{"id":1,"active":true,"blank":false}])),
            inventory(json!([{"id":1,"active":false,"blank":false},{"id":2,"active":true,"blank":true}])),
            inventory(json!([{"id":2,"active":true,"blank":true}])),
        ].into());
        c.journal.begin_settlement("retirement-pause-test").unwrap();
        let hold = Rc::new(Notify::new());
        rig.desktop.release_hold.replace(Some(hold.clone()));
        let pause = async {
            rig.desktop.entered.notified().await;
            // Real pause changes authority before an in-progress reset resumes.
            c.admin_pause(crate::store::PauseOrigin::Person).await.unwrap();
            hold.notify_one();
        };
        let (reset, _) = tokio::join!(c.reset_desktop_windows(), pause);
        assert!(!reset.unwrap(), "person pause must block reset readiness");
        assert_eq!(rig.desktop.windows.borrow().len(), 1, "reset closed the person's window after pause");
        assert!(!rig.desktop.acts.borrow().iter().any(|act| act.starts_with("close_window") || act.starts_with("Close(")));
    });
}

// Failure cases first: Fix It only restarts and leaves retry fence forever;
// retry overrides a human pause; retry interrupts a live task; failed repair
// reports success or auto-loops uncertain input.
#[test]
fn repair_cleanup_retries_once_and_preserves_person_control() {
    run(async {
        let mut rig=rig(false); disposable(&mut rig); let c=&rig.controller;
        c.reconcile().await.unwrap();
        rig.desktop.windows.borrow_mut().push(win("0x9",900,"mousepad","unsaved",true,false));
        rig.desktop.ignores_close.borrow_mut().push("0x9".into());
        c.reconcile().await.unwrap();
        let refused=c.repair_now("retry_cleanup").await.unwrap();
        assert_eq!(refused["state"],"still_broken");
        rig.desktop.ignores_close.borrow_mut().clear();
        let recovered=c.repair_now("retry_cleanup").await.unwrap();
        assert_eq!(recovered["state"],"fixed","{recovered}");
        assert!(rig.desktop.windows.borrow().is_empty());
        assert!(!c.journal.get_control().unwrap().unsettled);
        c.admin_pause(crate::store::PauseOrigin::Person).await.unwrap();
        let paused=c.repair_now("retry_cleanup").await.unwrap();
        assert_eq!(paused["state"],"still_broken");
        assert_eq!(c.journal.get_control().unwrap().pause_origin,Some(crate::store::PauseOrigin::Person));
    });
}
