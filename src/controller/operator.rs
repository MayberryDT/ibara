//! Operator operations (`core.ts:201-289`): session, status, task following,
//! previews, files, pause and resume, the take-control/handback transitions,
//! viewer tickets and the shared clipboard. Every success carries
//! `endpoint_id`, `controller_epoch` and `authorization_generation`.
//! `pairing_confirm` stays in the server layer, and so does saving what
//! `viewer_register` authorizes here.

use super::{Controller, OperatorGrant, is_ref, squash};
use crate::error::{IbaraError, Result, invalid};
use base64::Engine;
use serde_json::{Map, Value, json};

/// The task an operator's `status` pinned for `task_status` (`operatorFollowPins`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FollowPin {
    pub task_ref: String,
    pub epoch: String,
    pub generation: Value,
}

/// The latest step of the running task, in a person's words, and where its
/// click landed; kept in memory for `status`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepSeen {
    pub task_ref: String,
    pub summary: String,
    pub at: String,
    /// `(x, y, at)`: the click, as fractions of the screen showing it.
    pub point: Option<(f64, f64, String)>,
}

/// A step's words as the end of "… wants to" ("click the “Sign Up”
/// button") turned into what is happening ("Clicking the “Sign Up” button"),
/// at most 80 characters.
pub(crate) fn step_words(said: &str) -> String {
    let said = said.trim();
    let (verb, rest) = said.split_once(' ').unwrap_or((said, ""));
    let (lead, last) = verb.rsplit_once('-').map_or(("", verb), |(l, v)| (l, v));
    let doing = match last {
        "run" => "running".to_string(),
        "go" => "going".to_string(),
        v if v.ends_with('e') && !v.ends_with("ee") => format!("{}ing", &v[..v.len() - 1]),
        v => format!("{v}ing"),
    };
    let doing = if lead.is_empty() { doing } else { format!("{lead}-{doing}") };
    let mut chars = doing.chars();
    let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    let words = if rest.is_empty() { format!("{first}{}", chars.as_str()) } else { format!("{first}{} {rest}", chars.as_str()) };
    squash(&words, 80)
}

/// A finished task's state as the console's outcome word.
fn finished_outcome(state: &str) -> Option<&'static str> {
    Some(match state {
        "completed" => "done",
        "partial" => "failed",
        "cancelled" => "cancelled",
        "blocked" => "blocked",
        _ => return None,
    })
}

impl Controller {
    /// A running task's step is starting: `said` is its words as the end of
    /// "… wants to". A step that clicks adds its point once it has landed.
    pub(crate) fn note_step(&self, task_ref: &str, said: &str) {
        *self.last_step.borrow_mut() = Some(StepSeen { task_ref: task_ref.to_string(), summary: step_words(said), at: self.now_iso(), point: None });
    }

    /// Where the running task's latest click landed, as screen fractions.
    pub(crate) fn note_click(&self, task_ref: &str, (x, y): (f64, f64)) {
        let at = self.now_iso();
        if let Some(seen) = self.last_step.borrow_mut().as_mut().filter(|s| s.task_ref == task_ref) {
            seen.point = Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0), at));
        }
    }
}

const FILE_OPS: &[&str] = &[
    "files_roots",
    "files_list",
    "files_begin_upload",
    "files_resume_upload",
    "files_upload_chunk",
    "files_publish",
    "files_begin_download",
    "files_download_chunk",
    "files_status",
];

/// File operations that read a whole file (a sent one's final check, a fetched
/// one's digest): their own transport and the long relay deadline.
pub const SLOW_FILE_OPS: &[&str] = &["files_publish", "files_begin_download"];

fn denied(message: &str) -> IbaraError {
    IbaraError::new("PERMISSION_DENIED", message, true)
}

