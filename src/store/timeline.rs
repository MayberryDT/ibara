//! The timeline (core schema 3): events, attention items and app notes.
//!
//! One record of what ibara did, attention items for "ask", and app notes
//! (automatic facts about apps, shown to later agents). Every table is
//! bounded: events age out with the journal retention cutoff (except for
//! retained tasks) and are capped by count; settled attention items age out;
//! app notes are capped by count, least recently updated first.

use super::clip;
use super::journal::{Journal, parse_json};
use crate::error::{Result, invalid};
use crate::ids::id;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Oldest events beyond this many are dropped regardless of age.
pub const MAX_TIMELINE_EVENTS: i64 = 100_000;
/// App notes kept per computer.
pub const MAX_APP_NOTES: i64 = 5_000;
const MAX_SUMMARY_CHARS: usize = 2_000;
const MAX_DATA_BYTES: usize = 16 * 1024;
const MAX_TEXT_CHARS: usize = 4_000;
const MAX_KEY_CHARS: usize = 256;
const MAX_OPTIONS: usize = 20;
const MAX_LIST: i64 = 500;

/// One timeline event. `id` is the revision: strictly increasing, never reused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub task_ref: Option<String>,
    pub actor: String,
    pub summary: String,
    pub data: Value,
}

/// Input of `append_event`.
#[derive(Debug, Clone, Copy)]
pub struct NewEvent<'a> {
    pub at: &'a str,
    pub kind: &'a str,
    pub task_ref: Option<&'a str>,
    pub actor: &'a str,
    pub summary: &'a str,
    pub data: &'a Value,
}

/// Something a person needs to see, answer or approve (`att_…`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub att_ref: String,
    pub task_ref: String,
    pub principal: String,
    /// `question` (from `computer_checkpoint({ask})`) or `approval` (a held step).
    pub kind: String,
    /// The held step, when the item is an approval for an operation.
    pub operation_ref: Option<String>,
    /// The lease generation the item was raised under; an approval applies only to it.
    pub generation: Option<String>,
    /// What a person reads: an agent's question in its own words, or an
    /// approval in plain words (who asks, what, where and why).
    pub question: String,
    /// The structured request behind an approval's words, for a Details view
    /// and for agents; null for questions and for items from older builds.
    #[serde(default)]
    pub details: Value,
    pub options: Vec<String>,
    /// `open | answered | expired`.
    pub state: String,
    pub answer: Option<String>,
    pub answered_by: Option<String>,
    pub created_at: String,
    pub answered_at: Option<String>,
}

/// Input of `raise_attention`.
#[derive(Debug, Clone, Copy)]
pub struct NewAttention<'a> {
    pub task_ref: &'a str,
    pub principal: &'a str,
    /// `question` or `approval`.
    pub kind: &'a str,
    pub operation_ref: Option<&'a str>,
    pub generation: Option<&'a str>,
    pub question: &'a str,
    pub details: Option<&'a Value>,
    pub options: &'a [String],
    pub now_iso: &'a str,
}

/// An automatic fact about an app surface on this computer (`note_…`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppNote {
    pub note_ref: String,
    pub app: String,
    pub version: String,
    pub surface_signature: String,
    pub fact_kind: String,
    pub fact: Value,
    pub updated_at: String,
    /// How many step outcomes have confirmed this fact.
    pub count: i64,
}

fn insert_event(conn: &Connection, event: &NewEvent<'_>) -> Result<i64> {
    let mut data = event.data.to_string();
    if data.len() > MAX_DATA_BYTES {
        data = json!({ "truncated": true, "bytes": data.len() }).to_string();
    }
    conn.prepare_cached("INSERT INTO timeline_events(at, kind, task_ref, actor, summary, data) VALUES (?, ?, ?, ?, ?, ?)")?.execute(params![
        event.at,
        clip(event.kind, MAX_KEY_CHARS),
        event.task_ref,
        clip(event.actor, MAX_KEY_CHARS),
        clip(event.summary, MAX_SUMMARY_CHARS),
        data
    ])?;
    Ok(conn.last_insert_rowid())
}

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<(TimelineEvent, String)> {
    Ok((
        TimelineEvent {
            id: r.get("id")?,
            at: r.get("at")?,
            kind: r.get("kind")?,
            task_ref: r.get("task_ref")?,
            actor: r.get("actor")?,
            summary: r.get("summary")?,
            data: Value::Null,
        },
        r.get("data")?,
    ))
}

