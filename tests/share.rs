//! Sharing a computer with a friend, end to end, in the pairing world of
//! `tests/pairing.rs` (see `tests/support`): a real target daemon on Tulip1,
//! Riley's own console there making invites, and real consoles on Dana's
//! computers (`command`, `lab`) and on Riley's Vesper entering the codes.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange as JSON lines in DIR.

mod support;

use serde_json::{Value, json};
use std::fs;
use support::*;

fn failed(envelope: &Value, code: &str) -> String {
    assert_eq!(envelope["error"]["code"], code, "{envelope}");
    envelope["error"]["message"].as_str().unwrap().to_string()
}

/// `console`'s view of `computer`'s access table.
fn access(console: &mut Console, computer: &str) -> Value {
    let session = console.ok("operator-session", &["--computer", computer]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    console.ok("operator-access", &["--computer", computer, "--epoch", &epoch])["result"].clone()
}

fn row<'a>(table: &'a Value, subject: &str) -> &'a Value {
    table["rows"].as_array().unwrap().iter().find(|r| r["subject"] == subject).unwrap_or_else(|| panic!("no {subject}: {table}"))
}

/// The rule for each capability, and every grant's end, for `subject`.
fn rights(table: &Value, subject: &str) -> (Value, Vec<Value>) {
    let capabilities = row(table, subject)["capabilities"].clone();
    let ends = table["grants"].as_object().unwrap().values().filter(|g| g["subject"] == subject).map(|g| g["expires_at"].clone()).collect();
    (capabilities, ends)
}

fn caps(watch: &str, files: &str, control: &str, agents: &str) -> Value {
    json!({"watch": watch, "files": files, "control": control, "agents": agents, "administer": "deny"})
}

fn invite(owner: &mut Console, level: &str, lasts: &str) -> (String, String, Value) {
    let made = owner.ok("invite-create", &[level, lasts])["invite"].clone();
    let code = made["code"].as_str().unwrap().to_string();
    (made["id"].as_str().unwrap().to_string(), code, made)
}

fn listed(owner: &mut Console, id: &str) -> Option<Value> {
    owner.ok("invites", &[])["invites"].as_array().unwrap().iter().find(|i| i["id"] == id).cloned()
}

