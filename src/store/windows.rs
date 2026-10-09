//! The windows each task opened (core schema 6). They are kept in the journal,
//! not in memory, because the apps behind them outlive ibarad (they run in
//! desktop scopes of their own): after ibara restarts, finishing the task
//! still closes its own windows, and still leaves one a person used.

use super::clip;
use super::journal::Journal;
use crate::error::Result;
use rusqlite::{Row, Transaction, TransactionBehavior, params};

/// Windows remembered across all tasks; the oldest go first.
pub const MAX_TASK_WINDOWS: i64 = 64;
const MAX_TEXT_CHARS: usize = 256;

/// A window a task opened, while it lives: its Hyprland address and pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWindow {
    pub task_ref: String,
    pub address: String,
    pub pid: i64,
    pub class: String,
    pub title: String,
    pub process_start_ticks: Option<u64>,
    pub compositor_instance: String,
    /// A person used it: it took focus or changed its title while no agent
    /// step ran.
    pub touched: bool,
}

fn window_from_row(r: &Row<'_>) -> rusqlite::Result<TaskWindow> {
    Ok(TaskWindow {
        task_ref: r.get("task_ref")?,
        address: r.get("address")?,
        pid: r.get("pid")?,
        class: r.get("class")?,
        title: r.get("title")?,
        process_start_ticks: r.get("process_start_ticks")?,
        compositor_instance: r.get("compositor_instance")?,
        touched: r.get::<_, i64>("touched")? != 0,
    })
}

impl Journal {
    /// Durable across daemon/browser crashes; cleared only after native retirement and zero inventory.
    pub fn browser_retirement_pending(&self) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let value: Option<String> = self.db().query_row("SELECT value FROM meta WHERE key='browser_retirement_pending'", [], |r| r.get(0)).optional()?;
        Ok(value.as_deref() == Some("true"))
    }

    pub fn browser_window_generation(&self) -> Result<i64> {
        use rusqlite::OptionalExtension;
        Ok(self.db().query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='browser_window_generation'", [], |r| r.get(0)).optional()?.unwrap_or(0))
    }

    pub fn last_browser_window_opened(&self) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self.db().query_row("SELECT value FROM meta WHERE key='last_browser_window_opened'", [], |r| r.get(0)).optional()?)
    }

    pub fn mark_browser_window_opened(&self, address: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO meta(key,value) VALUES('browser_window_generation','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1", [])?;
        tx.execute("INSERT INTO meta(key,value) VALUES('browser_retirement_pending','true') ON CONFLICT(key) DO UPDATE SET value='true'", [])?;
        tx.execute("INSERT INTO meta(key,value) VALUES('last_browser_window_opened',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [address])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_browser_retirement_pending(&self, pending: bool) -> Result<()> {
        self.db().execute("INSERT INTO meta(key,value) VALUES('browser_retirement_pending',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [if pending { "true" } else { "false" }])?;
        Ok(())
    }

    pub fn desktop_reset(&self) -> Result<serde_json::Value> {
        use rusqlite::OptionalExtension;
        let raw: Option<String> = self.db().query_row("SELECT value FROM meta WHERE key='desktop_reset'", [], |r| r.get(0)).optional()?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null))
    }

    pub fn put_desktop_reset(&self, value: &serde_json::Value) -> Result<()> {
        self.db().execute("INSERT INTO meta(key,value) VALUES('desktop_reset',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value.to_string()])?;
        Ok(())
    }
    /// `task_ref` opened this window. A window already a task's stays that
    /// task's; an address now naming another process's window (Hyprland
    /// started again) is the new window's.
    pub fn own_window(&self, task_ref: &str, address: &str, pid: i64, class: &str, title: &str, process_start_ticks: Option<u64>, compositor_instance: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM task_windows WHERE address = ? AND (pid != ? OR class != ? OR process_start_ticks IS NOT ? OR compositor_instance != ?)",
            params![address, pid, class, process_start_ticks, compositor_instance])?;
        tx.execute(
            "INSERT OR IGNORE INTO task_windows(address, task_ref, pid, class, title, process_start_ticks, compositor_instance) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![address, task_ref, pid, clip(class, MAX_TEXT_CHARS), clip(title, MAX_TEXT_CHARS), process_start_ticks, compositor_instance],
        )?;
        tx.execute("DELETE FROM task_windows WHERE id IN (SELECT id FROM task_windows ORDER BY id DESC LIMIT -1 OFFSET ?)", [MAX_TASK_WINDOWS])?;
        tx.commit()?;
        Ok(())
    }

    /// A person used the window at `address` (it took focus or changed its
    /// title while no agent step ran). While a person has the computer every
    /// such change is theirs, so windows they never went to stay the task's to
    /// close.
    pub fn touch_window(&self, address: &str) -> Result<()> {
        self.db().prepare_cached("UPDATE task_windows SET touched = 1 WHERE address = ? AND touched = 0")?.execute([address])?;
        Ok(())
    }

    /// The window at `address` has a new title.
    pub fn retitle_window(&self, address: &str, title: &str) -> Result<()> {
        let title = clip(title, MAX_TEXT_CHARS);
        self.db().prepare_cached("UPDATE task_windows SET title = ?2 WHERE address = ?1 AND title != ?2")?.execute(params![address, title])?;
        Ok(())
    }

    /// The window at `address` closed.
    pub fn window_closed(&self, address: &str) -> Result<()> {
        self.db().prepare_cached("DELETE FROM task_windows WHERE address = ?")?.execute([address])?;
        Ok(())
    }

    /// The windows `task_ref` opened, oldest first.
    pub fn task_windows(&self, task_ref: &str) -> Result<Vec<TaskWindow>> {
        Ok(self
            .db()
            .prepare_cached("SELECT * FROM task_windows WHERE task_ref = ? ORDER BY id")?
            .query_map([task_ref], window_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every window a task opened, oldest first.
    pub fn owned_windows(&self) -> Result<Vec<TaskWindow>> {
        Ok(self.db().prepare_cached("SELECT * FROM task_windows ORDER BY id")?.query_map([], window_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The task finished: its windows are no longer anyone's.
    pub fn forget_task_windows(&self, task_ref: &str) -> Result<()> {
        self.db().prepare_cached("DELETE FROM task_windows WHERE task_ref = ?")?.execute([task_ref])?;
        Ok(())
    }
}
