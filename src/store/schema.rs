//! Journal schema and migrations.
//!
//! Versions 1 and 2 are the TypeScript controller's (`controller/src/journal.ts:806-945`),
//! reproduced verbatim and recorded in `meta.schema_version`. That value stays
//! `'2'` so the Node controller can still open the journal after a rollback
//! (it refuses anything outside `{'1','2'}`).
//!
//! Later versions are additive and recorded in `meta.core_schema_version`:
//! - **3**: the timeline (`timeline_events`, `attention_items`, `app_notes`),
//!   `operations.effect_class` (default `'change'`), and, once, the cutover
//!   settlement of stale job-backed `running` receipts.
//! - **4**: the access model (`meta.access_model`, written by `crate::access`).
//! - **5**: `control_state.pause_origin` (`person` or `system`, set while
//!   paused). A journal already paused when it migrates was paused by a start
//!   or shutdown, so it is recorded as the system's.
//! - **6**: `task_windows`, the windows each task opened (`windows.rs`), so
//!   they stay the task's when ibara restarts and the apps outlive it.
//!
//! Columns an older build reads past are added without a version, so going
//! back to it needs no journal restore:
//! - `attention_items.details`: the structured request behind an item's plain
//!   words (JSON). Approvals still open when it is added were asked in words
//!   that could show JSON, window addresses and process ids; they belong to an
//!   earlier start of ibara, whose control they no longer match, so they
//!   expire.
//! - `operations.session`: the agent session (connection id) that made the
//!   request, so a request_id can be reused for a new request in a later
//!   session. Rows from before it have none and count as another session's.
//! - `attention_items.kind` also allows `login` (an agent's request for the
//!   person's logins). SQLite cannot change a CHECK in place, so the table is
//!   rebuilt once with the same columns and rows; an older build reads the
//!   rows as they are and never writes that kind.
//!
//! Well-known agents' short names (`codex@vesper`, formerly
//! `codex-mcp-client@vesper`) need no version either: at every open, what an
//! older build kept under a long name moves to the short name
//! ([`crate::access::rename_agents`]).

use crate::error::{IbaraError, Result};
use crate::ids::id;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use std::path::Path;

/// `SCHEMA_VERSION` (journal.ts:7).
pub const SCHEMA_VERSION: &str = "2";
/// The latest additive version this build writes into `meta.core_schema_version`.
pub const CORE_SCHEMA_VERSION: u32 = 6;

pub(crate) fn unsupported() -> IbaraError {
    IbaraError::new(
        "INTERNAL_ERROR",
        "Unsupported journal schema; restore a compatible runtime or reviewed backup without modifying this database.",
        false,
    )
}

/// The versions recorded in a journal's `meta`, read without writing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaVersions {
    pub schema_version: Option<String>,
    pub core_schema_version: Option<String>,
}

pub(crate) fn read_versions(conn: &Connection) -> Result<SchemaVersions> {
    let has_meta: bool = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |_| Ok(()))
        .optional()?
        .is_some();
    if !has_meta {
        return Ok(SchemaVersions { schema_version: None, core_schema_version: None });
    }
    let get = |key: &str| -> Result<Option<String>> {
        Ok(conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).optional()?)
    };
    Ok(SchemaVersions { schema_version: get("schema_version")?, core_schema_version: get("core_schema_version")? })
}

/// Refuse versions this build does not understand, before writing anything
/// (journal.ts:809-812).
pub(crate) fn check_supported(versions: &SchemaVersions) -> Result<u32> {
    if let Some(v) = &versions.schema_version
        && v != "1"
        && v != "3"
        && v != SCHEMA_VERSION
    {
        return Err(unsupported());
    }
    match &versions.core_schema_version {
        None => Ok(0),
        Some(v) => match v.parse::<u32>() {
            Ok(n) if n <= CORE_SCHEMA_VERSION => Ok(n),
            _ => Err(unsupported()),
        },
    }
}

/// What a migration changed, for the caller's log and the timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MigrationOutcome {
    pub from_core_version: u32,
    pub to_core_version: u32,
    pub stale_running_receipts_settled: usize,
    pub jobs_mirror_refreshed: usize,
}

