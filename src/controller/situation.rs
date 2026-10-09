//! The situation: frames with compact lines and ranked choices, the one-line
//! situation header, and `since` events per session.
//!
//! Frames live in memory (the latest per task) and are also written to the
//! journal as observations, so `computer_status({ref: "frame_…"})` resolves
//! them and a restart expires them.

use super::ports::{Capture, Image, Rect, Tab, Win, WinKey};
use super::{Controller, clip, squash};
use crate::contract::{
    Action, Choice, Frame, KeyAction, LaunchAction, Ref, SurfaceAction, Target, TargetAction, TypeAction, View,
};
use crate::desktop::atspi::{Element, ElementPage};
use crate::desktop::watch::DesktopEvent;
use crate::error::{IbaraError, Result};
use crate::ids::id;
use crate::store::{LeaseRecord, TaskRecord, TaskWindow};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use tokio::sync::broadcast;

pub(crate) const MAX_CHOICES: usize = 20;
const MAX_EVENTS: usize = 256;
const MAX_SESSIONS: usize = 128;
const MAX_FRAMES: usize = 16;
const MAX_TITLES: usize = 256;
const MAX_SINCE: usize = 12;
/// A window event this soon after an effect is attributed to the effect.
pub(crate) const EFFECT_ECHO_MS: i64 = 3000;
const SITUATION_ELEMENTS: u32 = 24;
const SITUATION_WINDOWS: usize = 8;

// ---- sessions -----------------------------------------------------------------

/// One MCP connection's view: who it is and how far it has read events.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub client_name: String,
    pub since: u64,
    pub last_ms: i64,
}

#[derive(Debug, Default)]
pub(crate) struct Sessions {
    map: HashMap<String, Session>,
}

impl Sessions {
    /// The session for `connection_id`, created at the current event head.
    pub fn touch(&mut self, connection_id: &str, client_name: &str, head: u64, now: i64) -> Session {
        if !self.map.contains_key(connection_id) && self.map.len() >= MAX_SESSIONS
            && let Some(oldest) = self.map.iter().min_by_key(|(_, s)| s.last_ms).map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        let session = self.map.entry(connection_id.to_string()).or_insert_with(|| Session {
            client_name: client_name.to_string(),
            since: head,
            last_ms: now,
        });
        session.last_ms = now;
        if !client_name.is_empty() {
            session.client_name = client_name.to_string();
        }
        session.clone()
    }

    /// Advance the session's cursor, returning the previous one.
    pub fn advance(&mut self, connection_id: &str, head: u64) -> Option<u64> {
        self.map.get_mut(connection_id).map(|s| std::mem::replace(&mut s.since, head))
    }

    pub fn remove(&mut self, connection_id: &str) {
        self.map.remove(connection_id);
    }
}

// ---- events -------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Entry {
    seq: u64,
    /// Desktop observations are shown only to the session holding control.
    desktop: bool,
    task_ref: Option<String>,
    text: String,
}

/// A bounded log of meaningful events; sessions read it by sequence number.
#[derive(Debug, Default)]
pub(crate) struct EventLog {
    next: u64,
    items: VecDeque<Entry>,
    titles: HashMap<String, (String, String)>,
    pub last_desktop_ms: i64,
}

impl EventLog {
    pub fn head(&self) -> u64 {
        self.next
    }

    fn push(&mut self, desktop: bool, task_ref: Option<&str>, text: String) {
        self.next += 1;
        if self.items.len() >= MAX_EVENTS {
            self.items.pop_front();
        }
        self.items.push_back(Entry { seq: self.next, desktop, task_ref: task_ref.map(str::to_string), text });
    }

    fn remember_title(&mut self, address: &str, class: &str, title: &str) {
        if self.titles.len() >= MAX_TITLES && !self.titles.contains_key(address) {
            self.titles.clear();
        }
        self.titles.insert(address.to_string(), (class.to_string(), title.to_string()));
    }

    fn name_of(&self, address: &str) -> String {
        match self.titles.get(address) {
            Some((class, title)) if !title.is_empty() => format!("\"{}\"", squash(title, 48)),
            Some((class, _)) => class.clone(),
            None => format!("window {address}"),
        }
    }

    /// Events after `after` visible to a session: controller events for its
    /// task or for everyone, and desktop events only when it holds control.
    pub fn since(&self, after: u64, task_ref: Option<&str>, holds_control: bool) -> Vec<String> {
        let visible: Vec<&Entry> = self
            .items
            .iter()
            .filter(|e| e.seq > after)
            .filter(|e| if e.desktop { holds_control } else { e.task_ref.is_none() || e.task_ref.as_deref() == task_ref })
            .collect();
        let dropped = self.items.front().is_some_and(|e| e.seq > after + 1) && after > 0;
        let mut out: Vec<String> = Vec::new();
        if dropped {
            out.push("earlier events were dropped".into());
        }
        let skip = visible.len().saturating_sub(MAX_SINCE);
        if skip > 0 {
            out.push(format!("{skip} earlier events omitted"));
        }
        for e in visible.into_iter().skip(skip) {
            if out.last() != Some(&e.text) {
                out.push(e.text.clone());
            }
        }
        out
    }
}

/// An unsaved-changes title, as GTK and most editors mark it.
pub(crate) fn dirty_title(title: &str) -> bool {
    let t = title.trim();
    t.starts_with('*') || t.ends_with('*') || t.contains(" *") || t.starts_with('●')
}

// ---- frames -------------------------------------------------------------------

/// A page element in the Chrome extension's latest observation.
#[derive(Debug, Clone)]
pub(crate) struct BrowserNode {
    pub id: String,
    pub role: String,
    pub name: String,
    pub actions: Vec<String>,
    pub states: Vec<String>,
    /// Its containers on the page, as the page reader names them.
    pub context: String,
    /// Bounded, non-password state from this observation (not target identity).
    pub value: Option<String>,
    pub files: Vec<String>,
    pub file_count: usize,
    pub selected_label: Option<String>,
    /// The address of the page it is on.
    pub page: String,
    pub tab_id: i64,
    pub document_id: String,
    pub capture: String,
    pub token: Value,
}

