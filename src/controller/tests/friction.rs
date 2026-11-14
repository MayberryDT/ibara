//! Refusals agents met most in the fleet's journals (November 2026). Failure
//! cases, written before the changes:
//!
//! Refused page targets:
//! 1. The page as it is now comes with a refused page target only as a cut
//!    JSON fragment in the text an agent reads, so the agent observes again.
//! 2. A covered page target brings no fresh page at all; or a page target
//!    refused through computer_act brings a new frame, which renumbers the
//!    windows, native elements and choices its next call may still name.
//! 3. The page reader's reason is lost: the frame holding the element is
//!    loading, the tab lost focus or closed, or which part of the element
//!    changed; the message blames "the page or the tab" either way, and does
//!    not say that nothing was sent.
//! 4. A page read that found no controls for a moment makes the elements of
//!    the page's earlier observations unknown, though the reader still holds
//!    and checks them.
//! 5. ...while an element of another document must stay refused, unasked.
use super::*;

fn text(env: &Value) -> String {
    crate::contract::render_text(&serde_json::from_value(env.clone()).unwrap())
}

/// The page reader's observations, as it sends them, with `revalidates_refs`.
fn page(rig: &Rig, document: &str, capture: &str, nodes: Value) {
    let count = nodes.as_array().map_or(0, Vec::len);
    rig.desktop.extension.borrow_mut().insert("observe".into(), json!({
        "capture": capture, "documentId": document, "url": "https://shop.example/", "title": "Shop",
        "count": count, "nodes": nodes, "revalidates_refs": true }));
}

fn shop(rig: &Rig) {
    page(rig, "d1", "c1", json!([
        { "role": "button", "name": "Next", "actions": ["click"], "states": ["enabled"], "elementId": "next", "token": "t1" },
        { "role": "textbox", "name": "Name", "actions": ["click", "fill"], "states": ["enabled"], "elementId": "name", "token": "t2" },
    ]));
}

#[test]
fn a_refused_page_target_shows_the_page_now_in_the_text_an_agent_reads() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        let task = observe_page(&rig.controller).await;
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({"refused": true, "why": "gone"}));
        let env = call(&rig.controller, "browser_act", click_next(&task, "gone-1")).await;
        assert_eq!(code(&env), "STALE_TARGET", "{env}");
        let shown = text(&env);
        assert!(shown.lines().any(|l| l.starts_with("b2 textbox \"Name\"")), "the page now, one control a line: {shown}");
        assert!(!shown.contains("recovery_frame: {"), "no cut JSON: {shown}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

#[test]
fn a_covered_page_target_brings_the_page_now_and_computer_act_keeps_its_frame() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        let task = observe_page(&rig.controller).await;
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({"covered": true}));
        let env = call(&rig.controller, "browser_act", click_next(&task, "covered-1")).await;
        assert_eq!(code(&env), "BLOCKED_BY_DIALOG", "{env}");
        assert!(env["error"]["recovery_frame"]["lines"].to_string().contains("b1 button"), "{env}");
        // Through computer_act the agent's frame stays the latest: its next
        // call may name a window or choice of it.
        let shown = call(&rig.controller, "computer_observe", json!({"task_ref": task})).await;
        let refused = call(&rig.controller, "computer_act", json!({"task_ref": task, "request_id": "native-route", "action": {"kind": "click", "target": "b1"}})).await;
        assert_eq!(code(&refused), "BLOCKED_BY_DIALOG", "{refused}");
        assert!(refused["error"].get("recovery_frame").is_none(), "{refused}");
        let again = call(&rig.controller, "computer_status", json!({"ref": shown["result"]["frame"]["frame_ref"]})).await;
        assert_eq!(again["result"]["state"], "current", "the observed frame is still the latest: {again}");
        assert_eq!(rig.desktop.acts(), 0, "nothing was sent");
    });
}

#[test]
fn the_page_readers_reason_reaches_the_agent() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        let task = observe_page(&rig.controller).await;
        for (answer, says) in [
            (json!({"refused": true, "why": "loading"}), "is loading another address"),
            (json!({"refused": true, "why": "unfocused"}), "not the active tab"),
            (json!({"refused": true, "why": "closed"}), "tab was closed"),
            (json!({"refused": true, "why": "navigated"}), "went to another document"),
            (json!({"refused": true, "why": "changed", "changed": "context"}), "text around that element"),
            (json!({"refused": true, "why": "changed", "changed": "name"}), "name changed"),
            (json!({"refused": true, "why": "changed", "changed": "disabled"}), "disabled now"),
            (json!({"refused": true, "why": "changed", "changed": "hidden"}), "hidden now"),
        ] {
            rig.desktop.extension.borrow_mut().insert("locate".into(), answer.clone());
            let env = call(&rig.controller, "browser_act", click_next(&task, &id("why"))).await;
            let message = env["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(says) && message.contains("nothing was sent"), "{answer}: {env}");
            assert!(!message.contains("or the tab is not focused"), "{answer}: {env}");
        }
        assert_eq!(rig.desktop.acts(), 0);
    });
}

#[test]
fn a_page_read_with_no_controls_keeps_the_pages_earlier_elements_but_not_another_documents() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({"observed": true, "hit": true}));
        let task = observe_page(&rig.controller).await;
        // The page shows no controls for a moment, as a view changes over.
        page(&rig, "d1", "c2", json!([]));
        let shown = call(&rig.controller, "computer_observe", json!({"task_ref": task, "surface": "tab", "view": "elements"})).await;
        assert!(shown["result"]["frame"]["covered"].as_str().unwrap().ends_with("0 page controls"), "{shown}");
        let env = call(&rig.controller, "browser_act", click_next(&task, "after-empty")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "the reader still holds b1 and checks it: {env}");
        let located = rig.desktop.extension_args.borrow().iter().rev().find(|(op, _)| op == "locate").map(|(_, a)| a.clone()).unwrap();
        assert_eq!((located["capture"].as_str(), located["token"].as_str()), (Some("c1"), Some("t1")), "{located}");
        // Another document with no controls: b1 belonged to the one before.
        page(&rig, "d2", "c3", json!([]));
        call(&rig.controller, "computer_observe", json!({"task_ref": task, "surface": "tab", "view": "elements"})).await;
        let locates = || rig.desktop.extension_ops.borrow().iter().filter(|op| *op == "locate").count();
        let before = locates();
        let env = call(&rig.controller, "browser_act", click_next(&task, "other-document")).await;
        assert_eq!(code(&env), "STALE_TARGET", "{env}");
        assert_eq!(locates(), before, "nothing asked of the new document");
    });
}