impl Controller {
    /// Called only after the transport verified an operator credential.
    pub async fn operator_call(&self, operator_id: &str, action: Value) -> Result<Value> {
        self.ensure_open()?;
        let op = action.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        let known = matches!(
            op.as_str(),
            "session" | "status" | "task_status" | "observe" | "observe_video" | "take_control" | "handback" | "pause" | "resume" | "access" | "access_set"
                | "access_remove" | "access_unpair" | "attention" | "answer_attention" | "viewer_register" | "viewer_ticket"
                | "clipboard_get" | "clipboard_set"
        )
            || FILE_OPS.contains(&op.as_str())
            || super::EVERYDAY_OPS.contains(&op.as_str());
        if !known {
            return Err(invalid("Unknown operator operation."));
        }
        let access_revision=crate::access::Access::load(&self.journal)?.map(|a|a.revision);
        let authorize = || -> Result<OperatorGrant> {
            if crate::access::Access::load(&self.journal)?.map(|a|a.revision)!=access_revision {
                return Err(denied("Access changed while this request was running."));
            }
            let grant = self.operator_grant(operator_id).filter(|g| g.active(self.now_ms()));
            let Some(grant) = grant else {
                self.follow_pins.borrow_mut().remove(operator_id);
                return Err(denied("Operator grant unavailable."));
            };
            if !grant.generation_matches(action.get("expected_authorization_generation")) {
                self.follow_pins.borrow_mut().remove(operator_id);
                return Err(denied("Operator grant generation changed."));
            }
            let endpoint_ok = action.get("endpoint_id").and_then(Value::as_str) == Some(self.endpoint_id.as_str());
            let epoch_ok = op == "session" || action.get("controller_epoch").and_then(Value::as_str) == Some(self.epoch.as_str());
            if !endpoint_ok || !epoch_ok {
                self.follow_pins.borrow_mut().remove(operator_id);
                return Err(denied("Target binding changed."));
            }
            Ok(grant)
        };
        let mut grant = authorize()?;
        if op == "access" { return self.access_view(operator_id).map(|mut v| { v["endpoint_id"]=json!(self.endpoint_id); v["controller_epoch"]=json!(self.epoch); v["authorization_generation"]=grant.generation.clone(); v }); }
        // `attention` is read by every paired computer: approvals are listed
        // only to one that may answer them (see `attention` below).
        let capability = match op.as_str() {
            "access_set"|"access_remove"|"access_unpair"|"answer_attention"=>Some("administer"),
            "observe"|"observe_video"|"task_status"=>Some("watch"), "take_control"|"pause"|"resume"=>Some("control"),
            op if FILE_OPS.contains(&op)=>Some("files"),
            op if super::EVERYDAY_OPS.contains(&op)=>Some(super::everyday::capability(op)),
            _=>None,
        };
        if let Some(cap)=capability {
            let gate = if cap == "watch" { self.watch_gate(operator_id)? } else { self.access_gate(operator_id,cap,&action)? };
            if let Some(mut pending)=gate {
                pending["endpoint_id"]=json!(self.endpoint_id); pending["controller_epoch"]=json!(self.epoch); pending["authorization_generation"]=grant.generation.clone();
                return Ok(pending);
            }
            // An allowed or approved grant stands in for the legacy flags; without
            // an access model the legacy grant alone decides.
            if crate::access::Access::load(&self.journal)?.is_some() {
                if cap=="watch" { grant.observe=true; }
                if cap=="files" { grant.files=true; }
            }
        }
        let identity = |generation: &Value| {
            let mut out = Map::new();
            out.insert("endpoint_id".into(), json!(self.endpoint_id));
            out.insert("controller_epoch".into(), json!(self.epoch));
            out.insert("authorization_generation".into(), generation.clone());
            out
        };
        match op.as_str() {
            "access_set"|"access_remove"|"access_unpair" => {
                let mut v=self.change_access_authorized(operator_id,&action).await?;
                v.as_object_mut().unwrap().extend(identity(&grant.generation)); Ok(v)
            }
            "attention" => {
                let answers = crate::access::Access::load(&self.journal)?.is_none_or(|a| a.rule(operator_id, "administer", self.now_ms()) == super::Rule::Allow);
                let items: Vec<Value> = if answers {
                    self.journal.list_attention(Some("open"), None, 100)?.into_iter().map(|item| {
                        let mut v = json!(item);
                        v["summary"] = json!(squash(&item.question, 400));
                        v
                    }).collect()
                } else {
                    Vec::new()
                };
                let mut v = identity(&grant.generation);
                v.insert("items".into(), json!(items));
                v.insert("can_answer".into(), json!(answers));
                v.insert("repair".into(), self.repair_status());
                Ok(Value::Object(v))
            }
            "answer_attention" => {
                let (att_ref, answer) = (action["att_ref"].as_str().unwrap_or(""), action["answer"].as_str().unwrap_or(""));
                let asked = self.journal.get_attention(att_ref)?.ok_or_else(|| invalid("Unknown attention item."))?;
                // Always Allow, or Allow on an agent's request to stop asking, also changes access.
                let mut allowed = None;
                // An agent's question takes one of its options (any short answer when it
                // has none), or is dismissed unanswered; an approval is approved, denied or,
                // for an agent's send, spend or delete, always allowed.
                let item = if asked.kind == "question" && action["dismiss"] == true {
                    self.journal.expire_attention(Some(att_ref), None, operator_id, &self.now_iso())?;
                    self.journal.get_attention(att_ref)?.ok_or_else(|| invalid("Unknown attention item."))?
                } else if asked.kind == "question" {
                    if answer.trim().is_empty() || answer.chars().count() > 1000 {return Err(invalid("Give an answer of 1 to 1000 characters."));}
                    self.journal.answer_attention(att_ref,answer,operator_id,&self.now_iso())?
                } else {
                    if !matches!(answer,"approve"|"deny"|"always") {return Err(invalid("Choose approve, deny or always."));}
                    let (item,changed)=self.answer_approval(att_ref,answer,operator_id).await?;
                    allowed=changed;
                    item
                };
                let mut v=identity(&grant.generation); v.insert("item".into(),json!(item));
                if let Some(allowed)=allowed { v.insert("allowed".into(),allowed); }
                Ok(Value::Object(v))
            }
            "session" => Ok(Value::Object(identity(&grant.generation))),
            "status" => self.operator_status(operator_id, &grant, &authorize, &identity).await,
            "task_status" => {
                if !grant.observe {
                    self.follow_pins.borrow_mut().remove(operator_id);
                    return Err(denied("Task following requires observation access."));
                }
                let task_ref = action.get("task_ref").and_then(Value::as_str).unwrap_or("");
                if !is_ref(task_ref) {
                    return Err(invalid("Expected one task reference."));
                }
                let pin = self.follow_pins.borrow().get(operator_id).cloned();
                let pinned = pin.is_some_and(|p| p.task_ref == task_ref && p.epoch == self.epoch && p.generation == grant.generation);
                if !pinned {
                    return Err(denied("Task is unavailable."));
                }
                let task = self.journal.get_task(task_ref)?.ok_or_else(|| denied("Task is unavailable."))?;
                let terminal = matches!(task.state.as_str(), "completed" | "partial" | "cancelled" | "blocked");
                let needs_attention = matches!(task.state.as_str(), "waiting_for_human" | "interrupted" | "blocked")
                    || self.journal.count_open_attention(Some(task_ref)).unwrap_or(0) > 0;
                let verified = task.state == "completed" && self.completion_state(&task)?.get("verified_complete") == Some(&Value::Bool(true));
                authorize()?;
                if self.require_access(operator_id,"watch").is_err() {
                    self.follow_pins.borrow_mut().remove(operator_id);
                    return Err(denied("Observation grant unavailable."));
                }
                let mut out = identity(&grant.generation);
                out.insert("task_ref".into(), json!(task_ref));
                out.insert("state".into(), json!(task.state));
                out.insert("terminal".into(), json!(terminal));
                out.insert("needs_attention".into(), json!(needs_attention));
                out.insert("verified_complete".into(), json!(verified));
                out.insert("updated_at".into(), json!(task.updated_at));
                Ok(Value::Object(out))
            }
            "take_control" | "handback" => self.operator_control(operator_id, op == "take_control", &action, &authorize).await,
            // Only the holder, who was allowed control when taking it.
            "viewer_ticket" => self.viewer_ticket(operator_id, &authorize).await,
            // The server saves the certificate once this has authorized it.
            "viewer_register" => {
                let cert = action.get("viewer_cert_sha256").and_then(Value::as_str).unwrap_or("");
                if !crate::server::authority::is_hex_lower(cert, 64) {
                    return Err(invalid("Expected the viewer certificate's SHA-256."));
                }
                Ok(Value::Object(identity(&grant.generation)))
            }
            "clipboard_get" | "clipboard_set" => {
                let result = if op == "clipboard_get" {
                    self.clipboard_get(operator_id, &action).await?
                } else {
                    self.clipboard_set(operator_id, &action).await?
                };
                authorize()?;
                let mut out = identity(&grant.generation);
                out.extend(result.as_object().cloned().unwrap_or_default());
                Ok(Value::Object(out))
            }
            "pause" | "resume" => {
                let mut v = self.operator_pause(op == "pause", &authorize).await?;
                v.as_object_mut().expect("object").extend(identity(&grant.generation));
                Ok(v)
            }
            op if super::EVERYDAY_OPS.contains(&op) => {
                let result = self.everyday(operator_id, op, &action).await?;
                authorize()?;
                let mut out = match result {
                    Value::Object(map) => map,
                    other => Map::from_iter([("value".to_string(), other)]),
                };
                out.extend(identity(&grant.generation));
                Ok(Value::Object(out))
            }
            op if FILE_OPS.contains(&op) => {
                if !grant.files {
                    return Err(denied("Operator file access unavailable."));
                }
                let result = self.storage.operator_files(operator_id, &action)?;
                authorize()?;
                let mut out = result.as_object().cloned().unwrap_or_default();
                out.extend(identity(&grant.generation));
                Ok(Value::Object(out))
            }
            "observe_video" => self.operator_video(operator_id, &grant, &action, &authorize).await,
            _ => self.operator_observe(&grant, &action, &authorize).await,
        }
    }

