//! The compact text rendering of an envelope: MCP `content[0]`.

use super::{Envelope, Status};
use serde_json::{Map, Value};
use std::fmt::Write;

/// Longest rendering of a value the renderer does not know.
const OTHER_MAX: usize = 300;

/// The situation line, then status, `since`, the result and the error, in a
/// few short lines. Never a JSON dump of the whole result.
pub fn render_text(envelope: &Envelope) -> String {
    let mut out = envelope.situation.clone();
    if envelope.status == Status::Pending {
        out.push_str("\nstatus: pending");
    }
    if !envelope.since.is_empty() {
        let _ = write!(out, "\nsince: {}", envelope.since.join(" · "));
    }
    match &envelope.result {
        Value::Null => {}
        Value::Object(map) => render_object(map, &mut out),
        other => {
            out.push('\n');
            out.push_str(&short(other));
        }
    }
    if let Some(err) = &envelope.error {
        render_error(err, &mut out);
    }
    out
}

/// Keys rendered by their own sections, in this order, after the plain fields.
const SECTIONS: &[&str] = &["help", "computers", "checks", "steps", "entries", "artifacts", "cleanup", "frame"];

fn render_object(map: &Map<String, Value>, out: &mut String) {
    let mut plain = Vec::new();
    for (key, value) in map {
        if SECTIONS.contains(&key.as_str()) || key == "next" || value.is_null() {
            continue;
        }
        plain.push(format!("{key}: {}", short(value)));
    }
    if !plain.is_empty() {
        out.push('\n');
        out.push_str(&plain.join(" · "));
    }
    render_next(map.get("next"), out);
    for key in SECTIONS {
        let Some(value) = map.get(*key) else { continue };
        match *key {
            "help" => {
                out.push('\n');
                out.push_str(value.as_str().unwrap_or_default());
            }
            "frame" => render_frame(value, out),
            "cleanup" => {
                let closed = names(value.get("closed"));
                let _ = write!(out, "\nclosed: {}", if closed.is_empty() { "nothing".into() } else { closed.join(", ") });
                for left in items(value.get("left")) {
                    let _ = write!(out, "\nleft open: {} ({})", s(left, "surface"), s(left, "reason"));
                }
            }
            _ => {
                for item in items(Some(value)) {
                    out.push('\n');
                    out.push_str(&row(key, item));
                }
            }
        }
    }
}

fn row(section: &str, item: &Value) -> String {
    match section {
        "computers" => {
            let mut line = format!("{} {} {}", s(item, "name"), s(item, "id"), s(item, "state"));
            for (key, label) in [("user", "user"), ("holder", "held by")] {
                if let Some(v) = item.get(key).and_then(Value::as_str) {
                    let _ = write!(line, " · {label} {v}");
                }
            }
            let _ = write!(line, " · {}", s(item, "capabilities"));
            line
        }
        "checks" => {
            let mut line = format!("check {} {} ({})", s(item, "id"), s(item, "state"), s(item, "basis"));
            if let Some(d) = item.get("detail").and_then(Value::as_str) {
                let _ = write!(line, ": {d}");
            }
            line
        }
        "steps" => {
            let mut line = format!("step {} {}: {}", short(&item["index"]), s(item, "outcome"), s(item, "effect"));
            if let Some(op) = item.get("op_ref").and_then(Value::as_str) {
                let _ = write!(line, " ({op})");
            }
            line
        }
        "entries" => format!("{} {} {}", s(item, "kind"), s(item, "name"), short(&item["size"])),
        "artifacts" => format!("{} {} {}", s(item, "art_ref"), s(item, "path"), s(item, "sha256")),
        _ => short(item),
    }
}

fn render_frame(frame: &Value, out: &mut String) {
    let _ = write!(
        out,
        "\nframe {} r{} · {} · {} B",
        s(frame, "frame_ref"),
        short(&frame["revision"]),
        s(frame, "covered"),
        short(&frame["cost_bytes"])
    );
    for line in names(frame.get("lines")) {
        out.push('\n');
        out.push_str(&line);
    }
    let choices = items(frame.get("choices"));
    if !choices.is_empty() {
        out.push_str("\nchoices:");
        for c in choices {
            let _ = write!(out, "\n{} {}", s(c, "choice_id"), s(c, "label"));
            if let Some(p) = c.get("param").and_then(Value::as_str) {
                let _ = write!(out, " (needs {p})");
            }
        }
    }
    if let Some(v) = frame.get("next_richer").and_then(Value::as_str) {
        let _ = write!(out, "\nricher view: {v}");
    }
    if let Some(v) = frame.get("next_cursor").and_then(Value::as_str) {
        let _ = write!(out, "\nmore: cursor {v}");
    }
}

fn render_error(err: &Value, out: &mut String) {
    let _ = write!(out, "\nerror {}: {}", s(err, "code"), s(err, "message"));
    let mut flags = Vec::new();
    if err.get("retry_safe") == Some(&Value::Bool(false)) {
        flags.push("not safe to retry as is".to_string());
    }
    if err.get("requires_reconciliation") == Some(&Value::Bool(true)) {
        flags.push("reconcile before anything else".to_string());
    }
    if let Value::Object(map) = err {
        for (key, value) in map {
            if !matches!(key.as_str(), "code" | "message" | "retry_safe" | "requires_reconciliation" | "next" | "field") {
                flags.push(format!("{key}: {}", short(value)));
            }
        }
    }
    if !flags.is_empty() {
        let _ = write!(out, "\n{}", flags.join(" · "));
    }
    render_next(err.get("next"), out);
}

/// Next moves, one per line and never cut: they are what the agent does next.
fn render_next(next: Option<&Value>, out: &mut String) {
    match next {
        Some(Value::String(next)) => {
            let _ = write!(out, "\nnext: {next}");
        }
        Some(Value::Array(moves)) => {
            for m in moves {
                let _ = write!(out, "\nnext: {}", m.as_str().map_or_else(|| short(m), str::to_string));
            }
        }
        _ => {}
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("?")
}

fn items(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()
}

fn names(v: Option<&Value>) -> Vec<String> {
    items(v).iter().map(short).collect()
}

/// A scalar as text; anything else as compact JSON, cut at `OTHER_MAX` bytes.
fn short(v: &Value) -> String {
    let text = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.len() <= OTHER_MAX {
        return text;
    }
    let mut end = OTHER_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
