//! Windows for a person (the console's Windows tab): the workspaces that hold
//! windows, and the active one, with the windows on each; closing a window;
//! moving one to another workspace without showing it. A window the task
//! holding control opened is that agent's while it works: closing or moving
//! it is refused (`BUSY`, `reason: agent_window`) and nothing is sent.

use super::everyday::number;
use super::ports::Win;
use super::{Controller, squash};
use crate::error::{IbaraError, Result, invalid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// How long a closed window may take to go (an app may ask to save first).
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How long a moved window may take to report its new workspace.
const MOVE_WAIT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(100);
/// A window's title in a timeline summary or an approval.
const TITLE_CHARS: usize = 60;

/// `address` (`0x` and 1 to 16 lowercase hex digits) and `pid` (positive).
fn window_fields(action: &Value) -> Result<(String, i64)> {
    let address = action.get("address").and_then(Value::as_str).unwrap_or("");
    let hex = address.strip_prefix("0x").unwrap_or("");
    if !(1..=16).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(invalid("Expected a window address such as 0x1a2b."));
    }
    let pid = number(action, "pid").filter(|p| *p > 0).ok_or_else(|| invalid("Expected the window's process id."))?;
    Ok((address.to_string(), pid))
}

fn gone() -> IbaraError {
    IbaraError::new("NOT_FOUND", "That window is gone.", true)
}

/// Hyprland not finding the window it was asked about means it is gone.
fn gone_if_stale(error: IbaraError) -> IbaraError {
    if error.code == "STALE_TARGET" { gone() } else { error }
}

/// A special workspace (the scratchpad), by its name alone: a named
/// workspace (`workspace name:hdmi`) has a negative id too but is ordinary.
fn special(name: &str) -> bool {
    name.starts_with("special:")
}

/// Where a workspace is listed: numbered ones by id, then named ones by name,
/// then special ones by name.
fn order(id: i64, name: &str) -> (u8, i64, String) {
    match (special(name), id > 0) {
        (true, _) => (2, 0, name.to_string()),
        (false, true) => (0, id, String::new()),
        (false, false) => (1, 0, name.to_string()),
    }
}

/// Terminal programs by their `/proc/<pid>/comm` (at most 15 characters, so
/// `gnome-terminal-server` is `gnome-terminal-`). A window's class does not
/// tell: Omarchy and agents start terminal apps with their own app ids
/// (`foot -a fleet-btop`, `org.omarchy.btop`).
const TERMINALS: &[&str] =
    &["foot", "kitty", "alacritty", "ghostty", "wezterm-gui", "xterm", "urxvt", "konsole", "gnome-terminal-", "ptyxis", "tilix"];

/// Whether the process behind a window is a terminal; false when unreadable.
fn terminal(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| TERMINALS.contains(&comm.trim()))
}

fn same(w: &Win, address: &str, pid: i64) -> bool {
    w.address == address && w.pid == pid
}

impl Controller {
    /// `windows`: the workspaces holding windows, and the active one even
    /// when empty (numbered, then named, then special), each window with
    /// whether a terminal shows it and the task that opened it, if any; and
    /// the task holding control now.
    pub(super) async fn op_windows(&self) -> Result<Value> {
        let (windows, active) = tokio::join!(self.desktop.windows(), self.desktop.active_workspace());
        let (mut windows, active) = (windows?, active?);
        let running = self.journal.get_active_lease()?.map(|l| l.task_ref);
        let owned = self.journal.owned_windows()?;
        let mut goals: HashMap<String, String> = HashMap::new();
        let mut goal = |task_ref: &str| -> Result<String> {
            if let Some(known) = goals.get(task_ref) {
                return Ok(known.clone());
            }
            let read = self.journal.get_task(task_ref)?.map(|t| squash(&t.goal, 120)).unwrap_or_default();
            goals.insert(task_ref.to_string(), read.clone());
            Ok(read)
        };
        windows.sort_by_key(|w| (w.rect.y, w.rect.x));
        let mut spaces: Vec<(i64, String)> = windows.iter().map(|w| (w.workspace_id, w.workspace.clone())).collect();
        spaces.extend(active.iter().map(|a| (a.id, a.name.clone())));
        spaces.sort_by_cached_key(|(id, name)| (order(*id, name), *id));
        spaces.dedup_by_key(|(id, _)| *id);
        let mut workspaces = Vec::with_capacity(spaces.len());
        for (id, name) in spaces {
            let mut listed = Vec::new();
            for w in windows.iter().filter(|w| w.workspace_id == id) {
                let task = match owned.iter().find(|o| o.address == w.address && o.pid == w.pid) {
                    Some(o) => json!({"task_ref": o.task_ref, "goal": goal(&o.task_ref)?, "running": running.as_deref() == Some(o.task_ref.as_str())}),
                    None => Value::Null,
                };
                listed.push(json!({
                    "address": w.address, "pid": w.pid, "class": w.class, "title": w.title,
                    "floating": w.floating, "fullscreen": w.fullscreen, "focused": w.focused, "terminal": terminal(w.pid), "task": task
                }));
            }
            let is_active = active.as_ref().is_some_and(|a| a.id == id);
            workspaces.push(json!({"id": id, "special": special(&name), "name": name, "active": is_active, "windows": listed}));
        }
        let agent = match &running {
            Some(task_ref) => json!({"task_ref": task_ref, "goal": goal(task_ref)?}),
            None => Value::Null,
        };
        Ok(json!({"workspaces": workspaces, "agent": agent}))
    }