    async fn operator_status(
        &self,
        operator_id: &str,
        grant: &OperatorGrant,
        authorize: &dyn Fn() -> Result<OperatorGrant>,
        identity: &dyn Fn(&Value) -> Map<String, Value>,
    ) -> Result<Value> {
        let mut outputs: Vec<Value> = Vec::new();
        // Watch that asks first lists the displays, without pictures, so the
        // other computer can ask to watch one; a declined sitting does not.
        let asks = crate::access::Access::load(&self.journal)?.is_some_and(|a| {
            a.rule(operator_id, "watch", self.now_ms()) == super::Rule::Ask && self.sitting(operator_id, &a).is_none()
        });
        if (grant.observe || asks) && self.desktop.session_available() {
            let listed = async {
                self.desktop.session_ready().await?;
                self.desktop.outputs().await
            }
            .await;
            if let Ok(listed) = listed {
                outputs = listed.into_iter().map(|o| json!({ "display_id": o.display_id, "label": o.label, "display_revision": o.display_revision })).collect();
            }
        }
        let current = match authorize() {
            Ok(g) => g,
            Err(e) => {
                self.follow_pins.borrow_mut().remove(operator_id);
                return Err(e);
            }
        };
        if grant.observe && !current.observe {
            self.follow_pins.borrow_mut().remove(operator_id);
            return Err(denied("Observation grant unavailable."));
        }
        let lease = if current.observe { self.journal.get_active_lease()? } else { None };
        let active_task_ref = lease.as_ref().map(|l| l.task_ref.clone());
        let task = match &lease {
            Some(l) => self.journal.get_task(&l.task_ref)?,
            None => None,
        };
        let active_task = match (&lease, &task) {
            (Some(l), Some(t)) => {
                let mut active = json!({
                    "task_ref": l.task_ref,
                    "title": squash(&t.goal, 160),
                    "principal": l.principal,
                    "state": t.state,
                    "started_at": l.acquired_at,
                });
                if let Some(seen) = self.last_step.borrow().as_ref().filter(|s| s.task_ref == l.task_ref) {
                    active["last_step"] = json!({ "summary": seen.summary, "at": seen.at });
                    if let Some((x, y, at)) = &seen.point {
                        active["last_point"] = json!({ "x": x, "y": y, "at": at });
                    }
                }
                active
            }
            _ => Value::Null,
        };
        // The latest finished task, seen by the same people as the running one.
        let last_task = match current.observe {
            true => self.journal.last_finished_task()?.and_then(|t| {
                Some(json!({
                    "ref": t.task_ref,
                    "title": squash(&t.goal, 160),
                    "outcome": finished_outcome(&t.state)?,
                    "finished_at": t.updated_at,
                }))
            }),
            false => None,
        };
        match &active_task_ref {
            Some(task_ref) => {
                self.follow_pins.borrow_mut().insert(
                    operator_id.to_string(),
                    FollowPin { task_ref: task_ref.clone(), epoch: self.epoch.clone(), generation: current.generation.clone() },
                );
            }
            None if !current.observe => {
                self.follow_pins.borrow_mut().remove(operator_id);
            }
            None => {}
        }
        let (fault, holds) = {
            let viewer = self.viewer_state.borrow();
            (viewer.fault, !viewer.fault && viewer.owner.as_deref() == Some(operator_id))
        };
        let control_allowed=crate::access::Access::load(&self.journal)?.is_none_or(|a|a.rule(operator_id,"control",self.now_ms())!=super::Rule::Deny);
        let stream = self.stream.as_ref().is_some_and(|s| s.available());
        let interactive = if control_allowed && stream && !fault {
            "available_if_exclusive"
        } else {
            "unsupported_without_verified_viewer_adapter"
        };
        let mut out = identity(&current.generation);
        let observation = if current.observe { "available_if_desktop_ready" } else if asks { "ask_first" } else { "denied" };
        out.insert("observation".into(), json!(observation));
        out.insert("active_task_ref".into(), json!(active_task_ref));
        out.insert("active_task".into(), active_task);
        out.insert("last_task".into(), json!(last_task));
        out.insert("outputs".into(), json!(outputs));
        out.insert("files".into(), json!(if current.files { "available_if_root_approved" } else { "denied" }));
        out.insert("interactive_control".into(), json!(interactive));
        out.insert("owner".into(), json!(self.viewer_owner_name()?));
        out.insert("ownership_revision".into(), json!(self.viewer_revision_name()?));
        out.insert("holds_control".into(), json!(holds));
        out.insert("video".into(), json!(self.desktop.video_capability()));
        out.insert("repair".into(), self.repair_status());
        let control = self.journal.get_control()?;
        out.insert("paused".into(), json!(control.paused));
        out.insert("pause_origin".into(), json!(control.pause_origin.map(|o| o.as_str())));
        out.insert("name".into(), json!(crate::settings::current().text("name")));
        out.insert("wake".into(), self.wake_info().await);
        out.insert("disk_password".into(), self.disk_password().await);
        if let Some(a)=crate::access::Access::load(&self.journal)? {
            out.insert("access".into(),a.own_row(operator_id,self.now_ms(),(self.ask_first)()));
            if a.rule(operator_id,"administer",self.now_ms())==super::Rule::Allow { out.insert("attention".into(),json!(self.journal.list_attention(Some("open"),None,20)?)); }
        }
        Ok(Value::Object(out))
    }

