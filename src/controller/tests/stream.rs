//! Take Control over `ibara-stream`, and the shared clipboard. Failure cases,
//! written first:
//! - a viewer this computer does not know (never registered, or registered
//!   under an earlier pairing) gets a ticket, or pauses the computer first
//! - a new holder's ticket is minted before the earlier viewer's held keys are
//!   released, or while that release is unproven
//! - a ticket is minted for another certificate or a generation not admitted
//! - a person cannot take control from an agent whose input stopped cleanly
//!   when control changed hands (typing between pieces, or a finished step),
//!   or can while input the agent sent may still be held
//! - closing the viewer hands control back, resumes agents, or leaves the
//!   stream running; Open Viewer then fails, or works for someone else
//! - handing back leaves the stream running, resumes an agent, or reports
//!   success when the keys were not proven released
//! - the clipboard reaches someone not holding control, shares what was
//!   copied before control began, sends a copy back where it came from,
//!   accepts pieces out of order or entries over 8 MiB, or is recorded
//! - a shorter copy made while a large entry travels is refused (which ends
//!   sharing) instead of being fetched from its start
//! - a copy made after control ended, while the holder's first fetch was still
//!   reading the clipboard, reaches whoever takes control next
//! - an agent whose control a person took cannot finish its task afterwards,
//!   so its summary is lost; finishing takes control back from the person;
//!   it closes windows while the person still has the computer, or closes
//!   one the person went to; or it leaves a window nobody used open after
//!   the person handed back

use super::*;
use crate::controller::clipboard::{CHUNK, CLIP_MAX};
use base64::Engine;
use sha2::{Digest, Sha256};

const HAZEL_CERT: &str = "ba11a000ba11a000ba11a000ba11a000ba11a000ba11a000ba11a000ba11a000";

/// Take control as `who` at the current owner and revision.
pub(super) async fn take(c: &Controller, who: &str) -> crate::error::Result<Value> {
    transition(c, who, "take_control").await
}

async fn transition(c: &Controller, who: &str, op: &str) -> crate::error::Result<Value> {
    let status = c.operator_call(who, operator_action(c, "status")).await?;
    let mut action = operator_action(c, op);
    action["expected_owner"] = status["owner"].clone();
    action["expected_ownership_revision"] = status["ownership_revision"].clone();
    c.operator_call(who, action).await
}

fn ticket(reply: &Value) -> String {
    reply["stream"]["ticket"].as_str().expect("a ticket").to_string()
}

/// `vesper` and `hazel`, both paired at generation 3; `viewer` is vesper's registration.
fn set_grants(rig: &mut Rig, viewer: Value) {
    Rc::get_mut(&mut rig.controller).expect("unshared controller").operator_grants = Rc::new(move || {
        let mut grants = Map::new();
        grants.insert("vesper".into(), json!({ "enabled": true, "generation": 3, "observe": true, "files": true, "viewer": viewer }));
        grants.insert(
            "hazel".into(),
            json!({ "enabled": true, "generation": 3, "observe": true, "files": true, "viewer": { "cert_sha256": HAZEL_CERT, "generation": 3 } }),
        );
        grants
    });
}

fn registered() -> Value {
    json!({ "cert_sha256": CERT, "generation": 3 })
}

#[test]
fn taking_control_pauses_and_mints_one_ticket_for_the_registered_viewer() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        let taken = take(c, "vesper").await.unwrap();
        assert_eq!(rig.stream.calls(), ["revoke", "start", "open 1", "ticket 1 c0ffee"]);
        assert_eq!(taken["owner"], "operator:vesper");
        assert_eq!(taken["control_generation"], 1);
        let stream = &taken["stream"];
        assert!(crate::server::authority::is_hex_lower(&ticket(&taken), 64), "{stream}");
        assert_eq!(
            [&stream["server_cert_sha256"], &stream["http_port"], &stream["https_port"], &stream["width"], &stream["height"], &stream["fps"]],
            [&json!("5e".repeat(32)), &json!(47989), &json!(47984), &json!(1920), &json!(1080), &json!(30)]
        );
        assert!(rig.stream.connect(&ticket(&taken), CERT), "the registered viewer is admitted with its ticket");
        let control = c.journal.get_control().unwrap();
        assert!(control.paused && control.human_control, "{control:?}");
        let begin = call(c, "computer_begin", json!({ "goal": "Write", "request_id": "r1" })).await;
        assert_eq!(code(&begin), "HUMAN_CONTROL");
    });
}