    /// The live window a person named, unless it is gone or the task holding
    /// control opened it.
    async fn window_for_person(&self, address: &str, pid: i64) -> Result<Win> {
        let win = self.desktop.windows().await?.into_iter().find(|w| same(w, address, pid)).ok_or_else(gone)?;
        if let Some(lease) = self.journal.get_active_lease()?
            && self.journal.owned_windows()?.iter().any(|o| o.task_ref == lease.task_ref && o.address == address && o.pid == pid)
        {
            return Err(IbaraError::new("BUSY", "An agent is using this window. Stop its task first.", true)
                .with("reason", "agent_window")
                .with("task_ref", lease.task_ref));
        }
        Ok(win)
    }

    /// `window_close`: ask the window to close, as its close button does, and
    /// wait up to 2 s for it to go (`closed: false` when the app is still
    /// open, perhaps asking to save).
    pub(super) async fn op_window_close(&self, operator_id: &str, action: &Value) -> Result<Value> {
        let (address, pid) = window_fields(action)?;
        let win = self.window_for_person(&address, pid).await?;
        self.desktop.close_window(&win.key()).await.map_err(gone_if_stale)?;
        let deadline = Instant::now() + CLOSE_WAIT;
        let closed = loop {
            if !self.desktop.windows().await?.iter().any(|w| same(w, &address, pid)) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            sleep(POLL).await;
        };
        let title = squash(&win.title, TITLE_CHARS);
        let (kind, summary) = if closed {
            ("window_closed", format!("Closed {} “{title}”", win.class))
        } else {
            ("window_close_asked", format!("Asked {} “{title}” to close; it is still open", win.class))
        };
        self.timeline(kind, None, operator_id, &summary, json!({"address": address, "pid": pid, "class": win.class}));
        Ok(json!({"closed": closed}))
    }

    /// `window_move`: move the window to workspace 1 to 10, leaving the shown
    /// workspace as it is, and wait up to 1 s for it to report that workspace.
    pub(super) async fn op_window_move(&self, operator_id: &str, action: &Value) -> Result<Value> {
        let (address, pid) = window_fields(action)?;
        let workspace = number(action, "workspace").filter(|n| (1..=10).contains(n)).ok_or_else(|| invalid("Choose a workspace from 1 to 10."))?;
        let win = self.window_for_person(&address, pid).await?;
        if win.workspace_id == workspace {
            return Ok(json!({"moved": true, "workspace": workspace}));
        }
        self.desktop.move_window(&win.key(), workspace).await.map_err(gone_if_stale)?;
        let deadline = Instant::now() + MOVE_WAIT;
        loop {
            if self.desktop.windows().await?.iter().any(|w| same(w, &address, pid) && w.workspace_id == workspace) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(IbaraError::new("OUTCOME_UNKNOWN", "The window did not move.", false));
            }
            sleep(POLL).await;
        }
        let summary = format!("Moved {} “{}” to workspace {workspace}", win.class, squash(&win.title, TITLE_CHARS));
        self.timeline("window_moved", None, operator_id, &summary, json!({"address": address, "pid": pid, "class": win.class, "workspace": workspace}));
        Ok(json!({"moved": true, "workspace": workspace}))
    }

    /// Before the access gate of a `window_close` or `window_move`: the named
    /// window's live title, for the words of an approval (the gate writes them
    /// without waiting on the desktop). Nothing when it cannot be read.
    pub(super) async fn note_window_title(&self, action: &Value) {
        let Ok((address, pid)) = window_fields(action) else { return };
        let title = self.desktop.windows().await.ok().and_then(|all| all.into_iter().find(|w| same(w, &address, pid))).map(|w| w.title);
        *self.window_title.borrow_mut() = title.map(|t| (address, t));
    }

    /// The title [`Self::note_window_title`] read for the window at `address`.
    pub(super) fn noted_window_title(&self, address: &str) -> Option<String> {
        self.window_title.borrow().as_ref().filter(|(a, t)| a == address && !t.trim().is_empty()).map(|(_, t)| squash(t, TITLE_CHARS))
    }
}