    /// `observe`: one preview of a display (tile or selected).
    async fn operator_observe(&self, grant: &OperatorGrant, action: &Value, authorize: &dyn Fn() -> Result<OperatorGrant>) -> Result<Value> {
        if !grant.observe {
            return Err(denied("Observation not granted."));
        }
        if !self.desktop.session_available() {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Desktop session locked or unavailable.", true).with("reason", "locked"));
        }
        self.desktop.session_ready().await?;
        authorize()?;
        let display_id = action.get("display_id").and_then(Value::as_str).unwrap_or("");
        let quality = action.get("quality").and_then(Value::as_str).unwrap_or("");
        if display_id.is_empty() || display_id.len() > 128 || !matches!(quality, "tile" | "selected") {
            return Err(invalid("Expected display and quality."));
        }
        // A console that reads JPEG asks for it; one from before gets PNG.
        let format = if action.get("format").and_then(Value::as_str) == Some("jpeg") { "jpeg" } else { "png" };
        // The digest of the picture that console already shows.
        let previous = action.get("previous").and_then(Value::as_str);
        // The picture interval setting: within it, every computer gets the last picture again.
        let interval_ms = crate::settings::current().number("preview_seconds").max(1) * 1000;
        let key = (display_id.to_string(), quality.to_string());
        let expected_revision = action.get("display_revision").and_then(Value::as_str).filter(|s| !s.is_empty());
        if interval_ms > 1000 {
            let now = self.now_ms();
            let cached = self.previews.borrow().get(&key).filter(|(at, frame)| now - at < interval_ms && frame.get(format).is_some()).map(|(_, frame)| frame.clone());
            if let Some(mut frame) = cached.filter(|f| expected_revision.is_none_or(|r| f["display_revision"] == r)) {
                let delivery = authorize()?;
                frame["authorization_generation"] = delivery.generation;
                return Ok(unless_shown(frame, previous));
            }
        } else if !self.previews.borrow().is_empty() {
            self.previews.borrow_mut().clear();
        }
        let frame = self.desktop.preview(display_id, quality, format).await?;
        let delivery = authorize()?;
        self.desktop.session_ready().await?;
        authorize()?;
        if let Some(expected) = expected_revision
            && expected != frame.display_revision
        {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Display generation changed.", true).with("reason", "stale_display"));
        }
        self.preview_sequence.set(self.preview_sequence.get() + 1);
        let mut out = json!({
            "digest": crate::server::policy::sha256_hex(&frame.picture),
            "width": frame.width,
            "height": frame.height,
            "sourceWidth": frame.source_width,
            "sourceHeight": frame.source_height,
            "display_revision": frame.display_revision,
            "capture_time": self.now_iso(),
            "frame_sequence": self.preview_sequence.get(),
            "endpoint_id": self.endpoint_id,
            "controller_epoch": self.epoch,
            "authorization_generation": delivery.generation,
        });
        out[format] = json!(base64::engine::general_purpose::STANDARD.encode(&frame.picture));
        if interval_ms > 1000 {
            self.previews.borrow_mut().insert(key, (self.now_ms(), out.clone()));
        }
        Ok(unless_shown(out, previous))
    }