/// Failure cases this must catch:
/// 1. A friend's computer with a valid code waits for a person, or gets more or
///    less than the invite's level (administer, agent tasks without asking), or
///    a grant without the invite's end.
/// 2. The same code works a second time; an expired or revoked code works.
/// 3. A wrong code is told apart from a used or expired one (so a guess learns
///    whether a code exists), or guessing is unlimited: after 5 codes that
///    didn't work, a good code from that computer still gets in (or is used up).
/// 4. The code is stored readable on the computer being shared.
/// 5. Revoking a used invite leaves the friend's route working.
/// 6. The owner can't see who joined: nothing in Access or on the timeline.
/// 7. The owner's own computer sending a code is treated as a friend's (loses
///    administer) or uses the invite up.
#[test]
fn a_friend_joins_with_an_invite_for_exactly_its_level_until_it_ends() {
    let world = World::new("share");
    let (tulip1_node, vesper_node) = (node("tulip1"), node("vesper"));
    let tulip1 = Target::start(&world, tulip1_node, Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, vesper_node, None, 300_000);
    let mut owner = Console::start(&world, "tulip1", tulip1_node, Some(&tulip1));
    let mut dana = Console::start(&world, "command", node("command"), None);
    let mut lab = Console::start(&world, "lab", node("lab"), None);
    let mut vesper = Console::start(&world, "vesper", vesper_node, Some(&vesper_target));

    // The owner's console has this computer on its fleet, and nothing shared yet.
    let started = owner.ok("pair-start", &["tulip1"]);
    let own_id = owner.settled(started["request_id"].as_str().unwrap())["computer_id"].as_str().unwrap().to_string();
    let listing = owner.ok("invites", &[]);
    assert_eq!(listing["invites"], json!([]), "{listing}");
    assert_eq!(listing["tailscale"]["share_url"], "https://login.tailscale.com/admin/machines/127.0.0.2", "{listing}");

    // 1. Watch for a day: Dana's computer types the code as read aloud and is added at once.
    let (watch_id, code, made) = invite(&mut owner, "watch", "day");
    let valid = regex_like(&code);
    assert!(valid, "a code like 4H7K-92QX: {code}");
    let ends = made["expires_at"].as_i64().unwrap();
    assert!((ends - now_ms() - 86_400_000).abs() < 60_000, "{made}");
    // 4. Only its digest is kept.
    let stored = fs::read_to_string(tulip1.root.join("state/invites.json")).unwrap();
    assert!(!stored.contains(&code) && !stored.contains(&code.replace('-', "")), "{stored}");
    let listed_before = listed(&mut owner, &watch_id).unwrap();
    assert!(listed_before.get("code").is_none() && listed_before["used"].is_null(), "{listed_before}");

    let typed = code.to_lowercase().replace('-', " ");
    let joined = dana.ok("pair-start", &["tulip1", &typed]);
    assert_eq!((joined["mode"].as_str(), joined["state"].as_str()), (Some("invite"), Some("paired")), "no person needed: {joined}");
    let dana_t1 = joined["computer_id"].as_str().unwrap().to_string();
    route_works(&mut dana, &dana_t1);
    let seen_by_dana = access(&mut dana, &dana_t1);
    assert_eq!(seen_by_dana["can_administer"], false);
    let (granted, grant_ends) = rights(&seen_by_dana, "command");
    assert_eq!(granted, caps("allow", "deny", "deny", "deny"), "exactly Watch");
    let end_iso = json!(iso(ends));
    assert!(grant_ends.len() == 5 && grant_ends.iter().all(|e| *e == end_iso), "every grant ends with the invite: {grant_ends:?}");
    let projected = tulip1.projections().last().unwrap().clone();
    assert_eq!(projected["peers"]["command"], json!({"agent": false, "operator": true}), "{projected}");
    assert_eq!(listed(&mut owner, &watch_id).unwrap()["used"]["login"], "dana@example.net");

    // 6. The owner sees who joined, in Access and on the timeline.
    let seen_by_owner = access(&mut owner, &own_id);
    let dana_row = row(&seen_by_owner, "command");
    assert_eq!((dana_row["owner"].as_str(), dana_row["invite"].as_str()), (Some("dana@example.net"), Some("watch")), "{dana_row}");
    let away = owner.ok("away", &[]);
    let sentence = format!("dana@example.net's command joined with an invite: Watch until {}.", ibara::ids::local_short(ends));
    let summaries: Vec<&str> =
        away["computers"].as_array().unwrap().iter().flat_map(|c| c["events"].as_array().unwrap()).filter_map(|e| e["summary"].as_str()).collect();
    assert!(summaries.contains(&sentence.as_str()), "{sentence:?} in {summaries:?}");

    // 2 and 3. Used, wrong, expired and revoked codes all get the same plain refusal.
    let used_again = failed(&lab.ask("pair-start", &["tulip1", &code]), "INVITE_REFUSED");
    let wrong = failed(&lab.ask("pair-start", &["tulip1", "ZZZZ-ZZZZ"]), "INVITE_REFUSED");
    assert_eq!(used_again, wrong, "a used code reads like a wrong one");
    let (expiring_id, expiring, _) = invite(&mut owner, "take_control", "hour");
    let mut file: Value = serde_json::from_str(&fs::read_to_string(tulip1.root.join("state/invites.json")).unwrap()).unwrap();
    for entry in file["invites"].as_array_mut().unwrap() {
        if entry["id"] == expiring_id.as_str() {
            entry["expires_at"] = json!(now_ms() + 1_500);
        }
    }
    fs::write(tulip1.root.join("state/invites.json"), file.to_string()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2_000));
    assert_eq!(failed(&lab.ask("pair-start", &["tulip1", &expiring]), "INVITE_REFUSED"), wrong, "an expired code reads like a wrong one");
    assert!(listed(&mut owner, &expiring_id).is_none(), "an ended invite leaves the list");
    let (revoked_id, revoked, _) = invite(&mut owner, "use_with_approval", "week");
    assert_eq!(owner.ok("invite-revoke", &[&revoked_id]), json!({"id": revoked_id, "ended": false}));
    assert!(listed(&mut owner, &revoked_id).is_none());
    assert_eq!(failed(&lab.ask("pair-start", &["tulip1", &revoked]), "INVITE_REFUSED"), wrong, "a revoked code reads like a wrong one");

    // Lab joins with approval, until revoked: files, take control and agent tasks ask first.
    let (lab_invite, lab_code, _) = invite(&mut owner, "use_with_approval", "never");
    let joined = lab.ok("pair-start", &["tulip1", &lab_code]);
    assert_eq!(joined["state"], "paired", "{joined}");
    let lab_t1 = joined["computer_id"].as_str().unwrap().to_string();
    let (granted, grant_ends) = rights(&access(&mut lab, &lab_t1), "lab");
    assert_eq!(granted, caps("allow", "ask", "ask", "ask"), "exactly Use with Approval");
    assert!(grant_ends.iter().all(Value::is_null), "until revoked: {grant_ends:?}");
    let session = lab.ok("operator-session", &["--computer", &lab_t1]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    let asked = lab.ok("operator-files", &["--computer", &lab_t1, "--epoch", &epoch, "--op", "files_roots"]);
    assert_eq!(asked["result"]["state"], "pending_approval", "files ask first: {asked}");

    // 5. Revoking the used invite ends Lab's pairing at once.
    assert_eq!(owner.ok("invite-revoke", &[&lab_invite]), json!({"id": lab_invite, "ended": true}));
    let after = lab.ask("operator-status", &["--computer", &lab_t1, "--epoch", &epoch]);
    assert!(!after["error"].is_null(), "Lab can't reach Tulip1 any more: {after}");
    assert!(!access(&mut owner, &own_id)["pairings"]["lab"]["active"].as_bool().unwrap(), "the pairing ended");
    route_works(&mut dana, &dana_t1);

    // 3. Five codes that didn't work from Dana's computer: then even a good one is refused, and not used up.
    for _ in 0..5 {
        failed(&dana.ask("pair-start", &["tulip1", "WWWW-WWWW"]), "INVITE_REFUSED");
    }
    let (good_id, good, _) = invite(&mut owner, "take_control", "day");
    failed(&dana.ask("pair-start", &["tulip1", &good]), "TOO_MANY_TRIES");
    assert!(listed(&mut owner, &good_id).unwrap()["used"].is_null(), "still unused");

    // 7. The owner's own Vesper is added as always, even with a code: administer, and the code stays unused.
    let started = vesper.ok("pair-start", &["tulip1", &good]);
    assert_eq!(started["mode"], "own_computer", "{started}");
    let vesper_t1 = vesper.settled(started["request_id"].as_str().unwrap())["computer_id"].as_str().unwrap().to_string();
    assert_eq!(access(&mut vesper, &vesper_t1)["can_administer"], true);
    assert!(listed(&mut owner, &good_id).unwrap()["used"].is_null(), "an own computer uses no invite");
    world.record(json!({"target": "tulip1", "projections": tulip1.projections(), "authority": tulip1.authority(), "invites": owner.ok("invites", &[])}));
}

