//! The tool list and `help:<tool>`, generated from the input types.

use super::tools::*;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

/// One MCP tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    /// Changes nothing anywhere, so a harness may run it without asking.
    pub read_only: bool,
}

/// The tools that only read: MCP's `readOnlyHint` lets harnesses such as
/// Codex and Claude Code allow them without a prompt for each call.
const READ_ONLY: &[&str] = &["computer_status", "computer_observe", "computer_wait", "computer_procedures"];

impl ToolDef {
    /// The MCP `tools/list` entry.
    pub fn to_mcp(&self) -> Value {
        let mut tool = json!({ "name": self.name, "description": self.description, "inputSchema": self.input_schema });
        if self.read_only {
            tool["annotations"] = json!({ "readOnlyHint": true });
        }
        tool
    }
}

struct Tool {
    name: &'static str,
    summary: &'static str,
    long: &'static str,
    example: &'static str,
    schema: fn(&mut SchemaGenerator) -> Schema,
}

fn schema_of<T: JsonSchema>(g: &mut SchemaGenerator) -> Schema {
    g.root_schema_for::<T>()
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "computer_status",
        summary: "The fleet (no ref), a ref's state and next moves, or ref \"help:<tool>\" for a tool's full description. Read-only; no screen capture.",
        long: "With no ref: every computer you may use, with id, state, user, holder and a capability line. With a ref (cmp_, task_, op_, att_, art_, frame_, checkpoint_): its current state, parent, children and valid next moves; old or unknown refs answer ended or expired with next moves. With ref \"help:<tool>\": this text for that tool.",
        example: r#"{"ref": "task_3f2a"}"#,
        schema: schema_of::<StatusInput>,
    },
    Tool {
        name: "computer_begin",
        summary: "Start a task on one computer: takes control, states how each check is proven, returns the first frame. Replaying request_id returns the original result.",
        long: "computer is a name or cmp_ id, resolved once and echoed as an id; omit it when only one computer is reachable. checks are {id, description, check?}; a typed check (kind file_exists, file_content, artifact, url, element, text_present, delivered) is evaluated automatically, otherwise you assess it at finish. A typed check that can never pass is refused here. deliver names a {host, path} the result must reach. The result gives task_ref, who you are, each check's basis and the first frame. Every later call carries task_ref.",
        example: r#"{"computer": "tulip1", "goal": "Save a note as dogfood.txt", "checks": [{"id": "saved", "description": "file exists", "check": {"kind": "file_exists", "path": "~/dogfood.txt"}}], "request_id": "req-1"}"#,
        schema: schema_of::<BeginInput>,
    },
    Tool {
        name: "computer_observe",
        summary: "See more than the last frame: ranked elements, a paged query, an image crop of one surface, or the full screen. Read-only.",
        long: "view situation (default): windows, focus, the dialog and up to 20 ranked choices as compact lines. view elements: a query, subtree or surface, paged with cursor and limit. view image: a WebP or JPEG crop of the named or focused surface. view screen: the whole screen. The frame reports what it covered, what it cost and next_richer, the next richer view.",
        example: r#"{"task_ref": "task_3f2a", "view": "elements", "query": "Save", "limit": 20}"#,
        schema: schema_of::<ObserveInput>,
    },
    Tool {
        name: "computer_act",
        summary: "Do 1 to 8 steps, each a choice from the latest frame or an action, with optional expect and effect. Real input; stops at the first unmet expect; returns the new frame. Pending: a person must approve; computer_wait its att_, then resend the same request_id.",
        long: "A step is {choice (+ text) | action, expect?, effect?}; an expect-only step just waits. Actions (kind): launch app (editor, terminal, browser or files, the file manager); focus surface; click, double_click, right_click target; type text; key keys; scroll target dx dy; close surface (only surfaces this task opened). target is an element id like e12, or a point {x, y, frame?} in the pixels of a picture from computer_observe view image or screen: frame names the frame that returned the picture, else the task's latest picture is used. A point is refused, with nothing sent, when the task has no picture yet or the pictured window moved or changed size since. Expectations (kind): window, dialog, focus, text, element, url, file, settled; each takes within_ms. effect declares a stricter class than the action implies, such as send for submitting an order; send, spend and destructive may be held for approval. Each step reports done, unmet (not seen by the deadline, not a failure), unknown (never replayed) or not_run. A step that needs approval returns status pending with an att_ ref and next, and does not run; wait for the att_ with computer_wait, then send the same request (same request_id) again, which runs it once if approved. The same step under a new request_id is refused while its approval is open, and asked again once it was answered, whatever effect it declares. ibara itself counts a press that submits a page's form as send.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-2", "steps": [{"action": {"kind": "key", "keys": "ctrl+s"}, "expect": {"kind": "dialog", "title": "Save As"}}, {"action": {"kind": "type", "text": "dogfood.txt"}}, {"action": {"kind": "key", "keys": "Return"}, "expect": {"kind": "dialog", "title": "Save As", "gone": true}}]}"#,
        schema: schema_of::<ActInput>,
    },
    Tool {
        name: "browser_act",
        summary: "Act in the signed-in browser's focused tab with real input and effects in the user's session. Pending: as computer_act.",
        long: "Observe surface \"tab\" view \"elements\" first for page element ids like b3. action.kind: navigate url (http or https); click target; type text (target? — with a target its text is replaced, without one text goes to the focus; text beyond ASCII needs a target); select target value (an option's value or label); scroll dx dy (target? — a target is scrolled into view); key keys; wait_for target and/or text. The browser window must be focused. expect, effect and a pending reply work as in computer_act.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-3", "action": {"kind": "navigate", "url": "https://example.com"}, "expect": {"kind": "url", "contains": "example.com"}}"#,
        schema: schema_of::<BrowserActInput>,
    },
    Tool {
        name: "computer_exec",
        summary: "Run a bounded command (no shell) in the task's workspace or a folder you name. Declare effect send, spend or destructive if any. Pending: as computer_act.",
        long: "command is the program and its arguments. cwd is relative to the workspace, or absolute inside it or the home folder (~/ works too); ibara's own folders are refused. timeout_ms bounds the run; background returns an op_ ref at once to wait on. The effect class is change unless declared otherwise. A command that needs approval returns status pending with an att_ ref and next, and does not run; wait for the att_ with computer_wait, then send the same request (same request_id) again, which runs it once if approved.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-4", "command": ["ls", "-la"], "timeout_ms": 5000}"#,
        schema: schema_of::<ExecInput>,
    },
    Tool {
        name: "computer_wait",
        summary: "Wait for an op, an attention answer or an expectation until deadline_ms; returns status pending at the deadline. Never repeats an effect.",
        long: "for is exactly one of {op: \"op_…\"}, {attention: \"att_…\"} or {expect: <expectation as in computer_act>}. Use it after a pending result, a background exec or a checkpoint ask.",
        example: r#"{"task_ref": "task_3f2a", "for": {"attention": "att_91c0"}, "deadline_ms": 60000}"#,
        schema: schema_of::<WaitInput>,
    },
    Tool {
        name: "computer_files",
        summary: "List, read, write, publish or send files in the workspace or home folder, or show their status. Sends and overwrites may be held for approval.",
        long: "Paths are relative to the workspace, or absolute inside it or the home folder (~/ works too); ibara's own folders are refused. op list (dir?); read path (max_bytes?); write path with text or base64; publish path (links the step that wrote it when ibara recorded one); send path to {host, path}; status. Published files get an art_ ref. send goes only to the computer you work from (host: its name or cmp_/computer_ id; anything else is refused before anyone is asked) and an absolute path there. A send moves no bytes: it publishes the file and records where it must go; your collector then fetches it there with `ibara client --computer <this computer's cmp_ id> fetch <art_ ref> <path>` (the send's next says exactly), or a person saves it from the ibara console, and only then is the delivery verified. A send or overwrite that needs approval returns status pending with an att_ ref and next, and does not run; wait for the att_ with computer_wait, then send the same request (same request_id) again, which runs it once if approved.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-5", "op": "publish", "path": "dogfood.txt"}"#,
        schema: schema_of::<FilesInput>,
    },
    Tool {
        name: "computer_checkpoint",
        summary: "Save a continuation note, or ask a person a question you cannot decide. stop_asking asks your person once to let you send, spend and delete here unapproved.",
        long: "note is stored as your continuation note for the task. ask {question, options?} raises an attention item; the answer appears in the next situation and since, and computer_wait({for: {attention}}) waits for it. stop_asking true is for when your person tells you that you need not ask them before you send, spend or delete: you cannot change your own access, so ibara shows them one request for this computer (Allow or Not Now) and changes nothing until they allow it. The reply's stop_asking holds its att_ (state waiting_for_person), or state nothing_to_ask when those steps already run without asking here. Carry on meanwhile; ask on each computer they meant.",
        example: r#"{"task_ref": "task_3f2a", "ask": {"question": "Overwrite the existing report?", "options": ["yes", "no"]}}"#,
        schema: schema_of::<CheckpointInput>,
    },
    Tool {
        name: "computer_finish",
        summary: "End the task: evaluates checks, records your assessments, closes what the task opened and still owns, releases control. complete only if all is verified.",
        long: "outcome is complete, partial, cancelled or blocked. assessments [{check, met, reason}] cover checks whose basis is your_assessment and are attributed to you; an assessment of an automatic check counts only when ibara could not read it (no accessibility tree, page reader not answering), else it is ignored; notes says which. Windows or tabs a person touched, or with unsaved changes, are left open and reported. After your control ended (a person took the computer, it expired), finish still records the outcome and summary without taking control back; windows then stay open while someone else has the computer.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-6", "outcome": "complete", "summary": "Saved dogfood.txt", "assessments": [{"check": "looks_right", "met": true, "reason": "text matches"}]}"#,
        schema: schema_of::<FinishInput>,
    },
    Tool {
        name: "computer_procedures",
        summary: "Search or read approved procedures and app notes. Read-only.",
        long: "op search with a query returns matching procedures and app notes; op read with a ref returns one in full. Until approved procedures exist, only app notes are returned.",
        example: r#"{"op": "search", "query": "save as dialog"}"#,
        schema: schema_of::<ProceduresInput>,
    },
];