/// The latest observation of one task, with what is needed to execute its choices.
#[derive(Debug, Clone)]
pub(crate) struct FrameState {
    pub frame: Frame,
    pub task_ref: String,
    pub generation: String,
    pub windows: Vec<(String, Win)>,
    /// The window the elements were read from.
    pub surface: Option<WinKey>,
    /// The window each typing or key choice was offered for, by choice id.
    pub input_for: Vec<(String, WinKey)>,
    pub elements: Vec<Element>,
    pub browser: Vec<BrowserNode>,
    continuation: Option<Continuation>,
    /// The latest picture of each surface this task took (newest last):
    /// point coordinates are a picture's pixels, whatever frame came after.
    pub pictures: Vec<Picture>,
}

#[derive(Debug, Clone)]
struct Continuation {
    token: String,
    query: Option<String>,
    surface: Option<WinKey>,
    offset: u32,
    // Reader capture and tab; None means native accessibility pagination.
    browser: Option<(String, i64)>,
}

/// A picture a frame returned, with what is needed to map its pixels to
/// the screen.
#[derive(Debug, Clone)]
pub(crate) struct Picture {
    pub frame_ref: String,
    /// The window it shows as it was then (its place and size), or none
    /// for the whole screen.
    pub window: Option<Win>,
    /// What the frame called it: `w1 mousepad`, or `the screen`.
    pub named: String,
    /// The logical region it shows and its size in pixels.
    pub region: Rect,
    pub width: u32,
    pub height: u32,
}

impl FrameState {
    pub fn window(&self, id: &str) -> Option<&Win> {
        self.windows.iter().find(|(w, _)| w == id).map(|(_, win)| win)
    }
    pub fn element(&self, id: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }
    pub fn choice(&self, id: &str) -> Option<&Choice> {
        self.frame.choices.iter().find(|c| c.choice_id == id)
    }
    /// The window a typing or key choice was offered for.
    pub fn input_for(&self, id: &str) -> Option<&WinKey> {
        self.input_for.iter().find(|(c, _)| c == id).map(|(_, w)| w)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Frames {
    map: HashMap<String, Rc<FrameState>>,
    browser_ids: HashMap<String, HashMap<(String, String), String>>,
    next_browser_id: u64,
}

impl Frames {
    pub fn latest(&self, task_ref: &str) -> Option<Rc<FrameState>> {
        self.map.get(task_ref).cloned()
    }
    pub fn put(&mut self, frame: Rc<FrameState>) {
        if !self.map.contains_key(&frame.task_ref) && self.map.len() >= MAX_FRAMES {
            let oldest = self.map.iter().min_by_key(|(_, f)| f.frame.revision).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.map.remove(&k);
                self.browser_ids.remove(&k);
            }
        }
        self.map.insert(frame.task_ref.clone(), frame);
    }
    pub fn forget(&mut self, task_ref: &str) {
        self.map.remove(task_ref);
        self.browser_ids.remove(task_ref);
    }
    pub fn clear(&mut self) {
        self.map.clear();
        self.browser_ids.clear();
    }
    /// Stable for the same reader-bound element, never recycled for another
    /// element when a query, screenshot or page replaces the current frame.
    fn browser_id(&mut self, task: &str, document: &str, identity: &str) -> String {
        let ids = self.browser_ids.entry(task.to_string()).or_default();
        let key = (document.to_string(), identity.to_string());
        if let Some(id) = ids.get(&key) { return id.clone(); }
        // Bound a long task's cache. Retiring entries never recycles numbers.
        if ids.len() >= 4096 { ids.clear(); }
        self.next_browser_id += 1;
        let id = format!("b{}", self.next_browser_id);
        ids.insert(key, id.clone());
        id
    }
}

/// What to observe.
#[derive(Debug, Clone, Default)]
pub(crate) struct FrameSpec {
    pub view: View,
    pub query: Option<String>,
    pub surface: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

/// Goal words that name an approved app.
fn apps_named_by(goal: &str) -> Vec<&'static str> {
    let goal = goal.to_lowercase();
    let mut apps = Vec::new();
    let has = |words: &[&str]| words.iter().any(|w| goal.contains(w));
    if has(&["editor", "text file", "note", "mousepad", "write a file", "txt"]) {
        apps.push("editor");
    }
    if has(&["terminal", "shell", "command line", "foot"]) {
        apps.push("terminal");
    }
    if has(&["browser", "chrome", "web", "http", "website", "url"]) {
        apps.push("browser");
    }
    if has(&["file manager", "files app", "nautilus"]) {
        apps.push("files");
    }
    apps
}

/// The window class an approved app runs as.
pub(crate) fn app_class(app_id: &str) -> Option<&'static str> {
    match app_id {
        "editor" => Some("mousepad"),
        "terminal" => Some("foot"),
        "browser" => Some("chrom"),
        "files" => Some("nautilus"),
        _ => None,
    }
}

/// Map an agent's app name to an approved app id.
pub(crate) fn app_id_for(name: &str) -> Option<&'static str> {
    match name.trim().to_lowercase().as_str() {
        "editor" | "text editor" | "mousepad" => Some("editor"),
        "terminal" | "foot" | "shell" => Some("terminal"),
        "browser" | "chrome" | "chromium" | "google chrome" | "web browser" => Some("browser"),
        "files" | "file manager" | "file_manager" | "nautilus" => Some("files"),
        _ => None,
    }
}

fn is_enabled(states: &[String]) -> bool {
    states.iter().any(|s| s == "enabled" || s == "sensitive")
}

fn clickable(e: &Element) -> bool {
    e.actions.iter().any(|a| {
        let a = a.to_lowercase();
        a.contains("click") || a.contains("press") || a.contains("activate") || a.contains("toggle") || a.contains("jump")
    })
}

fn editable(e: &Element) -> bool {
    e.states.iter().any(|s| s == "editable")
}

