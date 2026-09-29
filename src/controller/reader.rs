//! The journal as storage sees it (`storage::JournalView`).
//!
//! Storage is `Send + Sync` and calls back into the journal from its own
//! code paths, sometimes while the controller is inside a journal call, so
//! the view uses its own read-only connection instead of sharing `Journal`.

use crate::error::{IbaraError, Result, denied, invalid};
use crate::storage::{JournalView, TaskState, TaskView};
use crate::store::{Clock, GrantRecord, TaskRecord, may_read_shared_task};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) struct JournalReader {
    db: Mutex<Connection>,
    clock: Clock,
}

fn json_array(text: Option<String>) -> Vec<Value> {
    text.and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| if let Value::Array(items) = v { Some(items) } else { None })
        .unwrap_or_default()
}

impl JournalReader {
    pub fn open(state_dir: &Path, clock: Clock) -> Result<JournalReader> {
        let db = Connection::open_with_flags(
            state_dir.join("journal.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(std::time::Duration::from_millis(5000))?;
        db.pragma_update(None, "cache_size", -256)?;
        Ok(JournalReader { db: Mutex::new(db), clock })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let db = self.db.lock().map_err(|_| crate::error::internal("journal view poisoned"))?;
        Ok(f(&db)?)
    }

    /// `requireReadableTask` without the journal: owner, or an active agent grant
    /// in the task's group when the task is shared.
    pub fn readable(&self, principal: &str, task_ref: &str) -> Result<TaskRecord> {
        if !super::is_ref(task_ref) {
            return Err(invalid("task_ref is not a valid identifier."));
        }
        let row = self.with(|db| {
            db.query_row(
                "SELECT principal, state, visibility, owner_group, deliveries FROM tasks WHERE task_ref = ?",
                [task_ref],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
        })?;
        let Some((owner, state, visibility, owner_group, deliveries)) = row else {
            return Err(unreadable());
        };
        let task = TaskRecord {
            task_ref: task_ref.to_string(),
            principal: owner,
            created_at: String::new(),
            updated_at: String::new(),
            state,
            goal: String::new(),
            success_criteria: Vec::new(),
            budgets: Value::Null,
            client_flags: Value::Null,
            authorization_ref: None,
            required_capabilities: Vec::new(),
            control_started_ms: None,
            last_charge_ms: None,
            active_control_used_ms: 0,
            actions_used: 0,
            images_used: 0,
            last_checkpoint_ref: None,
            completion: None,
            contract_version: String::new(),
            visibility: if visibility.as_deref() == Some("shared") { "shared" } else { "private" }.into(),
            owner_group,
            deliveries: json_array(deliveries),
        };
        let grants = match task.owner_group.as_deref().filter(|g| !g.is_empty()) {
            Some(group) if task.principal != principal => self.with(|db| {
                let mut stmt = db.prepare_cached(
                    "SELECT group_name, principal, role, state, peer_key, updated_at FROM grants WHERE group_name = ?",
                )?;
                stmt.query_map([group], |r| {
                    Ok(GrantRecord {
                        group: r.get(0)?,
                        principal: r.get(1)?,
                        role: r.get(2)?,
                        state: r.get(3)?,
                        peer_key: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })?,
            _ => Vec::new(),
        };
        if !may_read_shared_task(&task, principal, &grants) {
            return Err(unreadable());
        }
        Ok(task)
    }

    /// `context.assertAuthority()` (`core.ts:995-1003`) for storage calls: the
    /// task is readable, no person holds the desktop, and the same lease
    /// generation is active for the same connection.
    pub fn authority(self: &Arc<Self>, principal: &str, task_ref: &str, generation: &str, connection_id: &str) -> crate::storage::AuthorityFn {
        let me = self.clone();
        let (principal, task_ref, generation, connection_id) =
            (principal.to_string(), task_ref.to_string(), generation.to_string(), connection_id.to_string());
        Arc::new(move || {
            let task=me.readable(&principal, &task_ref)?;
            {
                let db=me.db.lock().map_err(|_|crate::error::internal("journal view poisoned"))?;
                if let Some(a)=crate::access::Access::read(&db)? {
                    let subject=task.client_flags["agent"].as_str().unwrap_or(&principal);
                    if a.rule(subject,"agents",(me.clock)())==crate::access::Rule::Deny {return Err(denied("Agent access revoked or expired."));}
                }
            }
            let (human, by_system, live) = me.with(|db| {
                let (human, origin): (i64, Option<String>) =
                    db.query_row("SELECT human_control, pause_origin FROM control_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
                let live = db
                    .query_row(
                        "SELECT generation, connection_id, principal, idle_expires_at_ms FROM leases WHERE state = 'active'",
                        [],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)),
                    )
                    .optional()?;
                Ok((human != 0, origin.as_deref() == Some(crate::store::PauseOrigin::System.as_str()), live))
            })?;
            if human {
                // Only a start or stop pauses for the system while an agent holds control.
                return Err(if by_system {
                    super::control::restart_ended()
                } else {
                    IbaraError::new("HUMAN_CONTROL", "A person currently holds the computer.", false)
                });
            }
            match live {
                Some((g, c, p, expires)) if g == generation && c == connection_id && p == principal && expires > (me.clock)() => Ok(()),
                _ => Err(IbaraError::new("LEASE_EXPIRED", "Lease generation changed.", false)),
            }
        })
    }

    fn receipts(&self, task_ref: &str) -> Result<Vec<Value>> {
        self.with(|db| {
            let mut stmt = db.prepare_cached("SELECT receipt FROM operations WHERE task_ref = ?")?;
            stmt.query_map([task_ref], |r| r.get::<_, String>(0))?
                .map(|t| t.map(|t| serde_json::from_str(&t).unwrap_or(Value::Null)))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
    }
}

fn unreadable() -> IbaraError {
    denied("Task is private or this principal has no active group grant.")
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

impl JournalView for JournalReader {
    fn read_task(&self, principal: &str, task_ref: &str) -> Result<TaskView> {
        let task = self.readable(principal, task_ref)?;
        Ok(TaskView { state: task.state, deliveries: task.deliveries })
    }

    fn collector(&self, principal: &str) -> Option<String> {
        self.with(|db| {
            db.query_row("SELECT value FROM meta WHERE key = ?", params![format!("collector:{principal}")], |r| r.get(0))
                .optional()
        })
        .ok()
        .flatten()
    }

    fn task_state(&self, task_ref: &str) -> Result<Option<TaskState>> {
        self.with(|db| {
            db.query_row("SELECT state, updated_at FROM tasks WHERE task_ref = ?", [task_ref], |r| {
                Ok(TaskState { state: r.get(0)?, updated_at: r.get(1)? })
            })
            .optional()
        })
    }

    fn has_unresolved(&self, task_ref: &str) -> Result<bool> {
        Ok(self.receipts(task_ref)?.iter().any(|r| {
            matches!(str_at(r, "execution"), Some("unknown" | "running"))
                || matches!(str_at(r, "verification"), Some("unknown" | "pending"))
                || str_at(r, "effect") == Some("unknown")
        }))
    }

    fn has_unsettled(&self, task_ref: &str) -> Result<bool> {
        Ok(self.receipts(task_ref)?.iter().any(|r| {
            matches!(str_at(r, "execution"), Some("unknown" | "pending"))
                || r.get("error").and_then(|e| str_at(e, "code")) == Some("CONTROL_UNSETTLED")
        }))
    }
}