/// The eleven tools with compact input schemas.
pub fn tool_definitions() -> Vec<ToolDef> {
    let mut g = SchemaSettings::draft2020_12()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let mut seen = Vec::new();
    TOOLS
        .iter()
        .map(|t| {
            let mut schema = (t.schema)(&mut g).to_value();
            compact(&mut schema);
            if let Value::Object(root) = &mut schema {
                // The tool description replaces the type's doc, but a union's
                // field listing (computer_files) stays.
                if root.get("description").and_then(Value::as_str).is_some_and(|d| !d.starts_with("By ")) {
                    root.shift_remove("description");
                }
                // Unknown fields are refused everywhere by the parser; saying so
                // once at the root keeps the list small.
                root.insert("additionalProperties".into(), false.into());
                for (name, prop) in root.get_mut("properties").and_then(Value::as_object_mut).into_iter().flatten() {
                    refer_repeats(t.name, name, prop, &mut seen);
                }
            }
            ToolDef { name: t.name, description: t.summary, input_schema: schema, read_only: READ_ONLY.contains(&t.name) }
        })
        .collect()
}

/// The long description and an example for one tool (the `help:<tool>` ref).
pub fn help(tool: &str) -> Option<String> {
    let t = TOOLS.iter().find(|t| t.name == tool)?;
    Some(format!("{}\n\n{}\n\nExample: {}({})", t.summary, t.long, t.name, t.example))
}

