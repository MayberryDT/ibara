//! The tool list and `help:<tool>`, generated from the input types.

use super::tools::*;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Value, json};

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
        summary: "Read fleet or ref state, job output and next moves. ref help:<tool> gives full help. No screen capture.",
        long: "With no ref: every computer you may use, with id, state, user, holder and a capability line. With a ref (cmp_, task_, op_, att_, art_, frame_, checkpoint_): its current state, parent, children and valid next moves; old or unknown refs answer ended or expired with next moves. With ref \"help:<tool>\": this text for that tool.",
        example: r#"{"ref": "task_3f2a"}"#,
        schema: schema_of::<StatusInput>,
    },
    Tool {
        name: "computer_begin",
        summary: "Start on any free computer; pin only when required. Returns task, workspace and frame. Independent tasks may use different machines.",
        long: "Omit computer for ordinary work: the fleet chooses a ready computer and tries another after a definite acquisition refusal. A provided name or cmp_ id pins the task; use it for a person-named machine, required local state or a fixed benchmark cell. Each task stays bound to the returned computer id. Begin independent tasks with distinct request_ids to use multiple machines; a busy one does not block the fleet. An uncertain acquisition never moves to a second machine; retry its request_id to reconcile. checks are {id, description, check?}; a typed check (kind file_exists, file_content, artifact, url, element, text_present, delivered) is evaluated automatically, otherwise you assess it at finish. A typed check that can never pass is refused here. deliver names a {host, path} the result must reach. logins declares likely sites (at most 20) without asking anyone or copying cookies. Its states describe availability, never authentication. Visit the site, use its existing browser session and verify the expected account before requesting assistance. The result gives task_ref, who you are, each check's basis and the first frame. Every later call carries task_ref.",
        example: r#"{"computer": "tulip1", "goal": "Save a note as dogfood.txt", "checks": [{"id": "saved", "description": "file exists", "check": {"kind": "file_exists", "path": "~/dogfood.txt"}}], "request_id": "req-1"}"#,
        schema: schema_of::<BeginInput>,
    },
    Tool {
        name: "computer_observe",
        summary: "Read focused browser/native controls, a paged query, a surface image or full screen. Pass back next_cursor with the same query/surface for more.",
        long: "view situation (default): windows, focus, the dialog and up to 20 ranked choices as compact lines. view elements: a query, subtree or surface, paged with cursor and limit. view image: a WebP or JPEG crop of the named or focused surface. view screen: the whole screen. The frame reports what it covered, what it cost and next_richer, the next richer view.",
        example: r#"{"task_ref": "task_3f2a", "view": "elements", "query": "Save", "limit": 20}"#,
        schema: schema_of::<ObserveInput>,
    },
    Tool {
        name: "computer_act",
        summary: "Send 1–8 real-input steps. Stops on unmet expect; returns frame. Pending approval: wait on att_, then resend the same request_id.",
        long: "A step is {choice (+ text) | action, expect?, effect?}; an expect-only step just waits. Actions (kind): launch app (editor, terminal, browser or files, the file manager); focus surface; click, double_click, right_click target; type text; key keys; scroll target dx dy; close surface (personal desktop: only this task's windows; disposable desktop: any current window). target is an element id like e12, or a point {x, y, frame?} in the pixels of a picture from computer_observe view image or screen: frame names the frame that returned the picture, else the task's latest picture is used. A point is refused, with nothing sent, when the task has no picture yet or the pictured window moved or changed size since. Expectations (kind): window, dialog, focus, text, element, url, file, settled; each takes within_ms. effect declares a stricter class than the action implies, such as send for submitting an order; send, spend and destructive may be held for approval. Each step reports done, unmet (not seen by the deadline, not a failure), unknown (never replayed) or not_run. A step that needs approval returns status pending with an att_ ref and next, and does not run; wait for the att_ with computer_wait, then send the same request (same request_id) again, which runs it once if approved. The same step under a new request_id is refused while its approval is open, and asked again once it was answered, whatever effect it declares. ibara itself counts a press that submits a page's form as send.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-2", "steps": [{"action": {"kind": "key", "keys": "ctrl+s"}, "expect": {"kind": "dialog", "title": "Save As"}}, {"action": {"kind": "type", "text": "dogfood.txt"}}, {"action": {"kind": "key", "keys": "Return"}, "expect": {"kind": "dialog", "title": "Save As", "gone": true}}]}"#,
        schema: schema_of::<ActInput>,
    },
    Tool {
        name: "browser_act",
        summary: "Act in the signed-in browser's focused tab with real input and effects in the user's session. Pending: as computer_act.",
        long: "Observe surface \"tab\" view \"elements\" first for page element ids like b3. action.kind: navigate url (http or https); click target; type text (target? — with a target its text is replaced, without one text goes to the focus; text beyond ASCII needs a target); select target value (an option's value or label); scroll dx dy (target? — a target is scrolled into view); key keys; wait_for target and/or text; sign_in (sites?) requests assistance only at a visible sign-in form blocking this task. It does not enter credentials or guarantee login. First inspect the site's existing session and verify the expected account. Sites must match the focused sign-in page or its redirect source. Eligible approved session sharing may copy cookies and reload; Google/Gmail require an independent session. The result gives sign_in {sites [{site, state}], page left_sign_in, still_sign_in or unknown}. Even left_sign_in is not proof of the expected account; verify the page. Never ask your person for a password. A pending sign_in names a reason (waiting_for_person, waiting_for_browser, waiting_for_sharing_computer, signed_out_there, sharing_off) and next; declined, denied and site_rejected are final for the task. The browser window must be focused. expect, effect and a pending reply work as in computer_act.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-3", "action": {"kind": "navigate", "url": "https://example.com"}, "expect": {"kind": "url", "contains": "example.com"}}"#,
        schema: schema_of::<BrowserActInput>,
    },
    Tool {
        name: "computer_exec",
        summary: "Run managed command argv in the task workspace (cwd optional). Jobs retain ownership through disconnect. Optional timeout_ms; background returns immediately. Declare risky effect; pending as computer_act.",
        long: "command is program and arguments without shell interpretation. cwd defaults to the task workspace; absolute home paths are shared, not isolated. No implicit command deadline; timeout_ms sets one, subject to explicit host policy. The job receipt reports its effective limit. Registered running or unconfirmed jobs retain ownership across connection loss. Reconnect as the same agent with task_ref; inspect the op_ before retrying, using its original request_id. Detached external services and administrative SSH are not supervised. Explicit finish/cancel, takeover and access revocation stop owned jobs. background returns at once; computer_status of op_ reads output even after control ended. Effect is change unless declared stricter. Pending att_: wait, then resend the same request_id after approval.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-4", "command": ["ls", "-la"], "timeout_ms": 5000}"#,
        schema: schema_of::<ExecInput>,
    },
    Tool {
        name: "computer_wait",
        summary: "Wait on op, attention or expectation. Deadline returns pending without cancelling work or repeating effects.",
        long: "for is exactly one of {op: \"op_…\"}, {attention: \"att_…\"} or {expect: <expectation as in computer_act>}. Use it after a pending result, a background exec or a checkpoint ask.",
        example: r#"{"task_ref": "task_3f2a", "for": {"attention": "att_91c0"}, "deadline_ms": 60000}"#,
        schema: schema_of::<WaitInput>,
    },
    Tool {
        name: "computer_files",
        summary: "List/read/write/publish/send task or home files. Sends and overwrites may require approval.",
        long: "Paths are relative to the workspace, or absolute inside it or the home folder (~/ works too); ibara's own folders are refused. op list (dir?); read path (max_bytes?); write path with text or base64; publish path (links the step that wrote it when ibara recorded one); send path to {host, path}; status. Published files get an art_ ref. send goes only to the computer you work from (host: its name or cmp_/computer_ id; anything else is refused before anyone is asked) and an absolute path there. A send moves no bytes: it publishes the file and records where it must go; your collector then fetches it there with `ibara client --computer <this computer's cmp_ id> fetch <art_ ref> <path>` (the send's next says exactly), or a person saves it from the ibara console, and only then is the delivery verified. A send or overwrite that needs approval returns status pending with an att_ ref and next, and does not run; wait for the att_ with computer_wait, then send the same request (same request_id) again, which runs it once if approved.",
        example: r#"{"task_ref": "task_3f2a", "request_id": "req-5", "op": "publish", "path": "dogfood.txt"}"#,
        schema: schema_of::<FilesInput>,
    },
    Tool {
        name: "computer_checkpoint",
        summary: "Save a continuation note or ask a person. stop_asking requests permission to send, spend and delete without further approval.",
        long: "note is stored as your continuation note for the task. ask {question, options?} raises an attention item; the answer appears in the next situation and since, and computer_wait({for: {attention}}) waits for it. stop_asking true is for when your person tells you that you need not ask them before you send, spend or delete: you cannot change your own access, so ibara shows them one request for this computer (Allow or Not Now) and changes nothing until they allow it. The reply's stop_asking holds its att_ (state waiting_for_person), or state nothing_to_ask when those steps already run without asking here. Carry on meanwhile; ask on each computer they meant.",
        example: r#"{"task_ref": "task_3f2a", "ask": {"question": "Overwrite the existing report?", "options": ["yes", "no"]}}"#,
        schema: schema_of::<CheckpointInput>,
    },
    Tool {
        name: "computer_finish",
        summary: "End work and cancel remaining jobs, evaluate checks, release control and verify window cleanup. Save outputs first. Read cleanup/notes; claim complete only with verified checks.",
        long: "outcome is complete, partial, cancelled or blocked. assessments [{check, met, reason}] cover checks whose basis is your_assessment and are attributed to you; an assessment of an automatic check counts only when ibara could not read it (no accessibility tree, page reader not answering), else it is ignored; notes says which. On personal desktops, touched or apparently unsaved windows are left open. Disposable desktops politely close all windows regardless of ownership; verified task-owned Mousepad save dialogs are resolved automatically, discarding unsaved task edits without overwriting saved files. Apps that do not close on request are then force-closed, discarding unsaved work. Save required work before finish. After your control ended (a person took the computer, it expired), finish still records the outcome and summary without taking control back; windows then stay open while someone else has the computer.",
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
                // FilesInput is a union of strict objects; closing the union
                // root without properties would reject every variant.
                if root.contains_key("properties") {
                    root.insert("additionalProperties".into(), false.into());
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

/// Remove presentation-only metadata. Keep validation constraints and each
/// nested shape intact so a tool works when a harness selects it alone.
fn compact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for key in ["$schema", "title", "format", "default", "examples"] {
                map.shift_remove(key);
            }
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    map.insert("type".into(), only);
                }
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

/// Self-contained discriminated alternatives. Each branch retains its own
/// required fields and denies fields from other variants, just like parsing.
pub(crate) fn tagged_schema(tag: &str, variants: &[(&str, Schema)]) -> Schema {
    let branches: Vec<Value> = variants.iter().map(|(kind, schema)| {
        let mut branch = schema.clone().to_value();
        let object = branch.as_object_mut().expect("tagged variant is an object");
        object.entry("properties").or_insert_with(|| json!({})).as_object_mut()
            .expect("variant properties").insert(tag.into(), json!({"const": kind}));
        object.entry("required").or_insert_with(|| json!([]))
            .as_array_mut().expect("required array").push(tag.into());
        object.insert("additionalProperties".into(), false.into());
        branch
    }).collect();
    schemars::json_schema!({"type":"object", "oneOf":branches})
}