fn with_data(rows: Vec<(TimelineEvent, String)>) -> Result<Vec<TimelineEvent>> {
    rows.into_iter()
        .map(|(mut e, d)| {
            e.data = parse_json(Some(&d), json!({}))?;
            Ok(e)
        })
        .collect()
}

fn attention_from_row(r: &Row<'_>) -> rusqlite::Result<AttentionItem> {
    let options: String = r.get("options")?;
    Ok(AttentionItem {
        att_ref: r.get("att_ref")?,
        task_ref: r.get("task_ref")?,
        principal: r.get("principal")?,
        kind: r.get("kind")?,
        operation_ref: r.get("operation_ref")?,
        generation: r.get("generation")?,
        question: r.get("question")?,
        details: r.get::<_, Option<String>>("details")?.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(Value::Null),
        options: serde_json::from_str(&options).unwrap_or_default(),
        state: r.get("state")?,
        answer: r.get("answer")?,
        answered_by: r.get("answered_by")?,
        created_at: r.get("created_at")?,
        answered_at: r.get("answered_at")?,
    })
}

fn note_from_row(r: &Row<'_>) -> rusqlite::Result<(AppNote, String)> {
    Ok((
        AppNote {
            note_ref: r.get("note_ref")?,
            app: r.get("app")?,
            version: r.get("version")?,
            surface_signature: r.get("surface_signature")?,
            fact_kind: r.get("fact_kind")?,
            fact: Value::Null,
            updated_at: r.get("updated_at")?,
            count: r.get("count")?,
        },
        r.get("fact")?,
    ))
}

/// Retention for the timeline tables, inside the journal's retention
/// transaction. Events of tasks the journal still retains are pruned with
/// their task (see `Journal::enforce_retention`); here: events with no task or
/// an unknown task, settled attention items, and the event row cap.
pub(crate) fn prune(tx: &Transaction<'_>, cutoff: &str) -> Result<usize> {
    let mut deleted = tx.execute(
        "DELETE FROM timeline_events WHERE at <= ?1 AND (task_ref IS NULL OR task_ref NOT IN (SELECT task_ref FROM tasks))",
        [cutoff],
    )?;
    deleted += tx.execute(
        "DELETE FROM attention_items WHERE state != 'open' AND COALESCE(answered_at, created_at) <= ?1",
        [cutoff],
    )?;
    deleted += tx.execute(
        "DELETE FROM timeline_events WHERE id <= (SELECT MAX(id) FROM timeline_events) - ?1",
        [MAX_TIMELINE_EVENTS],
    )?;
    Ok(deleted)
}

impl Journal {
    // ---- timeline events ----------------------------------------------------

    /// Append an event; returns its id (the new revision). `summary` is clipped
    /// to 2 000 characters and `data` above 16 KiB is replaced by a size marker.
    pub fn append_event(&self, event: NewEvent<'_>) -> Result<i64> {
        insert_event(self.db(), &event)
    }

    /// The latest revision (0 when the timeline is empty).
    pub fn timeline_revision(&self) -> Result<i64> {
        Ok(self.db().prepare_cached("SELECT COALESCE(MAX(id), 0) FROM timeline_events")?.query_row([], |r| r.get(0))?)
    }

