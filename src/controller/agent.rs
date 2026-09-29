//! The eleven contract-4 tools (docs/agent-tools.md).
//!
//! Every effect is written to the journal as an operation before it is
//! dispatched. An act's first step shares the call's operation (its
//! `request_id`); later steps are operations `<request_id>#<n>`, a form an
//! agent's own request ids cannot take. The call's whole result is stored on
//! the first operation, so a replay returns it without dispatching anything.

use super::budget;
use super::control::{Charge, Release};
use super::checks::{TaskPath, pause};
use super::ports::{Button, Cancel, Done, Effect, Image, PutBack, TypingCursor, Win, WinKey};
use super::approval::{self, Ask, Place};
use super::situation::{FrameSpec, FrameState, agent_label, app_class, app_id_for, dirty_title, surface_signature};
use super::{Controller, Rule, clip, squash};
use crate::contract::{
    self, ActInput, ActResult, Action, BeginInput, BeginResult, BrowserAction, BrowserActInput, Check, CheckBasis, CheckState,
    CheckStatus, CheckpointInput, Cleanup, ComputerId, ComputerSummary, DeliveryState, DeliveryStatus, EffectClass, EntryKind,
    Envelope, ExecInput, Expectation, FileEntry, FilesInput, FilesOp, FilesResult, FinishInput, FinishResult, Frame, LeftOpen,
    ObserveInput, ObserveResult, Outcome, ProceduresInput, ProceduresOp, RefStatus, Status, StatusInput, StatusResult, Step,
    StepOutcome, StepResult, Target, View, WaitFor, WaitInput, You, parse_input,
};
use crate::error::{IbaraError, Result, denied, invalid, unavailable};
use crate::ids::id;
use crate::mcp::CallOutcome;
use crate::storage::Context;
use crate::store::{
    LeaseRecord, NewAttention, NewEvent, OperationPatch, OperationRecord, RememberIntent, Remembered, TaskRecord, error_from_json,
};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

mod guard;
use guard::Guard;

/// What a tool produced before the envelope is assembled.
pub(crate) struct Reply {
    pub status: Status,
    pub result: Value,
    pub images: Vec<Image>,
    /// The task the situation line should describe.
    pub task_ref: Option<String>,
}

impl Reply {
    fn ok(result: impl serde::Serialize, task_ref: Option<&str>) -> Reply {
        Reply { status: Status::Ok, result: to_value(result), images: Vec::new(), task_ref: task_ref.map(str::to_string) }
    }
    fn with_status(status: Status, result: impl serde::Serialize, task_ref: &str) -> Reply {
        Reply { status, result: to_value(result), images: Vec::new(), task_ref: Some(task_ref.to_string()) }
    }
}

fn to_value(v: impl serde::Serialize) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

const APPROVED: &[&str] = &["approve", "approved", "yes", "allow", "ok"];

fn fail(code: &'static str, message: impl Into<String>, retry_safe: bool) -> IbaraError {
    IbaraError::new(code, message, retry_safe)
}

/// The call's arguments without `request_id`: the fingerprint source (`core.ts:2130`).
fn fingerprint_source(args: &Value) -> Value {
    let mut map = args.as_object().cloned().unwrap_or_default();
    map.remove("request_id");
    Value::Object(map)
}

fn class_name(class: EffectClass) -> &'static str {
    match class {
        EffectClass::Change => "change",
        EffectClass::Send => "send",
        EffectClass::Spend => "spend",
        EffectClass::Destructive => "destructive",
    }
}

/// The agent's declaration can only make a step stricter.
fn stricter(base: &'static str, declared: Option<EffectClass>) -> &'static str {
    match declared {
        Some(EffectClass::Change) | None => base,
        Some(other) => class_name(other),
    }
}

fn not_started(e: &IbaraError) -> bool {
    e.details.get("execution_not_started") == Some(&Value::Bool(true))
        || e.details.get("execution").and_then(Value::as_str) == Some("not_started")
}

/// Where typed text goes: after what the focused window holds, or over a
/// page field's text, which ibara clicked and selected first.
#[derive(Clone, Copy, PartialEq)]
enum TypedInto {
    Window,
    Field,
}

/// The cause of a per-call refusal that says ibara sent nothing, true only
/// of that call, for a step that had already sent part of its input.
fn stop_cause(e: &IbaraError) -> Option<&'static str> {
    match e.details.get("reason").and_then(Value::as_str)? {
        "person_active" => Some("A person took over the mouse."),
        "interrupted" => Some("A person used the mouse or keyboard."),
        "agent_cursor_hidden" => Some("The agent's cursor was no longer on the screen."),
        "agent_cursor_unavailable" => Some("ibara could not show the agent's cursor."),
        _ => None,
    }
}

/// Typing stopped by `e`: the step says how many of its `total` characters
/// went through for sure (`before` this call, plus the desktop's
/// `typed_chars`) and how many more may have (`unsure_chars`), never that
/// nothing was sent once part was, so an agent does not type them twice.
/// The text itself is never named. Into a window with nothing typed,
/// `e` stands.
fn typing_stopped(mut e: IbaraError, before: usize, total: usize, into: TypedInto) -> IbaraError {
    let count = |key: &str| e.details.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
    let (typed, unsure) = (before + count("typed_chars"), count("unsure_chars"));
    if typed + unsure == 0 && into == TypedInto::Window {
        return e;
    }
    let cause = stop_cause(&e).map_or_else(|| format!("{}.", e.message.trim_end_matches('.')), str::to_string);
    let rest = total.saturating_sub(typed + unsure);
    let next = match (into, unsure) {
        (TypedInto::Field, _) => "Typing the whole text again replaces what the field holds.".to_string(),
        (TypedInto::Window, 0) => format!("The other {rest} were not typed: type only those."),
        (TypedInto::Window, _) => "Check what arrived before typing again.".to_string(),
    };
    e.message = match (typed, unsure) {
        (0, 0) => format!("The click went through, but nothing was typed. {cause} {next}"),
        (0, _) => format!("The first {unsure} of the {total} characters may be partly typed. {cause} {next}"),
        (_, 0) => format!("ibara typed the first {typed} of the {total} characters, then stopped. {cause} {next}"),
        _ => format!("ibara typed the first {typed} of the {total} characters, and the next {unsure} may be partly typed. {cause} {next}"),
    };
    e.with("typed_chars", typed).with("execution_not_started", false)
}

/// A step that had sent part of its input before a per-call refusal
/// stopped it: that refusal's "sent nothing" was only its own call's.
/// Typing says more ([`typing_stopped`]).
fn partly_sent(mut e: IbaraError) -> IbaraError {
    if let Some(cause) = stop_cause(&e).filter(|_| !e.details.contains_key("typed_chars")) {
        e.message = format!("Part of this step went through before it stopped. {cause} Check what it did before trying again.");
    }
    e
}

fn step_result(index: usize, outcome: StepOutcome, effect: impl Into<String>, op_ref: Option<&str>) -> StepResult {
    StepResult { index, outcome, effect: clip(&effect.into(), 300), op_ref: op_ref.map(str::to_string) }
}

/// What the agent does with a `tool` request held for a person's approval
/// `att`: wait for the answer, then send the same request again. Sent again
/// once approved, the held request runs once (`replay_act`, `resume_single`,
/// the access gate); under a new request_id it is refused while the approval
/// is open and asked again after (`approval_guard`). Without a task there is
/// nothing to wait on, so a begin is sent again until it goes ahead.
fn approval_next(tool: &str, task_ref: Option<&str>, att: &str) -> String {
    const STAY: &str = "Don't end your turn or ask your user; the person answers in ibara.";
    let Some(task_ref) = task_ref else {
        return format!(
            "A person must approve this task ({att}) before it begins. Wait a few seconds, then send this same {tool} request again (same request_id), until it is no longer pending: it begins once approved. {STAY}"
        );
    };
    let again = if tool == "computer_observe" {
        "call computer_observe again with the same arguments: it goes ahead once approved"
    } else {
        "send this same request again (same request_id): it runs once if approved"
    };
    format!(
        "A person must approve this step ({att}) before it runs. Call computer_wait({{task_ref: \"{task_ref}\", for: {{attention: \"{att}\"}}, deadline_ms: 50000}}) to wait for their answer, and again while it is still open. Then {again}. {STAY}"
    )
}

/// A request the access rules hold for a person's approval, with what the
/// agent does next.
fn access_pending(tool: &str, args: &Value, mut pending: Value) -> Reply {
    let task_ref = args["task_ref"].as_str();
    if let Some(att) = pending["attention"].as_str() {
        pending["next"] = json!(approval_next(tool, task_ref, att));
    }
    Reply { status: Status::Pending, result: pending, images: vec![], task_ref: task_ref.map(str::to_string) }
}

/// How long a browser step waits for the extension to connect.
const BROWSER_CONNECT: Duration = Duration::from_secs(10);
/// How long a pasted piece gets to show in the page's field.
const PASTE_WITHIN: Duration = Duration::from_secs(2);
/// How long `finish` waits for the windows it asked to close to go, and how
/// often it looks.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
const CLOSE_POLL: Duration = Duration::from_millis(100);
/// How long a launch waits for its app's first window, and how often it
/// looks: the program runs when the launch returns, and its window maps
/// later, seconds later on a slow computer.
const LAUNCH_WINDOW_WAIT: Duration = Duration::from_secs(10);
const LAUNCH_WINDOW_POLL: Duration = Duration::from_millis(100);

/// What one step will do, resolved against the latest frame.
enum Planned {
    Desktop(Box<Effect>),
    Browser(Box<BrowserStep>),
    Observe,
}

/// A `browser_act` step: the extension reads the page (where the element
/// is, whether it is covered, which element a click reached); Cua does every
/// click and keypress through the real pointer and keyboard.
struct BrowserStep {
    /// The focused browser window when the step was resolved.
    window: WinKey,
    /// The page element: `{tabId, documentId, capture, token}`.
    target: Option<Value>,
    op: BrowserOp,
}

enum BrowserOp {
    Navigate(String),
    Click,
    /// Replace the target's text, or type at the focus without a target.
    Type(String),
    /// An option's value or label.
    Select(String),
    Scroll { dx: i32, dy: i32 },
    Key(String),
}

struct Resolved {
    plan: Planned,
    describe: String,
    /// What the step does, in a person's words, as the end of "… wants to".
    said: String,
    class: &'static str,
    /// Route recorded in app notes (`route:<kind>`).
    route: &'static str,
}

/// A step of `computer_act` or the one step of `browser_act`.
#[derive(Clone)]
enum StepSpec {
    Desktop(Step),
    Browser { action: BrowserAction, expect: Option<Expectation>, effect: Option<EffectClass> },
}

impl StepSpec {
    fn expect(&self) -> Option<&Expectation> {
        match self {
            StepSpec::Desktop(s) => s.expect.as_ref(),
            StepSpec::Browser { expect, .. } => expect.as_ref(),
        }
    }
}

/// The fixed parts of one call.
struct CallCtx<'a> {
    principal: &'a str,
    task_ref: &'a str,
    request_id: &'a str,
    tool: &'a str,
    lease: &'a LeaseRecord,
    /// Fires when the caller goes away; the call stops at its next safe point.
    gone: &'a Cancel,
}

