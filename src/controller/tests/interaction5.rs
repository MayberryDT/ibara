//! Cycle5 controls written before the explicit-image semantic routing repair.
use super::*;

#[test]
fn named_browser_pictures_and_explicit_tab_reads_keep_page_controls() {
    run(async {
        let rig=browser_rig();let task=observe_page(&rig.controller).await;
        for (surface,view) in [("w1","image"),("w1","screen"),("tab","image"),("tab","screen"),("tab","situation"),("tab","elements")] {
            let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":surface,"view":view})).await;
            assert!(env["result"]["frame"]["lines"].to_string().contains("b1 button"),"{surface}/{view} cleared browser controls: {env}");
        }
        for view in ["elements","situation"] {
            let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":"w1","view":view})).await;
            assert!(!env["result"]["frame"]["lines"].to_string().contains("b1 button"),"explicit native inspection changed: {env}");
        }
    });
}

#[test]
fn named_native_dialog_or_unfocused_browser_picture_does_not_read_the_page() {
    run(async {
        let rig=browser_rig();let task=observe_page(&rig.controller).await;
        rig.desktop.windows.replace(vec![win("0xd",301,"xdg-desktop-portal-gtk","Open File",true,true),win("0x9",300,"chromium","Shop - Chromium",false,false)]);
        for surface in ["w1","w2"] {
            let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":surface,"view":"image"})).await;
            assert!(!env["result"]["frame"]["lines"].to_string().contains("b1 button"),"covered/unfocused page revived: {env}");
        }
        rig.desktop.windows.replace(vec![win("0xd",300,"chromium","Save File",true,true)]);
        let env=call(&rig.controller,"computer_observe",json!({"task_ref":task,"surface":"w1","view":"image"})).await;
        assert!(!env["result"]["frame"]["lines"].to_string().contains("b1 button"),"floating browser dialog revived page: {env}");
    });
}
