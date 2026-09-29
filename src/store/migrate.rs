//! Migration check: what switching a machine from the Node controller to
//! `ibarad` would find in its state directory, without changing anything.
//!
//! The journal and storage databases are copied with the SQLite online backup
//! API into memory (read-only source, WAL-safe) and inspected there.

use super::canonical;
use super::journal::receipt_needs_retention;
use super::schema::{self, SchemaVersions, StorageJob};
use crate::error::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Fingerprints recomputed per tool, from rows whose arguments can be rebuilt.
const SAMPLE_PER_TOOL: usize = 400;

#[derive(Debug, Clone, Default, Serialize)]
pub struct FingerprintSample {
    /// Operations tried.
    pub sampled: usize,
    /// Of those, how many recomputed to the stored fingerprint.
    pub matched: usize,
    /// The fingerprinted arguments are stored verbatim, so every row must match;
    /// otherwise they are rebuilt from derived records and a miss may only mean
    /// the arguments cannot be recovered (older releases, merged budgets).
    pub exact: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub journal_present: bool,
    pub storage_present: bool,
    pub schema: Option<SchemaVersions>,
    /// Whether this build can open (and additively migrate) the journal.
    pub schema_supported: bool,
    pub core_schema_after_open: u32,
    pub endpoint_identity_present: bool,
    pub fingerprint_secret_present: bool,
    /// The secret is 64 hex characters (32 bytes), as the Node controller wrote it.
    pub fingerprint_secret_well_formed: bool,
    pub operations: i64,
    /// Per tool: `computer_checkpoint` (exact: from the checkpoint record) and
    /// `computer_begin` (rebuilt from the task row).
    pub fingerprint_samples: BTreeMap<String, FingerprintSample>,
    /// `matched / sampled` over the exact samples, or `None` if there were none.
    pub fingerprint_pass_rate: Option<f64>,
    /// Dispatched receipts still `running` without a job: `ibarad` marks them
    /// unknown at its first epoch rotation, as the Node controller did.
    pub running_receipts_without_job: usize,
    /// Dispatched receipts still `running` with a job: `ibarad` marks them
    /// unknown on first open, keeping storage's job outcome as evidence.
    pub stale_job_receipts: usize,
    /// Storage's last recorded state for those jobs (`missing` if no row).
    pub stale_job_storage_states: BTreeMap<String, usize>,
    /// Of the stale job receipts, how many storage records as settled
    /// (completed, failed or cancelled). They still become unknown, never done.
    pub stale_job_storage_settled: usize,
    /// Journal job mirror rows whose state differs from storage (refreshed on open).
    pub jobs_mirror_stale: usize,
    /// Begin rows that never got a task (`task_ref` NULL); replays return their original error.
    pub orphan_begin_rows: i64,
    pub orphan_begin_errors: BTreeMap<String, usize>,
    /// Operations that retention keeps (unresolved or uncertain receipts).
    pub operations_needing_retention: usize,
    pub problems: Vec<String>,
}