    /// `observe_video`: the live video bytes of a display since `cursor`,
    /// under the same grant and checks as a preview, on every call. Each
    /// operator reads an encoder of their own, started again whenever the
    /// access model or their grant changes, so no bytes recorded before they
    /// were allowed reach them.
    async fn operator_video(&self, operator_id: &str, grant: &OperatorGrant, action: &Value, authorize: &dyn Fn() -> Result<OperatorGrant>) -> Result<Value> {
        if !grant.observe {
            return Err(denied("Observation not granted."));
        }
        // A person at the controls sees the screen itself; the encoder rests.
        if self.viewer_state.borrow().owner.is_some() {
            self.desktop.stop_video();
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "A person has control of this computer.", true).with("reason", "control_held"));
        }
        if !self.desktop.session_available() {
            self.desktop.stop_video();
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Desktop session locked or unavailable.", true).with("reason", "locked"));
        }
        if let Err(e) = self.desktop.session_ready().await {
            self.desktop.stop_video();
            return Err(e);
        }
        let current = authorize()?;
        let revision = crate::access::Access::load(&self.journal)?.map(|a| a.revision);
        let access = format!("access {revision:?}, grant {}", current.generation);
        let edge = |key: &str| action.get(key).and_then(Value::as_i64);
        let (Some(width), Some(height)) = (edge("width"), edge("height")) else {
            return Err(invalid("Expected width and height.").with("field", "width/height"));
        };
        let (width, height) = crate::desktop::video::clamp_size(width, height)?;
        let cursor = match action.get("cursor") {
            None | Some(Value::Null) => None,
            Some(c) => Some(c.as_u64().ok_or_else(|| invalid("Expected the cursor from the last answer.").with("field", "cursor"))?),
        };
        let display = action.get("display_id").and_then(Value::as_str).filter(|d| !d.is_empty());
        if display.is_some_and(|d| d.len() > 128) {
            return Err(invalid("Invalid display_id.").with("field", "display_id"));
        }
        let chunk = self.desktop.observe_video(operator_id, &access, display, width, height, cursor).await?;
        let delivery = authorize()?;
        Ok(json!({
            "cursor": chunk.cursor,
            "data": base64::engine::general_purpose::STANDARD.encode(&chunk.data),
            "reset": chunk.reset,
            "ended": chunk.ended,
            "width": chunk.width,
            "height": chunk.height,
            "endpoint_id": self.endpoint_id,
            "controller_epoch": self.epoch,
            "authorization_generation": delivery.generation,
        }))
    }
}

