//! Keep a response inside the client's byte limit (`response-budget.ts`).
//!
//! Contract 4 has one small envelope per response, so the ladder is short:
//! drop images, then shorten frame lines and choices, then keep only the
//! references that let the agent read the rest through `computer_status`.

use crate::contract::Envelope;
use crate::mcp::Image;
use serde_json::{Map, Value, json};

/// What a response may cost, text and base64 images together.
pub(crate) const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SINCE: usize = 12;

fn size(envelope: &Envelope, images: &[Image]) -> usize {
    serde_json::to_vec(envelope).map_or(usize::MAX, |v| v.len()) + images.iter().map(|i| i.base64.len() + 64).sum::<usize>()
}

fn frames_mut(result: &mut Value) -> Vec<&mut Map<String, Value>> {
    let mut out = Vec::new();
    if let Some(obj) = result.as_object_mut()
        && let Some(Value::Object(frame)) = obj.get_mut("frame")
    {
        out.push(frame);
    }
    out
}

fn shorten(result: &mut Value, lines: usize, choices: usize) {
    for frame in frames_mut(result) {
        if let Some(Value::Array(items)) = frame.get_mut("lines")
            && items.len() > lines
        {
            let dropped = items.len() - lines;
            items.truncate(lines);
            items.push(json!(format!("… {dropped} more lines; observe with view \"elements\" and a query")));
        }
        if let Some(Value::Array(items)) = frame.get_mut("choices") {
            items.truncate(choices);
        }
    }
}

/// Keep only references and outcomes.
fn skeleton(result: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = result.as_object() {
        for key in ["task_ref", "computer", "you", "checks", "steps", "complete", "attention", "met", "state"] {
            if let Some(v) = obj.get(key) {
                out.insert(key.into(), v.clone());
            }
        }
        if let Some(frame_ref) = obj.get("frame").and_then(|f| f.get("frame_ref")) {
            out.insert("frame_ref".into(), frame_ref.clone());
        }
    }
    out.insert("truncated".into(), json!("The full result exceeded the response limit; read references with computer_status."));
    Value::Object(out)
}

/// Fit `envelope` and `images` within `max` bytes.
pub(crate) fn fit(envelope: &mut Envelope, images: &mut Vec<Image>, max: usize) {
    if envelope.since.len() > MAX_SINCE {
        let extra = envelope.since.len() - MAX_SINCE;
        envelope.since.drain(..extra);
    }
    if size(envelope, images) <= max {
        return;
    }
    if !images.is_empty() {
        images.clear();
        envelope.since.push("image omitted to stay under the response limit".into());
        if size(envelope, images) <= max {
            return;
        }
    }
    for (lines, choices) in [(40, 20), (12, 8), (4, 3)] {
        shorten(&mut envelope.result, lines, choices);
        if size(envelope, images) <= max {
            return;
        }
    }
    envelope.result = skeleton(&envelope.result);
    if size(envelope, images) > max {
        envelope.result = json!({ "truncated": "Result omitted; read references with computer_status." });
        envelope.since.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Status;

    fn envelope_with_lines(n: usize) -> Envelope {
        let lines: Vec<String> = (0..n).map(|i| format!("e{i} button \"Button number {i}\" enabled")).collect();
        Envelope {
            situation: "Tulip1 · r1".into(),
            status: Status::Ok,
            since: Vec::new(),
            result: json!({ "task_ref": "task_1", "frame": { "frame_ref": "frame_1", "lines": lines, "choices": [] } }),
            error: None,
        }
    }

    #[test]
    fn over_limit_drops_images_before_touching_the_frame() {
        let mut env = envelope_with_lines(5);
        let mut images = vec![Image { mime: "image/webp".into(), base64: "A".repeat(10_000) }];
        fit(&mut env, &mut images, 4_000);
        assert!(images.is_empty());
        assert_eq!(env.result["frame"]["lines"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn a_frame_too_large_keeps_its_reference_and_the_task() {
        let mut env = envelope_with_lines(5_000);
        let mut images = Vec::new();
        fit(&mut env, &mut images, 300);
        assert!(serde_json::to_vec(&env).unwrap().len() <= 300);
        assert_eq!(env.result["task_ref"], "task_1");
        assert_eq!(env.result["frame_ref"], "frame_1");
    }
}
