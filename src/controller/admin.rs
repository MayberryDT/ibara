//! Administrator operations (`core.ts:539-642`, `2317-2644`) with the same
//! reply shapes the console and lifecycle parse, plus the completion
//! projection and receipt summaries they show.

use crate::access::Rule;
use super::control::Release;
use super::ports::OutputChange;
use super::{Controller, clip, is_principal, is_ref, is_stable_identity};
use crate::error::{IbaraError, Result, invalid};
use crate::ids::millis_from_iso;
use crate::store::{ControlPatch, GrantRecord, OperationRecord, TaskRecord};
use serde_json::{Value, json};
use std::path::Path;

fn fail(code: &'static str, message: &str, retry_safe: bool) -> IbaraError {
    IbaraError::new(code, message, retry_safe)
}

fn s<'a>(action: &'a Value, key: &str) -> &'a str {
    action.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `Number(x || default)`.
fn n(action: &Value, key: &str, default: f64) -> f64 {
    match action.get(key) {
        Some(Value::Number(v)) => v.as_f64().filter(|v| *v != 0.0).unwrap_or(default),
        Some(Value::String(v)) if !v.is_empty() => v.parse().unwrap_or(f64::NAN),
        _ => default,
    }
}

impl Controller {
    /// One administrator operation (`admin.sock`).
    pub async fn admin(&self, action: Value) -> Result<Value> {
        self.ensure_open()?;
        self.reconcile().await?;
        let op = s(&action, "op").to_string();
        match op.as_str() {
            "access" => self.access_view("owner"),
            "access_set" | "access_remove" | "access_unpair" => self.change_access("owner", &action).await,
            "access_sync" => { *self.access_projection.borrow_mut()=None; self.apply_saved_access().await?; self.access_view("owner") },
            "status" => {
                let caps = self.refresh_capabilities().await?;
                Ok(json!({
                    "availability": self.availability()?,
                    "capabilities": caps,
                    "lease": self.journal.get_active_lease()?,
                    "control": self.journal.get_control()?,
                    "epoch": self.epoch,
                    "repair": self.repair_status(),
                }))
            }
            "doctor" => {
                let caps = self.refresh_capabilities().await?;
                Ok(json!({
                    "availability": self.availability()?,
                    "capabilities": caps,
                    "session_hint": self.desktop.session_available(),
                    "epoch": self.epoch,
                    "queue_depth": self.queue_depth.get(),
                    "note": "Doctor does not inject input or capture the screen.",
                }))
            }
            "endpoint" => Ok(json!({
                "endpoint_id": self.endpoint_id,
                "protocols": ["4"],
                "pairing": "requires_client_status_and_image_roundtrip",
            })),
            "outputs" => {
                if !self.desktop.session_available() {
                    return Ok(json!({ "outputs": [], "desktop_ready": false }));
                }
                self.desktop.session_ready().await?;
                let outputs: Vec<Value> = self
                    .desktop
                    .outputs()
                    .await?
                    .into_iter()
                    .map(|o| json!({ "display_id": o.display_id, "label": o.label, "display_revision": o.display_revision }))
                    .collect();
                Ok(json!({ "outputs": outputs, "desktop_ready": true, "endpoint_id": self.endpoint_id, "controller_epoch": self.epoch }))
            }
            "register_collector" => {
                let (principal, identity) = (s(&action, "principal"), s(&action, "host_id"));
                if !is_principal(principal) || !is_stable_identity(identity) {
                    return Err(invalid("Expected principal and stable host identity."));
                }
                self.journal.register_collector(principal, identity)?;
                Ok(json!({ "principal": principal, "host_id": identity, "authority": "collector_identity_only" }))
            }
            "amend_delivery" => self.amend_delivery(&action),
            "reconcile_operation" => self.reconcile_operation(&action),
            // `computerctl pause` is a person's pause; start and shutdown use `system_pause`.
            "pause" => self.admin_pause(crate::store::PauseOrigin::Person).await,
            "resume" => self.admin_resume().await,
            "tasks" => {
                let principal = action.get("principal").and_then(Value::as_str);
                let tasks = self.journal.list_tasks(principal, 100)?;
                let summaries = tasks.iter().map(|t| self.operator_task_summary(t)).collect::<Result<Vec<_>>>()?;
                Ok(json!({ "tasks": summaries }))
            }
            "task" => self.admin_task(s(&action, "task_ref")),
            "receipts" => {
                let cursor = action.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty());
                self.admin_receipts(s(&action, "task_ref"), n(&action, "limit", 20.0), cursor)
            }
            "operation" => {
                let reference = [s(&action, "request_id"), s(&action, "operation_ref")].into_iter().find(|r| !r.is_empty()).unwrap_or("");
                if reference.is_empty() {
                    return Err(invalid("request_id or operation_ref is required."));
                }
                let op = match self.journal.get_operation_by_ref(reference)? {
                    Some(op) => Some(op),
                    None => self.journal.get_operation_by_request_id(reference, None)?,
                };
                let op = op.ok_or_else(|| invalid("Unknown operation."))?;
                Ok(json!({
                    "ok": true,
                    "operation": self.receipt_summary(&op, true)?,
                    "reconciliations": self.journal.read_audit(&format!("operation:{}", op.operation_ref))?,
                }))
            }
            "artifacts" => {
                let cursor = action.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty());
                let page = self.storage.list_published_artifacts(n(&action, "limit", 20.0), cursor)?;
                Ok(json!({ "ok": true, "items": page.get("items"), "next_cursor": page.get("next_cursor"), "total": page.get("total") }))
            }
            "artifact" => {
                let artifact_ref = s(&action, "artifact_ref");
                if artifact_ref.is_empty() {
                    return Err(invalid("artifact_ref is required."));
                }
                let page = self.storage.list_published_artifacts(1000.0, None)?;
                let item = page
                    .get("items")
                    .and_then(Value::as_array)
                    .and_then(|items| items.iter().find(|i| i.get("artifact_ref").and_then(Value::as_str) == Some(artifact_ref)).cloned())
                    .ok_or_else(|| invalid("Unknown artifact_ref."))?;
                Ok(json!({ "ok": true, "artifact": item }))
            }
            "procedures" => Ok(json!({ "ok": true, "items": self.storage.list_procedures("operator")? })),
            "procedure" => {
                let procedure_ref = s(&action, "procedure_ref");
                if procedure_ref.is_empty() {
                    return Err(invalid("procedure_ref is required."));
                }
                let record = self.storage.get_procedure_record(procedure_ref, "operator")?.ok_or_else(|| invalid("Unknown procedure_ref."))?;
                Ok(json!({ "ok": true, "procedure": record }))
            }
            "telemetry" => Ok(self.telemetry()),
            "logs" => admin_logs(s(&action, "unit"), n(&action, "lines", 80.0)),
            "extend" => self.admin_extend(s(&action, "task_ref"), n(&action, "extra_seconds", 0.0)),
            "share_task" => {
                let task = self.journal.get_task(s(&action, "task_ref"))?.ok_or_else(|| invalid("Unknown task_ref."))?;
                let shared = s(&action, "visibility") == "shared";
                let group = shared.then(|| s(&action, "group").to_string());
                if shared && !group.as_deref().is_some_and(is_ref) {
                    return Err(invalid("A stable group is required for sharing."));
                }
                let updated = self.journal.set_task_sharing(&task.task_ref, if shared { "shared" } else { "private" }, group.as_deref(), &self.now_iso())?;
                self.revoke_unauthorized_lease().await?;
                Ok(json!({ "task": updated }))
            }
            "grant" => {
                if crate::access::Access::load(&self.journal)?.is_some() { return Err(invalid("Use access_set; group grants no longer grant computer access.")); }
                let (group, member, peer_key) = (s(&action, "group"), s(&action, "principal"), s(&action, "peer_key"));
                if !is_ref(group) || !is_principal(member) || !is_stable_identity(peer_key) {
                    return Err(invalid("Grant requires stable group, principal and peer identity."));
                }
                self.journal.put_grant(&GrantRecord {
                    group: group.into(),
                    principal: member.into(),
                    role: "agent".into(),
                    state: "active".into(),
                    peer_key: peer_key.into(),
                    updated_at: self.now_iso(),
                })?;
                Ok(json!({ "grants": self.journal.list_grants(Some(group))? }))
            }
            "revoke_grant" => {
                let (group, member) = (s(&action, "group"), s(&action, "principal"));
                let prior = self.journal.list_grants(Some(group))?.into_iter().find(|g| g.principal == member).ok_or_else(|| invalid("Unknown grant."))?;
                self.journal.put_grant(&GrantRecord { state: "revoked".into(), updated_at: self.now_iso(), ..prior })?;
                self.revoke_unauthorized_lease().await?;
                Ok(json!({ "grants": self.journal.list_grants(Some(group))? }))
            }
            "grants" => {
                let group = action.get("group").and_then(Value::as_str).filter(|g| !g.is_empty());
                Ok(json!({ "grants": self.journal.list_grants(group)? }))
            }
            "operator_access" => {
                if let Some(a)=crate::access::Access::load(&self.journal)? {
                    let operators:Vec<_>=a.pairings.iter().map(|(id,p)|json!({"principal":id,"enabled":p.active,"active":p.active,"generation":p.generation,
                        "observe":a.rule(id,"watch",self.now_ms())==Rule::Allow,"files":a.rule(id,"files",self.now_ms())==Rule::Allow,
                        "take_control":a.rule(id,"control",self.now_ms())==Rule::Allow})).collect();
                    return Ok(json!({"operators":operators}));
                }
                let grants = (self.operator_grants)();
                let mut ids: Vec<&String> = grants.keys().collect();
                ids.sort();
                let now = self.now_ms();
                let operators: Vec<Value> = ids
                    .into_iter()
                    .filter_map(|id| {
                        let raw = grants.get(id)?;
                        let grant = super::OperatorGrant::from_value(raw)?;
                        Some(json!({
                            "principal": id,
                            "enabled": grant.enabled,
                            "active": grant.active(now),
                            "expires_at": grant.expires_at,
                            "observe": grant.observe,
                            "files": grant.files,
                            "take_control": self.stream.is_some() && self.viewer_identity(id, &grant).is_some(),
                            "generation": grant.generation,
                        }))
                    })
                    .collect();
                Ok(json!({ "operators": operators }))
            }
            "revoke" => {
                let task_ref = action.get("task_ref").and_then(Value::as_str).filter(|t| !t.is_empty());
                if let Some(lease) = self.journal.get_active_lease()?
                    && task_ref.is_none_or(|t| t == lease.task_ref)
                {
                    self.release_lease(Some(lease), Release::OperatorRevoke, false).await?;
                }
                Ok(json!({ "ok": true, "availability": self.availability()? }))
            }
            "transfer" => {
                let connection_id = action.get("connection_id").and_then(Value::as_str);
                let request = action.get("request").cloned().unwrap_or_else(|| json!({}));
                self.storage.transfer(s(&action, "principal"), &request, connection_id)
            }
            "set_session" => {
                let available = action.get("available").is_some_and(truthy);
                self.journal.set_control(ControlPatch { session_hint: Some(available), ..Default::default() })?;
                Ok(json!({ "ok": true, "session_hint": available }))
            }
            "approve_procedure" | "quarantine_procedure" => self.admin_procedure(&op, s(&action, "procedure_ref"), action.get("expected_sha256").and_then(Value::as_str)),
            "reconcile_display" => self.reconcile_display().await,
            "attention" => {
                let state = action.get("state").and_then(Value::as_str).filter(|v| !v.is_empty()).or(Some("open"));
                let task_ref = action.get("task_ref").and_then(Value::as_str).filter(|v| !v.is_empty());
                Ok(json!({ "items": self.journal.list_attention(state, task_ref, 100)? }))
            }
            "answer_attention" => {
                let att_ref = s(&action, "att_ref");
                let answer = s(&action, "answer");
                if !is_ref(att_ref) || answer.trim().is_empty() {
                    return Err(invalid("answer_attention requires att_ref and a non-empty answer."));
                }
                let by = action.get("answered_by").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("operator");
                let (item, allowed) = self.answer_approval(att_ref, &clip(answer, 1000), by).await?;
                self.push_event(Some(&item.task_ref), &format!("{att_ref} answered: {}", clip(answer, 80)));
                Ok(json!({ "ok": true, "item": item, "allowed": allowed }))
            }
            _ => Err(invalid("Unknown admin operation.")),
        }
    }

    fn amend_delivery(&self, action: &Value) -> Result<Value> {
        let task = self.journal.get_task(s(action, "task_ref"))?;
        let obligation_id = action.get("obligation_id");
        let expected = action.get("expected_revision");
        let Some(mut task) = task else {
            return Err(fail("REQUEST_CONFLICT", "Delivery obligation changed; review its current revision.", true));
        };
        let index = task.deliveries.iter().position(|d| d.get("id") == obligation_id && obligation_id.is_some());
        let Some(index) = index.filter(|i| expected.is_some() && task.deliveries[*i].get("revision").and_then(Value::as_f64) == expected.and_then(Value::as_f64)) else {
            return Err(fail("REQUEST_CONFLICT", "Delivery obligation changed; review its current revision.", true));
        };
        let destination = s(action, "destination_path");
        let normalized = Path::new(destination).components().collect::<std::path::PathBuf>();
        if !is_stable_identity(s(action, "host_id")) || !Path::new(destination).is_absolute() || normalized.to_str() != Some(destination) {
            return Err(invalid("Use a stable host identity and normalized absolute destination."));
        }
        let previous = task.deliveries[index].clone();
        self.journal.append_audit(&format!("delivery:{}", task.task_ref), json!({ "previous": previous, "actor": "operator", "at": self.now_iso() }))?;
        let revision = previous.get("revision").and_then(Value::as_i64).unwrap_or(0) + 1;
        if let Some(obj) = task.deliveries[index].as_object_mut() {
            obj.insert("host_id".into(), json!(s(action, "host_id")));
            obj.insert("destination_path".into(), json!(destination));
            obj.insert("revision".into(), json!(revision));
        }
        self.journal.put_task(&task)?;
        Ok(json!({ "delivery": task.deliveries[index], "completion_projection": self.completion_state(&task)? }))
    }

    fn reconcile_operation(&self, action: &Value) -> Result<Value> {
        let operation = self.journal.get_operation_by_ref(s(action, "operation_ref"))?;
        let resolution = s(action, "resolution");
        let Some(operation) = operation.filter(|_| matches!(resolution, "confirmed" | "not_occurred" | "abandoned")) else {
            return Err(invalid("Expected retained operation and an explicit reconciliation resolution."));
        };
        let refs: Vec<String> = action
            .get("evidence_refs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if resolution != "abandoned" {
            let valid = !refs.is_empty()
                && refs.iter().all(|r| {
                    self.journal
                        .lookup_evidence(r)
                        .ok()
                        .flatten()
                        .is_some_and(|e| e.task_ref == operation.task_ref && !e.expired)
                });
            if !valid {
                return Err(fail("POSTCONDITION_FAILED", "Reconciliation requires retained evidence from the original task.", false));
            }
        }
        let reconciliation = json!({
            "operation_ref": operation.operation_ref,
            "request_id": operation.request_id,
            "resolution": resolution,
            "evidence_refs": refs,
            "actor": "operator",
            "at": self.now_iso(),
            "note": clip(s(action, "note"), 1000),
            "authorizes_replay": false,
        });
        let history = self.journal.append_audit(&format!("operation:{}", operation.operation_ref), reconciliation.clone())?;
        Ok(json!({ "reconciliation": reconciliation, "history": history, "original_receipt": operation.receipt }))
    }

    /// `adminReconcileDisplay` (`core.ts:2340-2361`). Creating `IbaraVirtual`
    /// takes nothing from a person, so a pause (every start and every viewer
    /// fault pauses, which also sets `human_control`) or an unsettled
    /// controller does not hold it back: a computer that started without a
    /// display needs the output before its viewer can settle. Removing it
    /// waits until the controller is resumed and settled. Nothing changes
    /// under a lease or while a person holds control through the viewer, nor
    /// while control is being settled: the settlement counts every effect
    /// queued behind it as work that may still run, and this one would
    /// leave the computer unsettled.
    async fn reconcile_display(&self) -> Result<Value> {
        let control = self.journal.get_control()?;
        let viewer_held = self.viewer_state.borrow().owner.is_some();
        if self.journal.get_active_lease()?.is_some() || viewer_held || self.display_maintenance.get() || self.settling.get() {
            return Ok(json!({ "deferred": true }));
        }
        let removal_allowed = !control.human_control && !control.paused && !control.unsettled;
        self.display_maintenance.set(true);
        let result = self
            .enqueue(async {
                match self.desktop.output_change().await? {
                    None => Ok(json!({ "deferred": false, "changed": false })),
                    Some(OutputChange::Remove) if !removal_allowed => Ok(json!({ "deferred": true })),
                    Some(change) => {
                        self.desktop.apply_output_change(change).await?;
                        self.frames.borrow_mut().clear();
                        Ok(json!({ "deferred": false, "changed": true }))
                    }
                }
            })
            .await;
        self.display_maintenance.set(false);
        match result {
            Err(e) if e.code == "CAPABILITY_UNAVAILABLE" && e.message.contains("unsupported") => Ok(json!({ "deferred": true, "reason": "unsupported" })),
            result => result,
        }
    }

    /// `adminExtend` (`core.ts:2379-2398`).
    fn admin_extend(&self, task_ref: &str, extra: f64) -> Result<Value> {
        if task_ref.is_empty() || extra.fract() != 0.0 || !(1.0..=86_400.0).contains(&extra) {
            return Err(invalid("extend requires task_ref and integer extra_seconds from 1 to 86400."));
        }
        let mut task = self.journal.get_task(task_ref)?.ok_or_else(|| invalid("Unknown task_ref."))?;
        let Some(current) = task.budgets.get("active_control_seconds").and_then(Value::as_f64) else {
            return Ok(json!({
                "ok": true,
                "already_unlimited": true,
                "note": "Task already has unlimited cumulative control.",
                "budgets": self.budgets_for(&task),
            }));
        };
        let extended = current + extra;
        if extended > 86_400.0 {
            return Err(invalid("Extended finite active-control budget cannot exceed 86400 seconds."));
        }
        if let Some(obj) = task.budgets.as_object_mut() {
            obj.insert("active_control_seconds".into(), json!(extended));
        }
        task.updated_at = self.now_iso();
        self.journal.put_task(&task)?;
        Ok(json!({ "ok": true, "budgets": self.budgets_for(&task) }))
    }

    /// `adminProcedure` (`core.ts:2406-2433`).
    fn admin_procedure(&self, op: &str, procedure_ref: &str, expected: Option<&str>) -> Result<Value> {
        if procedure_ref.is_empty() {
            return Err(invalid("procedure_ref is required."));
        }
        let approve = op == "approve_procedure";
        if approve {
            let expected = expected.unwrap_or("");
            if expected.len() != 64 || !expected.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(invalid("Approval requires the exact reviewed candidate digest."));
            }
            let current = self.storage.get_procedure_record(procedure_ref, "operator")?;
            let digest = current.as_ref().and_then(|c| c.get("source_sha256")).and_then(Value::as_str).unwrap_or("").to_string();
            if digest.is_empty() || digest != expected {
                return Err(fail("REQUEST_CONFLICT", "Procedure contents changed since review; inspect the current candidate before approving.", true)
                    .with("expected_sha256", expected)
                    .with("current_sha256", if digest.is_empty() { Value::Null } else { json!(digest) }));
            }
        }
        let mut ctx = crate::storage::Context::new("admin", "operator", &self.epoch);
        ctx.signal = Some(self.abort_handles().1);
        let action = json!({ "kind": if approve { "approve" } else { "quarantine" }, "procedure_ref": procedure_ref, "expected_sha256": expected });
        let result = self.storage.procedures(&action, &ctx)?;
        Ok(json!({ "ok": true, "records": result.records }))
    }

    fn admin_task(&self, task_ref: &str) -> Result<Value> {
        if task_ref.is_empty() {
            return Err(invalid("task_ref is required."));
        }
        let task = self.journal.get_task(task_ref)?.ok_or_else(|| invalid("Unknown task_ref."))?;
        let lease = self.journal.get_active_lease()?;
        let receipts = self.journal.list_operations_for_task(task_ref, 20, None)?;
        let artifacts: Vec<Value> = self
            .journal
            .list_artifacts(task_ref)?
            .into_iter()
            .take(30)
            .map(|a| {
                json!({
                    "artifact_ref": a.get("artifact_ref").or_else(|| a.get("ref")),
                    "name": a.get("name"),
                    "delivery": a.get("delivery"),
                    "size_bytes": a.get("size_bytes"),
                    "sha256": a.get("sha256"),
                })
            })
            .collect();
        let claims: Vec<Value> = task.completion.as_ref().and_then(|c| c.get("criteria")).and_then(Value::as_array).cloned().unwrap_or_default();
        let criteria = task
            .success_criteria
            .iter()
            .map(|criterion| {
                let id = criterion.get("id").cloned().unwrap_or(Value::Null);
                let mine: Vec<Value> = claims.iter().filter(|c| c.get("criterion_id") == Some(&id)).cloned().collect();
                let mut refs: Vec<String> = Vec::new();
                for claim in &mine {
                    for r in claim.get("evidence_refs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                        if !refs.iter().any(|x| x == r) && refs.len() < 20 {
                            refs.push(r.to_string());
                        }
                    }
                }
                let single = TaskRecord { success_criteria: vec![criterion.clone()], ..task.clone() };
                let satisfied = self.criteria_satisfied(&single, &mine).map(|(_, met)| met > 0).unwrap_or(false);
                let evidence: Vec<Value> = refs
                    .iter()
                    .map(|r| {
                        let item = self.journal.lookup_evidence(r).ok().flatten();
                        let rec = item.as_ref().map(|i| &i.record);
                        json!({
                            "ref": r,
                            "kind": item.as_ref().map_or("missing", |i| i.kind.as_str()),
                            "expired": item.as_ref().is_none_or(|i| i.expired),
                            "source": rec.and_then(|r| r.get("source")),
                            "outcome": rec.and_then(|r| r.get("outcome")),
                            "checked_at": rec.and_then(|r| r.get("checked_at")),
                            "predicate": rec.and_then(|r| r.get("predicate")),
                            "limitation": rec.and_then(|r| r.get("limitation")),
                            "summary": clip(rec.and_then(|r| r.get("summary")).and_then(Value::as_str).unwrap_or(""), 300),
                        })
                    })
                    .collect();
                json!({
                    "id": id,
                    "description": criterion.get("description"),
                    "required": criterion.get("required") != Some(&Value::Bool(false)),
                    "policy": criterion.get("proof").or_else(|| criterion.get("check")).cloned().unwrap_or(Value::Null),
                    "state": if satisfied { "satisfied" } else { "unverified" },
                    "claims": mine,
                    "evidence": evidence,
                })
            })
            .collect::<Vec<_>>();
        let deliveries: Vec<Value> = task
            .deliveries
            .iter()
            .map(|d| {
                let mut d = d.clone();
                let verified = self.storage.delivery_verified(&task.task_ref, &d);
                if let Some(obj) = d.as_object_mut() {
                    obj.insert("state".into(), json!(if verified { "verified" } else { "pending" }));
                }
                d
            })
            .collect();
        let jobs: Vec<Value> = match self.storage.task_jobs(task_ref, None) {
            Ok(jobs) if !jobs.is_empty() => jobs,
            _ => self.journal.list_jobs(task_ref)?,
        };
        let live = lease.as_ref().is_some_and(|l| l.task_ref == task.task_ref && l.state == "active");
        Ok(json!({
            "ok": true,
            "task": self.operator_task_summary(&task)?,
            "completion_projection": self.completion_state(&task)?,
            "criteria": criteria,
            "deliveries": deliveries,
            "live": live,
            "lease": lease.filter(|l| l.task_ref == task.task_ref),
            "receipts": receipts.items.iter().map(|op| self.receipt_summary(op, false)).collect::<Result<Vec<_>>>()?,
            "receipt_next_cursor": receipts.next_cursor,
            "artifacts": artifacts,
            "jobs": jobs.into_iter().take(30).collect::<Vec<_>>(),
        }))
    }

    fn admin_receipts(&self, task_ref: &str, limit: f64, cursor: Option<&str>) -> Result<Value> {
        if task_ref.is_empty() {
            return Err(invalid("task_ref is required."));
        }
        if self.journal.get_task(task_ref)?.is_none() {
            return Err(invalid("Unknown task_ref."));
        }
        let limit = if limit.is_finite() { limit.floor() as i64 } else { 20 };
        let page = self.journal.list_operations_for_task(task_ref, limit, cursor)?;
        Ok(json!({
            "ok": true,
            "task_ref": task_ref,
            "items": page.items.iter().map(|op| self.receipt_summary(op, false)).collect::<Result<Vec<_>>>()?,
            "next_cursor": page.next_cursor,
            "total": page.total,
        }))
    }

    fn telemetry(&self) -> Value {
        let mem = read_key_values("/proc/meminfo");
        let load = std::fs::read_to_string("/proc/loadavg").ok().and_then(|t| t.lines().next().map(str::to_string));
        let loads: Vec<Option<f64>> = (0..3).map(|i| load.as_deref().and_then(|l| l.split_whitespace().nth(i)).and_then(|v| v.parse().ok())).collect();
        let number = |key: &str| mem.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect::<String>().parse::<f64>().ok());
        json!({
            "ok": true,
            "observed_at": self.now_iso(),
            "cpu": { "loadavg_1": loads[0], "loadavg_5": loads[1], "loadavg_15": loads[2] },
            "memory": {
                "mem_total_kb": number("MemTotal"),
                "mem_available_kb": number("MemAvailable"),
                "swap_total_kb": number("SwapTotal"),
                "swap_free_kb": number("SwapFree"),
            },
            "disk": read_disk("/"),
            "queue_depth": self.queue_depth.get(),
        })
    }

    /// Task budgets with the remaining active-control seconds (`budgetsFor`).
    fn budgets_for(&self, task: &TaskRecord) -> Value {
        let mut budgets = task.budgets.clone();
        let remaining = super::control::control_limit_ms(task).map(|limit| ((limit - task.active_control_used_ms as f64) / 1000.0).ceil().max(0.0));
        if let Some(obj) = budgets.as_object_mut() {
            obj.insert("active_control_seconds".into(), json!(remaining));
        }
        budgets
    }

    /// `operatorTaskSummary` (`core.ts:1353-1375`).
    pub(crate) fn operator_task_summary(&self, task: &TaskRecord) -> Result<Value> {
        let lease = self.journal.get_active_lease()?;
        let live = lease.is_some_and(|l| l.task_ref == task.task_ref && l.state == "active");
        let completion = task.completion.clone().unwrap_or_else(|| json!({}));
        Ok(json!({
            "task_ref": task.task_ref,
            "principal": task.principal,
            "state": task.state,
            "goal": clip(&task.goal, 400),
            "contract_version": task.contract_version,
            "visibility": task.visibility,
            "owner_group": task.owner_group,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "live": live,
            "budgets": self.budgets_for(task),
            "last_checkpoint_ref": task.last_checkpoint_ref,
            "completion_outcome": completion.get("outcome").cloned().unwrap_or(Value::Null),
            "completion_summary": completion.get("summary").and_then(Value::as_str).map(|s| clip(s, 400)),
            "verification": self.completion_state(task)?,
            "unresolved_request_ids": self.unresolved_requests(&task.task_ref),
        }))
    }

    /// `receiptSummary` (`core.ts:2574-2596`).
    fn receipt_summary(&self, op: &OperationRecord, detail: bool) -> Result<Value> {
        let mut receipt = if op.receipt.is_object() { op.receipt.clone() } else { json!({}) };
        if matches!(receipt.get("job_state").and_then(Value::as_str), None | Some("running"))
            && let Some(job_ref) = receipt.get("job_ref").and_then(Value::as_str)
        {
            // Older receipts replaced the command with this sentence. Jobs
            // retain state and output, but not commands, so keep that summary.
            let old_state = receipt.get("summary").and_then(Value::as_str)
                .and_then(|s| s.strip_prefix(&format!("Job {job_ref} is ")))
                .and_then(|s| s.strip_suffix('.'))
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let state = match old_state {
                Some(state) => Some(state),
                None => self.storage.get_job(job_ref, op.task_ref.as_deref(), Some(&op.principal))?
                    .and_then(|j| j.get("state").and_then(Value::as_str).map(str::to_string)),
            };
            if let Some(state) = state {
                receipt["job_state"] = json!(state);
            }
        }
        let audits = self.journal.read_audit(&format!("operation:{}", op.operation_ref))?;
        let last = audits.last().and_then(|a| a.get("resolution")).and_then(Value::as_str);
        let dependency = if last == Some("abandoned") {
            "abandoned"
        } else if self.operation_needs_resolution(op, op.task_ref.as_deref()) {
            "unresolved"
        } else {
            "settled"
        };
        let error = receipt.get("error").filter(|e| !e.is_null()).map(|e| {
            let text = e.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()).or_else(|| e.get("code").and_then(Value::as_str));
            clip(text.map_or_else(|| e.to_string(), str::to_string).as_str(), 400)
        });
        let mut summary = json!({
            "operation_ref": op.operation_ref,
            "request_id": op.request_id,
            "principal": op.principal,
            "task_ref": op.task_ref,
            "tool": op.tool,
            "created_at": op.created_at,
            "updated_at": op.updated_at,
            "dispatched": op.dispatched,
            "execution": receipt.get("execution"),
            "verification": receipt.get("verification"),
            "effect": receipt.get("effect"),
            "outcome": receipt.get("outcome").or_else(|| receipt.get("step").and_then(|s| s.get("outcome"))),
            "summary": receipt.get("summary").and_then(Value::as_str).map(|s| clip(s, 400)),
            "error": error,
            "effect_class": op.effect_class,
            "job_state": receipt.get("job_state"),
            "reconciliations": audits,
            "dependency_state": dependency,
        });
        if detail {
            summary["receipt"] = receipt;
        }
        Ok(summary)
    }

    /// `operationNeedsResolution` (`core.ts:1940-1950`).
    pub(crate) fn operation_needs_resolution(&self, op: &OperationRecord, task_ref: Option<&str>) -> bool {
        let r = &op.receipt;
        let execution = r.get("execution").and_then(Value::as_str);
        let verification = r.get("verification").and_then(Value::as_str);
        if execution != Some("running") && execution != Some("unknown") && verification != Some("pending") {
            return false;
        }
        if execution == Some("running") {
            return true;
        }
        let audits = self.journal.read_audit(&format!("operation:{}", op.operation_ref)).unwrap_or_default();
        let Some(resolution) = audits.last() else { return true };
        let refs: Vec<&str> = resolution.get("evidence_refs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if resolution.get("resolution").and_then(Value::as_str) == Some("abandoned") || refs.is_empty() {
            return true;
        }
        let Some(task_ref) = task_ref else { return true };
        refs.iter().any(|r| {
            self.journal
                .lookup_evidence(r)
                .ok()
                .flatten()
                .is_none_or(|e| e.task_ref.as_deref() != Some(task_ref) || e.expired)
        })
    }

    /// `unresolvedRequests` (`core.ts:1933-1938`).
    pub(crate) fn unresolved_requests(&self, task_ref: &str) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for op in self.journal.unresolved_operations(task_ref).unwrap_or_default() {
            if self.operation_needs_resolution(&op, Some(task_ref)) && !ids.contains(&op.request_id) {
                ids.push(op.request_id);
            }
        }
        ids
    }

    /// Required criteria and how many are satisfied. Contract-4 tasks carry
    /// their check states; older tasks are judged by their claims and proofs
    /// (`completionState`, `core.ts:1952-1995`).
    fn criteria_satisfied(&self, task: &TaskRecord, claims: &[Value]) -> Result<(usize, usize)> {
        let required: Vec<&Value> = task.success_criteria.iter().filter(|c| c.get("required") != Some(&Value::Bool(false))).collect();
        if task.contract_version == "4" {
            let met = required.iter().filter(|c| c.get("state").and_then(Value::as_str) == Some("met")).count();
            return Ok((required.len(), met));
        }
        let now = self.now_ms();
        let last_effect = self.journal.last_effect_at(&task.task_ref)?.as_deref().and_then(millis_from_iso).unwrap_or(0);
        let checks = self.journal.list_checks(&task.task_ref)?;
        let mut met: Vec<String> = Vec::new();
        for claim in claims {
            let criterion_id = claim.get("criterion_id");
            let criterion = task.success_criteria.iter().find(|c| c.get("id") == criterion_id && criterion_id.is_some());
            let policy = criterion.and_then(|c| c.get("proof")).filter(|p| p.is_object());
            let historical = policy.and_then(|p| p.get("freshness")).and_then(Value::as_str) == Some("historical");
            let max_age = match policy.and_then(|p| p.get("max_age_seconds")).and_then(Value::as_f64) {
                None if historical => f64::INFINITY,
                Some(seconds) => seconds * 1000.0,
                None => 300_000.0,
            };
            let claimed: Vec<&str> = claim.get("evidence_refs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let records: Vec<Value> = claimed
                .iter()
                .filter_map(|r| self.journal.lookup_evidence(r).ok().flatten())
                .filter(|e| e.task_ref.as_deref() == Some(task.task_ref.as_str()) && !e.expired)
                .map(|e| e.record)
                .collect();
            let floor = if historical { 0 } else { last_effect };
            let predicate = policy.and_then(|p| p.get("predicate")).filter(|p| !p.is_null());
            let fresh = |r: &Value| {
                r.get("checked_at").and_then(Value::as_str).and_then(millis_from_iso).is_some_and(|at| at >= floor && at <= now && (now - at) as f64 <= max_age)
            };
            let proves = |r: &Value| {
                r.get("kind").and_then(Value::as_str) == Some("check")
                    && r.get("outcome").and_then(Value::as_str) == Some("satisfied")
                    && !matches!(r.get("source").and_then(Value::as_str), Some("agent_assessment" | "human_assessment"))
                    && predicate.is_some_and(|p| r.get("predicate") == Some(p))
                    && fresh(r)
            };
            let deterministic = records.iter().any(proves);
            let latest = records.iter().filter(|r| proves(r)).filter_map(|r| r.get("checked_at").and_then(Value::as_str).and_then(millis_from_iso)).max().unwrap_or(0);
            let contradictory = records.iter().any(|r| r.get("kind").and_then(Value::as_str) == Some("check") && r.get("outcome").and_then(Value::as_str) != Some("satisfied"))
                || (!historical
                    && checks.iter().any(|r| {
                        predicate.is_some_and(|p| r.get("predicate") == Some(p))
                            && r.get("outcome").and_then(Value::as_str) != Some("satisfied")
                            && r.get("checked_at").and_then(Value::as_str).and_then(millis_from_iso).is_some_and(|at| at >= latest)
                    }));
            let assessment = claim.get("assessment");
            let assessed_at = assessment.and_then(|a| a.get("assessed_at")).and_then(Value::as_str).and_then(millis_from_iso);
            let scope = policy.and_then(|p| p.get("scope")).and_then(Value::as_str).unwrap_or("task");
            let assessed = policy.and_then(|p| p.get("assessor")).is_some_and(|who| assessment.and_then(|a| a.get("assessor")) == Some(who))
                && assessment.and_then(|a| a.get("scope")).and_then(Value::as_str) == Some(scope)
                && assessed_at.is_some_and(|at| at >= floor && at <= now && (now - at) as f64 <= max_age)
                && assessment.and_then(|a| a.get("limitations")).is_some_and(Value::is_string)
                && assessment.and_then(|a| a.get("reason")).and_then(Value::as_str).is_some_and(|r| !r.trim().is_empty())
                && !records.is_empty();
            let proven = policy.is_some_and(|p| match p.get("kind").and_then(Value::as_str) {
                Some("assessment") => assessed,
                Some("both") => assessed && deterministic,
                _ => deterministic,
            });
            let claim_met = claim.get("claim").and_then(Value::as_str) == Some("met");
            if claim_met && proven && !contradictory && records.len() == claimed.len()
                && let Some(id) = criterion_id.and_then(Value::as_str)
            {
                met.push(id.to_string());
            }
        }
        let satisfied = required.iter().filter(|c| c.get("id").or_else(|| c.get("criterion_id")).and_then(Value::as_str).is_some_and(|id| met.iter().any(|m| m == id))).count();
        Ok((required.len(), satisfied))
    }

    /// The completion projection the console reads (`completionState`).
    pub(crate) fn completion_state(&self, task: &TaskRecord) -> Result<Value> {
        let claims: Vec<Value> = task.completion.as_ref().and_then(|c| c.get("criteria")).and_then(Value::as_array).cloned().unwrap_or_default();
        let (required, satisfied) = self.criteria_satisfied(task, &claims)?;
        let deliveries_required = task.deliveries.iter().filter(|d| d.get("required") != Some(&Value::Bool(false))).count();
        let verified: Vec<bool> = task.deliveries.iter().map(|d| self.storage.delivery_verified(&task.task_ref, d)).collect();
        let deliveries_ok = task.deliveries.iter().zip(&verified).all(|(d, v)| d.get("required") == Some(&Value::Bool(false)) || *v);
        let unresolved = self.unresolved_requests(&task.task_ref);
        let settled = !self.journal.get_control()?.unsettled && !self.storage.has_active_jobs(Some(&task.task_ref));
        let abandoned = unresolved
            .iter()
            .filter(|id| {
                self.journal.get_operation_by_request_id(id, Some(&task.task_ref)).ok().flatten().is_some_and(|op| {
                    self.journal.read_audit(&format!("operation:{}", op.operation_ref)).unwrap_or_default().last().and_then(|a| a.get("resolution")).and_then(Value::as_str) == Some("abandoned")
                })
            })
            .count();
        Ok(json!({
            "criteria": { "required": required, "satisfied": satisfied },
            "delivery": { "required": deliveries_required, "verified": verified.iter().filter(|v| **v).count() },
            "unresolved_request_refs": unresolved.iter().take(30).collect::<Vec<_>>(),
            "unresolved_request_count": unresolved.len(),
            "cleanup": if settled { "settled" } else { "unsettled" },
            "verified_complete": satisfied == required && deliveries_ok && unresolved.is_empty() && settled,
            "dependencies": { "blocking": unresolved.len() - abandoned, "abandoned": abandoned, "authorizes_replay": false },
        }))
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

/// `adminLogs` (`core.ts:2551-2572`): a journalctl argv, not executed.
fn admin_logs(unit: &str, lines: f64) -> Result<Value> {
    let (scope, systemd_unit) = match unit {
        "controller" => ("user", "agent-computer.service"),
        "output" => ("user", "ibara-output.service"),
        "sunshine" => ("user", "app-dev.lizardbyte.app.Sunshine.service"),
        "gateway" => ("system", "ibara-agent-sshd.service"),
        _ => return Err(invalid("Unknown log unit. Use controller, output, sunshine or gateway.")),
    };
    let bounded = if lines.is_finite() && lines.floor() >= 1.0 { (lines.floor() as i64).min(200) } else { 80 };
    let mut command = vec!["journalctl".to_string()];
    if scope == "user" {
        command.push("--user".into());
    }
    command.extend(["-u".into(), systemd_unit.into(), "-n".into(), bounded.to_string(), "--no-pager".into(), "--output=short-iso".into()]);
    Ok(json!({ "ok": true, "unit": unit, "systemd_unit": systemd_unit, "scope": scope, "lines": bounded, "command": command }))
}

fn read_key_values(path: &str) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().trim_end_matches(" kB").to_string()))
        })
        .collect()
}

pub(super) fn read_disk(target: &str) -> Value {
    let path = std::ffi::CString::new(target).unwrap_or_default();
    // SAFETY: statvfs writes into the zeroed struct we own; the path is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return json!({ "path": target, "total_bytes": null, "available_bytes": null });
    }
    let block = stat.f_frsize as u64;
    json!({
        "path": target,
        "total_bytes": block.saturating_mul(stat.f_blocks as u64),
        "available_bytes": block.saturating_mul(stat.f_bavail as u64),
    })
}
