use ibara_screen::admission::{Admission,Decision};
#[test]
fn admission_fences_and_binds_reattaches() {
 let mut s=Admission::default(); let ticket="ab".repeat(32); let cert="cd".repeat(32); let other="ef".repeat(32);
 assert!(s.ticket(&ticket,&cert,1,1000,true,0).is_err());
 s.open(1).unwrap(); assert!(s.open(1).is_err());
 s.ticket(&ticket,&cert,1,1000,true,0).unwrap();
 assert_eq!(s.admit(&ticket,&other,1),Decision::Refused);
 assert_eq!(s.admit(&ticket,&cert,2),Decision::Refused);
 s.ticket(&ticket,&cert,1,1000,true,2).unwrap();
 assert_eq!(s.admit(&ticket,&cert,3),Decision::Accepted{input:true,generation:1});
 assert_eq!(s.admit(&ticket,&cert,4),Decision::Accepted{input:true,generation:1});
 assert_eq!(s.admit(&ticket,&cert,1000),Decision::Refused);
 s.revoke().unwrap(); assert!(s.ticket(&ticket,&cert,2,2000,true,1000).is_err());
 s.open(3).unwrap(); assert!(s.ticket(&ticket,&cert,1,2000,true,1000).is_err());
}
#[test]
fn unknown_ticket_does_not_destroy_admission() {
 let mut s=Admission::default();let t="ab".repeat(32);let c="cd".repeat(32);
 s.open(1).unwrap(); s.ticket(&t,&c,1,100,true,0).unwrap();
 assert_eq!(s.admit(&"ff".repeat(32),&c,1),Decision::Refused);
 assert_eq!(s.admit(&t,&c,2),Decision::Accepted{input:true,generation:1});
}
