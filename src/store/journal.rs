//! The controller journal: a port of `controller/src/journal.ts` over the same
//! `journal.sqlite`. Every public method names the TypeScript line it mirrors.

use super::canonical;
use super::records::*;
use super::schema::{self, MigrationOutcome};
use crate::error::{IbaraError, Result, invalid};
use crate::ids::{id, iso_from_millis, now_millis};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_MAX_METADATA_BYTES: u64 = 100 * 1024 * 1024;
const DEFAULT_METADATA_HEADROOM_BYTES: u64 = 1024 * 1024;
const DEFAULT_METADATA_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const TERMINAL_SETTLED_STATES: &[&str] = &["completed", "cancelled", "partial"];

/// Milliseconds since the epoch; injectable for tests (`JournalOptions.now`).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// `JournalOptions.fingerprintSecret` (journal.ts:14): a `Buffer`, or a string
/// that is hex-decoded when it is exactly 64 characters and UTF-8 otherwise.
#[derive(Clone)]
pub enum FingerprintSecret {
    Bytes(Vec<u8>),
    Text(String),
}

/// `JournalOptions` (journal.ts:13).
#[derive(Clone, Default)]
pub struct JournalOptions {
    pub fingerprint_secret: Option<FingerprintSecret>,
    pub max_metadata_bytes: Option<u64>,
    pub metadata_headroom_bytes: Option<u64>,
    pub metadata_retention_ms: Option<i64>,
    pub now: Option<Clock>,
}

/// Input of `remember_intent` (journal.ts:449-458).
#[derive(Debug, Clone, Copy)]
pub struct RememberIntent<'a> {
    pub principal: &'a str,
    pub request_id: &'a str,
    pub task_ref: Option<&'a str>,
    pub tool: &'a str,
    pub args_fingerprint_source: &'a Value,
    pub receipt: &'a Value,
    pub now_iso: &'a str,
    pub recovery_operation_ref: Option<&'a str>,
    /// One of [`EFFECT_CLASSES`]; the TypeScript had no effect class (`change`).
    pub effect_class: &'a str,
}

/// Result of `remember_intent`: `{ replay }` or `{ fresh }` in the TypeScript.
#[derive(Debug, Clone)]
pub enum Remembered {
    /// The same request was seen before with the same arguments.
    Replay(OperationRecord),
    /// A new intent row was written (`dispatched = 0`).
    Fresh(OperationRecord),
    /// The same `computer_begin` was seen before and failed before creating a
    /// task; the replay answer is the original error.
    FailedBeginReplay { operation: OperationRecord, error: IbaraError },
}

/// The journal. Methods take `&self`; the connection is not shared across threads.
pub struct Journal {
    db: Connection,
    state_dir: PathBuf,
    db_path: PathBuf,
    secret: Vec<u8>,
    max_metadata_bytes: u64,
    metadata_headroom_bytes: u64,
    metadata_retention_ms: i64,
    clock: Clock,
    migration: MigrationOutcome,
}

