//! Turning approvals off: Ask before agents send, spend or delete; Always
//! Allow on an approval; and an agent asking a person to stop asking. The
//! paired computer `vesper` is the owner's own, with every permission and
//! its agent rules as pairing writes them; its agents are `codex@vesper`
//! and `claude@vesper`. A friend's computer, `hazel`, joins with an invite
//! where a test needs one.

use super::*;
use crate::access::{Access, CAPABILITIES, CLASSES, Identity, PairRights, Pairing, Rule, ShareLevel, default_effect};
use std::collections::BTreeMap;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// The root access helper, a fake socket. It answers each projection (no
/// while `fail` is set), and runs `during` once, before it answers the next.
#[derive(Clone, Default)]
struct Helper {
    fail: Rc<Cell<bool>>,
    during: Rc<RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>>,
}

/// A rig whose access model has the local owner and `vesper`, and whose
/// Ask before agents send, spend or delete is the returned switch (on).
/// `vesper_effects` are set on vesper's agent rules after pairing's.
fn owned_rig(vesper_effects: &[(&str, Rule)]) -> (Rig, Rc<Cell<bool>>, Helper) {
    let mut rig = rig(false);
    let switch = Rc::new(Cell::new(true));
    let read = switch.clone();
    let path = rig.dir.join("a.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("helper socket");
    let helper = Helper::default();
    let answers = helper.clone();
    tokio::task::spawn_local(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let _ = tokio::io::BufReader::new(read).read_line(&mut String::new()).await;
            let during = answers.during.borrow_mut().take();
            if let Some(during) = during {
                during.await;
            }
            let reply: &[u8] = if answers.fail.get() { b"{\"ok\":false}\n" } else { b"{\"ok\":true}\n" };
            let _ = write.write_all(reply).await;
        }
    });
    let controller = Rc::get_mut(&mut rig.controller).expect("unshared controller");
    controller.access_socket = path;
    controller.ask_first = Rc::new(move || read.get());
    let mut a = Access::empty();
    a.identities.insert("owner".into(), Identity { kind: "person".into(), computer: "owner".into(), key: "admin-sha256:0".into() });
    a.identities.insert("vesper".into(), Identity { kind: "computer".into(), computer: "vesper".into(), key: "SHA256:vesper".into() });
    a.pairings.insert("vesper".into(), Pairing { key: "SHA256:vesper".into(), endpoint: None, active: true, generation: 3 });
    let mut paired: BTreeMap<String, Rule> = CLASSES.iter().map(|c| (c.to_string(), default_effect(c))).collect();
    for (class, rule) in vesper_effects {
        paired.insert(class.to_string(), *rule);
    }
    for cap in CAPABILITIES {
        a.import_grant("owner", cap, true, None, BTreeMap::new());
        a.import_grant("vesper", cap, true, None, if cap == "agents" { paired.clone() } else { BTreeMap::new() });
    }
    a.save(&rig.controller.journal, "owner", "Test access").unwrap();
    (rig, switch, helper)
}

/// A friend's computer `computer` joins with an invite at `level`, ending at
/// `expires_at`, as pairing records it.
async fn share(c: &Controller, computer: &str, level: ShareLevel, expires_at: Option<String>) {
    let binding = json!({ "operator_key_fingerprint": format!("SHA256:{computer}") });
    let rights = PairRights::Invite { level, expires_at, summary: format!("Shared this computer with {computer}") };
    c.access_pair(computer, &binding, 1, &rights).await.unwrap();
}

fn model(c: &Controller) -> Access {
    Access::load(&c.journal).unwrap().expect("access model")
}

/// The owner changes access here (`computerctl access_set` or
/// `access_remove`), at the current revision.
async fn change(c: &Controller, mut action: Value) {
    action["expected_revision"] = json!(model(c).revision);
    c.admin(action).await.unwrap();
}

/// `agent` on `computer` calls `tool`.
async fn call_from(c: &Controller, computer: &str, agent: &str, tool: &str, args: Value) -> Value {
    c.call(computer, &format!("connection_{agent}_{computer}"), agent, tool, args, Cancel::new()).await.envelope
}

