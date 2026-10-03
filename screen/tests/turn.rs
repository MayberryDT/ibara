use ibara_screen::turn::{TurnGate,Action};
use serde_json::json;
#[test]
fn only_meaningful_input_requests_a_turn_and_pending_is_fenced() {
 let mut g=TurnGate::default();
 assert_eq!(g.event("a",true,json!({"move":[20,30]})),Action::Ignore);
 assert_eq!(g.event("a",true,json!({"key":[30,true]})),Action::Request);
 assert_eq!(g.event("b",true,json!({"key":[31,true]})),Action::Refused);
 let pending=g.grant(true,Some("Tyler")); assert_eq!(pending.len(),2);assert_eq!(pending[0],json!({"move":[20,30]}));
 assert_eq!(g.event("a",true,json!({"key":[30,false]})),Action::Deliver(json!({"key":[30,false]})));
 assert_eq!(g.event("b",true,json!({"key":[31,true]})),Action::Refused);
 g.grant(false,None);assert_eq!(g.event("a",false,json!({"button":[272,true]})),Action::Ignore);
 assert_eq!(g.event("a",true,json!({"wheel":[0,120]})),Action::Request);g.clear();assert!(g.grant(true,None).is_empty());
}