impl Journal {
    /// `constructor` (journal.ts:170-186): create the state dir 0700, open
    /// `journal.sqlite`, migrate, set pragmas, load the fingerprint secret and
    /// enforce retention.
    pub fn open(state_dir: impl AsRef<Path>, options: JournalOptions) -> Result<Journal> {
        let state_dir = state_dir.as_ref().to_path_buf();
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&state_dir)?;
        let db_path = state_dir.join("journal.sqlite");
        // A new journal is private from the first byte (the unit's UMask=0077 did this for Node).
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&db_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let db = Connection::open(&db_path)?;
        db.busy_timeout(std::time::Duration::from_millis(5000))?;
        let clock = options.now.clone().unwrap_or_else(|| Arc::new(now_millis));
        let migration = schema::migrate(&db, &state_dir.join("storage.sqlite"), &iso_from_millis(clock()))?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        let secret = load_secret(&db, options.fingerprint_secret.as_ref())?;
        let journal = Journal {
            db,
            state_dir,
            db_path,
            secret,
            max_metadata_bytes: options.max_metadata_bytes.unwrap_or(DEFAULT_MAX_METADATA_BYTES).max(1024),
            metadata_headroom_bytes: options.metadata_headroom_bytes.unwrap_or(DEFAULT_METADATA_HEADROOM_BYTES),
            metadata_retention_ms: options.metadata_retention_ms.unwrap_or(DEFAULT_METADATA_RETENTION_MS).max(0),
            clock,
            migration,
        };
        journal.enforce_retention(false)?;
        Ok(journal)
    }

    /// `close` (journal.ts:188).
    pub fn close(self) -> Result<()> {
        self.db.close().map_err(|(_, e)| e.into())
    }

    /// `readonly db` (journal.ts:161), for read queries the engine owns.
    pub fn db(&self) -> &Connection {
        &self.db
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// What `open` migrated (core schema versions, cutover settlements).
    pub fn migration(&self) -> &MigrationOutcome {
        &self.migration
    }

    fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    // ---- fingerprints -------------------------------------------------------

    /// `fingerprint` (journal.ts:192).
    pub fn fingerprint(&self, value: &Value) -> String {
        canonical::fingerprint(&self.secret, value)
    }

    /// `fingerprintsEqual` (journal.ts:196).
    pub fn fingerprints_equal(&self, left: &str, right: &str) -> bool {
        canonical::fingerprints_equal(left, right)
    }

    // ---- epoch and control --------------------------------------------------

    /// `rotateEpoch` (journal.ts:202-231): new epoch; an active lease is revoked
    /// (`controller_restart`) and its task interrupted with human control,
    /// pause and unsettled set; dispatched `running` receipts without a
    /// `job_ref` become `unknown` with `OUTCOME_UNKNOWN`; older observations
    /// expire; and the open questions and approvals of every task that has
    /// ended expire (including those left by a build that kept them).
    pub fn rotate_epoch(&self, now_iso: &str) -> Result<String> {
        let epoch = id("epoch");
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if let Some(active) = self.get_active_lease()? {
            tx.execute(
                "UPDATE tasks SET state = 'interrupted', last_charge_ms = NULL, updated_at = ? WHERE task_ref = ? AND state IN ('active', 'created')",
                params![now_iso, active.task_ref],
            )?;
            tx.execute("UPDATE leases SET state = 'revoked', reason = 'controller_restart' WHERE state = 'active'", [])?;
            // A restart during a lease is the system's pause; a person's pause stays theirs.
            tx.execute(
                "UPDATE control_state SET human_control = 1, paused = 1, unsettled = 1,
                   pause_origin = CASE WHEN pause_origin = 'person' AND (paused = 1 OR human_control = 1) THEN 'person' ELSE 'system' END
                 WHERE id = 1",
                [],
            )?;
        }
        let running: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT operation_ref, receipt FROM operations WHERE dispatched = 1 AND json_extract(receipt, '$.execution') = 'running'",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (operation_ref, text) in running {
            let mut receipt = parse_object(&text)?;
            if receipt.get("job_ref").is_some_and(js_truthy) {
                continue;
            }
            receipt.insert("execution".into(), json!("unknown"));
            receipt.insert("verification".into(), json!("unknown"));
            receipt.insert("effect".into(), json!("unknown"));
            receipt.insert(
                "summary".into(),
                json!("ibara restarted after this step was sent; check what it did before any new step."),
            );
            receipt.insert(
                "error".into(),
                json!({ "code": "OUTCOME_UNKNOWN", "message": "ibara restarted before this step's result was recorded.", "retry_safe": false, "requires_reconciliation": true }),
            );
            tx.execute(
                "UPDATE operations SET receipt = ?, updated_at = ? WHERE operation_ref = ?",
                params![Value::Object(receipt).to_string(), now_iso, operation_ref],
            )?;
        }
        tx.execute("UPDATE observations SET expired = 1 WHERE epoch != ?", [&epoch])?;
        tx.execute("UPDATE control_state SET epoch = ? WHERE id = 1", [&epoch])?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('last_epoch_at', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now_iso],
        )?;
        tx.commit()?;
        self.expire_attention_of_ended_tasks("controller_restart", now_iso)?;
        self.enforce_retention(false)?;
        Ok(epoch)
    }

    /// `getEpoch` (journal.ts:233).
    pub fn get_epoch(&self) -> Result<String> {
        Ok(self.get_control()?.epoch)
    }

    /// `getControl` (journal.ts:237). Columns are read by name (their order differs by machine).
    pub fn get_control(&self) -> Result<ControlState> {
        Ok(self.db.prepare_cached(
            "SELECT human_control, paused, unsettled, settling_generation, epoch, session_hint, pause_origin FROM control_state WHERE id = 1",
        )?
        .query_row([], |r| {
            Ok(ControlState {
                human_control: get_i64(r, "human_control")? != 0,
                paused: get_i64(r, "paused")? != 0,
                unsettled: get_i64(r, "unsettled")? != 0,
                settling_generation: r.get("settling_generation")?,
                epoch: r.get("epoch")?,
                session_hint: get_i64(r, "session_hint")? != 0,
                pause_origin: r.get::<_, Option<String>>("pause_origin")?.as_deref().and_then(PauseOrigin::parse),
            })
        })?)
    }

    /// `setControl` (journal.ts:251).
    pub fn set_control(&self, patch: ControlPatch) -> Result<ControlState> {
        let current = self.get_control()?;
        let next = ControlState {
            human_control: patch.human_control.unwrap_or(current.human_control),
            paused: patch.paused.unwrap_or(current.paused),
            unsettled: patch.unsettled.unwrap_or(current.unsettled),
            settling_generation: patch.settling_generation.unwrap_or(current.settling_generation),
            epoch: patch.epoch.unwrap_or(current.epoch),
            session_hint: patch.session_hint.unwrap_or(current.session_hint),
            pause_origin: patch.pause_origin.unwrap_or(current.pause_origin),
        };
        self.db.prepare_cached(
            "UPDATE control_state SET human_control = ?, paused = ?, unsettled = ?, settling_generation = ?, epoch = ?, session_hint = ?, pause_origin = ? WHERE id = 1",
        )?
        .execute(params![
            next.human_control as i64,
            next.paused as i64,
            next.unsettled as i64,
            next.settling_generation,
            next.epoch,
            next.session_hint as i64,
            next.pause_origin.map(PauseOrigin::as_str)
        ])?;
        Ok(next)
    }

    /// `beginSettlement` (journal.ts:260).
    pub fn begin_settlement(&self, generation: &str) -> Result<ControlState> {
        self.set_control(ControlPatch {
            unsettled: Some(true),
            settling_generation: Some(Some(generation.to_string())),
            ..Default::default()
        })
    }

    /// `completeSettlement` (journal.ts:264): only clears the token it set.
    pub fn complete_settlement(&self, generation: &str, unsettled: bool) -> Result<ControlState> {
        self.db
            .prepare_cached("UPDATE control_state SET unsettled = ?, settling_generation = NULL WHERE id = 1 AND settling_generation = ?")?
            .execute(params![unsettled as i64, generation])?;
        self.get_control()
    }

    // ---- tasks and grants ---------------------------------------------------

    /// `putTask` (journal.ts:270): upsert; `created_at` is kept on update.
    pub fn put_task(&self, task: &TaskRecord) -> Result<()> {
        self.db.prepare_cached(
            "INSERT INTO tasks(
        task_ref, principal, created_at, updated_at, state, goal, success_criteria, budgets, client_flags,
        authorization_ref, required_capabilities, control_started_ms, last_charge_ms, active_control_used_ms,
        actions_used, images_used, last_checkpoint_ref, completion, contract_version, visibility, owner_group, deliveries
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
      ON CONFLICT(task_ref) DO UPDATE SET
        principal = excluded.principal,
        updated_at = excluded.updated_at,
        state = excluded.state,
        goal = excluded.goal,
        success_criteria = excluded.success_criteria,
        budgets = excluded.budgets,
        client_flags = excluded.client_flags,
        authorization_ref = excluded.authorization_ref,
        required_capabilities = excluded.required_capabilities,
        control_started_ms = excluded.control_started_ms,
        last_charge_ms = excluded.last_charge_ms,
        active_control_used_ms = excluded.active_control_used_ms,
        actions_used = excluded.actions_used,
        images_used = excluded.images_used,
        last_checkpoint_ref = excluded.last_checkpoint_ref,
        completion = excluded.completion,
        contract_version = excluded.contract_version,
        visibility = excluded.visibility,
        owner_group = excluded.owner_group,
        deliveries = excluded.deliveries",
        )?
        .execute(params![
            task.task_ref,
            task.principal,
            task.created_at,
            task.updated_at,
            task.state,
            task.goal,
            serde_json::to_string(&task.success_criteria).unwrap_or_else(|_| "[]".into()),
            task.budgets.to_string(),
            if task.client_flags.is_null() { "{}".to_string() } else { task.client_flags.to_string() },
            task.authorization_ref,
            serde_json::to_string(&task.required_capabilities).unwrap_or_else(|_| "[]".into()),
            task.control_started_ms,
            task.last_charge_ms,
            task.active_control_used_ms,
            task.actions_used,
            task.images_used,
            task.last_checkpoint_ref,
            task.completion.as_ref().filter(|c| js_truthy(c)).map(Value::to_string),
            task.contract_version,
            task.visibility,
            task.owner_group,
            serde_json::to_string(&task.deliveries).unwrap_or_else(|_| "[]".into()),
        ])?;
        Ok(())
    }

    /// `getTask` (journal.ts:314).
    pub fn get_task(&self, task_ref: &str) -> Result<Option<TaskRecord>> {
        Ok(self.db.prepare_cached("SELECT * FROM tasks WHERE task_ref = ?")?.query_row([task_ref], task_from_row).optional()?)
    }

    /// The task that finished most recently (completed, partial, cancelled or blocked).
    pub fn last_finished_task(&self) -> Result<Option<TaskRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM tasks WHERE state IN ('completed', 'partial', 'cancelled', 'blocked') ORDER BY updated_at DESC, task_ref DESC LIMIT 1")?
            .query_row([], task_from_row)
            .optional()?)
    }

    /// `listTasks` (journal.ts:321): newest first, `limit` clamped to 1..=500.
    pub fn list_tasks(&self, principal: Option<&str>, limit: i64) -> Result<Vec<TaskRecord>> {
        let bounded = limit.clamp(1, 500);
        let rows = match principal.filter(|p| !p.is_empty()) {
            Some(p) => self
                .db
                .prepare_cached("SELECT * FROM tasks WHERE principal = ? ORDER BY created_at DESC LIMIT ?")?
                .query_map(params![p, bounded], task_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .db
                .prepare_cached("SELECT * FROM tasks ORDER BY created_at DESC LIMIT ?")?
                .query_map([bounded], task_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// `setTaskSharing` (journal.ts:329).
    pub fn set_task_sharing(&self, task_ref: &str, visibility: &str, owner_group: Option<&str>, now_iso: &str) -> Result<TaskRecord> {
        self.db
            .prepare_cached("UPDATE tasks SET visibility = ?, owner_group = ?, updated_at = ? WHERE task_ref = ?")?
            .execute(params![visibility, owner_group, now_iso, task_ref])?;
        self.get_task(task_ref)?.ok_or_else(|| invalid("Unknown task_ref."))
    }

    /// The task if `principal` may read it (`requireReadableTask`, core.ts:954-961,
    /// without the identifier check): owner, or an active grant in its shared group.
    pub fn readable_task(&self, principal: &str, task_ref: &str) -> Result<Option<TaskRecord>> {
        let Some(task) = self.get_task(task_ref)? else { return Ok(None) };
        let grants = self.list_grants(task.owner_group.as_deref())?;
        Ok(may_read_shared_task(&task, principal, &grants).then_some(task))
    }

    /// `putGrant` (journal.ts:336).
    pub fn put_grant(&self, grant: &GrantRecord) -> Result<()> {
        self.db.prepare_cached(
            "INSERT INTO grants(group_name, principal, role, state, peer_key, updated_at) VALUES (?, ?, ?, ?, ?, ?)
      ON CONFLICT(group_name, principal) DO UPDATE SET role=excluded.role, state=excluded.state, peer_key=excluded.peer_key, updated_at=excluded.updated_at",
        )?
        .execute(params![grant.group, grant.principal, grant.role, grant.state, grant.peer_key, grant.updated_at])?;
        Ok(())
    }

    /// `listGrants` (journal.ts:342). An empty or absent group lists all grants.
    pub fn list_grants(&self, group: Option<&str>) -> Result<Vec<GrantRecord>> {
        let map = |r: &Row<'_>| {
            Ok(GrantRecord {
                group: r.get("group_name")?,
                principal: r.get("principal")?,
                role: r.get("role")?,
                state: r.get("state")?,
                peer_key: r.get("peer_key")?,
                updated_at: r.get("updated_at")?,
            })
        };
        Ok(match group.filter(|g| !g.is_empty()) {
            Some(g) => self.db.prepare_cached("SELECT * FROM grants WHERE group_name = ?")?.query_map([g], map)?.collect::<rusqlite::Result<_>>()?,
            None => self.db.prepare_cached("SELECT * FROM grants")?.query_map([], map)?.collect::<rusqlite::Result<_>>()?,
        })
    }

    // ---- meta ---------------------------------------------------------------

    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.prepare_cached("SELECT value FROM meta WHERE key = ?")?.query_row([key], |r| r.get(0)).optional()?)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db
            .prepare_cached("INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")?
            .execute(params![key, value])?;
        Ok(())
    }

    /// `endpointIdentity` (journal.ts:349): `ibara_<32hex>`, created once and never changed.
    pub fn endpoint_identity(&self) -> Result<String> {
        if let Some(value) = self.meta("endpoint_identity")? {
            return Ok(value);
        }
        let identity = id("ibara");
        self.db.execute("INSERT INTO meta(key, value) VALUES('endpoint_identity', ?)", [&identity])?;
        Ok(identity)
    }

    /// `collectorIdentity` (journal.ts:357).
    pub fn collector_identity(&self, principal: &str) -> Result<Option<String>> {
        self.meta(&format!("collector:{principal}"))
    }

    /// `registerCollector` (journal.ts:361).
    pub fn register_collector(&self, principal: &str, identity: &str) -> Result<()> {
        self.set_meta(&format!("collector:{principal}"), identity)
    }

    /// `appendAudit` (journal.ts:577): read-modify-write of `meta['audit:<key>']`.
    pub fn append_audit(&self, key: &str, record: Value) -> Result<Vec<Value>> {
        let mut rows = self.read_audit(key)?;
        rows.push(record);
        self.set_meta(&format!("audit:{key}"), &Value::Array(rows.clone()).to_string())?;
        Ok(rows)
    }

    /// `readAudit` (journal.ts:584).
    pub fn read_audit(&self, key: &str) -> Result<Vec<Value>> {
        match self.meta(&format!("audit:{key}"))? {
            None => Ok(Vec::new()),
            Some(text) => match parse_json(Some(&text), json!([]))? {
                Value::Array(items) => Ok(items),
                _ => Ok(Vec::new()),
            },
        }
    }

    // ---- operations ---------------------------------------------------------

    /// `listOperationsForTask` (journal.ts:365): newest first, paged by operation_ref cursor.
    /// Paged in SQL rather than in memory; same ordering and cursor semantics.
    pub fn list_operations_for_task(&self, task_ref: &str, limit: i64, cursor: Option<&str>) -> Result<Page<OperationRecord>> {
        let bounded = if limit <= 0 { 20 } else { limit.min(100) };
        let total: i64 = self.db.prepare_cached("SELECT COUNT(*) FROM operations WHERE task_ref = ?")?.query_row([task_ref], |r| r.get(0))?;
        let mut items = match cursor.filter(|c| !c.is_empty()) {
            None => self
                .db
                .prepare_cached("SELECT * FROM operations WHERE task_ref = ? ORDER BY created_at DESC, operation_ref DESC LIMIT ?")?
                .query_map(params![task_ref, bounded + 1], operation_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            Some(c) => {
                let anchor: Option<String> = self
                    .db
                    .prepare_cached("SELECT created_at FROM operations WHERE task_ref = ? AND operation_ref = ?")?
                    .query_row(params![task_ref, c], |r| r.get(0))
                    .optional()?;
                match anchor {
                    None => Vec::new(),
                    Some(at) => self
                        .db
                        .prepare_cached(
                            "SELECT * FROM operations WHERE task_ref = ?1 AND (created_at < ?2 OR (created_at = ?2 AND operation_ref < ?3))
                             ORDER BY created_at DESC, operation_ref DESC LIMIT ?4",
                        )?
                        .query_map(params![task_ref, at, c, bounded + 1], operation_from_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                }
            }
        };
        let more = items.len() as i64 > bounded;
        items.truncate(bounded as usize);
        let next_cursor = if more { items.last().map(|o| o.operation_ref.clone()) } else { None };
        Ok(Page { items, next_cursor, total })
    }

    /// `getOperationByRequestId` (journal.ts:372): only an unambiguous match.
    pub fn get_operation_by_request_id(&self, request_id: &str, task_ref: Option<&str>) -> Result<Option<OperationRecord>> {
        let rows = match task_ref.filter(|t| !t.is_empty()) {
            Some(t) => self
                .db
                .prepare_cached("SELECT * FROM operations WHERE request_id = ? AND task_ref = ? ORDER BY created_at DESC LIMIT 2")?
                .query_map(params![request_id, t], operation_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .db
                .prepare_cached("SELECT * FROM operations WHERE request_id = ? ORDER BY created_at DESC LIMIT 2")?
                .query_map([request_id], operation_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(if rows.len() == 1 { rows.into_iter().next() } else { None })
    }

    /// `rememberIntent` (journal.ts:449-520): replay an identical request, refuse a
    /// changed one with `REQUEST_CONFLICT`, otherwise persist the intent before any effect.
    pub fn remember_intent(&self, input: RememberIntent<'_>) -> Result<Remembered> {
        if !EFFECT_CLASSES.contains(&input.effect_class) {
            return Err(invalid(format!("Unknown effect class {:?}.", input.effect_class)));
        }
        let fingerprint = self.fingerprint(&json!({ "tool": input.tool, "args": input.args_fingerprint_source }));
        let existing = match input.recovery_operation_ref {
            Some(op) => self.get_operation_by_ref(op)?,
            None if input.tool == "computer_begin" => self.get_begin_operation(input.principal, input.request_id)?,
            None => self.get_mutation_operation(input.principal, input.task_ref.unwrap_or(""), input.request_id)?,
        };
        if input.recovery_operation_ref.is_some() && existing.is_none() {
            return Err(IbaraError::new("REQUEST_CONFLICT", "Canonical operation recovery identity is unknown.", false)
                .requires_reconciliation()
                .with("recovery", "Inspect the retained operation list and use the exact original operation_ref; do not dispatch a new effect."));
        }
        if let Some(existing) = existing {
            if input.recovery_operation_ref.is_some() && (existing.task_ref.as_deref() != input.task_ref || existing.tool != input.tool) {
                return Err(IbaraError::new("REQUEST_CONFLICT", "Canonical operation recovery does not match this task or tool.", false)
                    .requires_reconciliation()
                    .with("recovery", "Inspect the original operation_ref without dispatching a new effect."));
            }
            // Older mutation receipts fingerprinted request_id with the effect
            // arguments (journal.ts:478-486). Recover that form with the retained ID.
            let legacy = match (input.args_fingerprint_source, input.task_ref) {
                (Value::Object(args), Some(task_ref)) if args.get("task_ref").and_then(Value::as_str) == Some(task_ref) => {
                    let mut legacy = args.clone();
                    legacy.insert("request_id".into(), json!(existing.request_id));
                    Some(self.fingerprint(&json!({ "tool": input.tool, "args": Value::Object(legacy) })))
                }
                _ => None,
            };
            let matches = self.fingerprints_equal(&existing.fingerprint, &fingerprint)
                || legacy.is_some_and(|l| self.fingerprints_equal(&existing.fingerprint, &l));
            if !matches {
                let recovery = if input.recovery_operation_ref.is_some() {
                    "Inspect the original operation_ref and its exact arguments; do not dispatch a new effect to recover it."
                } else {
                    "Issue a new request_id for a new intention."
                };
                return Err(IbaraError::new("REQUEST_CONFLICT", "The same request ID was reused with different arguments.", false)
                    .with("requires_reconciliation", input.recovery_operation_ref.is_some())
                    .with("recovery", recovery));
            }
            if let Some(error) = existing.failed_begin_error() {
                return Ok(Remembered::FailedBeginReplay { operation: existing, error });
            }
            return Ok(Remembered::Replay(existing));
        }
        self.enforce_retention(true)?;
        let operation = OperationRecord {
            operation_ref: id("op"),
            request_id: input.request_id.to_string(),
            principal: input.principal.to_string(),
            task_ref: input.task_ref.map(str::to_string),
            tool: input.tool.to_string(),
            fingerprint,
            created_at: input.now_iso.to_string(),
            updated_at: input.now_iso.to_string(),
            receipt: input.receipt.clone(),
            dispatched: false,
            effect_class: input.effect_class.to_string(),
        };
        self.db
            .prepare_cached(
                "INSERT INTO operations(operation_ref, request_id, principal, task_ref, tool, fingerprint, created_at, updated_at, receipt, dispatched, effect_class)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
            )?
            .execute(params![
                operation.operation_ref,
                operation.request_id,
                operation.principal,
                operation.task_ref,
                operation.tool,
                operation.fingerprint,
                operation.created_at,
                operation.updated_at,
                operation.receipt.to_string(),
                operation.effect_class,
            ])
            .map_err(map_capacity_error)?;
        Ok(Remembered::Fresh(operation))
    }

    /// `getBeginOperation` (journal.ts:522).
    pub fn get_begin_operation(&self, principal: &str, request_id: &str) -> Result<Option<OperationRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM operations WHERE principal = ? AND request_id = ? AND tool = 'computer_begin'")?
            .query_row(params![principal, request_id], operation_from_row)
            .optional()?)
    }

    /// `getMutationOperation` (journal.ts:529).
    pub fn get_mutation_operation(&self, principal: &str, task_ref: &str, request_id: &str) -> Result<Option<OperationRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM operations WHERE principal = ? AND task_ref = ? AND request_id = ?")?
            .query_row(params![principal, task_ref, request_id], operation_from_row)
            .optional()?)
    }

    /// `getOperationByRef` (journal.ts:536).
    pub fn get_operation_by_ref(&self, operation_ref: &str) -> Result<Option<OperationRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM operations WHERE operation_ref = ?")?
            .query_row([operation_ref], operation_from_row)
            .optional()?)
    }

    /// `getOperationByRequest` (journal.ts:541).
    pub fn get_operation_by_request(&self, principal: &str, request_id: &str, task_ref: Option<&str>) -> Result<Option<OperationRecord>> {
        if let Some(t) = task_ref.filter(|t| !t.is_empty()) {
            if let Some(op) = self.get_mutation_operation(principal, t, request_id)? {
                return Ok(Some(op));
            }
            return self.get_begin_operation(principal, request_id);
        }
        Ok(self
            .db
            .prepare_cached("SELECT * FROM operations WHERE principal = ? AND request_id = ? ORDER BY created_at DESC LIMIT 1")?
            .query_row(params![principal, request_id], operation_from_row)
            .optional()?)
    }

    /// `updateOperation` (journal.ts:551).
    pub fn update_operation(&self, operation_ref: &str, patch: OperationPatch) -> Result<OperationRecord> {
        let Some(current) = self.get_operation_by_ref(operation_ref)? else {
            return Err(IbaraError::new("INTERNAL_ERROR", "Missing operation while updating receipt.", false).requires_reconciliation());
        };
        let next = OperationRecord {
            receipt: patch.receipt.unwrap_or(current.receipt.clone()),
            dispatched: patch.dispatched.unwrap_or(current.dispatched),
            task_ref: patch.task_ref.unwrap_or(current.task_ref.clone()),
            updated_at: patch.now_iso,
            ..current
        };
        self.db
            .prepare_cached("UPDATE operations SET receipt = ?, dispatched = ?, task_ref = ?, updated_at = ? WHERE operation_ref = ?")?
            .execute(params![next.receipt.to_string(), next.dispatched as i64, next.task_ref, next.updated_at, operation_ref])?;
        Ok(next)
    }

    /// `unresolvedRequestIds` (journal.ts:567): distinct, in first-seen order.
    pub fn unresolved_request_ids(&self, task_ref: &str) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Vec::new();
        for op in self.unresolved_operations(task_ref)? {
            if !ids.contains(&op.request_id) {
                ids.push(op.request_id);
            }
        }
        Ok(ids)
    }

    /// `unresolvedOperations` (journal.ts:571): `execution ∈ {running, unknown}` or
    /// `verification = pending`, oldest first. Filtered in SQL.
    pub fn unresolved_operations(&self, task_ref: &str) -> Result<Vec<OperationRecord>> {
        Ok(self
            .db
            .prepare_cached(
                "SELECT * FROM operations WHERE task_ref = ? AND (json_extract(receipt, '$.execution') IN ('running', 'unknown') OR json_extract(receipt, '$.verification') = 'pending')
                 ORDER BY created_at ASC, operation_ref ASC",
            )?
            .query_map([task_ref], operation_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// `MAX(created_at)` of dispatched effects other than begin/finish (core.ts:1964).
    pub fn last_effect_at(&self, task_ref: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .prepare_cached(
                "SELECT MAX(created_at) AS at FROM operations WHERE task_ref = ? AND dispatched = 1 AND tool NOT IN ('computer_begin', 'computer_finish') AND json_extract(receipt, '$.effect') != 'none'",
            )?
            .query_row([task_ref], |r| r.get(0))?)
    }

    // ---- leases and connections ---------------------------------------------

    /// `putLease` (journal.ts:397): upsert; an update changes only heartbeat, expiry, state, reason and connection.
    pub fn put_lease(&self, lease: &LeaseRecord) -> Result<()> {
        self.db.prepare_cached(
            "INSERT INTO leases(generation, task_ref, principal, connection_id, epoch, acquired_at, last_heartbeat_at, last_heartbeat_ms, idle_expires_at_ms, state, reason)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(generation) DO UPDATE SET
         last_heartbeat_at = excluded.last_heartbeat_at,
         last_heartbeat_ms = excluded.last_heartbeat_ms,
         idle_expires_at_ms = excluded.idle_expires_at_ms,
         state = excluded.state,
         reason = excluded.reason,
         connection_id = excluded.connection_id",
        )?
        .execute(params![
            lease.generation,
            lease.task_ref,
            lease.principal,
            lease.connection_id,
            lease.epoch,
            lease.acquired_at,
            lease.last_heartbeat_at,
            lease.last_heartbeat_ms,
            lease.idle_expires_at_ms,
            lease.state,
            lease.reason
        ])?;
        Ok(())
    }

    /// `getLease` (journal.ts:411).
    pub fn get_lease(&self, generation: &str) -> Result<Option<LeaseRecord>> {
        Ok(self.db.prepare_cached("SELECT * FROM leases WHERE generation = ?")?.query_row([generation], lease_from_row).optional()?)
    }

    /// `getActiveLease` (journal.ts:415).
    pub fn get_active_lease(&self) -> Result<Option<LeaseRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM leases WHERE state = 'active' ORDER BY acquired_at DESC LIMIT 1")?
            .query_row([], lease_from_row)
            .optional()?)
    }

    /// `listLeasesForTask` (journal.ts:419).
    pub fn list_leases_for_task(&self, task_ref: &str) -> Result<Vec<LeaseRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM leases WHERE task_ref = ? ORDER BY acquired_at DESC")?
            .query_map([task_ref], lease_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// `revokeActiveLeases` (journal.ts:423).
    pub fn revoke_active_leases(&self, reason: &str, at_ms: i64) -> Result<Vec<LeaseRecord>> {
        let active: Vec<LeaseRecord> = self
            .db
            .prepare_cached("SELECT * FROM leases WHERE state = 'active'")?
            .query_map([], lease_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        self.db.prepare_cached("UPDATE leases SET state = 'revoked', reason = ? WHERE state = 'active'")?.execute([reason])?;
        Ok(active
            .into_iter()
            .map(|lease| LeaseRecord { state: "revoked".into(), reason: Some(reason.to_string()), last_heartbeat_ms: at_ms, ..lease })
            .collect())
    }

    /// `expireLease` (journal.ts:429).
    pub fn expire_lease(&self, generation: &str, reason: &str) -> Result<()> {
        self.db
            .prepare_cached("UPDATE leases SET state = 'expired', reason = ? WHERE generation = ? AND state = 'active'")?
            .execute(params![reason, generation])?;
        Ok(())
    }

    /// `putConnection` (journal.ts:433).
    pub fn put_connection(&self, conn: &ConnectionRecord) -> Result<()> {
        self.db.prepare_cached(
            "INSERT INTO connections(connection_id, principal, last_heartbeat_ms, disconnected_at_ms, grace_expires_at_ms)
       VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(connection_id) DO UPDATE SET
         principal = excluded.principal,
         last_heartbeat_ms = excluded.last_heartbeat_ms,
         disconnected_at_ms = excluded.disconnected_at_ms,
         grace_expires_at_ms = excluded.grace_expires_at_ms",
        )?
        .execute(params![conn.connection_id, conn.principal, conn.last_heartbeat_ms, conn.disconnected_at_ms, conn.grace_expires_at_ms])?;
        Ok(())
    }

    /// `getConnection` (journal.ts:445).
    pub fn get_connection(&self, connection_id: &str) -> Result<Option<ConnectionRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM connections WHERE connection_id = ?")?
            .query_row([connection_id], |r| {
                Ok(ConnectionRecord {
                    connection_id: r.get("connection_id")?,
                    principal: r.get("principal")?,
                    last_heartbeat_ms: get_i64(r, "last_heartbeat_ms")?,
                    disconnected_at_ms: get_opt_i64(r, "disconnected_at_ms")?,
                    grace_expires_at_ms: get_opt_i64(r, "grace_expires_at_ms")?,
                })
            })
            .optional()?)
    }

    // ---- evidence records ---------------------------------------------------

    /// `putObservation` (journal.ts:589). Records without an `observation_ref` are ignored.
    pub fn put_observation(&self, task_ref: &str, principal: &str, record: &Value, expired: bool) -> Result<()> {
        let observation_ref = js_string_or_empty(record.get("observation_ref"));
        if observation_ref.is_empty() {
            return Ok(());
        }
        self.db
            .prepare_cached(
                "INSERT INTO observations(observation_ref, task_ref, principal, epoch, captured_at, expired, record)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(observation_ref) DO UPDATE SET expired = excluded.expired, record = excluded.record",
            )?
            .execute(params![
                observation_ref,
                task_ref,
                principal,
                json_to_sql(record.get("epoch")),
                json_to_sql(record.get("captured_at")),
                expired as i64,
                record.to_string()
            ])?;
        Ok(())
    }

    /// `getObservation` (journal.ts:599).
    pub fn get_observation(&self, observation_ref: &str) -> Result<Option<StoredRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM observations WHERE observation_ref = ?")?
            .query_row([observation_ref], |r| {
                Ok(StoredRecord {
                    reference: r.get("observation_ref")?,
                    kind: "observation".into(),
                    task_ref: r.get("task_ref")?,
                    principal: r.get("principal")?,
                    epoch: r.get("epoch")?,
                    expired: get_i64(r, "expired")? != 0,
                    record: json_col(r, "record", json!({}))?,
                })
            })
            .optional()?)
    }

    /// `listObservationRefs` (journal.ts:613).
    pub fn list_observation_refs(&self, task_ref: &str, limit: i64) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare_cached("SELECT observation_ref FROM observations WHERE task_ref = ? ORDER BY captured_at DESC LIMIT ?")?
            .query_map(params![task_ref, limit], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// `expireDecisionFrames` (journal.ts:619).
    pub fn expire_decision_frames(&self, task_ref: &str) -> Result<()> {
        self.db
            .prepare_cached("UPDATE observations SET expired = 1 WHERE task_ref = ? AND json_extract(record, '$.kind') = 'decision_frame'")?
            .execute([task_ref])?;
        Ok(())
    }

    /// `putCheck` (journal.ts:623).
    pub fn put_check(&self, task_ref: &str, principal: &str, record: &Value) -> Result<()> {
        let check_ref = js_string_or_empty(record.get("check_ref"));
        if check_ref.is_empty() {
            return Ok(());
        }
        self.db
            .prepare_cached(
                "INSERT INTO checks(check_ref, task_ref, principal, checked_at, record)
       VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(check_ref) DO UPDATE SET record = excluded.record, checked_at = excluded.checked_at",
            )?
            .execute(params![check_ref, task_ref, principal, json_to_sql(record.get("checked_at")), record.to_string()])?;
        Ok(())
    }

    /// `getCheck` (journal.ts:633). Checks never expire.
    pub fn get_check(&self, check_ref: &str) -> Result<Option<StoredRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM checks WHERE check_ref = ?")?
            .query_row([check_ref], |r| stored_simple(r, "check_ref", "check"))
            .optional()?)
    }

    /// `listChecks` (journal.ts:639).
    pub fn list_checks(&self, task_ref: &str) -> Result<Vec<Value>> {
        self.records("SELECT record FROM checks WHERE task_ref = ? ORDER BY checked_at DESC", task_ref)
    }

    /// `putCheckpoint` (journal.ts:643). `created_at` uses the journal clock
    /// (the TypeScript used the wall clock; they are the same in production).
    pub fn put_checkpoint(&self, record: &Value) -> Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO checkpoints(checkpoint_ref, task_ref, created_at, record) VALUES (?, ?, ?, ?)
       ON CONFLICT(checkpoint_ref) DO UPDATE SET record = excluded.record",
            )?
            .execute(params![
                json_to_sql(record.get("checkpoint_ref")),
                json_to_sql(record.get("task_ref")),
                iso_from_millis(self.now_ms()),
                record.to_string()
            ])?;
        Ok(())
    }

    /// `getCheckpoint` (journal.ts:650).
    pub fn get_checkpoint(&self, checkpoint_ref: &str) -> Result<Option<Value>> {
        let text: Option<String> =
            self.db.prepare_cached("SELECT record FROM checkpoints WHERE checkpoint_ref = ?")?.query_row([checkpoint_ref], |r| r.get(0)).optional()?;
        text.map(|t| parse_json(Some(&t), Value::Null)).transpose()
    }

    /// `latestCheckpoint` (journal.ts:655).
    pub fn latest_checkpoint(&self, task_ref: &str) -> Result<Option<Value>> {
        let text: Option<String> = self
            .db
            .prepare_cached("SELECT record FROM checkpoints WHERE task_ref = ? ORDER BY created_at DESC LIMIT 1")?
            .query_row([task_ref], |r| r.get(0))
            .optional()?;
        text.map(|t| parse_json(Some(&t), Value::Null)).transpose()
    }

    /// `putArtifact` (journal.ts:660): a snapshot of the storage record.
    pub fn put_artifact(&self, task_ref: &str, principal: &str, record: &Value) -> Result<()> {
        let artifact_ref = js_string_or_empty(record.get("artifact_ref"));
        if artifact_ref.is_empty() {
            return Ok(());
        }
        self.db
            .prepare_cached(
                "INSERT INTO artifacts(artifact_ref, task_ref, principal, record) VALUES (?, ?, ?, ?)
       ON CONFLICT(artifact_ref) DO UPDATE SET record = excluded.record",
            )?
            .execute(params![artifact_ref, task_ref, principal, record.to_string()])?;
        Ok(())
    }

    /// `getArtifact` (journal.ts:669).
    pub fn get_artifact(&self, artifact_ref: &str) -> Result<Option<StoredRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM artifacts WHERE artifact_ref = ?")?
            .query_row([artifact_ref], |r| stored_simple(r, "artifact_ref", "artifact"))
            .optional()?)
    }

    /// `listArtifacts` (journal.ts:675).
    pub fn list_artifacts(&self, task_ref: &str) -> Result<Vec<Value>> {
        self.records("SELECT record FROM artifacts WHERE task_ref = ?", task_ref)
    }

    /// `putJob` (journal.ts:679): the journal's mirror of a storage job.
    pub fn put_job(&self, task_ref: &str, principal: &str, record: &Value) -> Result<()> {
        let job_ref = js_string_or_empty(record.get("job_ref"));
        if job_ref.is_empty() {
            return Ok(());
        }
        self.db
            .prepare_cached(
                "INSERT INTO jobs(job_ref, task_ref, principal, request_id, record) VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(job_ref) DO UPDATE SET record = excluded.record, request_id = excluded.request_id",
            )?
            .execute(params![job_ref, task_ref, principal, json_to_sql(record.get("request_id")), record.to_string()])?;
        Ok(())
    }

    /// `getJob` (journal.ts:688).
    pub fn get_job(&self, job_ref: &str) -> Result<Option<StoredRecord>> {
        Ok(self
            .db
            .prepare_cached("SELECT * FROM jobs WHERE job_ref = ?")?
            .query_row([job_ref], |r| stored_simple(r, "job_ref", "job"))
            .optional()?)
    }

    /// `listJobs` (journal.ts:694).
    pub fn list_jobs(&self, task_ref: &str) -> Result<Vec<Value>> {
        self.records("SELECT record FROM jobs WHERE task_ref = ?", task_ref)
    }

    /// `putCapabilities` (journal.ts:698): replaces the whole table.
    pub fn put_capabilities(&self, records: &[Value]) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM capabilities", [])?;
        {
            let mut insert = tx.prepare("INSERT INTO capabilities(name, record) VALUES (?, ?)")?;
            for record in records {
                let mut name = js_string_or_empty(record.get("name"));
                if name.is_empty() {
                    name = id("cap");
                }
                insert.execute(params![name, record.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `listCapabilities` (journal.ts:707).
    pub fn list_capabilities(&self) -> Result<Vec<Value>> {
        let texts: Vec<String> =
            self.db.prepare_cached("SELECT record FROM capabilities")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        texts.iter().map(|t| parse_json(Some(t), json!({}))).collect()
    }

    /// `lookupEvidence` (journal.ts:711): observations → checks → artifacts → jobs → operations.
    pub fn lookup_evidence(&self, reference: &str) -> Result<Option<StoredRecord>> {
        if let Some(r) = self.get_observation(reference)? {
            return Ok(Some(r));
        }
        if let Some(r) = self.get_check(reference)? {
            return Ok(Some(r));
        }
        if let Some(r) = self.get_artifact(reference)? {
            return Ok(Some(r));
        }
        if let Some(r) = self.get_job(reference)? {
            return Ok(Some(r));
        }
        Ok(self.get_operation_by_ref(reference)?.map(|op| StoredRecord {
            reference: op.operation_ref,
            kind: "receipt".into(),
            task_ref: op.task_ref,
            principal: Some(op.principal),
            epoch: op.receipt.get("epoch").and_then(Value::as_str).map(str::to_string),
            expired: false,
            record: op.receipt,
        }))
    }

    fn records(&self, sql: &str, key: &str) -> Result<Vec<Value>> {
        let texts: Vec<String> = self.db.prepare_cached(sql)?.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        texts.iter().map(|t| parse_json(Some(t), json!({}))).collect()
    }

    // ---- capacity and retention ---------------------------------------------

    /// `metadataBytes` (journal.ts:723): `journal.sqlite` and `storage.sqlite` with their `-wal`/`-shm`.
    pub fn metadata_bytes(&self) -> Result<u64> {
        let mut total = 0;
        for base in [self.db_path.clone(), self.state_dir.join("storage.sqlite")] {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = base.clone().into_os_string();
                path.push(suffix);
                total += file_size(Path::new(&path))?;
            }
        }
        Ok(total)
    }

    /// `enforceRetention` (journal.ts:727-741): prune settled terminal tasks and old
    /// timeline rows, then enforce the metadata cap. For a new intent the cap
    /// keeps headroom and an overflow is `BUDGET_EXCEEDED`.
    pub fn enforce_retention(&self, for_new_intent: bool) -> Result<()> {
        let deleted = self.prune_terminal_settled()?;
        if deleted > 0 {
            self.checkpoint_wal()?;
        }
        let limit = if for_new_intent { self.intent_limit() } else { self.max_metadata_bytes };
        if self.metadata_bytes()? <= limit {
            return Ok(());
        }
        if deleted > 0 {
            self.db.execute_batch("VACUUM")?;
            self.checkpoint_wal()?;
        }
        if for_new_intent && self.metadata_bytes()? > limit {
            return Err(capacity_exhausted());
        }
        Ok(())
    }

    fn intent_limit(&self) -> u64 {
        let headroom = self.metadata_headroom_bytes.min(self.max_metadata_bytes.saturating_sub(1));
        self.max_metadata_bytes - headroom
    }

    /// `pruneTerminalSettled` (journal.ts:748-784), plus the timeline tables:
    /// events older than the cutoff (except those of retained tasks),
    /// settled attention items, and the event row cap.
    fn prune_terminal_settled(&self) -> Result<usize> {
        let cutoff = iso_from_millis(self.now_ms() - self.metadata_retention_ms);
        let tasks: Vec<(String, String, String)> = self
            .db
            .prepare_cached("SELECT task_ref, state, updated_at FROM tasks")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let prunable: Vec<String> = tasks
            .into_iter()
            .filter(|(_, state, updated_at)| TERMINAL_SETTLED_STATES.contains(&state.as_str()) && updated_at.as_str() <= cutoff.as_str())
            .map(|(task_ref, _, _)| task_ref)
            .collect();
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let mut deleted = 0usize;
        for task_ref in &prunable {
            let jobs: Vec<(String, String)> = tx
                .prepare_cached("SELECT job_ref, record FROM jobs WHERE task_ref = ?")?
                .query_map([task_ref], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let ops: Vec<(String, String, i64)> = tx
                .prepare_cached("SELECT operation_ref, receipt, dispatched FROM operations WHERE task_ref = ?")?
                .query_map([task_ref], |r| Ok((r.get(0)?, r.get(1)?, get_i64(r, "dispatched")?)))?
                .collect::<rusqlite::Result<_>>()?;
            let job_keep: Vec<bool> = jobs.iter().map(|(_, t)| parse_json(Some(t), json!({})).map(|v| job_needs_retention(&v))).collect::<Result<_>>()?;
            let op_keep: Vec<bool> =
                ops.iter().map(|(_, t, d)| parse_json(Some(t), json!({})).map(|v| receipt_needs_retention(&v, *d != 0))).collect::<Result<_>>()?;
            // Unknown receipts may rely on old observations/checks to reconcile:
            // keep the task's entire evidence set until every operation is settled.
            if job_keep.iter().any(|k| *k) || op_keep.iter().any(|k| *k) {
                continue;
            }
            deleted += tx.execute("DELETE FROM observations WHERE task_ref = ?", [task_ref])?;
            deleted += tx.execute("DELETE FROM checks WHERE task_ref = ?", [task_ref])?;
            deleted += tx.execute("DELETE FROM checkpoints WHERE task_ref = ?", [task_ref])?;
            for (job_ref, _) in &jobs {
                deleted += tx.execute("DELETE FROM jobs WHERE job_ref = ?", [job_ref])?;
            }
            for (operation_ref, _, _) in &ops {
                deleted += tx.execute("DELETE FROM operations WHERE operation_ref = ?", [operation_ref])?;
            }
            deleted += tx.execute("DELETE FROM timeline_events WHERE task_ref = ? AND at <= ?", params![task_ref, cutoff])?;
        }
        deleted += super::timeline::prune(&tx, &cutoff)?;
        tx.commit()?;
        Ok(deleted)
    }

    fn checkpoint_wal(&self) -> Result<()> {
        self.db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }
}

// ---- helpers ------------------------------------------------------------------

/// `loadSecret` (journal.ts:947-957): the stored secret wins; otherwise the
/// configured one (64-char strings are hex); otherwise 32 random bytes. Persisted as hex.
fn load_secret(db: &Connection, provided: Option<&FingerprintSecret>) -> Result<Vec<u8>> {
    let existing: Option<String> = db.query_row("SELECT value FROM meta WHERE key = 'fingerprint_secret'", [], |r| r.get(0)).optional()?;
    if let Some(value) = existing {
        return Ok(canonical::node_hex_decode(&value));
    }
    let secret = match provided {
        None => {
            let mut bytes = vec![0u8; 32];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            bytes
        }
        Some(FingerprintSecret::Bytes(b)) => b.clone(),
        Some(FingerprintSecret::Text(t)) if t.encode_utf16().count() == 64 => canonical::node_hex_decode(t),
        Some(FingerprintSecret::Text(t)) => t.as_bytes().to_vec(),
    };
    db.execute("INSERT INTO meta(key, value) VALUES('fingerprint_secret', ?)", [canonical::hex_encode(&secret)])?;
    Ok(secret)
}

fn capacity_exhausted() -> IbaraError {
    IbaraError::new("BUDGET_EXCEEDED", "Journal metadata capacity is exhausted.", true).with(
        "recovery",
        "Status and emergency control remain available. Inspect retained receipts; do not dispatch a new effect until metadata is below the cap.",
    )
}

/// `mapCapacityError` (journal.ts:794): a full disk is `BUDGET_EXCEEDED`.
fn map_capacity_error(err: rusqlite::Error) -> IbaraError {
    if let rusqlite::Error::SqliteFailure(e, _) = &err
        && (e.code == rusqlite::ErrorCode::DiskFull || e.extended_code == rusqlite::ffi::SQLITE_IOERR_WRITE)
    {
        return capacity_exhausted();
    }
    err.into()
}

fn file_size(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// `receiptNeedsRetention` (journal.ts:144).
pub(crate) fn receipt_needs_retention(receipt: &Value, dispatched: bool) -> bool {
    let execution = js_string_or_empty(receipt.get("execution"));
    let verification = js_string_or_empty(receipt.get("verification"));
    let effect = js_string_or_empty(receipt.get("effect"));
    if execution == "unknown" || execution == "running" {
        return true;
    }
    if verification == "unknown" || verification == "pending" {
        return true;
    }
    if effect == "unknown" {
        return true;
    }
    dispatched && execution != "completed" && execution != "not_started"
}

/// `jobNeedsRetention` (journal.ts:155).
pub(crate) fn job_needs_retention(record: &Value) -> bool {
    let state = js_string_or_empty(record.get("state"));
    state == "unknown" || state == "running" || record.get("termination_confirmed") == Some(&Value::Bool(false))
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `String(value || '')`.
pub(crate) fn js_string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if js_truthy(v) => match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => {
                let mut s = String::new();
                let _ = canonical::write_js_number(n.as_f64().unwrap_or(0.0), &mut s);
                s
            }
            Value::Bool(_) => "true".into(),
            Value::Array(_) => v.as_array().map(|a| a.iter().map(|x| js_string_or_empty(Some(x))).collect::<Vec<_>>().join(",")).unwrap_or_default(),
            _ => "[object Object]".into(),
        },
        _ => String::new(),
    }
}

/// Bind a JSON field the way better-sqlite3 bound the JS value (`?? null`).
fn json_to_sql(v: Option<&Value>) -> SqlValue {
    match v {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::String(s)) => SqlValue::Text(s.clone()),
        Some(Value::Bool(b)) => SqlValue::Integer(*b as i64),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Some(other) => SqlValue::Text(other.to_string()),
    }
}

/// `parseJson` (journal.ts:130): NULL or empty text is the fallback.
pub(crate) fn parse_json(text: Option<&str>, fallback: Value) -> Result<Value> {
    match text {
        None | Some("") => Ok(fallback),
        Some(t) => serde_json::from_str(t).map_err(|e| IbaraError::new("INTERNAL_ERROR", format!("store: invalid JSON record: {e}"), false)),
    }
}

fn parse_object(text: &str) -> Result<Map<String, Value>> {
    match parse_json(Some(text), json!({}))? {
        Value::Object(m) => Ok(m),
        _ => Ok(Map::new()),
    }
}

fn conversion_error(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn json_col(r: &Row<'_>, name: &str, fallback: Value) -> rusqlite::Result<Value> {
    let text: Option<String> = r.get(name)?;
    match text.as_deref() {
        None | Some("") => Ok(fallback),
        Some(t) => serde_json::from_str(t).map_err(conversion_error),
    }
}

fn json_array_col(r: &Row<'_>, name: &str) -> rusqlite::Result<Vec<Value>> {
    match json_col(r, name, json!([]))? {
        Value::Array(items) => Ok(items),
        _ => Ok(Vec::new()),
    }
}

/// Integer columns may hold REAL values written from JavaScript numbers.
fn get_opt_i64(r: &Row<'_>, name: &str) -> rusqlite::Result<Option<i64>> {
    Ok(match r.get_ref(name)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|s| s.parse::<f64>().ok()).map(|f| f as i64),
        ValueRef::Blob(_) => None,
    })
}

