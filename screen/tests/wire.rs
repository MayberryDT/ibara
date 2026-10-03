use ibara_screen::wire::{VideoHeader,VideoGate};
#[test]
fn frame_parser_rejects_invalid_headers() {
 let h=VideoHeader{keyframe:true,sequence:7,capture_us:42};let b=h.encode();
 let p=VideoHeader::decode(&b).unwrap();assert_eq!(p.sequence,7); assert_eq!(p.capture_us,42);
 for n in 0..24 {assert!(VideoHeader::decode(&b[..n]).is_err());}
 for index in [0,4,6,7] {let mut bad=b;bad[index]^=128;assert!(VideoHeader::decode(&bad).is_err());}
}
#[test]
fn gaps_require_keyframes_and_old_frames_are_dropped() {
 let mut g=VideoGate::default();
 assert!(!g.accept(1,false));assert!(g.accept(2,true));assert!(g.accept(3,false));
 assert!(!g.accept(5,false));assert!(!g.accept(6,false));assert!(g.accept(7,true));assert!(!g.accept(6,true));
}
#[test]
fn input_sequences_consume_failed_delivery_and_reject_gaps() {
 use ibara_screen::wire::InputSequence;
 let mut s=InputSequence::default();assert!(s.consume(30,2).is_ok());
 assert!(s.consume(30,2).is_err());assert!(s.consume(34,1).is_err());assert_eq!(s.expected(),Some(32));
 assert!(s.consume(32,1).is_ok());assert_eq!(s.expected(),Some(33));assert!(s.consume(u64::MAX,2).is_err());
}
