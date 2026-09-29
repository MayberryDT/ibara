//! A person's save in the console as an agent's delivery, through the agent's
//! own calls and the console's `artifact_transfer`. Failure cases, written first:
//! - a save of the sent file to exactly the task's destination leaves the
//!   delivery pending, and the task finishes incomplete
//! - a save of the sent file somewhere else verifies the delivery
//! - a save that claims other bytes verifies the delivery
//! - after a person moves the destination, a save to the old one verifies it,
//!   or one to the new one does not
//!
//! And an agent's send, through its own calls and its collector's transfer
//! session. Failure cases, written first:
//! - a send to a computer ibara cannot deliver to, to a relative or
//!   unnormalized destination, or of a file that is not there, is held for a
//!   person, who approves it, and only then refused
//! - the refusal does not say which computer a send can reach
//! - the computer the agent works from is refused under its short name, in
//!   another case, or by its `cmp_` or `computer_` id
//! - a delivery named at begin under one of its names and a send under
//!   another are two deliveries, so the begin's is never verified
//! - a send, the sent file's status or the task's status says the file is
//!   delivered, or says nothing of how its bytes move
//! - the collector's fetch, run as the send says, leaves the delivery pending

use super::*;
use crate::operator::client::{TransferLink, collect};
use std::pin::Pin;

const BODY: &str = "delivery report\n";

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// An agent task that must deliver `report.txt` to `host:path`, with the file
/// sent there (the held send approved by a person). Returns the task and the
/// sent file's reference.
async fn sent(c: &Controller, tag: &str, host: &str, path: &str) -> (String, String) {
    let begun = call(c, "computer_begin", json!({ "goal": "Deliver the report", "request_id": format!("b-{tag}"), "deliver": { "host": host, "path": path } })).await;
    assert_eq!(begun["status"], "ok", "{begun}");
    let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
    let wrote = call(c, "computer_files", json!({ "task_ref": task, "request_id": format!("w-{tag}"), "op": "write", "path": "report.txt", "text": BODY })).await;
    assert_eq!(wrote["status"], "ok", "{wrote}");
    let args = json!({ "task_ref": task, "request_id": format!("s-{tag}"), "op": "send", "path": "report.txt", "to": { "host": host, "path": path } });
    let mut send = call(c, "computer_files", args.clone()).await;
    if send["status"] == "pending" {
        let att = send["result"]["attention"].as_str().unwrap().to_string();
        c.admin(json!({ "op": "answer_attention", "att_ref": att, "answer": "approve" })).await.unwrap();
        send = call(c, "computer_files", args).await;
    }
    assert_eq!(send["status"], "ok", "{send}");
    assert_eq!(send["result"]["state"], "pending", "{send}");
    (task, send["result"]["receipt"].as_str().unwrap().to_string())
}

/// The console's acknowledgement after saving `artifact` to `to` (what `collect`
/// sends once the bytes are on disk and checked).
async fn console_saved(c: &Controller, artifact: &str, sha256: &str, to: &str) -> Result<Value> {
    let mut action = operator_action(c, "artifact_transfer");
    action["request"] = json!({ "kind": "ack_collected", "artifact_ref": artifact, "size_bytes": BODY.len(), "sha256": sha256, "destination_path": to });
    c.operator_call("vesper", action).await
}

async fn delivery(c: &Controller, task: &str) -> Value {
    let view = c.admin(json!({ "op": "task", "task_ref": task })).await.unwrap();
    view["deliveries"][0].clone()
}

async fn finish(c: &Controller, task: &str) -> Value {
    let done = call(c, "computer_finish", json!({ "task_ref": task, "request_id": format!("f-{task}"), "outcome": "complete", "summary": "Delivered the report." })).await;
    assert_eq!(done["status"], "ok", "{done}");
    done["result"].clone()
}

#[test]
fn a_console_save_to_the_exact_destination_is_the_delivery() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let sha = digest(BODY.as_bytes());
        c.admin(json!({ "op": "register_collector", "principal": "vesper", "host_id": "tulip1-console" })).await.unwrap();
        let (task, artifact) = sent(c, "a", "tulip1-console", "/home/riley/Downloads/report.txt").await;

        let other = console_saved(c, &artifact, &digest(b"something else"), "/home/riley/Downloads/report.txt").await.unwrap_err();
        assert_eq!(other.code, "POSTCONDITION_FAILED", "other bytes are refused");
        assert_eq!(delivery(c, &task).await["state"], "pending");

        let elsewhere = console_saved(c, &artifact, &sha, "/home/riley/Desktop/report.txt").await.unwrap();
        assert_eq!(elsewhere["delivery"], "collected", "{elsewhere}");
        assert_eq!(delivery(c, &task).await["state"], "pending", "a save elsewhere is not the delivery");

        console_saved(c, &artifact, &sha, "/home/riley/Downloads/report.txt").await.unwrap();
        assert_eq!(delivery(c, &task).await["state"], "verified");
        let done = finish(c, &task).await;
        assert_eq!(done["complete"], true, "{done}");
        assert_eq!(done["delivery"]["state"], "verified", "{done}");
    });
}