pub(crate) fn element_line(e: &Element) -> String {
    let mut line = format!("{} {} \"{}\"", e.id, e.role, squash(&e.name, 60));
    line.push_str(if is_enabled(&e.states) { " enabled" } else { " disabled" });
    for s in ["focused", "checked", "selected", "editable"] {
        if e.states.iter().any(|x| x == s) {
            line.push(' ');
            line.push_str(s);
        }
    }
    if let Some(v) = &e.value {
        line.push_str(&format!(" = \"{}\"", squash(v, 40)));
    }
    if let Some(ctx) = e.context.last() {
        line.push_str(" · ");
        line.push_str(ctx);
    }
    line
}

fn window_line(id: &str, w: &Win) -> String {
    let mut line = format!("{id} {} \"{}\"", w.class, squash(&w.title, 60));
    line.push_str(&format!(" {}x{} workspace {}", w.rect.width, w.rect.height, w.workspace));
    if w.focused {
        line.push_str(" focused");
    }
    if w.floating {
        line.push_str(" floating");
    }
    line
}

fn host_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    squash(rest, 60)
}

fn element_ref(id: &str) -> Target {
    Target::Element(Ref::new(id).unwrap_or_else(|| Ref::new("e0").expect("valid ref")))
}

/// The dialog in front, if any: a floating focused window, else the context of
/// the elements.
fn dialog_of(windows: &[(String, Win)], elements: &[Element]) -> Option<String> {
    if let Some((id, w)) = windows.iter().find(|(_, w)| w.focused && w.floating) {
        return Some(format!("dialog \"{}\" ({id})", squash(&w.title, 48)));
    }
    elements
        .iter()
        .find_map(|e| e.context.iter().find(|c| c.starts_with("dialog ") || c.starts_with("alert ") || c.starts_with("file chooser ")))
        .cloned()
}

/// Build the ranked choices of a frame, each typing or key choice with the
/// window its input is for: `surface`, the window the elements and
/// shortcuts are of, or the focused one.
pub(crate) fn choices_for(
    windows: &[(String, Win)],
    surface: Option<&Win>,
    elements: &[Element],
    shortcuts: &[(String, String)],
    goal: &str,
    owned: &[TaskWindow],
    disposable: bool,
) -> (Vec<Choice>, Vec<(String, WinKey)>) {
    let mut ranked: Vec<(u8, String, Action, Option<String>, Option<WinKey>)> = Vec::new();
    let focused = windows.iter().find(|(_, w)| w.focused);
    let of_surface = surface.map(Win::key);
    let mut typed = false;
    for e in elements {
        if !is_enabled(&e.states) {
            continue;
        }
        let in_dialog = e.context.iter().any(|c| c.starts_with("dialog ") || c.starts_with("alert ") || c.starts_with("file chooser "));
        let ctx = e.context.last().map(|c| format!(" · {c}")).unwrap_or_default();
        if editable(e) && e.states.iter().any(|s| s == "focused") && !typed {
            typed = true;
            ranked.push((0, format!("type into {} \"{}\"{ctx}", e.role, squash(&e.name, 40)), Action::Type(TypeAction { text: String::new() }), Some("text".into()), of_surface.clone()));
            continue;
        }
        if clickable(e) {
            let rank = if in_dialog { 1 } else { 3 };
            ranked.push((rank, format!("click {} {} \"{}\"{ctx}", e.id, e.role, squash(&e.name, 40)), Action::Click(TargetAction { target: element_ref(&e.id) }), None, None));
        } else if editable(e) {
            let rank = if in_dialog { 1 } else { 3 };
            ranked.push((rank, format!("focus {} {} \"{}\"{ctx}", e.id, e.role, squash(&e.name, 40)), Action::Click(TargetAction { target: element_ref(&e.id) }), None, None));
        }
    }
    // Keys go wherever the window's keyboard focus is; when that is a button
    // or another control that takes no text, typing would press it.
    let focus_on_control = elements.iter().any(|e| e.states.iter().any(|s| s == "focused") && !editable(e));
    if let Some((id, w)) = focused
        && !typed
        && !focus_on_control
    {
        ranked.push((2, format!("type into {id} {} (focused)", w.class), Action::Type(TypeAction { text: String::new() }), Some("text".into()), Some(w.key())));
    }
    for (keys, label) in shortcuts {
        ranked.push((4, format!("key {keys} · {label}"), Action::Key(KeyAction { keys: keys.clone() }), None, of_surface.clone()));
    }
    // Another app window that someone else left open is not this task's: the
    // launch is offered until the task has a window of the app itself.
    for app in apps_named_by(goal) {
        let running = app_class(app).is_some_and(|class| {
            windows.iter().any(|(_, w)| w.class.to_lowercase().contains(class) && owned.iter().any(|o| o.address == w.address))
        });
        if !running {
            ranked.push((5, format!("launch {app}"), Action::Launch(LaunchAction { app: app.into() }), None, None));
        }
    }
    for (id, w) in windows.iter().filter(|(_, w)| !w.focused).take(5) {
        ranked.push((6, format!("focus {id} {} \"{}\"", w.class, squash(&w.title, 40)), Action::Focus(SurfaceAction { surface: id.clone() }), None, None));
    }
    for (id, w) in windows {
        if disposable || owned.iter().any(|o| o.address == w.address) {
            ranked.push((if disposable { 2 } else { 7 }, format!("close {id} {} ({})", w.class, if disposable { "disposable desktop" } else { "this task opened it" }), Action::Close(SurfaceAction { surface: id.clone() }), None, None));
        }
    }
    ranked.sort_by_key(|(rank, ..)| *rank);
    let mut input_for = Vec::new();
    let choices = ranked
        .into_iter()
        .take(MAX_CHOICES)
        .enumerate()
        .map(|(i, (_, label, action, param, window))| {
            let choice_id = format!("c{}", i + 1);
            if let Some(window) = window {
                input_for.push((choice_id.clone(), window));
            }
            Choice { choice_id, label, action, param }
        })
        .collect();
    (choices, input_for)
}