async fn agent_call(c: &Controller, agent: &str, tool: &str, args: Value) -> Value {
    call_from(c, "vesper", agent, tool, args).await
}

async fn begin_as(c: &Controller, agent: &str) -> String {
    begin_on(c, "vesper", agent).await
}

/// `agent` on `computer` begins a task; when its computer's Agent Tasks asks
/// first, the owner approves it and the agent sends it again.
async fn begin_on(c: &Controller, computer: &str, agent: &str) -> String {
    let args = json!({ "goal": "Reply to the team", "request_id": id("req") });
    let mut begun = call_from(c, computer, agent, "computer_begin", args.clone()).await;
    if begun["status"] == "pending" {
        answer(c, begun["result"]["attention"].as_str().unwrap(), "approve").await;
        begun = call_from(c, computer, agent, "computer_begin", args).await;
    }
    assert_eq!(begun["status"], "ok", "{begun}");
    begun["result"]["task_ref"].as_str().unwrap().to_string()
}

fn send_key(task: &str, request_id: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "key", "keys": "Return" }, "effect": "send" })
}

fn command(task: &str, request_id: &str, command: &[&str], effect: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "command": command, "effect": effect })
}

/// A person at `vesper` answers `att` from its console.
async fn answer_from_vesper(c: &Controller, att: &str, answer: &str) -> crate::error::Result<Value> {
    let mut action = operator_action(c, "answer_attention");
    action["att_ref"] = json!(att);
    action["answer"] = json!(answer);
    c.operator_call("vesper", action).await
}

/// `agent`'s own agent rules on this computer, as its grant sets them.
fn own_rules(c: &Controller, agent: &str) -> Value {
    let row = model(c).own_row(agent, c.now_ms(), true);
    row["own_effects"].clone()
}

/// What each agent's request to stop asking says, as vesper's console lists it.
async fn stop_asking_requests(c: &Controller) -> Vec<String> {
    let listed = c.operator_call("vesper", operator_action(c, "attention")).await.unwrap();
    let summaries = listed["items"].as_array().unwrap().iter().filter_map(|i| i["summary"].as_str());
    summaries.filter(|s| s.contains("asks to stop asking")).map(str::to_string).collect()
}

async fn finish(c: &Controller, computer: &str, agent: &str, task: &str) {
    let finished = call_from(c, computer, agent, "computer_finish", json!({ "task_ref": task, "request_id": id("fin"), "outcome": "partial", "summary": "Sent" })).await;
    assert_eq!(finished["status"], "ok", "{finished}");
}

/// Failure cases:
/// 1. With the switch off, a step declared send, a command that spends or
///    one that deletes still waits for a person.
/// 2. Off lets a kind that is denied run, or overrides an agent a person set
///    to ask first.
/// 3. Turned on again, a send does not wait.
#[test]
fn with_ask_first_off_a_send_runs_without_a_person() {
    run(async {
        let (rig, switch, _) = owned_rig(&[("spend", Rule::Deny)]);
        let c = &rig.controller;
        let task = begin_as(c, "codex").await;
        switch.set(false);
        let sent = agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await;
        assert_eq!((sent["status"].as_str(), sent["result"]["steps"][0]["outcome"].as_str()), (Some("ok"), Some("done")), "{sent}");
        assert_eq!(rig.desktop.acts(), 1);
        let deleted = agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["true"], "destructive")).await;
        assert_eq!(deleted["status"], "ok", "{deleted}");
        assert_eq!(c.journal.count_open_attention(None).unwrap(), 0, "nobody was asked");

        let spent = agent_call(c, "codex", "computer_exec", command(&task, "exec-2", &["true"], "spend")).await;
        assert_eq!(code(&spent), "PERMISSION_DENIED", "a denied kind stays denied: {spent}");

        let revision = model(c).revision;
        let ask = json!({ "op": "access_set", "subject": "codex@vesper", "capability": "agents", "rule": "allow", "effects": { "send": "ask" }, "expected_revision": revision });
        c.admin(ask).await.unwrap();
        held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-2")).await, "send");
        assert_eq!(rig.desktop.acts(), 1, "an agent set to ask first still asks");

        switch.set(true);
        held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-3", &["true"], "destructive")).await, "destructive");
    });
}