/// Shortest subschema worth replacing by a pointer to its first occurrence.
const REPEAT_MIN: usize = 120;

/// A large subschema already seen: its value, tool and path.
type Seen = Vec<(Value, &'static str, String)>;

/// Replace a large subschema that already appeared earlier in the list (e.g.
/// `steps[].action` after `action`, or `expect` after computer_act's) with a
/// short pointer to it. Plain text rather than `$ref`, which some harnesses do
/// not resolve; the parser still checks the full shape.
fn refer_repeats(tool: &'static str, path: &str, v: &mut Value, seen: &mut Seen) {
    if !v.is_object() {
        return;
    }
    if v.to_string().len() >= REPEAT_MIN {
        if let Some((_, first_tool, first)) = seen.iter().find(|(s, _, _)| s == v) {
            let kind = v.get("type").cloned().unwrap_or(Value::from("object"));
            let place = if *first_tool == tool { first.clone() } else { format!("{first} in {first_tool}") };
            *v = json!({ "type": kind, "description": format!("Same shape as {place}.") });
            return;
        }
        seen.push((v.clone(), tool, path.to_string()));
    }
    let Value::Object(map) = v else { return };
    if let Some(Value::Object(props)) = map.get_mut("properties") {
        for (name, prop) in props.iter_mut() {
            refer_repeats(tool, &format!("{path}.{name}"), prop, seen);
        }
    }
    if let Some(items) = map.get_mut("items") {
        refer_repeats(tool, &format!("{path}[]"), items, seen);
    }
}

/// Remove what an agent does not need: titles, formats, defaults, the null
/// alternative of optional fields, zero minimums, `type` beside a string
/// `enum`, `$defs` left empty, and
/// `additionalProperties: false` (restored at the root by the caller).
fn compact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for key in ["$schema", "title", "format", "default", "examples"] {
                map.shift_remove(key);
            }
            if map.get("additionalProperties") == Some(&Value::Bool(false)) {
                map.shift_remove("additionalProperties");
            }
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    map.insert("type".into(), only);
                }
            }
            if map.get("minimum") == Some(&json!(0)) {
                map.shift_remove("minimum");
            }
            // A list of string values already says the type.
            if map.get("type") == Some(&json!("string"))
                && map.get("enum").and_then(Value::as_array).is_some_and(|e| e.iter().all(Value::is_string))
            {
                map.shift_remove("type");
            }
            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(alts)) = map.get_mut(key) {
                    alts.retain(|a| a.get("type") != Some(&json!("null")));
                    if alts.len() == 1 {
                        let Value::Object(only) = alts.remove(0) else { continue };
                        map.shift_remove(key);
                        for (k, val) in only {
                            map.entry(k).or_insert(val);
                        }
                    }
                }
            }
            if matches!(map.get("$defs"), Some(Value::Object(d)) if d.is_empty()) {
                map.shift_remove("$defs");
            }
            for (key, val) in map.iter_mut() {
                match (key.as_str(), val) {
                    // Maps of named subschemas: the names are field names, not keywords.
                    ("properties" | "$defs", Value::Object(named)) => named.values_mut().for_each(compact),
                    (_, val) => compact(val),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(compact),
        _ => {}
    }
}