impl Controller {
    /// Record a controller event (control changes, answers) for `since`.
    pub(crate) fn push_event(&self, task_ref: Option<&str>, text: &str) {
        self.events.borrow_mut().push(false, task_ref, text.to_string());
    }

    /// Log a failed write of which windows are a task's; the event goes on.
    pub(crate) fn record_window(&self, written: Result<()>) {
        if let Err(e) = written {
            super::log_event("task_windows_write_failed", &e.to_string());
        }
    }

    /// Fold one desktop event into the log, ownership and touch tracking.
    pub(crate) fn on_desktop_event(&self, event: DesktopEvent) {
        let now = self.now_ms();
        let quiet = self.effect_task.borrow().is_none() && now - self.last_effect_end_ms.get() > EFFECT_ECHO_MS;
        let mut events = self.events.borrow_mut();
        events.last_desktop_ms = now;
        let text = match event {
            DesktopEvent::WindowOpened { address, class, title, .. } => {
                if (self.disposable_desktop)() && super::windows::is_browser(&class) {
                    self.record_window(self.journal.mark_browser_window_opened(&address));
                }
                if (self.disposable_desktop)() && self.journal.get_active_lease().ok().flatten().is_none() {
                    self.record_window(self.journal.put_desktop_reset(&serde_json::json!({"state":"resetting","reason":"a window opened while idle","at":self.now_iso()})));
                    self.record_window(self.journal.set_control(crate::store::ControlPatch { unsettled:Some(true), ..Default::default() }).map(|_| ()));
                }
                events.remember_title(&address, &class, &title);
                format!("window \"{}\" opened ({class})", squash(&title, 48))
            }
            DesktopEvent::WindowClosed { address } => {
                let name = events.name_of(&address);
                events.titles.remove(&address);
                self.record_window(self.journal.window_closed(&address));
                format!("window {name} closed")
            }
            DesktopEvent::WindowTitle { address, title } => {
                let before = events.name_of(&address);
                let class = events.titles.get(&address).map(|(c, _)| c.clone()).unwrap_or_default();
                events.remember_title(&address, &class, &title);
                self.record_window(self.journal.retitle_window(&address, &title));
                if quiet {
                    self.record_window(self.journal.touch_window(&address));
                }
                format!("window {before} is now \"{}\"", squash(&title, 48))
            }
            DesktopEvent::Focus { address } => match address {
                Some(address) => {
                    if quiet {
                        self.record_window(self.journal.touch_window(&address));
                    }
                    format!("focus moved to {}", events.name_of(&address))
                }
                None => "focus left all windows".to_string(),
            },
            DesktopEvent::MonitorAdded { name } => format!("display {name} added"),
            DesktopEvent::MonitorRemoved { name } => format!("display {name} removed"),
            DesktopEvent::DisplayChanged => {
                drop(events);
                self.display_changed();
                return;
            }
            DesktopEvent::Resync => "desktop events restarted; some changes may be missing".to_string(),
        };
        events.push(true, None, text);
    }

    /// Start reconciling outputs, unless a loop already runs.
    fn display_changed(&self) {
        if self.display_loop.replace(true) {
            return;
        }
        let me = self.me.borrow().clone();
        if me.upgrade().is_none() {
            self.display_loop.set(false);
            return;
        }
        tokio::task::spawn_local(reconcile_display_loop(me));
    }

    /// The situation line (about 200 characters).
    pub(crate) fn situation_line(&self, principal: &str, connection_id: &str, agent: &str, task: Option<&TaskRecord>) -> String {
        let mut parts: Vec<String> = vec![squash(&self.computer_name(), 32)];
        let control = self.journal.get_control().ok();
        let lease = self.journal.get_active_lease().ok().flatten();
        let viewer_owner = self.viewer_state.borrow().owner.is_some();
        let holder = if !self.desktop.session_available() || control.as_ref().is_some_and(|c| !c.session_hint) {
            "desktop unavailable".to_string()
        } else if let Some(wait) = control.as_ref().and_then(|c| self.system_wait(c)) {
            wait.holder().to_string()
        } else if control.as_ref().is_some_and(|c| c.unsettled) {
            "control unsettled".to_string()
        } else if viewer_owner {
            "a person controls".to_string()
        } else if control.as_ref().is_some_and(|c| c.human_control || c.paused) {
            "paused for a person".to_string()
        } else {
            match &lease {
                Some(l) if self.holds_lease(l, principal, connection_id, agent) => format!("you ({agent}) control"),
                Some(_) => "another agent controls".to_string(),
                None => "nobody controls".to_string(),
            }
        };
        parts.push(holder);
        if let Some(task) = task {
            let (met, total) = checks_progress(task);
            parts.push(format!("task \"{}\" {met}/{total} checks", squash(&task.goal, 40)));
            parts.push(format!("{} unknown", self.unknown_operations(&task.task_ref).len()));
            parts.push(format!("{} attention", self.journal.count_open_attention(Some(&task.task_ref)).unwrap_or(0)));
        } else {
            parts.push(format!("{} attention", self.journal.count_open_attention(None).unwrap_or(0)));
        }
        if let Some(pressure) = self.desktop.memory_pressure() {
            parts.push(squash(&pressure, 40));
        }
        let revision = task.and_then(|t| self.frames.borrow().latest(&t.task_ref)).map_or(self.revision.get(), |f| f.frame.revision);
        parts.push(format!("r{revision}"));
        clip(&parts.join(" · "), 240)
    }

    /// Events for this session since its previous response.
    pub(crate) fn take_since(&self, connection_id: &str, task_ref: Option<&str>, holds_control: bool) -> Vec<String> {
        let head = self.events.borrow().head();
        let Some(after) = self.sessions.borrow_mut().advance(connection_id, head) else {
            return Vec::new();
        };
        self.events.borrow().since(after, task_ref, holds_control)
    }

