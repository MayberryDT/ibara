//! A person's approval is asked for an action, not a request. While an
//! approval is open, the same action under a new request is refused
//! whatever effect the agent declares. Once a person answered it, declined
//! or approved, or it expired, the same action asks again, as at least the
//! class it was held under: it neither runs without a person nor is refused
//! for the rest of the task. ibara also classes what it sees itself: a
//! press the page says submits a form, a Send button.

use super::{BrowserOp, Planned, Resolved, is_browser, target_identity};
use crate::controller::Controller;
use crate::controller::ports::{Effect, Tab, Win, WinKey};
use crate::controller::situation::{BrowserNode, FrameState};
use crate::desktop::input::cua_keys;
use crate::error::{IbaraError, Result, denied};
use serde_json::{Map, Value, json};

/// Clicks on the screen this close to an earlier one are the same click.
const SAME_POINT: f64 = 16.0;

/// What earlier approvals say about a step.
pub(super) enum Guard {
    /// No approval was asked for this action.
    Clear,
    /// Its approval is open.
    Refuse(IbaraError),
    /// A person answered it before, or its approval ended: it counts as at
    /// least this class. `true` when the latest answer was no: then it asks
    /// whatever the class's rule, so a step a person refused never runs
    /// without asking. Otherwise the class's rule decides, and a step a
    /// person lets run without asking runs.
    AskAgain(&'static str, bool),
}

/// What a key meets, read just before it is sent: the page its browser
/// window shows, without its query (null when ibara cannot tell), and
/// whether the page reader says the press submits a form.
#[derive(Default)]
pub(super) struct Focus {
    page: Value,
    submits: bool,
}

/// The action a step takes, the same whichever tool or request sends it. A
/// key is the key in its window and, in a browser, the page the window
/// shows. A click is where it lands, not how: the page element (its role,
/// name, place and page, not the observation's token), the desktop element
/// or the point on the screen. Typing is the text and where it goes.
pub(super) fn action(resolved: &Resolved, frame: Option<&FrameState>, focus: &Focus) -> Value {
    let mut id = target_identity(resolved);
    let key = |combo: &str, window: &Value| json!({ "key": keys(combo), "window": window, "page": focus.page });
    match &resolved.plan {
        Planned::Browser(step) => {
            if let BrowserOp::Key(combo) = &step.op {
                // A key in the page is a key in the browser window.
                return key(combo, &id["window"]);
            }
            id["target"] = match step.target.as_ref() {
                Some(target) => node(frame, target)
                    .map(|n| json!({ "role": n.role, "name": n.name, "context": n.context, "page": page(&n.page) }))
                    .unwrap_or_else(|| target.clone()),
                None => Value::Null,
            };
        }
        Planned::Desktop(effect) => match effect.as_ref() {
            Effect::Key { combo, .. } => return key(combo, &id["window"]),
            Effect::ClickElement { .. } | Effect::ClickPoint { .. } => {
                // A double or right click there is the same click.
                if let Some(id) = id.as_object_mut() {
                    id.remove("button");
                    id.remove("double");
                }
            }
            _ => {}
        },
        Planned::Observe => {}
    }
    id
}

/// A command's folder under its base in one spelling: `.`, `./` and none
/// alike, `a//b/` as `a/b`.
pub(super) fn folder(rel: &str) -> String {
    rel.split('/').filter(|part| !part.is_empty() && *part != ".").collect::<Vec<_>>().join("/")
}

/// Whether two actions are the same: a page ibara could not tell matches
/// any page, and clicks on the screen match within a few pixels.
fn same_action(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else { return false };
    let exact = |k: &str| k != "page" && k != "click_point";
    let rest = |m: &Map<String, Value>| m.keys().filter(|k| exact(k)).count();
    if rest(a) != rest(b) || a.iter().any(|(k, v)| exact(k) && b.get(k) != Some(v)) {
        return false;
    }
    fn known_page(m: &Map<String, Value>) -> Option<&Value> {
        m.get("page").filter(|v| !v.is_null())
    }
    if let (Some(x), Some(y)) = (known_page(a), known_page(b))
        && x != y
    {
        return false;
    }
    let point = |m: &Map<String, Value>| Some((m.get("click_point")?[0].as_f64()?, m.get("click_point")?[1].as_f64()?));
    match (point(a), point(b)) {
        (Some((ax, ay)), Some((bx, by))) => (ax - bx).abs() <= SAME_POINT && (ay - by).abs() <= SAME_POINT,
        _ => a.get("click_point") == b.get("click_point"),
    }
}

/// A key chord in one spelling: `enter`, `Return` and `ctrl + return` alike.
fn keys(combo: &str) -> String {
    cua_keys(combo).map(|k| k.join("+")).unwrap_or_else(|_| combo.trim().to_ascii_lowercase())
}

/// The page element a browser step targets, from the latest observation.
fn node<'a>(frame: Option<&'a FrameState>, target: &Value) -> Option<&'a BrowserNode> {
    frame?.browser.iter().find(|n| n.token == target["token"] && target["capture"].as_str() == Some(n.capture.as_str()))
}