#[test]
fn a_viewer_this_computer_does_not_know_cannot_take_control() {
    run(async {
        // Never registered, and registered under an earlier pairing.
        for viewer in [Value::Null, json!({ "cert_sha256": CERT, "generation": 2 })] {
            let mut rig = rig(true);
            set_grants(&mut rig, viewer.clone());
            let c = &rig.controller;
            let err = take(c, "vesper").await.unwrap_err();
            assert_eq!(err.code, "CAPABILITY_UNAVAILABLE", "{viewer}: {err:?}");
            assert!(rig.stream.calls().is_empty(), "nothing was fenced or started for {viewer}");
            assert!(!c.journal.get_control().unwrap().paused, "nothing paused for {viewer}");
        }
    });
}

#[test]
fn a_takeover_releases_the_earlier_viewers_keys_before_the_new_ticket() {
    run(async {
        let mut rig = rig(true);
        set_grants(&mut rig, registered());
        let c = &rig.controller;
        let first = take(c, "vesper").await.unwrap();
        rig.stream.connect(&ticket(&first), CERT);
        let before = rig.stream.calls().len();
        let second = take(c, "hazel").await.unwrap();
        assert_eq!(rig.stream.calls()[before..], ["revoke", "open 3", "ticket 3 ba11a0"]);
        assert_eq!(second["owner"], "operator:hazel");
        assert!(!rig.stream.connect(&ticket(&first), CERT), "the earlier viewer's ticket is gone");
        assert!(rig.stream.connect(&ticket(&second), HAZEL_CERT));

        // A release that cannot be proven mints nothing and fences the computer.
        rig.stream.0.unsettled.set(true);
        let before = rig.stream.calls().len();
        let err = take(c, "vesper").await.unwrap_err();
        assert_eq!(err.code, "CONTROL_UNSETTLED", "{err:?}");
        let after = rig.stream.calls()[before..].to_vec();
        assert!(!after.iter().any(|call| call.starts_with("ticket") || call.starts_with("open")), "{after:?}");
        assert_eq!(after.last().map(String::as_str), Some("stop"), "the stream is ended: {after:?}");
        let control = c.journal.get_control().unwrap();
        assert!(control.paused && control.unsettled, "{control:?}");
        assert!(c.viewer_state.borrow().fault);
    });
}

#[test]
fn a_person_takes_control_from_an_agent_whose_input_stopped_cleanly() {
    run(async {
        let between_pieces = IbaraError::new("TIMEOUT", "Typing cancelled between pieces.", false)
            .with("typed_chars", 16)
            .with("no_input_held", true)
            .with("execution_not_started", false);
        let cut_off = IbaraError::new("TIMEOUT", "Cua did not answer.", false);
        for (case, end, admitted) in [("between pieces", Err(between_pieces), true), ("step finished", Ok(Done::default()), true), ("cut off", Err(cut_off), false)] {
            let rig = rig(true);
            let c = &rig.controller;
            let task = begin(c).await;
            rig.desktop.until_cancelled.replace(Some(end));
            let agent = rig.controller.clone();
            let typing = tokio::task::spawn_local(async move {
                call(&agent, "computer_act", json!({ "task_ref": task, "request_id": "type-1", "action": { "kind": "type", "text": "a line that takes a while" } })).await
            });
            rig.desktop.entered.notified().await;
            let taken = take(c, "vesper").await;
            let typed = typing.await.unwrap();
            assert_eq!(typed["result"]["steps"][0]["outcome"], "unknown", "{case}: {typed}");
            let control = c.journal.get_control().unwrap();
            if admitted {
                let taken = taken.unwrap_or_else(|e| panic!("{case}: {e:?}"));
                assert_eq!(taken["owner"], "operator:vesper", "{case}");
                assert!(rig.stream.connect(&ticket(&taken), CERT), "{case}");
                assert!(control.paused && !control.unsettled, "{case}: {control:?}");
            } else {
                assert_eq!(taken.unwrap_err().code, "CONTROL_UNSETTLED", "{case}");
                assert!(control.paused && control.unsettled, "{case}: input that may be held stays fenced: {control:?}");
            }
        }
    });
}

