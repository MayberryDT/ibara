//! What agents reported in November 2026 (docs/agent-feedback-failure-cases.md).
//! Failure cases, written before the changes; the numbers are that page's.
//!
//! A typing stop in a page field:
//! 7. The step tells an agent that typed into a page field only that typing
//!    the whole text again replaces the field, not how to add only the rest.
//!
//! Text beyond ASCII where the caret is:
//! 8. Typed into the browser without a target, it is sent although no page
//!    field has the keyboard (the address bar could take it).
//! 9. It replaces what the field holds instead of adding at the caret.
//! 10. A piece is sent before the paste before it showed in the page; a paste
//!    that never shows is reported done, or more is sent after it.
//! 11. The person's clipboard is not put back.
//! 12. The refusal for such text through computer_act does not say that a
//!    field's type replaces its text, or how to add at the caret.
//!
//! Scrolling a list in a page:
//! 19. A page scroll with an element as its target sends the wheel with no
//!    place, so the window's centre scrolls and a list given as the target
//!    does not.
//! 20. A page scroll whose target cannot be pointed at fails where it worked
//!    before, instead of scrolling the window as before.
//!
//! Refusals:
//! 22. exec with too many or too long arguments does not say the limits or
//!    which was exceeded.
//! 23. send without to.host is refused, or goes anywhere but the computer the
//!    agent works from.
use super::*;

/// `browser_act` type with no target: into whatever has the keyboard.
fn type_at_caret(task: &str, request_id: &str, text: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "type", "text": text } })
}

/// A page whose Name field has the keyboard focus and holds `value`.
fn focused_field_rig(value: &str) -> Rig {
    let rig = field_rig(true, value);
    rig.desktop.field.borrow_mut().focused = true;
    rig
}

