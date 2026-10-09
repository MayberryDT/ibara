//! Cycle 4 failure controls, written before the focus-aware selection change.
use super::*;

#[test]
fn every_automatic_browser_view_keeps_fresh_page_controls() {
    run(async {
        let rig=browser_rig(); let task=observe_page(&rig.controller).await;
        for view in ["situation","elements","image","screen"] {
            let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":view})).await;
            assert!(env["result"]["frame"]["lines"].to_string().contains("b1 button"),"{view} discarded page controls: {env}");
        }
        rig.desktop.windows.replace(vec![win("0xd",301,"xdg-desktop-portal-gtk","Open File",true,true),win("0x9",300,"chromium","Shop - Chromium",false,false)]);
        let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"screen"})).await;
        assert!(!env["result"]["frame"]["lines"].to_string().contains("b1 button"),"screen revived covered page: {env}");
    });
}

fn focused_list() -> Rig {
    let rig = browser_rig();
    let mut e = rig.desktop.extension.borrow_mut();
    e.get_mut("observe").unwrap()["nodes"] = json!([{"role":"combobox","name":"Record","actions":["select","click"],"states":["enabled","focused"],"token":"t1"}]);
    e.get_mut("locate").unwrap()["selection"] = json!({"value":"second","index":1,"from":"Home","steps":1});
    // An already open native menu consumes a redundant pointer press.
    e.insert("verify".into(), json!({"observed":false,"hit":false}));
    e.insert("selected".into(), json!({"ready":true,"open":true,"matches":true}));
    drop(e);
    rig
}
fn choose(task: &str) -> Value {
    json!({"task_ref":task,"request_id":"focused-choice","action":{"kind":"select","target":"b1","value":"second"}})
}

#[test]
fn focused_native_list_selects_without_a_second_click_and_replays_once() {
    run(async {
        let rig=focused_list(); let task=observe_page(&rig.controller).await;
        let args=choose(&task); let env=call(&rig.controller,"browser_act",args.clone()).await;
        assert_eq!(env["result"]["steps"][0]["outcome"],"done","{env}");
        assert!(!rig.desktop.acts.borrow().iter().any(|a|a.starts_with("Click")),"redundant menu click");
        assert_eq!(rig.desktop.acts.borrow().iter().filter(|a|a.starts_with("Key")).count(),3,"native selection must execute");
        let count=rig.desktop.acts(); let again=call(&rig.controller,"browser_act",args).await;
        assert_eq!(again["result"],env["result"]); assert_eq!(rig.desktop.acts(),count);
    });
}

#[test]
fn selection_focus_lost_before_first_key_refuses_without_input() {
    run(async {
        let rig=focused_list(); let task=observe_page(&rig.controller).await;
        rig.desktop.reader_replies.borrow_mut().insert("selected".into(),std::collections::VecDeque::from([json!({"ready":true,"open":true}),json!({"ready":false})]));
        let count=rig.desktop.acts(); let env=call(&rig.controller,"browser_act",choose(&task)).await;
        assert_eq!(rig.desktop.acts(),count,"focus race must not send input: {env}");
        assert_ne!(env["result"]["steps"][0]["outcome"],"unknown","nothing dispatched: {env}");
        assert_ne!(env["result"]["steps"][0]["outcome"],"done","{env}");
    });
}

#[test]
fn selection_focus_lost_after_first_key_remains_unknown() {
    run(async {
        let rig=focused_list(); let task=observe_page(&rig.controller).await;
        rig.desktop.reader_replies.borrow_mut().insert("selected".into(),std::collections::VecDeque::from([json!({"ready":true,"open":true}),json!({"ready":true,"open":true}),json!({"ready":false})]));
        let env=call(&rig.controller,"browser_act",choose(&task)).await;
        assert_eq!(rig.desktop.acts.borrow().iter().filter(|a|a.starts_with("Key")).count(),1,"stop after focus loss: {env}");
        assert_eq!(env["result"]["steps"][0]["outcome"],"unknown","{env}");
        assert!(!rig.desktop.acts.borrow().iter().any(|a|a.starts_with("Click")));
    });
}

#[test]
fn focused_but_closed_list_opens_before_ordinal_keys() {
    run(async {
        let rig=focused_list(); let task=observe_page(&rig.controller).await;
        rig.desktop.extension.borrow_mut().insert("verify".into(),json!({"observed":true,"hit":true}));
        rig.desktop.reader_replies.borrow_mut().insert("selected".into(),std::collections::VecDeque::from([json!({"ready":true,"open":false})]));
        let env=call(&rig.controller,"browser_act",choose(&task)).await;
        assert_eq!(env["result"]["steps"][0]["outcome"],"done","{env}");
        let acts=rig.desktop.acts.borrow();
        assert!(acts.first().is_some_and(|a|a.starts_with("Click")),"closed list must open before changing options: {acts:?}");
    });
}