/// A page's address without its query or fragment.
fn page(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// The page `window` shows, when the page reader's focused `tab` is in it:
/// the window has the desktop's focus and is named after the tab, as the
/// browser names its windows. Null otherwise.
fn page_in(window: &WinKey, windows: &[Win], tab: &Tab) -> Value {
    let shown = windows.iter().any(|w| w.focused && w.address == window.address && w.pid == window.pid && !tab.title.is_empty() && w.title.starts_with(&tab.title));
    if shown { json!(page(&tab.url)) } else { Value::Null }
}

/// Effect classes from least to most strict.
const CLASSES: [&str; 4] = ["change", "send", "spend", "destructive"];

/// The stricter of two effect classes.
pub(super) fn stricter_class(a: &'static str, b: &'static str) -> &'static str {
    let rank = |c: &str| CLASSES.iter().position(|x| *x == c).unwrap_or(0);
    if rank(b) > rank(a) { b } else { a }
}

/// A button whose name starts with Send or Submit.
fn send_button(role: &str, name: &str) -> bool {
    let first = name.split_whitespace().next().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase());
    role.contains("button") && matches!(first.as_deref(), Some("send" | "submit"))
}

/// The key a chord ends in, when the page could take it as a press on a
/// form: Return (in a field or on a submit button) or space (on a button).
fn press(combo: &str) -> Option<&'static str> {
    match cua_keys(combo).ok()?.last()?.as_str() {
        "Return" => Some("enter"),
        "space" => Some("space"),
        _ => None,
    }
}

/// `send` when ibara sees the step submit a page's form or press a Send
/// button, whatever the agent declared.
pub(super) fn seen_class(resolved: &Resolved, frame: Option<&FrameState>, focus: &Focus) -> Option<&'static str> {
    let submits = match &resolved.plan {
        Planned::Observe => false,
        Planned::Desktop(effect) => match effect.as_ref() {
            Effect::ClickElement { element, .. } => send_button(&element.role, &element.name),
            Effect::Key { .. } | Effect::Type { .. } => focus.submits,
            _ => false,
        },
        Planned::Browser(step) => {
            let target = step.target.as_ref().and_then(|t| node(frame, t));
            let has = |state: &str| target.is_some_and(|n| n.states.iter().any(|s| s == state));
            match &step.op {
                BrowserOp::Click => has("submits") || target.is_some_and(|n| send_button(&n.role, &n.name)),
                BrowserOp::Type(text) if text.contains(['\n', '\r']) => match step.target {
                    Some(_) => has("enter_submits"),
                    None => focus.submits,
                },
                BrowserOp::Key(_) => focus.submits,
                _ => false,
            }
        }
    };
    submits.then_some("send")
}

