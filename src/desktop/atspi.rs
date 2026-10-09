//! The element model the controller reads: accessibility elements as Cua
//! reports them ([`super::cua`]), made compact and ranked.
//!
//! Elements become compact [`Element`]s: unnamed fillers dropped, the ancestor
//! path reduced to its row, dialog and document context, and an optional
//! ranking by the query.

use super::run::clip;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most elements one page returns.
pub const MAX_LIMIT: u32 = 100;

/// One element before compaction.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawElement {
    pub selector: Value,
    pub role: String,
    pub name: String,
    pub states: Vec<String>,
    pub ancestor: String,
    pub actions: Vec<String>,
    pub text: String,
}

/// A window's elements.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub available: bool,
    pub truncated: bool,
    pub elements: Vec<RawElement>,
    pub text: String,
    pub available_count: Option<u64>,
    pub returned_count: u64,
}

/// An element resolved again by identity.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ResolvedElement {
    pub role: String,
    pub name: String,
    pub states: Vec<String>,
    pub actions: Vec<String>,
    pub text: String,
}

/// A compact element for frames and choices.
#[derive(Debug, Clone, Serialize)]
pub struct Element {
    /// `e<n>`: position in the query's result order, stable across pages.
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub states: Vec<String>,
    pub actions: Vec<String>,
    /// The nearest named ancestor, as `role "name"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Row, dialog and document context, outermost first, e.g. `dialog "Save As"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// The element's identity, needed to resolve or act on it again.
    #[serde(skip)]
    pub selector: Value,
}

/// One page of elements.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ElementPage {
    /// False when the application exposes no usable tree for this window.
    pub available: bool,
    pub elements: Vec<Element>,
    /// More matches exist, or the traversal was cut short.
    pub truncated: bool,
    /// Matches in the whole window, when the traversal was complete.
    pub total: Option<u64>,
    /// Pass back as `cursor` for the next page.
    pub next_cursor: Option<u32>,
    /// The window's visible text, bounded.
    pub text: String,
}

fn clip_owned(text: &str, max: usize) -> String {
    clip(text, max).to_string()
}

/// Split an ancestor path (`frame:T/panel:/push button:Save`) into `(role, name)`
/// pairs. Role names are lower-case words, so a `/` inside a name is kept as
/// part of that name.
pub fn parse_path(path: &str) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    if path.is_empty() {
        return parts;
    }
    for segment in path.split('/') {
        let starts_node = segment
            .split_once(':')
            .is_some_and(|(role, _)| !role.is_empty() && role.bytes().all(|b| b.is_ascii_lowercase() || b == b' '));
        match (starts_node, parts.last_mut()) {
            (true, _) | (false, None) => {
                let (role, name) = segment.split_once(':').unwrap_or(("", segment));
                parts.push((role.to_string(), name.to_string()));
            }
            (false, Some(last)) => {
                last.1.push('/');
                last.1.push_str(segment);
            }
        }
    }
    parts
}

fn describe(role: &str, name: &str) -> String {
    format!("{role} \"{}\"", clip(name, 120))
}

fn context_kind(role: &str) -> bool {
    matches!(
        role,
        "dialog"
            | "alert"
            | "file chooser"
            | "color chooser"
            | "font chooser"
            | "table row"
            | "list item"
            | "tree item"
            | "row"
            | "page tab"
            | "form"
            | "document frame"
            | "document web"
            | "document text"
            | "document email"
            | "document spreadsheet"
            | "document presentation"
    )
}

fn meaningful(raw: &RawElement, value: &Option<String>) -> bool {
    !raw.name.trim().is_empty()
        || value.is_some()
        || !raw.actions.is_empty()
        || raw.states.iter().any(|s| matches!(s.as_str(), "focused" | "editable" | "checked" | "selected"))
}

/// Turn elements into compact elements, dropping unnamed fillers.
/// `first_index` is the offset of `raw[0]`, so ids stay stable across pages.
pub fn compact(raw: Vec<RawElement>, first_index: u32) -> Vec<Element> {
    let mut out = Vec::with_capacity(raw.len());
    for (i, element) in raw.into_iter().enumerate() {
        let value = (!element.text.is_empty() && element.text != element.name).then(|| clip_owned(&element.text, 400));
        if !meaningful(&element, &value) {
            continue;
        }
        let ancestors = parse_path(&element.ancestor);
        let parent = ancestors.iter().rev().find(|(_, name)| !name.trim().is_empty()).map(|(role, name)| describe(role, name));
        let mut context: Vec<String> = ancestors
            .iter()
            .filter(|(role, name)| context_kind(role) && !name.trim().is_empty())
            .map(|(role, name)| describe(role, name))
            .collect();
        if context.len() > 3 {
            context.drain(..context.len() - 3);
        }
        out.push(Element {
            id: format!("e{}", first_index as usize + i + 1),
            role: clip_owned(if element.role.is_empty() { "unknown" } else { &element.role }, 80),
            name: clip_owned(&element.name, 1000),
            value,
            states: element.states.into_iter().take(10).collect(),
            actions: element.actions.into_iter().take(12).collect(),
            parent,
            context,
            selector: element.selector,
        });
    }
    out
}