fn get_i64(r: &Row<'_>, name: &str) -> rusqlite::Result<i64> {
    Ok(get_opt_i64(r, name)?.unwrap_or(0))
}

/// `taskFromRow` (journal.ts:959).
fn task_from_row(r: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    let contract_version: Option<String> = r.get("contract_version")?;
    let visibility: Option<String> = r.get("visibility")?;
    Ok(TaskRecord {
        task_ref: r.get("task_ref")?,
        principal: r.get("principal")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        state: r.get("state")?,
        goal: r.get("goal")?,
        success_criteria: json_array_col(r, "success_criteria")?,
        budgets: json_col(r, "budgets", json!({}))?,
        client_flags: json_col(r, "client_flags", json!({}))?,
        authorization_ref: r.get("authorization_ref")?,
        required_capabilities: json_array_col(r, "required_capabilities")?,
        control_started_ms: get_opt_i64(r, "control_started_ms")?,
        last_charge_ms: get_opt_i64(r, "last_charge_ms")?,
        active_control_used_ms: get_i64(r, "active_control_used_ms")?,
        actions_used: get_i64(r, "actions_used")?,
        images_used: get_i64(r, "images_used")?,
        last_checkpoint_ref: r.get("last_checkpoint_ref")?,
        completion: Some(json_col(r, "completion", Value::Null)?).filter(|v| !v.is_null()),
        contract_version: contract_version.filter(|v| v == "3.0" || v == "4").unwrap_or_else(|| "2.0".into()),
        visibility: if visibility.as_deref() == Some("shared") { "shared" } else { "private" }.into(),
        owner_group: r.get("owner_group")?,
        deliveries: json_array_col(r, "deliveries")?,
    })
}