/// Failure cases:
/// 1. The approved step does not run when the agent sends it again.
/// 2. That agent's next send, or the same send under a new request, waits.
/// 3. Its spends, or another agent's sends from the same computer, stop
///    asking; the computer's own rules change.
/// 4. A step the person refused runs without asking afterwards.
/// 5. Always Allow changes access from a computer that may not administer
///    here without asking, or on a question (whose answer is only text).
#[test]
fn always_allow_lets_that_agent_send_here_without_asking() {
    run(async {
        let (rig, _, _) = owned_rig(&[]);
        let c = &rig.controller;
        let task = begin_as(c, "codex").await;
        let refused = held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["rm", "-f", "draft"], "send")).await, "send");
        answer_from_vesper(c, &refused, "deny").await.unwrap();

        let question = agent_call(c, "codex", "computer_checkpoint", json!({ "task_ref": task, "ask": { "question": "Which team?" } })).await;
        let question = question["result"]["attention"].as_str().unwrap().to_string();
        let revision = model(c).revision;
        let answered = answer_from_vesper(c, &question, "always").await.unwrap();
        assert!(answered.get("allowed").is_none() && model(c).revision == revision, "a question's answer changes no access: {answered}");

        held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        let revision = model(c).revision;
        let set = |rule: &str, revision: u64| json!({ "op": "access_set", "subject": "vesper", "capability": "administer", "rule": rule, "expected_revision": revision });
        c.admin(set("ask", revision)).await.unwrap();
        let att = held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1b")).await, "send");
        let asked = answer_from_vesper(c, &att, "always").await.unwrap();
        assert_eq!(asked["state"], "pending_approval", "without administer it is itself a request: {asked}");
        assert_eq!(own_rules(c, "codex@vesper"), json!({}), "nothing changed yet");
        c.admin(set("allow", model(c).revision)).await.unwrap();
        let att = held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1c")).await, "send");
        let vesper_before = model(c).grants["vesper:agents"].effects.clone();

        let answered = answer_from_vesper(c, &att, "always").await.unwrap();
        assert_eq!(answered["allowed"], json!({ "agent": "codex@vesper", "kinds": ["send"] }), "{answered}");
        assert_eq!(answered["item"]["answer"], "approve", "{answered}");
        assert_eq!(own_rules(c, "codex@vesper"), json!({ "send": "allow" }));
        assert_eq!(model(c).grants["vesper:agents"].effects, vesper_before, "vesper's other agents keep their rules");
        let ran = agent_call(c, "codex", "computer_act", send_key(&task, "act-1c")).await;
        assert_eq!(ran["result"]["steps"][0]["outcome"], "done", "{ran}");
        let again = agent_call(c, "codex", "computer_act", send_key(&task, "act-2")).await;
        assert_eq!(again["result"]["steps"][0]["outcome"], "done", "the same send under a new request: {again}");
        let other = agent_call(c, "codex", "computer_exec", command(&task, "exec-2", &["true"], "send")).await;
        assert_eq!(other["status"], "ok", "{other}");
        assert_eq!(rig.desktop.acts(), 2);

        held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-3", &["rm", "-f", "draft"], "change")).await, "send");
        held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-4", &["true"], "spend")).await, "spend");
        let finished = agent_call(c, "codex", "computer_finish", json!({ "task_ref": task, "request_id": "fin", "outcome": "partial", "summary": "Sent" })).await;
        assert_eq!(finished["status"], "ok", "{finished}");
        let theirs = begin_as(c, "claude").await;
        held_as(c, &agent_call(c, "claude", "computer_act", send_key(&theirs, "act-1")).await, "send");
    });
}

