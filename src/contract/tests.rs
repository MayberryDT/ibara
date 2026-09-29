use super::*;
use serde_json::json;

fn message_of<T: DeserializeOwned + Validate + fmt::Debug>(tool: &str, args: Value) -> String {
    let err = parse_input::<T>(tool, &args).expect_err("input should be refused");
    assert_eq!(err.code, "INVALID_ARGUMENT");
    err.message
}

fn act(steps: Value) -> Value {
    json!({ "task_ref": "task_1", "request_id": "r1", "steps": steps })
}

fn key_step() -> Value {
    json!({ "action": { "kind": "key", "keys": "Return" } })
}

// ---- parse_input: failure cases name the path ----

#[test]
fn unknown_top_level_field_is_named() {
    let m = message_of::<StatusInput>("computer_status", json!({ "reff": "task_1" }));
    assert!(m.starts_with("reff: unknown field"), "{m}");
}

#[test]
fn unknown_field_inside_a_tagged_variant_is_named() {
    let args = act(json!([key_step(), { "action": { "kind": "key", "keys": "a", "hold_ms": 5 } }]));
    let m = message_of::<ActInput>("computer_act", args);
    assert!(m.starts_with("steps[1].action.hold_ms: unknown field"), "{m}");
}

#[test]
fn wrong_expectation_kind_in_nested_step_names_kind_and_lists_variants() {
    let bad = json!({ "action": { "kind": "key", "keys": "ctrl+s" }, "expect": { "kind": "windw", "title": "Save As" } });
    let m = message_of::<ActInput>("computer_act", act(json!([key_step(), key_step(), bad])));
    assert!(
        m.starts_with("steps[2].expect.kind: unknown variant 'windw', expected one of window, dialog, focus, text"),
        "{m}"
    );
}

#[test]
fn wrong_type_inside_a_variant_is_named() {
    let bad = json!({ "expect": { "kind": "settled", "quiet_ms": "soon" } });
    let m = message_of::<ActInput>("computer_act", act(json!([bad])));
    assert!(m.starts_with("steps[0].expect.quiet_ms: invalid type"), "{m}");
}

#[test]
fn missing_required_field_is_named() {
    let m = message_of::<BeginInput>("computer_begin", json!({ "computer": "tulip1", "goal": "x" }));
    assert_eq!(m, "request_id: required field is missing");
    let m = message_of::<ActInput>("computer_act", act(json!([{ "expect": { "kind": "text" } }])));
    assert_eq!(m, "steps[0].expect.text: required field is missing");
}

#[test]
fn missing_kind_is_named() {
    let m = message_of::<ActInput>("computer_act", act(json!([{ "action": { "keys": "a" } }])));
    assert!(m.starts_with("steps[0].action.kind: required field is missing"), "{m}");
}

#[test]
fn more_than_eight_steps_is_refused_at_steps() {
    let steps: Vec<Value> = (0..9).map(|_| key_step()).collect();
    let m = message_of::<ActInput>("computer_act", act(json!(steps)));
    assert_eq!(m, "steps: at most 8 items allowed, got 9");
    let steps: Vec<Value> = (0..8).map(|_| key_step()).collect();
    assert!(parse_input::<ActInput>("computer_act", &act(json!(steps))).is_ok());
}

#[test]
fn more_than_twenty_checks_is_refused_at_checks() {
    let checks: Vec<Value> = (0..21).map(|i| json!({ "id": format!("c{i}"), "description": "d" })).collect();
    let args = json!({ "computer": "tulip1", "goal": "g", "checks": checks, "request_id": "r1" });
    let m = message_of::<BeginInput>("computer_begin", args);
    assert_eq!(m, "checks: at most 20 items allowed, got 21");
}

