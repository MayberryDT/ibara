//! Failure cases written before the ownership change. Real child processes,
//! journal and storage; fake time/desktop let us cross the old five-minute
//! boundary deterministically without sleeping through a build.
//! - Disconnect/idle expiry cancels a running build or admits conflicting work.
//! - Reconnect repeats the command, cannot recover it, or lets a live rival steal it.
//! - Finished disconnected jobs retain occupancy forever or lose their output.
//! - Explicit cancel fails to settle the process group or kills unrelated work.
//! - Default image/action totals silently terminate otherwise legitimate work.

use super::*;
use crate::controller::control::Charge;

async fn as_connection(c: &Controller, connection: &str, agent: &str, tool: &str, args: Value) -> Value {
    c.call("vesper", connection, agent, tool, args, Cancel::new()).await.envelope
}

async fn launch(c: &Controller, task: &str) -> (Value, String) {
    let args = json!({"task_ref":task,"request_id":"build-once","background":true,
        "command":["/bin/sh","-c","echo started >> starts; while [ ! -f release ]; do sleep 0.05; done; echo built > output; echo complete"],
        "timeout_ms":60000});
    let reply = call(c, "computer_exec", args.clone()).await;
    assert_eq!(reply["status"], "pending", "{reply}");
    let op = reply["result"]["op_ref"].as_str().unwrap().to_owned();
    (args, op)
}

#[test]
fn running_work_survives_connection_loss_and_reconnect_without_duplicate_execution() {
    run(async {
        let rig=rig(false); let c=&rig.controller; let task=begin(c).await;
        let (args,op)=launch(c,&task).await;
        c.disconnect("vesper","connection_a").await.unwrap();
        rig.clock.fetch_add(IDLE_EXPIRY_MS+DISCONNECT_GRACE_MS+1,Ordering::SeqCst);
        c.reconcile().await.unwrap();
        assert!(c.storage.has_active_jobs(Some(&task)), "transport loss killed registered work");
        assert_eq!(c.journal.get_active_lease().unwrap().unwrap().task_ref,task);
        let rival=as_connection(c,"rival","claude","computer_begin",json!({"goal":"conflict","request_id":"rival"})).await;
        assert_eq!(code(&rival),"BUSY","{rival}");
        let back=as_connection(c,"connection_b","codex","computer_checkpoint",json!({"task_ref":task,"note":"resumed"})).await;
        assert_eq!(back["status"],"ok","{back}");
        let replay=as_connection(c,"connection_b","codex","computer_exec",args).await;
        assert_eq!(replay["result"]["op_ref"],op,"{replay}");
        let workspace=c.storage.workspace(&task,false).unwrap();
        std::fs::write(workspace.join("release"),b"go").unwrap();
        let result=as_connection(c,"connection_b","codex","computer_wait",json!({"task_ref":task,"for":{"op":op},"deadline_ms":5000})).await;
        assert_eq!(result["status"],"ok","{result}");
        assert_eq!(std::fs::read_to_string(workspace.join("starts")).unwrap(),"started\n");
        assert_eq!(std::fs::read_to_string(workspace.join("output")).unwrap(),"built\n");
        let done=as_connection(c,"connection_b","codex","computer_finish",json!({"task_ref":task,"request_id":"finish","outcome":"complete","summary":"build complete"})).await;
        assert_eq!(done["status"],"ok","{done}");
        assert!(!c.storage.has_active_jobs(None));
    });
}

#[test]
fn disconnected_work_releases_after_last_job_finishes_and_preserves_output() {
    run(async {
        let rig=rig(false); let c=&rig.controller; let task=begin(c).await;
        let (_,op)=launch(c,&task).await;
        c.disconnect("vesper","connection_a").await.unwrap();
        rig.clock.fetch_add(IDLE_EXPIRY_MS+1,Ordering::SeqCst);
        c.reconcile().await.unwrap();
        assert!(c.storage.has_active_jobs(Some(&task)));
        let workspace=c.storage.workspace(&task,false).unwrap();
        std::fs::write(workspace.join("release"),b"go").unwrap();
        for _ in 0..100 { if !c.storage.has_active_jobs(Some(&task)) {break;} tokio::time::sleep(Duration::from_millis(25)).await; }
        assert!(!c.storage.has_active_jobs(Some(&task)));
        c.reconcile().await.unwrap();
        assert!(c.journal.get_active_lease().unwrap().is_none());
        let saved=as_connection(c,"connection_b","codex","computer_status",json!({"ref":op})).await;
        assert_eq!(saved["result"]["state"],"done","{saved}");
        assert_eq!(std::fs::read_to_string(workspace.join("output")).unwrap(),"built\n");
    });
}

#[test]
fn explicit_cancel_settles_owned_job_and_preserves_unrelated_process() {
    run(async {
        let rig=rig(false); let c=&rig.controller; let task=begin(c).await;
        let mut sentinel=tokio::process::Command::new("sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        launch(c,&task).await;
        c.disconnect("vesper","connection_a").await.unwrap();
        rig.clock.fetch_add(DISCONNECT_GRACE_MS+1,Ordering::SeqCst);
        let result=as_connection(c,"connection_b","codex","computer_finish",json!({"task_ref":task,"request_id":"cancel","outcome":"cancelled","summary":"explicitly cancelled"})).await;
        assert_eq!(result["status"],"ok","{result}");
        assert!(!c.storage.has_active_jobs(Some(&task)));
        assert!(sentinel.try_wait().unwrap().is_none());
        sentinel.kill().await.unwrap();
    });
}

#[test]
fn default_work_has_no_cumulative_action_or_image_cutoff() {
    run(async {
        let rig=rig(false); let c=&rig.controller; let task=begin(c).await;
        let mut record=c.journal.get_task(&task).unwrap().unwrap();
        for _ in 0..220 { c.charge(&mut record,Charge::Action).unwrap(); }
        for _ in 0..45 { c.charge(&mut record,Charge::Image).unwrap(); }
        assert_eq!(record.actions_used,220);
        assert_eq!(record.images_used,45);
        // An explicit persisted policy still applies.
        record.budgets["max_actions"]=json!(220);
        assert!(c.charge(&mut record,Charge::Action).is_err());
    });
}