#[test]
fn closing_the_viewer_stops_the_stream_but_keeps_control_and_the_pause() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        let taken = take(c, "vesper").await.unwrap();
        rig.stream.connect(&ticket(&taken), CERT);
        let calls = rig.stream.calls().len();
        c.stop_idle_stream().await;
        assert_eq!(rig.stream.calls().len(), calls, "a stream someone watches keeps running");

        rig.stream.0.viewing.set(false);
        c.stop_idle_stream().await;
        assert_eq!(rig.stream.calls().last().map(String::as_str), Some("stop"));
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        assert_eq!((status["owner"].clone(), status["holds_control"].clone()), (json!("operator:vesper"), json!(true)));
        assert!(c.journal.get_control().unwrap().paused);

        // Open Viewer starts it again with a fresh ticket on a new generation.
        let before = rig.stream.calls().len();
        let opened = c.operator_call("vesper", operator_action(c, "viewer_ticket")).await.unwrap();
        assert_eq!(rig.stream.calls()[before..], ["start", "open 2", "ticket 2 c0ffee"]);
        assert_ne!(ticket(&opened), ticket(&taken));
        // While it runs, another ticket needs no new generation.
        let again = c.operator_call("vesper", operator_action(c, "viewer_ticket")).await.unwrap();
        assert_eq!(rig.stream.calls().last().map(String::as_str), Some("ticket 2 c0ffee"));
        assert!(rig.stream.connect(&ticket(&again), CERT));

        transition(c, "vesper", "handback").await.unwrap();
        let err = c.operator_call("vesper", operator_action(c, "viewer_ticket")).await.unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED", "only the holder opens a viewer");
    });
}

#[test]
fn handing_back_releases_keys_and_ends_the_stream_without_resuming_an_agent() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        let taken = take(c, "vesper").await.unwrap();
        rig.stream.connect(&ticket(&taken), CERT);
        let before = rig.stream.calls().len();
        let back = transition(c, "vesper", "handback").await.unwrap();
        assert_eq!(rig.stream.calls()[before..], ["revoke", "stop"]);
        assert_eq!((back["owner"].clone(), back["agent_resumed"].clone()), (json!("none"), json!(false)));

        // Keys not proven released: the hand back fails and the computer stays fenced.
        let taken = take(c, "vesper").await.unwrap();
        rig.stream.connect(&ticket(&taken), CERT);
        rig.stream.0.unsettled.set(true);
        let err = transition(c, "vesper", "handback").await.unwrap_err();
        assert_eq!(err.code, "CONTROL_UNSETTLED", "{err:?}");
        assert!(!rig.stream.0.running.get(), "the stream is ended anyway");
        let control = c.journal.get_control().unwrap();
        assert!(control.paused && control.unsettled, "{control:?}");
    });
}

