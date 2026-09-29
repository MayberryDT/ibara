//! Input types for the eleven tools, and the action, expectation and check
//! vocabularies they share.

use super::{FieldError, Ref, Validate, at_most, from_value_at, tagged_union, take_field};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

// ---- shared pieces ----

// A host and path, used for delivery obligations and `send`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub host: String,
    pub path: String,
}

// What an action points at: an element of the latest frame, or a point.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Target {
    // An element id from the latest frame, like `e12`.
    Element(Ref),
    Point(Point),
}

impl JsonSchema for Target {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Target".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": ["string", "object"],
            "description": "Element e12 or picture pixel {x, y, frame?}."
        })
    }
}

// A point in the pixels of a picture (computer_observe view image or screen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: i32,
    pub y: i32,
    /// The frame that returned the picture; the task's latest picture if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Ref>,
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            v @ Value::String(_) => from_value_at("", v).map(Target::Element),
            v @ Value::Object(_) => from_value_at("", v).map(Target::Point),
            _ => Err(serde::de::Error::custom("expected an element id like \"e12\" or {x, y, frame?}")),
        }
    }
}

// ---- actions (computer_act) ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchAction {
    pub app: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SurfaceAction {
    pub surface: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetAction {
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeAction {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyAction {
    /// Like `ctrl+s` or `Return`.
    pub keys: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollAction {
    pub target: Target,
    #[serde(default)]
    pub dx: i32,
    #[serde(default)]
    pub dy: i32,
}

tagged_union! {
    /// An explicit desktop action.
    pub enum Action tag "kind" {
        "launch" => Launch(LaunchAction),
        "focus" => Focus(SurfaceAction),
        "click" => Click(TargetAction),
        "double_click" => DoubleClick(TargetAction),
        "right_click" => RightClick(TargetAction),
        "type" => Type(TypeAction),
        "key" => Key(KeyAction),
        "scroll" => Scroll(ScrollAction),
        /// Only surfaces this task opened.
        "close" => Close(SurfaceAction),
    }
}

// ---- expectations ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowExpect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    // Expect it to disappear instead.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DialogExpect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusExpect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextExpect {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementExpect {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UrlExpect {
    pub contains: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileExpect {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettledExpect {
    pub quiet_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

tagged_union! {
    /// What ibara waits to see after an action. `within_ms` overrides the
    /// deadline for the kind.
    pub enum Expectation tag "kind" {
        "window" => Window(WindowExpect),
        "dialog" => Dialog(DialogExpect),
        "focus" => Focus(FocusExpect),
        "text" => Text(TextExpect),
        "element" => Element(ElementExpect),
        "url" => Url(UrlExpect),
        "file" => File(FileExpect),
        "settled" => Settled(SettledExpect),
    }
}

impl Expectation {
    /// The agent's own deadline, if it set one.
    pub fn within_ms(&self) -> Option<u64> {
        match self {
            Expectation::Window(e) => e.within_ms,
            Expectation::Dialog(e) => e.within_ms,
            Expectation::Focus(e) => e.within_ms,
            Expectation::Text(e) => e.within_ms,
            Expectation::Element(e) => e.within_ms,
            Expectation::Url(e) => e.within_ms,
            Expectation::File(e) => e.within_ms,
            Expectation::Settled(e) => e.within_ms,
        }
    }
}

// ---- typed checks ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathCheck {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileContentCheck {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCheck {
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UrlCheck {
    pub contains: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementCheck {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextCheck {
    pub text: String,
}

tagged_union! {
    /// A check ibara evaluates itself; its basis is `automatic`.
    pub enum Check tag "kind" {
        "file_exists" => FileExists(PathCheck),
        "file_content" => FileContent(FileContentCheck),
        "artifact" => Artifact(ArtifactCheck),
        "url" => Url(UrlCheck),
        "element" => Element(ElementCheck),
        "text_present" => TextPresent(TextCheck),
        "delivered" => Delivered(Destination),
    }
}

impl Validate for Check {
    fn validate(&self) -> Result<(), FieldError> {
        if let Check::FileContent(c) = self
            && c.equals.is_some() == c.contains.is_some()
        {
            return Err(FieldError::new("equals", "give exactly one of equals or contains"));
        }
        Ok(())
    }
}

// ---- browser actions ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NavigateAction {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserTarget {
    /// A page element id like b3 from the latest page observation.
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserType {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserSelect {
    pub target: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScroll {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub dx: i32,
    #[serde(default)]
    pub dy: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitFor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

tagged_union! {
    /// An action in the signed-in browser. ibara clicks and types with real
    /// input; the extension only reads the page.
    pub enum BrowserAction tag "kind" {
        "navigate" => Navigate(NavigateAction),
        "click" => Click(BrowserTarget),
        "type" => Type(BrowserType),
        "select" => Select(BrowserSelect),
        "scroll" => Scroll(BrowserScroll),
        "key" => Key(KeyAction),
        "wait_for" => WaitFor(BrowserWaitFor),
    }
}

impl Validate for BrowserAction {
    fn validate(&self) -> Result<(), FieldError> {
        if let BrowserAction::WaitFor(w) = self
            && w.target.is_none()
            && w.text.is_none()
        {
            return Err(FieldError::new("target", "wait_for needs a target, a text, or both"));
        }
        Ok(())
    }
}

// ---- files ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListFiles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendFile {
    pub path: String,
    pub to: Destination,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoFields {}

tagged_union! {
    /// A file operation in the task's workspace or the home folder.
    pub enum FilesOp tag "op" {
        "list" => List(ListFiles),
        "read" => Read(ReadFile),
        "write" => Write(WriteFile),
        "publish" => Publish(PathCheck),
        "send" => Send(SendFile),
        "status" => Status(NoFields),
    }
}

// ---- the eleven tool inputs ----

/// `computer_status({ref?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusInput {
    /// Any reference, or `help:<tool>`; omit for the fleet.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Ref>,
}

// A success criterion stated at begin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub id: Ref,
    pub description: String,
    /// Without it the basis is your_assessment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
}

/// `computer_begin({computer, goal, checks?, deliver?, request_id})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BeginInput {
    /// Name or cmp_ id; optional with one computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<String>,
    pub goal: String,
    #[serde(default, deserialize_with = "at_most::<_, _, 20>", skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 20))]
    pub checks: Vec<CheckSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<Destination>,
    pub request_id: Ref,
}

impl Validate for BeginInput {
    fn validate(&self) -> Result<(), FieldError> {
        for (i, spec) in self.checks.iter().enumerate() {
            if self.checks[..i].iter().any(|c| c.id == spec.id) {
                return Err(FieldError::new(format!("checks[{i}].id"), format!("duplicate check id '{}'", spec.id)));
            }
            if let Some(check) = &spec.check {
                check.validate().map_err(|e| FieldError::new(format!("checks[{i}].check.{}", e.path), e.message))?;
            }
        }
        Ok(())
    }
}

// How much to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Situation,
    Elements,
    Image,
    Screen,
}

impl View {
    pub fn as_str(self) -> &'static str {
        match self {
            View::Situation => "situation",
            View::Elements => "elements",
            View::Image => "image",
            View::Screen => "screen",
        }
    }
}

/// `computer_observe({task_ref, view?, query?, surface?, limit?, cursor?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveInput {
    pub task_ref: Ref,
    #[serde(default)]
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Validate for ObserveInput {
    fn validate(&self) -> Result<(), FieldError> {
        match self.limit {
            Some(n) if !(1..=100).contains(&n) => Err(FieldError::new("limit", format!("must be 1-100, got {n}"))),
            _ => Ok(()),
        }
    }
}

// One step of an act: a choice or an action, and what to expect after it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Step {
    // As ActInput::choice (described once in the schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<Ref>,
    // As ActInput::text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectation>,
    // A stricter effect class than the action implies (e.g. send).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectClass>,
}

impl Step {
    fn check(&self, at: &str) -> Result<(), FieldError> {
        let path = |field: &str| if at.is_empty() { field.to_string() } else { format!("{at}.{field}") };
        if self.choice.is_some() && self.action.is_some() {
            return Err(FieldError::new(path("action"), "give a choice or an action, not both"));
        }
        if self.text.is_some() && self.choice.is_none() {
            return Err(FieldError::new(path("text"), "text is a parameter of a choice; for an action use kind \"type\""));
        }
        if self.choice.is_none() && self.action.is_none() && self.expect.is_none() {
            let place = if at.is_empty() { "action" } else { at };
            return Err(FieldError::new(place, "give a choice, an action or an expect"));
        }
        Ok(())
    }
}

/// `computer_act({task_ref, request_id, choice?, action?, expect?, steps?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActInput {
    pub task_ref: Ref,
    pub request_id: Ref,
    /// Choice id from the latest frame, e.g. c3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<Ref>,
    /// Text for a choice that takes text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectation>,
    // Declare send, spend or destructive when the step has that effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectClass>,
    /// Instead of one step; stops at the first unmet expect.
    #[serde(default, deserialize_with = "at_most::<_, _, 8>", skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    pub steps: Vec<Step>,
}

impl ActInput {
    /// The steps to run, whether given as one step or as `steps`.
    pub fn into_steps(self) -> Vec<Step> {
        if self.steps.is_empty() {
            vec![Step { choice: self.choice, text: self.text, action: self.action, expect: self.expect, effect: self.effect }]
        } else {
            self.steps
        }
    }
}

impl Validate for ActInput {
    fn validate(&self) -> Result<(), FieldError> {
        if self.steps.is_empty() {
            let single = Step {
                choice: self.choice.clone(),
                text: self.text.clone(),
                action: self.action.clone(),
                expect: self.expect.clone(),
                effect: self.effect,
            };
            return single.check("");
        }
        for (field, given) in [
            ("choice", self.choice.is_some()),
            ("text", self.text.is_some()),
            ("action", self.action.is_some()),
            ("expect", self.expect.is_some()),
            ("effect", self.effect.is_some()),
        ] {
            if given {
                return Err(FieldError::new(field, "give steps or a single step, not both; move it into steps"));
            }
        }
        for (i, step) in self.steps.iter().enumerate() {
            step.check(&format!("steps[{i}]"))?;
        }
        Ok(())
    }
}

/// `browser_act({task_ref, request_id, action, expect?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserActInput {
    pub task_ref: Ref,
    pub request_id: Ref,
    pub action: BrowserAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectation>,
    // Declare send, spend or destructive when the action has that effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectClass>,
}

impl Validate for BrowserActInput {
    fn validate(&self) -> Result<(), FieldError> {
        self.action.validate().map_err(|e| FieldError::new(format!("action.{}", e.path), e.message))
    }
}

// Effect class of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    #[default]
    Change,
    Send,
    Spend,
    Destructive,
}

/// `computer_exec({task_ref, request_id, command[], cwd?, timeout_ms?, background?, effect?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecInput {
    pub task_ref: Ref,
    pub request_id: Ref,
    /// Program and arguments; no shell.
    #[serde(deserialize_with = "at_most::<_, _, 256>")]
    #[schemars(length(min = 1, max = 256))]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
    #[serde(default)]
    pub effect: EffectClass,
}

impl Validate for ExecInput {
    fn validate(&self) -> Result<(), FieldError> {
        match self.command.first() {
            Some(program) if !program.is_empty() => Ok(()),
            _ => Err(FieldError::new("command", "name a program as the first item")),
        }
    }
}

// What `computer_wait` waits for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    // An `op_` reference.
    Op(Ref),
    // An `att_` reference.
    Attention(Ref),
    Expect(Expectation),
}