/// Failure cases:
/// 1. Asking changes access, or lets a send run, before a person answers.
/// 2. Asking again while the request waits shows the person a second one.
/// 3. Not Now changes anything.
/// 4. Allow lets another agent, a kind the agent is denied, or changes of
///    access run without asking.
/// 5. The agent reaches its own rules through its tools: ibara's settings
///    file is refused to it.
#[test]
fn an_agents_request_to_stop_asking_changes_nothing_until_a_person_allows_it() {
    run(async {
        let (rig, _, _) = owned_rig(&[("destructive", Rule::Deny)]);
        let c = &rig.controller;
        let task = begin_as(c, "codex").await;
        let revision = model(c).revision;
        let stop = || json!({ "task_ref": task, "stop_asking": true });
        let asked = agent_call(c, "codex", "computer_checkpoint", stop()).await;
        assert_eq!(asked["result"]["stop_asking"]["state"], "waiting_for_person", "{asked}");
        let att = asked["result"]["stop_asking"]["attention"].as_str().unwrap().to_string();
        let again = agent_call(c, "codex", "computer_checkpoint", stop()).await;
        assert_eq!(again["result"]["stop_asking"]["attention"], att.as_str(), "one request at a time: {again}");
        let listed = c.operator_call("vesper", operator_action(c, "attention")).await.unwrap();
        let summaries: Vec<&str> = listed["items"].as_array().unwrap().iter().map(|i| i["summary"].as_str().unwrap()).collect();
        assert_eq!(summaries, ["codex@vesper asks to stop asking you before it sends or spends on Tulip1."], "{listed}");
        assert_eq!(model(c).revision, revision, "asking changed no access");
        held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");

        let written = agent_call(c, "codex", "computer_files", json!({ "task_ref": task, "request_id": "w1", "op": "write", "path": "~/.config/ibara/settings.toml", "text": "[agents]\nask_first = \"off\"\n" })).await;
        assert_eq!(code(&written), "INVALID_ARGUMENT", "{written}");
        assert!(!rig.dir.join(".config/ibara/settings.toml").exists());

        answer_from_vesper(c, &att, "deny").await.unwrap();
        assert_eq!(model(c).revision, revision, "Not Now changed nothing");
        let asked = agent_call(c, "codex", "computer_checkpoint", stop()).await;
        let att = asked["result"]["stop_asking"]["attention"].as_str().unwrap().to_string();
        let allowed = answer_from_vesper(c, &att, "approve").await.unwrap();
        assert_eq!(allowed["allowed"], json!({ "agent": "codex@vesper", "kinds": ["send", "spend"] }), "{allowed}");
        assert_eq!(own_rules(c, "codex@vesper"), json!({ "send": "allow", "spend": "allow" }));

        let sent = agent_call(c, "codex", "computer_act", send_key(&task, "act-2")).await;
        assert_eq!(sent["result"]["steps"][0]["outcome"], "done", "{sent}");
        let spent = agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["true"], "spend")).await;
        assert_eq!(spent["status"], "ok", "{spent}");
        let deleted = agent_call(c, "codex", "computer_exec", command(&task, "exec-2", &["true"], "destructive")).await;
        assert_eq!(code(&deleted), "PERMISSION_DENIED", "{deleted}");
        let row = model(c).own_row("codex@vesper", c.now_ms(), true);
        assert_eq!(row["effects"]["access"], "ask", "changes of access still ask: {row}");
        let nothing = agent_call(c, "codex", "computer_checkpoint", stop()).await;
        assert_eq!(nothing["result"]["stop_asking"], json!({ "state": "nothing_to_ask" }), "{nothing}");

        let finished = agent_call(c, "codex", "computer_finish", json!({ "task_ref": task, "request_id": "fin", "outcome": "partial", "summary": "Sent" })).await;
        assert_eq!(finished["status"], "ok", "{finished}");
        let theirs = begin_as(c, "claude").await;
        held_as(c, &agent_call(c, "claude", "computer_act", send_key(&theirs, "act-1")).await, "send");
    });
}