#[test]
fn nested_typed_check_error_is_named() {
    let args = json!({ "computer": "tulip1", "goal": "g", "request_id": "r1",
        "checks": [{ "id": "a", "description": "d", "check": { "kind": "file_exist", "path": "x" } }] });
    let m = message_of::<BeginInput>("computer_begin", args);
    assert!(m.starts_with("checks[0].check.kind: unknown variant 'file_exist'"), "{m}");
}

#[test]
fn bad_reference_is_named() {
    let m = message_of::<ObserveInput>("computer_observe", json!({ "task_ref": "task 1" }));
    assert!(m.starts_with("task_ref: 'task 1' is not a reference"), "{m}");
}

#[test]
fn unknown_simple_enum_value_lists_variants() {
    let m = message_of::<ObserveInput>("computer_observe", json!({ "task_ref": "t", "view": "pixels" }));
    assert_eq!(m, "view: unknown variant 'pixels', expected one of situation, elements, image, screen");
}

#[test]
fn wait_for_expectation_error_is_named() {
    let args = json!({ "task_ref": "t", "for": { "expect": { "kind": "url" } }, "deadline_ms": 1000 });
    let m = message_of::<WaitInput>("computer_wait", args);
    assert_eq!(m, "for.expect.contains: required field is missing");
}

#[test]
fn files_op_errors_are_named() {
    let m = message_of::<FilesInput>("computer_files", json!({ "task_ref": "t", "request_id": "r", "op": "move" }));
    assert!(m.starts_with("op: unknown variant 'move'"), "{m}");
    let m = message_of::<FilesInput>("computer_files", json!({ "task_ref": "t", "op": "status" }));
    assert_eq!(m, "request_id: required field is missing");
    let args = json!({ "task_ref": "t", "request_id": "r", "op": "send", "path": "a", "to": { "host": "vesper" } });
    let m = message_of::<FilesInput>("computer_files", args);
    assert_eq!(m, "to.path: required field is missing");
}

// ---- cross-field rules ----

#[test]
fn choice_and_action_together_are_refused() {
    let step = json!({ "choice": "c3", "action": { "kind": "key", "keys": "a" } });
    let m = message_of::<ActInput>("computer_act", act(json!([key_step(), step])));
    assert_eq!(m, "steps[1].action: give a choice or an action, not both");
}

#[test]
fn single_step_fields_beside_steps_are_refused() {
    let mut args = act(json!([key_step()]));
    args["choice"] = json!("c1");
    let m = message_of::<ActInput>("computer_act", args);
    assert!(m.starts_with("choice: give steps or a single step"), "{m}");
}

#[test]
fn empty_act_is_refused() {
    let m = message_of::<ActInput>("computer_act", json!({ "task_ref": "t", "request_id": "r" }));
    assert_eq!(m, "action: give a choice, an action or an expect");
}

#[test]
fn file_content_check_needs_exactly_one_comparison() {
    let args = json!({ "computer": "tulip1", "goal": "g", "request_id": "r1",
        "checks": [{ "id": "a", "description": "d", "check": { "kind": "file_content", "path": "x" } }] });
    let m = message_of::<BeginInput>("computer_begin", args);
    assert_eq!(m, "checks[0].check.equals: give exactly one of equals or contains");
}

#[test]
fn duplicate_check_ids_are_refused() {
    let args = json!({ "computer": "tulip1", "goal": "g", "request_id": "r1",
        "checks": [{ "id": "a", "description": "d" }, { "id": "a", "description": "e" }] });
    let m = message_of::<BeginInput>("computer_begin", args);
    assert_eq!(m, "checks[1].id: duplicate check id 'a'");
}

#[test]
fn error_names_field_and_help() {
    let err = parse_input::<StatusInput>("computer_status", &json!({ "x": 1 })).unwrap_err();
    let body = err.to_json();
    assert_eq!(body["field"], "x");
    assert!(body["next"].as_str().unwrap().contains("help:computer_status"));
}

// ---- accepted input round-trips ----