#[test]
fn an_agent_finishes_after_a_person_took_control_without_taking_it_back() {
    run(async {
        // Handed back: the window the person never went to closes and the one
        // they used stays. Still held: every window stays with the person.
        for handed_back in [true, false] {
            let rig = rig(true);
            let c = &rig.controller;
            let task = begin(c).await;
            for (address, pid, class, title, app) in [("0x2", 200, "mousepad", "notes.txt - Mousepad", "editor"), ("0x3", 201, "foot", "shell", "terminal")] {
                rig.desktop.spawn.replace(Some(win(address, pid, class, title, false, false)));
                rig.desktop.launch_pid.set(Some(pid as u32));
                let env = call(c, "computer_act", json!({ "task_ref": task, "request_id": format!("open-{app}"), "action": { "kind": "launch", "app": app } })).await;
                assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
            }
            take(c, "vesper").await.unwrap();
            rig.clock.fetch_add(10_000, Ordering::SeqCst);
            c.on_desktop_event(DesktopEvent::Focus { address: Some("0x3".into()) });
            if handed_back {
                transition(c, "vesper", "handback").await.unwrap();
            }
            let stream_calls = rig.stream.calls().len();
            let summary = "A person took over; the note was saved.";
            let finish = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "blocked", "summary": summary })).await;
            assert_eq!(finish["status"], "ok", "handed back {handed_back}: {finish}");

            let recorded = c.journal.get_task(&task).unwrap().unwrap();
            assert_eq!(recorded.state, "blocked", "handed back {handed_back}");
            assert_eq!(recorded.completion.as_ref().map(|c| c["summary"].clone()), Some(json!(summary)), "handed back {handed_back}");
            let closes: Vec<String> = rig.desktop.acts.borrow().iter().filter(|a| a.starts_with("Close")).cloned().collect();
            let cleanup = &finish["result"]["cleanup"];
            if handed_back {
                assert_eq!(cleanup["closed"], json!(["mousepad \"notes.txt - Mousepad\""]), "{finish}");
                assert_eq!(cleanup["left"], json!([{ "surface": "foot \"shell\"", "reason": "a person used it" }]), "{finish}");
                assert!(closes.len() == 1 && closes[0].contains("0x2"), "{closes:?}");
            } else {
                let left: Vec<&str> = cleanup["left"].as_array().unwrap().iter().filter_map(|l| l["reason"].as_str()).collect();
                assert_eq!(left, ["a person has the computer", "a person has the computer"], "{finish}");
                assert!(closes.is_empty(), "nothing closes under the person: {closes:?}");
            }

            // Finishing took nothing back.
            assert!(c.journal.get_active_lease().unwrap().is_none(), "handed back {handed_back}");
            assert_eq!(rig.stream.calls().len(), stream_calls, "handed back {handed_back}: the person's stream is untouched");
            let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
            let control = c.journal.get_control().unwrap();
            if handed_back {
                assert_eq!(status["owner"], "none");
                assert!(!control.paused && !control.human_control, "{control:?}");
            } else {
                assert_eq!(status["owner"], "operator:vesper");
                assert!(control.paused && control.human_control, "{control:?}");
            }
            let again = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f2", "outcome": "complete", "summary": "again" })).await;
            assert_eq!(code(&again), if handed_back { "LEASE_EXPIRED" } else { "HUMAN_CONTROL" }, "a finished task stays finished: {again}");
        }
    });
}

fn clip_action(c: &Controller, op: &str, fields: Value) -> Value {
    let mut action = operator_action(c, op);
    action.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    action
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[test]
fn the_clipboard_is_shared_only_with_the_holder_both_ways_and_never_recorded() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        // As `start` does: the clipboard watcher reaches the controller through it.
        *c.me.borrow_mut() = Rc::downgrade(c);
        let get = |since: u64| c.operator_call("vesper", clip_action(c, "clipboard_get", json!({ "since": since })));
        let text = |s: &str| Clip { mime: "text/plain;charset=utf-8".into(), data: s.as_bytes().to_vec() };
        let settle = || tokio::time::sleep(Duration::from_millis(20));
        assert_eq!(get(0).await.unwrap_err().code, "PERMISSION_DENIED", "nobody holds control yet");

        rig.desktop.copy(text("copied before control"));
        take(c, "vesper").await.unwrap();
        let recorded = c.journal.recent_events(None, 500).unwrap().len();
        let first = get(0).await.unwrap();
        assert!(first.get("clip").is_none(), "what was copied before control is not shared: {first}");

        // A copy on this computer reaches the holder.
        rig.desktop.copy(text("copied on Tulip1"));
        settle().await;
        let got = get(0).await.unwrap();
        let seq = got["seq"].as_u64().unwrap();
        assert_eq!(got["clip"]["mime"], "text/plain;charset=utf-8");
        assert_eq!(got["clip"]["data_b64"], b64(b"copied on Tulip1"));
        assert!(get(seq).await.unwrap().get("clip").is_none(), "nothing newer");

        // A picture from the holder, in two pieces, in order only.
        let png: Vec<u8> = (0..CHUNK + 10).map(|i| i as u8).collect();
        let sha = crate::store::canonical::hex_encode(&Sha256::digest(&png));
        let piece = |offset: usize, end: usize| {
            clip_action(c, "clipboard_set", json!({ "mime": "image/png", "size": png.len(), "sha256": sha, "offset": offset, "data_b64": b64(&png[offset..end]) }))
        };
        let part = c.operator_call("vesper", piece(0, CHUNK)).await.unwrap();
        assert_eq!((part["done"].clone(), part["received"].clone()), (json!(false), json!(CHUNK)));
        assert_eq!(c.operator_call("vesper", piece(5, CHUNK + 10)).await.unwrap_err().code, "INVALID_ARGUMENT");
        let done = c.operator_call("vesper", piece(CHUNK, CHUNK + 10)).await.unwrap();
        assert_eq!(done["done"], true);
        assert_eq!(*rig.desktop.clipboard_writes.borrow(), [Clip { mime: "image/png".into(), data: png.clone() }]);
        settle().await;
        assert!(get(seq).await.unwrap().get("clip").is_none(), "what the holder sent is not sent back");

        let large = clip_action(c, "clipboard_set", json!({ "mime": "image/png", "size": CLIP_MAX + 1, "sha256": sha, "offset": 0, "data_b64": b64(b"x") }));
        assert_eq!(c.operator_call("vesper", large).await.unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(c.journal.recent_events(None, 500).unwrap().len(), recorded, "nothing about the clipboard is recorded");

        // After hand back the watcher stops and nothing more is shared.
        transition(c, "vesper", "handback").await.unwrap();
        settle().await;
        assert!(rig.desktop.clipboard_watch.borrow().as_ref().is_some_and(|tx| tx.is_closed()), "the watcher stopped");
        assert_eq!(get(0).await.unwrap_err().code, "PERMISSION_DENIED");
    });
}