/// One object schema for a union tagged by `tag`: the tag is an `enum`, the
/// properties are the union of every variant's, and the description lists
/// each kind's own fields (`*` marks required ones). Far smaller than `oneOf`
/// and accepted by every harness.
pub(crate) fn flat_union(tag: &str, variants: &[(&str, Schema)]) -> Schema {
    let kinds: Vec<&str> = variants.iter().map(|(k, _)| *k).collect();
    let fields: Vec<(Map<String, Value>, Vec<String>)> = variants
        .iter()
        .map(|(_, s)| {
            let props = s.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            let required = s
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            (props, required)
        })
        .collect();
    let in_all = |name: &str| fields.iter().all(|(p, _)| p.contains_key(name));

    let mut properties = Map::new();
    properties.insert(tag.into(), json!({ "enum": kinds }));
    let mut required = vec![Value::from(tag)];
    let mut parts = Vec::with_capacity(variants.len());
    for ((kind, _), (props, req)) in variants.iter().zip(&fields) {
        let own: Vec<String> = props
            .keys()
            .filter(|name| !in_all(name))
            .map(|name| if req.contains(name) { format!("{name}*") } else { name.clone() })
            .collect();
        parts.push(if own.is_empty() { kind.to_string() } else { format!("{kind}({})", own.join(" ")) });
        for (name, schema) in props {
            properties.entry(name.clone()).or_insert_with(|| schema.clone());
        }
    }
    if let Some((first, _)) = fields.first() {
        for name in first.keys() {
            if in_all(name) && fields.iter().all(|(_, r)| r.contains(name)) {
                required.push(name.as_str().into());
            }
        }
    }
    let description = format!("By {tag}, * required: {}", parts.join(" "));
    let mut schema = Map::new();
    schema.insert("type".into(), "object".into());
    schema.insert("description".into(), description.into());
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("required".into(), Value::Array(required));
    Schema::from(schema)
}
