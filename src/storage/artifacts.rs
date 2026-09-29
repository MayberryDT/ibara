//! Published artifacts, delivery and retention, and the transfer-v1 request
//! kinds (storage.ts:1098-1125, 1296-1332, 1661-1673, 1713-1896, 1897-2139;
//! transfer.ts).

use super::fsx::{self, O_CREAT, O_EXCL, O_NONBLOCK, O_RDONLY, O_WRONLY, errno, fail, fd_ref};
use super::jsv::{self, num_or, str_or, to_js_string};
use super::{Context, StorageService, ToolResult, is_journal_terminal_state};
use crate::error::Result;
use crate::ids::{id, iso_from_millis};
use base64::Engine;
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Map, Value, json};
use std::os::fd::BorrowedFd;

/// Reader pins expire after this idle time (storage.ts:1319-1321).
pub const READER_IDLE_MS: i64 = 300_000;
/// At most this many reader pins exist at once (storage.ts:1317).
pub const MAX_READERS: i64 = 1024;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A row of `artifacts`.
#[derive(Debug, Clone)]
pub(super) struct ArtifactRow {
    pub artifact_ref: String,
    pub task_ref: String,
    pub principal: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub producer_ref: String,
    pub evidence_json: String,
    pub delivery: String,
    pub file_name: String,
    pub created_at: String,
    pub collected_at: Option<String>,
}

const ARTIFACT_COLUMNS: &str = "artifact_ref, task_ref, principal, name, mime_type, size_bytes, sha256, producer_ref, evidence_json, delivery, file_name, created_at, collected_at";

fn lenient_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Integer(i) => i.max(0) as u64,
        ValueRef::Real(f) => f.max(0.0) as u64,
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        _ => 0,
    })
}

impl ArtifactRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(ArtifactRow {
            artifact_ref: row.get(0)?,
            task_ref: row.get(1)?,
            principal: row.get(2)?,
            name: row.get(3)?,
            mime_type: row.get(4)?,
            size_bytes: lenient_u64(row, 5)?,
            sha256: row.get(6)?,
            producer_ref: row.get(7)?,
            evidence_json: row.get(8)?,
            delivery: row.get(9)?,
            file_name: row.get(10)?,
            created_at: row.get(11)?,
            collected_at: row.get(12)?,
        })
    }

    /// storage.ts:1736-1750 `artifactRecord`.
    fn record(&self) -> Value {
        json!({
            "kind": "artifact",
            "artifact_ref": self.artifact_ref,
            "task_ref": self.task_ref,
            "name": self.name,
            "mime_type": self.mime_type,
            "size_bytes": self.size_bytes,
            "sha256": self.sha256,
            "producer_ref": self.producer_ref,
            "evidence_refs": serde_json::from_str::<Value>(&self.evidence_json).unwrap_or_else(|_| json!([])),
            "ready": true,
            "delivery": self.delivery,
        })
    }

    /// storage.ts:1897-1912 `operatorArtifactSummary`.
    fn operator_summary(&self) -> Map<String, Value> {
        let v = json!({
            "kind": "artifact_summary",
            "artifact_ref": self.artifact_ref,
            "task_ref": self.task_ref,
            "principal": self.principal,
            "name": self.name,
            "mime_type": self.mime_type,
            "size_bytes": self.size_bytes,
            "sha256": self.sha256,
            "created_at": self.created_at,
            "collected_at": self.collected_at,
            "delivery": self.delivery,
            "bytes_available": self.delivery != "unavailable",
        });
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }
}

/// A row of `staged`.
#[derive(Debug, Clone)]
pub(super) struct StagedRow {
    pub staged_ref: String,
    pub principal: String,
    pub name: String,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub received_bytes: u64,
    pub complete: bool,
    pub file_name: String,
}

/// A chunk of artifact bytes (storage.ts:1926-1943, 2035-2051).
fn chunk_record(row: &ArtifactRow, offset: u64, bytes: &[u8]) -> Value {
    json!({
        "kind": "chunk",
        "artifact_ref": row.artifact_ref,
        "offset": offset,
        "length": bytes.len(),
        "eof": offset + bytes.len() as u64 >= row.size_bytes,
        "data": B64.encode(bytes),
        "size_bytes": row.size_bytes,
        "sha256": row.sha256,
    })
}

fn is_permission_like(e: &crate::error::IbaraError) -> bool {
    e.code == "PERMISSION_DENIED" || e.code == "INVALID_ARGUMENT"
}

impl StorageService {
    pub(super) fn lookup_artifact(&self, artifact_ref: &str) -> Result<Option<ArtifactRow>> {
        self.with_db(|db| {
            Ok(db
                .query_row(
                    &format!("SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE artifact_ref = ?1"),
                    [artifact_ref],
                    ArtifactRow::from_row,
                )
                .optional()?)
        })
    }

    /// storage.ts:1869-1882 `requireOwnedArtifact`.
    fn require_owned_artifact(&self, principal: &str, artifact_ref: &str) -> Result<ArtifactRow> {
        fsx::assert_id(artifact_ref, "artifact_ref")?;
        let denied = || fail("PERMISSION_DENIED", "Artifact is not available to this principal.", false);
        let row = self.lookup_artifact(artifact_ref)?.ok_or_else(denied)?;
        match self.journal() {
            Some(view) => {
                if let Err(e) = view.read_task(principal, &row.task_ref) {
                    return Err(if is_permission_like(&e) { denied() } else { e });
                }
            }
            None if row.principal != principal => return Err(denied()),
            None => {}
        }
        Ok(row)
    }