/// One object with three optional properties, of which exactly one is given:
/// flatter than `oneOf`, which some harnesses drop.
impl JsonSchema for WaitFor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WaitFor".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "Exactly one of op, attention or expect.",
            "properties": {
                "op": g.subschema_for::<Ref>(),
                "attention": g.subschema_for::<Ref>(),
                "expect": g.subschema_for::<Expectation>(),
            },
            "additionalProperties": false
        })
    }
}

/// `computer_wait({task_ref, for, deadline_ms})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {
    pub task_ref: Ref,
    #[serde(rename = "for")]
    pub wait_for: WaitFor,
    #[schemars(range(max = 600_000))]
    pub deadline_ms: u64,
}

impl Validate for WaitInput {
    fn validate(&self) -> Result<(), FieldError> {
        if self.deadline_ms > 600_000 {
            return Err(FieldError::new("deadline_ms", "at most 600000 (10 minutes)"));
        }
        Ok(())
    }
}

/// `computer_files({task_ref, request_id, op, …})`
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilesInput {
    pub task_ref: Ref,
    pub request_id: Ref,
    #[serde(flatten)]
    pub op: FilesOp,
}

impl<'de> Deserialize<'de> for FilesInput {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map = Map::<String, Value>::deserialize(d)?;
        let task_ref = take_field(&mut map, "task_ref")?;
        let request_id = take_field(&mut map, "request_id")?;
        let op = from_value_at("", Value::Object(map))?;
        Ok(FilesInput { task_ref, request_id, op })
    }
}