/// Inspect `state_dir` read-only and report what a cutover would do.
pub fn check(state_dir: &Path) -> Result<Report> {
    let mut report = Report::default();
    let journal = match snapshot(&state_dir.join("journal.sqlite"))? {
        Some(db) => db,
        None => {
            report.problems.push("journal.sqlite is missing".into());
            return Ok(report);
        }
    };
    report.journal_present = true;
    let storage = snapshot(&state_dir.join("storage.sqlite"))?;
    report.storage_present = storage.is_some();
    let storage_jobs = storage.as_ref().is_some_and(|db| has_table(db, "jobs"));

    let versions = schema::read_versions(&journal)?;
    match schema::check_supported(&versions) {
        Ok(_) => report.schema_supported = true,
        Err(e) => report.problems.push(e.message.clone()),
    }
    report.core_schema_after_open = schema::CORE_SCHEMA_VERSION;
    report.schema = Some(versions);
    if !has_table(&journal, "meta") || !has_table(&journal, "operations") {
        report.problems.push("journal has no meta or operations table".into());
        return Ok(report);
    }

    let meta = |key: &str| -> Result<Option<String>> {
        Ok(journal.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).optional()?)
    };
    report.endpoint_identity_present = meta("endpoint_identity")?.is_some_and(|v| !v.is_empty());
    let secret_hex = meta("fingerprint_secret")?;
    report.fingerprint_secret_present = secret_hex.is_some();
    report.fingerprint_secret_well_formed =
        secret_hex.as_deref().is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if !report.endpoint_identity_present {
        report.problems.push("endpoint_identity is missing: operator pairings would break".into());
    }
    if !report.fingerprint_secret_well_formed {
        report.problems.push("fingerprint_secret is missing or malformed: retried requests would not be recognised".into());
    }
    report.operations = journal.query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))?;

    if let Some(hex) = &secret_hex {
        let secret = canonical::node_hex_decode(hex);
        sample_begins(&journal, &secret, &mut report)?;
        sample_checkpoints(&journal, &secret, &mut report)?;
        let (sampled, matched) =
            report.fingerprint_samples.values().filter(|x| x.exact).fold((0, 0), |(s, m), x| (s + x.sampled, m + x.matched));
        report.fingerprint_pass_rate = (sampled > 0).then(|| matched as f64 / sampled as f64);
        if sampled > 0 && matched < sampled {
            report.problems.push(format!("{} of {sampled} sampled fingerprints did not recompute", sampled - matched));
        }
    }

    // Running receipts.
    let mut stmt = journal.prepare(
        "SELECT receipt FROM operations WHERE dispatched = 1 AND json_extract(receipt, '$.execution') = 'running'",
    )?;
    let running: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for text in running {
        let receipt: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let job_ref = receipt.get("job_ref").and_then(Value::as_str).unwrap_or("");
        if job_ref.is_empty() {
            report.running_receipts_without_job += 1;
            continue;
        }
        report.stale_job_receipts += 1;
        let job = if storage_jobs { schema::storage_job(storage.as_ref().unwrap(), job_ref)? } else { None };
        let state = job.as_ref().and_then(|j| j.state.clone()).unwrap_or_else(|| "missing".into());
        if matches!(state.as_str(), "completed" | "failed" | "cancelled") {
            report.stale_job_storage_settled += 1;
        }
        *report.stale_job_storage_states.entry(state).or_default() += 1;
    }
    if storage_jobs {
        let storage = storage.as_ref().unwrap();
        let mut stmt = journal.prepare("SELECT job_ref, record FROM jobs")?;
        let rows: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (job_ref, text) in rows {
            let mirrored = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_string));
            if let Some(StorageJob { state, .. }) = schema::storage_job(storage, &job_ref)?
                && state != mirrored
            {
                report.jobs_mirror_stale += 1;
            }
        }
    }

    report.orphan_begin_rows =
        journal.query_row("SELECT COUNT(*) FROM operations WHERE tool = 'computer_begin' AND task_ref IS NULL", [], |r| r.get(0))?;
    let mut stmt = journal.prepare(
        "SELECT COALESCE(json_extract(receipt, '$.error.code'), 'none') FROM operations WHERE tool = 'computer_begin' AND task_ref IS NULL",
    )?;
    for code in stmt.query_map([], |r| r.get::<_, String>(0))? {
        *report.orphan_begin_errors.entry(code?).or_default() += 1;
    }

    let mut stmt = journal.prepare("SELECT receipt, dispatched FROM operations WHERE task_ref IS NOT NULL")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (text, dispatched) = row?;
        let receipt: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        if receipt_needs_retention(&receipt, dispatched != 0) {
            report.operations_needing_retention += 1;
        }
    }
    Ok(report)
}

/// Copy a database into memory with the online backup API (source opened read-only).
pub(crate) fn snapshot(path: &Path) -> Result<Option<Connection>> {
    if !path.is_file() {
        return Ok(None);
    }
    let uri = format!("file:{}?mode=ro", path.display());
    let source = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let mut copy = Connection::open_in_memory()?;
    rusqlite::backup::Backup::new(&source, &mut copy)?.run_to_completion(1024, std::time::Duration::from_millis(5), None)?;
    Ok(Some(copy))
}