    /// Resolve a surface name: a window id of the latest frame (`w2`), else a
    /// unique class or title match among the live windows.
    pub(crate) fn resolve_surface(&self, frame: Option<&FrameState>, windows: &[Win], name: &str) -> Result<Win> {
        if let Some(frame) = frame
            && let Some(win) = frame.window(name)
        {
            return windows
                .iter()
                .find(|w| w.address == win.address && w.pid == win.pid)
                .cloned()
                .ok_or_else(|| IbaraError::new("STALE_TARGET", format!("surface {name} is gone; observe again"), true));
        }
        let lower = name.to_lowercase();
        let by_class: Vec<&Win> = windows.iter().filter(|w| w.class.to_lowercase() == lower).collect();
        let matches: Vec<&Win> = if by_class.is_empty() {
            windows.iter().filter(|w| w.title.to_lowercase().contains(&lower)).collect()
        } else {
            by_class
        };
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(IbaraError::new("STALE_TARGET", format!("surface '{}' is not on screen; observe again", clip(name, 60)), true)),
            many => Err(IbaraError::new("AMBIGUOUS_TARGET", format!("surface '{}' matches {} windows; name one by its w id", clip(name, 60), many.len()), true)
                .with("candidates", many.iter().map(|w| format!("{} \"{}\"", w.class, squash(&w.title, 40))).collect::<Vec<_>>())),
        }
    }

    /// Observe and store a frame for `task` (`lease` binds its choices).
    pub(crate) async fn build_frame(&self, task: &TaskRecord, lease: &LeaseRecord, spec: &FrameSpec) -> Result<(Rc<FrameState>, Option<Image>)> {
        let observed_at = self.now_ms();
        let previous = self.frames.borrow().latest(&task.task_ref);
        let windows = self.desktop.windows().await?;
        let mut ordered: Vec<Win> = windows.clone();
        ordered.sort_by_key(|w| (!w.focused, !w.floating));
        let listed: Vec<(String, Win)> = ordered.iter().enumerate().map(|(i, w)| (format!("w{}", i + 1), w.clone())).collect();
        let target = match spec.surface.as_deref() {
            Some(name) if !is_browser_surface(name) => {
                let frame_like = FrameState {
                    windows: listed.clone(),
                    ..previous.as_deref().cloned().unwrap_or_else(|| empty_frame(task, lease))
                };
                Some(self.resolve_surface(Some(&frame_like), &windows, name)?)
            }
            _ => ordered.iter().find(|w| w.focused).cloned(),
        };
        let tabs = if self.desktop.browser_connected() { self.desktop.tabs().await.unwrap_or_default() } else { Vec::new() };
        let explicit_page = spec.surface.as_deref().is_some_and(is_browser_surface) && spec.view == View::Elements;
        // Automatic semantics follow the focused surface. A Chromium-owned
        // file dialog is still native: require a normal window matching the
        // focused tab before asking the page reader. A named browser picture
        // carries the same fresh page controls; named native inspection stays native.
        let follows_page = spec.surface.is_none()
            || spec.surface.as_deref().is_some_and(is_browser_surface)
            || matches!(spec.view, View::Image | View::Screen);
        let automatic_browser = follows_page && matches!(spec.view, View::Situation | View::Elements | View::Image | View::Screen) && target.as_ref().is_some_and(|w| {
            w.focused && !w.floating && super::agent::is_browser(&w.class)
        });
        let automatic_page = automatic_browser && target.as_ref().is_some_and(|w| {
            tabs.iter().any(|t| t.focused && !t.title.is_empty() && w.title.contains(&t.title))
        });
        let mut wants_elements = matches!(spec.view, View::Situation | View::Elements) && !explicit_page;
        let limit = match spec.view {
            View::Elements | View::Image | View::Screen => spec.limit.unwrap_or(40).min(100),
            _ => spec.limit.unwrap_or(SITUATION_ELEMENTS).min(100),
        };
        let continuation = match spec.cursor.as_deref() {
            None => None,
            Some(token) => Some(previous.as_ref().filter(|p|p.generation==lease.generation)
                .and_then(|p|p.continuation.clone())
                .filter(|c|c.token==token && c.query==spec.query && c.surface==target.as_ref().map(Win::key)
                    && c.browser.is_some()==(explicit_page || automatic_page))
                .ok_or_else(||IbaraError::new("STALE_TARGET","Observation cursor is stale or belongs to a different query/surface; observe without cursor.",true))?),
        };
        let cursor = continuation.as_ref().filter(|c|c.browser.is_none()).map(|c|c.offset);
        let mut page_note = (automatic_browser && !automatic_page).then(||
            "page controls unavailable (focused tab not verified); showing the native surface; use a fresh image if needed".to_string());
        let (browser, browser_total, browser_context, modal_background, browser_next) = if explicit_page || automatic_page {
            match self.observe_browser(&task.task_ref, &tabs, spec.query.as_deref(), limit, continuation.as_ref()).await {
                Ok(nodes) => { wants_elements = false; nodes }
                Err(e) if spec.cursor.is_none() && automatic_page && !matches!(e.code, "SESSION_UNAVAILABLE" | "HUMAN_CONTROL") => {
                    page_note = Some(format!("page controls unavailable ({}); showing the native surface; use a fresh image if needed", e.code));
                    (Vec::new(), 0, None, None, None)
                }
                Err(e) => return Err(e),
            }
        } else { (Vec::new(), 0, None, None, None) };
        let page: ElementPage = match (&target, wants_elements) {
            (Some(win), true) => match self.desktop.elements(&win.key(), spec.query.as_deref(), limit, cursor).await {
                Ok(page) => page,
                Err(e) if spec.cursor.is_some() || e.code == "SESSION_UNAVAILABLE" || e.code == "HUMAN_CONTROL" => return Err(e),
                Err(_) => ElementPage::default(),
            },
            _ => ElementPage::default(),
        };
        let focused_app = target.as_ref().map(|w| w.class.to_lowercase()).unwrap_or_default();
        let notes = if focused_app.is_empty() { Vec::new() } else { self.journal.list_app_notes(&focused_app, None, None, 12).unwrap_or_default() };
        let image = match spec.view {
            View::Image => {
                let what = match &target {
                    Some(w) => Capture::Surface(w.key()),
                    None => Capture::Screen,
                };
                Some(self.desktop.capture(&what, 256 * 1024).await?)
            }
            View::Screen => Some(self.desktop.capture(&Capture::Screen, 1024 * 1024).await?),
            _ => None,
        };

        let mut lines: Vec<String> = Vec::new();
        if continuation.is_some() {
            lines.push("continued observation snapshot; omit cursor to refresh current state".into());
        }
        let windows_shown = if spec.view == View::Elements { 3 } else { SITUATION_WINDOWS };
        for (id, w) in listed.iter().take(windows_shown) {
            lines.push(window_line(id, w));
        }
        if listed.len() > windows_shown {
            lines.push(format!("… {} more windows", listed.len() - windows_shown));
        }
        if let Some(dialog) = dialog_of(&listed, &page.elements) {
            lines.push(dialog);
        }
        let target_id = target.as_ref().and_then(|t| listed.iter().find(|(_, w)| w.address == t.address).map(|(id, _)| id.clone()));
        if wants_elements && let Some(tid) = &target_id {
            if !page.available {
                lines.push(format!("{tid} exposes no accessibility tree; view \"image\" shows it"));
            }
            for e in &page.elements {
                lines.push(element_line(e));
            }
        }
        for (i, tab) in tabs.iter().take(5).enumerate() {
            lines.push(format!("t{} tab \"{}\" {}{}", i + 1, squash(&tab.title, 48), host_path(&tab.url), if tab.focused { " focused" } else { "" }));
        }
        let mut last_context = "";
        for node in &browser {
            let mut line = format!("{} {} \"{}\"", node.id, node.role, squash(&node.name, 160));
            for state in ["disabled", "readonly", "checked", "unchecked", "selected", "expanded", "collapsed", "invalid", "required", "value_hidden"] {
                if node.states.iter().any(|s| s == state) { line.push(' '); line.push_str(state); }
            }
            if !node.actions.is_empty() && node.states.iter().any(|s| s == "enabled") { line.push_str(" enabled"); }
            if let Some(value) = &node.value { line.push_str(&format!(" = {}", serde_json::to_string(value).unwrap_or_default())); }
            if let Some(label) = &node.selected_label { line.push_str(&format!(" · selected {}", serde_json::to_string(label).unwrap_or_default())); }
            if node.role == "file_input" {
                if node.actions.iter().any(|a| a == "click") { line.push_str(" · click opens native file chooser"); }
                if !node.files.is_empty() { line.push_str(&format!(" · selected files: {} (not proof of upload)", node.files.join(", "))); }
                if node.file_count > node.files.len() { line.push_str(&format!(" · {} more selected files not shown", node.file_count - node.files.len())); }
            }
            if matches!(node.role.as_str(), "date_input" | "datetime-local_input" | "month_input" | "time_input" | "week_input") {
                line.push_str(" · native picker: click; computer_act type enters text in the displayed format, key sends one key/chord; observe value");
            }
            if !node.context.is_empty() && node.context != last_context { line.push_str(&format!(" · {}", squash(&node.context, 140))); }
            last_context = &node.context;
            lines.push(line);
        }
        if let Some((text, truncated)) = browser_context {
            lines.push(format!("page text{}: {}", if truncated { " (excerpt truncated; query or view image for more)" } else { "" }, serde_json::to_string(&text).unwrap_or_default()));
        }
        if browser_total > browser.len() {
            let start=continuation.as_ref().map_or(0,|c|c.offset as usize);
            lines.push(format!("page controls: {}–{} of {browser_total}; {}",start+1,start+browser.len(),
                if browser_next.is_some() {"continue with next_cursor and the same query/surface, or start a new elements query"}
                else if start+browser.len()<browser_total {"reader supplied no continuation; narrow the elements query or view an image"}
                else {"end of snapshot; start a new elements query or omit cursor to refresh"}));
        }
        if let Some(omitted) = modal_background {
            lines.push(format!("page controls: modal scope; {omitted} background controls omitted; use an elements query to inspect the full page"));
        }
        if let Some(note) = page_note { lines.push(note); }
        let signature = listed.iter().find(|(_, w)| w.focused).map(|(_, w)| surface_signature(w, &page.elements));
        if wants_elements && let Some(win) = &target {
            let signature = surface_signature(win, &page.elements);
            let text = if page.available { "has an accessibility tree" } else { "has no accessibility tree; view \"image\" shows it" };
            let fact = json!({ "text": text, "available": page.available });
            let version = self.desktop.app_version(win.pid).unwrap_or_default();
            if let Err(e) = self.journal.record_app_note(&win.class.to_lowercase(), &version, &signature, "semantics", &fact, &self.now_iso()) {
                super::log_event("app_note_failed", &e.to_string());
            }
        }
        let mut shortcuts: Vec<(String, String)> = Vec::new();
        let mut note_lines = 0;
        for note in &notes {
            if let Some(line) = note_line(note, signature.as_deref())
                && note_lines < 3
            {
                lines.push(line);
                note_lines += 1;
            }
            if note.fact_kind == "shortcut"
                && let (Some(keys), Some(effect)) = (note.fact.get("keys").and_then(Value::as_str), note.fact.get("opens").and_then(Value::as_str))
            {
                shortcuts.push((keys.to_string(), format!("opened {effect} before")));
            }
        }
        shortcuts.truncate(3);
        let owned = self.journal.task_windows(&task.task_ref)?;
        let (choices, input_for) = choices_for(&listed, target.as_ref(), if wants_elements { &page.elements } else { &[] }, &shortcuts, &task.goal, &owned, (self.disposable_desktop)());

        let revision = self.next_revision();
        let frame_ref = id("frame");
        let continuation = browser_next.map(|(capture,tab,offset)|Continuation {
            token:format!("page_{revision}_{offset}"), query:spec.query.clone(),surface:target.as_ref().map(Win::key),offset,browser:Some((capture,tab)),
        }).or_else(||page.next_cursor.map(|offset|Continuation {
            token:format!("page_{revision}_{offset}"), query:spec.query.clone(),surface:target.as_ref().map(Win::key),offset,browser:None,
        }));
        let captured_at = self.now_iso();
        let covered = match spec.view {
            View::Situation | View::Elements => format!(
                "{} of {} windows; {} native elements{}{}; {} page controls",
                spec.view.as_str(),
                listed.len(),
                page.elements.len(),
                target_id.as_deref().map(|t| format!(" in {t}")).unwrap_or_default(),
                page.total.map(|t| format!(" of {t}")).unwrap_or_default(),
                browser.len()
            ),
            View::Image => format!("image of {}", target_id.as_deref().unwrap_or("the screen")),
            View::Screen => "full screen image".to_string(),
        };
        let next_richer = match spec.view {
            View::Situation => Some(View::Elements),
            View::Elements => Some(View::Image),
            View::Image => Some(View::Screen),
            View::Screen => None,
        };
        let cost = lines.iter().map(|l| l.len() + 4).sum::<usize>() + image.as_ref().map_or(0, |i| i.bytes.len());
        let frame = Frame {
            frame_ref: frame_ref.clone(),
            revision,
            captured_at: captured_at.clone(),
            covered,
            cost_bytes: cost as u64,
            lines,
            choices,
            next_richer,
            next_cursor: continuation.as_ref().map(|c|c.token.clone()),
        };
        // Pictures of this lease carry over; a new one replaces its surface's.
        let mut pictures = previous.as_deref().filter(|p| p.generation == lease.generation).map(|p| p.pictures.clone()).unwrap_or_default();
        if let Some(image) = &image {
            let window = if spec.view == View::Image { target.clone() } else { None };
            let named = match (&window, &target_id) {
                (Some(w), Some(tid)) => format!("{tid} {}", w.class),
                (Some(w), None) => w.class.clone(),
                (None, _) => "the screen".to_string(),
            };
            let same = |p: &Picture| match (&p.window, &window) {
                (Some(a), Some(b)) => a.address == b.address && a.pid == b.pid,
                (a, b) => a.is_none() && b.is_none(),
            };
            pictures.retain(|p| !same(p));
            pictures.push(Picture { frame_ref: frame_ref.clone(), window, named, region: image.region, width: image.width, height: image.height });
        }
        let state = Rc::new(FrameState {
            frame,
            task_ref: task.task_ref.clone(),
            generation: lease.generation.clone(),
            windows: listed,
            surface: target.as_ref().map(Win::key),
            input_for,
            elements: page.elements,
            browser,
            continuation,
            pictures,
        });
        let record = json!({
            "kind": "frame",
            "observation_ref": frame_ref,
            "frame_ref": frame_ref,
            "task_ref": task.task_ref,
            "epoch": self.epoch,
            "lease_generation": lease.generation,
            "captured_at": captured_at,
            "revision": revision,
            "covered": state.frame.covered,
            "lines": state.frame.lines,
            "choices": state.frame.choices,
        });
        self.journal.put_observation(&task.task_ref, &lease.principal, &record, false)?;
        self.frames.borrow_mut().put(state.clone());
        self.turns.borrow_mut().observed_since(observed_at);
        Ok((state, image))
    }

    /// Page elements of the focused Chrome tab through the extension.
    async fn observe_browser(&self, task_ref: &str, tabs: &[Tab], query: Option<&str>, limit: u32, continuation: Option<&Continuation>) -> Result<(Vec<BrowserNode>, usize, Option<(String, bool)>, Option<u64>, Option<(String,i64,u32)>)> {
        let Some(tab) = tabs.iter().find(|t| t.focused).or(tabs.first()) else {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "No Chrome tab is available through the extension.", true)
                .with("next", "Use computer_act on the browser window, or view \"image\"."));
        };
        let mut args=json!({ "tabId": tab.id, "query": query.unwrap_or(""), "view": "elements", "limit": limit, "maxChars": 12000 });
        if let Some(c)=continuation && let Some((capture,tab_id))=&c.browser {
            if *tab_id!=tab.id {return Err(IbaraError::new("STALE_TARGET","The tab changed; observe without cursor.",true));}
            args["capture"]=json!(capture);args["offset"]=json!(c.offset);
        }
        let data = self
            .desktop
            .browser_call("observe", args, false)
            .await?;
        if data.get("refused").and_then(Value::as_bool) == Some(true) {
            return Err(IbaraError::new("STALE_TARGET", "The Chrome tab is no longer visible or focused.", true));
        }
        let (Some(nodes), Some(document_id), Some(capture)) =
            (data.get("nodes").and_then(Value::as_array), data.get("documentId").and_then(Value::as_str), data.get("capture").and_then(Value::as_str))
        else {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Incomplete Chrome observation.", true));
        };
        let strings = |v: Option<&Value>| -> Vec<String> {
            v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).take(12).collect()).unwrap_or_default()
        };
        let total = data.get("count").and_then(Value::as_u64).unwrap_or(nodes.len() as u64) as usize;
        let nodes = nodes
            .iter()
            .take(limit as usize)
            .enumerate()
            .map(|(i, node)| BrowserNode {
                id: self.frames.borrow_mut().browser_id(task_ref, document_id,
                    &node.get("elementId").and_then(Value::as_str).map(str::to_string)
                        .unwrap_or_else(|| format!("{capture}:{}:{i}", node.get("token").unwrap_or(&Value::Null)))),
                role: clip(node.get("role").and_then(Value::as_str).unwrap_or("node"), 80),
                name: clip(node.get("name").and_then(Value::as_str).unwrap_or(""), 400),
                actions: strings(node.get("actions")),
                states: strings(node.get("states")),
                context: clip(node.get("ancestor").and_then(Value::as_str).unwrap_or(""), 500),
                value: if node.get("states").and_then(Value::as_array).is_some_and(|states| states.iter().any(|s| matches!(s.as_str(), Some("password" | "value_hidden")))) || node.get("role").and_then(Value::as_str) == Some("password") {
                    None
                } else { node.get("value").and_then(Value::as_str).map(|s| format!("{}{}", clip(s, 240), if node.get("value_truncated").and_then(Value::as_bool) == Some(true) { "… [truncated]" } else { "" })) },
                files: node.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(8).map(|s| clip(s, 180)).collect()).unwrap_or_default(),
                file_count: node.get("file_count").and_then(Value::as_u64).unwrap_or(0) as usize,
                selected_label: node.get("selected_label").and_then(Value::as_str).map(|s| clip(s, 260)),
                page: clip(data.get("url").and_then(Value::as_str).unwrap_or(""), 4096),
                tab_id: tab.id,
                document_id: document_id.to_string(),
                capture: capture.to_string(),
                token: node.get("token").cloned().unwrap_or(Value::Null),
            })
            .collect();
        let context = data.get("contextText").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(|text| {
            (text.chars().take(800).collect(), text.chars().count() > 800 || data.get("contextTruncated").and_then(Value::as_bool) == Some(true))
        });
        let modal_background = (data.get("modalScope").and_then(Value::as_bool) == Some(true))
            .then(|| data.get("omittedBackground").and_then(Value::as_u64).unwrap_or(0));
        let next=data.get("next_offset").and_then(Value::as_u64).filter(|n|*n<total as u64 && *n>continuation.map_or(0,|c|c.offset as u64))
            .and_then(|n|u32::try_from(n).ok()).map(|offset|(capture.to_string(),tab.id,offset));
        Ok((nodes, total, context, modal_background,next))
    }

    /// Operations of a task whose outcome is unknown and not reconciled.
    pub(crate) fn unknown_operations(&self, task_ref: &str) -> Vec<crate::store::OperationRecord> {
        self.journal
            .unresolved_operations(task_ref)
            .unwrap_or_default()
            .into_iter()
            .filter(|op| op.receipt.get("execution").and_then(Value::as_str) == Some("unknown"))
            .filter(|op| self.operation_needs_resolution(op, Some(task_ref)))
            .collect()
    }
}