/// Always Allow for a friend's agent, whose computer's invite ends in an hour.
/// Failure cases:
/// 1. Once the share ended, the agent still runs agent tasks here, or its
///    computer's agent transport stays open.
/// 2. Given Agent Tasks again later, the agent sends without asking: its
///    Always Allow outlived the share it came from.
#[test]
fn always_allow_ends_with_the_share_it_came_from() {
    run(async {
        let (rig, _, _) = owned_rig(&[]);
        let c = &rig.controller;
        share(c, "hazel", ShareLevel::TakeControl, Some(crate::ids::iso_from_millis(c.now_ms() + 3_600_000))).await;
        let task = begin_on(c, "hazel", "codex").await;
        let att = held_as(c, &call_from(c, "hazel", "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        answer(c, &att, "always").await;
        let sent = call_from(c, "hazel", "codex", "computer_act", send_key(&task, "act-1")).await;
        assert_eq!(sent["result"]["steps"][0]["outcome"], "done", "{sent}");

        rig.clock.fetch_add(3_600_000, Ordering::SeqCst);
        let ended = call_from(c, "hazel", "codex", "computer_act", send_key(&task, "act-2")).await;
        assert_eq!(code(&ended), "PERMISSION_DENIED", "the share ended: {ended}");
        assert_eq!(model(c).transport(c.now_ms())["peers"]["hazel"]["agent"], false, "hazel's agents are cut off");

        change(c, json!({ "op": "access_set", "subject": "hazel", "capability": "agents", "rule": "ask" })).await;
        let again = begin_on(c, "hazel", "codex").await;
        held_as(c, &call_from(c, "hazel", "codex", "computer_act", send_key(&again, "act-3")).await, "send");
    });
}

/// Failure case: with its computer's Agent Tasks grant removed, an agent
/// that Always Allow let send still runs tasks here, and the computer's
/// agent transport stays open.
#[test]
fn removing_a_computers_agent_tasks_ends_what_always_allow_gave_its_agents() {
    run(async {
        let (rig, _, _) = owned_rig(&[]);
        let c = &rig.controller;
        let task = begin_as(c, "codex").await;
        let att = held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        answer_from_vesper(c, &att, "always").await.unwrap();
        change(c, json!({ "op": "access_remove", "grant_id": "vesper:agents" })).await;
        let after = agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await;
        assert_eq!(code(&after), "PERMISSION_DENIED", "{after}");
        assert_eq!(model(c).transport(c.now_ms())["peers"]["vesper"], json!({ "agent": false, "operator": true }));
    });
}

/// An agent a person set to ask before it sends, from an older console
/// whose Access tab wrote every one of the agent's rules, on a computer
/// whose agents may send without asking.
/// Failure cases:
/// 1. The agent sends without asking: its computer's Allowed wins over its
///    own Ask First.
/// 2. Turning Ask before agents send, spend or delete off lets it send.
/// 3. The computer's other agents stop following the computer's Allowed.
#[test]
fn an_agent_an_older_console_set_to_ask_still_asks_where_its_computer_allows() {
    run(async {
        let (rig, switch, _) = owned_rig(&[("send", Rule::Allow)]);
        let c = &rig.controller;
        let whole = json!({ "observe": "allow", "change": "allow", "send": "ask", "spend": "ask", "destructive": "ask", "access": "ask" });
        change(c, json!({ "op": "access_set", "subject": "codex@vesper", "capability": "agents", "rule": "allow", "effects": whole })).await;
        let task = begin_as(c, "codex").await;
        held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        switch.set(false);
        held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["true"], "send")).await, "send");
        finish(c, "vesper", "codex", &task).await;

        switch.set(true);
        let theirs = begin_as(c, "claude").await;
        let sent = agent_call(c, "claude", "computer_act", send_key(&theirs, "act-1")).await;
        assert_eq!(sent["result"]["steps"][0]["outcome"], "done", "vesper's other agents send as it allows: {sent}");
    });
}