impl JsonSchema for FilesInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FilesInput".into()
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = FilesOp::json_schema(g);
        let obj = schema.ensure_object();
        if let Some(Value::Object(props)) = obj.get_mut("properties") {
            let r = serde_json::to_value(g.subschema_for::<Ref>()).unwrap_or(Value::Null);
            props.insert("task_ref".into(), r.clone());
            props.insert("request_id".into(), r);
        }
        if let Some(Value::Array(required)) = obj.get_mut("required") {
            required.push("task_ref".into());
            required.push("request_id".into());
        }
        schema
    }
}

impl Validate for FilesInput {
    fn validate(&self) -> Result<(), FieldError> {
        if let FilesOp::Write(w) = &self.op
            && w.text.is_some() == w.base64.is_some()
        {
            return Err(FieldError::new("text", "give exactly one of text or base64"));
        }
        Ok(())
    }
}

// A question for a person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub question: String,
    #[serde(default, deserialize_with = "at_most::<_, _, 10>", skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10))]
    pub options: Vec<String>,
}

/// `computer_checkpoint({task_ref, note?, ask?, stop_asking?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointInput {
    pub task_ref: Ref,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<Ask>,
    // Ask the person, once, to let this agent send, spend and delete here without approval.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stop_asking: bool,
}

