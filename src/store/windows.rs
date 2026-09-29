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
        touched: r.get::<_, i64>("touched")? != 0,
    })
}

impl Journal {
    /// `task_ref` opened this window. A window already a task's stays that
    /// task's; an address now naming another process's window (Hyprland
    /// started again) is the new window's.
    pub fn own_window(&self, task_ref: &str, address: &str, pid: i64, class: &str, title: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM task_windows WHERE address = ? AND pid != ?", params![address, pid])?;
        tx.execute(
            "INSERT OR IGNORE INTO task_windows(address, task_ref, pid, class, title) VALUES (?, ?, ?, ?, ?)",
            params![address, task_ref, pid, clip(class, MAX_TEXT_CHARS), clip(title, MAX_TEXT_CHARS)],
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

    /// The task finished: its windows are no longer anyone's.
    pub fn forget_task_windows(&self, task_ref: &str) -> Result<()> {
        self.db().prepare_cached("DELETE FROM task_windows WHERE task_ref = ?")?.execute([task_ref])?;
        Ok(())
    }
}