#[test]
fn a_shorter_copy_during_a_large_transfer_is_fetched_from_its_start() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        *c.me.borrow_mut() = Rc::downgrade(c);
        let get = |since: u64, offset: usize| c.operator_call("vesper", clip_action(c, "clipboard_get", json!({ "since": since, "offset": offset })));
        let settle = || tokio::time::sleep(Duration::from_millis(20));
        take(c, "vesper").await.unwrap();
        get(0, 0).await.unwrap();

        // A screenshot is copied here and the console fetches its first piece.
        rig.desktop.copy(Clip { mime: "image/png".into(), data: (0..CHUNK * 3).map(|i| i as u8).collect() });
        settle().await;
        let first = get(0, 0).await.unwrap();
        assert_eq!((first["clip"]["offset"].clone(), first["clip"]["size"].clone()), (json!(0), json!(CHUNK * 3)), "{first}");

        // Before the next piece, a word is copied: that offset is past its end.
        rig.desktop.copy(Clip { mime: "text/plain;charset=utf-8".into(), data: b"a word".to_vec() });
        settle().await;
        let next = get(0, CHUNK).await.expect("not refused: a refusal ends sharing");
        assert!(next["enabled"] == true && next.get("clip").is_none(), "nothing at that offset: {next}");
        let word = get(0, 0).await.unwrap();
        assert_eq!(word["clip"]["data_b64"], b64(b"a word"));
        assert!(word["seq"].as_u64() > first["seq"].as_u64(), "{word}");
    });
}

#[test]
fn a_copy_made_after_control_ended_never_reaches_the_next_holder() {
    run(async {
        let mut rig = rig(true);
        set_grants(&mut rig, registered());
        let c = rig.controller.clone();
        *c.me.borrow_mut() = Rc::downgrade(&c);
        take(&c, "vesper").await.unwrap();

        // Vesper's first fetch is still reading the clipboard when it hands back.
        let hold = Rc::new(Notify::new());
        rig.desktop.clipboard_read_hold.replace(Some(hold.clone()));
        let fetch = {
            let c = c.clone();
            tokio::task::spawn_local(async move { c.operator_call("vesper", clip_action(&c, "clipboard_get", json!({ "since": 0 }))).await })
        };
        for _ in 0..20 {
            if rig.desktop.clipboard_watch.borrow().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(rig.desktop.clipboard_watch.borrow().is_some(), "the fetch is reading the clipboard");
        transition(&c, "vesper", "handback").await.unwrap();
        rig.desktop.clipboard_read_hold.replace(None);
        hold.notify_one();
        assert_eq!(fetch.await.unwrap().unwrap_err().code, "PERMISSION_DENIED", "control ended during the fetch");

        // Nobody holds control; the person here copies a password.
        rig.desktop.copy(Clip { mime: "text/plain;charset=utf-8".into(), data: b"hunter2".to_vec() });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Hazel takes control next: nothing copied before that is shared.
        take(&c, "hazel").await.unwrap();
        let got = c.operator_call("hazel", clip_action(&c, "clipboard_get", json!({ "since": 0 }))).await.unwrap();
        assert!(got.get("clip").is_none(), "the password reached the next holder: {got}");
    });
}