/// A picture the console already shows (its digest is `previous`) is not sent
/// again: the answer says `unchanged` instead, so a still screen costs a few
/// hundred bytes a second rather than the whole picture.
fn unless_shown(mut frame: Value, previous: Option<&str>) -> Value {
    if previous.is_some_and(|p| frame["digest"] == p)
        && let Some(fields) = frame.as_object_mut()
    {
        fields.shift_remove("png");
        fields.shift_remove("jpeg");
        fields.insert("unchanged".into(), json!(true));
    }
    frame
}

impl Controller {
    /// What a person reads for an access request: who asks, the action it
    /// holds and what that acts on. One not recognised here names only the
    /// capability; its details say exactly what it is.
    pub(super) fn access_words(&self, asking: &str, cap: &str, action: &Value) -> String {
        let words = match self.held_action(asking, cap, action) {
            Some(what) => format!("{asking} asks to {what}."),
            None => {
                let can = capability_words(cap).unwrap_or("use this computer");
                format!("{asking} asks to {can} in a way ibara can't describe; check the details.")
            }
        };
        super::squash(&words, 300)
    }

    /// What a held action does, as the end of "X asks to …", when recognised.
    fn held_action(&self, asking: &str, cap: &str, action: &Value) -> Option<String> {
        let text = |key: &str| action.get(key).and_then(Value::as_str).map(|v| super::squash(v, 80)).filter(|v| !v.is_empty());
        let quoted = |v: &str| format!("“{}”", super::squash(v, 80));
        let task = |what: &str| match text("task_ref").and_then(|r| self.journal.get_task(&r).ok().flatten()) {
            Some(task) => format!("{what} the agent task {}", quoted(&task.goal)),
            None => format!("{what} an agent task"),
        };
        let procedure = |what: &str| {
            let record = text("procedure_ref").and_then(|r| self.storage.get_procedure_record(&r, "operator").ok().flatten());
            match record.as_ref().and_then(|r| r.pointer("/definition/title")).and_then(Value::as_str) {
                Some(title) => format!("{what} the procedure {}", quoted(title)),
                None => format!("{what} a procedure"),
            }
        };
        // Held for an agent rather than a computer: starting a task, or looking.
        match cap {
            "agents" => return Some(text("goal").map_or("start an agent task".into(), |g| format!("start the agent task {}", quoted(&g)))),
            "observe" => {
                return Some(match (action.get("op").and_then(Value::as_str), text("path"), text("dir")) {
                    (Some("read"), Some(path), _) => format!("read the file {}", quoted(&path)),
                    (Some("list"), _, Some(dir)) => format!("look at the files in {}", quoted(&dir)),
                    (Some("list"), _, None) => "look at its files here".into(),
                    (None, ..) => "look at the screen".into(),
                    _ => return None,
                });
            }
            _ => {}
        }
        let subject = || text("subject").map(|s| if s == asking { "itself".to_string() } else { s });
        Some(match action.get("op").and_then(Value::as_str)? {
            "access_set" => {
                let (who, can) = (subject()?, capability_words(action.get("capability").and_then(Value::as_str)?)?);
                match action.get("rule").and_then(Value::as_str)? {
                    "allow" => format!("let {who} {can}"),
                    "ask" => format!("make {who} ask first to {can}"),
                    "deny" => format!("stop {who} being able to {can}"),
                    _ => return None,
                }
            }
            "access_remove" => match action.get("capability").and_then(Value::as_str) {
                Some(capability) => format!("reset whether {} may {}", subject()?, capability_words(capability)?),
                None => format!("reset what {} may do here", subject()?),
            },
            "access_unpair" => format!("remove {} from this computer", subject()?),
            "answer_attention" => {
                let asked = text("att_ref").and_then(|r| self.journal.get_attention(&r).ok().flatten());
                if let Some(question) = asked.as_ref().filter(|a| a.kind == "question") {
                    let what = quoted(question.question.trim_end_matches(['.', '?']));
                    return Some(if action.get("dismiss") == Some(&Value::Bool(true)) {
                        format!("dismiss the question {what}")
                    } else {
                        format!("answer the question {what} with {}", quoted(&text("answer")?))
                    });
                }
                let verb = match action.get("answer").and_then(Value::as_str)? {
                    "approve" => "approve",
                    "deny" => "deny",
                    "always" => "always allow",
                    _ => return None,
                };
                // One level only: a request to answer a request to answer is "another request".
                let inner = text("att_ref")
                    .and_then(|r| self.journal.get_attention(&r).ok().flatten())
                    .filter(|inner| inner.details.pointer("/request/op").and_then(Value::as_str) != Some("answer_attention"));
                match inner {
                    Some(inner) => format!("{verb} the request {}", quoted(inner.question.trim_end_matches('.'))),
                    None => format!("{verb} another request"),
                }
            }
            "take_control" => "take control".into(),
            "pause" => "pause the agent working here".into(),
            "resume" => "let the paused agent here carry on".into(),
            "viewer_enroll" => "set up taking control of this computer".into(),
            "observe" => "watch the screen".into(),
            // Watch that asks first: one approval for a sitting of pictures and reads.
            "watch" => "watch this computer: its screen, agent tasks and history".into(),
            "task_status" => task("follow"),
            "files_roots" => "see which folders it can use here".into(),
            "files_list" => text("relative_path").map_or("look at the files here".into(), |p| format!("look at the files in {}", quoted(&p))),
            "files_begin_upload" => text("name").map_or("send a file here".into(), |n| format!("send the file {} here", quoted(&n))),
            "files_begin_download" => text("relative_path").map_or("get a file from here".into(), |p| format!("get {} from here", quoted(&p))),
            op if FILE_OPS.contains(&op) => "send or get files".into(),
            "logs" => "read this computer's logs".into(),
            "health" => "see how this computer is doing".into(),
            "power" => match action.get("action").and_then(Value::as_str)? {
                "restart" => "restart this computer".into(),
                "shutdown" => "shut down this computer".into(),
                "sleep" => "put this computer to sleep".into(),
                "lock" => "lock this computer's screen".into(),
                "update" => "update this computer".into(),
                _ => return None,
            },
            "settings" => {
                let args: Vec<&str> = action.get("args").and_then(Value::as_array)?.iter().filter_map(Value::as_str).collect();
                let title = |key: &str| crate::settings::title(key).map(quoted);
                match args.as_slice() {
                    ["get", ..] => "read this computer's settings".into(),
                    ["set", key, value] => format!("change the setting {} to {}", title(key)?, quoted(value)),
                    ["reset", "--section", section] => format!("reset the {} settings", title(section)?),
                    ["reset", key] => format!("reset the setting {}", title(key)?),
                    _ => return None,
                }
            }
            "theme_apply" => format!("switch this computer to the theme {}", quoted(&text("name")?)),
            "theme_upload" => format!("install the theme {}", quoted(&text("name")?)),
            "repair" => match action.get("fix").and_then(Value::as_str)? {
                "restart_ibara" => "restart ibara on this computer".into(),
                "reconnect_display" => "give this computer a screen again".into(),
                "restart_viewer" => "restart screen sharing on this computer".into(),
                _ => return None,
            },
            "timeline" => "read this computer's history".into(),
            "send_wake" => "have this computer send a wake-up signal on its network".into(),
            "tasks" => "see the agent tasks here".into(),
            "task" => task("see"),
            "artifacts" => "see the agent results here".into(),
            "procedures" => "see the saved procedures here".into(),
            "procedure" => procedure("see"),
            "task_extend" => {
                let seconds = action.get("extra_seconds").and_then(|s| s.as_i64().or_else(|| s.as_str()?.trim().parse().ok()))?;
                let minutes = (seconds.clamp(1, 86_400) + 59) / 60;
                let unit = if minutes == 1 { "minute" } else { "minutes" };
                format!("{} {minutes} more {unit}", task("give"))
            }
            "task_revoke" => task("stop"),
            "procedure_review" => match action.get("decision").and_then(Value::as_str)? {
                "approve" => procedure("approve"),
                "quarantine" => procedure("set aside"),
                _ => return None,
            },
            "artifact_transfer" => "collect an agent result".into(),
            _ => return None,
        })
    }
}

/// What a capability lets a computer do, as the end of "may …".
fn capability_words(capability: &str) -> Option<&'static str> {
    Some(match capability {
        "watch" => "watch the screen",
        "files" => "send or get files",
        "control" => "take control",
        "agents" => "run agent tasks",
        "administer" => "manage this computer",
        _ => return None,
    })
}