/// `migrate()` (journal.ts:806-945) followed by the additive core versions,
/// in one transaction. `storage_path` is read (read-only) for the version 3
/// cutover evidence.
pub(crate) fn migrate(conn: &Connection, storage_path: &Path, now_iso: &str) -> Result<MigrationOutcome> {
    let versions = read_versions(conn)?;
    let from = check_supported(&versions)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(TS_DDL)?;
    let task_columns = columns(&tx, "tasks")?;
    if !task_columns.iter().any(|c| c == "contract_version") {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN contract_version TEXT NOT NULL DEFAULT '2.0'")?;
    }
    if !task_columns.iter().any(|c| c == "visibility") {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private'")?;
    }
    if !task_columns.iter().any(|c| c == "owner_group") {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN owner_group TEXT")?;
    }
    if !task_columns.iter().any(|c| c == "deliveries") {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN deliveries TEXT NOT NULL DEFAULT '[]'")?;
    }
    if !columns(&tx, "control_state")?.iter().any(|c| c == "settling_generation") {
        tx.execute_batch("ALTER TABLE control_state ADD COLUMN settling_generation TEXT")?;
    }
    let version: Option<String> =
        tx.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).optional()?;
    match version {
        None => {
            tx.execute("INSERT INTO meta(key, value) VALUES('schema_version', ?)", [SCHEMA_VERSION])?;
        }
        Some(v) if v != SCHEMA_VERSION && v != "3" => {
            tx.execute("UPDATE meta SET value = ? WHERE key = 'schema_version'", [SCHEMA_VERSION])?;
        }
        Some(_) => {}
    }
    let control: Option<i64> = tx.query_row("SELECT id FROM control_state WHERE id = 1", [], |r| r.get(0)).optional()?;
    if control.is_none() {
        tx.execute("INSERT INTO control_state(id, epoch) VALUES (1, ?)", [id("epoch")])?;
    }

    let mut outcome = MigrationOutcome { from_core_version: from, to_core_version: from.max(CORE_SCHEMA_VERSION), ..Default::default() };
    if from < 3 {
        migrate_v3(&tx, storage_path, now_iso, &mut outcome)?;
    }
    if from < 5 {
        migrate_v5(&tx)?;
    }
    if from < 6 {
        tx.execute_batch(V6_DDL)?;
    }
    if !columns(&tx, "attention_items")?.iter().any(|c| c == "details") {
        tx.execute_batch(
            "ALTER TABLE attention_items ADD COLUMN details TEXT;
             UPDATE attention_items SET state = 'expired' WHERE state = 'open' AND kind = 'approval';",
        )?;
    }
    if !columns(&tx, "operations")?.iter().any(|c| c == "session") {
        tx.execute_batch("ALTER TABLE operations ADD COLUMN session TEXT")?;
    }
    let attention_sql: String =
        tx.query_row("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'attention_items'", [], |r| r.get(0))?;
    if !attention_sql.contains("'login'") {
        tx.execute_batch(ATTENTION_WITH_LOGIN)?;
    }
    crate::access::rename_agents(&tx)?;
    if from < CORE_SCHEMA_VERSION {
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('core_schema_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [CORE_SCHEMA_VERSION.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(outcome)
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>("name"))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

/// Core schema 5: who paused the computer. The column check makes a repeated
/// run harmless.
fn migrate_v5(tx: &Transaction<'_>) -> Result<()> {
    if !columns(tx, "control_state")?.iter().any(|c| c == "pause_origin") {
        tx.execute_batch(
            "ALTER TABLE control_state ADD COLUMN pause_origin TEXT;
             UPDATE control_state SET pause_origin = 'system' WHERE paused = 1 OR human_control = 1;",
        )?;
    }
    Ok(())
}

fn migrate_v3(tx: &Transaction<'_>, storage_path: &Path, now_iso: &str, outcome: &mut MigrationOutcome) -> Result<()> {
    tx.execute_batch(V3_DDL)?;
    if !columns(tx, "operations")?.iter().any(|c| c == "effect_class") {
        tx.execute_batch("ALTER TABLE operations ADD COLUMN effect_class TEXT NOT NULL DEFAULT 'change'")?;
    }
    let storage = open_storage_readonly(storage_path);

    // Stale job-backed receipts: the TypeScript left these `running` until a
    // status call refreshed them. Steps that were running
    // become unknown, never done; storage's view of the job is kept as evidence.
    let stale: Vec<(String, String)> = {
        let mut stmt = tx.prepare(STALE_JOB_RECEIPTS_SQL)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (operation_ref, text) in stale {
        let Ok(Value::Object(mut receipt)) = serde_json::from_str::<Value>(&text) else { continue };
        let job_ref = receipt.get("job_ref").and_then(Value::as_str).unwrap_or("").to_string();
        let job = match &storage {
            Some(db) => storage_job(db, &job_ref)?,
            None => None,
        };
        settle_stale_job_receipt(&mut receipt, &job_ref, job.as_ref());
        tx.execute(
            "UPDATE operations SET receipt = ?, updated_at = ? WHERE operation_ref = ?",
            params![Value::Object(receipt).to_string(), now_iso, operation_ref],
        )?;
        outcome.stale_running_receipts_settled += 1;
    }

    // The journal `jobs` table is a mirror refreshed only on reads; storage is
    // the source of truth. Refresh rows storage disagrees with.
    if let Some(db) = &storage {
        let rows: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT job_ref, record FROM jobs")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (job_ref, text) in rows {
            let Some(job) = storage_job(db, &job_ref)? else { continue };
            let Some(record) = job.record_text.as_deref() else { continue };
            let mirrored_state = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_string));
            if mirrored_state.as_deref() != job.state.as_deref() && serde_json::from_str::<Value>(record).is_ok() {
                tx.execute("UPDATE jobs SET record = ? WHERE job_ref = ?", params![record, job_ref])?;
                outcome.jobs_mirror_refreshed += 1;
            }
        }
    }

    let data = json!({
        "from_core_version": outcome.from_core_version,
        "to_core_version": CORE_SCHEMA_VERSION,
        "stale_running_receipts_settled": outcome.stale_running_receipts_settled,
        "jobs_mirror_refreshed": outcome.jobs_mirror_refreshed,
    });
    tx.execute(
        "INSERT INTO timeline_events(at, kind, task_ref, actor, summary, data) VALUES (?, 'store.migrated', NULL, 'ibarad', ?, ?)",
        params![
            now_iso,
            format!("Journal migrated to core schema {CORE_SCHEMA_VERSION}; {} stale running step(s) marked unknown.", outcome.stale_running_receipts_settled),
            data.to_string()
        ],
    )?;
    Ok(())
}

