//! The response envelope and the tool result types.

use super::{Action, Destination, View};
use crate::error::IbaraError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Work continues; wait on the returned reference.
    Pending,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Pending => "pending",
            Status::Error => "error",
        }
    }
}

/// What every tool returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// One line of about 200 characters that orients the agent on its own.
    pub situation: String,
    pub status: Status,
    /// Meaningful events since this session's previous response.
    #[serde(default)]
    pub since: Vec<String>,
    #[serde(default)]
    pub result: Value,
    /// The public error object from `IbaraError::to_json`.
    #[serde(default)]
    pub error: Option<Value>,
}

impl Envelope {
    pub fn ok(situation: impl Into<String>, since: Vec<String>, result: impl Serialize) -> Self {
        Self::with_status(Status::Ok, situation, since, result)
    }
    pub fn pending(situation: impl Into<String>, since: Vec<String>, result: impl Serialize) -> Self {
        Self::with_status(Status::Pending, situation, since, result)
    }
    pub fn error(situation: impl Into<String>, since: Vec<String>, error: &IbaraError) -> Self {
        Envelope { situation: situation.into(), status: Status::Error, since, result: Value::Null, error: Some(error.to_json()) }
    }
    fn with_status(status: Status, situation: impl Into<String>, since: Vec<String>, result: impl Serialize) -> Self {
        let result = serde_json::to_value(result).unwrap_or(Value::Null);
        Envelope { situation: situation.into(), status, since, result, error: None }
    }
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

// ---- shared result pieces ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputerId {
    pub id: String,
    pub name: String,
}

/// Who the session is, e.g. agent `codex@vesper`, principal `vesper`. The
/// agent label is also the label on the agent's cursor. Cua picks the
/// cursor's color anew each time it starts, so ibara names none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct You {
    pub agent: String,
    pub principal: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckBasis {
    Automatic,
    YourAssessment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pending,
    Met,
    Unmet,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckStatus {
    pub id: String,
    pub basis: CheckBasis,
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A ranked thing the agent can do next, executable by `choice_id` alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub choice_id: String,
    pub label: String,
    pub action: Action,
    /// The parameter this choice needs, e.g. `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// One observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub frame_ref: String,
    pub revision: u64,
    pub captured_at: String,
    /// What the observation covered, e.g. `situation of 3 windows`.
    pub covered: String,
    pub cost_bytes: u64,
    /// Compact lines like `e12 button "Save" enabled · dialog "Save As"`.
    pub lines: Vec<String>,
    /// At most 20, ranked.
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_richer: Option<View>,
    /// Pass back as `cursor` for the next page of elements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ---- per-tool results ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeginResult {
    pub task_ref: String,
    pub computer: ComputerId,
    pub you: You,
    /// Absolute path of the task's workspace; relative paths in files,
    /// checks and expectations resolve here.
    pub workspace: String,
    pub checks: Vec<CheckStatus>,
    pub frame: Frame,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserveResult {
    pub frame: Frame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Done,
    /// The expectation was not seen within its deadline; not a failure.
    Unmet,
    /// ibara cannot tell whether the effect happened. Nothing is replayed.
    Unknown,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    pub index: usize,
    pub outcome: StepOutcome,
    /// What happened, in a few words.
    pub effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActResult {
    pub steps: Vec<StepResult>,
    /// Captured after the last expectation resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
    /// Set when a step is held for approval (status `pending`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
    /// With a held step: how to wait for the person's answer and then run it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Verified,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryStatus {
    pub host: String,
    pub path: String,
    pub state: DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeftOpen {
    pub surface: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Cleanup {
    pub closed: Vec<String>,
    pub left: Vec<LeftOpen>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishResult {
    pub checks: Vec<CheckStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryStatus>,
    pub cleanup: Cleanup,
    /// True only with no unknown outcomes, every required check met and every
    /// delivery verified.
    pub complete: bool,
    /// What ibara did with the call that the agent may not expect, such as
    /// ignoring an assessment of a check ibara evaluates itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactStatus {
    pub art_ref: String,
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivered_to: Vec<Destination>,
}

/// `computer_files` result, tagged by the op that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FilesResult {
    List {
        dir: String,
        entries: Vec<FileEntry>,
        #[serde(default)]
        truncated: bool,
    },
    Read {
        path: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base64: Option<String>,
        #[serde(default)]
        truncated: bool,
    },
    Write {
        path: String,
        bytes: u64,
        op_ref: String,
    },
    Publish {
        path: String,
        art_ref: String,
        sha256: String,
        size: u64,
        /// The step that wrote the file, when ibara recorded one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author_op: Option<String>,
    },
    Send {
        path: String,
        to: Destination,
        state: DeliveryState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<String>,
        /// How the bytes move and when the delivery counts: a send only
        /// publishes the file and records where it must go.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        next: String,
    },
    Status {
        artifacts: Vec<ArtifactStatus>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResult {
    pub met: bool,
    pub waited_ms: u64,
    /// The awaited thing's state, e.g. an op's `done` or an attention item's `answered`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputerSummary {
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// One line.
    pub capabilities: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefStatus {
    #[serde(rename = "ref")]
    pub reference: String,
    /// `computer`, `task`, `op`, `attention`, `artifact`, `frame`, …
    pub kind: String,
    /// Current state, or `ended` / `expired`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    pub next: Vec<String>,
}

/// `computer_status` result: the fleet, one reference, or a tool's help.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StatusResult {
    Fleet { computers: Vec<ComputerSummary> },
    Help { tool: String, help: String },
    Ref(RefStatus),
}