    /// Events after revision `after`, oldest first, optionally for one task.
    pub fn events_since(&self, after: i64, task_ref: Option<&str>, limit: i64) -> Result<Vec<TimelineEvent>> {
        let limit = limit.clamp(1, MAX_LIST);
        let rows = match task_ref {
            Some(t) => self
                .db()
                .prepare_cached("SELECT * FROM timeline_events WHERE id > ? AND task_ref = ? ORDER BY id ASC LIMIT ?")?
                .query_map(params![after, t, limit], event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .db()
                .prepare_cached("SELECT * FROM timeline_events WHERE id > ? ORDER BY id ASC LIMIT ?")?
                .query_map(params![after, limit], event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        with_data(rows)
    }

    /// Recent events, newest first, before revision `before` (exclusive) when given.
    pub fn recent_events(&self, before: Option<i64>, limit: i64) -> Result<Vec<TimelineEvent>> {
        let limit = limit.clamp(1, MAX_LIST);
        let rows = self
            .db()
            .prepare_cached("SELECT * FROM timeline_events WHERE id < ? ORDER BY id DESC LIMIT ?")?
            .query_map(params![before.unwrap_or(i64::MAX), limit], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        with_data(rows)
    }

    /// What a person reads ("while you were away"): the newest `limit` events
    /// after revision `after`, oldest first, without the agents' individual steps.
    pub fn events_for_person(&self, after: i64, limit: i64) -> Result<Vec<TimelineEvent>> {
        let limit = limit.clamp(1, MAX_LIST);
        let mut rows = self
            .db()
            .prepare_cached("SELECT * FROM timeline_events WHERE id > ? AND kind != 'step' ORDER BY id DESC LIMIT ?")?
            .query_map(params![after, limit], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        with_data(rows)
    }

    /// When the newest event of `kind` happened, and its summary.
    pub fn last_event(&self, kind: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db()
            .prepare_cached("SELECT at, summary FROM timeline_events WHERE kind = ? ORDER BY id DESC LIMIT 1")?
            .query_row([kind], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    // ---- attention ----------------------------------------------------------

    /// Raise an open attention item and record `attention.raised` on the timeline.
    pub fn raise_attention(&self, input: NewAttention<'_>) -> Result<AttentionItem> {
        if input.question.trim().is_empty() {
            return Err(invalid("An attention item needs a question."));
        }
        if input.kind != "question" && input.kind != "approval" {
            return Err(invalid("An attention item is a question or an approval."));
        }
        if input.options.len() > MAX_OPTIONS {
            return Err(invalid(format!("At most {MAX_OPTIONS} options are allowed.")));
        }
        if input.details.is_some_and(|d| d.to_string().len() > MAX_DATA_BYTES) {
            return Err(invalid("An attention item's details must be at most 16 KiB."));
        }
        let item = AttentionItem {
            att_ref: id("att"),
            task_ref: input.task_ref.to_string(),
            principal: input.principal.to_string(),
            kind: input.kind.to_string(),
            operation_ref: input.operation_ref.map(str::to_string),
            generation: input.generation.map(str::to_string),
            question: clip(input.question, MAX_TEXT_CHARS),
            details: input.details.cloned().unwrap_or(Value::Null),
            options: input.options.iter().map(|o| clip(o, MAX_KEY_CHARS)).collect(),
            state: "open".into(),
            answer: None,
            answered_by: None,
            created_at: input.now_iso.to_string(),
            answered_at: None,
        };
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO attention_items(att_ref, task_ref, principal, kind, operation_ref, generation, question, details, options, state, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?)",
            params![
                item.att_ref,
                item.task_ref,
                item.principal,
                item.kind,
                item.operation_ref,
                item.generation,
                item.question,
                input.details.map(Value::to_string),
                serde_json::to_string(&item.options).unwrap_or_else(|_| "[]".into()),
                item.created_at
            ],
        )?;
        insert_event(
            &tx,
            &NewEvent {
                at: input.now_iso,
                kind: "attention.raised",
                task_ref: Some(input.task_ref),
                actor: input.principal,
                summary: &item.question,
                data: &json!({ "att_ref": item.att_ref, "kind": item.kind, "operation_ref": item.operation_ref, "options": item.options }),
            },
        )?;
        tx.commit()?;
        Ok(item)
    }

    /// Answer an open item. With options, the answer must be one of them.
    /// Answering again with the same answer returns the item unchanged.
    pub fn answer_attention(&self, att_ref: &str, answer: &str, answered_by: &str, now_iso: &str) -> Result<AttentionItem> {
        self.answer_attention_with(att_ref, answer, answered_by, now_iso, |_| Ok(()))
    }

    /// [`Journal::answer_attention`], with `also` written in the same
    /// transaction: the answer and `also` are saved together, or neither is.
    /// `also` does not run for an item already answered the same way.
    pub fn answer_attention_with(
        &self,
        att_ref: &str,
        answer: &str,
        answered_by: &str,
        now_iso: &str,
        also: impl FnOnce(&Transaction<'_>) -> Result<()>,
    ) -> Result<AttentionItem> {
        let item = self.get_attention(att_ref)?.ok_or_else(|| invalid("Unknown attention item."))?;
        if item.state == "answered" && item.answer.as_deref() == Some(answer) {
            return Ok(item);
        }
        if item.state != "open" {
            return Err(invalid(format!("The attention item is already {}.", item.state)));
        }
        if !item.options.is_empty() && !item.options.iter().any(|o| o == answer) {
            return Err(invalid("The answer must be one of the item's options."));
        }
        let answer = clip(answer, MAX_TEXT_CHARS);
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        let answered = tx.execute(
            "UPDATE attention_items SET state = 'answered', answer = ?, answered_by = ?, answered_at = ? WHERE att_ref = ? AND state = 'open'",
            params![answer, answered_by, now_iso, att_ref],
        )?;
        if answered != 1 {
            return Err(invalid("The attention item is no longer open."));
        }
        insert_event(
            &tx,
            &NewEvent {
                at: now_iso,
                kind: "attention.answered",
                task_ref: Some(&item.task_ref),
                actor: answered_by,
                summary: &answer,
                data: &json!({ "att_ref": att_ref, "operation_ref": item.operation_ref }),
            },
        )?;
        also(&tx)?;
        tx.commit()?;
        self.get_attention(att_ref)?.ok_or_else(|| invalid("Unknown attention item."))
    }

    /// Expire open items: one item, or every open item of a task (for example
    /// when control changes hands and approvals no longer apply). Returns how many expired.
    pub fn expire_attention(&self, att_ref: Option<&str>, task_ref: Option<&str>, actor: &str, now_iso: &str) -> Result<usize> {
        let open: Vec<AttentionItem> = match (att_ref, task_ref) {
            (Some(a), _) => self.get_attention(a)?.into_iter().filter(|i| i.state == "open").collect(),
            (None, Some(t)) => self.list_attention(Some("open"), Some(t), MAX_LIST)?,
            (None, None) => return Err(invalid("Name an attention item or a task.")),
        };
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        for item in &open {
            tx.execute(
                "UPDATE attention_items SET state = 'expired', answered_at = ? WHERE att_ref = ? AND state = 'open'",
                params![now_iso, item.att_ref],
            )?;
            insert_event(
                &tx,
                &NewEvent {
                    at: now_iso,
                    kind: "attention.expired",
                    task_ref: Some(&item.task_ref),
                    actor,
                    summary: &item.question,
                    data: &json!({ "att_ref": item.att_ref, "operation_ref": item.operation_ref }),
                },
            )?;
        }
        tx.commit()?;
        Ok(open.len())
    }

    /// Expire the open items of every task that has ended (finished, or
    /// interrupted when its control ended): no agent is left to read an
    /// answer. Items that belong to no task (access requests) stay. Returns
    /// how many expired.
    pub fn expire_attention_of_ended_tasks(&self, actor: &str, now_iso: &str) -> Result<usize> {
        let ended: Vec<String> = self
            .db()
            .prepare_cached(
                "SELECT DISTINCT a.task_ref FROM attention_items a JOIN tasks t ON t.task_ref = a.task_ref
                 WHERE a.state = 'open' AND t.state NOT IN ('active', 'created', 'waiting_for_human')",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut expired = 0;
        for task_ref in &ended {
            expired += self.expire_attention(None, Some(task_ref), actor, now_iso)?;
        }
        Ok(expired)
    }

    pub fn get_attention(&self, att_ref: &str) -> Result<Option<AttentionItem>> {
        Ok(self
            .db()
            .prepare_cached("SELECT * FROM attention_items WHERE att_ref = ?")?
            .query_row([att_ref], attention_from_row)
            .optional()?)
    }

    /// Items newest first, filtered by state and task.
    pub fn list_attention(&self, state: Option<&str>, task_ref: Option<&str>, limit: i64) -> Result<Vec<AttentionItem>> {
        Ok(self
            .db()
            .prepare_cached(
                "SELECT * FROM attention_items WHERE (?1 IS NULL OR state = ?1) AND (?2 IS NULL OR task_ref = ?2) ORDER BY created_at DESC, att_ref DESC LIMIT ?3",
            )?
            .query_map(params![state, task_ref, limit.clamp(1, MAX_LIST)], attention_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every approval of a task held for an operation, newest first: no
    /// question or other item, however many, hides one.
    pub fn list_task_approvals(&self, task_ref: &str) -> Result<Vec<AttentionItem>> {
        Ok(self
            .db()
            .prepare_cached("SELECT * FROM attention_items WHERE task_ref = ? AND kind = 'approval' AND operation_ref IS NOT NULL ORDER BY created_at DESC, att_ref DESC")?
            .query_map([task_ref], attention_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Open items, for the situation line.
    pub fn count_open_attention(&self, task_ref: Option<&str>) -> Result<i64> {
        Ok(self
            .db()
            .prepare_cached("SELECT COUNT(*) FROM attention_items WHERE state = 'open' AND (?1 IS NULL OR task_ref = ?1)")?
            .query_row([task_ref], |r| r.get(0))?)
    }

    // ---- app notes ----------------------------------------------------------

    /// Record one step outcome as a fact about an app surface: insert, or
    /// replace the fact and bump `count`. Keeps at most `MAX_APP_NOTES` notes.
    pub fn record_app_note(&self, app: &str, version: &str, surface_signature: &str, fact_kind: &str, fact: &Value, now_iso: &str) -> Result<AppNote> {
        if app.is_empty() || fact_kind.is_empty() {
            return Err(invalid("An app note needs an app and a fact kind."));
        }
        let fact_text = fact.to_string();
        if fact_text.len() > MAX_DATA_BYTES {
            return Err(invalid("An app note fact must be at most 16 KiB."));
        }
        let (app, version, surface, kind) =
            (clip(app, MAX_KEY_CHARS), clip(version, MAX_KEY_CHARS), clip(surface_signature, MAX_KEY_CHARS), clip(fact_kind, MAX_KEY_CHARS));
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO app_notes(note_ref, app, version, surface_signature, fact_kind, fact, updated_at, count) VALUES (?, ?, ?, ?, ?, ?, ?, 1)
             ON CONFLICT(app, version, surface_signature, fact_kind) DO UPDATE SET fact = excluded.fact, updated_at = excluded.updated_at, count = count + 1",
            params![id("note"), app, version, surface, kind, fact_text, now_iso],
        )?;
        tx.execute(
            "DELETE FROM app_notes WHERE note_ref IN (SELECT note_ref FROM app_notes ORDER BY updated_at DESC, note_ref DESC LIMIT -1 OFFSET ?)",
            [MAX_APP_NOTES],
        )?;
        tx.commit()?;
        let (mut note, text) = self
            .db()
            .prepare_cached("SELECT * FROM app_notes WHERE app = ? AND version = ? AND surface_signature = ? AND fact_kind = ?")?
            .query_row(params![app, version, surface, kind], note_from_row)?;
        note.fact = parse_json(Some(&text), json!({}))?;
        Ok(note)
    }

    /// Notes for an app, optionally narrowed to a version and surface, most recently confirmed first.
    pub fn list_app_notes(&self, app: &str, version: Option<&str>, surface_signature: Option<&str>, limit: i64) -> Result<Vec<AppNote>> {
        let rows = self
            .db()
            .prepare_cached(
                "SELECT * FROM app_notes WHERE app = ?1 AND (?2 IS NULL OR version = ?2) AND (?3 IS NULL OR surface_signature = ?3) ORDER BY updated_at DESC, note_ref DESC LIMIT ?4",
            )?
            .query_map(params![app, version, surface_signature, limit.clamp(1, MAX_LIST)], note_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(mut n, t)| {
                n.fact = parse_json(Some(&t), json!({}))?;
                Ok(n)
            })
            .collect()
    }
}
