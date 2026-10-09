//! Cycle 3 failure controls, written before implementation.
use super::*;

fn page_nodes(rig: &Rig, nodes: Value) {
    rig.desktop.extension.borrow_mut().get_mut("observe").unwrap()["nodes"] = nodes;
}
fn first_ref(env: &Value) -> String {
    env["result"]["frame"]["lines"].as_array().unwrap().iter()
        .filter_map(Value::as_str).find(|s| s.starts_with('b')).unwrap().split_whitespace().next().unwrap().into()
}
#[test]
fn filtered_page_keeps_element_identity_instead_of_reassigning_a_number() {
    run(async {
        let rig=browser_rig();
        page_nodes(&rig,json!([
            {"role":"button","name":"First","actions":["click"],"elementId":"first","token":"a"},
            {"role":"button","name":"Second","actions":["click"],"elementId":"second","token":"b"}
        ]));
        let task=observe_page(&rig.controller).await;
        page_nodes(&rig,json!([{"role":"button","name":"Second","actions":["click"],"elementId":"second","token":"new-b"}]));
        let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":"tab","view":"elements","query":"Second"})).await;
        assert_eq!(first_ref(&env),"b2","filter reassigned the first element's id: {env}");
        let count=rig.desktop.acts();
        let refused=call(&rig.controller,"browser_act",json!({"task_ref":task,"request_id":"retired","action":{"kind":"click","target":"b1"}})).await;
        assert_eq!(refused["error"]["code"],"STALE_TARGET","{refused}");
        assert_eq!(rig.desktop.acts(),count);
    });
}
#[test]
fn replacement_node_never_inherits_the_old_nodes_reference() {
    run(async {
        let rig=browser_rig();
        page_nodes(&rig,json!([{"role":"button","name":"Delete old record","actions":["click"],"elementId":"old","token":"a"}]));
        let task=observe_page(&rig.controller).await;
        page_nodes(&rig,json!([{"role":"button","name":"Delete new record","actions":["click"],"elementId":"new","token":"b"}]));
        let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
        assert_ne!(first_ref(&env),"b1","new record reused old ref: {env}");
        let count=rig.desktop.acts();
        let refused=call(&rig.controller,"browser_act",json!({"task_ref":task,"request_id":"old-record","action":{"kind":"click","target":"b1"}})).await;
        assert_eq!(refused["error"]["code"],"STALE_TARGET","{refused}");assert_eq!(rig.desktop.acts(),count);
    });
}
#[test]
fn browser_image_returns_fresh_semantics_but_native_dialog_does_not() {
    run(async {
        let rig=browser_rig();let task=observe_page(&rig.controller).await;
        let image=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"image"})).await;
        assert!(image["result"]["frame"]["lines"].to_string().contains("b1 button"),"{image}");
        rig.desktop.windows.replace(vec![win("0xd",301,"xdg-desktop-portal-gtk","Open File",true,true),win("0x9",300,"chromium","Shop - Chromium",false,false)]);
        let native=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"image"})).await;
        assert!(!native["result"]["frame"]["lines"].to_string().contains("b1 button"),"underlying page revived: {native}");
    });
}
#[test]
fn truncated_page_read_reports_coverage_and_query_recovery() {
    run(async {
        let rig=browser_rig();rig.desktop.extension.borrow_mut().get_mut("observe").unwrap()["count"]=json!(83);
        let task=observe_page(&rig.controller).await;
        let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
        let lines=env["result"]["frame"]["lines"].to_string();
        assert!(lines.contains("83")&&lines.contains("query"),"silent omission: {env}");
    });
}
#[test]
fn computer_click_routes_page_target_through_the_native_browser_guard() {
    run(async {
        let rig=browser_rig();
        rig.desktop.extension.borrow_mut().insert("verify".into(),json!({"observed":true,"hit":true}));
        let task=observe_page(&rig.controller).await;
        let env=call(&rig.controller,"computer_act",json!({"task_ref":task,"request_id":"unified-click","action":{"kind":"click","target":"b1"}})).await;
        assert_eq!(env["result"]["steps"][0]["outcome"],"done","{env}");
        assert!(rig.desktop.extension_ops.borrow().iter().any(|op|op=="locate"));
        assert!(rig.desktop.acts()>0,"must use actual native input");
    });
}
