//! Adding a computer, end to end.
//!
//! Real target daemons (`ibarad`, stub desktop tools, temporary state) listen for
//! pairing on loopback addresses that a fake `tailscale` reports as their
//! Tailscale addresses; its `whois` answers name each loopback address's
//! computer and owner from fixtures. Real operator consoles
//! (`ibarad --role operator`) are driven over their sockets; a console started
//! with its computer's own target daemon has that daemon vouch for its
//! requests. A fake `ssh` stands in for the target's sshd: it checks the pinned
//! host key and the enrolled operator key, then runs the real `ibara
//! agent-entry` against the target, so the new route reaches the real target
//! controller. Raw requests stand in for any other program on a computer.
//!
//! Computers (all fictional): `vesper` 127.0.0.3, `tulip1` 127.0.0.2 and `hazel`
//! 127.0.0.5 belong to riley@example.com; `command` 127.0.0.4 and `lab`
//! 127.0.0.8 to dana@example.net; `hazel` has no ibara target, `oldbox` is
//! offline, `server` is tagged, a phone and a Windows computer are not listed.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange, `ibara join` run and
//! access projection as JSON lines in DIR.

mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use support::*;

const ENDPOINT: &str = "host_0123456789abcdef0123456789abcdef";

/// One raw request to `to`'s pairing port, sent from `from`'s address: what any
/// program on `from` could send, bypassing the console.
fn raw(world: &World, from: &Node, to: &Node, request: &Value) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let reply = rt.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(format!("{}:0", from.ip).parse().unwrap()).unwrap();
        let stream = socket.connect(format!("{}:{}", to.ip, world.port).parse().unwrap()).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        tokio::io::BufReader::new(read).read_line(&mut line).await.unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    });
    world.record(json!({"raw_from": from.name, "to": to.name, "request": request, "reply": reply}));
    reply
}