#[test]
fn a_page_field_typed_into_part_of_the_way_says_how_to_add_only_the_rest() {
    run(async {
        let rig = field_rig(true, "");
        // Cua's account of a field that took the keyboard focus again: 4 keys arrived.
        let ended = crate::error::unavailable("Cua's input connection ended part way.")
            .with("reason", "text_interrupted")
            .with("typed_chars", 4)
            .with("unsure_chars", 0)
            .with("execution_not_started", false);
        rig.desktop.fail_on.replace(Some((r#"text: "Forty minutes in mid""#.into(), ended)));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Forty minutes in mid")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        let said = step["effect"].as_str().unwrap();
        assert!(said.contains("4 of the 20"), "how much went through: {said}");
        assert!(said.contains("other 16") && said.contains("without a target"), "how to add only the rest, at the caret: {said}");
        assert!(said.contains("replaces"), "and that typing it all again into the field replaces it: {said}");
    });
}

#[test]
fn text_beyond_ascii_typed_without_a_target_is_added_where_the_caret_is() {
    run(async {
        let rig = focused_field_rig("Dear ");
        let person = SavedClipboard(vec![("image/png".into(), vec![137, 80, 78, 71, 0, 255])]);
        rig.desktop.selection.replace(person.clone());
        let task = observe_page(&rig.controller).await;
        let text = "Zoë Ångström, hello";
        let env = call(&rig.controller, "browser_act", type_at_caret(&task, "act-1", text)).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        assert_eq!(rig.desktop.field.borrow().value, format!("Dear {text}"), "added after what the field held, every character in place");
        assert_eq!(*rig.desktop.selection.borrow(), person, "the person's clipboard is back, every type and byte");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(!acts.iter().any(|a| a.contains("ctrl+a")), "nothing is selected, so nothing is replaced: {acts:?}");
        assert!(!acts.iter().any(|a| a.starts_with("ClickPoint")), "no click: the caret stays where it is: {acts:?}");
        assert!(acts.iter().all(|a| !a.starts_with("Type") || a.is_ascii()), "Cua is asked to type ASCII only: {acts:?}");
        assert_eq!(acts.iter().filter(|a| a.contains("ctrl+v")).count(), 2, "one paste per run beyond ASCII: {acts:?}");
        let pieces = ["Zoë", "Ångström"];
        assert!(!pieces.iter().any(|p| env.to_string().contains(p)), "{env}");
        assert_eq!(stored_anywhere(&rig.dir, &pieces), Vec::<String>::new(), "typed text is stored nowhere");
    });
}

#[test]
fn text_beyond_ascii_without_a_target_is_refused_when_no_page_field_has_the_keyboard() {
    // The address bar has it (the page's field is not focused), or the page
    // reader is an earlier one that cannot say.
    for reader_knows in [true, false] {
        run(async {
            let rig = field_rig(true, "kept");
            if !reader_knows {
                rig.desktop.field.borrow_mut().present = false;
            }
            let person = SavedClipboard(vec![(PASTED.into(), b"the person's own copy".to_vec())]);
            rig.desktop.selection.replace(person.clone());
            let task = observe_page(&rig.controller).await;
            let env = call(&rig.controller, "browser_act", type_at_caret(&task, "act-1", "Zoë")).await;
            assert_eq!(code(&env), "INVALID_ARGUMENT", "reader knows {reader_knows}: {env}");
            assert_eq!(env["error"]["execution_not_started"], true, "{env}");
            let said = format!("{} {}", env["error"]["message"].as_str().unwrap(), env["error"]["next"].as_str().unwrap_or_default());
            assert!(said.contains("target") && said.contains("click"), "name the field, or click into it first: {said}");
            assert_eq!(rig.desktop.acts(), 0, "nothing was sent: {:?}", rig.desktop.acts.borrow());
            assert_eq!(*rig.desktop.selection.borrow(), person, "the clipboard is untouched");
        });
    }
}

#[test]
fn a_paste_at_the_caret_that_never_shows_is_unknown_and_nothing_follows_it() {
    run(async {
        let rig = focused_field_rig("Dear ");
        rig.desktop.field.borrow_mut().ignores_paste = true;
        let person = SavedClipboard(vec![(PASTED.into(), b"the person's own copy".to_vec())]);
        rig.desktop.selection.replace(person.clone());
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_at_caret(&task, "act-1", "Zoë Ångström")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        assert!(step["effect"].as_str().unwrap().contains("2 of the 12"), "what went through before the paste: {env}");
        assert_eq!(rig.desktop.field.borrow().value, "Dear Zo", "nothing after the missing paste");
        assert_eq!(*rig.desktop.selection.borrow(), person, "the person's clipboard is back");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(acts.last().is_some_and(|a| a.contains("ctrl+v")), "nothing more is sent after the missing paste: {acts:?}");
    });
}

#[test]
fn text_beyond_ascii_into_the_browser_window_says_what_each_route_does() {
    run(async {
        let rig = focused_field_rig("kept");
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "computer_act", type_at_caret(&task, "act-1", "Zoë")).await;
        assert_eq!(code(&env), "WRONG_TOOL", "{env}");
        let said = format!("{} {}", env["error"]["message"].as_str().unwrap(), env["error"]["next"].as_str().unwrap_or_default());
        assert!(said.contains("replaces"), "a field's type replaces its text: {said}");
        assert!(said.contains("without a target") && said.contains("caret"), "browser_act type without a target adds at the caret: {said}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

/// Where the page reader says an element is, as `locate` and `reveal` give it.
fn at_100_50() -> Value {
    json!({ "revealed": true, "x": 100, "y": 50, "outer": [1000, 1000], "inner": [1000, 913], "zoom": 1 })
}

fn scroll_name(task: &str, request_id: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "scroll", "target": "b2", "dy": 3 } })
}

#[test]
fn a_page_scroll_with_a_target_sends_the_wheel_over_that_element() {
    run(async {
        let rig = browser_rig();
        rig.desktop.extension.borrow_mut().insert("reveal".into(), at_100_50());
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", scroll_name(&task, "act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let acts = rig.desktop.acts.borrow().clone();
        assert_eq!(acts.len(), 1, "{acts:?}");
        // The page starts 87 pixels below the window's top (its toolbars).
        assert!(acts[0].starts_with("Scroll") && acts[0].contains("100.0, 137.0") && acts[0].contains("dy: 3"), "the wheel goes over the element, so its own list scrolls: {acts:?}");
    });
}

#[test]
fn a_page_scroll_whose_target_cannot_be_pointed_at_scrolls_the_window_as_before() {
    // The element is covered, or the reader is an earlier one: no place comes back.
    run(async {
        let rig = browser_rig();
        rig.desktop.extension.borrow_mut().insert("reveal".into(), json!({ "revealed": true }));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", scroll_name(&task, "act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(acts.len() == 1 && acts[0].starts_with("Scroll") && acts[0].contains("at: None") && acts[0].contains("dy: 3"), "{acts:?}");
    });
}

#[test]
fn exec_arguments_past_a_limit_are_refused_with_the_limit_and_which_one() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let exec = |id: &str, command: Vec<String>| json!({ "task_ref": task, "request_id": id, "command": command });

        let many: Vec<String> = std::iter::once("echo".to_string()).chain((0..140).map(|n| n.to_string())).collect();
        let refused = call(c, "computer_exec", exec("x-many", many)).await;
        assert_eq!(code(&refused), "INVALID_ARGUMENT", "{refused}");
        let said = refused["error"]["message"].as_str().unwrap();
        assert!(said.contains("140") && said.contains("100"), "how many it has and how many it may have: {said}");

        let long = vec!["sh".to_string(), "-c".to_string(), "x".repeat(5210)];
        let refused = call(c, "computer_exec", exec("x-long", long)).await;
        assert_eq!(code(&refused), "INVALID_ARGUMENT", "{refused}");
        let said = format!("{} {}", refused["error"]["message"].as_str().unwrap(), refused["error"]["next"].as_str().unwrap_or_default());
        assert!(said.contains("5210") && said.contains("4096"), "how long it is and how long it may be: {said}");
        assert!(said.contains("argument 2"), "which argument: {said}");
        assert!(said.contains("computer_files"), "the way that has no such limit, a file: {said}");
    });
}

#[test]
fn a_send_without_a_host_goes_to_the_computer_the_agent_works_from() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let dest = rig.dir.join("inbox").join("report.txt").to_string_lossy().into_owned();
        call(c, "computer_files", json!({ "task_ref": task, "request_id": "w-1", "op": "write", "path": "report.txt", "text": "The report.\n" })).await;

        let args = json!({ "task_ref": task, "request_id": "s-1", "op": "send", "path": "report.txt", "to": { "path": dest } });
        let held = call(c, "computer_files", args.clone()).await;
        assert_eq!(held["status"], "pending", "asked of a person as any send is, not refused: {held}");
        c.admin(json!({ "op": "answer_attention", "att_ref": held["result"]["attention"], "answer": "approve" })).await.unwrap();
        let sent = call(c, "computer_files", args).await;
        assert_eq!((sent["status"].as_str(), sent["result"]["state"].as_str()), (Some("ok"), Some("pending")), "{sent}");
        assert_eq!(sent["result"]["to"]["host"], "vesper", "the computer the agent works from: {sent}");
    });
}
