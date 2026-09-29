//! What a person reads when an agent's step waits for approval, as the
//! console reads it (the operator `attention` list). Failure cases:
//! 1. The words read as code: JSON, a window address, a process id or a tool
//!    name reaches them.
//! 2. They leave out who asks, what the step does, in which app and on which
//!    page (host and path; never the query, which can carry what was typed),
//!    why it asks first, or the task it is for.
//! 3. A click names no button; a file send names no file, place or computer;
//!    a delete does not say it deletes.
//! 4. The details lose the step or the exact target it was held for, so a
//!    Details view and the agent cannot see what exactly is approved.

use super::*;

const GOAL: &str = "Sign up for the newsletter";

/// Begin a task with `GOAL` on a browser rig whose page is `url`, and observe
/// the page so b1 (a "Next" button) and b2 (a "Name" field) name its elements.
async fn on_page(rig: &Rig, url: &str) -> String {
    rig.desktop.extension.borrow_mut().get_mut("observe").unwrap()["url"] = json!(url);
    let begun = call(&rig.controller, "computer_begin", json!({ "goal": GOAL, "request_id": id("req") })).await;
    let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
    let seen = call(&rig.controller, "computer_observe", json!({ "task_ref": task, "surface": "tab", "view": "elements" })).await;
    assert_eq!(seen["status"], "ok", "{seen}");
    task
}

/// The one open approval as the console lists it: its words and its details.
async fn listed(c: &Controller) -> (String, Value) {
    let listed = c.operator_call("vesper", operator_action(c, "attention")).await.unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    (items[0]["summary"].as_str().unwrap().to_string(), items[0]["details"].clone())
}

/// Words a person reads carry none of the machinery behind them.
fn assert_plain(summary: &str) {
    for code in ["{", "}", "0x", "300", "computer_", "browser_act", "\"kind\""] {
        assert!(!summary.contains(code), "{code:?} in {summary:?}");
    }
}

#[test]
fn a_send_key_in_a_browser_page_reads_as_who_what_where_and_why() {
    run(async {
        let rig = browser_rig();
        let task = on_page(&rig, "http://localhost:8080/signup?email=ada%40example.com#form").await;
        // As the agent in the report sent it: a Return key to the browser window.
        let held = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "Return" }, "effect": "send" })).await;
        assert_eq!(held["status"], "pending", "{held}");
        let (summary, details) = listed(&rig.controller).await;
        assert_eq!(
            summary,
            "codex@vesper wants to press Return in Chromium on localhost:8080/signup, which sends something. For the task “Sign up for the newsletter”."
        );
        assert_plain(&summary);
        assert_eq!(details["request"]["step"]["action"], json!({ "kind": "key", "keys": "Return" }), "{details}");
        assert_eq!(details["request"]["target"]["window"], json!({ "address": "0x9", "pid": 300, "class": "chromium" }), "{details}");
        assert_eq!((details["effect"].as_str(), details["where"]["page"].as_str()), (Some("send"), Some("localhost:8080/signup")), "{details}");
    });
}

#[test]
fn a_click_names_the_button_and_the_page() {
    run(async {
        let rig = browser_rig();
        let task = on_page(&rig, "https://shop.example/checkout").await;
        let held = call(&rig.controller, "browser_act", json!({ "task_ref": task, "request_id": "click-1", "action": { "kind": "click", "target": "b1" }, "effect": "spend" })).await;
        assert_eq!(held["status"], "pending", "{held}");
        let (summary, details) = listed(&rig.controller).await;
        assert_eq!(
            summary,
            "codex@vesper wants to click the “Next” button in Chromium on shop.example/checkout, which spends money. For the task “Sign up for the newsletter”."
        );
        assert_plain(&summary);
        assert_eq!(details["request"]["step"]["action"], json!({ "kind": "click", "target": "b1" }), "{details}");
    });
}

#[test]
fn typing_into_a_desktop_window_names_the_app_and_its_window() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let held = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "type-1", "action": { "kind": "type", "text": "Dear Bob," }, "effect": "send" })).await;
        assert_eq!(held["status"], "pending", "{held}");
        let (summary, _) = listed(&rig.controller).await;
        assert_eq!(summary, "codex@vesper wants to type 9 characters in Mousepad (“Untitled 1”), which sends something. For the task “Save a note in the editor”.");
    });
}

#[test]
fn a_file_send_names_the_file_where_it_goes_and_the_computer() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let wrote = call(&rig.controller, "computer_files", json!({ "task_ref": task, "request_id": "write-1", "op": "write", "path": "report.pdf", "text": "report" })).await;
        assert_eq!(wrote["status"], "ok", "{wrote}");
        let send = json!({ "task_ref": task, "request_id": "send-1", "op": "send", "path": "report.pdf", "to": { "host": "Vesper", "path": "/home/riley/Downloads/report.pdf" } });
        let held = call(&rig.controller, "computer_files", send).await;
        assert_eq!(held["status"], "pending", "{held}");
        let (summary, details) = listed(&rig.controller).await;
        assert_eq!(
            summary,
            "codex@vesper wants to send the file “report.pdf” to “/home/riley/Downloads/report.pdf” on vesper. For the task “Save a note in the editor”."
        );
        assert_plain(&summary);
        assert_eq!(details["request"], json!({ "op": "send", "path": "report.pdf", "to": { "host": "vesper", "path": "/home/riley/Downloads/report.pdf" } }));
    });
}

#[test]
fn a_delete_says_it_deletes() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let rm = json!({ "task_ref": task, "request_id": "rm-1", "command": ["rm", "old notes.txt"], "effect": "destructive" });
        let held = call(&rig.controller, "computer_exec", rm).await;
        assert_eq!(held["status"], "pending", "{held}");
        let (summary, details) = listed(&rig.controller).await;
        assert_eq!(
            summary,
            "codex@vesper wants to run the command “rm 'old notes.txt'”, which deletes or overwrites something. For the task “Save a note in the editor”."
        );
        assert_eq!(details["request"]["command"], json!(["rm", "old notes.txt"]), "{details}");
    });
}

#[test]
fn an_approved_step_whose_window_changed_says_why_it_asks_again() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+Return" }, "effect": "send" });
        let held = call(&rig.controller, "computer_act", args.clone()).await;
        let att = held["result"]["attention"].as_str().unwrap().to_string();
        rig.desktop.windows.replace(vec![win("0x9", 301, "foot", "~/mail - Foot", true, false)]);
        rig.controller.admin(json!({ "op": "answer_attention", "att_ref": att, "answer": "approve" })).await.unwrap();
        call(&rig.controller, "computer_act", args).await;
        let (summary, details) = listed(&rig.controller).await;
        assert_eq!(
            summary,
            "codex@vesper wants to press Ctrl+Return in Foot (“~/mail”), which sends something. What it acts on changed since you approved it, so it asks again. For the task “Save a note in the editor”."
        );
        assert_eq!(details["target_changed"], true, "{details}");
    });
}