/// A key a program makes for itself with `ssh-keygen`: its private key file
/// and its public key line.
fn fresh_key(world: &World, name: &str) -> (PathBuf, String) {
    let path = world.root.join(format!("key-{name}"));
    let made = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"]).arg(&path).status().unwrap();
    assert!(made.success());
    let public = fs::read_to_string(world.root.join(format!("key-{name}.pub"))).unwrap().trim().to_string();
    (path, public)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// A `pair` request offering `public_key`, signed at `signed_at` with the
/// private key at `signer` for the computer `to`, carrying `nonce`.
fn signed_pair(public_key: &str, signer: &Path, to: &Node, signed_at: i64, nonce: Option<&str>) -> Value {
    let statement = ibara::server::pairing::statement(to.ip.parse().unwrap(), ENDPOINT, nonce, signed_at, None);
    let mut sign = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", "ibara-pair", "-f"])
        .arg(signer)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    sign.stdin.take().unwrap().write_all(statement.as_bytes()).unwrap();
    let out = sign.wait_with_output().unwrap();
    assert!(out.status.success());
    let mut request = json!({"op": "pair", "public_key": public_key, "endpoint_id": ENDPOINT, "signed_at": signed_at,
                             "signature": String::from_utf8(out.stdout).unwrap()});
    if let Some(nonce) = nonce {
        request["nonce"] = json!(nonce);
    }
    request
}

/// Whether `computer`'s owner lets `console`'s computer administer it.
fn can_administer(console: &mut Console, computer: &str) -> bool {
    let session = console.ok("operator-session", &["--computer", computer]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    let access = console.ok("operator-access", &["--computer", computer, "--epoch", &epoch]);
    access["result"]["can_administer"].as_bool().unwrap()
}

#[test]
fn own_computers_pair_without_a_person_and_the_tailnet_lists_linux_computers() {
    let world = World::new("own");
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));

    // A fresh console: no station file is not an error.
    let status = vesper.ask("status", &[]);
    assert!(status["error"].is_null(), "{status}");
    assert_eq!(status["data"], json!({"station_configured": false}));

    let tailnet = vesper.ok("tailnet", &[]);
    assert_eq!(
        tailnet["tailscale"],
        json!({"state": "running", "login": "riley@example.com", "self_node": "vesper", "login_url": null})
    );
    let listed = computers(&tailnet);
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        ["command", "hazel", "lab", "oldbox", "owner", "server", "tulip1", "vesper"],
        "Linux computers only: {tailnet}"
    );
    assert_eq!(tailnet["computers"][0]["node"], "vesper", "this computer comes first");
    assert_eq!(
        listed["vesper"],
        json!({"node": "vesper", "dns_name": "vesper.tail0000.ts.net", "ip": "127.0.0.3", "online": true,
               "owner": "riley@example.com", "same_owner": true, "is_self": true, "ibara": "ready",
               "paired": false, "computer_id": null, "label": null})
    );
    let brief = |c: &Value| (c["ibara"].as_str().unwrap().to_string(), c["same_owner"].as_bool().unwrap(), c["owner"].clone());
    assert_eq!(brief(&listed["tulip1"]), ("ready".into(), true, json!("riley@example.com")));
    assert_eq!(brief(&listed["hazel"]), ("not_installed".into(), true, json!("riley@example.com")));
    assert_eq!(brief(&listed["oldbox"]), ("offline".into(), true, json!("riley@example.com")));
    assert_eq!(brief(&listed["command"]), ("not_installed".into(), false, json!("dana@example.net")));
    assert_eq!(brief(&listed["server"]), ("not_installed".into(), false, Value::Null), "a tagged computer has no owner");

    // Pairing to another of my computers: accepted by that computer on its own.
    assert!(!vesper.home.join(".ssh/ibara_agent_ed25519").exists());
    let started = vesper.ok("pair-start", &["tulip1"]);
    assert_eq!(started["mode"], "own_computer", "{started}");
    assert!(six_digits(&started["code"]), "{started}");
    let request_id = started["request_id"].as_str().unwrap().to_string();
    let paired = vesper.settled(&request_id);
    assert_eq!(paired["state"], "paired", "{paired}");
    assert_eq!((paired["mode"].as_str(), paired["label"].as_str()), (Some("own_computer"), Some("Tulip1")), "{paired}");
    assert_eq!(paired["code"], started["code"]);
    let computer_id = paired["computer_id"].as_str().unwrap().to_string();
    let key = fs::metadata(vesper.home.join(".ssh/ibara_agent_ed25519")).unwrap();
    assert_eq!(key.permissions().mode() & 0o777, 0o600, "the new operator key is private");

    let directory = vesper.ok("directory", &[]);
    let row = directory["computers"].as_array().unwrap().iter().find(|c| c["computer_id"] == computer_id.as_str()).unwrap().clone();
    assert_eq!((row["label"].as_str(), row["host"].as_str(), row["user"].as_str()), (Some("Tulip1"), Some("127.0.0.2"), Some("vesper")));
    route_works(&mut vesper, &computer_id);

    // The target enrolled this key for its own owner: watching, files, control and agents.
    let record = &tulip1.authority()["vesper"];
    assert_eq!((record["enabled"].as_bool(), record["operator_public_key"].as_str()), (Some(true), Some(vesper.public_key().as_str())));
    let projected = tulip1.projections().last().unwrap().clone();
    assert_eq!(projected["peers"]["vesper"], json!({"agent": true, "operator": true}), "{projected}");
    assert_eq!(projected["keys"]["vesper"], vesper.public_key());

    // Asking again is answered at once with the same computer.
    let again = vesper.ok("pair-start", &["tulip1"]);
    let again = vesper.settled(again["request_id"].as_str().unwrap());
    assert_eq!((again["state"].as_str(), again["computer_id"].as_str()), (Some("paired"), Some(computer_id.as_str())), "{again}");

    // One computer is enough: pairing to this computer itself.
    let started = vesper.ok("pair-start", &["vesper"]);
    assert_eq!(started["mode"], "own_computer");
    let own = vesper.settled(started["request_id"].as_str().unwrap());
    assert_eq!((own["state"].as_str(), own["label"].as_str()), (Some("paired"), Some("Vesper")), "{own}");
    route_works(&mut vesper, own["computer_id"].as_str().unwrap());

    let listed = computers(&vesper.ok("tailnet", &[]));
    for (name, id, label) in [("tulip1", computer_id.as_str(), "Tulip1"), ("vesper", own["computer_id"].as_str().unwrap(), "Vesper")] {
        assert_eq!(
            (listed[name]["paired"].as_bool(), listed[name]["computer_id"].as_str(), listed[name]["label"].as_str()),
            (Some(true), Some(id), Some(label)),
            "{name}"
        );
    }

    // A program on Tulip1 itself (any local account) cannot add itself as Tulip1's
    // owner through the pairing port: this computer is added only over its desktop
    // user's local socket. Nothing is enrolled for it.
    let (intruder, intruder_public) = fresh_key(&world, "intruder");
    let refused = raw(&world, node("tulip1"), node("tulip1"), &signed_pair(&intruder_public, &intruder, node("tulip1"), now_ms(), None));
    assert_eq!(refused["error"]["code"], "USE_LOCAL", "{refused}");
    let projected = tulip1.projections().last().unwrap().clone();
    let blob = intruder_public.split(' ').nth(1).unwrap();
    assert!(!projected.to_string().contains(blob), "no key enrolled: {projected}");

    // Unknown names and requests are refused plainly.
    let unknown = vesper.ask("pair-start", &["nowhere"]);
    assert!(unknown["error"]["message"].as_str().unwrap().contains("nowhere"), "{unknown}");
    let offline = vesper.ask("pair-start", &["oldbox"]);
    assert_eq!(offline["error"]["message"], "oldbox is offline. Turn it on, then try again.", "{offline}");
    let missing = vesper.ask("pair-status", &["pr_00000000000000000000000000000000"]);
    assert_eq!(missing["error"]["code"], "PAIRING_UNKNOWN", "{missing}");
    world.record(json!({"target": "tulip1", "projections": tulip1.projections()}));
}

