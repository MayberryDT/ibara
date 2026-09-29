//! Journal record types. Field names and JSON shapes mirror the TypeScript
//! interfaces in `controller/src/journal.ts:21-116`.

use crate::error::IbaraError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `TaskRecord` (journal.ts:30). `state` is a free string: `finish(blocked)`
/// writes `blocked`, which is outside the declared `TaskState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_ref: String,
    pub principal: String,
    pub created_at: String,
    pub updated_at: String,
    pub state: String,
    pub goal: String,
    pub success_criteria: Vec<Value>,
    pub budgets: Value,
    pub client_flags: Value,
    pub authorization_ref: Option<String>,
    pub required_capabilities: Vec<Value>,
    pub control_started_ms: Option<i64>,
    pub last_charge_ms: Option<i64>,
    pub active_control_used_ms: i64,
    pub actions_used: i64,
    pub images_used: i64,
    pub last_checkpoint_ref: Option<String>,
    pub completion: Option<Value>,
    /// `'2.0'`, `'3.0'` or `'4'` (agent contract 4, written only by `ibarad`);
    /// anything else reads as `'2.0'` (journal.ts:979). Node reads `'4'` as `'2.0'`.
    pub contract_version: String,
    /// `'private'` or `'shared'`; anything else reads as `'private'` (journal.ts:980).
    pub visibility: String,
    pub owner_group: Option<String>,
    pub deliveries: Vec<Value>,
}

/// `LeaseRecord` (journal.ts:55). `state` ∈ `active | revoked | expired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub generation: String,
    pub task_ref: String,
    pub principal: String,
    pub connection_id: String,
    pub epoch: String,
    pub acquired_at: String,
    pub last_heartbeat_at: String,
    pub last_heartbeat_ms: i64,
    pub idle_expires_at_ms: i64,
    pub state: String,
    pub reason: Option<String>,
}

/// `ConnectionRecord` (journal.ts:69).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    pub connection_id: String,
    pub principal: String,
    pub last_heartbeat_ms: i64,
    pub disconnected_at_ms: Option<i64>,
    pub grace_expires_at_ms: Option<i64>,
}

/// Effect classes (docs/security-and-access.md, Approvals). Stored on each operation; rows written
/// by the TypeScript controller read as the column default, `change`.
pub const EFFECT_CLASSES: &[&str] = &["observe", "change", "send", "spend", "destructive", "access"];

/// `OperationRecord` (journal.ts:77) plus the additive `effect_class`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation_ref: String,
    pub request_id: String,
    pub principal: String,
    pub task_ref: Option<String>,
    pub tool: String,
    pub fingerprint: String,
    pub created_at: String,
    pub updated_at: String,
    pub receipt: Value,
    pub dispatched: bool,
    pub effect_class: String,
}

impl OperationRecord {
    /// The error a replay of this operation must return, if it is a begin
    /// that failed before creating a task (`task_ref` NULL and the receipt
    /// carries an error). The TypeScript replay instead looked up the
    /// placeholder task and answered `PERMISSION_DENIED`.
    pub fn failed_begin_error(&self) -> Option<IbaraError> {
        if self.tool != "computer_begin" || self.task_ref.is_some() {
            return None;
        }
        self.receipt.get("error").and_then(error_from_json)
    }
}

/// Rebuild an `IbaraError` from a stored public error object
/// (`{code, message, retry_safe, requires_reconciliation?, …details}`).
pub fn error_from_json(value: &Value) -> Option<IbaraError> {
    let obj = value.as_object()?;
    let code = obj.get("code")?.as_str()?;
    let code: &'static str = crate::error::CODES.iter().map(|(c, _)| *c).find(|c| *c == code).unwrap_or("INTERNAL_ERROR");
    let message = obj.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    let retry_safe = obj.get("retry_safe").and_then(Value::as_bool).unwrap_or(false);
    let mut error = IbaraError::new(code, message, retry_safe);
    let mut details = Map::new();
    for (k, v) in obj {
        if !matches!(k.as_str(), "code" | "message" | "retry_safe" | "next") {
            details.insert(k.clone(), v.clone());
        }
    }
    error.details = details;
    Some(error)
}

/// Who paused the computer (core schema 5). A person pauses explicitly or by
/// taking control; the system pauses at start, at shutdown and while it
/// repairs. Only a system pause ends by itself once the computer is healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseOrigin {
    Person,
    System,
}

impl PauseOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseOrigin::Person => "person",
            PauseOrigin::System => "system",
        }
    }

    pub fn parse(text: &str) -> Option<PauseOrigin> {
        match text {
            "person" => Some(PauseOrigin::Person),
            "system" => Some(PauseOrigin::System),
            _ => None,
        }
    }
}

/// `ControlState` (journal.ts:90), plus who paused it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlState {
    pub human_control: bool,
    pub paused: bool,
    pub unsettled: bool,
    pub settling_generation: Option<String>,
    pub epoch: String,
    pub session_hint: bool,
    /// Set while paused; `None` once resumed.
    pub pause_origin: Option<PauseOrigin>,
}

impl ControlState {
    /// Paused by ibara itself (starting, stopping, settling), not by a person.
    pub fn paused_by_system(&self) -> bool {
        (self.paused || self.human_control) && self.pause_origin == Some(PauseOrigin::System)
    }
}

/// `Partial<ControlState>` for `set_control`.
#[derive(Debug, Clone, Default)]
pub struct ControlPatch {
    pub human_control: Option<bool>,
    pub paused: Option<bool>,
    pub unsettled: Option<bool>,
    pub settling_generation: Option<Option<String>>,
    pub epoch: Option<String>,
    pub session_hint: Option<bool>,
    pub pause_origin: Option<Option<PauseOrigin>>,
}

/// `StoredRecord` (journal.ts:99): an evidence record found by ref.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRecord {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: String,
    pub task_ref: Option<String>,
    pub principal: Option<String>,
    pub epoch: Option<String>,
    pub expired: bool,
    pub record: Value,
}

/// `GrantRecord` (journal.ts:109). `role` is always `agent`; `state` ∈ `active | revoked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub group: String,
    pub principal: String,
    pub role: String,
    pub state: String,
    pub peer_key: String,
    pub updated_at: String,
}

/// One page of `pageById` (journal.ts:383).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total: i64,
}

/// The patch accepted by `update_operation` (journal.ts:551).
#[derive(Debug, Clone)]
pub struct OperationPatch {
    pub receipt: Option<Value>,
    pub dispatched: Option<bool>,
    /// `None` leaves `task_ref` unchanged; `Some(None)` clears it.
    pub task_ref: Option<Option<String>>,
    pub now_iso: String,
}

impl OperationPatch {
    pub fn at(now_iso: impl Into<String>) -> Self {
        OperationPatch { receipt: None, dispatched: None, task_ref: None, now_iso: now_iso.into() }
    }
}

/// `mayReadSharedTask` (`controller/src/agent-native.ts:195-198`): the owner, or
/// an active `agent` grant in the task's owner group when the task is shared.
/// `peer_key` is not checked, as in the TypeScript.
pub fn may_read_shared_task(task: &TaskRecord, principal: &str, grants: &[GrantRecord]) -> bool {
    if task.principal == principal {
        return true;
    }
    let Some(group) = task.owner_group.as_deref().filter(|g| !g.is_empty()) else {
        return false;
    };
    if task.visibility != "shared" {
        return false;
    }
    grants
        .iter()
        .any(|g| g.group == group && g.principal == principal && g.role == "agent" && g.state == "active")
}