/// `operationFromRow` (journal.ts:986).
fn operation_from_row(r: &Row<'_>) -> rusqlite::Result<OperationRecord> {
    Ok(OperationRecord {
        operation_ref: r.get("operation_ref")?,
        request_id: r.get("request_id")?,
        principal: r.get("principal")?,
        task_ref: r.get("task_ref")?,
        tool: r.get("tool")?,
        fingerprint: r.get("fingerprint")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        receipt: json_col(r, "receipt", json!({}))?,
        dispatched: get_i64(r, "dispatched")? != 0,
        effect_class: r.get::<_, Option<String>>("effect_class")?.unwrap_or_else(|| "change".into()),
    })
}

fn lease_from_row(r: &Row<'_>) -> rusqlite::Result<LeaseRecord> {
    Ok(LeaseRecord {
        generation: r.get("generation")?,
        task_ref: r.get("task_ref")?,
        principal: r.get("principal")?,
        connection_id: r.get("connection_id")?,
        epoch: r.get("epoch")?,
        acquired_at: r.get("acquired_at")?,
        last_heartbeat_at: r.get("last_heartbeat_at")?,
        last_heartbeat_ms: get_i64(r, "last_heartbeat_ms")?,
        idle_expires_at_ms: get_i64(r, "idle_expires_at_ms")?,
        state: r.get("state")?,
        reason: r.get("reason")?,
    })
}

fn stored_simple(r: &Row<'_>, ref_col: &str, kind: &str) -> rusqlite::Result<StoredRecord> {
    Ok(StoredRecord {
        reference: r.get(ref_col)?,
        kind: kind.into(),
        task_ref: r.get("task_ref")?,
        principal: r.get("principal")?,
        epoch: None,
        expired: false,
        record: json_col(r, "record", json!({}))?,
    })
}