#[test]
fn someone_elses_computer_waits_for_a_person_on_the_computer_being_added() {
    let world = World::new("other");
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 6_000);
    let mut here = Console::start(&world, "tulip1", node("tulip1"), Some(&tulip1));
    assert_eq!(here.ok("pair-requests", &[]), json!({"requests": []}));

    // Accepted on the computer being added.
    let mut dana = Console::start(&world, "command-accepted", node("command"), None);
    let started = dana.ok("pair-start", &["tulip1"]);
    assert_eq!((started["mode"].as_str(), started["state"].as_str()), (Some("needs_approval"), Some("waiting")), "{started}");
    assert!(six_digits(&started["code"]), "{started}");
    let request_id = started["request_id"].as_str().unwrap().to_string();
    let requests = here.ok("pair-requests", &[]);
    let waiting = requests["requests"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{requests}");
    assert_eq!(
        (waiting[0]["request_id"].as_str(), waiting[0]["from_owner"].as_str(), waiting[0]["from_computer"].as_str()),
        (Some(request_id.as_str()), Some("dana@example.net"), Some("command"))
    );
    assert_eq!(waiting[0]["code"], started["code"], "both screens show the same code");
    assert!(waiting[0]["expires_at"].as_i64().is_some_and(|t| t > 0), "{requests}");
    assert_eq!(dana.ok("pair-status", &[&request_id])["state"], "waiting");
    assert_eq!(here.ok("pair-answer", &[&request_id, "accept"]), json!({"request_id": request_id, "state": "paired"}));
    let paired = dana.settled(&request_id);
    assert_eq!((paired["state"].as_str(), paired["label"].as_str()), (Some("paired"), Some("Tulip1")), "{paired}");
    route_works(&mut dana, paired["computer_id"].as_str().unwrap());
    let projected = tulip1.projections().last().unwrap().clone();
    assert_eq!(projected["peers"]["command"], json!({"agent": false, "operator": true}), "someone else gets today's default rights");

    // Declined.
    let mut declined = Console::start(&world, "lab-declined", node("lab"), None);
    let started = declined.ok("pair-start", &["tulip1"]);
    let id = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(here.ok("pair-answer", &[&id, "decline"]), json!({"request_id": id, "state": "declined"}));
    assert_eq!(declined.settled(&id)["state"], "declined");
    assert_eq!(declined.ok("directory", &[])["computers"], json!([]));

    // Canceled by the person who asked.
    let started = declined.ok("pair-start", &["tulip1"]);
    let id = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(declined.ok("pair-cancel", &[&id]), json!({"request_id": id, "state": "canceled"}));
    assert_eq!(here.ok("pair-requests", &[]), json!({"requests": []}));
    assert_eq!(declined.ok("pair-status", &[&id])["state"], "canceled");

    // Accepted from a terminal with `ibara join`.
    let mut joined = Console::start(&world, "lab-joined", node("lab"), None);
    let started = joined.ok("pair-start", &["tulip1"]);
    let id = started["request_id"].as_str().unwrap().to_string();
    let code = started["code"].as_str().unwrap().to_string();
    let join = |args: &[&str]| {
        let out = Command::new(IBARA).arg("join").args(args).env("IBARA_RUNTIME_DIR", tulip1.root.join("run")).stdin(Stdio::null()).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        world.record(json!({"ibara join": args, "status": out.status.code(), "stdout": text, "stderr": String::from_utf8_lossy(&out.stderr)}));
        (out.status.code(), text)
    };
    let (status, listing) = join(&[]);
    assert_eq!(status, Some(0));
    assert!(listing.contains(&code) && listing.contains("lab") && listing.contains("dana@example.net"), "{listing}");
    let (status, _) = join(&["--accept", "000 000"]);
    assert_eq!(status, Some(1), "an unknown code accepts nothing");
    let (status, accepted) = join(&["--accept", &code.replace(' ', "")]);
    assert_eq!(status, Some(0), "{accepted}");
    assert_eq!(joined.settled(&id)["state"], "paired");

    // Expired: nobody answered in time.
    let mut late = Console::start(&world, "command-late", node("command"), None);
    let started = late.ok("pair-start", &["tulip1"]);
    let id = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(late.settled(&id)["state"], "expired");
    assert_eq!(here.ok("pair-requests", &[]), json!({"requests": []}));
    assert_eq!(here.ok("pair-answer", &[&id, "accept"]), json!({"request_id": id, "state": "expired"}), "too late to accept");
    world.record(json!({"target": "tulip1", "projections": tulip1.projections(), "authority_principals": tulip1.authority().as_object().unwrap().keys().collect::<Vec<_>>()}));
}

