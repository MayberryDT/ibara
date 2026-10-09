//! Cycle 2 failure controls, written before the product changes.
use super::*;
fn selection_rig() -> Rig {
    let rig = browser_rig();
    let mut e = rig.desktop.extension.borrow_mut();
    e.get_mut("observe").unwrap()["nodes"] = json!([{"role":"combobox","name":"Document","actions":["select"],"states":["enabled"],"token":"t1"}]);
    e.get_mut("locate").unwrap()["label"] = json!("Corrected — résumé.pdf");
    e.get_mut("locate").unwrap()["selection"] = json!({"value":"corrected","index":3,"from":"Home","steps":2});
    e.insert("verify".into(), json!({"observed":true,"hit":true}));
    e.insert("selected".into(), json!({"ready":true,"matches":true,"text":"Corrected — résumé.pdf","value":"corrected","index":3}));
    drop(e); rig
}
fn selection(task: &str) -> Value {
    json!({"task_ref":task,"request_id":"select-once","action":{"kind":"select","target":"b1","value":"corrected"}})
}
#[test]
fn unicode_option_is_selected_without_typing_and_replay_sends_no_input() {
    run(async { let rig=selection_rig(); let task=observe_page(&rig.controller).await;
        let args=selection(&task);let first=call(&rig.controller,"browser_act",args.clone()).await;
        assert_eq!(first["result"]["steps"][0]["outcome"],"done","{first}");
        assert!(!rig.desktop.acts.borrow().iter().any(|a|a.starts_with("Type")),"a select must never type a label");
        let count=rig.desktop.acts();let replay=call(&rig.controller,"browser_act",args).await;
        assert_eq!(first["result"],replay["result"]);assert_eq!(rig.desktop.acts(),count);
    });
}
#[test]
fn old_reader_without_exact_selection_metadata_refuses_before_click() {
    run(async { let rig=selection_rig();rig.desktop.extension.borrow_mut().get_mut("locate").unwrap().as_object_mut().unwrap().remove("selection");
        let task=observe_page(&rig.controller).await;let count=rig.desktop.acts();let env=call(&rig.controller,"browser_act",selection(&task)).await;
        assert_ne!(env["result"]["steps"][0]["outcome"],"done","{env}");assert_eq!(rig.desktop.acts(),count,"reader incompatibility must precede input");
    });
}
#[test]
fn matching_label_alone_does_not_prove_the_option_was_selected() {
    run(async {let rig=selection_rig();rig.desktop.extension.borrow_mut().get_mut("selected").unwrap()["matches"]=json!(false);
        let task=observe_page(&rig.controller).await;let env=call(&rig.controller,"browser_act",selection(&task)).await;
        assert_ne!(env["result"]["steps"][0]["outcome"],"done","{env}");
    });
}
#[test]
fn lost_page_selection_focus_never_sends_selection_keys() {
    run(async {let rig=selection_rig();rig.desktop.extension.borrow_mut().get_mut("selected").unwrap()["ready"]=json!(false);
        let task=observe_page(&rig.controller).await;let env=call(&rig.controller,"browser_act",selection(&task)).await;
        assert_ne!(env["result"]["steps"][0]["outcome"],"done","{env}");assert!(!rig.desktop.acts.borrow().iter().any(|a|a.starts_with("Key")),"keys after lost focus");
    });
}
#[test]
fn page_review_exposes_values_missing_checkbox_and_file_action_without_passwords() {
    run(async {let rig=browser_rig();rig.desktop.extension.borrow_mut().get_mut("observe").unwrap()["nodes"]=json!([
        {"role":"textbox","name":"Memo","value":"Saved review text","actions":["fill"],"states":["enabled"],"token":"t1"},
        {"role":"checkbox","name":"Review completed","actions":["click"],"states":["enabled","unchecked","required"],"token":"t2"},
        {"role":"file_input","name":"Attachment","files":["review.pdf"],"actions":["click"],"states":["enabled"],"token":"t3"},
        {"role":"textbox","name":"Secret","value":"SYNTHETIC-MUST-NOT-LEAK","states":["password"],"token":"t4"}
    ]);let task=observe_page(&rig.controller).await;let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"view":"elements"})).await;
        let lines=env["result"]["frame"]["lines"].to_string();assert!(lines.contains("Saved review text") && lines.contains("unchecked") && lines.contains("review.pdf") && lines.contains("file chooser"),"{env}");assert!(!lines.contains("SYNTHETIC-MUST-NOT-LEAK"));
    });
}