#[test]
fn after_the_destination_moves_only_a_console_save_to_the_new_one_delivers() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let sha = digest(BODY.as_bytes());
        c.admin(json!({ "op": "register_collector", "principal": "vesper", "host_id": "tulip1-console" })).await.unwrap();
        let (task, artifact) = sent(c, "m", "tulip1-console", "/home/riley/Downloads/first.txt").await;
        let before = delivery(c, &task).await;
        c.admin(json!({ "op": "amend_delivery", "task_ref": task, "obligation_id": before["id"], "expected_revision": before["revision"],
                         "host_id": "tulip1-console", "destination_path": "/home/riley/Documents/amended.txt" }))
            .await
            .unwrap();

        console_saved(c, &artifact, &sha, "/home/riley/Downloads/first.txt").await.unwrap();
        assert_eq!(delivery(c, &task).await["state"], "pending", "the old destination no longer delivers");
        let early = finish(c, &task).await;
        assert_eq!(early["complete"], false, "{early}");

        console_saved(c, &artifact, &sha, "/home/riley/Documents/amended.txt").await.unwrap();
        let after = delivery(c, &task).await;
        assert_eq!(after["state"], "verified", "{after}");
        assert_eq!(after["revision"], 2);
    });
}

/// `vesper`'s ids: `cmp_` as it names itself, `computer_` as a console names
/// it; both the first 24 hex of its endpoint id's digest.
fn vesper_ids() -> (String, String) {
    let hex = &digest(VESPER_ENDPOINT.as_bytes())[..24];
    (format!("cmp_{hex}"), format!("computer_{hex}"))
}

async fn with_report(c: &Controller, tag: &str) -> String {
    let task = begin(c).await;
    let wrote = call(c, "computer_files", json!({ "task_ref": task, "request_id": format!("w-{tag}"), "op": "write", "path": "report.txt", "text": BODY })).await;
    assert_eq!(wrote["status"], "ok", "{wrote}");
    task
}

fn send(task: &str, id: &str, path: &str, host: &str, to: &str) -> Value {
    json!({ "task_ref": task, "request_id": id, "op": "send", "path": path, "to": { "host": host, "path": to } })
}

fn open_approvals(c: &Controller) -> usize {
    c.journal.list_attention(Some("open"), None, 50).unwrap().len()
}

/// The agent's collector: its transfer session to this computer, as `vesper`.
struct Collector(crate::storage::StorageService);

impl TransferLink for Collector {
    fn request(&mut self, request: Value) -> Pin<Box<dyn Future<Output = crate::error::Result<Value>> + Send + '_>> {
        let reply = self.0.transfer("vesper", &request, Some("transfer_collector"));
        Box::pin(async move { reply })
    }
}

#[test]
fn a_send_that_cannot_happen_is_refused_before_a_person_is_asked() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = with_report(c, "r").await;
        let (cmp, computer) = vesper_ids();
        let to = "/home/riley/Downloads/report.txt";

        for host in ["orchid1", "Tulip1", "juniper-1"] {
            let refused = call(c, "computer_files", send(&task, &format!("s-{host}"), "report.txt", host, to)).await;
            assert_eq!(code(&refused), "INVALID_ARGUMENT", "{refused}");
            assert_eq!(refused["error"]["execution_not_started"], true, "{refused}");
            let said = refused["error"]["message"].as_str().unwrap();
            assert!(said.starts_with("to.host:") && said.contains(&format!("\"{host}\"")), "{said}");
            assert!(said.contains("\"vesper\"") && said.contains(&cmp) && said.contains(&computer), "names the computer a send can reach and its ids: {said}");
        }
        for (id, path, to, field) in [
            ("s-rel", "report.txt", "Downloads/report.txt", "to.path"),
            ("s-dots", "report.txt", "/home/riley/../riley/report.txt", "to.path"),
            ("s-slashes", "report.txt", "/home/riley//report.txt", "to.path"),
            ("s-missing", "missing.txt", to, "path"),
        ] {
            let refused = call(c, "computer_files", send(&task, id, path, "vesper", to)).await;
            assert_eq!(code(&refused), "INVALID_ARGUMENT", "{id}: {refused}");
            assert_eq!(refused["error"]["execution_not_started"], true, "{id}: {refused}");
            assert!(refused["error"]["message"].as_str().unwrap().starts_with(&format!("{field}:")), "{id}: {refused}");
        }
        assert_eq!(open_approvals(c), 0, "no person is asked about a send that cannot happen");
        assert!(c.storage.artifacts(&task, "vesper").unwrap().is_empty(), "nothing was published");
    });
}