#[test]
fn contract_example_act_parses_into_three_steps() {
    let args: Value = serde_json::from_str(
        r#"{"task_ref":"task_3f2a","request_id":"req-2","steps":[
            {"action":{"kind":"key","keys":"ctrl+s"},"expect":{"kind":"dialog","title":"Save As"}},
            {"action":{"kind":"type","text":"dogfood.txt"}},
            {"action":{"kind":"click","target":{"x":10,"y":20}},"expect":{"kind":"dialog","gone":true,"within_ms":900}},
            {"expect":{"kind":"file","path":"~/dogfood.txt"}}]}"#,
    )
    .unwrap();
    let input = parse_input::<ActInput>("computer_act", &args).unwrap();
    let steps = input.into_steps();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[2].expect.as_ref().and_then(Expectation::within_ms), Some(900));
    assert!(matches!(&steps[2].action, Some(Action::Click(TargetAction { target: Target::Point(p) })) if p.x == 10));
    // Serializing gives back the wire shape.
    assert_eq!(serde_json::to_value(&steps[0]).unwrap(), args["steps"][0]);
}

#[test]
fn files_input_round_trips() {
    let args = json!({ "task_ref": "t", "request_id": "r", "op": "write", "path": "a.txt", "text": "hi" });
    let input = parse_input::<FilesInput>("computer_files", &args).unwrap();
    assert!(matches!(&input.op, FilesOp::Write(w) if w.text.as_deref() == Some("hi")));
    assert_eq!(serde_json::to_value(&input).unwrap(), args);
}

// ---- tool list ----

const SCHEMA_KEYWORDS: &[&str] = &[
    "type", "properties", "required", "additionalProperties", "items", "enum", "const", "description",
    "pattern", "minimum", "maximum", "minItems", "maxItems", "anyOf", "oneOf",
];