fn empty_frame(task: &TaskRecord, lease: &LeaseRecord) -> FrameState {
    FrameState {
        frame: Frame {
            frame_ref: String::new(),
            revision: 0,
            captured_at: String::new(),
            covered: String::new(),
            cost_bytes: 0,
            lines: Vec::new(),
            choices: Vec::new(),
            next_richer: None,
            next_cursor: None,
        },
        task_ref: task.task_ref.clone(),
        generation: lease.generation.clone(),
        windows: Vec::new(),
        surface: None,
        input_for: Vec::new(),
        elements: Vec::new(),
        browser: Vec::new(),
        continuation: None,
        pictures: Vec::new(),
    }
}

/// `tab`, `t1`, `browser`, `chrome`: the page, not the browser window.
pub(crate) fn is_browser_surface(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "browser" || lower == "chrome" || lower == "tab" || (lower.starts_with('t') && lower.len() > 1 && lower[1..].bytes().all(|b| b.is_ascii_digit()))
}

/// The surface signature app notes key on: the dialog in front, else the app.
pub(crate) fn surface_signature(win: &Win, elements: &[Element]) -> String {
    if win.floating {
        return format!("dialog \"{}\"", squash(&win.title, 48));
    }
    if let Some(dialog) = elements.iter().find_map(|e| e.context.iter().find(|c| c.starts_with("dialog ")).cloned()) {
        return dialog;
    }
    "main window".to_string()
}