impl Controller {
    /// One tool call: dispatch, then wrap the outcome in the envelope with the
    /// situation line and `since`, within the response budget.
    ///
    /// `cancel` fires when the caller goes away. The call then stops at its
    /// next safe point (before a step, or in a wait); a dispatched effect always
    /// runs to completion and the outcome is stored as usual. Keep polling the
    /// future to completion rather than dropping it.
    pub async fn call(&self, principal: &str, connection_id: &str, client_name: &str, tool: &str, args: Value, cancel: Cancel) -> CallOutcome {
        let agent = agent_label(client_name, principal);
        let outcome = self.dispatch(principal, connection_id, client_name, &agent, tool, &args, &cancel).await;
        let hint = match &outcome {
            Ok(reply) => reply.task_ref.clone(),
            Err(_) => None,
        }
        .or_else(|| args.get("task_ref").and_then(Value::as_str).map(str::to_string));
        let lease = self.journal.get_active_lease().ok().flatten();
        let holds = lease.as_ref().is_some_and(|l| l.principal == principal && l.connection_id == connection_id);
        let task = hint
            .and_then(|t| self.journal.readable_task(principal, &t).ok().flatten())
            .or_else(|| if holds { lease.as_ref().and_then(|l| self.journal.get_task(&l.task_ref).ok().flatten()) } else { None });
        let situation = self.situation_line(principal, connection_id, &agent, task.as_ref());
        let since = self.take_since(connection_id, task.as_ref().map(|t| t.task_ref.as_str()), holds);
        let (mut envelope, images) = match outcome {
            Ok(reply) => (
                Envelope { situation, status: reply.status, since, result: reply.result, error: None },
                reply.images,
            ),
            Err(e) => (Envelope::error(situation, since, &e), Vec::new()),
        };
        let mut images: Vec<crate::mcp::Image> = images
            .into_iter()
            .map(|i| crate::mcp::Image { mime: i.mime, base64: base64::engine::general_purpose::STANDARD.encode(&i.bytes) })
            .collect();
        budget::fit(&mut envelope, &mut images, budget::MAX_RESULT_BYTES);
        CallOutcome { envelope: envelope.to_value(), images }
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch(&self, principal: &str, connection_id: &str, client_name: &str, agent: &str, tool: &str, args: &Value, gone: &Cancel) -> Result<Reply> {
        self.ensure_open()?;
        self.assert_identity(principal, connection_id)?;
        let head = self.events.borrow().head();
        self.sessions.borrow_mut().touch(connection_id, client_name, head, self.now_ms());
        self.touch_rate(connection_id)?;
        self.reconcile().await?;
        self.touch_connection(principal, connection_id)?;
        if tool != "computer_status" {
            self.require_access(agent, "agents")?;
            if let Some(task_ref)=args["task_ref"].as_str() {
                if self.task_subject(task_ref,principal)!=agent { return Err(denied("This task belongs to another agent identity.")); }
                // The agent back over a new connection carries on at once.
                self.resume_lease(principal, connection_id, agent, task_ref)?;
            }
        }
        if tool=="computer_observe" || tool=="computer_files" && matches!(args["op"].as_str(),Some("read"|"list")) {
            if let Some(a)=crate::access::Access::load(&self.journal)? {
                if let Some(pending)=self.access_gate_rule(agent,"observe",args,a.effect(agent,"observe",self.now_ms(),(self.ask_first)()))? {
                    return Ok(access_pending(tool, args, pending));
                }
            }
        }
        if tool == "computer_begin" {
            let _: BeginInput = parse_input(tool,args)?;
            if let Some(pending)=self.access_gate(agent,"agents",args)? {
                return Ok(access_pending(tool, args, pending));
            }
        }
        match tool {
            "computer_status" => self.status_tool(principal, connection_id, agent, parse_input(tool, args)?).await,
            "computer_begin" => self.begin(principal, connection_id, agent, args).await,
            "computer_observe" => self.observe(principal, connection_id, parse_input(tool, args)?).await,
            "computer_act" => {
                let input: ActInput = parse_input(tool, args)?;
                let (task_ref, request_id) = (input.task_ref.to_string(), input.request_id.to_string());
                let steps = input.into_steps().into_iter().map(StepSpec::Desktop).collect();
                self.require_live_lease(principal, connection_id, &task_ref)?;
                self.once(principal, &task_ref, &request_id, args, self.act(principal, connection_id, tool, &task_ref, &request_id, args, steps, gone)).await
            }
            "browser_act" => {
                let input: BrowserActInput = parse_input(tool, args)?;
                let (task_ref, request_id) = (input.task_ref.to_string(), input.request_id.to_string());
                let steps = vec![StepSpec::Browser { action: input.action, expect: input.expect, effect: input.effect }];
                self.require_live_lease(principal, connection_id, &task_ref)?;
                self.once(principal, &task_ref, &request_id, args, self.act(principal, connection_id, tool, &task_ref, &request_id, args, steps, gone)).await
            }
            "computer_exec" => {
                let input: ExecInput = parse_input(tool, args)?;
                let (task_ref, request_id) = (input.task_ref.to_string(), input.request_id.to_string());
                self.require_live_lease(principal, connection_id, &task_ref)?;
                self.once(principal, &task_ref, &request_id, args, self.exec(principal, connection_id, args, input)).await
            }
            "computer_wait" => self.wait(principal, connection_id, parse_input(tool, args)?, gone).await,
            "computer_files" => {
                let input: FilesInput = parse_input(tool, args)?;
                let (task_ref, request_id) = (input.task_ref.to_string(), input.request_id.to_string());
                self.require_live_lease(principal, connection_id, &task_ref)?;
                self.once(principal, &task_ref, &request_id, args, self.files(principal, connection_id, args, input)).await
            }
            "computer_checkpoint" => self.checkpoint(principal, connection_id, parse_input(tool, args)?).await,
            "computer_finish" => {
                let input: FinishInput = parse_input(tool, args)?;
                let (task_ref, request_id) = (input.task_ref.to_string(), input.request_id.to_string());
                self.finish_lease(principal, connection_id, &task_ref)?;
                self.once(principal, &task_ref, &request_id, args, self.finish(principal, connection_id, agent, args, input)).await
            }
            "computer_procedures" => self.procedures_tool(principal, parse_input(tool, args)?),
            _ => Err(invalid(format!("tool: '{}' is not one of the eleven computer tools", clip(tool, 60)))),
        }
    }

    /// Run a journalled call in the effect queue, once at a time per request.
    /// A repeat while the same call is still queued or running answers
    /// `pending` with its operation at once instead of waiting behind it;
    /// different arguments are `REQUEST_CONFLICT`.
    async fn once(&self, principal: &str, task_ref: &str, request_id: &str, args: &Value, work: impl Future<Output = Result<Reply>>) -> Result<Reply> {
        let key = (principal.to_string(), task_ref.to_string(), request_id.to_string());
        let source = fingerprint_source(args);
        let running = self.in_flight.borrow().get(&key).map(|existing| *existing == source);
        match running {
            Some(false) => return Err(fail("REQUEST_CONFLICT", "The same request ID was reused with different arguments.", false)),
            Some(true) => {
                let op_ref = self.journal.get_mutation_operation(principal, task_ref, request_id)?.map(|o| o.operation_ref);
                let next = match &op_ref {
                    Some(op) => format!("computer_wait({{for: {{op: \"{op}\"}}}}), then repeat this request_id for its result"),
                    None => "the call is queued; repeat this request_id shortly for its result".to_string(),
                };
                return Ok(Reply::with_status(Status::Pending, json!({ "op_ref": op_ref, "state": "running", "next": next }), task_ref));
            }
            None => {}
        }
        self.in_flight.borrow_mut().insert(key.clone(), source);
        let _flight = InFlight { map: &self.in_flight, key };
        self.enqueue(work).await
    }

    // ---- receipts and operations ----------------------------------------------------

    /// `receipt()` (`core.ts:1180-1201`) for an operation that has not started.
    fn receipt(&self, request_id: &str, op_ref: &str, task_ref: &str, summary: &str) -> Map<String, Value> {
        let mut r = Map::new();
        r.insert("kind".into(), json!("receipt"));
        r.insert("request_id".into(), json!(request_id));
        r.insert("operation_ref".into(), json!(op_ref));
        r.insert("task_ref".into(), json!(task_ref));
        r.insert("epoch".into(), json!(self.epoch));
        r.insert("execution".into(), json!("not_started"));
        r.insert("verification".into(), json!("not_requested"));
        r.insert("effect".into(), json!("none"));
        r.insert("evidence_refs".into(), json!([]));
        r.insert("summary".into(), json!(clip(summary, 2000)));
        r
    }

    fn save_receipt(&self, op_ref: &str, receipt: &Map<String, Value>, dispatched: Option<bool>) -> Result<OperationRecord> {
        let mut patch = OperationPatch::at(self.now_iso());
        patch.receipt = Some(Value::Object(receipt.clone()));
        patch.dispatched = dispatched;
        self.journal.update_operation(op_ref, patch)
    }

    fn remember(&self, ctx: &CallCtx<'_>, request_id: &str, source: &Value, class: &str) -> Result<Remembered> {
        self.remember_for(ctx.principal, ctx.task_ref, ctx.tool, request_id, source, class)
    }

    fn remember_for(&self, principal: &str, task_ref: &str, tool: &str, request_id: &str, source: &Value, class: &str) -> Result<Remembered> {
        let provisional = Value::Object(self.receipt(request_id, "pending", task_ref, &format!("{tool} intent recorded.")));
        let now = self.now_iso();
        self.journal.remember_intent(RememberIntent {
            principal,
            request_id,
            task_ref: Some(task_ref),
            tool,
            args_fingerprint_source: source,
            receipt: &provisional,
            now_iso: &now,
            recovery_operation_ref: None,
            effect_class: class,
        })
    }

    fn timeline(&self, kind: &str, task_ref: Option<&str>, actor: &str, summary: &str, data: Value) {
        let at = self.now_iso();
        if let Err(e) = self.journal.append_event(NewEvent { at: &at, kind, task_ref, actor, summary, data: &data }) {
            super::log_event("timeline_write_failed", &e.to_string());
        }
    }

    /// The stored call of a replayed operation, as a reply or its error.
    fn stored_call(&self, op: &OperationRecord, task_ref: &str) -> Option<Result<Reply>> {
        let call = op.receipt.get("call")?;
        if let Some(error) = call.get("error").filter(|e| !e.is_null()) {
            return Some(Err(error_from_json(error).unwrap_or_else(|| crate::error::internal("stored error unreadable"))));
        }
        let status = match call.get("status").and_then(Value::as_str) {
            Some("pending") => Status::Pending,
            _ => Status::Ok,
        };
        Some(Ok(Reply { status, result: call.get("result").cloned().unwrap_or(Value::Null), images: Vec::new(), task_ref: Some(task_ref.to_string()) }))
    }

    fn store_call(&self, op_ref: &str, receipt: &mut Map<String, Value>, outcome: &Result<Reply>, held: Option<(usize, &str)>) -> Result<()> {
        let call = match outcome {
            Ok(reply) => {
                let mut call = json!({ "status": reply.status.as_str(), "result": reply.result });
                if let Some((index, att)) = held {
                    call["held"] = json!({ "index": index, "attention": att });
                }
                call
            }
            Err(e) => json!({ "status": "error", "error": e.to_json() }),
        };
        receipt.insert("call".into(), call);
        self.save_receipt(op_ref, receipt, None).map(|_| ())
    }

    /// Raise an approval item for a held operation: `summary` is what a person
    /// reads, `details` the structured request behind it.
    fn hold(&self, ctx: &CallCtx<'_>, op_ref: &str, describe: &str, summary: &str, details: &Value) -> Result<String> {
        let now = self.now_iso();
        if details.to_string().len() > 12000 { return Err(invalid("This approval is too large to review. Split the action into smaller steps.")); }
        let options = vec!["approve".to_string(), "deny".to_string()];
        let item = self.journal.raise_attention(NewAttention {
            task_ref: ctx.task_ref,
            principal: ctx.principal,
            kind: "approval",
            operation_ref: Some(op_ref),
            generation: Some(&ctx.lease.generation),
            question: summary,
            details: Some(details),
            options: &options,
            now_iso: &now,
        })?;
        self.push_event(Some(ctx.task_ref), &format!("{} asks a person to approve: {}", item.att_ref, squash(describe, 80)));
        Ok(item.att_ref)
    }

    /// A held request's words and details: who asks (the task's agent), what,
    /// where and why, and the task it is for.
    fn approval_words(&self, ctx: &CallCtx<'_>, ask: &Ask, request: Value) -> (String, Value) {
        let agent = self.task_subject(ctx.task_ref, ctx.principal);
        let goal = self.journal.get_task(ctx.task_ref).ok().flatten().map(|t| t.goal);
        let summary = approval::summary(&agent, goal.as_deref(), ask);
        let mut details = json!({
            "agent": agent,
            "task": { "task_ref": ctx.task_ref, "goal": goal },
            "tool": ctx.tool,
            "effect": ask.class,
            "request": request,
        });
        if let Some(place) = &ask.place {
            details["where"] = place.details();
        }
        if ask.changed {
            details["target_changed"] = json!(true);
        }
        (summary, details)
    }

    /// Where a held step acts, as a person names it: the app, and the page it
    /// shows (its host and path, read from the browser's focused tab) or the
    /// window's title. None for steps whose words already name what they act on.
    async fn place_of(&self, resolved: &Resolved, windows: &[Win]) -> Option<Place> {
        let (key, on_page) = match &resolved.plan {
            Planned::Browser(step) => (step.window.clone(), !matches!(step.op, BrowserOp::Navigate(_))),
            Planned::Desktop(effect) => match effect.as_ref() {
                Effect::ClickElement { surface, .. } | Effect::Type { surface, .. } | Effect::Key { surface, .. } => (surface.clone(), true),
                _ => return None,
            },
            Planned::Observe => return None,
        };
        if !on_page {
            return Some(Place::of_window(&key.class, "", None));
        }
        let win = windows.iter().find(|w| w.address == key.address && w.pid == key.pid);
        let url = match win {
            Some(w) if w.focused && is_browser(&w.class) && self.desktop.browser_connected() => {
                self.desktop.tabs().await.ok().and_then(|tabs| tabs.into_iter().find(|t| t.focused)).map(|t| t.url)
            }
            _ => None,
        };
        Some(Place::of_window(&key.class, win.map_or("", |w| w.title.as_str()), url.as_deref()))
    }

    /// The decision on a held operation: `Some(true)` approved under this
    /// control, `Some(false)` declined or no longer valid, `None` still open.
    fn approval(&self, att_ref: &str, lease: &LeaseRecord) -> Result<(Option<bool>, String)> {
        let Some(item) = self.journal.get_attention(att_ref)? else {
            return Ok((Some(false), "the approval request is gone".into()));
        };
        Ok(match item.state.as_str() {
            "open" => (None, format!("waiting for a person to answer {att_ref}")),
            "answered" => {
                let yes = item.answer.as_deref().is_some_and(|a| APPROVED.contains(&a.trim().to_lowercase().as_str()));
                if !yes {
                    (Some(false), "a person declined this step".into())
                } else if item.generation.as_deref() != Some(lease.generation.as_str()) {
                    (Some(false), "the approval ended when control changed hands".into())
                } else {
                    (Some(true), "approved by a person".into())
                }
            }
            _ => (Some(false), "the approval request expired".into()),
        })
    }

    // ---- computer_status ------------------------------------------------------------

    fn holder_text(&self, principal: &str, connection_id: &str, agent: &str) -> Option<String> {
        if self.viewer_state.borrow().owner.is_some() {
            return Some("a person".into());
        }
        match self.journal.get_active_lease().ok().flatten() {
            Some(l) if self.holds_lease(&l, principal, connection_id, agent) => Some(format!("you ({agent})")),
            Some(_) => Some("another agent".into()),
            None => {
                let control = self.journal.get_control().ok()?;
                (control.human_control || control.paused).then(|| "a person (paused)".to_string())
            }
        }
    }

    /// One line: what is not available, then the browser in plain words, read
    /// live (it opens and closes between capability refreshes).
    fn capability_line(&self) -> String {
        let caps = self.capabilities.borrow();
        let browser = self.browser_row();
        let browser_status = browser["status"].as_str().unwrap_or("");
        let mut not_ok: Vec<String> = caps
            .iter()
            .filter(|c| c.get("name").and_then(Value::as_str) != Some("browser_semantics"))
            .filter_map(|c| {
                let status = c.get("status").and_then(Value::as_str).unwrap_or("not_tested");
                (status != "available" && status != "not_tested").then(|| format!("{}: {status}", c.get("name").and_then(Value::as_str).unwrap_or("?")))
            })
            .take(4)
            .collect();
        if browser_status == "unavailable" {
            not_ok.push("browser page reader (not installed)".into());
        }
        let mut line = if caps.is_empty() {
            "capabilities not tested yet".to_string()
        } else if not_ok.is_empty() {
            "all available".to_string()
        } else {
            format!("all available except {}", not_ok.join(", "))
        };
        if browser_status == "not_open" {
            line.push_str("; no browser open yet (its page reader connects when one opens)");
        }
        if let Some(p) = self.desktop.memory_pressure() {
            line.push_str(&format!("; {p}"));
        }
        line
    }

    fn computer_summary(&self, principal: &str, connection_id: &str, agent: &str) -> Result<ComputerSummary> {
        Ok(ComputerSummary {
            id: self.computer_id.clone(),
            name: self.computer_name(),
            state: self.availability()?.to_string(),
            user: self.computer.user.clone(),
            holder: self.holder_text(principal, connection_id, agent),
            capabilities: self.capability_line(),
        })
    }

    fn ended(&self, reference: &str, kind: &str, summary: &str) -> RefStatus {
        RefStatus {
            reference: reference.to_string(),
            kind: kind.to_string(),
            state: "ended".into(),
            summary: Some(summary.to_string()),
            parent: Some(self.computer_id.clone()),
            children: Vec::new(),
            next: vec![format!("computer_status() for this computer, or computer_begin({{computer: \"{}\", goal, request_id}})", self.computer_name())],
        }
    }

    async fn status_tool(&self, principal: &str, connection_id: &str, agent: &str, input: StatusInput) -> Result<Reply> {
        if self.capabilities.borrow().is_empty() {
            self.refresh_capabilities().await?;
        }
        let Some(reference) = input.reference.map(|r| r.into_string()) else {
            let summary = self.computer_summary(principal, connection_id, agent)?;
            let mut reply=Reply::ok(StatusResult::Fleet { computers: vec![summary] }, None);
            if let Some(a)=crate::access::Access::load(&self.journal)? { reply.result["access"]=a.own_row(agent,self.now_ms(),(self.ask_first)()); }
            return Ok(reply);
        };
        if let Some(tool) = reference.strip_prefix("help:") {
            let help = contract::help(tool).ok_or_else(|| invalid(format!("ref: no tool named '{}'; see the tool list", clip(tool, 60))))?;
            return Ok(Reply::ok(StatusResult::Help { tool: tool.to_string(), help }, None));
        }
        let (status, task_ref) = if reference == self.computer_id || reference.starts_with("cmp_") {
            if reference != self.computer_id {
                (self.ended(&reference, "computer", "Not a computer this connection reaches."), None)
            } else {
                let s = self.computer_summary(principal, connection_id, agent)?;
                let children = self.journal.get_active_lease()?.filter(|l| l.principal == principal).map(|l| vec![l.task_ref]).unwrap_or_default();
                (
                    RefStatus {
                        reference: reference.clone(),
                        kind: "computer".into(),
                        state: s.state.clone(),
                        summary: Some(format!("{} · {} · {}", s.name, s.holder.unwrap_or_else(|| "nobody controls".into()), s.capabilities)),
                        parent: None,
                        children,
                        next: vec![format!("computer_begin({{computer: \"{}\", goal, request_id}})", self.computer_name())],
                    },
                    None,
                )
            }
        } else if reference.starts_with("task_") {
            self.task_status(principal, connection_id, agent, &reference).await?
        } else if reference.starts_with("op_") {
            self.op_status(principal, &reference)?
        } else if reference.starts_with("att_") {
            self.attention_status(principal, &reference)?
        } else if reference.starts_with("art_") || reference.starts_with("artifact_") {
            self.artifact_status(principal, &reference)?
        } else if reference.starts_with("frame_") {
            self.frame_status(principal, &reference)?
        } else if reference.starts_with("checkpoint_") {
            self.note_status(principal, &reference)?
        } else {
            return Err(invalid(format!(
                "ref: '{}' is not a reference ibara gave out (cmp_, task_, op_, att_, art_, frame_, checkpoint_ or help:<tool>)",
                clip(&reference, 60)
            )));
        };
        Ok(Reply::ok(StatusResult::Ref(status), task_ref.as_deref()))
    }

    async fn task_status(&self, principal: &str, connection_id: &str, agent: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let Ok(mut task) = self.require_readable_task(principal, reference) else {
            return Ok((self.ended(reference, "task", "No such task for you."), None));
        };
        let statuses = self.evaluate_task_checks(&mut task, &[], false).await?;
        let live = self.journal.get_active_lease()?.is_some_and(|l| l.task_ref == task.task_ref);
        let mine = self.journal.get_active_lease()?.is_some_and(|l| l.task_ref == task.task_ref && self.holds_lease(&l, principal, connection_id, agent));
        let unknown = self.unknown_operations(&task.task_ref);
        let attention = self.journal.list_attention(Some("open"), Some(&task.task_ref), 10)?;
        let recent = self.journal.list_operations_for_task(&task.task_ref, 10, None)?;
        let mut children: Vec<String> = attention.iter().map(|a| a.att_ref.clone()).collect();
        children.extend(recent.items.iter().map(|o| o.operation_ref.clone()));
        if let Some(frame) = self.frames.borrow().latest(&task.task_ref) {
            children.push(frame.frame.frame_ref.clone());
        }
        // The last note is how a task continues after its context is lost.
        let note = task.last_checkpoint_ref.as_deref().map(|r| self.journal.get_checkpoint(r)).transpose()?.flatten();
        if let Some(r) = &task.last_checkpoint_ref {
            children.push(r.clone());
        }
        let (met, total) = (statuses.iter().filter(|s| s.state == CheckState::Met).count(), statuses.len());
        let state = if live { "active".to_string() } else { task.state.clone() };
        let mut next = Vec::new();
        if mine {
            next.push("computer_act or computer_observe to continue; computer_finish when the checks are met".into());
        } else if live {
            next.push("another agent holds this task; wait or ask a person".into());
        } else if matches!(task.state.as_str(), "active" | "created" | "interrupted") {
            next.push("control ended; computer_finish records how it went, or computer_begin starts again (old handles are not revived)".into());
        } else {
            next.push("the task is over; begin a new one for more work".into());
        }
        if !unknown.is_empty() {
            next.push(format!("check {} before repeating anything", unknown.iter().map(|o| o.operation_ref.as_str()).take(3).collect::<Vec<_>>().join(", ")));
        }
        for d in task.deliveries.iter().filter(|d| d.get("required") != Some(&Value::Bool(false)) && !self.storage.delivery_verified(&task.task_ref, d)) {
            let (host, path) = (d.get("host_id").and_then(Value::as_str).unwrap_or(""), d.get("destination_path").and_then(Value::as_str).unwrap_or(""));
            next.push(match d.get("artifact_ref").and_then(Value::as_str) {
                Some(art) => format!("{art} is not delivered to {host}:{path} yet. {}", self.fetch_hint(art, host, path)),
                None => format!("nothing is sent to {host}:{path} yet: computer_files({{op: \"send\", path, to: {{host: \"{host}\", path: \"{path}\"}}}}) first"),
            });
        }
        if let Some(text) = note.as_ref().and_then(|n| n.get("next_step")).and_then(Value::as_str) {
            next.push(format!("your last note: {}", squash(text, 300)));
        }
        let summary = format!(
            "\"{}\" · {met}/{total} checks · {} unknown · {} attention{}",
            squash(&task.goal, 80),
            unknown.len(),
            attention.len(),
            task.completion.as_ref().and_then(|c| c.get("summary")).and_then(Value::as_str).map(|s| format!(" · finished: {}", squash(s, 80))).unwrap_or_default()
        );
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "task".into(),
                state,
                summary: Some(summary),
                parent: Some(self.computer_id.clone()),
                children: children.into_iter().take(20).collect(),
                next,
            },
            Some(task.task_ref.clone()),
        ))
    }

    fn op_status(&self, principal: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let Some(op) = self.journal.get_operation_by_ref(reference)? else {
            return Ok((self.ended(reference, "op", "No such operation."), None));
        };
        let readable = match &op.task_ref {
            Some(t) => self.require_readable_task(principal, t).is_ok(),
            None => op.principal == principal,
        };
        if !readable {
            return Ok((self.ended(reference, "op", "No such operation."), None));
        }
        // A job-backed call reads as running until something asks its job;
        // ask now, so an agent that lost the reply (and its control with it)
        // learns what happened instead of waiting on a finished job.
        let op = if op.receipt.get("execution").and_then(Value::as_str) == Some("running") && op.receipt.get("job_ref").is_some() {
            self.op_state(&op, principal)?;
            self.journal.get_operation_by_ref(reference)?.unwrap_or(op)
        } else {
            op
        };
        let r = &op.receipt;
        let execution = r.get("execution").and_then(Value::as_str).unwrap_or("unknown");
        let outcome = r.get("step").and_then(|s| s.get("outcome")).and_then(Value::as_str);
        let state = match (execution, outcome) {
            (_, Some(o)) if execution != "unknown" => o.to_string(),
            ("completed", _) => "done".into(),
            (e, _) => e.to_string(),
        };
        let mut next = Vec::new();
        if execution == "unknown" {
            next.push("ibara cannot tell whether this happened: observe the result before any new attempt; it is never replayed".to_string());
        } else if execution == "running" {
            next.push(format!("computer_wait({{for: {{op: \"{reference}\"}}}})"));
        } else if let Some(att) = r.get("held").and_then(|h| h.get("attention")).and_then(Value::as_str) {
            next.push(format!("computer_wait({{for: {{attention: \"{att}\"}}}}), then repeat the same request_id"));
        }
        let summary = format!(
            "{} {} · {} · {}",
            op.tool,
            op.request_id,
            op.effect_class,
            r.get("summary").and_then(Value::as_str).map(|s| squash(s, 120)).unwrap_or_default()
        );
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "op".into(),
                state,
                summary: Some(summary),
                parent: op.task_ref.clone(),
                children: Vec::new(),
                next,
            },
            op.task_ref.clone(),
        ))
    }

    fn attention_status(&self, principal: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let item = self.journal.get_attention(reference)?;
        let Some(item) = item.filter(|i| self.require_readable_task(principal, &i.task_ref).is_ok()) else {
            return Ok((self.ended(reference, "attention", "No such attention item."), None));
        };
        let summary = match &item.answer {
            Some(answer) => format!("{} · answered: {}", squash(&item.question, 120), squash(answer, 80)),
            None => squash(&item.question, 160),
        };
        let next = match item.state.as_str() {
            "open" => vec![format!("computer_wait({{for: {{attention: \"{reference}\"}}}})")],
            _ if item.kind == "approval" => vec!["repeat the held call with the same request_id".to_string()],
            _ => Vec::new(),
        };
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "attention".into(),
                state: item.state.clone(),
                summary: Some(summary),
                parent: Some(item.task_ref.clone()),
                children: item.operation_ref.iter().cloned().collect(),
                next,
            },
            Some(item.task_ref),
        ))
    }

    fn artifact_status(&self, principal: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let stored = self.journal.get_artifact(reference)?;
        let Some(stored) = stored.filter(|s| s.task_ref.as_deref().is_some_and(|t| self.require_readable_task(principal, t).is_ok())) else {
            return Ok((self.ended(reference, "artifact", "No such published file."), None));
        };
        let r = &stored.record;
        let state = r.get("delivery").and_then(Value::as_str).unwrap_or("available").to_string();
        let summary = format!(
            "{} · {} bytes · sha256 {}",
            r.get("name").and_then(Value::as_str).unwrap_or("file"),
            r.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
            r.get("sha256").and_then(Value::as_str).map(|s| clip(s, 12)).unwrap_or_default()
        );
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "artifact".into(),
                state,
                summary: Some(summary),
                parent: stored.task_ref.clone(),
                children: Vec::new(),
                next: self.artifact_next(reference, stored.task_ref.as_deref()),
            },
            stored.task_ref,
        ))
    }

    /// What moves a published file's bytes: for each delivery of it, the
    /// collector's fetch until it is verified; with none, the send first.
    fn artifact_next(&self, artifact_ref: &str, task_ref: Option<&str>) -> Vec<String> {
        let task = task_ref.and_then(|t| self.task(t).ok());
        let deliveries: Vec<&Value> = task.iter().flat_map(|t| &t.deliveries).filter(|d| d.get("artifact_ref").and_then(Value::as_str) == Some(artifact_ref)).collect();
        if deliveries.is_empty() {
            return vec![format!(
                "not sent anywhere yet: computer_files({{op: \"send\", path, to: {{host, path}}}}) names where it must go, the computer you work from; then run `ibara client --computer {} fetch {artifact_ref} <path>` there to fetch it, or a person saves it from the ibara console",
                self.computer_id
            )];
        }
        deliveries
            .into_iter()
            .map(|d| {
                let (host, path) = (d.get("host_id").and_then(Value::as_str).unwrap_or(""), d.get("destination_path").and_then(Value::as_str).unwrap_or(""));
                if task_ref.is_some_and(|t| self.storage.delivery_verified(t, d)) {
                    format!("delivered: ibara verified that {host} has it at {path}")
                } else {
                    format!("not delivered yet. {}", self.fetch_hint(artifact_ref, host, path))
                }
            })
            .collect()
    }

    fn frame_status(&self, principal: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let stored = self.journal.get_observation(reference)?;
        let Some(stored) = stored.filter(|s| s.task_ref.as_deref().is_some_and(|t| self.require_readable_task(principal, t).is_ok())) else {
            return Ok((self.ended(reference, "frame", "No such frame."), None));
        };
        let task_ref = stored.task_ref.clone().unwrap_or_default();
        let latest = self.frames.borrow().latest(&task_ref).map(|f| f.frame.frame_ref.clone());
        let state = if stored.expired || stored.epoch.as_deref() != Some(self.epoch.as_str()) {
            "expired"
        } else if latest.as_deref() == Some(reference) {
            "current"
        } else {
            "stale"
        };
        let next = if state == "current" {
            vec!["computer_act with a choice_id from this frame".to_string()]
        } else {
            vec!["computer_observe for a fresh frame; old choices are not revived".to_string()]
        };
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "frame".into(),
                state: state.into(),
                summary: stored.record.get("covered").and_then(Value::as_str).map(str::to_string),
                parent: Some(task_ref.clone()),
                children: Vec::new(),
                next,
            },
            Some(task_ref),
        ))
    }

    /// A continuation note saved with `computer_checkpoint`: its text, as the
    /// agent wrote it (not verified by ibara).
    fn note_status(&self, principal: &str, reference: &str) -> Result<(RefStatus, Option<String>)> {
        let note = self.journal.get_checkpoint(reference)?;
        let task_ref = note.as_ref().and_then(|n| n.get("task_ref")).and_then(Value::as_str).map(str::to_string);
        let (Some(note), Some(task_ref)) = (note, task_ref.filter(|t| self.require_readable_task(principal, t).is_ok())) else {
            return Ok((self.ended(reference, "note", "No such note."), None));
        };
        Ok((
            RefStatus {
                reference: reference.to_string(),
                kind: "note".into(),
                state: "saved".into(),
                summary: note.get("next_step").and_then(Value::as_str).map(|t| squash(t, 600)),
                parent: Some(task_ref.clone()),
                children: Vec::new(),
                next: vec![format!("computer_status({{ref: \"{task_ref}\"}}) for the task")],
            },
            Some(task_ref),
        ))
    }

    /// Evaluate a task's checks and record the states on the task. Checks that
    /// need the desktop are evaluated only when `live` (finish). An automatic
    /// check ibara could not read (no accessibility tree, the page reader not
    /// answering) takes the agent's assessment of it, when there is one.
    async fn evaluate_task_checks(&self, task: &mut TaskRecord, assessments: &[(String, bool, String)], live: bool) -> Result<Vec<CheckStatus>> {
        let mut out = Vec::new();
        let mut changed = false;
        for criterion in task.success_criteria.clone().iter() {
            let id = criterion.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let check: Option<Check> = criterion.get("check").filter(|c| !c.is_null()).and_then(|c| serde_json::from_value(c.clone()).ok());
            let previous = criterion.get("state").and_then(Value::as_str).map(str::to_string);
            let assessed = assessments.iter().find(|(c, ..)| *c == id);
            let yours = |met: bool| if met { CheckState::Met } else { CheckState::Unmet };
            let (basis, state, detail) = match check {
                Some(check) => {
                    let needs_desktop = matches!(check, Check::Url(_) | Check::Element(_) | Check::TextPresent(_));
                    if needs_desktop && !live {
                        (CheckBasis::Automatic, parse_state(previous.as_deref()), None)
                    } else {
                        let e = self.evaluate_check(task, &check).await;
                        match assessed {
                            Some((_, met, reason)) if e.unread => {
                                let why = e.detail.as_deref().unwrap_or("it could not be read");
                                (CheckBasis::YourAssessment, yours(*met), Some(clip(&format!("ibara could not read it ({why}); your assessment: {reason}"), 400)))
                            }
                            _ => (CheckBasis::Automatic, e.state, e.detail),
                        }
                    }
                }
                None => match assessed {
                    Some((_, met, reason)) => (CheckBasis::YourAssessment, yours(*met), Some(clip(reason, 300))),
                    None => (CheckBasis::YourAssessment, parse_state(previous.as_deref()), None),
                },
            };
            let state_str = state_name(state);
            if previous.as_deref() != Some(state_str) {
                changed = true;
                if let Some(c) = task.success_criteria.iter_mut().find(|c| c.get("id").and_then(Value::as_str) == Some(id.as_str()))
                    && let Some(obj) = c.as_object_mut()
                {
                    obj.insert("state".into(), json!(state_str));
                }
            }
            out.push(CheckStatus { id, basis, state, detail });
        }
        if changed {
            task.updated_at = self.now_iso();
            self.journal.put_task(task)?;
        }
        Ok(out)
    }

    // ---- computer_begin ---------------------------------------------------------------

    fn resolve_computer(&self, name: Option<&str>) -> Result<()> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        let lower = name.to_lowercase();
        let known = name == self.computer_id
            || name == self.endpoint_id
            || self.computer.name.to_lowercase() == lower
            || self.computer_name().to_lowercase() == lower
            || self.computer.labels.iter().any(|l| l.to_lowercase() == lower);
        if known {
            Ok(())
        } else {
            Err(invalid(format!(
                "computer: '{}' is not a computer this connection reaches; use \"{}\" ({})",
                clip(name, 60),
                self.computer_name(),
                self.computer_id
            )))
        }
    }

    async fn begin(&self, principal: &str, connection_id: &str, agent: &str, args: &Value) -> Result<Reply> {
        let input: BeginInput = parse_input("computer_begin", args)?;
        self.resolve_computer(input.computer.as_deref())?;
        let request_id = input.request_id.to_string();
        let source = fingerprint_source(args);
        let provisional = Value::Object(self.receipt(&request_id, "pending", "task_pending", "Begin intent recorded."));
        let now = self.now_iso();
        let remembered = self.journal.remember_intent(RememberIntent {
            principal,
            request_id: &request_id,
            task_ref: None,
            tool: "computer_begin",
            args_fingerprint_source: &source,
            receipt: &provisional,
            now_iso: &now,
            recovery_operation_ref: None,
            effect_class: "observe",
        })?;
        let op = match remembered {
            Remembered::FailedBeginReplay { error, .. } => return Err(error),
            Remembered::Replay(op) => {
                let task_ref = op.task_ref.clone().or_else(|| op.receipt.get("task_ref").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
                self.require_readable_task(principal, &task_ref)?;
                return self.stored_call(&op, &task_ref).unwrap_or_else(|| {
                    Err(fail("OUTCOME_UNKNOWN", "An earlier begin with this request_id was interrupted; begin again with a new request_id.", false))
                });
            }
            Remembered::Fresh(op) => op,
        };
        if let Some(mut a)=crate::access::Access::load(&self.journal)? && !a.identities.contains_key(agent) {
            if a.identities.len()>=512 { return Err(invalid("Too many identities; remove unused agent identities.")); }
            let key=a.pairings.get(principal).ok_or_else(||denied("Computer is not paired."))?.key.clone();
            a.identities.insert(agent.into(),crate::access::Identity {kind:"agent".into(),computer:principal.into(),key});
            a.save_identity(&self.journal,agent,"First use of a computer-vouched agent identity")?;
        }
        let outcome = self.begin_fresh(principal, connection_id, agent, &input, &op).await;
        let mut receipt;
        match &outcome {
            Ok(reply) => {
                let task_ref = reply.task_ref.clone().unwrap_or_default();
                receipt = self.receipt(&request_id, &op.operation_ref, &task_ref, "Task acquired.");
                receipt.insert("execution".into(), json!("completed"));
                let mut patch = OperationPatch::at(self.now_iso());
                patch.task_ref = Some(Some(task_ref));
                patch.receipt = Some(Value::Object(receipt.clone()));
                patch.dispatched = Some(false);
                self.journal.update_operation(&op.operation_ref, patch)?;
            }
            Err(e) => {
                receipt = self.receipt(&request_id, &op.operation_ref, "task_none", "Begin did not acquire control.");
                receipt.insert("error".into(), e.to_json());
            }
        }
        self.store_call(&op.operation_ref, &mut receipt, &outcome, None)?;
        outcome
    }

    async fn begin_fresh(&self, principal: &str, connection_id: &str, agent: &str, input: &BeginInput, op: &OperationRecord) -> Result<Reply> {
        let control = self.journal.get_control()?;
        if control.human_control {
            if let Some(wait) = self.system_wait(&control) {
                return Err(wait.refusal());
            }
            return Err(fail("HUMAN_CONTROL", "A person currently holds the computer.", false).with("next", "Wait for the person to hand back, then begin again."));
        }
        if self.display_maintenance.get() {
            return Err(fail("BUSY", "Display output maintenance is in progress.", true));
        }
        if !self.desktop.session_available() || !control.session_hint {
            return Err(fail("SESSION_UNAVAILABLE", "The graphical session is not available.", true));
        }
        self.desktop.session_ready().await?;
        let capabilities = self.refresh_capabilities().await?;
        if let Some(active) = self.journal.get_active_lease()? {
            if self.holds_lease(&active, principal, connection_id, agent) {
                return Err(fail("BUSY", format!("You already control {}; carry on with it, or finish it before beginning another task.", active.task_ref), true)
                    .with("task_ref", &active.task_ref)
                    .with("next", format!("computer_act or computer_observe to continue {}; computer_finish it before beginning another task.", active.task_ref)));
            }
            return Err(self.redacted_busy());
        }
        if control.unsettled || self.queue_depth.get() > 0 || self.storage.has_active_jobs(None) {
            self.journal.set_control(crate::store::ControlPatch { unsettled: Some(true), ..Default::default() })?;
            return Err(fail("CONTROL_UNSETTLED", "Previous controllable work has not settled.", false).requires_reconciliation());
        }
        let task_ref = id("task");
        let workspace = self.storage.workspace(&task_ref, true)?;
        for (i, spec) in input.checks.iter().enumerate() {
            if let Some(check) = &spec.check {
                self.refuse_impossible_check(&task_ref, principal, i, check, &capabilities)?;
            }
        }
        let deliver_host = match &input.deliver {
            Some(d) => {
                super::checks::destination_path("deliver.path", &d.path)?;
                Some(self.delivery_host(principal, "deliver.host", &d.host)?)
            }
            None => None,
        };
        let now_ms = self.now_ms();
        let now = self.now_iso();
        let criteria: Vec<Value> = input
            .checks
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "description": c.description,
                    "check": c.check,
                    "basis": if c.check.is_some() { "automatic" } else { "your_assessment" },
                    "required": true,
                    "state": "pending",
                })
            })
            .collect();
        let deliveries: Vec<Value> = input
            .deliver
            .iter()
            .zip(&deliver_host)
            .map(|(d, host)| json!({ "id": "deliver", "destination": "artifact_collection", "host_id": host, "destination_path": d.path, "required": true, "revision": 1 }))
            .collect();
        let task = TaskRecord {
            task_ref: task_ref.clone(),
            principal: principal.to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            state: "active".into(),
            goal: input.goal.clone(),
            success_criteria: criteria,
            budgets: json!({ "active_control_seconds": null, "max_actions": 200, "text_chars": 12000, "image_count": 40, "max_download_bytes": 67108864, "max_task_tabs": 5, "max_wait_ms": 600000 }),
            client_flags: json!({ "agent": agent }),
            authorization_ref: None,
            required_capabilities: Vec::new(),
            control_started_ms: Some(now_ms),
            last_charge_ms: Some(now_ms),
            active_control_used_ms: 0,
            actions_used: 0,
            images_used: 0,
            last_checkpoint_ref: None,
            completion: None,
            contract_version: "4".into(),
            visibility: "private".into(),
            owner_group: None,
            deliveries,
        };
        let lease = LeaseRecord {
            generation: id("lease"),
            task_ref: task_ref.clone(),
            principal: principal.to_string(),
            connection_id: connection_id.to_string(),
            epoch: self.epoch.clone(),
            acquired_at: now.clone(),
            last_heartbeat_at: now.clone(),
            last_heartbeat_ms: now_ms,
            idle_expires_at_ms: now_ms + self.idle_expiry_ms,
            state: "active".into(),
            reason: None,
        };
        self.journal.put_task(&task)?;
        self.journal.put_lease(&lease)?;
        self.journal.set_control(crate::store::ControlPatch { unsettled: Some(false), ..Default::default() })?;
        let mut update = OperationPatch::at(self.now_iso());
        update.task_ref = Some(Some(task_ref.clone()));
        self.journal.update_operation(&op.operation_ref, update)?;
        if let Err(e) = self.desktop.set_idle_inhibited(true).await {
            super::log_event("idle_inhibit_failed", &e.to_string());
        }
        self.desktop.set_agent(Some(agent.to_string()));
        self.timeline("task_began", Some(&task_ref), agent, &squash(&input.goal, 200), json!({ "principal": principal }));
        let frame = match self.build_frame(&task, &lease, &FrameSpec::default()).await {
            Ok((frame, _)) => frame.frame.clone(),
            Err(e) => unreadable_frame(&e),
        };
        let notes = frame.lines.iter().filter(|l| l.starts_with("note: ")).map(|l| l.trim_start_matches("note: ").to_string()).collect();
        let checks = input
            .checks
            .iter()
            .map(|c| CheckStatus {
                id: c.id.to_string(),
                basis: if c.check.is_some() { CheckBasis::Automatic } else { CheckBasis::YourAssessment },
                state: CheckState::Pending,
                detail: None,
            })
            .collect();
        let result = BeginResult {
            task_ref: task_ref.clone(),
            computer: ComputerId { id: self.computer_id.clone(), name: self.computer_name() },
            you: You { agent: agent.to_string(), principal: principal.to_string() },
            workspace: workspace.to_string_lossy().into_owned(),
            checks,
            frame,
            notes,
        };
        Ok(Reply::ok(result, Some(&task_ref)))
    }

    // ---- computer_observe -------------------------------------------------------------

    async fn observe(&self, principal: &str, connection_id: &str, input: ObserveInput) -> Result<Reply> {
        let lease = self.require_live_lease(principal, connection_id, input.task_ref.as_str())?;
        let mut task = self.task(&lease.task_ref)?;
        let charge = if matches!(input.view, View::Image | View::Screen) { Charge::Image } else { Charge::Control };
        self.charge(&mut task, charge)?;
        self.desktop.session_ready().await?;
        let spec = FrameSpec { view: input.view, query: input.query, surface: input.surface, limit: input.limit, cursor: input.cursor };
        let (frame, image) = self.build_frame(&task, &lease, &spec).await?;
        let mut reply = Reply::ok(ObserveResult { frame: frame.frame.clone() }, Some(&task.task_ref));
        reply.images.extend(image);
        Ok(reply)
    }

    fn task(&self, task_ref: &str) -> Result<TaskRecord> {
        self.journal.get_task(task_ref)?.ok_or_else(|| denied("Task is private or this principal has no active group grant."))
    }

    // ---- computer_act and browser_act ----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn act(&self, principal: &str, connection_id: &str, tool: &str, task_ref: &str, request_id: &str, args: &Value, steps: Vec<StepSpec>, gone: &Cancel) -> Result<Reply> {
        let lease = self.require_live_lease(principal, connection_id, task_ref)?;
        let mut task = self.task(task_ref)?;
        self.charge(&mut task, Charge::Action)?;
        let ctx = CallCtx { principal, task_ref, request_id, tool, lease: &lease, gone };
        let declared = steps
            .iter()
            .filter_map(|s| match s {
                StepSpec::Desktop(step) => step.effect,
                StepSpec::Browser { effect, .. } => *effect,
            })
            .map(class_name)
            .find(|c| *c != "change")
            .unwrap_or("change");
        match self.remember(&ctx, request_id, &fingerprint_source(args), declared)? {
            Remembered::Fresh(op) => self.run_steps(&ctx, op, &steps, 0, Vec::new(), false).await,
            Remembered::Replay(op) | Remembered::FailedBeginReplay { operation: op, .. } => self.replay_act(&ctx, op, &steps).await,
        }
    }

    /// A repeated act: the stored result, or, when a step was held and a person
    /// has since approved it under the same control, the rest of the call.
    async fn replay_act(&self, ctx: &CallCtx<'_>, op: OperationRecord, steps: &[StepSpec]) -> Result<Reply> {
        let held = op
            .receipt
            .get("call")
            .and_then(|c| c.get("held"))
            .and_then(|h| Some((h.get("index")?.as_u64()? as usize, h.get("attention")?.as_str()?.to_string())));
        let Some((index, att)) = held else {
            return self.stored_call(&op, ctx.task_ref).unwrap_or_else(|| self.reconstruct_act(ctx, &op, steps.len()));
        };
        // A held step that has since been dispatched (then interrupted) is
        // reported from its operations, never run again.
        let held_op = if index == 0 { Some(op.clone()) } else { self.journal.get_mutation_operation(ctx.principal, ctx.task_ref, &format!("{}#{index}", ctx.request_id))? };
        if held_op.is_none_or(|o| o.receipt.get("execution").and_then(Value::as_str) != Some("not_started")) {
            return self.reconstruct_act(ctx, &op, steps.len());
        }
        let prior: Vec<StepResult> = op
            .receipt
            .get("call")
            .and_then(|c| c.get("result"))
            .and_then(|r| r.get("steps"))
            .and_then(|s| serde_json::from_value::<Vec<StepResult>>(s.clone()).ok())
            .unwrap_or_default();
        let (decision, why) = self.approval(&att, ctx.lease)?;
        match decision {
            None => self.stored_call(&op, ctx.task_ref).unwrap_or_else(|| Err(crate::error::internal("held call without a stored result"))),
            Some(true) => self.run_steps(ctx, op, steps, index, prior.into_iter().take(index).collect(), true).await,
            Some(false) => {
                let mut results: Vec<StepResult> = prior.into_iter().take(index).collect();
                let step_op = if index == 0 { Some(op.operation_ref.clone()) } else { self.step_op_ref(ctx, index) };
                results.push(step_result(index, StepOutcome::NotRun, format!("not run: {why}"), step_op.as_deref()));
                for i in index + 1..steps.len() {
                    results.push(step_result(i, StepOutcome::NotRun, "not run: an earlier step did not run", None));
                }
                if let Some(step_op) = &step_op
                    && let Some(record) = self.journal.get_operation_by_ref(step_op)?
                {
                    let mut receipt = record.receipt.as_object().cloned().unwrap_or_default();
                    receipt.remove("held");
                    receipt.insert("summary".into(), json!(clip(&format!("Not run: {why}."), 2000)));
                    receipt.insert("step".into(), json!({ "index": index, "outcome": "not_run" }));
                    self.save_receipt(step_op, &receipt, Some(false))?;
                }
                let reply = Reply::ok(ActResult { steps: results, frame: None, attention: None, next: None }, Some(ctx.task_ref));
                let mut receipt = self.journal.get_operation_by_ref(&op.operation_ref)?.map(|o| o.receipt).and_then(|r| r.as_object().cloned()).unwrap_or_default();
                let outcome = Ok(reply);
                self.store_call(&op.operation_ref, &mut receipt, &outcome, None)?;
                outcome
            }
        }
    }

    fn step_op_ref(&self, ctx: &CallCtx<'_>, index: usize) -> Option<String> {
        self.journal
            .get_mutation_operation(ctx.principal, ctx.task_ref, &format!("{}#{index}", ctx.request_id))
            .ok()
            .flatten()
            .map(|o| o.operation_ref)
    }

    /// A call interrupted before its result was stored (a restart): report each
    /// step from its own operation. Nothing is dispatched again.
    fn reconstruct_act(&self, ctx: &CallCtx<'_>, op0: &OperationRecord, count: usize) -> Result<Reply> {
        let mut results = Vec::new();
        for i in 0..count {
            let op = if i == 0 {
                Some(op0.clone())
            } else {
                self.journal.get_mutation_operation(ctx.principal, ctx.task_ref, &format!("{}#{i}", ctx.request_id))?
            };
            let Some(op) = op else {
                results.push(step_result(i, StepOutcome::NotRun, "not run", None));
                continue;
            };
            let r = &op.receipt;
            let (outcome, text) = match (r.get("execution").and_then(Value::as_str), r.get("verification").and_then(Value::as_str)) {
                (Some("unknown"), _) | (Some("running"), _) if r.get("epoch").and_then(Value::as_str) != Some(self.epoch.as_str()) => {
                    (StepOutcome::Unknown, "outcome unknown: the controller restarted during this step")
                }
                (Some("unknown"), _) | (Some("running"), _) => (StepOutcome::Unknown, "outcome unknown: the call ended during this step"),
                (Some("completed"), Some("unsatisfied")) => (StepOutcome::Unmet, "ran; expectation not seen"),
                (Some("completed"), _) => (StepOutcome::Done, "done"),
                _ => (StepOutcome::NotRun, "not run"),
            };
            results.push(step_result(i, outcome, text, Some(&op.operation_ref)));
        }
        Ok(Reply::ok(ActResult { steps: results, frame: None, attention: None, next: None }, Some(ctx.task_ref)))
    }

    /// Run steps from `start`, stopping at the first step that is unmet,
    /// unknown, refused or held. `approved_first`: the step at `start` was
    /// approved by a person.
    async fn run_steps(&self, ctx: &CallCtx<'_>, op0: OperationRecord, steps: &[StepSpec], start: usize, mut results: Vec<StepResult>, approved_first: bool) -> Result<Reply> {
        let mut receipt0: Map<String, Value> = op0.receipt.as_object().cloned().unwrap_or_default();
        let mut stop = false;
        let mut attention: Option<(usize, String)> = None;
        let mut touched_desktop = false;
        // A step's "after" picture is the next step's "before" when both show
        // the same window: nothing acts in between.
        let mut last_after: Option<(WinKey, Image)> = None;
        for (i, spec) in steps.iter().enumerate().skip(start) {
            if stop || ctx.gone.is_cancelled() {
                stop = true;
                let why = if ctx.gone.is_cancelled() { "not run: the caller went away" } else { "not run: an earlier step did not complete" };
                results.push(step_result(i, StepOutcome::NotRun, why, None));
                continue;
            }
            let frame = self.frames.borrow().latest(ctx.task_ref).filter(|f| f.generation == ctx.lease.generation);
            let windows = match self.desktop.windows().await {
                Ok(w) => w,
                Err(e) if i == 0 => return self.refuse_call(ctx, &op0, &mut receipt0, e),
                Err(e) => {
                    results.push(step_result(i, StepOutcome::NotRun, format!("not run: {}", e.message), None));
                    stop = true;
                    continue;
                }
            };
            let mut resolved = match self.resolve_step(ctx.task_ref, frame.as_deref(), &windows, spec) {
                Ok(r) => r,
                Err(e) if i == 0 => return self.refuse_call(ctx, &op0, &mut receipt0, e),
                Err(e) => {
                    results.push(step_result(i, StepOutcome::NotRun, format!("not run: {}", e.message), None));
                    stop = true;
                    continue;
                }
            };
            // What a key, or typing that ends a line, meets, read now.
            let focus = self.focus(&resolved, &windows).await;
            // A press ibara sees submit a form is a send, whatever was declared.
            if let Some(seen) = guard::seen_class(&resolved, frame.as_deref(), &focus) {
                resolved.class = guard::stricter_class(resolved.class, seen);
            }
            // The step's operation: the call's for step 0, its own otherwise.
            let (step_op, mut receipt) = if i == 0 {
                (op0.clone(), receipt0.clone())
            } else {
                let source = json!({ "index": i, "step": step_source(spec) });
                let op = match self.remember(ctx, &format!("{}#{i}", ctx.request_id), &source, resolved.class)? {
                    Remembered::Fresh(op) | Remembered::Replay(op) | Remembered::FailedBeginReplay { operation: op, .. } => op,
                };
                let receipt = op.receipt.as_object().cloned().unwrap_or_default();
                (op, receipt)
            };
            let op_ref = step_op.operation_ref.clone();
            let step_request = if i == 0 { ctx.request_id.to_string() } else { format!("{}#{i}", ctx.request_id) };
            let action = guard::action(&resolved, frame.as_deref(), &focus);
            let again = match self.approval_guard(ctx.task_ref, &op_ref, &action)? {
                Guard::Refuse(e) if i == 0 => return self.refuse_call(ctx, &op0, &mut receipt0, e),
                Guard::Refuse(e) => {
                    receipt.insert("error".into(), e.to_json());
                    self.save_receipt(&op_ref, &receipt, Some(false))?;
                    results.push(step_result(i, StepOutcome::NotRun, format!("refused: {}", e.message), Some(&op_ref)));
                    stop = true;
                    continue;
                }
                Guard::AskAgain(held, refused) => {
                    resolved.class = guard::stricter_class(resolved.class, held);
                    refused
                }
                Guard::Clear => false,
            };
            receipt.insert("action".into(), action);
            receipt.insert("operation_ref".into(), json!(op_ref));
            receipt.insert("effect_class".into(), json!(resolved.class));
            receipt.insert("step".into(), json!({ "index": i, "describe": resolved.describe }));
            // An approval covers the exact target the person saw: re-resolving
            // at replay can land elsewhere (focus moved, a newer frame).
            let target = target_identity(&resolved);
            let approved_target = receipt.get("held").and_then(|h| h.get("target")).cloned();
            let changed = approved_first && i == start && approved_target.as_ref() != Some(&target);
            let approved = approved_first && i == start && !changed;
            let rule = self.effect_rule(ctx.task_ref, ctx.principal, "change").max(self.effect_rule(ctx.task_ref, ctx.principal, resolved.class));
            match if again { rule.max(Rule::Ask) } else { rule } {
                Rule::Deny => {
                    let e = denied(format!("{} steps are not allowed on this computer.", resolved.class)).with("effect_class", resolved.class);
                    if results.is_empty() {
                        return self.refuse_call(ctx, &op0, &mut receipt0, e);
                    }
                    receipt.insert("error".into(), e.to_json());
                    self.save_receipt(&op_ref, &receipt, Some(false))?;
                    results.push(step_result(i, StepOutcome::NotRun, format!("refused: {} steps are denied here", resolved.class), Some(&op_ref)));
                    stop = true;
                    continue;
                }
                Rule::Ask if !approved => {
                    let describe = if changed { format!("{} (the target changed since it was approved)", resolved.describe) } else { resolved.describe.clone() };
                    let ask = Ask { doing: resolved.said.clone(), place: self.place_of(&resolved, &windows).await, class: resolved.class, says_why: false, changed, again };
                    let (summary, details) = self.approval_words(ctx, &ask, json!({ "step": step_source(spec), "target": target }));
                    let att = self.hold(ctx, &op_ref, &describe, &summary, &details)?;
                    receipt.insert("held".into(), json!({ "attention": att, "index": i, "target": target }));
                    receipt.insert("summary".into(), json!(format!("Held for approval ({att}).")));
                    self.save_receipt(&op_ref, &receipt, Some(false))?;
                    if i == 0 {
                        receipt0 = receipt.clone();
                    }
                    let wait = "not run yet: wait for the answer, then send this same request again";
                    let text = if changed {
                        format!("the target changed since it was approved; held again for a person's approval ({att}); {wait}")
                    } else {
                        format!("held for a person's approval ({att}); {wait}")
                    };
                    results.push(step_result(i, StepOutcome::NotRun, text, Some(&op_ref)));
                    attention = Some((i, att));
                    stop = true;
                    continue;
                }
                _ => {}
            }
            receipt.remove("held");
            let expect = spec.expect();
            receipt.insert("execution".into(), json!("running"));
            receipt.insert("verification".into(), json!(if expect.is_some() { "pending" } else { "not_requested" }));
            receipt.insert("effect".into(), json!(if matches!(resolved.plan, Planned::Observe) { "none" } else { "unknown" }));
            receipt.insert("summary".into(), json!(format!("{} dispatched.", ctx.tool)));
            receipt.insert("request_id".into(), json!(step_request));
            self.save_receipt(&op_ref, &receipt, Some(true))?;
            self.note_step(ctx.task_ref, &resolved.said);
            let started = Instant::now();
            let focused_before = windows.iter().find(|w| w.focused).cloned();
            // Replay pictures: the window the step acts on, before and after.
            let picture_surface = step_surface(&resolved, focused_before.as_ref());
            let pictures = !matches!(resolved.plan, Planned::Observe) && !self.password_shown(&resolved, frame.as_deref()).await;
            let before = match last_after.take() {
                Some((key, image)) if pictures && picture_surface.as_ref() == Some(&key) => Some(image),
                _ if pictures => self.replay_capture(picture_surface.as_ref()).await,
                _ => None,
            };
            let dispatched = self.dispatch_planned(ctx, &resolved).await;
            if !matches!(resolved.plan, Planned::Observe) {
                touched_desktop = true;
            }
            let launched = dispatched.as_ref().ok().and_then(|d| d.pid);
            // Control that changed hands after the input finished leaves nothing held.
            let dispatched = dispatched.and_then(|done| self.assert_authority(ctx.lease).map(|_| done).map_err(|e| e.with("no_input_held", true)));
            let (outcome, text) = match dispatched {
                Err(e) if not_started(&e) => {
                    receipt.insert("execution".into(), json!("not_started"));
                    receipt.insert("verification".into(), json!("not_requested"));
                    receipt.insert("effect".into(), json!("none"));
                    receipt.insert("summary".into(), json!(clip(&format!("Not started: {}", e.message), 2000)));
                    receipt.insert("error".into(), e.to_json());
                    receipt.insert("step".into(), json!({ "index": i, "describe": resolved.describe, "outcome": "not_run" }));
                    self.save_receipt(&op_ref, &receipt, Some(false))?;
                    self.note_route(focused_before.as_ref(), frame.as_deref(), &resolved, Some(&e));
                    if results.is_empty() {
                        return self.refuse_call(ctx, &op0, &mut receipt0, e);
                    }
                    results.push(step_result(i, StepOutcome::NotRun, format!("not run: {}", e.message), Some(&op_ref)));
                    stop = true;
                    continue;
                }
                Err(e) => {
                    let e = partly_sent(e);
                    // Input that may still be held keeps the computer fenced until
                    // the watchdog settles it; input that stopped cleanly (between
                    // typed pieces, or finished before control changed hands) does not.
                    let held = e.details.get("no_input_held") != Some(&json!(true));
                    if held && self.journal.get_control()?.settling_generation.is_some() {
                        self.journal.set_control(crate::store::ControlPatch { unsettled: Some(true), settling_generation: Some(None), ..Default::default() })?;
                    }
                    receipt.insert("execution".into(), json!("unknown"));
                    receipt.insert("verification".into(), json!("unknown"));
                    receipt.insert("effect".into(), json!("unknown"));
                    // With the cause, which also says what happened to the clipboard.
                    receipt.insert("summary".into(), json!(clip(&format!("Dispatch started; ibara cannot confirm whether the effect completed: {}", e.message), 2000)));
                    receipt.insert(
                        "error".into(),
                        json!({ "code": "OUTCOME_UNKNOWN", "message": clip(&e.message, 1000), "detail": e.details.get("detail").cloned().unwrap_or(Value::Null), "retry_safe": false, "requires_reconciliation": true }),
                    );
                    stop = true;
                    (StepOutcome::Unknown, format!("outcome unknown: {}", e.message))
                }
                Ok(done) => {
                    if let Some(point) = done.point {
                        self.note_click(ctx.task_ref, point);
                    }
                    let (outcome, awaited) = match expect {
                        Some(expect) => {
                            let awaited = self.await_expectation(ctx.task_ref, expect, &windows, frame.as_deref(), None, ctx.gone).await;
                            receipt.insert("verification".into(), json!(if awaited.met { "satisfied" } else { "unsatisfied" }));
                            receipt.insert("expectation".into(), json!({ "met": awaited.met, "detail": awaited.detail, "waited_ms": awaited.waited_ms }));
                            if awaited.met { (StepOutcome::Done, awaited.detail) } else { (StepOutcome::Unmet, awaited.detail) }
                        }
                        None => (StepOutcome::Done, String::new()),
                    };
                    // What the step itself saw happen (where a clicked page went), then the expectation.
                    let detail = match done.note {
                        Some(note) if awaited.is_empty() => note,
                        Some(note) => format!("{note} · {awaited}"),
                        None => awaited,
                    };
                    receipt.insert("execution".into(), json!("completed"));
                    receipt.insert("effect".into(), json!(if matches!(resolved.plan, Planned::Observe) { "none" } else { "local_change" }));
                    receipt.insert("summary".into(), json!(clip(&format!("{}{}", resolved.describe, if detail.is_empty() { String::new() } else { format!(" · {detail}") }), 2000)));
                    receipt.remove("error");
                    if outcome == StepOutcome::Unmet {
                        stop = true;
                    }
                    self.note_route(focused_before.as_ref(), frame.as_deref(), &resolved, None);
                    let text = if detail.is_empty() { resolved.describe.clone() } else { format!("{} · {detail}", resolved.describe) };
                    (outcome, text)
                }
            };
            if outcome == StepOutcome::Done {
                self.await_launched_window(ctx.task_ref, &windows, &resolved, launched, ctx.gone).await;
            }
            receipt.insert("elapsed_ms".into(), json!(started.elapsed().as_millis() as u64));
            receipt.insert("step".into(), json!({ "index": i, "describe": resolved.describe, "outcome": outcome_name(outcome) }));
            let replay = if pictures {
                let after_surface = match &resolved.plan {
                    Planned::Desktop(effect) if matches!(effect.as_ref(), Effect::Close(_) | Effect::Launch { .. }) => None,
                    _ => picture_surface.clone(),
                };
                let after = self.replay_capture(after_surface.as_ref()).await;
                if let (Some(key), Some(image)) = (&after_surface, &after) {
                    last_after = Some((key.clone(), image.clone()));
                }
                let long = matches!(resolved.class, "send" | "spend" | "destructive" | "access") || outcome == StepOutcome::Unknown;
                self.replay_save(&op_ref, before, after, long)
            } else {
                Vec::new()
            };
            if !replay.is_empty() {
                receipt.insert("replay".into(), json!(replay));
            }
            self.save_receipt(&op_ref, &receipt, Some(true))?;
            if i == 0 {
                receipt0 = receipt.clone();
            }
            self.track_new_windows(ctx.task_ref, &windows, &resolved, focused_before.as_ref(), launched).await;
            self.timeline("step", Some(ctx.task_ref), &ctx.lease.principal, &clip(&text, 200), json!({ "op": op_ref, "outcome": outcome_name(outcome), "effect_class": resolved.class, "replay": replay }));
            results.push(step_result(i, outcome, text, Some(&op_ref)));
        }
        let frame = if touched_desktop || results.iter().any(|r| r.outcome != StepOutcome::NotRun) {
            let task = self.task(ctx.task_ref)?;
            // After a browser step the new frame is the page's elements, so
            // the next browser step can name them without another observe.
            let page = FrameSpec { view: View::Elements, surface: Some("tab".into()), ..FrameSpec::default() };
            let browser = steps.iter().all(|s| matches!(s, StepSpec::Browser { .. }));
            let built = match browser {
                true => match self.build_frame(&task, ctx.lease, &page).await {
                    Ok(built) => Ok(built),
                    Err(_) => self.build_frame(&task, ctx.lease, &FrameSpec::default()).await,
                },
                false => self.build_frame(&task, ctx.lease, &FrameSpec::default()).await,
            };
            match built {
                Ok((f, _)) => Some(f.frame.clone()),
                Err(e) => Some(unreadable_frame(&e)),
            }
        } else {
            None
        };
        let held = attention.as_ref().map(|(i, a)| (*i, a.as_str()));
        let status = if held.is_some() { Status::Pending } else { Status::Ok };
        let next = held.map(|(_, a)| approval_next(ctx.tool, Some(ctx.task_ref), a));
        let reply = Reply::with_status(status, ActResult { steps: results, frame, attention: held.map(|(_, a)| a.to_string()), next }, ctx.task_ref);
        let outcome = Ok(reply);
        let mut stored = self.journal.get_operation_by_ref(&op0.operation_ref)?.and_then(|o| o.receipt.as_object().cloned()).unwrap_or(receipt0);
        self.store_call(&op0.operation_ref, &mut stored, &outcome, held)?;
        outcome
    }

    /// A typing step into a window that shows a password field keeps no
    /// picture. The latest frame answers when it read that window; otherwise
    /// the window is asked.
    async fn password_shown(&self, resolved: &Resolved, frame: Option<&FrameState>) -> bool {
        let Planned::Desktop(effect) = &resolved.plan else { return false };
        let Effect::Type { surface, .. } = effect.as_ref() else { return false };
        if let Some(frame) = frame.filter(|f| f.surface.as_ref() == Some(surface)) {
            return frame.elements.iter().any(|e| e.role == "password text");
        }
        // Unknown counts as shown: no picture rather than a picture of a secret.
        self.desktop.has_password_field(surface).await.unwrap_or(true)
    }

    /// Store an error for a call whose first step never ran, and return it.
    fn refuse_call(&self, ctx: &CallCtx<'_>, op0: &OperationRecord, receipt0: &mut Map<String, Value>, e: IbaraError) -> Result<Reply> {
        let _ = ctx;
        receipt0.insert("operation_ref".into(), json!(op0.operation_ref));
        receipt0.insert("execution".into(), json!("not_started"));
        receipt0.insert("effect".into(), json!("none"));
        receipt0.insert("summary".into(), json!("No effect was dispatched."));
        receipt0.insert("error".into(), e.to_json());
        let outcome = Err(e);
        let mut patch = OperationPatch::at(self.now_iso());
        patch.dispatched = Some(false);
        self.journal.update_operation(&op0.operation_ref, patch)?;
        self.store_call(&op0.operation_ref, receipt0, &outcome, None)?;
        outcome
    }

    /// Resolve one step against the latest frame and the live windows.
    fn resolve_step(&self, task_ref: &str, frame: Option<&FrameState>, windows: &[Win], spec: &StepSpec) -> Result<Resolved> {
        match spec {
            StepSpec::Desktop(step) => self.resolve_desktop(task_ref, frame, windows, step),
            StepSpec::Browser { action, effect, .. } => self.resolve_browser(frame, windows, action, *effect),
        }
    }

    fn resolve_desktop(&self, task_ref: &str, frame: Option<&FrameState>, windows: &[Win], step: &Step) -> Result<Resolved> {
        // The window a typing or key choice was offered for, as the frame named it.
        let mut offered: Option<(WinKey, String)> = None;
        let (action, text) = match (&step.choice, &step.action) {
            (Some(choice), _) => {
                let frame = frame.ok_or_else(|| fail("STALE_TARGET", "There is no current frame; observe first.", true))?;
                let chosen = frame
                    .choice(choice.as_str())
                    .ok_or_else(|| fail("STALE_TARGET", format!("choice: {} is not in the latest frame ({}); observe again", choice, frame.frame.frame_ref), true))?;
                match (&chosen.param, &step.text) {
                    (Some(param), None) => return Err(invalid(format!("text: choice {choice} takes a {param} parameter"))),
                    (None, Some(_)) => return Err(invalid(format!("text: choice {choice} takes no text"))),
                    _ => {}
                }
                offered = frame.input_for(choice.as_str()).map(|key| {
                    let id = frame.windows.iter().find(|(_, w)| w.address == key.address).map_or("", |(id, _)| id.as_str());
                    (key.clone(), format!("choice {choice} ({}) was for {id} {}", chosen.label, key.class))
                });
                (chosen.action.clone(), step.text.clone())
            }
            (None, Some(action)) => (action.clone(), None),
            (None, None) => {
                return Ok(Resolved { plan: Planned::Observe, describe: "expect".into(), said: "check the screen".into(), class: "observe", route: "expect" });
            }
        };
        // Keys and text go to the focused window; a choice's, only to the
        // window it was offered for, while that window still has the focus.
        let focused = || {
            let now = windows.iter().find(|w| w.focused);
            match (&offered, now) {
                (None, Some(w)) => Ok(w.clone()),
                (None, None) => Err(fail("STALE_TARGET", "Nothing has keyboard focus; focus a window first.", true).with("execution_not_started", true)),
                (Some((key, _)), Some(w)) if w.address == key.address && w.pid == key.pid => Ok(w.clone()),
                (Some((_, was_for)), now) => {
                    let went = now.map_or_else(|| "no window has the keyboard focus now".to_string(), |w| format!("{} \"{}\" has the keyboard focus now", w.class, squash(&w.title, 40)));
                    Err(fail("STALE_TARGET", format!("Nothing was sent: {was_for}, but {went}."), true)
                        .with("execution_not_started", true)
                        .with("next", "computer_observe for choices for the window that has the focus now, or focus the window the choice was for and then use it."))
                }
            }
        };
        let class = stricter("change", step.effect);
        // The effect, the words for the agent, and the words for a person.
        let element_target = |target: &Target| -> Result<(Effect, String, String)> {
            match target {
                Target::Element(r) => {
                    let frame = frame.ok_or_else(|| fail("STALE_TARGET", "There is no current frame; observe first.", true))?;
                    let element = frame
                        .element(r.as_str())
                        .ok_or_else(|| fail("STALE_TARGET", format!("target: {r} is not in the latest frame; observe again"), true))?;
                    let surface = frame.surface.clone().ok_or_else(|| fail("STALE_TARGET", "The frame has no window for its elements.", true))?;
                    Ok((
                        Effect::ClickElement { surface, element: Box::new(element.clone()), button: Button::Left, double: false },
                        format!("{r} {} \"{}\"", element.role, squash(&element.name, 40)),
                        approval::element_words(&element.role, &element.name),
                    ))
                }
                Target::Point(p) => {
                    // A point is a pixel of a picture: the one its frame
                    // returned, else the task's latest, whatever came after.
                    let again = "computer_observe with view \"image\" (and the window's surface), then read the point from that picture";
                    let refuse = |message: String| fail("STALE_TARGET", message, true).with("execution_not_started", true).with("next", again);
                    let pictures = frame.map_or(&[][..], |f| f.pictures.as_slice());
                    let picture = match &p.frame {
                        Some(given) => pictures.iter().find(|pic| pic.frame_ref == given.as_str()).ok_or_else(|| {
                            refuse(format!("Nothing was sent: target.frame {given} is not a picture ibara still holds for this task (only the latest picture of each window is kept)."))
                        })?,
                        None => pictures.last().ok_or_else(|| refuse("Nothing was sent: x and y are pixels of a picture, and this task has not taken one.".into()))?,
                    };
                    if let Some(was) = &picture.window {
                        match windows.iter().find(|w| w.address == was.address && w.pid == was.pid) {
                            None => return Err(refuse(format!("Nothing was sent: the window in the picture from {} ({}) is gone.", picture.frame_ref, picture.named))),
                            Some(now) if now.rect != was.rect => {
                                return Err(refuse(format!(
                                    "Nothing was sent: {} moved or changed size since its picture in {}, so the point would land elsewhere.",
                                    picture.named, picture.frame_ref
                                )));
                            }
                            Some(_) => {}
                        }
                    }
                    if p.x < 0 || p.y < 0 || p.x as u32 >= picture.width || p.y as u32 >= picture.height {
                        return Err(invalid(format!("target: {},{} is outside the picture from {} ({}x{} pixels)", p.x, p.y, picture.frame_ref, picture.width, picture.height))
                            .with("execution_not_started", true));
                    }
                    let img = Image { region: picture.region, width: picture.width, height: picture.height, ..Default::default() };
                    let (x, y) = img.to_logical(p.x as f64, p.y as f64);
                    Ok((
                        Effect::ClickPoint { x, y, surface: None, button: Button::Left, double: false },
                        format!("point {},{} of the picture of {} in {}, screen point {},{}", p.x, p.y, picture.named, picture.frame_ref, x.round(), y.round()),
                        "a spot on the screen".into(),
                    ))
                }
            }
        };
        let set_button = |effect: Effect, button: Button, double: bool| match effect {
            Effect::ClickElement { surface, element, .. } => Effect::ClickElement { surface, element, button, double },
            Effect::ClickPoint { x, y, surface, .. } => Effect::ClickPoint { x, y, surface, button, double },
            other => other,
        };
        let (effect, describe, said, route) = match action {
            Action::Launch(a) => {
                let app = app_id_for(&a.app).ok_or_else(|| {
                    denied(format!("app: '{}' is not an approved app here; approved: editor, terminal, browser, files (the file manager)", clip(&a.app, 40)))
                        .with("execution_not_started", true)
                })?;
                let said = match app {
                    "editor" => "open the text editor",
                    "terminal" => "open a terminal",
                    "files" => "open the file manager",
                    _ => "open the web browser",
                };
                (Effect::Launch { app_id: app.into() }, format!("launch {app}"), said.to_string(), "launch")
            }
            Action::Focus(s) => {
                let win = self.resolve_surface(frame, windows, &s.surface)?;
                let describe = format!("focus {} \"{}\"", win.class, squash(&win.title, 40));
                let said = format!("switch to {}", approval::window_words(&win.class, &win.title));
                (Effect::Focus(win.key()), describe, said, "focus")
            }
            Action::Close(s) => {
                let win = self.resolve_surface(frame, windows, &s.surface)?;
                let owned = self.journal.task_windows(task_ref).map_err(|e| e.with("execution_not_started", true))?;
                if !owned.iter().any(|o| o.address == win.address && o.pid == win.pid) {
                    return Err(denied(format!("surface: close only closes windows this task opened; {} was not", s.surface)).with("execution_not_started", true));
                }
                let describe = format!("close {} \"{}\"", win.class, squash(&win.title, 40));
                let said = format!("close {}", approval::window_words(&win.class, &win.title));
                (Effect::Close(win.key()), describe, said, "close")
            }
            Action::Click(t) => {
                let (e, d, s) = element_target(&t.target)?;
                let route = if matches!(e, Effect::ClickElement { .. }) { "click_element" } else { "click_point" };
                (e, format!("click {d}"), format!("click {s}"), route)
            }
            Action::DoubleClick(t) => {
                let (e, d, s) = element_target(&t.target)?;
                let route = if matches!(e, Effect::ClickElement { .. }) { "click_element" } else { "click_point" };
                (set_button(e, Button::Left, true), format!("double-click {d}"), format!("double-click {s}"), route)
            }
            Action::RightClick(t) => {
                let (e, d, s) = element_target(&t.target)?;
                (set_button(e, Button::Right, false), format!("right-click {d}"), format!("right-click {s}"), "right_click")
            }
            Action::Type(t) => {
                let text = text.unwrap_or(t.text);
                if text.contains('\0') || text.chars().count() > 6000 {
                    return Err(invalid("text: at most 6000 characters and no NUL"));
                }
                let win = focused()?;
                // Cua types only ASCII; other text would go through the
                // window's only editable element, which in a browser is the
                // address bar.
                if !text.is_ascii() && is_browser(&win.class) {
                    return Err(fail(
                        "WRONG_TOOL",
                        "Text with characters beyond ASCII goes into a web page with browser_act: type, with the field as its target; typed into the browser window it could land in the address bar.",
                        true,
                    )
                    .with("execution_not_started", true));
                }
                let describe = format!("type {} characters into {}", text.chars().count(), win.class);
                let said = format!("type {}", approval::characters(text.chars().count()));
                // A page's fields are the page reader's: a browser window's
                // accessibility tree is slow to read and holds many.
                let cursor = if is_browser(&win.class) { TypingCursor::Stays } else { TypingCursor::ToField };
                (Effect::Type { surface: win.key(), text, cursor }, describe, said, "type")
            }
            Action::Key(k) => {
                let win = focused()?;
                let describe = format!("key {} in {}", k.keys, win.class);
                let said = format!("press {}", approval::key_words(&k.keys));
                (Effect::Key { surface: win.key(), combo: k.keys }, describe, said, "key")
            }
            Action::Scroll(s) => match s.target {
                Target::Point(_) => {
                    let (e, d, _) = element_target(&s.target)?;
                    let at = match e {
                        Effect::ClickPoint { x, y, .. } => Some((x, y)),
                        _ => None,
                    };
                    (Effect::Scroll { at, dx: s.dx, dy: s.dy }, format!("scroll {},{} at {d}", s.dx, s.dy), "scroll".to_string(), "scroll")
                }
                Target::Element(_) => {
                    return Err(invalid("action.target: scroll needs a point {x, y}; elements have no position here"));
                }
            },
        };
        Ok(Resolved { plan: Planned::Desktop(Box::new(effect)), describe, said, class, route })
    }

    fn resolve_browser(&self, frame: Option<&FrameState>, windows: &[Win], action: &BrowserAction, effect: Option<EffectClass>) -> Result<Resolved> {
        if let BrowserAction::WaitFor(_) = action {
            return Ok(Resolved { plan: Planned::Observe, describe: "wait for the page".into(), said: "wait for the page".into(), class: "observe", route: "browser_wait" });
        }
        let window = windows.iter().find(|w| w.focused && is_browser(&w.class)).map(Win::key).ok_or_else(|| {
            fail("STALE_TARGET", "The browser window is not focused; focus it with computer_act, then observe surface \"tab\".", true)
                .with("execution_not_started", true)
        })?;
        // A page element of the latest observation that offers `wanted`: its
        // arguments, the words for the agent and the words for a person.
        let node = |target: &str, wanted: &str| -> Result<(Value, String, String)> {
            let frame = frame.ok_or_else(|| fail("STALE_TARGET", "There is no current page observation; observe with surface \"tab\" and view \"elements\".", true))?;
            let node = frame
                .browser
                .iter()
                .find(|n| n.id == target)
                .ok_or_else(|| fail("STALE_TARGET", format!("action.target: {} is not in the latest page observation", clip(target, 40)), true))?;
            if node.states.iter().any(|s| matches!(s.as_str(), "ambiguous" | "context_truncated")) {
                return Err(fail("AMBIGUOUS_TARGET", format!("{target} is not distinguishable from another element; observe with a query."), true));
            }
            if !node.actions.iter().any(|a| a == wanted) {
                if node.states.iter().any(|s| s == "disabled") {
                    return Err(fail("INVALID_ARGUMENT", format!("action.target: {target} is disabled."), true)
                        .with("next", "Wait for it to be enabled (wait_for), or choose another element."));
                }
                let takes = if node.actions.is_empty() { "nothing".to_string() } else { node.actions.join(", ") };
                let hint = if node.actions.iter().any(|a| a == "type") {
                    format!(" To fill it in, send {{kind: \"type\", target: \"{target}\", text: …}}: that replaces its text, no click needed.")
                } else {
                    String::new()
                };
                return Err(fail("INVALID_ARGUMENT", format!("action.target: {target} ({}) does not take {wanted}; it takes {takes}.", node.role), true)
                    .with("next", format!("Use one of the actions {target} takes: {takes}.{hint}")));
            }
            let args = json!({ "tabId": node.tab_id, "documentId": node.document_id, "capture": node.capture, "token": node.token });
            Ok((args, format!("{} \"{}\"", node.role, squash(&node.name, 40)), approval::element_words(&node.role, &node.name)))
        };
        let class = stricter("change", effect);
        let (target, op, describe, said, route) = match action {
            BrowserAction::Navigate(n) => {
                let url = n.url.trim();
                let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
                if !matches!(scheme.as_deref(), Some("http" | "https")) || url.len() > 4096 {
                    return Err(invalid("action.url: an http or https address of at most 4096 characters.").with("execution_not_started", true));
                }
                let said = format!("go to {}", approval::page_words(url).unwrap_or_else(|| "a web page".into()));
                (None, BrowserOp::Navigate(url.to_string()), format!("open {}", clip(url, 80)), said, "browser_navigate")
            }
            BrowserAction::Click(t) => {
                let (args, what, words) = node(&t.target, "click")?;
                (Some(args), BrowserOp::Click, format!("click {what}"), format!("click {words}"), "browser_click")
            }
            BrowserAction::Type(t) => {
                crate::desktop::input::check_text(&t.text)?;
                let typed = approval::characters(t.text.chars().count());
                match &t.target {
                    Some(target) => {
                        let (args, what, words) = node(target, "fill")?;
                        (Some(args), BrowserOp::Type(t.text.clone()), format!("type into {what}"), format!("type {typed} into {words}"), "browser_type")
                    }
                    None if !t.text.is_ascii() => {
                        return Err(invalid("action.target: text with characters beyond ASCII is pasted into a field, so name the field from the page's elements.")
                            .with("execution_not_started", true));
                    }
                    None => (None, BrowserOp::Type(t.text.clone()), "type in the page".into(), format!("type {typed}"), "browser_type"),
                }
            }
            BrowserAction::Select(s) => {
                let (args, what, words) = node(&s.target, "select")?;
                let said = format!("choose “{}” in {words}", squash(&s.value, 40));
                (Some(args), BrowserOp::Select(s.value.clone()), format!("choose \"{}\" in {what}", squash(&s.value, 40)), said, "browser_select")
            }
            BrowserAction::Scroll(s) => {
                if !(-50..=50).contains(&s.dx) || !(-50..=50).contains(&s.dy) {
                    return Err(invalid("Scroll notches must be between -50 and 50.").with("field", "action.dx/dy").with("execution_not_started", true));
                }
                let target = match &s.target {
                    Some(target) => {
                        let frame = frame.ok_or_else(|| fail("STALE_TARGET", "There is no current page observation.", true))?;
                        let node = frame.browser.iter().find(|n| &n.id == target).ok_or_else(|| {
                            fail("STALE_TARGET", format!("action.target: {} is not in the latest page observation", clip(target, 40)), true)
                        })?;
                        Some(json!({ "tabId": node.tab_id, "documentId": node.document_id, "capture": node.capture, "token": node.token }))
                    }
                    None => None,
                };
                (target, BrowserOp::Scroll { dx: s.dx, dy: s.dy }, "scroll the page".into(), "scroll the page".into(), "browser_scroll")
            }
            BrowserAction::Key(k) => {
                crate::desktop::input::cua_keys(&k.keys)?;
                (None, BrowserOp::Key(k.keys.clone()), format!("press {}", clip(&k.keys, 40)), format!("press {}", approval::key_words(&k.keys)), "browser_key")
            }
            BrowserAction::WaitFor(_) => unreachable!("handled above"),
        };
        Ok(Resolved { plan: Planned::Browser(Box::new(BrowserStep { window, target, op })), describe, said, class, route })
    }

    /// Run a browser step. Only the extension's page reads happen before the
    /// first input, so a refusal there is `execution_not_started`; after the
    /// first input nothing is, except typing refused because the clicked
    /// field did not take the keyboard focus (nothing typed; `clicked`).
    async fn browser_step(&self, step: &BrowserStep, cancel: &Cancel) -> Result<Done> {
        // A browser that has just started connects its extension within a
        // few seconds of opening its first window.
        let connecting = Instant::now();
        while !self.desktop.browser_connected() {
            if connecting.elapsed() >= BROWSER_CONNECT || cancel.is_cancelled() {
                return Err(unavailable("The browser extension is not connected on this computer; use computer_act on the browser window.")
                    .with("execution_not_started", true));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let started = |e: IbaraError| e.with("execution_not_started", false);
        let act = |effect: Effect| async move { self.send_input(&effect, cancel).await };
        let surface = || step.window.clone();
        match &step.op {
            BrowserOp::Navigate(url) => {
                let data = self.desktop.browser_call("navigate", json!({ "url": url }), true).await?;
                if data.get("refused").and_then(Value::as_bool) == Some(true) {
                    return Err(fail("STALE_TARGET", "The tab changed before navigating; observe again.", true).with("execution_not_started", true));
                }
                Ok(Done::default())
            }
            BrowserOp::Click => self.browser_click(step, step.target.clone().unwrap_or(Value::Null), true, cancel).await.map(|(done, _)| done),
            BrowserOp::Type(text) => match &step.target {
                Some(target) => {
                    self.browser_click(step, target.clone(), false, cancel).await?;
                    self.browser_type(step, target, text, cancel).await
                }
                None => act(Effect::Type { surface: surface(), text: text.clone(), cursor: TypingCursor::Stays })
                    .await
                    .map_err(|e| typing_stopped(e, 0, text.chars().count(), TypedInto::Window)),
            },
            BrowserOp::Select(value) => {
                // As a person does it: open the list with a click, type the
                // option's label, confirm with Return.
                let mut args = step.target.clone().unwrap_or(Value::Null);
                args["value"] = json!(value);
                let (_, located) = self.browser_click(step, args, false, cancel).await?;
                let label = located.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                act(Effect::Type { surface: surface(), text: label.clone(), cursor: TypingCursor::Stays })
                    .await
                    .map_err(|e| typing_stopped(e, 0, label.chars().count(), TypedInto::Field))?;
                act(Effect::Key { surface: surface(), combo: "Return".into() }).await.map_err(started)?;
                let chosen = self.desktop.browser_call("selected", step.target.clone().unwrap_or(Value::Null), false).await.map_err(started)?;
                let now = chosen.get("text").and_then(Value::as_str).unwrap_or_default();
                if now != label {
                    return Err(fail("OUTCOME_UNKNOWN", format!("The list now shows \"{}\", not \"{}\"; observe the page.", clip(now, 60), clip(&label, 60)), false));
                }
                Ok(Done::default())
            }
            BrowserOp::Scroll { dx, dy } => {
                let mut moved = false;
                if let Some(target) = &step.target {
                    let data = self.desktop.browser_call("reveal", target.clone(), true).await?;
                    page_refusal(&data)?;
                    moved = true;
                }
                if *dx != 0 || *dy != 0 {
                    let wheel = act(Effect::Scroll { at: None, dx: *dx, dy: *dy }).await;
                    return if moved { wheel.map_err(started) } else { wheel };
                }
                Ok(Done::default())
            }
            BrowserOp::Key(combo) => act(Effect::Key { surface: surface(), combo: combo.clone() }).await,
        }
    }

    /// Click a page element with Cua's real pointer: the extension scrolls it
    /// into view, waits for it to stop moving and returns an uncovered point
    /// in it; ibara maps that point to the window; after the click the
    /// extension says which element the trusted press reached. Only a press
    /// the page saw on the chosen element counts as done. With `navigation`
    /// (a plain click) the extension also says where the page went when the
    /// press, or the form it submitted, made it leave.
    async fn browser_click(&self, step: &BrowserStep, args: Value, navigation: bool, cancel: &Cancel) -> Result<(Done, Value)> {
        let mut target = step.target.clone().unwrap_or(Value::Null);
        let data = self.desktop.browser_call("locate", args, false).await.map_err(|e| e.with("execution_not_started", true))?;
        page_refusal(&data)?;
        if data.get("label").is_some_and(|l| l.as_str() == Some("")) {
            let why = data.get("reason").and_then(Value::as_str).unwrap_or("no option has that value or label");
            return Err(fail("AMBIGUOUS_TARGET", format!("That choice cannot be made by typing: {why}."), true)
                .with("options", data.get("options").cloned().unwrap_or(Value::Null))
                .with("execution_not_started", true));
        }
        let win = self
            .desktop
            .windows()
            .await
            .map_err(|e| e.with("execution_not_started", true))?
            .into_iter()
            .find(|w| w.address == step.window.address && w.pid == step.window.pid)
            .filter(|w| w.focused)
            .ok_or_else(|| fail("STALE_TARGET", "The browser window closed or lost focus; observe again.", true).with("execution_not_started", true))?;
        let (x, y) = page_point(&data, &win.rect)?;
        let click = Effect::ClickPoint { x, y, surface: Some(step.window.clone()), button: Button::Left, double: false };
        let done = self.send_input(&click, cancel).await?;
        // The page reports the press over a connection it opened before the
        // click, so a press that navigates the page away (a link, a form's
        // submit button) is still reported. A press the page never saw went
        // somewhere else: the browser's own controls, developer tools, a
        // frame or another window.
        target["navigation"] = json!(navigation);
        let seen = self.desktop.browser_call("verify", target, false).await.map_err(|e| {
            fail("OUTCOME_UNKNOWN", "ibara could not check which element the click reached; observe the page.", false).with("detail", &e.message)
        })?;
        match (seen.get("observed").and_then(Value::as_bool), seen.get("hit").and_then(Value::as_bool)) {
            (Some(true), Some(true)) => {
                let note = seen.get("navigated").and_then(Value::as_str).map(|url| {
                    let went = if seen.get("arrived").and_then(Value::as_bool) == Some(false) { "began loading" } else { "went to" };
                    format!("the click reached it; the page then {went} {}", page_address(url))
                });
                Ok((Done { note, ..done }, data))
            }
            (Some(true), _) => Err(fail("OUTCOME_UNKNOWN", "The click reached a different element than the one chosen; observe the page.", false)),
            _ if seen.get("gone").and_then(Value::as_bool) == Some(true) => {
                Err(fail("OUTCOME_UNKNOWN", "The page went away before it reported where the click landed; observe the page.", false))
            }
            _ => Err(fail(
                "OUTCOME_UNKNOWN",
                "The page did not receive the click; it may have landed in the browser's own controls, developer tools or another window. Observe the page.",
                false,
            )),
        }
    }

    /// Replace the text of the field just clicked, once the page reader says
    /// that field has the keyboard focus: text is never aimed at the address
    /// bar or wherever else the focus is. ASCII is typed with Cua's keyboard.
    /// Other characters, which Cua cannot type, are pasted: the person's
    /// clipboard is set aside first (every type) and watched, holds each such
    /// piece for one Ctrl+V whose arrival the page reader confirms, and is put
    /// back afterwards. A copy someone makes meanwhile is kept: nothing more is
    /// pasted over it and the old clipboard does not replace it. What happened
    /// to the clipboard, when the person needs to know, is part of the step's
    /// account whatever its outcome. Typed text reaches no message, receipt or
    /// timeline entry.
    async fn browser_type(&self, step: &BrowserStep, target: &Value, text: &str, cancel: &Cancel) -> Result<Done> {
        let focused = self.desktop.browser_call("field", target.clone(), false).await.ok().and_then(|f| f.get("focused").and_then(Value::as_bool));
        if focused != Some(true) {
            return Err(fail("CAPABILITY_UNAVAILABLE", "The click reached that element, but it did not take the keyboard focus, so nothing was typed.", true)
                .with("next", "Observe the page and choose the field that takes typing (a text box, not its label or a box around it).")
                .with("clicked", true)
                .with("execution_not_started", true));
        }
        if text.is_ascii() {
            return self.replace_text(step, target, text, cancel).await;
        }
        self.desktop.clipboard_set_aside().await.map_err(|e| {
            fail("CAPABILITY_UNAVAILABLE", format!("ibara pastes characters beyond ASCII and could not set the clipboard aside first, so nothing was typed. {}", e.message), true)
                .with("next", "Try again; if it keeps failing, type ASCII text only.")
                .with("clicked", true)
                .with("execution_not_started", true)
        })?;
        let typed = self.replace_text(step, target, text, cancel).await;
        let note = match self.desktop.clipboard_put_back().await {
            Ok(PutBack::Unchanged | PutBack::Restored) => None,
            // The refusal to paste over the copy already says so.
            Ok(PutBack::Copied) if typed.as_ref().is_err_and(|e| e.details.get("reason") == Some(&json!("clipboard_copied"))) => None,
            Ok(PutBack::Copied) => Some("something else was copied while ibara typed, and it stays on the clipboard".to_string()),
            Ok(PutBack::Emptied) => Some("the clipboard held a password, so ibara left it empty instead of putting the password back".to_string()),
            Err(e) => {
                super::log_event("clipboard_put_back_failed", &e.message);
                let said = e.message.trim_end_matches('.');
                Some(match said.strip_prefix("The clipboard could not be put back") {
                    Some(why) => format!("the clipboard could not be put back{why}"),
                    None => format!("the clipboard could not be put back: {said}"),
                })
            }
        };
        match (typed, note) {
            (Ok(done), note) => Ok(Done { note, ..done }),
            // First in the message, so the step, its receipt, its status and
            // the timeline all say it, however short they cut it.
            (Err(mut e), Some(note)) => {
                let mut note = note.chars();
                let first = note.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
                e.message = format!("{first}{}. {}", note.as_str(), e.message);
                Err(e)
            }
            (Err(e), None) => Err(e),
        }
    }

    /// Send one input unless control changed hands meanwhile (a pause, a
    /// takeover, a revoke): a step stops before its next input, never after.
    async fn send_input(&self, effect: &Effect, cancel: &Cancel) -> Result<Done> {
        crate::desktop::cua::unless_cancelled(Some(cancel)).map_err(|e| e.with("no_input_held", true))?;
        self.desktop.act(effect, cancel).await
    }

    /// Select the focused field's text and type `text` over it, piece by
    /// piece: ASCII typed, the rest pasted from the clipboard. Each paste is
    /// confirmed in the field before anything else is sent, so the pieces
    /// arrive in order. After the click, a stop says how much of the text
    /// went through.
    async fn replace_text(&self, step: &BrowserStep, target: &Value, text: &str, cancel: &Cancel) -> Result<Done> {
        let total = text.chars().count();
        let stopped = |before: usize| move |e: IbaraError| typing_stopped(e, before, total, TypedInto::Field);
        let act = |effect: Effect| async move { self.send_input(&effect, cancel).await };
        let key = |combo: &str| Effect::Key { surface: step.window.clone(), combo: combo.into() };
        act(key("ctrl+a")).await.map_err(stopped(0))?;
        if text.is_empty() {
            return act(key("BackSpace")).await.map_err(|e| e.with("execution_not_started", false));
        }
        let runs = text_runs(text);
        let mut shown = String::with_capacity(text.len());
        for (i, &(piece, ascii)) in runs.iter().enumerate() {
            let before = shown.chars().count();
            if cancel.is_cancelled() {
                return Err(stopped(before)(IbaraError::new("TIMEOUT", "Typing cancelled between pieces.", false).with("no_input_held", true)));
            }
            shown.push_str(piece);
            if ascii {
                act(Effect::Type { surface: step.window.clone(), text: piece.to_string(), cursor: TypingCursor::Stays }).await.map_err(stopped(before))?;
                // After a paste the last typed piece is checked too.
                if i + 1 == runs.len() && runs.len() > 1 {
                    self.field_shows(target, &shown).await?;
                }
                continue;
            }
            self.desktop.clipboard_paste_text(piece).await.map_err(stopped(before))?;
            // A Ctrl+V that went out may have pasted the piece.
            act(key("ctrl+v")).await.map_err(|e| {
                let unsure = if not_started(&e) { 0 } else { piece.chars().count() };
                stopped(before)(e.with("unsure_chars", unsure))
            })?;
            self.field_shows(target, &shown).await?;
        }
        Ok(Done::default())
    }

    /// Wait until the page reader says the field, which still has the
    /// keyboard focus, holds `expected`. The reader compares; the field's
    /// value never leaves the page.
    async fn field_shows(&self, target: &Value, expected: &str) -> Result<()> {
        let until = Instant::now() + PASTE_WITHIN;
        let mut ask = target.clone();
        ask["expected"] = json!(expected);
        loop {
            let field = self.desktop.browser_call("field", ask.clone(), false).await.unwrap_or(Value::Null);
            let focused = field.get("focused").and_then(Value::as_bool);
            if focused == Some(true) && field.get("matches").and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            if focused == Some(false) {
                return Err(fail("OUTCOME_UNKNOWN", "The field lost the keyboard focus while ibara typed into it; observe the page.", false));
            }
            if Instant::now() >= until {
                return Err(fail("OUTCOME_UNKNOWN", "The field does not show the text ibara typed into it; observe the page.", false));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn dispatch_planned(&self, ctx: &CallCtx<'_>, resolved: &Resolved) -> Result<Done> {
        self.assert_authority(ctx.lease).map_err(|e| e.with("execution_not_started", true))?;
        match &resolved.plan {
            Planned::Observe => Ok(Done::default()),
            Planned::Desktop(effect) => {
                let (cancel, _) = self.abort_handles();
                self.mark_effect(Some(ctx.task_ref));
                let done = self.desktop.act(effect, &cancel).await;
                self.mark_effect(None);
                let done = match effect.as_ref() {
                    Effect::Type { text, .. } => done.map_err(|e| typing_stopped(e, 0, text.chars().count(), TypedInto::Window)),
                    _ => done,
                };
                if let (Ok(Done { pid: Some(pid), .. }), Effect::Launch { .. }) = (&done, effect.as_ref()) {
                    self.push_event(Some(ctx.task_ref), &format!("launched pid {pid}"));
                }
                done
            }
            Planned::Browser(step) => {
                let (cancel, _) = self.abort_handles();
                self.mark_effect(Some(ctx.task_ref));
                let done = self.browser_step(step, &cancel).await;
                self.mark_effect(None);
                done
            }
        }
    }

    /// A launch returns once its program runs; the program's window maps
    /// later. Wait for it, up to `LAUNCH_WINDOW_WAIT`, so it becomes the
    /// task's (finishing closes it) and the frame shows it. Window events
    /// until it maps are the launch's, not a person's.
    async fn await_launched_window(&self, task_ref: &str, before: &[Win], resolved: &Resolved, launched: Option<u32>, gone: &Cancel) {
        // A launch with neither a pid nor a known class has no window to look for.
        let owner = match step_owner(resolved, None, launched) {
            Some(Owner::Launch { pid: None, class: None }) | Some(Owner::Process(_)) | None => return,
            Some(owner) => owner,
        };
        let started = Instant::now();
        self.mark_effect(Some(task_ref));
        loop {
            let mapped = self.desktop.windows().await.is_ok_and(|now| now.iter().any(|w| self.claims(&owner, before, w)));
            if mapped || started.elapsed() >= LAUNCH_WINDOW_WAIT || pause(LAUNCH_WINDOW_POLL, gone).await {
                break;
            }
        }
        self.mark_effect(None);
    }

    /// Windows that appeared during a step and belong to it are the task's
    /// (for cleanup). Windows a person opened meanwhile are left alone.
    async fn track_new_windows(&self, task_ref: &str, before: &[Win], resolved: &Resolved, focused_before: Option<&Win>, launched: Option<u32>) {
        let Some(owner) = step_owner(resolved, focused_before, launched) else { return };
        let Ok(after) = self.desktop.windows().await else { return };
        let fresh: Vec<&Win> = after.iter().filter(|w| self.claims(&owner, before, w)).collect();
        for w in &fresh {
            self.record_window(self.journal.own_window(task_ref, &w.address, w.pid, &w.class, &w.title));
        }
        if let Planned::Desktop(effect) = &resolved.plan
            && let Effect::Key { surface, combo } = effect.as_ref()
            && let Some(dialog) = fresh.iter().find(|w| w.floating)
        {
            let app = surface.class.to_lowercase();
            let opens = format!("dialog \"{}\"", squash(&dialog.title, 40));
            let fact = json!({ "keys": combo, "opens": opens, "text": format!("{combo} opens {opens}"), "worked": true });
            let now = self.now_iso();
            let _ = self.journal.record_app_note(&app, "", "main window", "shortcut", &fact, &now);
        }
    }

    /// `w` appeared during the step (it is not in `before`) and is `owner`'s.
    fn claims(&self, owner: &Owner, before: &[Win], w: &Win) -> bool {
        !before.iter().any(|b| b.address == w.address)
            && match owner {
                Owner::Process(pid) => w.pid == *pid,
                // While the launched program runs, only its process tree: a
                // person's own window of the same app is not the task's. A
                // launcher that handed the app to another process and ended
                // (the browser) leaves the app's class to go by.
                Owner::Launch { pid: Some(root), .. } if self.desktop.parent_pid(*root).is_some() => self.descends_from(w.pid, *root),
                Owner::Launch { pid, class } => {
                    class.is_some_and(|c| w.class.to_lowercase().contains(c)) || pid.is_some_and(|root| self.descends_from(w.pid, root))
                }
            }
    }

    /// `pid` is `root` or one of its descendants (at most 16 generations).
    fn descends_from(&self, pid: i64, root: i64) -> bool {
        let mut current = pid;
        for _ in 0..16 {
            if current == root {
                return true;
            }
            match self.desktop.parent_pid(current) {
                Some(parent) if parent > 1 && parent != current => current = parent,
                _ => return false,
            }
        }
        false
    }

    /// Record whether a route worked on this app surface. A refusal is kept
    /// only when it describes the app (`about: app`), never one element or
    /// one failed step: every later agent reads the app's notes.
    fn note_route(&self, focused: Option<&Win>, frame: Option<&FrameState>, resolved: &Resolved, refused: Option<&IbaraError>) {
        let Some(win) = focused else { return };
        if matches!(resolved.plan, Planned::Observe) || refused.is_some_and(|e| e.details.get("about") != Some(&json!("app"))) {
            return;
        }
        let app = win.class.to_lowercase();
        let version = self.desktop.app_version(win.pid).unwrap_or_default();
        let elements = frame.map(|f| f.elements.as_slice()).unwrap_or(&[]);
        let signature = surface_signature(win, elements);
        let route = resolved.route.replace('_', " ");
        let fact = match refused {
            None => json!({ "text": format!("{route} worked"), "worked": true, "route": resolved.route }),
            Some(e) => json!({ "text": format!("{route} refused: {}", squash(&e.message, 80)), "worked": false, "route": resolved.route, "about": "app" }),
        };
        let now = self.now_iso();
        if let Err(e) = self.journal.record_app_note(&app, &version, &signature, &format!("route:{}", resolved.route), &fact, &now) {
            super::log_event("app_note_failed", &e.to_string());
        }
    }

    // ---- computer_exec --------------------------------------------------------------

    async fn exec(&self, principal: &str, connection_id: &str, args: &Value, input: ExecInput) -> Result<Reply> {
        let task_ref = input.task_ref.to_string();
        let request_id = input.request_id.to_string();
        let lease = self.require_live_lease(principal, connection_id, &task_ref)?;
        let cwd = self.task_path(&task_ref, "cwd", input.cwd.as_deref().unwrap_or("."))?;
        let mut task = self.task(&task_ref)?;
        self.charge(&mut task, Charge::Action)?;
        let mut class = class_name(input.effect);
        let ctx = CallCtx { principal, task_ref: &task_ref, request_id: &request_id, tool: "computer_exec", lease: &lease, gone: &Cancel::new() };
        let describe = format!("run {}", serde_json::to_value(&input.command).unwrap_or_default());
        let mut ask = Ask { doing: format!("run the command “{}”", approval::command_words(&input.command)), place: None, class, says_why: false, changed: false, again: false };
        let asked = json!({ "command": input.command, "cwd": input.cwd });
        let mut op = match self.remember(&ctx, &request_id, &fingerprint_source(args), class)? {
            Remembered::Fresh(op) => op,
            Remembered::Replay(op) | Remembered::FailedBeginReplay { operation: op, .. } => match self.resume_single(&ctx, &op)? {
                Resume::Stored(reply) => return reply,
                Resume::Run => op,
            },
        };
        // A command held before is the same action whatever effect is declared
        // now, and whichever way its folder is named.
        let action = json!({ "exec": &input.command, "cwd": { "base": cwd.base, "rel": guard::folder(&cwd.rel) } });
        match self.approval_guard(&task_ref, &op.operation_ref, &action)? {
            Guard::Refuse(e) => {
                let mut receipt = op.receipt.as_object().cloned().unwrap_or_default();
                return self.refuse_call(&ctx, &op, &mut receipt, e);
            }
            Guard::AskAgain(held, refused) => {
                class = guard::stricter_class(class, held);
                ask.class = class;
                ask.again = refused;
            }
            Guard::Clear => {}
        }
        op.receipt["action"] = action;
        let request = json!({
            "command": input.command,
            "cwd": cwd.rel,
            "timeout_ms": input.timeout_ms,
            "background": input.background,
            "request_id": request_id,
        });
        let effected = self
            .single_effect(&ctx, op, &describe, &ask, asked, |mut storage_ctx| async move {
                storage_ctx.base_dir = cwd.base;
                let result = self.storage.exec(&request, &storage_ctx).await?;
                Ok(result.records.into_iter().find(|r| r.get("kind").and_then(Value::as_str) == Some("job")).unwrap_or(Value::Null))
            })
            .await?;
        let (op_ref, job) = match effected {
            Effected::Held(reply) => return Ok(reply),
            Effected::Done { op_ref, value } => (op_ref, value),
        };
        let running = job.get("state").and_then(Value::as_str) == Some("running");
        let status = if running { Status::Pending } else { Status::Ok };
        let reply = Reply::with_status(status, json!({ "op_ref": op_ref, "effect_class": class, "job": job }), &task_ref);
        self.store_reply(&op_ref, Ok(reply))
    }

    /// Store a call's final outcome on its operation (for replay) and return it.
    fn store_reply(&self, op_ref: &str, outcome: Result<Reply>) -> Result<Reply> {
        let mut receipt = self.journal.get_operation_by_ref(op_ref)?.and_then(|o| o.receipt.as_object().cloned()).unwrap_or_default();
        self.store_call(op_ref, &mut receipt, &outcome, None)?;
        outcome
    }

    /// Outcome of repeating a single-effect call. Only a held operation that
    /// never started may run now; anything dispatched is never run again.
    fn resume_single(&self, ctx: &CallCtx<'_>, op: &OperationRecord) -> Result<Resume> {
        let interrupted = || {
            Err(fail("OUTCOME_UNKNOWN", "This request was interrupted before its result was stored; check it with computer_status and never repeat it blindly.", false)
                .requires_reconciliation()
                .with("op_ref", &op.operation_ref))
        };
        let execution = op.receipt.get("execution").and_then(Value::as_str);
        let held = op.receipt.get("call").and_then(|c| c.get("held")).and_then(|h| h.get("attention")).and_then(Value::as_str).map(str::to_string);
        let Some(att) = held.filter(|_| execution == Some("not_started")) else {
            let stored = op.receipt.get("call").filter(|c| c.get("held").is_none()).and_then(|_| self.stored_call(op, ctx.task_ref));
            return Ok(Resume::Stored(stored.unwrap_or_else(interrupted)));
        };
        let (decision, why) = self.approval(&att, ctx.lease)?;
        match decision {
            None => Ok(Resume::Stored(self.stored_call(op, ctx.task_ref).unwrap_or_else(|| Err(crate::error::internal("held call without a stored result"))))),
            Some(true) => Ok(Resume::Run),
            Some(false) => {
                let e = denied(format!("Not run: {why}.")).with("attention", &att);
                let mut receipt = op.receipt.as_object().cloned().unwrap_or_default();
                receipt.remove("held");
                receipt.insert("error".into(), e.to_json());
                let outcome = Err(e);
                self.store_call(&op.operation_ref, &mut receipt, &outcome, None)?;
                Ok(Resume::Stored(outcome))
            }
        }
    }

    /// One journalled effect through storage, with the effect-class rule
    /// (`ask.class`). A held operation that is run again was approved
    /// (`resume_single`). `ask` and `asked` are what a person reads and the
    /// request behind it, should it be held.
    async fn single_effect<F, Fut>(&self, ctx: &CallCtx<'_>, op: OperationRecord, describe: &str, ask: &Ask, asked: Value, run: F) -> Result<Effected>
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = Result<Value>>,
    {
        let class = ask.class;
        let op_ref = op.operation_ref.clone();
        let mut receipt = op.receipt.as_object().cloned().unwrap_or_default();
        receipt.insert("operation_ref".into(), json!(op_ref));
        receipt.insert("effect_class".into(), json!(class));
        let approved = receipt.contains_key("held");
        match self.effect_rule(ctx.task_ref,ctx.principal,class).max(if ctx.tool=="computer_exec" {self.effect_rule(ctx.task_ref,ctx.principal,"change")} else {Rule::Allow}).max(if ask.again || receipt.get("target_changed")==Some(&json!(true)) {Rule::Ask}else{Rule::Allow}) {
            Rule::Deny => {
                let e = denied(format!("{class} steps are not allowed on this computer.")).with("effect_class", class);
                receipt.insert("error".into(), e.to_json());
                let outcome: Result<Reply> = Err(e.clone());
                self.store_call(&op_ref, &mut receipt, &outcome, None)?;
                return Err(e);
            }
            Rule::Ask if !approved => {
                let (summary, details) = self.approval_words(ctx, ask, asked);
                let att = self.hold(ctx, &op_ref, describe, &summary, &details)?;
                receipt.insert("held".into(), json!({ "attention": att, "index": 0 }));
                receipt.insert("summary".into(), json!(format!("Held for approval ({att}).")));
                let reply = Reply::with_status(
                    Status::Pending,
                    json!({ "op_ref": op_ref, "effect_class": class, "attention": att, "held": describe, "next": approval_next(ctx.tool, Some(ctx.task_ref), &att) }),
                    ctx.task_ref,
                );
                let outcome = Ok(reply);
                self.store_call(&op_ref, &mut receipt, &outcome, Some((0, &att)))?;
                return outcome.map(Effected::Held);
            }
            _ => {}
        }
        receipt.remove("held");
        receipt.remove("target_changed");
        receipt.insert("execution".into(), json!("running"));
        receipt.insert("effect".into(), json!("unknown"));
        receipt.insert("summary".into(), json!(format!("{} dispatched.", ctx.tool)));
        self.save_receipt(&op_ref, &receipt, Some(true))?;
        let (_, signal) = self.abort_handles();
        let mut storage_ctx = Context::new(ctx.task_ref, ctx.principal, &self.epoch);
        storage_ctx.request_id = Some(ctx.request_id.to_string());
        storage_ctx.operation_ref = Some(op_ref.clone());
        storage_ctx.signal = Some(signal);
        storage_ctx.authority = Some(self.reader.authority(ctx.principal, ctx.task_ref, &ctx.lease.generation, &ctx.lease.connection_id));
        self.note_step(ctx.task_ref, &ask.doing);
        self.mark_effect(Some(ctx.task_ref));
        let started = Instant::now();
        let result = run(storage_ctx).await;
        self.mark_effect(None);
        receipt.insert("elapsed_ms".into(), json!(started.elapsed().as_millis() as u64));
        match result {
            Ok(value) => {
                let running = value.get("state").and_then(Value::as_str) == Some("running");
                receipt.insert("execution".into(), json!(if running { "running" } else { "completed" }));
                receipt.insert("verification".into(), json!("not_requested"));
                receipt.insert("effect".into(), json!(if class == "observe" { "read" } else { "local_change" }));
                receipt.insert("summary".into(), json!(clip(describe, 2000)));
                if let Some(job_ref) = value.get("job_ref").and_then(Value::as_str) {
                    receipt.insert("job_ref".into(), json!(job_ref));
                }
                self.save_receipt(&op_ref, &receipt, Some(true))?;
                self.timeline("step", Some(ctx.task_ref), ctx.principal, &clip(describe, 200), json!({ "op": op_ref, "effect_class": class }));
                Ok(Effected::Done { op_ref, value })
            }
            Err(e) if not_started(&e) || e.retry_safe && matches!(e.code, "INVALID_ARGUMENT" | "REQUEST_CONFLICT" | "PERMISSION_DENIED" | "WRONG_TOOL" | "BUDGET_EXCEEDED") => {
                receipt.insert("execution".into(), json!("not_started"));
                receipt.insert("effect".into(), json!("none"));
                receipt.insert("summary".into(), json!("No effect was dispatched."));
                receipt.insert("error".into(), e.to_json());
                let outcome: Result<Reply> = Err(e.clone());
                let mut patch = OperationPatch::at(self.now_iso());
                patch.dispatched = Some(false);
                self.journal.update_operation(&op_ref, patch)?;
                self.store_call(&op_ref, &mut receipt, &outcome, None)?;
                Err(e)
            }
            Err(e) => {
                receipt.insert("execution".into(), json!("unknown"));
                receipt.insert("verification".into(), json!("unknown"));
                receipt.insert("effect".into(), json!("unknown"));
                receipt.insert("summary".into(), json!("Dispatch started; ibara cannot confirm whether the effect completed."));
                let unknown = fail("OUTCOME_UNKNOWN", clip(&e.message, 1000), false).requires_reconciliation().with("op_ref", &op_ref);
                receipt.insert("error".into(), unknown.to_json());
                let outcome: Result<Reply> = Err(unknown.clone());
                self.store_call(&op_ref, &mut receipt, &outcome, None)?;
                Err(unknown)
            }
        }
    }

    // ---- computer_files -------------------------------------------------------------

    async fn files(&self, principal: &str, connection_id: &str, args: &Value, input: FilesInput) -> Result<Reply> {
        let task_ref = input.task_ref.to_string();
        let request_id = input.request_id.to_string();
        let lease = self.require_live_lease(principal, connection_id, &task_ref)?;
        let place = match &input.op {
            FilesOp::List(l) => self.task_path(&task_ref, "dir", l.dir.as_deref().unwrap_or("."))?,
            FilesOp::Read(r) => self.task_path(&task_ref, "path", &r.path)?,
            FilesOp::Write(w) => self.task_path(&task_ref, "path", &w.path)?,
            FilesOp::Publish(p) => self.task_path(&task_ref, "path", &p.path)?,
            FilesOp::Send(s) => self.task_path(&task_ref, "path", &s.path)?,
            FilesOp::Status(_) => TaskPath { base: None, rel: ".".into() },
        };
        let mut task = self.task(&task_ref)?;
        self.charge(&mut task, Charge::Control)?;
        let read_ctx = || {
            let mut c = Context::new(&task_ref, principal, &self.epoch);
            c.authority = Some(self.reader.authority(principal, &task_ref, &lease.generation, connection_id));
            c.base_dir = place.base.clone();
            c
        };
        match &input.op {
            FilesOp::List(l) => {
                let dir = l.dir.clone().unwrap_or_else(|| ".".into());
                let result = self.storage.files(&json!({ "kind": "list", "path": place.rel }), &read_ctx())?;
                let text = observation_text(&result.records);
                let mut entries = Vec::new();
                for line in text.lines().filter(|l| !l.is_empty() && *l != "(empty directory)") {
                    if let Some(name) = line.strip_suffix('/') {
                        entries.push(FileEntry { name: name.to_string(), kind: EntryKind::Dir, size: 0 });
                    } else if let Some((name, size)) = line.rsplit_once(' ')
                        && let Ok(size) = size.parse::<u64>()
                    {
                        entries.push(FileEntry { name: name.to_string(), kind: EntryKind::File, size });
                    }
                }
                let truncated = result.records.iter().any(|r| r.get("coverage").and_then(|c| c.get("continuation")).is_some_and(|c| !c.is_null()));
                return Ok(Reply::ok(FilesResult::List { dir, entries, truncated }, Some(&task_ref)));
            }
            FilesOp::Read(r) => {
                let max_chars = r.max_bytes.map(|b| b.clamp(1, 12_000));
                let result = self.storage.files(&json!({ "kind": "read", "path": place.rel, "max_chars": max_chars }), &read_ctx())?;
                let text = observation_text(&result.records);
                let size = self.resolve_path(&task_ref, &r.path).ok().and_then(|p| std::fs::metadata(p).ok()).map_or(text.len() as u64, |m| m.len());
                let truncated = (text.len() as u64) < size;
                return Ok(Reply::ok(FilesResult::Read { path: r.path.clone(), size, text: Some(text), base64: None, truncated }, Some(&task_ref)));
            }
            FilesOp::Status(_) => {
                let artifacts = self.storage.artifacts(&task_ref, principal)?;
                let items = artifacts
                    .iter()
                    .map(|a| {
                        let art_ref = a.get("artifact_ref").and_then(Value::as_str).unwrap_or("").to_string();
                        let delivered_to = task
                            .deliveries
                            .iter()
                            .filter(|d| d.get("artifact_ref").and_then(Value::as_str) == Some(art_ref.as_str()) && self.storage.delivery_verified(&task_ref, d))
                            .filter_map(|d| {
                                Some(contract::Destination {
                                    host: d.get("host_id")?.as_str()?.to_string(),
                                    path: d.get("destination_path")?.as_str()?.to_string(),
                                })
                            })
                            .collect();
                        contract::ArtifactStatus {
                            art_ref,
                            path: a.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                            sha256: a.get("sha256").and_then(Value::as_str).unwrap_or("").to_string(),
                            size: a.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                            delivered_to,
                        }
                    })
                    .collect();
                return Ok(Reply::ok(FilesResult::Status { artifacts: items }, Some(&task_ref)));
            }
            _ => {}
        }
        // A send is checked whole before a person is asked about it: the file,
        // where it goes, and the computer it goes to. A request already
        // recorded replays as it was, even once its file is gone.
        let mut input = input;
        if let FilesOp::Send(s) = &mut input.op {
            let recorded = self.journal.get_operation_by_request_id(&request_id, Some(&task_ref))?.is_some();
            if !recorded && !self.resolve_path(&task_ref, &s.path).is_ok_and(|p| p.is_file()) {
                return Err(invalid(format!("path: '{}' is not a file in the task's workspace or the home folder", clip(&s.path, 80))).with("execution_not_started", true));
            }
            super::checks::destination_path("to.path", &s.to.path)?;
            s.to.host = self.delivery_host(principal, "to.host", &s.to.host)?;
        }
        let ctx = CallCtx { principal, task_ref: &task_ref, request_id: &request_id, tool: "computer_files", lease: &lease, gone: &Cancel::new() };
        // The effect class, the words for the agent, and the words for a person
        // (and whether those already say why it asks first).
        let (class, describe, doing, says_why): (&'static str, String, String, bool) = match &input.op {
            FilesOp::Write(w) => {
                let exists = self.resolve_path(&task_ref, &w.path).is_ok_and(|p| p.exists());
                let path = approval::path_words(&w.path);
                if exists {
                    ("destructive", format!("overwrite {}", clip(&w.path, 120)), format!("replace the file “{path}”"), true)
                } else {
                    ("change", format!("write {}", clip(&w.path, 120)), format!("write the file “{path}”"), false)
                }
            }
            FilesOp::Publish(p) => ("change", format!("publish {}", clip(&p.path, 120)), format!("publish the file “{}” as a result of the task", approval::path_words(&p.path)), false),
            FilesOp::Send(s) => (
                "send",
                format!("send {} to {}:{}", clip(&s.path, 80), clip(&s.to.host, 40), clip(&s.to.path, 80)),
                format!("send the file “{}” to “{}” on {}", approval::path_words(&s.path), approval::path_words(&s.to.path), squash(&s.to.host, 40)),
                true,
            ),
            _ => unreachable!("reads returned above"),
        };
        let mut ask = Ask { doing, place: None, class, says_why, changed: false, again: false };
        let op = match self.remember(&ctx, &request_id, &fingerprint_source(args), class)? {
            Remembered::Fresh(op) => op,
            Remembered::Replay(op) | Remembered::FailedBeginReplay { operation: op, .. } => match self.resume_single(&ctx, &op)? {
                Resume::Stored(reply) => return reply,
                Resume::Run => op,
            },
        };
        let call_op = op.operation_ref.clone();
        let result = match input.op.clone() {
            FilesOp::Write(w) => {
                let path = w.path.clone();
                let existing = self.resolve_path(&task_ref, &w.path).ok().filter(|p| p.is_file());
                let expected = existing.as_ref().and_then(|p| std::fs::read(p).ok()).map(|bytes| hex_sha256(&bytes));
                let target=json!({"path":self.resolve_path(&task_ref,&w.path)?.to_string_lossy(),"sha256":expected});
                let mut op=op;
                if op.receipt.get("file_target").is_some_and(|old|old!=&target) && op.receipt.get("held").is_some() {
                    if let Some(att)=op.receipt.pointer("/held/attention").and_then(Value::as_str) {self.journal.db().execute("UPDATE attention_items SET state='expired' WHERE att_ref=?1",[att])?;}
                    op.receipt.as_object_mut().unwrap().remove("held");
                    op.receipt["target_changed"]=json!(true);
                }
                op.receipt["file_target"]=target;
                self.save_receipt(&op.operation_ref,op.receipt.as_object().unwrap(),None)?;
                let mut action = json!({ "kind": "write", "path": place.rel });
                if let Some(text) = &w.text {
                    action["text"] = json!(text);
                }
                if let Some(b64) = &w.base64 {
                    action["base64"] = json!(b64);
                }
                if let Some(sha) = expected {
                    action["overwrite"] = json!(true);
                    action["expected_sha256"] = json!(sha);
                }
                let content = if let Some(text)=&w.text { json!({"text":text}) } else {
                    let data=base64::Engine::decode(&base64::engine::general_purpose::STANDARD,w.base64.as_deref().unwrap_or("")).map_err(|_|invalid("Invalid base64."))?;
                    json!({"bytes":data.len(),"sha256":hex_sha256(&data)})
                };
                let describe=format!("{describe} · target {} · content {content}",op.receipt["file_target"]);
                let asked = json!({ "op": "write", "path": w.path, "target": op.receipt["file_target"], "content": content });
                ask.changed = op.receipt.get("target_changed") == Some(&json!(true));
                let bytes = w.text.as_ref().map(|t| t.len() as u64).or_else(|| w.base64.as_ref().map(|b| (b.len() as u64 / 4) * 3)).unwrap_or(0);
                let effected = self
                    .single_effect(&ctx, op, &describe, &ask, asked, |mut c| async move {
                        c.base_dir = place.base;
                        self.storage.files(&action, &c).map(|r| json!({ "records": r.records }))
                    })
                    .await?;
                match effected {
                    Effected::Held(reply) => return Ok(reply),
                    Effected::Done { op_ref, .. } => FilesResult::Write { path, bytes, op_ref },
                }
            }
            FilesOp::Publish(p) => {
                let effected = self
                    .single_effect(&ctx, op, &describe, &ask, json!({ "op": "publish", "path": p.path }), |mut c| async move {
                        c.base_dir = place.base;
                        let records = self.storage.files(&json!({ "kind": "publish", "path": place.rel }), &c)?.records;
                        Ok(records.into_iter().find(|x| x.get("kind").and_then(Value::as_str) == Some("artifact")).unwrap_or(Value::Null))
                    })
                    .await?;
                let artifact = match effected {
                    Effected::Held(reply) => return Ok(reply),
                    Effected::Done { value, .. } => value,
                };
                self.journal.put_artifact(&task_ref, principal, &artifact)?;
                let text = |key: &str| artifact.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                FilesResult::Publish {
                    path: text("name"),
                    art_ref: text("artifact_ref"),
                    sha256: text("sha256"),
                    size: artifact.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                    author_op: artifact.get("producer_ref").and_then(Value::as_str).filter(|p| p.starts_with("op_")).map(str::to_string),
                }
            }
            FilesOp::Send(s) => {
                let to = s.to.clone();
                let path = s.path.clone();
                let effected = self
                    .single_effect(&ctx, op, &describe, &ask, json!({ "op": "send", "path": s.path, "to": { "host": s.to.host, "path": s.to.path } }), |mut c| async move {
                        c.base_dir = place.base;
                        let (artifact, obligation) = self.storage.send_file(&place.rel, &s.to.host, &s.to.path, &c)?;
                        Ok(json!({ "artifact": artifact, "obligation": obligation }))
                    })
                    .await?;
                let sent = match effected {
                    Effected::Held(reply) => return Ok(reply),
                    Effected::Done { value, .. } => value,
                };
                let obligation = sent.get("obligation").map(|o| o.get("obligation").unwrap_or(o)).cloned().unwrap_or(Value::Null);
                self.record_obligation(&task_ref, &to, &obligation)?;
                if let Some(artifact) = sent.get("artifact").filter(|a| !a.is_null()) {
                    self.journal.put_artifact(&task_ref, principal, artifact)?;
                }
                let verified = self.storage.delivery_verified(&task_ref, &obligation);
                let receipt = sent.get("artifact").and_then(|a| a.get("artifact_ref")).and_then(Value::as_str).map(str::to_string);
                let next = match (&receipt, verified) {
                    (_, true) => format!("Delivered: ibara verified that {} has the file at {}.", to.host, to.path),
                    (Some(art), false) => format!("No bytes have moved yet: “{path}” is ready as {art}. {}", self.fetch_hint(art, &to.host, &to.path)),
                    (None, false) => String::new(),
                };
                FilesResult::Send { path, to, state: if verified { DeliveryState::Verified } else { DeliveryState::Pending }, receipt, next }
            }
            _ => unreachable!("reads returned above"),
        };
        self.store_reply(&call_op, Ok(Reply::ok(result, Some(&task_ref))))
    }

    /// Put a send's delivery obligation on the task, replacing the begin's
    /// obligation for the same host and path (its id is kept).
    fn record_obligation(&self, task_ref: &str, to: &contract::Destination, obligation: &Value) -> Result<()> {
        let mut task = self.task(task_ref)?;
        let same = |d: &Value| {
            d.get("host_id").and_then(Value::as_str) == Some(to.host.as_str()) && d.get("destination_path").and_then(Value::as_str) == Some(to.path.as_str())
        };
        match task.deliveries.iter_mut().find(|d| same(d)) {
            Some(existing) => {
                let id = existing.get("id").cloned();
                *existing = obligation.clone();
                if let (Some(id), Some(obj)) = (id, existing.as_object_mut()) {
                    obj.insert("id".into(), id);
                }
            }
            None => task.deliveries.push(obligation.clone()),
        }
        task.updated_at = self.now_iso();
        self.journal.put_task(&task)
    }

    // ---- computer_wait --------------------------------------------------------------

    /// `computer_wait`: returns when the thing is met, at the deadline, or when
    /// the caller goes away (`pending`, like the deadline).
    async fn wait(&self, principal: &str, connection_id: &str, input: WaitInput, gone: &Cancel) -> Result<Reply> {
        let lease = self.require_live_lease(principal, connection_id, input.task_ref.as_str())?;
        let mut task = self.task(&lease.task_ref)?;
        self.charge(&mut task, Charge::Control)?;
        let deadline = Duration::from_millis(input.deadline_ms.min(600_000));
        let started = Instant::now();
        let result = match &input.wait_for {
            WaitFor::Op(op_ref) => {
                let op = self
                    .journal
                    .get_operation_by_ref(op_ref.as_str())?
                    .filter(|op| op.task_ref.as_deref() == Some(task.task_ref.as_str()))
                    .ok_or_else(|| invalid(format!("for.op: {op_ref} is not an operation of this task")))?;
                loop {
                    let state = self.op_state(&op, principal)?;
                    let settled = state != "running";
                    if settled || started.elapsed() >= deadline || gone.is_cancelled() {
                        break contract::WaitResult { met: settled, waited_ms: started.elapsed().as_millis() as u64, state, answer: None, frame: None };
                    }
                    pause(Duration::from_millis(250).min(deadline.saturating_sub(started.elapsed())), gone).await;
                }
            }
            WaitFor::Attention(att) => {
                let item = self.journal.get_attention(att.as_str())?.filter(|a| a.task_ref == task.task_ref);
                if item.is_none() {
                    return Err(invalid(format!("for.attention: {att} is not an attention item of this task")));
                }
                loop {
                    let item = self.journal.get_attention(att.as_str())?.ok_or_else(|| invalid("for.attention: the item is gone"))?;
                    if item.state != "open" || started.elapsed() >= deadline || gone.is_cancelled() {
                        break contract::WaitResult {
                            met: item.state != "open",
                            waited_ms: started.elapsed().as_millis() as u64,
                            state: item.state,
                            answer: item.answer,
                            frame: None,
                        };
                    }
                    pause(Duration::from_millis(250).min(deadline.saturating_sub(started.elapsed())), gone).await;
                    self.assert_authority(&lease)?;
                }
            }
            WaitFor::Expect(expect) => {
                // A wait is for a state, not a change: no `before`.
                let frame = self.frames.borrow().latest(&task.task_ref);
                let awaited = self.await_expectation(&task.task_ref, expect, &[], frame.as_deref(), Some(deadline), gone).await;
                self.assert_authority(&lease)?;
                let frame = self.build_frame(&task, &lease, &FrameSpec::default()).await.ok().map(|(f, _)| f.frame.clone());
                contract::WaitResult {
                    met: awaited.met,
                    waited_ms: awaited.waited_ms,
                    state: if awaited.met { "met".into() } else { awaited.detail },
                    answer: None,
                    frame,
                }
            }
        };
        let status = if result.met { Status::Ok } else { Status::Pending };
        Ok(Reply::with_status(status, result, &task.task_ref))
    }

    /// An operation's state, refreshing a job-backed receipt from storage.
    fn op_state(&self, op: &OperationRecord, principal: &str) -> Result<String> {
        let current = self.journal.get_operation_by_ref(&op.operation_ref)?.unwrap_or_else(|| op.clone());
        let receipt = &current.receipt;
        let execution = receipt.get("execution").and_then(Value::as_str).unwrap_or("unknown").to_string();
        if execution != "running" {
            return Ok(match (execution.as_str(), receipt.get("step").and_then(|s| s.get("outcome")).and_then(Value::as_str)) {
                ("completed", Some(o)) => o.to_string(),
                ("completed", None) => "done".into(),
                (e, _) => e.to_string(),
            });
        }
        let Some(job_ref) = receipt.get("job_ref").and_then(Value::as_str) else {
            return Ok(execution);
        };
        let job = self.storage.get_job(job_ref, current.task_ref.as_deref(), Some(principal))?;
        let Some(state) = job.as_ref().and_then(|j| j.get("state")).and_then(Value::as_str) else {
            return Ok(execution);
        };
        if state == "running" {
            return Ok(execution);
        }
        let mut updated = receipt.as_object().cloned().unwrap_or_default();
        let unknown = state == "unknown";
        updated.insert("execution".into(), json!(if unknown { "unknown" } else { "completed" }));
        if unknown {
            updated.insert("verification".into(), json!("unknown"));
            updated.insert("effect".into(), json!("unknown"));
        }
        updated.insert("summary".into(), json!(format!("Job {job_ref} is {state}.")));
        if let Some(call) = updated.get_mut("call").and_then(Value::as_object_mut) {
            call.insert("status".into(), json!("ok"));
            if let Some(result) = call.get_mut("result").and_then(Value::as_object_mut) {
                result.insert("job".into(), job.clone().unwrap_or(Value::Null));
            }
        }
        self.save_receipt(&current.operation_ref, &updated, None)?;
        Ok(if unknown { "unknown".into() } else { format!("job {state}") })
    }

    // ---- computer_checkpoint ----------------------------------------------------------

    async fn checkpoint(&self, principal: &str, connection_id: &str, input: CheckpointInput) -> Result<Reply> {
        let lease = self.require_live_lease(principal, connection_id, input.task_ref.as_str())?;
        let mut task = self.task(&lease.task_ref)?;
        self.charge(&mut task, Charge::Control)?;
        let mut result = Map::new();
        if let Some(note) = &input.note {
            let record = json!({
                "kind": "checkpoint",
                "checkpoint_ref": id("checkpoint"),
                "task_ref": task.task_ref,
                "authority": "agent_note_not_verified",
                "next_step": clip(note, 4000),
                "blockers": [],
                "hypotheses": [],
                "evidence_refs": [],
            });
            self.journal.put_checkpoint(&record)?;
            let mut task = self.task(&lease.task_ref)?;
            task.last_checkpoint_ref = record.get("checkpoint_ref").and_then(Value::as_str).map(str::to_string);
            task.updated_at = self.now_iso();
            self.journal.put_task(&task)?;
            result.insert("note_ref".into(), record["checkpoint_ref"].clone());
        }
        let mut status = Status::Ok;
        if let Some(ask) = &input.ask {
            let now = self.now_iso();
            let item = self.journal.raise_attention(NewAttention {
                task_ref: &task.task_ref,
                principal,
                kind: "question",
                operation_ref: None,
                generation: Some(&lease.generation),
                question: &clip(&ask.question, 1000),
                details: None,
                options: &ask.options,
                now_iso: &now,
            })?;
            self.push_event(Some(&task.task_ref), &format!("{} asks a person: {}", item.att_ref, squash(&ask.question, 80)));
            result.insert("attention".into(), json!(item.att_ref));
            status = Status::Pending;
        }
        // The agent cannot change its own access: it asks, and a person decides.
        if input.stop_asking {
            let subject = self.task_subject(&task.task_ref, principal);
            let asked = match self.ask_to_stop_asking(&subject)? {
                Some(att) => {
                    self.push_event(Some(&task.task_ref), &format!("{att} asks a person to stop asking before sends, spends and deletes"));
                    json!({ "attention": att, "state": "waiting_for_person" })
                }
                None => json!({ "state": "nothing_to_ask" }),
            };
            result.insert("stop_asking".into(), asked);
        }
        Ok(Reply::with_status(status, Value::Object(result), &task.task_ref))
    }

    // ---- computer_finish ----------------------------------------------------------------

    /// The control a finish runs under: this connection's live lease of the
    /// task, or none when that control already ended (a person took the
    /// computer, it expired, ibara restarted). A task whose control ended can
    /// still be finished, so its outcome and summary are kept; nothing is
    /// taken back for it. Refused as before when the task is over, or when its
    /// control lives on another connection.
    fn finish_lease(&self, principal: &str, connection_id: &str, task_ref: &str) -> Result<Option<LeaseRecord>> {
        let refused = match self.require_live_lease(principal, connection_id, task_ref) {
            Ok(lease) => return Ok(Some(lease)),
            Err(refused) => refused,
        };
        if !matches!(refused.code, "HUMAN_CONTROL" | "CONTROL_UNSETTLED" | "LEASE_EXPIRED" | "BUSY") {
            return Err(refused);
        }
        let over = matches!(self.task(task_ref)?.state.as_str(), "completed" | "partial" | "cancelled" | "blocked");
        let held_elsewhere = self.journal.get_active_lease()?.is_some_and(|l| l.task_ref == task_ref);
        if over || held_elsewhere {
            return Err(refused);
        }
        Ok(None)
    }

    /// Why a finish without control leaves the task's windows open: someone
    /// else has the computer, or ibara holds it back while it starts or
    /// settles, and closing windows then would take it back.
    fn computer_is_taken(&self) -> Result<Option<&'static str>> {
        let control = self.journal.get_control()?;
        Ok(if let Some(wait) = self.system_wait(&control) {
            Some(wait.holder())
        } else if self.viewer_state.borrow().owner.is_some() || control.human_control || control.paused {
            Some("a person has the computer")
        } else if control.unsettled {
            Some("the computer has not settled")
        } else if self.journal.get_active_lease()?.is_some() {
            Some("another task has the computer")
        } else {
            None
        })
    }

    async fn finish(&self, principal: &str, connection_id: &str, agent: &str, args: &Value, input: FinishInput) -> Result<Reply> {
        let task_ref = input.task_ref.to_string();
        let request_id = input.request_id.to_string();
        let lease = self.finish_lease(principal, connection_id, &task_ref)?;
        let mut task = self.task(&task_ref)?;
        if lease.is_some() {
            self.charge(&mut task, Charge::Control)?;
        }
        // An assessment of a check ibara evaluates itself counts only when
        // ibara cannot read what it names; otherwise ibara's own result
        // stands. Either way a note says so, and the finish goes through.
        let mut automatic = Vec::new();
        let mut assessments: Vec<(String, bool, String)> = Vec::new();
        for (i, a) in input.assessments.iter().enumerate() {
            let criterion = task.success_criteria.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(a.check.as_str()));
            match criterion {
                None => {
                    let yours: Vec<&str> = task
                        .success_criteria
                        .iter()
                        .filter(|c| c.get("check").is_none_or(Value::is_null))
                        .filter_map(|c| c.get("id").and_then(Value::as_str))
                        .collect();
                    let instead = if yours.is_empty() {
                        "it has no checks for you to assess, so leave assessments out".to_string()
                    } else {
                        format!("assess only {}", yours.join(", "))
                    };
                    return Err(invalid(format!("assessments[{i}].check: this task has no check '{}'; {instead}", a.check)));
                }
                Some(c) if c.get("check").is_some_and(|x| !x.is_null()) => automatic.push(a.check.as_str()),
                _ => {}
            }
            assessments.push((a.check.to_string(), a.met, a.reason.clone()));
        }
        let op = match self.remember_for(principal, &task_ref, "computer_finish", &request_id, &fingerprint_source(args), "change")? {
            Remembered::Fresh(op) => op,
            Remembered::Replay(op) | Remembered::FailedBeginReplay { operation: op, .. } => {
                return self.stored_call(&op, &task_ref).unwrap_or_else(|| Err(fail("OUTCOME_UNKNOWN", "An earlier finish with this request_id was interrupted; check the task with computer_status.", false)));
            }
        };
        let mut receipt = op.receipt.as_object().cloned().unwrap_or_default();
        receipt.insert("operation_ref".into(), json!(op.operation_ref));
        // Checks that read the screen need control; without it they keep their last state.
        let checks = self.evaluate_task_checks(&mut task, &assessments, lease.is_some()).await?;
        let (taken, ignored): (Vec<&str>, Vec<&str>) =
            automatic.into_iter().partition(|id| checks.iter().any(|c| c.id == **id && c.basis == CheckBasis::YourAssessment));
        let mut notes = Vec::new();
        match ignored.as_slice() {
            [] => {}
            [one] => notes.push(format!("ibara checks '{one}' itself, so your assessment of it was ignored; ibara's result is in checks.")),
            many => notes.push(format!("ibara checks {} itself, so your assessments of them were ignored; ibara's results are in checks.", quoted_list(many))),
        }
        match taken.as_slice() {
            [] => {}
            [one] => notes.push(format!("ibara could not read what '{one}' names, so your assessment of it counts; checks say why.")),
            many => notes.push(format!("ibara could not read what {} name, so your assessments of them count; checks say why.", quoted_list(many))),
        }
        let unknown = self.unknown_operations(&task_ref);
        let keep = if lease.is_some() { None } else { self.computer_is_taken()? };
        let cleanup = self.cleanup(&task_ref, keep).await;
        let delivery = task.deliveries.first().map(|d| {
            let verified = self.storage.delivery_verified(&task_ref, d);
            DeliveryStatus {
                host: d.get("host_id").and_then(Value::as_str).unwrap_or("").to_string(),
                path: d.get("destination_path").and_then(Value::as_str).unwrap_or("").to_string(),
                state: if verified { DeliveryState::Verified } else { DeliveryState::Pending },
                receipt: d.get("artifact_ref").and_then(Value::as_str).map(str::to_string),
            }
        });
        let deliveries_verified = task.deliveries.iter().all(|d| d.get("required") == Some(&Value::Bool(false)) || self.storage.delivery_verified(&task_ref, d));
        receipt.insert("execution".into(), json!("running"));
        receipt.insert("summary".into(), json!(if lease.is_some() { "Finish releasing control." } else { "Finish recording the outcome." }));
        self.save_receipt(&op.operation_ref, &receipt, Some(true))?;
        if let Some(lease) = &lease {
            self.release_lease(Some(lease.clone()), Release::Finished, true).await?;
        }
        let control = self.journal.get_control()?;
        let complete = !control.unsettled
            && unknown.is_empty()
            && checks.iter().all(|c| c.state == CheckState::Met)
            && deliveries_verified;
        let mut task = self.task(&task_ref)?;
        task.success_criteria = task
            .success_criteria
            .into_iter()
            .map(|mut c| {
                if let Some(obj) = c.as_object_mut()
                    && let Some(s) = checks.iter().find(|s| Some(s.id.as_str()) == obj.get("id").and_then(Value::as_str))
                {
                    obj.insert("state".into(), json!(state_name(s.state)));
                }
                c
            })
            .collect();
        let state = match input.outcome {
            Outcome::Complete if complete => "completed",
            Outcome::Complete | Outcome::Partial => "partial",
            Outcome::Cancelled => "cancelled",
            Outcome::Blocked => "blocked",
        };
        self.stop_charging(&mut task);
        let gaps: Vec<String> = checks
            .iter()
            .filter(|c| c.state != CheckState::Met)
            .map(|c| format!("Check {} is {}.", c.id, state_name(c.state)))
            .chain((!unknown.is_empty()).then(|| format!("{} step outcomes are unknown.", unknown.len())))
            .chain((!deliveries_verified).then(|| "Delivery is not verified.".to_string()))
            .take(20)
            .collect();
        let control_word = if control.unsettled { "unsettled" } else if control.human_control || control.paused { "paused" } else { "released" };
        task.completion = Some(json!({
            "kind": "completion",
            "task_ref": task_ref,
            "outcome": state,
            "claimed_outcome": outcome_word(input.outcome),
            "control": control_word,
            "criteria": checks.iter().map(|c| json!({
                "criterion_id": c.id,
                "claim": if c.state == CheckState::Met { "met" } else { "unmet" },
                "state": state_name(c.state),
                "basis": if c.basis == CheckBasis::Automatic { "automatic" } else { "your_assessment" },
                "assessor": if c.basis == CheckBasis::YourAssessment { Some(agent) } else { None },
                "detail": c.detail,
            })).collect::<Vec<_>>(),
            "artifact_refs": [],
            "verification_gaps": gaps,
            "summary": clip(&input.summary, 2000),
            "complete": complete,
            "cleanup": cleanup,
        }));
        task.state = state.into();
        task.updated_at = self.now_iso();
        self.journal.put_task(&task)?;
        let (execution, summary) = if control.unsettled {
            receipt.insert(
                "error".into(),
                json!({ "code": "CONTROL_UNSETTLED", "message": "Work may still be running.", "retry_safe": false, "requires_reconciliation": true }),
            );
            ("unknown", "Finish could not confirm that controllable work stopped.")
        } else if lease.is_some() {
            ("completed", "Task control released.")
        } else {
            ("completed", "Outcome recorded; control had already ended.")
        };
        receipt.insert("execution".into(), json!(execution));
        receipt.insert("verification".into(), json!(if control.unsettled { "unknown" } else { "not_requested" }));
        receipt.insert("effect".into(), json!(if control.unsettled { "unknown" } else { "none" }));
        receipt.insert("summary".into(), json!(summary));
        self.frames.borrow_mut().forget(&task_ref);
        self.record_window(self.journal.forget_task_windows(&task_ref));
        let _ = self.journal.expire_attention(None, Some(&task_ref), "finish", &self.now_iso());
        self.timeline("task_finished", Some(&task_ref), agent, &clip(&input.summary, 200), json!({ "outcome": state, "complete": complete }));
        let result = FinishResult { checks, delivery, cleanup, complete, notes };
        let outcome = Ok(Reply::ok(result, Some(&task_ref)));
        self.store_call(&op.operation_ref, &mut receipt, &outcome, None)?;
        outcome
    }

    /// Close windows the task opened and still owns; leave anything a person
    /// touched or with unsaved changes, and everything when `keep` says why.
    /// A window counts as closed only once it has left the window list: an
    /// app may ignore the request or ask first.
    async fn cleanup(&self, task_ref: &str, keep: Option<&str>) -> Cleanup {
        let mut cleanup = Cleanup::default();
        let owned = self.journal.task_windows(task_ref).unwrap_or_else(|e| {
            super::log_event("task_windows_read_failed", &e.to_string());
            Vec::new()
        });
        if owned.is_empty() {
            return cleanup;
        }
        let Ok(windows) = self.desktop.windows().await else {
            cleanup.left.extend(owned.iter().map(|o| LeftOpen { surface: o.title.clone(), reason: "the desktop could not be read".into() }));
            return cleanup;
        };
        let (cancel, _) = self.abort_handles();
        let mut asked: Vec<(WinKey, String)> = Vec::new();
        for o in owned {
            let Some(live) = windows.iter().find(|w| w.address == o.address && w.pid == o.pid) else {
                continue;
            };
            let name = format!("{} \"{}\"", live.class, squash(&live.title, 48));
            if let Some(reason) = keep {
                cleanup.left.push(LeftOpen { surface: name, reason: reason.into() });
            } else if o.touched {
                cleanup.left.push(LeftOpen { surface: name, reason: "a person used it".into() });
            } else if dirty_title(&live.title) {
                cleanup.left.push(LeftOpen { surface: name, reason: "it has unsaved changes".into() });
            } else {
                self.mark_effect(Some(task_ref));
                let closed = self.desktop.act(&Effect::Close(live.key()), &cancel).await;
                self.mark_effect(None);
                match closed {
                    Ok(_) => asked.push((live.key(), name)),
                    Err(e) => cleanup.left.push(LeftOpen { surface: name, reason: format!("close failed: {}", squash(&e.message, 80)) }),
                }
            }
        }
        let started = Instant::now();
        let mut unreadable = false;
        while !asked.is_empty() {
            match self.desktop.windows().await {
                Ok(now) => {
                    unreadable = false;
                    asked.retain(|(key, name)| {
                        let open = now.iter().any(|w| w.address == key.address && w.pid == key.pid);
                        if !open {
                            self.record_window(self.journal.window_closed(&key.address));
                            cleanup.closed.push(name.clone());
                        }
                        open
                    });
                }
                Err(_) => unreadable = true,
            }
            if asked.is_empty() || started.elapsed() >= CLOSE_WAIT || pause(CLOSE_POLL, &cancel).await {
                break;
            }
        }
        let reason = if unreadable { "the desktop could not be read after the close request" } else { "it was still open after the close request" };
        cleanup.left.extend(asked.into_iter().map(|(_, surface)| LeftOpen { surface, reason: reason.into() }));
        cleanup
    }

    // ---- computer_procedures --------------------------------------------------------------

    fn procedures_tool(&self, principal: &str, input: ProceduresInput) -> Result<Reply> {
        match input.op {
            ProceduresOp::Search => {
                let query = input.query.unwrap_or_default().trim().to_lowercase();
                let notes = if query.is_empty() {
                    Vec::new()
                } else {
                    let app = query.split_whitespace().next().unwrap_or("").to_string();
                    self.journal.list_app_notes(&app, None, None, 20)?
                };
                let procedures: Vec<Value> = self
                    .storage
                    .list_procedures(principal)?
                    .into_iter()
                    .filter(|p| query.is_empty() || p.to_string().to_lowercase().contains(&query))
                    .take(20)
                    .map(|p| {
                        json!({
                            "ref": p.get("procedure_ref"),
                            "title": p.get("title").or_else(|| p.get("name")),
                            "status": p.get("status"),
                        })
                    })
                    .collect();
                let notes: Vec<Value> = notes
                    .into_iter()
                    .map(|n| json!({ "ref": n.note_ref, "app": n.app, "version": n.version, "surface": n.surface_signature, "kind": n.fact_kind, "fact": n.fact, "seen": n.count }))
                    .collect();
                Ok(Reply::ok(json!({ "procedures": procedures, "notes": notes }), None))
            }
            ProceduresOp::Read => {
                let reference = input.reference.map(|r| r.into_string()).unwrap_or_default();
                if reference.starts_with("note_") {
                    return Err(invalid("ref: notes are read through op \"search\" with the app's name"));
                }
                let record = self
                    .storage
                    .get_procedure_record(&reference, principal)?
                    .ok_or_else(|| invalid(format!("ref: no approved procedure '{}'", clip(&reference, 60))))?;
                Ok(Reply::ok(json!({ "procedure": record }), None))
            }
        }
    }
}

/// Whose new windows a step may claim.
enum Owner {
    /// The acted-on process.
    Process(i64),
    /// A launch: its process tree while the launched program runs, else
    /// also the approved app's window class.
    Launch { pid: Option<i64>, class: Option<&'static str> },
}

/// Whose new windows a step may claim: for a launch, the launched process
/// tree or the app's class; otherwise the process the step acted on.
fn step_owner(resolved: &Resolved, focused_before: Option<&Win>, launched: Option<u32>) -> Option<Owner> {
    match &resolved.plan {
        Planned::Observe => None,
        Planned::Desktop(effect) => match effect.as_ref() {
            Effect::Launch { app_id } => Some(Owner::Launch { pid: launched.map(i64::from), class: app_class(app_id) }),
            Effect::Focus(k) | Effect::Close(k) | Effect::Key { surface: k, .. } | Effect::Type { surface: k, .. } | Effect::ClickElement { surface: k, .. } => {
                Some(Owner::Process(k.pid))
            }
            Effect::ClickPoint { surface: Some(k), .. } => Some(Owner::Process(k.pid)),
            Effect::ClickPoint { surface: None, .. } | Effect::Scroll { .. } => focused_before.map(|w| Owner::Process(w.pid)),
        },
        Planned::Browser(step) => Some(Owner::Process(step.window.pid)),
    }
}

/// The window a step acts on, for its replay pictures: the effect's own
/// surface, else the window focused before it.
fn step_surface(resolved: &Resolved, focused: Option<&Win>) -> Option<WinKey> {
    let own = match &resolved.plan {
        Planned::Desktop(effect) => match effect.as_ref() {
            Effect::Focus(k) | Effect::Close(k) | Effect::Key { surface: k, .. } | Effect::Type { surface: k, .. } | Effect::ClickElement { surface: k, .. } => Some(k.clone()),
            Effect::ClickPoint { surface, .. } => surface.clone(),
            Effect::Launch { .. } | Effect::Scroll { .. } => None,
        },
        Planned::Browser(step) => Some(step.window.clone()),
        Planned::Observe => None,
    };
    own.or_else(|| focused.map(Win::key))
}

/// What an approval is for: the resolved target and content of a step,
/// compared when a held step is run after approval.
fn target_identity(resolved: &Resolved) -> Value {
    let window = |k: &WinKey| json!({ "address": k.address, "pid": k.pid, "class": k.class });
    match &resolved.plan {
        Planned::Observe => Value::Null,
        Planned::Browser(step) => {
            let op = match &step.op {
                BrowserOp::Navigate(url) => json!({ "navigate": url }),
                BrowserOp::Click => json!("click"),
                BrowserOp::Type(text) => json!({ "type": hex_sha256(text.as_bytes()) }),
                BrowserOp::Select(value) => json!({ "select": value }),
                BrowserOp::Scroll { dx, dy } => json!({ "scroll": [dx, dy] }),
                BrowserOp::Key(combo) => json!({ "key": combo }),
            };
            json!({ "browser": op, "target": step.target, "window": window(&step.window) })
        }
        Planned::Desktop(effect) => match effect.as_ref() {
            Effect::Launch { app_id } => json!({ "launch": app_id }),
            Effect::Focus(k) => json!({ "focus": window(k) }),
            Effect::Close(k) => json!({ "close": window(k) }),
            Effect::ClickElement { surface, element, button, double } => json!({
                "click_element": {
                    "window": window(surface),
                    "role": element.role,
                    "name": element.name,
                    "context": element.context,
                    "selector": element.selector,
                },
                "button": format!("{button:?}"),
                "double": double,
            }),
            Effect::ClickPoint { x, y, surface, button, double } => json!({
                "click_point": [x.round(), y.round()],
                "window": surface.as_ref().map(window),
                "button": format!("{button:?}"),
                "double": double,
            }),
            Effect::Type { surface, text, .. } => json!({ "type": hex_sha256(text.as_bytes()), "window": window(surface) }),
            Effect::Key { surface, combo } => json!({ "key": combo, "window": window(surface) }),
            Effect::Scroll { at, dx, dy } => json!({ "scroll": [dx, dy], "at": at.map(|(x, y)| [x.round(), y.round()]) }),
        },
    }
}

/// A browser window class (Chromium or Google Chrome).
pub(super) fn is_browser(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    class.contains("chromium") || class.contains("google-chrome")
}

/// `'a' and 'b'`, `'a', 'b' and 'c'`.
fn quoted_list(ids: &[&str]) -> String {
    match ids {
        [first @ .., last] if !first.is_empty() => format!("{} and '{last}'", first.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ")),
        _ => ids.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", "),
    }
}

/// `text` as runs of ASCII and runs of other characters, in order, each with
/// whether it is ASCII.
fn text_runs(text: &str) -> Vec<(&str, bool)> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut current = None;
    for (i, c) in text.char_indices() {
        if let Some(ascii) = current.filter(|ascii| *ascii != c.is_ascii()) {
            runs.push((&text[start..i], ascii));
            start = i;
        }
        current = Some(c.is_ascii());
    }
    if let Some(ascii) = current {
        runs.push((&text[start..], ascii));
    }
    runs
}


/// Where a page went, without its query or fragment: a form sent with GET
/// carries what was typed in its address.
fn page_address(url: &str) -> String {
    clip(url.split(['?', '#']).next().unwrap_or(url), 120)
}

/// The extension's refusals before any input: the page changed since it was
/// observed, something covers the element, or it has not stopped moving.
fn page_refusal(data: &Value) -> Result<()> {
    if data.get("covered").and_then(Value::as_bool) == Some(true) {
        return Err(fail("BLOCKED_BY_DIALOG", "Something covers that element (a banner, menu or dialog); deal with it, then observe again.", true)
            .with("execution_not_started", true));
    }
    if data.get("moving").and_then(Value::as_bool) == Some(true) {
        return Err(fail("STALE_TARGET", "That element is still moving (an animation, or the page is still loading); try again.", true)
            .with("execution_not_started", true));
    }
    if data.get("refused").and_then(Value::as_bool) == Some(true) {
        return Err(fail("STALE_TARGET", "The page changed since it was observed, or the tab is not focused; observe again.", true)
            .with("execution_not_started", true));
    }
    Ok(())
}

/// The tallest band the browser's own bars above the page can make, in
/// window pixels. Chromium's and Chrome's tab strip and toolbar take 87 on
/// Hyprland (Tulip0 and Tulip1, 26 September), a bookmarks bar about 30 and
/// each infobar about 40. A taller band means something below the page
/// shrinks it (developer tools docked at the bottom), which `page_point`
/// cannot tell from the page's own measure and which would move every
/// click down by its height.
const BROWSER_BARS_MAX: f64 = 200.0;

/// A point in the page (CSS pixels of the viewport) as a desktop point, from
/// the page's own measure of itself: `zoom` is the tab's zoom, `outer` the
/// window in device-independent pixels and `inner` the viewport in CSS
/// pixels. Chromium on Hyprland draws the viewport flush with the window's
/// left and bottom edges, with its tab strip and toolbar above; a trusted
/// click landed within 0 CSS pixels at scale 1 and 1.5 and zoom 100 % and
/// 125 % (Tulip0 and Tulip1, 26 September; `.research/browser-route/`).
/// Refuses when the window and the page disagree (a scaling mismatch),
/// something sits beside the page, or the band above the page is taller than
/// the browser's bars can make it (something sits below the page).
fn page_point(data: &Value, rect: &crate::controller::Rect) -> Result<(f64, f64)> {
    let n = |p: &str| data.pointer(p).and_then(Value::as_f64);
    let refuse = |why: &str| {
        IbaraError::new("CAPABILITY_UNAVAILABLE", format!("{why}; use computer_act on the browser window."), true).with("execution_not_started", true)
    };
    let (Some(x), Some(y), Some(ow), Some(oh), Some(iw), Some(ih), Some(zoom)) =
        (n("/x"), n("/y"), n("/outer/0"), n("/outer/1"), n("/inner/0"), n("/inner/1"), n("/zoom"))
    else {
        return Err(refuse("The page did not report where the element is"));
    };
    if zoom <= 0.0 || (rect.width as f64 - ow).abs() > 2.0 || (rect.height as f64 - oh).abs() > 2.0 {
        return Err(refuse("The browser window and the page disagree about its size"));
    }
    let top = oh - ih * zoom;
    if (ow - iw * zoom).abs() > 2.0 || top < 0.0 {
        return Err(refuse("Something beside the page (a side panel or developer tools) moves it"));
    }
    if top > BROWSER_BARS_MAX {
        return Err(refuse("Something below the page (developer tools docked at the bottom) moves it"));
    }
    Ok((rect.x as f64 + x * zoom, rect.y as f64 + top + y * zoom))
}

/// Removes a call from `in_flight` when it ends, however it ends.
struct InFlight<'a> {
    map: &'a RefCell<HashMap<(String, String, String), Value>>,
    key: (String, String, String),
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.map.borrow_mut().remove(&self.key);
    }
}

/// What repeating a single-effect call does.
enum Resume {
    Stored(Result<Reply>),
    Run,
}

/// What a single effect did: ran (its operation and value), or was held for
/// a person's approval (the pending reply, already stored for replay).
enum Effected {
    Done { op_ref: String, value: Value },
    Held(Reply),
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The text of the first observation record storage returned.
fn observation_text(records: &[Value]) -> String {
    records
        .iter()
        .find_map(|r| r.get("content").and_then(|c| c.get("text")).and_then(Value::as_str).or_else(|| r.get("text").and_then(Value::as_str)))
        .unwrap_or("")
        .to_string()
}

fn step_source(spec: &StepSpec) -> Value {
    match spec {
        StepSpec::Desktop(step) => to_value(step),
        StepSpec::Browser { action, expect, effect } => json!({ "action": action, "expect": expect, "effect": effect }),
    }
}

fn outcome_name(o: StepOutcome) -> &'static str {
    match o {
        StepOutcome::Done => "done",
        StepOutcome::Unmet => "unmet",
        StepOutcome::Unknown => "unknown",
        StepOutcome::NotRun => "not_run",
    }
}

fn outcome_word(o: Outcome) -> &'static str {
    match o {
        Outcome::Complete => "complete",
        Outcome::Partial => "partial",
        Outcome::Cancelled => "cancelled",
        Outcome::Blocked => "blocked",
    }
}

pub(crate) fn state_name(s: CheckState) -> &'static str {
    match s {
        CheckState::Pending => "pending",
        CheckState::Met => "met",
        CheckState::Unmet => "unmet",
        CheckState::Unknown => "unknown",
    }
}

fn parse_state(s: Option<&str>) -> CheckState {
    match s {
        Some("met") => CheckState::Met,
        Some("unmet") => CheckState::Unmet,
        Some("unknown") => CheckState::Unknown,
        _ => CheckState::Pending,
    }
}

/// A frame that says the desktop could not be read.
fn unreadable_frame(e: &IbaraError) -> Frame {
    Frame {
        frame_ref: String::new(),
        revision: 0,
        captured_at: crate::ids::now_iso(),
        covered: "nothing: the desktop could not be read".into(),
        cost_bytes: 0,
        lines: vec![format!("desktop unreadable: {}", squash(&e.message, 120))],
        choices: Vec::new(),
        next_richer: None,
        next_cursor: None,
    }
}

