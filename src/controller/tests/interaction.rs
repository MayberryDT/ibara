//! Failure controls written before the interaction candidate.
use super::*;
fn lines(env: &Value) -> String { env["result"]["frame"]["lines"].to_string() }
#[test]
fn focused_page_is_read_without_knowing_a_special_surface() {
 run(async { let rig=browser_rig(); let task=begin(&rig.controller).await;
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
 assert_eq!(env["status"],"ok","{env}"); assert!(lines(&env).contains("b1 button"),"{env}");
 });
}
#[test]
fn native_input_returns_page_controls_and_does_not_claim_persistence() {
 run(async { let rig=browser_rig(); let task=begin(&rig.controller).await;
 let env=call(&rig.controller,"computer_act",json!({"task_ref":task,"request_id":"native-tab","action":{"kind":"key","keys":"Tab"}})).await;
 assert_eq!(env["result"]["steps"][0]["outcome"],"done","{env}");
 assert!(lines(&env).contains("b1 button"),"{env}");
 assert!(env["result"]["next"].as_str().unwrap_or("").contains("saved"),"{env}");
 });
}
#[test]
fn a_native_file_dialog_wins_over_a_connected_browser() {
 run(async { let rig=browser_rig(); let task=begin(&rig.controller).await;
 rig.desktop.windows.replace(vec![win("0xd",301,"xdg-desktop-portal-gtk","Open File",true,true),win("0x9",300,"chromium","Shop - Chromium",false,false)]);
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
 assert!(lines(&env).contains("Open File"),"{env}");assert!(!lines(&env).contains("b1 button"),"{env}");
 });
}
#[test]
fn chromium_owned_dialog_does_not_read_the_underlying_page() {
 run(async { let rig=browser_rig(); let task=begin(&rig.controller).await;
 rig.desktop.windows.replace(vec![win("0xd",300,"chromium","Save File",true,true)]);
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
 assert!(!lines(&env).contains("b1 button"),"{env}");
 });
}
#[test]
fn explicit_window_observation_keeps_native_semantics() {
 run(async { let rig=browser_rig(); let task=begin(&rig.controller).await;
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":"w1","view":"elements"})).await;
 assert!(!lines(&env).contains("b1 button"),"{env}");
 });
}
#[test]
fn page_controls_report_state_and_record_context() {
 run(async { let rig=browser_rig();
 rig.desktop.extension.borrow_mut().get_mut("observe").unwrap()["nodes"]=json!([{"role":"checkbox","name":"Required document","states":["enabled","checked"],"ancestor":"dialog:Review","actions":["click"],"token":"t1"}]);
 let task=begin(&rig.controller).await;
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":"tab","view":"elements"})).await;
 let s=lines(&env);assert!(s.contains("checked") && s.contains("dialog:Review"),"{env}");
 });
}
#[test]
fn refused_automatic_page_read_is_explicit_and_keeps_native_fallback() {
 run(async { let rig=browser_rig();let task=begin(&rig.controller).await;
 rig.desktop.extension.borrow_mut().insert("observe".into(),json!({"refused":true,"title":"Shop"}));
 let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
 assert_eq!(env["status"],"ok","{env}");assert!(lines(&env).contains("page controls unavailable"),"{env}");assert!(!lines(&env).contains("b1 button"),"{env}");
 });
}
#[test]
fn repeating_a_delivered_native_action_replays_without_another_effect() {
 run(async { let rig=browser_rig();let task=begin(&rig.controller).await;
 let args=json!({"task_ref":task,"request_id":"one-tab","action":{"kind":"key","keys":"Tab"}});
 let first=call(&rig.controller,"computer_act",args.clone()).await;
 let n=rig.desktop.acts.borrow().len();let second=call(&rig.controller,"computer_act",args).await;
 assert_eq!(first["result"],second["result"]);assert_eq!(rig.desktop.acts.borrow().len(),n);
 });
}