/// A note worth its bytes in a frame: a route that was refused on the surface
/// in front for a reason that describes the app (`about: app`); refusals
/// recorded before that rule, about one element or one step, stay unshown.
/// Shortcuts become choices; semantics are in the frame's own lines.
fn note_line(note: &crate::store::AppNote, signature: Option<&str>) -> Option<String> {
    if signature.is_some_and(|s| s != note.surface_signature) && note.surface_signature != "main window" {
        return None;
    }
    let positive = note.fact.get("worked") == Some(&Value::Bool(true)) || note.fact.get("available") == Some(&Value::Bool(true));
    if positive || note.fact_kind == "shortcut" || note.fact_kind == "semantics" || note.fact.get("about").and_then(Value::as_str) != Some("app") {
        return None;
    }
    let text = note.fact.get("text").and_then(Value::as_str)?;
    let app = if note.version.is_empty() { note.app.clone() } else { format!("{} {}", note.app, note.version) };
    Some(format!("note: {app} {}: {}", note.surface_signature, squash(text, 80)))
}

/// `met/total` of a task's checks, from the last evaluation recorded on it.
pub(crate) fn checks_progress(task: &TaskRecord) -> (usize, usize) {
    let total = task.success_criteria.len();
    let met = task.success_criteria.iter().filter(|c| c.get("state").and_then(Value::as_str) == Some("met")).count();
    (met, total)
}

/// Feed desktop events into the controller until the watcher stops.
pub(crate) async fn pump_events(me: Weak<Controller>, mut rx: broadcast::Receiver<DesktopEvent>) {
    loop {
        let event = rx.recv().await;
        let Some(controller) = me.upgrade() else { return };
        match event {
            Ok(event) => controller.on_desktop_event(event),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                controller.events.borrow_mut().push(true, None, format!("{n} desktop events were missed"));
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Reconcile outputs after a display change, retrying every 5 s while the
/// controller defers it (the output watcher's loop).
async fn reconcile_display_loop(me: Weak<Controller>) {
    loop {
        let Some(controller) = me.upgrade() else { return };
        if controller.closed.get() {
            controller.display_loop.set(false);
            return;
        }
        let deferred = match controller.admin(json!({ "op": "reconcile_display" })).await {
            Ok(v) => v.get("deferred") == Some(&Value::Bool(true)) && v.get("reason").is_none(),
            Err(e) => {
                super::log_event("reconcile_display_failed", &e.to_string());
                true
            }
        };
        if !deferred {
            controller.display_loop.set(false);
            return;
        }
        drop(controller);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}