#[test]
fn the_computer_the_agent_works_from_answers_to_each_of_its_names() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = with_report(c, "n").await;
        let (cmp, computer) = vesper_ids();
        for (i, host) in ["vesper", "Vesper", "VESPER-1", "vesper-1.tail5d.ts.net", cmp.as_str(), computer.as_str()].into_iter().enumerate() {
            let held = call(c, "computer_files", send(&task, &format!("s-{i}"), "report.txt", host, &format!("/home/riley/Downloads/report-{i}.txt"))).await;
            assert_eq!(held["status"], "pending", "{host}: {held}");
            let att = c.journal.get_attention(held["result"]["attention"].as_str().unwrap()).unwrap().unwrap();
            let asked = serde_json::to_string(&att).unwrap();
            assert!(asked.contains("on vesper") && asked.contains(r#""host":"vesper""#), "{host}: a person is asked about vesper: {asked}");
        }
    });
}

#[test]
fn a_send_says_how_its_bytes_move_and_the_collectors_fetch_delivers_it() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let (cmp, _) = vesper_ids();
        let dest = rig.dir.join("inbox").join("report.txt").to_string_lossy().into_owned();
        let begun = call(
            c,
            "computer_begin",
            json!({ "goal": "Bring the report home", "request_id": "b-1", "deliver": { "host": "Vesper", "path": dest },
                    "checks": [{ "id": "home", "description": "the report is home", "check": { "kind": "delivered", "host": "vesper-1", "path": dest } }] }),
        )
        .await;
        assert_eq!(begun["status"], "ok", "{begun}");
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        call(c, "computer_files", json!({ "task_ref": task, "request_id": "w-1", "op": "write", "path": "report.txt", "text": BODY })).await;

        let args = send(&task, "s-1", "report.txt", &cmp, &dest);
        let held = call(c, "computer_files", args.clone()).await;
        assert_eq!(held["status"], "pending", "{held}");
        c.admin(json!({ "op": "answer_attention", "att_ref": held["result"]["attention"], "answer": "approve" })).await.unwrap();
        let outcome = c.call("vesper", "connection_a", "codex", "computer_files", args, Cancel::new()).await;
        let sent = outcome.envelope.clone();
        assert_eq!((sent["status"].as_str(), sent["result"]["state"].as_str()), (Some("ok"), Some("pending")), "{sent}");
        assert_eq!(sent["result"]["to"]["host"], "vesper", "{sent}");
        let artifact = sent["result"]["receipt"].as_str().unwrap().to_string();

        // How the bytes move, word for word the same in every place that says it.
        let fetch = format!("ibara client --computer {} fetch {artifact} {dest}", c.computer_id);
        let next = sent["result"]["next"].as_str().unwrap_or_else(|| panic!("the send says what comes next: {sent}"));
        assert!(next.contains("No bytes have moved yet") && next.contains(&fetch) && next.contains("verified only then"), "{next}");
        let text = crate::mcp::call_result(&outcome)["content"][0]["text"].as_str().unwrap().to_string();
        assert!(text.lines().any(|line| line == format!("next: {next}")), "the text form carries it whole: {text}");
        let art_status = call(c, "computer_status", json!({ "ref": artifact })).await;
        let art_next = art_status["result"]["next"].to_string();
        assert!(art_next.contains(&fetch) && !art_next.contains("delivers it"), "{art_status}");
        let task_status = call(c, "computer_status", json!({ "ref": task })).await;
        assert!(task_status["result"]["next"].to_string().contains(&fetch), "{task_status}");
        let delivery = c.admin(json!({ "op": "task", "task_ref": task })).await.unwrap()["deliveries"].clone();
        assert_eq!(delivery.as_array().map(Vec::len), Some(1), "the begin's delivery and the send's are one: {delivery}");

        // The collector runs what the send said.
        let words: Vec<&str> = fetch.split(' ').collect();
        collect(&mut Collector(c.storage.clone()), words[5], Path::new(words[6])).await.unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), BODY);
        assert_eq!(c.admin(json!({ "op": "task", "task_ref": task })).await.unwrap()["deliveries"][0]["state"], "verified");
        let art_after = call(c, "computer_status", json!({ "ref": artifact })).await;
        assert!(!art_after["result"]["next"].to_string().contains("fetch"), "nothing is left to move: {art_after}");
        let done = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f-1", "outcome": "complete", "summary": "Brought the report home." })).await;
        assert_eq!(done["result"]["complete"], true, "{done}");
    });
}

#[test]
fn a_task_cannot_promise_a_delivery_a_send_cannot_make() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let deliver = call(c, "computer_begin", json!({ "goal": "g", "request_id": "b-1", "deliver": { "host": "orchid1", "path": "/home/riley/x.txt" } })).await;
        assert_eq!(code(&deliver), "INVALID_ARGUMENT", "{deliver}");
        assert!(deliver["error"]["message"].as_str().unwrap().starts_with("deliver.host:"), "{deliver}");
        let check = json!([{ "id": "home", "description": "d", "check": { "kind": "delivered", "host": "orchid1", "path": "/home/riley/x.txt" } }]);
        let checked = call(c, "computer_begin", json!({ "goal": "g", "request_id": "b-2", "checks": check })).await;
        assert_eq!(code(&checked), "INVALID_ARGUMENT", "{checked}");
        assert!(checked["error"]["message"].as_str().unwrap().starts_with("checks[0].check.host:"), "{checked}");
    });
}