/// Structural JSON Schema check: known keywords only, valid type names,
/// `required` names declared properties, and no dangling `$ref`.
fn assert_schema(path: &str, schema: &Value) {
    let obj = schema.as_object().unwrap_or_else(|| panic!("{path}: schema is not an object"));
    // A flat union's description names each kind's fields; all must be declared.
    let listing = obj.get("description").and_then(Value::as_str).and_then(|d| d.strip_prefix("By "));
    if let Some((_, listing)) = listing.and_then(|d| d.split_once(": ")) {
        let props = obj["properties"].as_object().expect("union properties");
        for group in listing.split(") ").filter_map(|g| g.split_once('(')) {
            for field in group.1.trim_end_matches(')').split(' ') {
                assert!(props.contains_key(field.trim_end_matches('*')), "{path}: {field} listed but not declared");
            }
        }
    }
    for (key, value) in obj {
        assert!(SCHEMA_KEYWORDS.contains(&key.as_str()), "{path}: unexpected keyword {key}");
        match key.as_str() {
            "type" => {
                let names: Vec<&Value> = match value {
                    Value::Array(a) => a.iter().collect(),
                    v => vec![v],
                };
                for n in names {
                    assert!(
                        matches!(n.as_str(), Some("object" | "array" | "string" | "integer" | "number" | "boolean")),
                        "{path}: bad type {n}"
                    );
                }
            }
            "properties" => {
                for (name, sub) in value.as_object().expect("properties object") {
                    assert_schema(&format!("{path}.{name}"), sub);
                }
            }
            "required" => {
                let props = obj.get("properties").and_then(Value::as_object).expect("required without properties");
                for name in value.as_array().expect("required array") {
                    assert!(props.contains_key(name.as_str().unwrap()), "{path}: required {name} not declared");
                }
            }
            "items" => assert_schema(&format!("{path}[]"), value),
            "anyOf" | "oneOf" => {
                for (i, sub) in value.as_array().expect("alternatives array").iter().enumerate() {
                    assert_schema(&format!("{path}|{i}"), sub);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn tool_list_is_eleven_valid_object_schemas_within_nine_kib() {
    let tools = tool_definitions();
    assert_eq!(tools.len(), 11);
    let json: Vec<Value> = tools.iter().map(ToolDef::to_mcp).collect();
    let size = serde_json::to_vec(&json).unwrap().len();
    assert!(size <= 9 * 1024, "tools/list is {size} bytes");
    for tool in &tools {
        assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
        assert_schema(tool.name, &tool.input_schema);
        assert!(help(tool.name).is_some_and(|h| h.contains("Example:")), "{}", tool.name);
    }
    assert!(help("computer_teleport").is_none());
}

#[test]
fn every_help_example_parses() {
    for tool in tool_definitions() {
        let help = help(tool.name).unwrap();
        let example = help.rsplit_once(&format!("{}(", tool.name)).unwrap().1.trim_end_matches(')');
        let args: Value = serde_json::from_str(example).unwrap();
        let parsed = match tool.name {
            "computer_status" => parse_input::<StatusInput>(tool.name, &args).map(drop),
            "computer_begin" => parse_input::<BeginInput>(tool.name, &args).map(drop),
            "computer_observe" => parse_input::<ObserveInput>(tool.name, &args).map(drop),
            "computer_act" => parse_input::<ActInput>(tool.name, &args).map(drop),
            "browser_act" => parse_input::<BrowserActInput>(tool.name, &args).map(drop),
            "computer_exec" => parse_input::<ExecInput>(tool.name, &args).map(drop),
            "computer_wait" => parse_input::<WaitInput>(tool.name, &args).map(drop),
            "computer_files" => parse_input::<FilesInput>(tool.name, &args).map(drop),
            "computer_checkpoint" => parse_input::<CheckpointInput>(tool.name, &args).map(drop),
            "computer_finish" => parse_input::<FinishInput>(tool.name, &args).map(drop),
            "computer_procedures" => parse_input::<ProceduresInput>(tool.name, &args).map(drop),
            other => panic!("no parser for {other}"),
        };
        parsed.unwrap_or_else(|e| panic!("{} example: {e}", tool.name));
    }
}

// ---- rendering ----

#[test]
fn render_shows_frame_choices_steps_and_error_moves_without_json_dump() {
    let frame = Frame {
        frame_ref: "frame_1".into(),
        revision: 7,
        captured_at: "2026-09-25T10:00:00.000Z".into(),
        covered: "situation".into(),
        cost_bytes: 812,
        lines: vec!["e12 button \"Save\" enabled · dialog \"Save As\"".into()],
        choices: vec![Choice {
            choice_id: "c3".into(),
            label: "Type the file name".into(),
            action: Action::Focus(SurfaceAction { surface: "Save As".into() }),
            param: Some("text".into()),
        }],
        next_richer: Some(View::Image),
        next_cursor: None,
    };
    let result = ActResult {
        steps: vec![StepResult { index: 0, outcome: StepOutcome::Unmet, effect: "pressed ctrl+s".into(), op_ref: Some("op_9".into()) }],
        frame: Some(frame),
        attention: None,
        next: None,
    };
    let text = render_text(&Envelope::ok("Tulip1 · r7", vec!["window \"Save As\" opened".into()], result));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Tulip1 · r7");
    assert_eq!(lines[1], "since: window \"Save As\" opened");
    assert!(text.contains("\nstep 0 unmet: pressed ctrl+s (op_9)"), "{text}");
    assert!(text.contains("\nframe frame_1 r7 · situation · 812 B\ne12 button \"Save\" enabled"), "{text}");
    assert!(text.contains("\nc3 Type the file name (needs text)"), "{text}");
    assert!(text.contains("\nricher view: image"), "{text}");
    assert!(!text.contains('{'), "{text}");

    let err = crate::error::IbaraError::new("STALE_TARGET", "e12 is gone", true);
    let text = render_text(&Envelope::error("Tulip1", vec![], &err));
    assert!(text.contains("\nerror STALE_TARGET: e12 is gone\nnext: The thing you pointed at changed."), "{text}");
}