/// A friend's computer, shared with an invite, whose agents may work here.
/// Failure cases:
/// 1. With Ask before agents send, spend or delete off, the friend's agent
///    sends without asking.
/// 2. The owner cannot Always Allow that one agent, or doing so lets it
///    delete, or lets the friend's other agents send, without asking.
#[test]
fn a_friends_agents_keep_asking_with_the_switch_off() {
    run(async {
        let (rig, switch, _) = owned_rig(&[]);
        let c = &rig.controller;
        share(c, "hazel", ShareLevel::UseWithApproval, None).await;
        switch.set(false);
        let task = begin_on(c, "hazel", "codex").await;
        let att = held_as(c, &call_from(c, "hazel", "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        answer(c, &att, "always").await;
        let sent = call_from(c, "hazel", "codex", "computer_act", send_key(&task, "act-2")).await;
        assert_eq!(sent["result"]["steps"][0]["outcome"], "done", "{sent}");
        held_as(c, &call_from(c, "hazel", "codex", "computer_exec", command(&task, "exec-1", &["true"], "destructive")).await, "destructive");
        finish(c, "hazel", "codex", &task).await;

        let theirs = begin_on(c, "hazel", "claude").await;
        held_as(c, &call_from(c, "hazel", "claude", "computer_act", send_key(&theirs, "act-1")).await, "send");
    });
}

/// Another console denies a send while this one's Always Allow for it is
/// being applied.
/// Failure cases:
/// 1. Always Allow is refused as already answered, yet the agent's rule
///    now lets it send without asking.
/// 2. The step reads as denied while access says Always Allow took effect.
#[test]
fn always_allow_changes_access_only_with_its_answer() {
    run(async {
        let (rig, _, helper) = owned_rig(&[]);
        let c = &rig.controller;
        // Transport cleanup fails for now, so the next attempt, a minute on,
        // is the one Always Allow makes.
        helper.fail.set(true);
        let task = begin_as(c, "codex").await;
        let att = held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        rig.clock.fetch_add(61_000, Ordering::SeqCst);
        helper.fail.set(false);
        let theirs: Rc<RefCell<Option<crate::error::Result<Value>>>> = Rc::default();
        let (answered, controller, their_att) = (theirs.clone(), rig.controller.clone(), att.clone());
        *helper.during.borrow_mut() = Some(Box::pin(async move {
            *answered.borrow_mut() = Some(answer_from_vesper(&controller, &their_att, "deny").await);
        }));

        let always = answer_from_vesper(c, &att, "always").await;
        assert!(theirs.borrow().is_some(), "the other console answered while Always Allow was applied");
        let approved = c.journal.get_attention(&att).unwrap().unwrap().answer.as_deref() == Some("approve");
        let allows = own_rules(c, "codex@vesper")["send"] == "allow";
        assert_eq!((always.is_ok(), allows), (approved, approved), "one answer counts, and access agrees with it: {always:?}");
    });
}

/// vesper's agents are set to ask before they send (vesper's row in
/// Access). Any agent on vesper can call itself codex, so codex@vesper's
/// own Allow must not let it, or anything using its name, skip that.
/// Failure cases:
/// 1. After Always Allow for codex@vesper, it sends without asking once
///    vesper is set to ask.
/// 2. Always Allow, or an agent's request to stop asking, offers to let one
///    agent name send where its computer asks; or a refused Always Allow
///    leaves the step unanswerable.
#[test]
fn an_agents_own_allow_does_not_outrank_its_computers_ask_first() {
    run(async {
        let (rig, _, _) = owned_rig(&[]);
        let c = &rig.controller;
        let task = begin_as(c, "codex").await;
        let att = held_as(c, &agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await, "send");
        answer_from_vesper(c, &att, "always").await.unwrap();
        let ran = agent_call(c, "codex", "computer_act", send_key(&task, "act-1")).await;
        assert_eq!(ran["result"]["steps"][0]["outcome"], "done", "{ran}");

        change(c, json!({ "op": "access_set", "subject": "vesper", "capability": "agents", "rule": "allow", "effects": { "send": "ask" } })).await;
        let att = held_as(c, &agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["true"], "send")).await, "send");
        let refused = answer_from_vesper(c, &att, "always").await.unwrap_err();
        assert!(refused.code == "INVALID_ARGUMENT" && refused.message.contains("vesper") && refused.message.contains("codex"), "{refused:?}");
        assert_eq!(c.journal.get_attention(&att).unwrap().unwrap().state, "open", "Approve or Deny still answers it");
        answer_from_vesper(c, &att, "approve").await.unwrap();
        let approved = agent_call(c, "codex", "computer_exec", command(&task, "exec-1", &["true"], "send")).await;
        assert_eq!(approved["status"], "ok", "{approved}");
        finish(c, "vesper", "codex", &task).await;

        let theirs = begin_as(c, "claude").await;
        agent_call(c, "claude", "computer_checkpoint", json!({ "task_ref": theirs, "stop_asking": true })).await;
        assert_eq!(stop_asking_requests(c).await, ["claude@vesper asks to stop asking you before it spends or deletes on Tulip1."]);
    });
}
