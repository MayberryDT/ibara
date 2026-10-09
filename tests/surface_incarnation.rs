//! Failure cases: artifact surface-incarnation-failures.md, written before v8.
use ibara::desktop::hyprland::{Hyprland, SurfaceId};
use serde_json::{json, Value};
use std::{fs, sync::Arc, os::unix::fs::PermissionsExt};

#[tokio::test(flavor="current_thread")]
async fn surface_incarnation_guards_real_adapter_dispatch() {
    let root=std::env::var_os("IBARA_E2E_EVIDENCE").map(std::path::PathBuf::from).expect("Set evidence directory");
    fs::create_dir_all(&root).unwrap();
    let fixture=root.join("hyprctl-fixture.py");
    fs::write(&fixture, r##"#!/usr/bin/python3
import json,pathlib,sys,os
p=pathlib.Path(__file__).parent;a=sys.argv[1:]
with (p/'calls.jsonl').open('a') as f:f.write(json.dumps(a)+'\n')
if a==['-j','instances']:
 print(json.dumps([{'instance':(p/'session').read_text(),'pid':1}]))
elif a[:1]==['-i']:
 selected=a[1];cmd=a[2:]
 if selected not in ('0',(p/'session').read_text()):print('instance not found');sys.exit(1)
 if cmd==['-j','clients']:
  if (p/'bad-inventory').exists():print('broken inventory');sys.exit(0)
  print(json.dumps([{'address':'0xabc','pid':int((p/'pid').read_text()),'class':'fixture','mapped':True,'processStartTicks':1,'compositorInstance':'spoofed'}]))
 elif cmd[:1]==['eval']:
  with (p/'effects.jsonl').open('a') as f:f.write(json.dumps(a)+'\n')
  print('ok')
 else:print('{}')
else:sys.exit(2)
"##).unwrap();fs::set_permissions(&fixture,fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("session"),"session-A").unwrap();fs::write(root.join("pid"),std::process::id().to_string()).unwrap();
    let hypr=Hyprland::new(fixture.as_os_str(),"0",Arc::from([]));
    let windows=hypr.clients().await.unwrap();assert_eq!(windows.len(),1);
    let original=windows[0].id();let identity=serde_json::to_value(&original).unwrap();
    let expected=fs::read_to_string(format!("/proc/{}/stat",std::process::id())).unwrap();
    let ticks:u64=expected.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse().unwrap();
    assert_eq!(identity["process_start_ticks"],ticks,"Read real process start; never accept fixture JSON metadata");
    assert_eq!(identity["compositor_instance"],"session-A");
    hypr.close_surface(&original).await.unwrap();
    let effects=||fs::read_to_string(root.join("effects.jsonl")).unwrap_or_default().lines().count();assert_eq!(effects(),1);
    let mut recycled=identity.clone();recycled["process_start_ticks"]=json!(ticks+1);
    let recycled:SurfaceId=serde_json::from_value(recycled).unwrap();assert!(hypr.close_surface(&recycled).await.is_err());assert_eq!(effects(),1,"PID reuse must not dispatch");
    let mut missing=identity.clone();missing["process_start_ticks"]=Value::Null;
    let missing:SurfaceId=serde_json::from_value(missing).unwrap();assert!(hypr.close_surface(&missing).await.is_err());assert_eq!(effects(),1);
    fs::write(root.join("session"),"session-B").unwrap();assert!(hypr.close_surface(&original).await.is_err());assert_eq!(effects(),1,"Do not retarget compositor index 0");
    let replacement=hypr.clients().await.unwrap()[0].id();assert_ne!(original,replacement);
    fs::write(root.join("pid"),"2147483647").unwrap();let unavailable=hypr.clients().await.unwrap();assert_eq!(unavailable.len(),1,"Unreadable identity is not zero windows");assert!(!unavailable[0].is(&unavailable[0].id()),"Unknown identity must not satisfy live validation for focus/key/type");assert!(hypr.close_surface(&unavailable[0].id()).await.is_err());assert_eq!(effects(),1);
    fs::write(root.join("bad-inventory"),"").unwrap();assert!(hypr.clients().await.is_err());
    let calls=fs::read_to_string(root.join("calls.jsonl")).unwrap();assert!(!calls.lines().map(|l|serde_json::from_str::<Value>(l).unwrap()).any(|v|v[1]=="0" && v[2]=="eval"));
    fs::write(root.join("result.json"),serde_json::to_vec_pretty(&json!({"passed":true,"scope":"External synthetic compositor adapter, not live desktop","checks":["process provenance","stable target close","PID start mismatch","missing identity refusal","session replacement refusal","unreadable identity retained","inventory failure refusal"],"identity":identity,"dispatches":effects()})).unwrap()).unwrap();
}