    /// storage.ts:1752-1758 `requireArtifactBytes`.
    pub(super) fn require_artifact_bytes(&self, row: &ArtifactRow) -> Result<()> {
        self.expire_collected_artifacts()?;
        let delivery = self.lookup_artifact(&row.artifact_ref)?.map_or_else(|| row.delivery.clone(), |r| r.delivery);
        if delivery == "unavailable" {
            return Err(fail("DELIVERY_UNAVAILABLE", "Collected artifact bytes expired after the retention window.", true));
        }
        Ok(())
    }

    /// storage.ts:1713-1726 `withArtifactFile`.
    pub(super) fn with_artifact_file<T>(&self, file_name: &str, f: impl FnOnce(BorrowedFd<'_>) -> Result<T>) -> Result<T> {
        let dir = fsx::open_dir(&self.artifact_root())?;
        let fd = match fsx::open_child(fd_ref(&dir), file_name, O_RDONLY | O_NONBLOCK, 0)? {
            Ok(fd) => fd,
            Err(e) if errno(&e) == libc::ENOENT => {
                return Err(fail("DELIVERY_UNAVAILABLE", "Artifact bytes are no longer retained.", true));
            }
            Err(e) => return Err(e.into()),
        };
        f(fd_ref(&fd))
    }

    /// storage.ts:1661-1665 `inspectArtifact`.
    pub(super) fn inspect_artifact(&self, artifact_ref: &str, principal: &str) -> Result<Value> {
        let row = self.require_owned_artifact(principal, artifact_ref)?;
        self.expire_collected_artifacts()?;
        Ok(self.lookup_artifact(&row.artifact_ref)?.unwrap_or(row).record())
    }

    /// storage.ts:1667-1673 `readArtifact`.
    pub(super) fn read_artifact(&self, artifact_ref: &str, ctx: &Context, offset: f64, max_chars: f64) -> Result<ToolResult> {
        let row = self.require_owned_artifact(&ctx.principal, artifact_ref)?;
        self.require_artifact_bytes(&row)?;
        let offset = if offset.is_finite() { offset.max(0.0) as u64 } else { 0 };
        let max_out = self.inner.opts.max_output_chars as f64;
        let chars = if max_chars.is_nan() { max_out } else { max_chars.max(1.0).min(max_out) };
        let buf = self.with_artifact_file(&row.file_name, |fd| fsx::read_fd_bytes(fd, offset, chars as u64 * 4))?;
        if buf.contains(&0) {
            return Err(fail("WRONG_TOOL", "Binary artifacts use the transfer data path, not read_artifact.", true));
        }
        let text = String::from_utf8_lossy(&buf);
        Ok(ToolResult { records: vec![self.observation(ctx, &text, None), row.record()] })
    }

    /// storage.ts:1098-1105 `artifacts`: the task's artifacts, after the
    /// engine's authority check (or only the principal's own without one).
    pub fn artifacts(&self, task_ref: &str, principal: &str) -> Result<Vec<Value>> {
        self.ensure_open()?;
        let view = self.journal();
        if let Some(view) = &view {
            view.read_task(principal, task_ref)?;
        }
        self.with_db(|db| {
            let mut out = Vec::new();
            if view.is_some() {
                let mut stmt = db.prepare(&format!("SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE task_ref = ?1"))?;
                for row in stmt.query_map([task_ref], ArtifactRow::from_row)? {
                    out.push(row?.record());
                }
            } else {
                let mut stmt = db.prepare(&format!(
                    "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE task_ref = ?1 AND principal = ?2"
                ))?;
                for row in stmt.query_map([task_ref, principal], ArtifactRow::from_row)? {
                    out.push(row?.record());
                }
            }
            Ok(out)
        })
    }

    /// storage.ts:1107-1125 `listPublishedArtifacts`: newest first, paged by
    /// the last `artifact_ref` seen. Paging is done in SQL so memory stays bounded.
    pub fn list_published_artifacts(&self, limit: f64, cursor: Option<&str>) -> Result<Value> {
        self.ensure_open()?;
        self.expire_collected_artifacts()?;
        let floor = limit.floor();
        let bounded = if floor == 0.0 || floor.is_nan() { 20.0 } else { floor };
        let bounded = bounded.clamp(1.0, 100.0) as i64;
        self.with_db(|db| {
            let total: i64 = db.query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))?;
            let start: i64 = match cursor.filter(|c| !c.is_empty()) {
                None => 0,
                Some(c) => db
                    .query_row(
                        "SELECT (SELECT COUNT(*) FROM artifacts a WHERE a.created_at > c.created_at
                           OR (a.created_at = c.created_at AND a.artifact_ref > c.artifact_ref))
                         FROM artifacts c WHERE c.artifact_ref = ?1",
                        [c],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .map_or(total, |index| index + 1),
            };
            let mut stmt = db.prepare(&format!(
                "SELECT {ARTIFACT_COLUMNS} FROM artifacts ORDER BY created_at DESC, artifact_ref DESC LIMIT ?1 OFFSET ?2"
            ))?;
            let items: Vec<Value> = stmt
                .query_map(params![bounded, start], ArtifactRow::from_row)?
                .map(|r| r.map(|row| Value::Object(row.operator_summary())))
                .collect::<rusqlite::Result<_>>()?;
            let more = start + (items.len() as i64) < total;
            let next = match (more, items.last()) {
                (true, Some(last)) => last["artifact_ref"].clone(),
                _ => Value::Null,
            };
            Ok(json!({ "items": items, "next_cursor": next, "total": total }))
        })
    }

    /// storage.ts:1760-1772 `expireCollectedArtifacts`: drop idle reader pins,
    /// then unlink collected bytes past retention that no reader pins and that
    /// [`Self::artifact_retention_eligible`] allows. Uncollected bytes never expire.
    pub(super) fn expire_collected_artifacts(&self) -> Result<()> {
        let now = self.clock();
        let cutoff = iso_from_millis(now - self.inner.opts.artifact_retention_ms);
        let rows: Vec<ArtifactRow> = self.with_db(|db| {
            db.execute("DELETE FROM artifact_readers WHERE expires_ms <= ?1", [now])?;
            let mut stmt = db.prepare(&format!(
                "SELECT {ARTIFACT_COLUMNS} FROM artifacts a WHERE delivery = 'collected' AND collected_at IS NOT NULL AND collected_at <= ?1
                 AND NOT EXISTS (SELECT 1 FROM artifact_readers r WHERE r.artifact_ref = a.artifact_ref)"
            ))?;
            let rows = stmt.query_map([&cutoff], ArtifactRow::from_row)?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        for row in rows {
            if !self.artifact_retention_eligible(&row) {
                continue;
            }
            self.unlink_artifact_file(&row.file_name)?;
            self.with_db(|db| {
                db.execute("UPDATE artifacts SET delivery = 'unavailable' WHERE artifact_ref = ?1", [&row.artifact_ref])?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// storage.ts:1774-1786 `artifactRetentionEligible`: only with the journal
    /// view installed, only for a readable terminal task whose required
    /// deliveries of this artifact are verified and which has no unresolved
    /// operation. Any journal error keeps the bytes.
    fn artifact_retention_eligible(&self, row: &ArtifactRow) -> bool {
        let Some(view) = self.journal() else { return false };
        let Ok(task) = view.read_task(&row.principal, &row.task_ref) else { return false };
        if !is_journal_terminal_state(&task.state) {
            return false;
        }
        let pending = task.deliveries.iter().any(|d| {
            d.get("destination") == Some(&json!("artifact_collection"))
                && d.get("path") == Some(&json!(row.name))
                && d.get("required") != Some(&Value::Bool(false))
                && !self.delivery_verified(&row.task_ref, d)
        });
        if pending {
            return false;
        }
        matches!(view.has_unresolved(&row.task_ref), Ok(false))
    }

    /// storage.ts:1805-1817 `unlinkArtifactFile`.
    fn unlink_artifact_file(&self, file_name: &str) -> Result<()> {
        if file_name.is_empty() || file_name.contains(['/', '\\', '\0']) || file_name == "." || file_name == ".." {
            return Ok(());
        }
        let dir = fsx::open_dir(&self.artifact_root())?;
        if let Err(e) = fsx::unlink_at(fd_ref(&dir), file_name)
            && errno(&e) != libc::ENOENT
        {
            return Err(e.into());
        }
        fsx::fsync(fd_ref(&dir))?;
        Ok(())
    }

    /// storage.ts:1884-1887 `deliveryObligations`.
    fn delivery_obligations(&self, principal: &str, row: &ArtifactRow) -> Result<Vec<Value>> {
        let Some(view) = self.journal() else { return Ok(Vec::new()) };
        let task = view.read_task(principal, &row.task_ref)?;
        Ok(task
            .deliveries
            .into_iter()
            .filter(|d| d.get("destination") == Some(&json!("artifact_collection")) && d.get("path") == Some(&json!(row.name)))
            .collect())
    }

    /// storage.ts:1889-1895 `deliveryVerified`: a collection receipt matches the
    /// obligation's id, revision, host, destination, artifact, digest and size.
    pub fn delivery_verified(&self, task_ref: &str, requirement: &Value) -> bool {
        let t = |k: &str| jsv::truthy(requirement.get(k));
        if !t("host_id") || !t("destination_path") || !t("revision") || !t("artifact_ref") || !t("sha256") {
            return false;
        }
        if matches!(requirement.get("size_bytes"), None | Some(Value::Null)) {
            return false;
        }
        let b = |k: &str| fsx::bind_json(requirement.get(k));
        self.with_db(|db| {
            Ok(db
                .query_row(
                    "SELECT 1 FROM collection_receipts r JOIN artifacts a ON a.artifact_ref = r.artifact_ref
                     WHERE r.task_ref = ?1 AND r.obligation_id = ?2 AND r.revision = ?3 AND r.host_id = ?4 AND r.destination_path = ?5
                     AND r.sha256 = a.sha256 AND r.size_bytes = a.size_bytes AND a.name = ?6 AND r.artifact_ref = ?7 AND r.sha256 = ?8 AND r.size_bytes = ?9 LIMIT 1",
                    params![
                        task_ref,
                        b("id"),
                        b("revision"),
                        b("host_id"),
                        b("destination_path"),
                        b("path"),
                        b("artifact_ref"),
                        b("sha256"),
                        b("size_bytes")
                    ],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .unwrap_or(false)
    }

    /// storage.ts:1296-1332 `transfer`: the transfer-v1 request kinds. The
    /// connection defaults to `legacy` (admin transfers).
    pub fn transfer(&self, principal: &str, request: &Value, connection_id: Option<&str>) -> Result<Value> {
        self.ensure_open()?;
        if principal.is_empty() || !jsv::id_re(principal) {
            return Err(fail("PERMISSION_DENIED", "Transfer requires an authenticated principal.", false));
        }
        let kind = str_or(request, "kind", "");
        let connection_id = connection_id.unwrap_or("legacy");
        if !jsv::id_re(connection_id) {
            return Err(fail("INVALID_ARGUMENT", "Invalid transfer connection.", true));
        }
        let operator = principal == "operator";
        match kind.as_str() {
            "end_transfer" => {
                self.with_db(|db| {
                    db.execute(
                        "DELETE FROM artifact_readers WHERE principal = ?1 AND connection_id = ?2",
                        [principal, connection_id],
                    )?;
                    Ok(())
                })?;
                self.expire_collected_artifacts()?;
                Ok(json!({ "kind": "transfer_closed" }))
            }
            "stat_artifact" => {
                let r = str_or(request, "artifact_ref", "");
                if operator { self.operator_stat_artifact(&r) } else { self.stat_artifact(principal, &r) }
            }
            "download_artifact" => {
                // The gateway owns connection identity. Pins retain bytes, never
                // authority: every chunk still enters the authorized read path first.
                let (row, offset, bytes) = self.download_chunk(principal, request, operator)?;
                let eof = offset + bytes.len() as u64 >= row.size_bytes;
                let expires = self.clock() + READER_IDLE_MS;
                self.with_db(|db| {
                    if eof {
                        db.execute(
                            "DELETE FROM artifact_readers WHERE principal = ?1 AND connection_id = ?2 AND artifact_ref = ?3",
                            [principal, connection_id, &row.artifact_ref],
                        )?;
                        return Ok(());
                    }
                    let existing = db
                        .query_row(
                            "SELECT 1 FROM artifact_readers WHERE principal = ?1 AND connection_id = ?2 AND artifact_ref = ?3",
                            [principal, connection_id, &row.artifact_ref],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    let count: i64 = db.query_row("SELECT COUNT(*) FROM artifact_readers", [], |r| r.get(0))?;
                    if !existing && count >= MAX_READERS {
                        return Err(fail(
                            "BUDGET_EXCEEDED",
                            "Too many active artifact readers; close or allow idle readers to expire.",
                            true,
                        ));
                    }
                    db.execute(
                        "INSERT INTO artifact_readers(principal, connection_id, artifact_ref, expires_ms) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(principal, connection_id, artifact_ref) DO UPDATE SET expires_ms = excluded.expires_ms",
                        params![principal, connection_id, row.artifact_ref, expires],
                    )?;
                    Ok(())
                })?;
                let mut chunk = chunk_record(&row, offset, &bytes);
                chunk["reader_idle_timeout_ms"] = json!(READER_IDLE_MS);
                Ok(chunk)
            }
            "ack_collected" => {
                if operator { self.operator_ack_collected(request) } else { self.ack_collected(principal, request) }
            }
            "begin_upload" => self.begin_upload(principal, request),
            "upload_chunk" => self.upload_chunk(principal, request),
            "stat_staged" => self.stat_staged(principal, &str_or(request, "staged_ref", "")),
            "complete_inbox" => {
                if !operator {
                    return Err(fail("PERMISSION_DENIED", "Inbox completion is an operator action.", false));
                }
                self.complete_inbox(principal, request)
            }
            _ => Err(fail(
                "INVALID_ARGUMENT",
                format!("Unsupported transfer kind: {}.", if kind.is_empty() { "missing" } else { &kind }),
                true,
            )),
        }
    }

    /// storage.ts:1926-1943 `operatorDownloadArtifact` and 2035-2051 `downloadArtifact`.
    fn download_chunk(&self, principal: &str, request: &Value, operator: bool) -> Result<(ArtifactRow, u64, Vec<u8>)> {
        let artifact_ref = str_or(request, "artifact_ref", "");
        let row = if operator {
            fsx::assert_id(&artifact_ref, "artifact_ref")?;
            self.lookup_artifact(&artifact_ref)?.ok_or_else(|| fail("INVALID_ARGUMENT", "Unknown artifact_ref.", true))?
        } else {
            self.require_owned_artifact(principal, &artifact_ref)?
        };
        self.require_artifact_bytes(&row)?;
        let offset = num_or(request, "offset", 0.0);
        let offset = if offset.is_finite() { offset.max(0.0) as u64 } else { 0 };
        let chunk = self.inner.opts.chunk_bytes as f64;
        let max = jsv::clamp_or(num_or(request, "max_bytes", chunk), 1.0, chunk, chunk) as u64;
        let bytes = self.with_artifact_file(&row.file_name, |fd| fsx::read_fd_bytes(fd, offset, max))?;
        Ok((row, offset, bytes))
    }

    /// storage.ts:1914-1924 `operatorStatArtifact`.
    fn operator_stat_artifact(&self, artifact_ref: &str) -> Result<Value> {
        fsx::assert_id(artifact_ref, "artifact_ref")?;
        let row = self.lookup_artifact(artifact_ref)?.ok_or_else(|| fail("INVALID_ARGUMENT", "Unknown artifact_ref.", true))?;
        self.expire_collected_artifacts()?;
        let current = self.lookup_artifact(&row.artifact_ref)?.unwrap_or(row);
        let mut out = current.operator_summary();
        out.insert("kind".into(), json!("artifact_stat"));
        out.insert("ready".into(), json!(current.delivery != "unavailable"));
        Ok(Value::Object(out))
    }

    /// storage.ts:1945-1956 `operatorAckCollected`: the person's console saved
    /// the file. A save of the sent bytes to exactly the current destination of
    /// one delivery of this file is that delivery: it records a receipt as the
    /// collector's does, for the delivery's host and revision. Any other save
    /// is collected, without a receipt.
    fn operator_ack_collected(&self, request: &Value) -> Result<Value> {
        let artifact_ref = str_or(request, "artifact_ref", "");
        fsx::assert_id(&artifact_ref, "artifact_ref")?;
        let row = self.lookup_artifact(&artifact_ref)?.ok_or_else(|| fail("INVALID_ARGUMENT", "Unknown artifact_ref.", true))?;
        self.check_receipt_matches(&row, request)?;
        let dest = request.get("destination_path").and_then(Value::as_str).filter(|p| p.starts_with('/') && jsv::posix_normalize(p) == *p);
        let obligations = if dest.is_some() { self.delivery_obligations(&row.principal, &row)? } else { Vec::new() };
        let mut delivered = obligations.iter().filter(|o| {
            o.get("destination_path").and_then(Value::as_str) == dest
                && o.get("artifact_ref").and_then(Value::as_str) == Some(row.artifact_ref.as_str())
                && o.get("host_id").and_then(Value::as_str).is_some_and(|h| !h.is_empty())
        });
        let obligation = match (delivered.next(), delivered.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        };
        let now = self.now_iso();
        let collected_at = match obligation {
            Some(_) => now.clone(),
            None => row.collected_at.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| now.clone()),
        };
        self.with_db(|db| {
            if let Some(o) = obligation {
                let host = o.get("host_id").cloned().unwrap_or(Value::Null);
                let (ob_id, ob_rev) = (o.get("id").cloned().unwrap_or(Value::Null), o.get("revision").cloned().unwrap_or(Value::Null));
                let source = json!([row.artifact_ref, "operator", host, dest, ob_id, ob_rev]);
                let receipt_id = fsx::sha256_hex(source.to_string().as_bytes());
                db.execute(
                    "INSERT OR IGNORE INTO collection_receipts(receipt_id, task_ref, artifact_ref, obligation_id, revision, principal, host_id, destination_path, sha256, size_bytes, collected_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'operator', ?6, ?7, ?8, ?9, ?10)",
                    params![
                        receipt_id,
                        row.task_ref,
                        row.artifact_ref,
                        fsx::bind_json(Some(&ob_id)),
                        fsx::bind_json(Some(&ob_rev)),
                        fsx::bind_json(Some(&host)),
                        dest,
                        row.sha256,
                        row.size_bytes as i64,
                        now
                    ],
                )?;
            }
            db.execute("UPDATE artifacts SET delivery = 'collected', collected_at = ?1 WHERE artifact_ref = ?2", [&collected_at, &row.artifact_ref])?;
            Ok(())
        })?;
        self.expire_collected_artifacts()?;
        self.collected_reply(row)
    }

    /// `Number(request.size_bytes) !== row.size_bytes || String(request.sha256) !== row.sha256`.
    fn check_receipt_matches(&self, row: &ArtifactRow, request: &Value) -> Result<()> {
        let size = jsv::to_number(request.get("size_bytes"));
        let sha = request.get("sha256").map_or_else(|| "undefined".to_string(), to_js_string);
        if size != row.size_bytes as f64 || sha != row.sha256 {
            return Err(fail("POSTCONDITION_FAILED", "Collection receipt does not match the stored artifact.", false));
        }
        Ok(())
    }

    fn collected_reply(&self, row: ArtifactRow) -> Result<Value> {
        let current = self.lookup_artifact(&row.artifact_ref)?.unwrap_or(ArtifactRow { delivery: "collected".into(), ..row });
        Ok(json!({
            "kind": "collected",
            "artifact_ref": current.artifact_ref,
            "delivery": current.delivery,
            "size_bytes": current.size_bytes,
            "sha256": current.sha256,
        }))
    }

    /// The host a delivery to `principal`'s own computer is recorded under, and
    /// that its collector's receipts carry: the collector identity registered
    /// for it, else the principal. A collector's transfer session is
    /// authenticated as its principal, so it collects on that computer.
    pub fn collector_host(&self, principal: &str) -> String {
        self.journal().and_then(|v| v.collector(principal)).unwrap_or_else(|| principal.to_string())
    }

    /// storage.ts:2024-2033 `statArtifact`.
    fn stat_artifact(&self, principal: &str, artifact_ref: &str) -> Result<Value> {
        let row = self.require_owned_artifact(principal, artifact_ref)?;
        self.expire_collected_artifacts()?;
        let current = self.lookup_artifact(&row.artifact_ref)?.unwrap_or(row);
        let obligations = self.delivery_obligations(principal, &current)?;
        let collector = self.collector_host(principal);
        let obligation = obligations
            .iter()
            .find(|o| o.get("host_id").and_then(Value::as_str) == Some(collector.as_str()))
            .or(if obligations.len() == 1 { obligations.first() } else { None });
        let mut out = json!({
            "kind": "artifact_stat",
            "artifact_ref": current.artifact_ref,
            "name": current.name,
            "mime_type": current.mime_type,
            "size_bytes": current.size_bytes,
            "sha256": current.sha256,
            "ready": current.delivery != "unavailable",
            "delivery": current.delivery,
        });
        if let Some(Value::Object(o)) = obligation {
            let mut ob = o.clone();
            ob.insert("obligation_id".into(), o.get("id").cloned().unwrap_or(Value::Null));
            ob.insert("obligation_revision".into(), o.get("revision").cloned().unwrap_or(Value::Null));
            out["delivery_obligation"] = Value::Object(ob);
        }
        Ok(out)
    }

    /// storage.ts:2053-2074 `ackCollected`: record a collection receipt and mark
    /// the artifact collected. With obligations for this name, the current
    /// obligation revision and a normalized absolute destination are required;
    /// the receipt is for the collector host ([`Self::collector_host`]).
    fn ack_collected(&self, principal: &str, request: &Value) -> Result<Value> {
        let row = self.require_owned_artifact(principal, &str_or(request, "artifact_ref", ""))?;
        self.check_receipt_matches(&row, request)?;
        let obligations = self.delivery_obligations(principal, &row)?;
        let obligation = obligations.iter().find(|o| o.get("id") == request.get("obligation_id"));
        let host = self.collector_host(principal);
        let dest = request.get("destination_path");
        if !obligations.is_empty() {
            let dest_ok = matches!(dest, Some(Value::String(p)) if p.starts_with('/') && jsv::posix_normalize(p) == *p);
            let revision_ok = obligation.is_some_and(|o| o.get("revision") == request.get("obligation_revision"));
            if obligation.is_none() || !revision_ok || !dest_ok {
                return Err(fail(
                    "POSTCONDITION_FAILED",
                    "Collection requires the current obligation revision and a normalized destination path.",
                    false,
                ));
            }
        }
        let ob_id = obligation.and_then(|o| o.get("id")).cloned().unwrap_or(Value::Null);
        let ob_rev = obligation.and_then(|o| o.get("revision")).cloned().unwrap_or(Value::Null);
        let receipt_source = Value::Array(vec![
            json!(row.artifact_ref),
            json!(principal),
            json!(host),
            dest.cloned().unwrap_or(Value::Null),
            ob_id.clone(),
            ob_rev.clone(),
        ]);
        let receipt_id = fsx::sha256_hex(serde_json::to_string(&receipt_source).unwrap_or_default().as_bytes());
        let final_delivery = obligation.is_some_and(|o| o.get("host_id").and_then(Value::as_str) == Some(host.as_str()) && o.get("destination_path") == dest);
        let now = self.now_iso();
        let collected_at = if final_delivery {
            now.clone()
        } else {
            row.collected_at.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| now.clone())
        };
        self.with_db(|db| {
            db.execute(
                "INSERT OR IGNORE INTO collection_receipts(receipt_id, task_ref, artifact_ref, obligation_id, revision, principal, host_id, destination_path, sha256, size_bytes, collected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    receipt_id,
                    row.task_ref,
                    row.artifact_ref,
                    fsx::bind_json(Some(&ob_id)),
                    fsx::bind_json(Some(&ob_rev)),
                    principal,
                    host,
                    fsx::bind_json(dest),
                    row.sha256,
                    row.size_bytes as i64,
                    now
                ],
            )?;
            db.execute(
                "UPDATE artifacts SET delivery = 'collected', collected_at = ?1 WHERE artifact_ref = ?2",
                [&collected_at, &row.artifact_ref],
            )?;
            Ok(())
        })?;
        self.expire_collected_artifacts()?;
        self.collected_reply(row)
    }

    /// storage.ts:2076-2094 `beginUpload`.
    fn begin_upload(&self, principal: &str, request: &Value) -> Result<Value> {
        let name = jsv::clip(&str_or(request, "name", "upload.bin"), 255).to_string();
        let size = jsv::to_number(request.get("size_bytes"));
        if !size.is_finite() || size < 0.0 || size > self.inner.opts.max_artifact_bytes as f64 {
            return Err(fsx::fail_not_started("BUDGET_EXCEEDED", "Upload size is missing or exceeds the artifact limit."));
        }
        if size.fract() != 0.0 {
            return Err(fail("INVALID_ARGUMENT", "size_bytes must be a whole number of bytes.", true));
        }
        let size = size as u64;
        self.assert_free_space(size)?;
        let staged_ref = id("staged");
        let file_name = format!("{staged_ref}.part");
        {
            let dir = fsx::open_dir(&self.staging_root())?;
            drop(fsx::open_child(fd_ref(&dir), &file_name, O_WRONLY | O_CREAT | O_EXCL, 0o600)??);
            fsx::fsync(fd_ref(&dir))?;
        }
        let mime = request.get("mime_type").filter(|v| jsv::truthy(Some(v))).map(to_js_string);
        let sha = request.get("sha256").filter(|v| jsv::truthy(Some(v))).map(to_js_string);
        let now = self.now_iso();
        self.with_db(|db| {
            db.execute(
                "INSERT INTO staged(staged_ref, principal, name, mime_type, size_bytes, sha256, received_bytes, complete, file_name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, ?8)",
                params![staged_ref, principal, name, mime, size as i64, sha, file_name, now],
            )?;
            Ok(())
        })?;
        let mut out = Map::new();
        out.insert("kind".into(), json!("staged"));
        out.insert("staged_ref".into(), json!(staged_ref));
        out.insert("name".into(), json!(name));
        out.insert(
            "mime_type".into(),
            match request.get("mime_type") {
                v @ Some(m) if jsv::truthy(v) => m.clone(),
                _ => json!("application/octet-stream"),
            },
        );
        out.insert("size_bytes".into(), json!(size));
        out.insert("received_bytes".into(), json!(0));
        if let Some(s) = request.get("sha256") {
            out.insert("sha256".into(), s.clone());
        }
        out.insert("complete".into(), json!(false));
        Ok(Value::Object(out))
    }

    /// storage.ts:2096-2128 `uploadChunk`: append at exactly `received_bytes`;
    /// the last chunk hashes the whole upload against the declared digest.
    fn upload_chunk(&self, principal: &str, request: &Value) -> Result<Value> {
        let staged = self.require_staged(principal, &str_or(request, "staged_ref", ""))?;
        if staged.complete {
            return Err(fail("REQUEST_CONFLICT", "Staged upload is already complete.", true));
        }
        let offset = jsv::to_number(request.get("offset"));
        if offset != staged.received_bytes as f64 {
            return Err(fail("INVALID_ARGUMENT", "Upload resume offset must equal received_bytes.", true));
        }
        let offset = staged.received_bytes;
        let data = jsv::lenient_base64(&str_or(request, "data", ""));
        let last_flag = request.get("last") == Some(&Value::Bool(true));
        if data.is_empty() && !jsv::truthy(request.get("last")) {
            return Err(fail("INVALID_ARGUMENT", "upload_chunk requires base64 data.", true));
        }
        if offset + data.len() as u64 > staged.size_bytes {
            return Err(fail("BUDGET_EXCEEDED", "Upload exceeds declared size.", true));
        }
        {
            let dir = fsx::open_dir(&self.staging_root())?;
            let fd = fsx::open_child(fd_ref(&dir), &staged.file_name, O_WRONLY, 0)??;
            fsx::pwrite_all(fd_ref(&fd), &data, offset)?;
            fsx::fsync(fd_ref(&fd))?;
        }
        let received = offset + data.len() as u64;
        let last = last_flag || received == staged.size_bytes;
        let mut sha = staged.sha256.clone();
        let mut complete = false;
        if last {
            if received != staged.size_bytes {
                return Err(fail("POSTCONDITION_FAILED", "Upload ended before declared size.", false));
            }
            let (actual, _) = self.with_staged_file(&staged.file_name, fsx::hash_fd)?;
            if staged.sha256.as_deref().is_some_and(|s| !s.is_empty() && s != actual) {
                return Err(fail("POSTCONDITION_FAILED", "Uploaded bytes do not match the declared sha256.", false));
            }
            sha = Some(actual);
            complete = true;
        }
        self.with_db(|db| {
            db.execute(
                "UPDATE staged SET received_bytes = ?1, sha256 = ?2, complete = ?3 WHERE staged_ref = ?4",
                params![received as i64, sha, i64::from(complete), staged.staged_ref],
            )?;
            Ok(())
        })?;
        if complete {
            let mime = staged.mime_type.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| fsx::mime_of(&staged.name, None).to_string());
            return Ok(json!({
                "kind": "upload_receipt",
                "staged_ref": staged.staged_ref,
                "name": staged.name,
                "mime_type": mime,
                "size_bytes": staged.size_bytes,
                "sha256": sha,
                "complete": true,
            }));
        }
        Ok(json!({
            "kind": "staged",
            "staged_ref": staged.staged_ref,
            "name": staged.name,
            "mime_type": staged.mime_type.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "application/octet-stream".into()),
            "size_bytes": staged.size_bytes,
            "received_bytes": received,
            "sha256": sha,
            "complete": false,
        }))
    }

    /// storage.ts:2130-2133 `statStaged`.
    fn stat_staged(&self, principal: &str, staged_ref: &str) -> Result<Value> {
        let s = self.require_staged(principal, staged_ref)?;
        Ok(json!({
            "kind": "staged",
            "staged_ref": s.staged_ref,
            "name": s.name,
            "mime_type": s.mime_type.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "application/octet-stream".into()),
            "size_bytes": s.size_bytes,
            "received_bytes": s.received_bytes,
            "sha256": s.sha256,
            "complete": s.complete,
        }))
    }

    /// storage.ts:2135-2139 `requireStaged`.
    pub(super) fn require_staged(&self, principal: &str, staged_ref: &str) -> Result<StagedRow> {
        fsx::assert_id(staged_ref, "staged_ref")?;
        let row = self.with_db(|db| {
            Ok(db
                .query_row(
                    "SELECT staged_ref, principal, name, mime_type, size_bytes, sha256, received_bytes, complete, file_name FROM staged WHERE staged_ref = ?1",
                    [staged_ref],
                    |r| {
                        Ok(StagedRow {
                            staged_ref: r.get(0)?,
                            principal: r.get(1)?,
                            name: r.get(2)?,
                            mime_type: r.get(3)?,
                            size_bytes: lenient_u64(r, 4)?,
                            sha256: r.get(5)?,
                            received_bytes: lenient_u64(r, 6)?,
                            complete: lenient_u64(r, 7)? != 0,
                            file_name: r.get(8)?,
                        })
                    },
                )
                .optional()?)
        })?;
        match row {
            Some(r) if r.principal == principal => Ok(r),
            _ => Err(fail("PERMISSION_DENIED", "Staged object is not owned by this principal.", false)),
        }
    }

    /// storage.ts:1958-2012 `completeInbox`: copy a completed staged upload into
    /// `operator-inbox/` under a free name. The copy streams instead of reading
    /// the whole file into memory.
    fn complete_inbox(&self, principal: &str, request: &Value) -> Result<Value> {
        let staged = self.require_staged(principal, &str_or(request, "staged_ref", ""))?;
        if !staged.complete {
            return Err(fail("INVALID_ARGUMENT", "Staged upload is not complete.", true));
        }
        let fallback = if staged.name.is_empty() { "upload.bin" } else { staged.name.as_str() };
        let requested = jsv::clip(&str_or(request, "name", fallback), 255).to_string();
        let base = jsv::basename(&requested).to_string();
        if base.is_empty() || base == "." || base == ".." || base.contains(['\\', '/', '\0']) {
            return Err(fail("INVALID_ARGUMENT", "Inbox file name is invalid.", true));
        }
        let inbox = self.inner.opts.root_dir.join("operator-inbox");
        fsx::mkdir_p(&inbox)?;
        let exists = |name: &str| fsx::lstat_path(&inbox.join(name)).is_ok();
        let mut dest_name = base.clone();
        if exists(&dest_name) {
            let (stem, ext) = match dest_name.rfind('.') {
                Some(i) => (&dest_name[..i], &dest_name[i..]),
                None => (dest_name.as_str(), ""),
            };
            let tail = &staged.staged_ref[staged.staged_ref.len().saturating_sub(8)..];
            dest_name = format!("{stem}-{tail}{ext}");
        }
        if exists(&dest_name) {
            return Err(fail("REQUEST_CONFLICT", "Inbox destination already exists.", true));
        }
        let (sha, size) = self.with_staged_file(&staged.file_name, fsx::hash_fd)?;
        if size != staged.size_bytes || staged.sha256.as_deref().is_some_and(|s| !s.is_empty() && s != sha) {
            return Err(fail("POSTCONDITION_FAILED", "Inbox bytes do not match the staged digest.", false));
        }
        let inbox_fd = fsx::open_dir(&inbox)?;
        let tmp_name = format!(".tmp.{}", staged.staged_ref);
        {
            let tmp = fsx::open_child(fd_ref(&inbox_fd), &tmp_name, O_WRONLY | O_CREAT | O_EXCL, 0o600)??;
            let copied = self.with_staged_file(&staged.file_name, |src| Ok(fsx::copy_hashing(src, fd_ref(&tmp), size)?));
            let synced = copied.and_then(|c| Ok(fsx::fsync(fd_ref(&tmp)).map(|_| c)?));
            match synced {
                Ok((copied_sha, copied_size)) if copied_sha == sha && copied_size == size => {}
                other => {
                    let _ = fsx::unlink_at(fd_ref(&inbox_fd), &tmp_name);
                    other?;
                    return Err(fail("POSTCONDITION_FAILED", "Inbox bytes do not match the staged digest.", false));
                }
            }
        }
        fsx::rename_at(fd_ref(&inbox_fd), &tmp_name, &dest_name)?;
        fsx::fsync(fd_ref(&inbox_fd))?;
        let dest_path = inbox.join(&dest_name);
        if fsx::lstat_path(&dest_path)?.size != size {
            return Err(fail("POSTCONDITION_FAILED", "Inbox file size mismatch after move.", false));
        }
        Ok(json!({
            "kind": "inbox_receipt",
            "staged_ref": staged.staged_ref,
            "name": dest_name,
            "path": dest_path,
            "size_bytes": size,
            "sha256": sha,
            "complete": true,
        }))
    }

    /// Whether an artifact with this name (and producer, if given) is ready (storage.ts:1049-1052).
    pub(super) fn find_artifact(&self, ctx: &Context, name: Option<&Value>, producer: Option<&Value>) -> Result<Option<Value>> {
        Ok(self.artifacts(&ctx.task_ref, &ctx.principal)?.into_iter().find(|a| {
            name.is_some() && a.get("name") == name && (!jsv::truthy(producer) || a.get("producer_ref") == producer)
        }))
    }
}