impl Controller {
    /// What earlier approvals in the task say about `action`; `op_ref` is the
    /// step's own operation, whose approval is its own business.
    pub(super) fn approval_guard(&self, task_ref: &str, op_ref: &str, action: &Value) -> Result<Guard> {
        if action.is_null() {
            return Ok(Guard::Clear);
        }
        let mut again = None;
        // The newest answer to this step (items come newest first): was it no?
        // An answer still counts once its approval ended with an access change.
        let mut refused = None;
        for item in self.journal.list_task_approvals(task_ref)? {
            let Some(held) = item.operation_ref.as_deref().filter(|r| *r != op_ref) else { continue };
            let Some(op) = self.journal.get_operation_by_ref(held)? else { continue };
            if !op.receipt.get("action").is_some_and(|a| same_action(a, action)) {
                continue;
            }
            if item.state == "open" {
                let att = &item.att_ref;
                return Ok(Guard::Refuse(
                    denied(format!(
                        "This step is already waiting for a person's approval ({att}), asked for by request {}. ibara does not run it under another request, whatever effect this one declares.",
                        op.request_id
                    ))
                    .with("attention", att)
                    .with(
                        "next",
                        format!(
                            "Wait for the answer with computer_wait({{for: {{attention: \"{att}\"}}}}) or check it with computer_status({{ref: \"{att}\"}}). Once approved, repeat request {} as it was.",
                            op.request_id
                        ),
                    ),
                ));
            }
            if let Some(answer) = item.answer.as_deref().filter(|_| refused.is_none()) {
                refused = Some(!super::APPROVED.contains(&answer.trim().to_lowercase().as_str()));
            }
            let class = op.receipt.get("effect_class").and_then(Value::as_str).and_then(|c| CLASSES.into_iter().find(|x| *x == c)).unwrap_or("change");
            again = Some(again.map_or(class, |a| stricter_class(a, class)));
        }
        Ok(again.map_or(Guard::Clear, |class| Guard::AskAgain(class, refused.unwrap_or(false))))
    }

    /// What a key, or typing that ends a line, meets, read now (see [`Focus`]).
    pub(super) async fn focus(&self, resolved: &Resolved, windows: &[Win]) -> Focus {
        let enter = || async {
            let submits = match self.focused_tab().await {
                Some(tab) => self.submits(&tab, "enter").await,
                None => false,
            };
            Focus { submits, ..Focus::default() }
        };
        match &resolved.plan {
            Planned::Browser(step) => match &step.op {
                BrowserOp::Key(combo) => self.key_focus(&step.window, windows, combo).await,
                BrowserOp::Type(text) if step.target.is_none() && text.contains(['\n', '\r']) => enter().await,
                _ => Focus::default(),
            },
            Planned::Desktop(effect) => match effect.as_ref() {
                Effect::Key { surface, combo } if is_browser(&surface.class) => self.key_focus(surface, windows, combo).await,
                Effect::Type { surface, text, .. } if is_browser(&surface.class) && text.contains(['\n', '\r']) => enter().await,
                _ => Focus::default(),
            },
            Planned::Observe => Focus::default(),
        }
    }

    /// What a key meets in a browser `window`: the page it shows, and
    /// whether the press submits a form there.
    async fn key_focus(&self, window: &WinKey, windows: &[Win], combo: &str) -> Focus {
        let Some(tab) = self.focused_tab().await else { return Focus::default() };
        let submits = match press(combo) {
            Some(key) => self.submits(&tab, key).await,
            None => false,
        };
        Focus { page: page_in(window, windows, &tab), submits }
    }

    /// The focused tab, when the page reader is connected.
    async fn focused_tab(&self) -> Option<Tab> {
        if !self.desktop.browser_connected() {
            return None;
        }
        self.desktop.tabs().await.ok()?.into_iter().find(|t| t.focused)
    }

    /// Whether the page reader says `key` (enter or space) pressed now in
    /// `tab` submits a form: never when the page does not have the keyboard
    /// (the address bar does), nor when the reader cannot be asked.
    async fn submits(&self, tab: &Tab, key: &str) -> bool {
        let answer = self.desktop.browser_call("keys", json!({ "tabId": tab.id, "key": key }), false).await;
        answer.is_ok_and(|a| a.get("submits").and_then(Value::as_bool) == Some(true))
    }
}