/// Failure cases this must catch:
/// 1. Revoking a friend's old, used invite ends the pairing a newer invite gave
///    the same computer (Watch upgraded to Take Control).
/// 2. A friend's computer reads who else this computer is shared with: another
///    computer's row, pairing or grants, or anyone's Tailscale login, computer
///    name or invite level.
#[test]
fn an_upgraded_friend_keeps_the_newer_invite_and_sees_only_itself() {
    let world = World::new("share-upgrade");
    let tulip1_node = node("tulip1");
    let tulip1 = Target::start(&world, tulip1_node, Some("Tulip1"), 300_000);
    let mut owner = Console::start(&world, "tulip1", tulip1_node, Some(&tulip1));
    let mut dana = Console::start(&world, "command", node("command"), None);
    let mut lab = Console::start(&world, "lab", node("lab"), None);
    let started = owner.ok("pair-start", &["tulip1"]);
    let own_id = owner.settled(started["request_id"].as_str().unwrap())["computer_id"].as_str().unwrap().to_string();

    // Dana's computer joins with Watch, then is upgraded with a Take Control invite.
    let (watch_id, watch_code, _) = invite(&mut owner, "watch", "day");
    assert_eq!(dana.ok("pair-start", &["tulip1", &watch_code])["state"], "paired");
    let (control_id, control_code, _) = invite(&mut owner, "take_control", "never");
    let upgraded = dana.ok("pair-start", &["tulip1", &control_code]);
    assert_eq!(upgraded["state"], "paired", "{upgraded}");
    let dana_t1 = upgraded["computer_id"].as_str().unwrap().to_string();
    for id in [&watch_id, &control_id] {
        assert_eq!(listed(&mut owner, id).unwrap()["used"]["computer"], "command", "both invites are used");
    }
    let (_, lab_code, _) = invite(&mut owner, "use_with_approval", "week");
    assert_eq!(lab.ok("pair-start", &["tulip1", &lab_code])["state"], "paired");

    // 2. Dana's computer sees only itself; the owner sees everyone and whose they are.
    let seen = access(&mut dana, &dana_t1);
    let subjects: Vec<&str> = seen["rows"].as_array().unwrap().iter().map(|r| r["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, ["command"], "{seen}");
    assert_eq!(seen["pairings"].as_object().unwrap().keys().collect::<Vec<_>>(), ["command"], "{seen}");
    assert!(seen["identities"].as_object().unwrap().keys().all(|k| k == "command"), "{seen}");
    assert!(seen["grants"].as_object().unwrap().values().all(|g| g["subject"] == "command"), "{seen}");
    let text = seen.to_string();
    assert!(!text.contains("\"lab\"") && !text.contains("@example.") && !text.contains("computer_name"), "{text}");
    assert_eq!(seen["can_administer"], false);
    let by_owner = access(&mut owner, &own_id);
    assert_eq!((row(&by_owner, "lab")["owner"].as_str(), row(&by_owner, "lab")["invite"].as_str()), (Some("dana@example.net"), Some("use_with_approval")));
    assert_eq!(row(&by_owner, "command")["invite"], "take_control");

    // 1. Tidying up the old Watch invite keeps the upgrade; revoking the newer one ends it.
    assert_eq!(owner.ok("invite-revoke", &[&watch_id]), json!({"id": watch_id, "ended": false}));
    route_works(&mut dana, &dana_t1);
    let (granted, _) = rights(&access(&mut dana, &dana_t1), "command");
    assert_eq!(granted, caps("allow", "allow", "allow", "ask"), "still Take Control");
    let session = dana.ok("operator-session", &["--computer", &dana_t1]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    assert_eq!(owner.ok("invite-revoke", &[&control_id]), json!({"id": control_id, "ended": true}));
    let after = dana.ask("operator-status", &["--computer", &dana_t1, "--epoch", &epoch]);
    assert!(!after["error"].is_null(), "the upgrade ended with its invite: {after}");
    world.record(json!({"target": "tulip1", "authority": tulip1.authority(), "invites": owner.ok("invites", &[])}));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn iso(ms: i64) -> String {
    ibara::ids::iso_from_millis(ms)
}

/// Four and four of `23456789ABCDEFGHJKMNPQRSTWXYZ`, a dash between.
fn regex_like(code: &str) -> bool {
    let alphabet = "23456789ABCDEFGHJKMNPQRSTWXYZ";
    code.len() == 9 && code.char_indices().all(|(i, c)| if i == 4 { c == '-' } else { alphabet.contains(c) })
}
