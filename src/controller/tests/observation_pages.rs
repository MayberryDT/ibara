//! Written before pagination repair: the declared 100-element request is cut
//! to 60; page continuation is missing; stale/mismatched cursors fall back to
//! unrelated native controls; continuation changes semantic IDs or repeats page1.
use super::*;

fn page(start:usize,end:usize,next:Option<usize>)->Value {
    json!({"title":"Shop","url":"https://shop.example/","documentId":"document-pages","capture":"capture-pages",
        "nodes":(start..end).map(|n|json!({"elementId":format!("element-{n}"),"role":"button","name":format!("Item {n}"),"actions":["click"],"states":[],"token":format!("token-{n}")})).collect::<Vec<_>>(),
        "count":120,"next_offset":next})
}

#[test]
fn page_limits_and_continuation_reach_the_rest_without_repeating() {run(async {
    let rig=browser_rig();let c=&rig.controller;let task=begin(c).await;
    rig.desktop.extension.borrow_mut().insert("observe".into(),page(0,100,Some(100)));
    let first=call(c,"computer_observe",json!({"task_ref":task,"view":"elements","limit":100})).await;
    assert_eq!(first["status"],"ok","{first}");
    let lines=first["result"]["frame"]["lines"].as_array().unwrap();
    assert!(lines.iter().any(|v|v.as_str().unwrap().contains("\"Item 99\"")),"requested controls silently capped: {first}");
    let cursor=first["result"]["frame"]["next_cursor"].as_str().expect("omitted controls have continuation").to_owned();
    rig.desktop.extension.borrow_mut().insert("observe".into(),page(100,120,None));
    let second=call(c,"computer_observe",json!({"task_ref":task,"view":"elements","limit":100,"cursor":cursor})).await;
    assert_eq!(second["status"],"ok","{second}");
    assert!(second["result"]["frame"]["lines"].to_string().contains("Item 119"));
    assert!(!second["result"]["frame"]["lines"].as_array().unwrap().iter().any(|s|s.as_str().unwrap().contains("\"Item 0\"")));
    assert!(second["result"]["frame"]["next_cursor"].is_null());
});}

#[test]
fn stale_or_changed_query_continuation_is_refused_without_native_fallback() {run(async {
    let rig=browser_rig();let c=&rig.controller;let task=begin(c).await;
    rig.desktop.extension.borrow_mut().insert("observe".into(),page(0,20,Some(20)));
    let first=call(c,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
    let cursor=first["result"]["frame"]["next_cursor"].as_str().unwrap().to_owned();
    let changed=call(c,"computer_observe",json!({"task_ref":task,"view":"elements","cursor":cursor,"query":"changed"})).await;
    assert_eq!(changed["status"],"error","{changed}");
    rig.desktop.extension.borrow_mut().insert("observe".into(),json!({"refused":true,"title":"Shop"}));
    let stale=call(c,"computer_observe",json!({"task_ref":task,"view":"elements","cursor":cursor})).await;
    assert_eq!(stale["status"],"error","stale cursor fell back to fresh native controls: {stale}");
});}