/// Dispatched receipts still `running` that name a job.
pub(crate) const STALE_JOB_RECEIPTS_SQL: &str = "SELECT operation_ref, receipt FROM operations WHERE dispatched = 1 AND json_extract(receipt, '$.execution') = 'running' AND json_extract(receipt, '$.job_ref') IS NOT NULL AND json_extract(receipt, '$.job_ref') != ''";

/// A `storage.jobs` row, as evidence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StorageJob {
    pub state: Option<String>,
    pub exit_code: Option<i64>,
    pub termination_confirmed: Option<bool>,
    pub updated_at: Option<String>,
    pub record_text: Option<String>,
}

pub(crate) fn open_storage_readonly(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    let uri = format!("file:{}?mode=ro", path.display());
    let conn = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI).ok()?;
    let has_jobs = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'jobs'", [], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some();
    has_jobs.then_some(conn)
}

pub(crate) fn storage_job(db: &Connection, job_ref: &str) -> Result<Option<StorageJob>> {
    if job_ref.is_empty() {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT state, exit_code, termination_confirmed, updated_at, record FROM jobs WHERE job_ref = ?",
            [job_ref],
            |r| {
                Ok(StorageJob {
                    state: r.get(0)?,
                    exit_code: r.get(1)?,
                    termination_confirmed: r.get::<_, Option<i64>>(2)?.map(|v| v != 0),
                    updated_at: r.get(3)?,
                    record_text: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Turn a stale job-backed `running` receipt into `unknown`, recording what
/// storage knows about the job as `storage_outcome` and citing the job.
pub(crate) fn settle_stale_job_receipt(receipt: &mut Map<String, Value>, job_ref: &str, job: Option<&StorageJob>) {
    let evidence = match job {
        Some(job) => json!({
            "source": "storage.jobs",
            "job_ref": job_ref,
            "state": job.state,
            "exit_code": job.exit_code,
            "termination_confirmed": job.termination_confirmed,
            "updated_at": job.updated_at,
        }),
        None => json!({ "source": "storage.jobs", "job_ref": job_ref, "state": Value::Null, "note": "No storage job row was found." }),
    };
    let storage_state = job.and_then(|j| j.state.clone()).unwrap_or_else(|| "not recorded".into());
    receipt.insert("execution".into(), json!("unknown"));
    receipt.insert("verification".into(), json!("unknown"));
    receipt.insert("effect".into(), json!("unknown"));
    receipt.insert(
        "summary".into(),
        json!(super::clip(
            &format!("The controller was replaced while this job-backed step was recorded as running; storage last recorded job {job_ref} as {storage_state}. Reconcile the original effect before any new intention."),
            2000
        )),
    );
    receipt.insert(
        "error".into(),
        json!({
            "code": "OUTCOME_UNKNOWN",
            "message": "The step was still recorded as running when the controller was replaced.",
            "retry_safe": false,
            "requires_reconciliation": true,
        }),
    );
    receipt.insert("storage_outcome".into(), evidence);
    if !job_ref.is_empty() {
        let refs = receipt.entry("evidence_refs").or_insert_with(|| json!([]));
        if let Value::Array(items) = refs
            && items.len() < 30
            && !items.iter().any(|v| v.as_str() == Some(job_ref))
        {
            items.push(json!(job_ref));
        }
    }
}

/// DDL verbatim from journal.ts:815-931.
const TS_DDL: &str = r#"
      CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS control_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        human_control INTEGER NOT NULL DEFAULT 0,
        paused INTEGER NOT NULL DEFAULT 0,
        unsettled INTEGER NOT NULL DEFAULT 0,
        settling_generation TEXT,
        epoch TEXT NOT NULL,
        session_hint INTEGER NOT NULL DEFAULT 1
      );
      CREATE TABLE IF NOT EXISTS tasks (
        task_ref TEXT PRIMARY KEY,
        principal TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        state TEXT NOT NULL,
        goal TEXT NOT NULL,
        success_criteria TEXT NOT NULL,
        budgets TEXT NOT NULL,
        client_flags TEXT NOT NULL,
        authorization_ref TEXT,
        required_capabilities TEXT NOT NULL,
        control_started_ms INTEGER,
        last_charge_ms INTEGER,
        active_control_used_ms INTEGER NOT NULL DEFAULT 0,
        actions_used INTEGER NOT NULL DEFAULT 0,
        images_used INTEGER NOT NULL DEFAULT 0,
        last_checkpoint_ref TEXT,
        completion TEXT
      );
      CREATE TABLE IF NOT EXISTS leases (
        generation TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        connection_id TEXT NOT NULL,
        epoch TEXT NOT NULL,
        acquired_at TEXT NOT NULL,
        last_heartbeat_at TEXT NOT NULL,
        last_heartbeat_ms INTEGER NOT NULL,
        idle_expires_at_ms INTEGER NOT NULL,
        state TEXT NOT NULL,
        reason TEXT
      );
      CREATE UNIQUE INDEX IF NOT EXISTS leases_one_active ON leases(state) WHERE state = 'active';
      CREATE TABLE IF NOT EXISTS connections (
        connection_id TEXT PRIMARY KEY,
        principal TEXT NOT NULL,
        last_heartbeat_ms INTEGER NOT NULL,
        disconnected_at_ms INTEGER,
        grace_expires_at_ms INTEGER
      );
      CREATE TABLE IF NOT EXISTS operations (
        operation_ref TEXT PRIMARY KEY,
        request_id TEXT NOT NULL,
        principal TEXT NOT NULL,
        task_ref TEXT,
        tool TEXT NOT NULL,
        fingerprint TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        receipt TEXT NOT NULL,
        dispatched INTEGER NOT NULL DEFAULT 0
      );
      CREATE UNIQUE INDEX IF NOT EXISTS operations_begin ON operations(principal, request_id) WHERE tool = 'computer_begin';
      CREATE UNIQUE INDEX IF NOT EXISTS operations_mutation ON operations(principal, task_ref, request_id) WHERE task_ref IS NOT NULL;
      CREATE TABLE IF NOT EXISTS observations (
        observation_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        epoch TEXT,
        captured_at TEXT,
        expired INTEGER NOT NULL DEFAULT 0,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS checks (
        check_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        checked_at TEXT,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS checkpoints (
        checkpoint_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        created_at TEXT NOT NULL,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS artifacts (
        artifact_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS jobs (
        job_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        request_id TEXT,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS capabilities (
        name TEXT PRIMARY KEY,
        record TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS grants (
        group_name TEXT NOT NULL,
        principal TEXT NOT NULL,
        role TEXT NOT NULL CHECK (role = 'agent'),
        state TEXT NOT NULL CHECK (state IN ('active','revoked')),
        peer_key TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY(group_name, principal)
      );
    "#;

/// Core schema 3: the timeline of events, attention items and app notes.
const V3_DDL: &str = r#"
      CREATE TABLE IF NOT EXISTS timeline_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at TEXT NOT NULL,
        kind TEXT NOT NULL,
        task_ref TEXT,
        actor TEXT NOT NULL,
        summary TEXT NOT NULL,
        data TEXT NOT NULL DEFAULT '{}'
      );
      CREATE INDEX IF NOT EXISTS timeline_events_task ON timeline_events(task_ref, id);
      CREATE INDEX IF NOT EXISTS timeline_events_at ON timeline_events(at);
      CREATE TABLE IF NOT EXISTS attention_items (
        att_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        kind TEXT NOT NULL DEFAULT 'question' CHECK (kind IN ('question','approval','login')),
        operation_ref TEXT,
        generation TEXT,
        question TEXT NOT NULL,
        options TEXT NOT NULL DEFAULT '[]',
        state TEXT NOT NULL CHECK (state IN ('open','answered','expired')),
        answer TEXT,
        answered_by TEXT,
        created_at TEXT NOT NULL,
        answered_at TEXT
      );
      CREATE INDEX IF NOT EXISTS attention_items_open ON attention_items(state, task_ref);
      CREATE TABLE IF NOT EXISTS app_notes (
        note_ref TEXT PRIMARY KEY,
        app TEXT NOT NULL,
        version TEXT NOT NULL,
        surface_signature TEXT NOT NULL,
        fact_kind TEXT NOT NULL,
        fact TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        count INTEGER NOT NULL DEFAULT 1,
        UNIQUE(app, version, surface_signature, fact_kind)
      );
      CREATE INDEX IF NOT EXISTS app_notes_updated ON app_notes(updated_at);
    "#;

/// `attention_items` rebuilt so its kind may be `login`: the same columns in
/// the same order (`details` last, as `ALTER TABLE` added it) and every row.
const ATTENTION_WITH_LOGIN: &str = r#"
      CREATE TABLE attention_items_login (
        att_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        kind TEXT NOT NULL DEFAULT 'question' CHECK (kind IN ('question','approval','login')),
        operation_ref TEXT,
        generation TEXT,
        question TEXT NOT NULL,
        options TEXT NOT NULL DEFAULT '[]',
        state TEXT NOT NULL CHECK (state IN ('open','answered','expired')),
        answer TEXT,
        answered_by TEXT,
        created_at TEXT NOT NULL,
        answered_at TEXT,
        details TEXT
      );
      INSERT INTO attention_items_login(att_ref, task_ref, principal, kind, operation_ref, generation, question, options, state, answer, answered_by, created_at, answered_at, details)
        SELECT att_ref, task_ref, principal, kind, operation_ref, generation, question, options, state, answer, answered_by, created_at, answered_at, details FROM attention_items;
      DROP TABLE attention_items;
      ALTER TABLE attention_items_login RENAME TO attention_items;
      CREATE INDEX IF NOT EXISTS attention_items_open ON attention_items(state, task_ref);
    "#;

/// Core schema 6: the windows each task opened.
const V6_DDL: &str = r#"
      CREATE TABLE IF NOT EXISTS task_windows (
        id INTEGER PRIMARY KEY,
        address TEXT NOT NULL UNIQUE,
        task_ref TEXT NOT NULL,
        pid INTEGER NOT NULL,
        class TEXT NOT NULL,
        title TEXT NOT NULL,
        touched INTEGER NOT NULL DEFAULT 0
      );
      CREATE INDEX IF NOT EXISTS task_windows_task ON task_windows(task_ref, id);
    "#;