impl Validate for CheckpointInput {
    fn validate(&self) -> Result<(), FieldError> {
        if self.note.is_none() && self.ask.is_none() && !self.stop_asking {
            return Err(FieldError::new("note", "give a note, an ask, stop_asking, or more than one"));
        }
        Ok(())
    }
}

// How the agent ends a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    Partial,
    Cancelled,
    Blocked,
}

// The agent's attributed judgement of a `your_assessment` check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub check: Ref,
    pub met: bool,
    pub reason: String,
}

/// `computer_finish({task_ref, request_id, outcome, summary, assessments?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinishInput {
    pub task_ref: Ref,
    pub request_id: Ref,
    pub outcome: Outcome,
    pub summary: String,
    #[serde(default, deserialize_with = "at_most::<_, _, 20>", skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 20))]
    pub assessments: Vec<Assessment>,
}

impl Validate for FinishInput {
    fn validate(&self) -> Result<(), FieldError> {
        for (i, a) in self.assessments.iter().enumerate() {
            if self.assessments[..i].iter().any(|b| b.check == a.check) {
                return Err(FieldError::new(format!("assessments[{i}].check"), format!("check '{}' assessed twice", a.check)));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProceduresOp {
    Search,
    Read,
}

/// `computer_procedures({op, query?, ref?})`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProceduresInput {
    pub op: ProceduresOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Ref>,
}

impl Validate for ProceduresInput {
    fn validate(&self) -> Result<(), FieldError> {
        match self.op {
            ProceduresOp::Read if self.reference.is_none() => Err(FieldError::new("ref", "read needs the ref of a procedure or note")),
            _ => Ok(()),
        }
    }
}

impl Validate for StatusInput {}

fn is_false(b: &bool) -> bool {
    !*b
}