/// Failure cases this must catch:
/// 1. A request with no signature, or signed by a key other than the one it
///    offers (someone else's public key is world-readable), is taken.
/// 2. A genuine signed request is replayed to another computer, or minutes later.
/// 3. A program on one of the owner's computers, under any account, makes its
///    own key and is accepted without a person because the owners match.
/// 4. A made-up nonce passes for the asking computer's vouch.
/// 5. A key paired for one computer is accepted at once from another, or a
///    friend's key presented from the owner's computer gains administer.
/// 6. An own computer whose console has no ibara of its own cannot be added,
///    or a person's Accept gives it less than an own computer gets.
#[test]
fn a_request_proves_its_key_and_an_own_computer_needs_its_own_ibara_to_vouch() {
    let world = World::new("proof");
    let (tulip1_node, vesper_node, hazel_node) = (node("tulip1"), node("vesper"), node("hazel"));
    let tulip1 = Target::start(&world, tulip1_node, Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, vesper_node, None, 300_000);
    let mut on_tulip1 = Console::start(&world, "tulip1", tulip1_node, Some(&tulip1));
    let mut vesper = Console::start(&world, "vesper", vesper_node, Some(&vesper_target));
    let mut dana = Console::start(&world, "command", node("command"), None);

    // The real console, vouched for by Vesper's own ibara: accepted at once.
    let started = vesper.ok("pair-start", &["tulip1"]);
    assert_eq!(started["mode"], "own_computer", "{started}");
    assert_eq!(vesper.settled(started["request_id"].as_str().unwrap())["state"], "paired");
    // Dana's computer, accepted by a person: watching and files, not administer.
    let started = dana.ok("pair-start", &["tulip1"]);
    let id = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(on_tulip1.ok("pair-answer", &[&id, "accept"])["state"], "paired");
    let dana_t1 = dana.settled(&id)["computer_id"].as_str().unwrap().to_string();
    assert!(!can_administer(&mut dana, &dana_t1));

    let refused = |reply: &Value, code: &str| assert_eq!(reply["error"]["code"], code, "{reply}");
    let (mine, mine_public) = fresh_key(&world, "mine");
    let vesper_key = vesper.home.join(".ssh/ibara_agent_ed25519");

    // 1. Unsigned; Vesper's public key signed with a key of one's own.
    let unsigned = json!({"op": "pair", "public_key": mine_public, "endpoint_id": ENDPOINT});
    refused(&raw(&world, vesper_node, tulip1_node, &unsigned), "UNSIGNED");
    refused(&raw(&world, vesper_node, tulip1_node, &signed_pair(&vesper.public_key(), &mine, tulip1_node, now_ms(), None)), "BAD_SIGNATURE");
    // 2. Vesper's genuine signature for Lab, replayed to Tulip1; one three minutes old.
    let for_lab = signed_pair(&vesper.public_key(), &vesper_key, node("lab"), now_ms(), None);
    refused(&raw(&world, vesper_node, tulip1_node, &for_lab), "BAD_SIGNATURE");
    let stale = signed_pair(&mine_public, &mine, tulip1_node, now_ms() - 180_000, None);
    let stale = raw(&world, vesper_node, tulip1_node, &stale);
    refused(&stale, "STALE_REQUEST");
    assert!(stale["error"]["message"].as_str().unwrap().contains("two minutes"), "{stale}");

    // 3. A program on Vesper with a key it made itself, properly signed: it
    // waits for a person on Tulip1, like anyone else's computer.
    let asked = raw(&world, vesper_node, tulip1_node, &signed_pair(&mine_public, &mine, tulip1_node, now_ms(), None));
    assert_eq!((asked["mode"].as_str(), asked["state"].as_str()), (Some("needs_approval"), Some("waiting")), "{asked}");
    // 4. The same with a nonce Vesper's ibara never gave out.
    let (other, other_public) = fresh_key(&world, "other");
    let forged = signed_pair(&other_public, &other, tulip1_node, now_ms(), Some("pv_0123456789abcdef0123456789abcdef"));
    let forged = raw(&world, vesper_node, tulip1_node, &forged);
    assert_eq!((forged["mode"].as_str(), forged["state"].as_str()), (Some("needs_approval"), Some("waiting")), "{forged}");
    let requests = on_tulip1.ok("pair-requests", &[]);
    let listed: Vec<(&str, &str, &str)> = requests["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["request_id"].as_str().unwrap(), r["from_computer"].as_str().unwrap(), r["from_owner"].as_str().unwrap()))
        .collect();
    let (asked_id, forged_id) = (asked["request_id"].as_str().unwrap(), forged["request_id"].as_str().unwrap());
    assert_eq!(listed, [(asked_id, "vesper", "riley@example.com"), (forged_id, "vesper", "riley@example.com")], "{requests}");
    for id in [asked_id, forged_id] {
        assert_eq!(on_tulip1.ok("pair-answer", &[id, "decline"])["state"], "declined");
    }
    let authority = tulip1.authority().to_string();
    for public in [&mine_public, &other_public] {
        assert!(!authority.contains(public.split(' ').nth(1).unwrap()), "nothing enrolled for a key nobody accepted: {authority}");
    }

    // 5. Dana's key, copied to Hazel (Riley's): it was paired for Dana's
    // computer, so from Hazel it waits for a person and Dana's computer still
    // may not administer Tulip1.
    let dana_key = dana.home.join(".ssh/ibara_agent_ed25519");
    let leaked = raw(&world, hazel_node, tulip1_node, &signed_pair(&dana.public_key(), &dana_key, tulip1_node, now_ms(), None));
    assert_eq!((leaked["mode"].as_str(), leaked["state"].as_str()), (Some("needs_approval"), Some("waiting")), "{leaked}");
    assert!(!can_administer(&mut dana, &dana_t1), "a friend's key gains nothing from the owner's computer");
    assert_eq!(on_tulip1.ok("pair-answer", &[leaked["request_id"].as_str().unwrap(), "decline"])["state"], "declined");

    // 6. Hazel's console has no ibara of its own to vouch for it: Tulip1 asks a
    // person, whose Accept gives Hazel everything an own computer gets.
    let mut hazel = Console::start(&world, "hazel", hazel_node, None);
    let started = hazel.ok("pair-start", &["tulip1"]);
    assert_eq!((started["mode"].as_str(), started["state"].as_str()), (Some("needs_approval"), Some("waiting")), "{started}");
    let id = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(on_tulip1.ok("pair-answer", &[&id, "accept"])["state"], "paired");
    let paired = hazel.settled(&id);
    assert_eq!(paired["state"], "paired", "{paired}");
    let projected = tulip1.projections().last().unwrap().clone();
    assert_eq!(projected["peers"]["hazel"], json!({"agent": true, "operator": true}), "{projected}");
    assert!(can_administer(&mut hazel, paired["computer_id"].as_str().unwrap()));
    world.record(json!({"target": "tulip1", "projections": tulip1.projections(), "authority": tulip1.authority()}));
}

/// Failure cases this must catch:
/// 1. A computer whose own name copies another computer's label is added
///    under that same label, so two cards read the same.
/// 2. A computer renaming itself to another computer's label (in any case)
///    takes that label here.
/// 3. A computer's own new name that no other computer here has is not taken.
#[test]
fn two_computers_never_read_the_same_here() {
    let world = World::new("names");
    let _tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), Some("tulip1"), 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let label = |console: &mut Console, id: &str| {
        let directory = console.ok("directory", &[]);
        let row = directory["computers"].as_array().unwrap().iter().find(|c| c["computer_id"] == id).unwrap().clone();
        row["label"].as_str().unwrap().to_string()
    };
    let started = vesper.ok("pair-start", &["tulip1"]);
    let t1 = vesper.settled(started["request_id"].as_str().unwrap())["computer_id"].as_str().unwrap().to_string();
    let started = vesper.ok("pair-start", &["vesper"]);
    let paired = vesper.settled(started["request_id"].as_str().unwrap());
    let vx = paired["computer_id"].as_str().unwrap().to_string();
    assert_eq!((label(&mut vesper, &t1).as_str(), paired["label"].as_str()), ("Tulip1", Some("tulip1 (vesper)")), "{paired}");

    let session = vesper.ok("operator-session", &["--computer", &vx]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    let rename = |console: &mut Console, name: &str| {
        let set = console.ok("operator-settings", &["--computer", &vx, "--epoch", &epoch, "set", "name", name]);
        assert_eq!(set["result"]["value"], name, "{set}");
    };
    rename(&mut vesper, "TULIP1");
    assert_eq!(label(&mut vesper, &vx), "tulip1 (vesper)", "another computer's name is not taken");
    rename(&mut vesper, "Vesper Two");
    assert_eq!(label(&mut vesper, &vx), "Vesper Two");
}

/// Failure cases this must catch:
/// 1. A computer whose Tailscale name is `owner` pairs under the name `owner`
///    and replaces this computer's local owner (the identity `computerctl`
///    acts as), so the owner's access edits are refused from then on.
/// 2. That computer cannot pair at all, or pairs without its rights.
/// 3. `computerctl enroll_operator owner …` enrols a computer as `owner`.
#[test]
fn a_computer_called_owner_never_takes_the_local_owners_name() {
    let world = World::new("owner-name");
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let owner_target = Target::start(&world, node("owner"), None, 300_000);
    let mut console = Console::start(&world, "owner", node("owner"), Some(&owner_target));

    let started = console.ok("pair-start", &["tulip1"]);
    assert_eq!(started["mode"], "own_computer", "{started}");
    let paired = console.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    let computer_id = paired["computer_id"].as_str().unwrap().to_string();
    route_works(&mut console, &computer_id);
    assert!(can_administer(&mut console, &computer_id), "an own computer's rights, under another name");
    let authority = tulip1.authority();
    assert!(authority.get("owner").is_none(), "{authority}");
    assert_eq!(authority["owner-2"]["operator_public_key"].as_str(), Some(console.public_key().as_str()), "{authority}");

    // The local owner is still the person with the admin key, and can still edit access.
    let (code, access) = tulip1.admin(&["access"]);
    assert_eq!(code, Some(0), "{access}");
    let access = &access["result"];
    assert_eq!((access["identities"]["owner"]["kind"].as_str(), access["can_administer"].as_bool()), (Some("person"), Some(true)), "{access}");
    assert_eq!(access["identities"]["owner-2"]["kind"], "computer", "{access}");
    let edit = json!({"subject": "owner-2", "capability": "files", "rule": "ask", "expected_revision": access["revision"]}).to_string();
    let (code, edited) = tulip1.admin(&["access_set", &edit]);
    assert_eq!(code, Some(0), "{edited}");

    let digest = "a".repeat(64);
    let (code, refused) = tulip1.admin(&["enroll_operator", "owner", &digest, "host_12345678", "ibara_12345678", "SHA256:abcdefghijklmnop"]);
    assert_ne!(code, Some(0), "{refused}");
    assert!(tulip1.authority().get("owner").is_none());
    world.record(json!({"target": "tulip1", "authority": tulip1.authority(), "access": tulip1.admin(&["access"]).1}));
}