fn score(element: &Element, query: &str) -> i32 {
    let name = element.name.to_lowercase();
    let mut score = if name == query {
        100
    } else if name.starts_with(query) {
        60
    } else if name.contains(query) {
        40
    } else if element.value.as_deref().is_some_and(|v| v.to_lowercase().contains(query)) {
        25
    } else if element.role.to_lowercase().contains(query) {
        20
    } else if element.context.iter().chain(&element.parent).any(|c| c.to_lowercase().contains(query)) {
        10
    } else {
        0
    };
    let actionable = element.actions.iter().any(|a| {
        let a = a.to_lowercase();
        a.contains("click") || a.contains("press") || a.contains("activate") || a.contains("toggle")
    }) || element.states.iter().any(|s| s == "editable");
    if actionable {
        score += 5;
    }
    if element.states.iter().any(|s| s == "focused") {
        score += 3;
    }
    if !element.states.iter().any(|s| s == "enabled" || s == "sensitive") {
        score -= 5;
    }
    score
}

/// Ranking hook: with a query, best matches first (exact name, prefix,
/// substring, value, role, context), actionable and focused elements ahead of
/// equal ones, disabled ones behind. Without a query, document order stays.
pub fn rank(elements: &mut [Element], query: Option<&str>) {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return;
    };
    let query = query.to_lowercase();
    elements.sort_by_cached_key(|e| std::cmp::Reverse(score(e, &query)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(role: &str, name: &str, ancestor: &str, actions: &[&str], states: &[&str]) -> RawElement {
        RawElement {
            role: role.into(),
            name: name.into(),
            ancestor: ancestor.into(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            states: states.iter().map(|s| s.to_string()).collect(),
            ..RawElement::default()
        }
    }

    #[test]
    fn slashes_inside_names_stay_in_the_name() {
        assert_eq!(
            parse_path("frame:Save to /home/riley/notes - Mousepad/panel:/push button:Save"),
            vec![
                ("frame".to_string(), "Save to /home/riley/notes - Mousepad".to_string()),
                ("panel".to_string(), String::new()),
                ("push button".to_string(), "Save".to_string()),
            ]
        );
    }

    #[test]
    fn unnamed_fillers_are_dropped_but_unnamed_controls_kept() {
        let elements = compact(
            vec![
                raw("filler", "", "frame:T", &[], &["visible", "showing"]),
                raw("panel", "", "frame:T", &[], &["enabled"]),
                raw("push button", "", "frame:T", &["click"], &["enabled"]),
                raw("text", "", "frame:T", &[], &["editable", "focused"]),
            ],
            10,
        );
        let kept: Vec<(&str, &str)> = elements.iter().map(|e| (e.id.as_str(), e.role.as_str())).collect();
        assert_eq!(kept, vec![("e13", "push button"), ("e14", "text")]);
    }

    #[test]
    fn context_names_the_row_and_dialog_an_element_belongs_to() {
        let elements = compact(
            vec![raw(
                "push button",
                "Open",
                "dialog:Save As/table:/table row:Invoice 12/table cell:",
                &["click"],
                &["enabled"],
            )],
            0,
        );
        assert_eq!(elements[0].context, vec!["dialog \"Save As\"", "table row \"Invoice 12\""]);
        assert_eq!(elements[0].parent.as_deref(), Some("table row \"Invoice 12\""));
    }

    #[test]
    fn ranking_puts_exact_names_before_substrings_and_disabled_last() {
        let mut elements = compact(
            vec![
                raw("push button", "Save As…", "frame:T", &["click"], &["enabled"]),
                raw("push button", "Save", "frame:T", &["click"], &[]),
                raw("label", "Autosave", "frame:T", &[], &["enabled"]),
                raw("push button", "Save", "frame:T", &["click"], &["enabled"]),
            ],
            0,
        );
        rank(&mut elements, Some("save"));
        let order: Vec<&str> = elements.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(order, vec!["e4", "e2", "e1", "e3"]);
    }
}