fn has_table(db: &Connection, name: &str) -> bool {
    db.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", [name], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

fn fp(secret: &[u8], tool: &str, args: Value) -> String {
    canonical::fingerprint(secret, &json!({ "tool": tool, "args": args }))
}

/// Caller budgets were merged over defaults (core.ts:1474), so the caller's
/// object is some subset of the stored keys: try every subset (at most 2^10).
fn budget_subsets(budgets: &Value) -> Vec<Value> {
    let Some(map) = budgets.as_object().filter(|m| m.len() <= 10) else { return vec![json!({}), budgets.clone()] };
    let entries: Vec<(&String, &Value)> = map.iter().collect();
    (0u32..(1 << entries.len()))
        .map(|mask| {
            Value::Object(entries.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, (k, v))| ((*k).clone(), (*v).clone())).collect())
        })
        .collect()
}

/// Fresh begins (not resumes): rebuild the fingerprint source (core.ts:1386-1396)
/// from the task row. Budgets were merged with defaults, deliveries gained
/// `revision`, and older releases fingerprinted other shapes, so candidate
/// forms are tried; any match counts. Not exact.
fn sample_begins(db: &Connection, secret: &[u8], report: &mut Report) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT o.fingerprint, t.goal, t.success_criteria, t.authorization_ref, t.required_capabilities, t.budgets, t.client_flags, t.contract_version, t.deliveries
         FROM operations o JOIN tasks t ON t.task_ref = o.task_ref
         WHERE o.tool = 'computer_begin' AND t.created_at >= o.created_at ORDER BY o.created_at DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([SAMPLE_PER_TOOL as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<String>>(8)?,
        ))
    })?;
    let parse = |t: &str| serde_json::from_str::<Value>(t).unwrap_or(Value::Null);
    let sample = report.fingerprint_samples.entry("computer_begin".into()).or_default();
    for row in rows {
        let (stored, goal, criteria, authorization, capabilities, budgets, flags, contract, deliveries) = row?;
        let budgets = parse(&budgets);
        let flags = parse(&flags);
        let deliveries: Vec<Value> = parse(deliveries.as_deref().unwrap_or("[]"))
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut d| {
                if let Some(o) = d.as_object_mut() {
                    o.shift_remove("revision");
                }
                d
            })
            .collect();
        let budget_options = budget_subsets(&budgets);
        let flag_options = [None, Some(flags)];
        let stored_contract = contract.unwrap_or_else(|| "2.0".into());
        let contracts = [stored_contract.as_str(), "2.0", "3.0"];
        let delivery_options = [Value::Array(deliveries), json!([])];
        let mut matched = false;
        'outer: for budgets in &budget_options {
            for flags in &flag_options {
                for contract in contracts {
                    for deliveries in &delivery_options {
                        let mut args = json!({
                            "goal": goal,
                            "success_criteria": parse(&criteria),
                            "resume_task_ref": null,
                            "authorization_ref": authorization,
                            "required_capabilities": parse(&capabilities),
                            "budgets": budgets,
                            "contract_version": contract,
                            "deliveries": deliveries,
                        });
                        if let Some(f) = flags {
                            args["client_flags"] = f.clone();
                        }
                        if canonical::fingerprints_equal(&stored, &fp(secret, "computer_begin", args)) {
                            matched = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        sample.sampled += 1;
        sample.matched += matched as usize;
    }
    Ok(())
}

/// Checkpoints: the record stores exactly the fingerprinted fields (core.ts:1777, :1786-1795).
fn sample_checkpoints(db: &Connection, secret: &[u8], report: &mut Report) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT o.fingerprint, c.record FROM operations o JOIN checkpoints c ON c.checkpoint_ref = json_extract(o.receipt, '$.evidence_refs[0]')
         WHERE o.tool = 'computer_checkpoint' ORDER BY o.created_at DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([SAMPLE_PER_TOOL as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let sample = report.fingerprint_samples.entry("computer_checkpoint".into()).or_default();
    sample.exact = true;
    for row in rows {
        let (stored, record) = row?;
        let record: Value = serde_json::from_str(&record).unwrap_or(Value::Null);
        let args = json!({
            "next_step": record.get("next_step").cloned().unwrap_or(Value::Null),
            "blockers": record.get("blockers").cloned().unwrap_or(json!([])),
            "hypotheses": record.get("hypotheses").cloned().unwrap_or(json!([])),
            "evidence_refs": record.get("evidence_refs").cloned().unwrap_or(json!([])),
        });
        sample.sampled += 1;
        sample.matched += canonical::fingerprints_equal(&stored, &fp(secret, "computer_checkpoint", args)) as usize;
    }
    Ok(())
}
