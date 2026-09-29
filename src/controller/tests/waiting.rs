//! What an agent reads when its step is held for a person's approval, in both
//! forms it gets: the envelope and the MCP text (what Codex reads). Failure
//! cases:
//! 1. A held act, browser act, command or file send replies with no `next`,
//!    so the agent ends its turn to ask its user, or finishes blocked,
//!    instead of waiting for the person.
//! 2. The next names no `computer_wait` on the approval, or sends the agent
//!    to a new request_id (refused while the approval is open, asked again
//!    once it was answered), so doing what it says never runs the step.
//! 3. The text form leaves the next out or cuts it short.
//! 4. The held step's own line says only that it did not run.
//! 5. Doing what the next says (wait, a person approves, the same request
//!    again) does not run the step, or runs it twice.

use super::*;

/// The envelope and the MCP text of one call.
async fn both(c: &Controller, tool: &str, args: Value) -> (Value, String) {
    let outcome = c.call("vesper", "connection_a", "codex", tool, args, Cancel::new()).await;
    let text = crate::mcp::call_result(&outcome)["content"][0]["text"].as_str().unwrap().to_string();
    (outcome.envelope, text)
}

/// A held reply tells the agent, in both forms, to wait for this approval
/// and then send the same request again. Returns the approval.
fn says_to_wait(tool: &str, (env, text): &(Value, String), task: &str) -> String {
    assert_eq!(env["status"], "pending", "{tool} is held: {env}");
    let att = env["result"]["attention"].as_str().unwrap().to_string();
    let next = env["result"]["next"].as_str().unwrap_or_else(|| panic!("{tool}: a held reply says what to do next: {env}"));
    let wait = format!("computer_wait({{task_ref: \"{task}\", for: {{attention: \"{att}\"}}, deadline_ms: 50000}})");
    assert!(next.contains(&wait), "{tool}: names the wait for its approval: {next}");
    for words in ["send this same request again (same request_id)", "it runs once if approved", "Don't end your turn or ask your user"] {
        assert!(next.contains(words), "{tool}: {words:?} in {next}");
    }
    assert!(text.lines().any(|line| line == format!("next: {next}")), "{tool}: the text form carries the whole next: {text}");
    att
}

/// Wait as the next says, once a person approved.
async fn approved(c: &Controller, task: &str, att: &str) {
    answer(c, att, "approve").await;
    let waited = call(c, "computer_wait", json!({ "task_ref": task, "for": { "attention": att }, "deadline_ms": 50000 })).await;
    assert_eq!((waited["status"].as_str(), waited["result"]["answer"].as_str()), (Some("ok"), Some("approve")), "{waited}");
}

#[test]
fn a_held_step_says_to_wait_and_runs_once_when_sent_again() {
    run(async {
        let rig = browser_rig();
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": true, "hit": true }));
        rig.desktop.extension.borrow_mut().insert("keys".into(), json!({ "page": true, "submits": false }));
        let c = &rig.controller;
        let task = observe_page(c).await;
        let desktop = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+Return" }, "effect": "send" });
        let page = json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "click", "target": "b1" }, "effect": "send" });

        for (n, (tool, args)) in [("browser_act", page), ("computer_act", desktop)].into_iter().enumerate() {
            let held = both(c, tool, args.clone()).await;
            let att = says_to_wait(tool, &held, &task);
            let step = held.0["result"]["steps"][0]["effect"].as_str().unwrap();
            assert!(step.contains(&att) && step.contains("send this same request again"), "{tool}: the step says it waits: {step}");
            let still = both(c, tool, args.clone()).await;
            assert_eq!(says_to_wait(tool, &still, &task), att, "{tool}: sent again while open, it still waits on the same approval");
            assert_eq!(rig.desktop.acts(), n, "{tool}: nothing runs before a person approves");

            approved(c, &task, &att).await;
            let ran = call(c, tool, args.clone()).await;
            assert_eq!((ran["status"].as_str(), ran["result"]["steps"][0]["outcome"].as_str()), (Some("ok"), Some("done")), "{tool}: {ran}");
            assert!(ran["result"].get("next").is_none(), "{tool}: nothing left to wait for: {ran}");
            call(c, tool, args).await;
            assert_eq!(rig.desktop.acts(), n + 1, "{tool}: the approved step runs once");
        }
    });
}

#[test]
fn a_held_command_says_to_wait_and_runs_once_when_sent_again() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let args = json!({ "task_ref": task, "request_id": "exec-1", "command": ["sh", "-c", "echo sent >> log.txt"], "effect": "send" });
        let log = c.storage.workspace(&task, false).unwrap().join("log.txt");

        let held = both(c, "computer_exec", args.clone()).await;
        let att = says_to_wait("computer_exec", &held, &task);
        assert!(!log.exists(), "a held command does not run");

        approved(c, &task, &att).await;
        let ran = call(c, "computer_exec", args.clone()).await;
        assert_eq!(ran["status"], "ok", "{ran}");
        assert!(ran["result"].get("next").is_none(), "{ran}");
        call(c, "computer_exec", args).await;
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "sent\n", "the approved command runs once");
    });
}

#[test]
fn a_held_file_send_says_to_wait_and_sends_once_when_sent_again() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let wrote = call(c, "computer_files", json!({ "task_ref": task, "request_id": "w-1", "op": "write", "path": "report.txt", "text": "report\n" })).await;
        assert_eq!(wrote["status"], "ok", "{wrote}");
        let args = json!({ "task_ref": task, "request_id": "send-1", "op": "send", "path": "report.txt", "to": { "host": "vesper", "path": "/home/riley/Downloads/report.txt" } });

        let held = both(c, "computer_files", args.clone()).await;
        let att = says_to_wait("computer_files", &held, &task);

        approved(c, &task, &att).await;
        let sent = call(c, "computer_files", args.clone()).await;
        assert_eq!(sent["status"], "ok", "{sent}");
        let receipt = sent["result"]["receipt"].as_str().unwrap().to_string();
        let again = call(c, "computer_files", args).await;
        assert_eq!(again["result"]["receipt"], receipt, "the approved send is sent once: {again}");
    });
}
