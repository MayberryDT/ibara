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
//!
//! A page that holds the pointer locked (a game's captured mouse):
//! 6. A click is still sent, though the press can only reach the locked
//!    element at the page's own pointer; or the refusal does not name what
//!    holds the pointer and how to release it.
//! 7. A press the lock took is reported as a click that "reached canvas"
//!    without saying why, or as a success.
//!
//! Navigate addresses:
//! 8. A bare host or host and path (`example.com/a`, `localhost:8000/x`) is
//!    refused, or opened as something other than that address over https.
//! 9. A scheme the page reader cannot read (file:, chrome:, about:,
//!    javascript:, mailto:) is opened, or refused without naming the accepted
//!    forms with an example, or a file:// address without the routes that
//!    show a file of the computer.
//!
//! Ambiguous and covered page targets:
//! 10. An ambiguous target's refusal sends the agent to "observe with a
//!    query", which cannot tell lookalikes apart, and names none of them.
//! 11. A covered target's refusal does not say what covers it.
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

#[test]
fn a_page_holding_the_pointer_locked_refuses_a_click_before_any_input() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        let task = observe_page(&rig.controller).await;
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({"locked": "canvas \"Arena\""}));
        let env = call(&rig.controller, "browser_act", click_next(&task, "locked-1")).await;
        assert_eq!(code(&env), "CAPABILITY_UNAVAILABLE", "{env}");
        let message = env["error"]["message"].as_str().unwrap();
        assert!(message.contains("canvas \"Arena\"") && message.contains("nothing was sent"), "{env}");
        assert!(env["error"]["next"].as_str().unwrap().contains("Escape"), "{env}");
        assert_eq!((&env["error"]["execution_not_started"], &env["error"]["reason"]), (&json!(true), &json!("pointer_locked")), "{env}");
        assert_eq!(rig.desktop.acts(), 0, "no press goes to the locked canvas");
        // The page locked the pointer between locating and pressing.
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({ "x": 100, "y": 50, "outer": [1000, 1000], "inner": [1000, 913], "zoom": 1 }));
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({"observed": true, "hit": false, "reached": "canvas \"Arena\"", "locked": "canvas \"Arena\""}));
        let env = call(&rig.controller, "browser_act", click_next(&task, "locked-2")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unknown", "a press at the page's own pointer is no success: {env}");
        assert!(env["result"]["steps"][0]["effect"].as_str().unwrap().contains("locked the pointer"), "{env}");
    });
}

#[test]
fn navigate_opens_a_bare_host_as_https_and_names_the_forms_it_takes() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        rig.desktop.extension.borrow_mut().insert("navigate".into(), json!({"navigated": true}));
        let task = observe_page(&rig.controller).await;
        let go = |url: &str| json!({"task_ref": task, "request_id": id("go"), "action": {"kind": "navigate", "url": url}});
        let opened = || rig.desktop.extension_args.borrow().iter().filter(|(op, _)| op == "navigate").map(|(_, a)| a["url"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        for (given, sent) in [
            ("example.com/a?b=1#c", "https://example.com/a?b=1#c"),
            ("localhost:8000/report.html", "https://localhost:8000/report.html"),
            ("  https://shop.example/cart ", "https://shop.example/cart"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080/"),
        ] {
            let env = call(&rig.controller, "browser_act", go(given)).await;
            assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{given}: {env}");
            assert_eq!(opened().last().map(String::as_str), Some(sent), "{given}");
        }
        let before = opened().len();
        for (given, says) in [
            ("file:///home/tulip1/sheet.png", "computer_files"),
            ("chrome://extensions/", "https://example.com/page"),
            ("about:blank", "https://example.com/page"),
            ("javascript:alert(1)", "https://example.com/page"),
            ("mailto:dana@example.com", "https://example.com/page"),
            ("http:example.com", "https://example.com/page"),
            ("two words", "https://example.com/page"),
        ] {
            let env = call(&rig.controller, "browser_act", go(given)).await;
            assert_eq!(code(&env), "INVALID_ARGUMENT", "{given}: {env}");
            assert!(env["error"]["message"].as_str().unwrap().contains(says), "{given}: {env}");
            assert_eq!(env["error"]["execution_not_started"], true, "{given}: {env}");
        }
        assert_eq!(opened().len(), before, "nothing else opened");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

#[test]
fn an_ambiguous_page_target_names_its_lookalikes_and_the_route_that_tells_them_apart() {
    run(async {
        let rig = browser_rig();
        // Three unnamed fields in one form; the page reader marks them ambiguous.
        let field = |id: &str, token: &str| json!({ "role": "textbox", "name": "", "actions": ["click", "fill"], "states": ["enabled", "ambiguous"], "ancestor": "form:Profile", "elementId": id, "token": token });
        page(&rig, "d1", "c1", json!([field("first", "t1"), field("second", "t2"), field("third", "t3"),
            { "role": "button", "name": "Save", "actions": ["click"], "states": ["enabled"], "elementId": "save", "token": "t4" }]));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", json!({"task_ref": task, "request_id": "ambiguous-1", "action": {"kind": "type", "target": "b2", "text": "Dana"}})).await;
        assert_eq!(code(&env), "AMBIGUOUS_TARGET", "{env}");
        let message = env["error"]["message"].as_str().unwrap();
        assert!(message.contains("b1") && message.contains("b3") && !message.contains("observe with a query"), "names the lookalikes: {env}");
        assert!(env["error"]["next"].as_str().unwrap().contains("view \"image\""), "the picture tells them apart: {env}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

#[test]
fn a_covered_page_target_names_what_covers_it() {
    run(async {
        let rig = browser_rig();
        shop(&rig);
        let task = observe_page(&rig.controller).await;
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({"covered": true, "cover": "dialog \"Cookie choices\""}));
        let env = call(&rig.controller, "browser_act", click_next(&task, "covered-2")).await;
        assert_eq!(code(&env), "BLOCKED_BY_DIALOG", "{env}");
        assert!(env["error"]["message"].as_str().unwrap().starts_with("dialog \"Cookie choices\" covers that element"), "{env}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}
