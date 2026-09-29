//! An agent's question (`computer_checkpoint` with `ask`) as the console lists
//! and answers it. Failure cases:
//! 1. A question stays open after its task's control ended (the agent went
//!    away, control expired), so every console counts it as waiting forever
//!    while no agent can read an answer.
//! 2. A question an earlier build left open on an ended task survives a
//!    restart.
//! 3. That restart sweep also expires what belongs to no task (an access
//!    request), which a person can still answer.
//! 4. The console cannot answer a question with one of its options, takes an
//!    answer that is not one of them, or cannot dismiss it; the waiting agent
//!    does not see the answer, or does not learn it was dismissed.
//! 5. A dismiss or a free answer settles an approval, which only Approve or
//!    Deny may settle.

use super::*;
use crate::store::NewAttention;

const OPTIONS: [&str; 2] = ["Approve file manager", "Use terminal instead"];

/// The agent asks on `task`; returns the question's att_ref.
async fn ask(c: &Controller, task: &str, options: &[&str]) -> String {
    let asked = call(c, "computer_checkpoint", json!({ "task_ref": task, "ask": { "question": "May I use the file manager for this task?", "options": options } })).await;
    assert_eq!(asked["status"], "pending", "{asked}");
    asked["result"]["attention"].as_str().unwrap().to_string()
}

/// What the console lists as waiting for a person.
async fn waiting(c: &Controller) -> Vec<String> {
    let listed = c.operator_call("vesper", operator_action(c, "attention")).await.unwrap();
    listed["items"].as_array().unwrap().iter().map(|i| i["att_ref"].as_str().unwrap().to_string()).collect()
}

/// The console's answer to `att`, with `fields` (`answer` or `dismiss`).
async fn answer_with(c: &Controller, att: &str, fields: Value) -> crate::error::Result<Value> {
    let mut action = operator_action(c, "answer_attention");
    action["att_ref"] = json!(att);
    for (key, value) in fields.as_object().unwrap() {
        action[key] = value.clone();
    }
    c.operator_call("vesper", action).await
}

async fn wait_for(c: &Controller, task: &str, att: &str) -> Value {
    let waited = call(c, "computer_wait", json!({ "task_ref": task, "for": { "attention": att }, "deadline_ms": 1000 })).await;
    assert_eq!(waited["status"], "ok", "{waited}");
    waited["result"].clone()
}

#[test]
fn a_question_ends_with_its_tasks_control_and_an_older_one_at_the_next_start() {
    let dir = std::env::temp_dir().join(id("ibara-controller-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = Arc::new(AtomicI64::new(1_790_000_000_000));
    let desktop = FakeDesktop::new();
    let controller = open(&dir, &clock, &desktop, false);
    let (att, access) = run(async {
        let c = &controller;
        let task = begin(c).await;
        let att = ask(c, &task, &OPTIONS).await;
        assert_eq!(waiting(c).await, [att.clone()]);
        // The agent stops calling (its process was killed): control ends after five idle minutes.
        clock.fetch_add(IDLE_EXPIRY_MS + 1, Ordering::SeqCst);
        let ended = call(c, "computer_observe", json!({ "task_ref": task })).await;
        assert_eq!(code(&ended), "LEASE_EXPIRED", "{ended}");
        assert_eq!(waiting(c).await, Vec::<String>::new(), "no agent is left to read an answer");
        assert_eq!(c.journal.get_attention(&att).unwrap().unwrap().state, "expired");
        // What an earlier build left: the same question still open on the ended task, and an
        // access request, which belongs to no task and waits for a person.
        c.journal.db().execute("UPDATE attention_items SET state = 'open', answered_at = NULL WHERE att_ref = ?", [&att]).unwrap();
        let options = ["approve".to_string(), "deny".to_string()];
        let access = c
            .journal
            .raise_attention(NewAttention {
                task_ref: "access:vesper",
                principal: "vesper",
                kind: "approval",
                operation_ref: None,
                generation: None,
                question: "vesper asks to watch the screen.",
                details: None,
                options: &options,
                now_iso: &c.now_iso(),
            })
            .unwrap();
        (att, access.att_ref)
    });
    drop(controller);
    run(async {
        let restarted = open(&dir, &clock, &desktop, false);
        assert_eq!(restarted.journal.get_attention(&att).unwrap().unwrap().state, "expired", "the ended task's question expired at the start");
        assert_eq!(waiting(&restarted).await, [access], "the access request still waits");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_console_answers_a_question_with_one_of_its_options_or_dismisses_it() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let att = ask(c, &task, &OPTIONS).await;
        let refused = answer_with(c, &att, json!({ "answer": "approve" })).await.unwrap_err();
        assert_eq!(refused.code, "INVALID_ARGUMENT", "not one of its options: {refused:?}");
        let answered = answer_with(c, &att, json!({ "answer": OPTIONS[1] })).await.unwrap();
        assert_eq!((answered["item"]["state"].as_str(), answered["item"]["answer"].as_str()), (Some("answered"), Some(OPTIONS[1])), "{answered}");
        assert_eq!(wait_for(c, &task, &att).await["answer"], OPTIONS[1]);

        // Without options, any short answer.
        let open = ask(c, &task, &[]).await;
        assert_eq!(answer_with(c, &open, json!({ "answer": "  " })).await.unwrap_err().code, "INVALID_ARGUMENT");
        answer_with(c, &open, json!({ "answer": "The one in Downloads" })).await.unwrap();
        assert_eq!(wait_for(c, &task, &open).await["answer"], "The one in Downloads");

        // Dismissed: it leaves the list, and the agent's wait ends with no answer.
        let dismissed = ask(c, &task, &OPTIONS).await;
        let reply = answer_with(c, &dismissed, json!({ "dismiss": true })).await.unwrap();
        assert_eq!(reply["item"]["state"], "expired", "{reply}");
        let waited = wait_for(c, &task, &dismissed).await;
        assert_eq!((&waited["met"], &waited["state"], &waited["answer"]), (&json!(true), &json!("expired"), &Value::Null), "{waited}");
        assert_eq!(waiting(c).await, Vec::<String>::new());
    });
}

#[test]
fn an_approval_is_settled_only_by_approve_or_deny() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let options = ["approve".to_string(), "deny".to_string()];
        let approval = c
            .journal
            .raise_attention(NewAttention {
                task_ref: "access:vesper",
                principal: "vesper",
                kind: "approval",
                operation_ref: None,
                generation: None,
                question: "vesper asks to watch the screen.",
                details: None,
                options: &options,
                now_iso: &c.now_iso(),
            })
            .unwrap();
        for fields in [json!({ "dismiss": true }), json!({ "answer": "Use terminal instead" })] {
            let refused = answer_with(c, &approval.att_ref, fields.clone()).await.unwrap_err();
            assert_eq!(refused.code, "INVALID_ARGUMENT", "{fields}: {refused:?}");
        }
        assert_eq!(waiting(c).await, [approval.att_ref.clone()]);
        let denied = answer_with(c, &approval.att_ref, json!({ "answer": "deny" })).await.unwrap();
        assert_eq!(denied["item"]["state"], "answered");
    });
}
