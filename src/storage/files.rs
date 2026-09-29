//! Task workspace files (`computer_files`): list, read, write, publish, send,
//! import of staged uploads, and file references (storage.ts:860-882,
//! 1149-1214, 1334-1369, 1491-1659, 1675-1734).

use super::fsx::{self, O_CREAT, O_DIRECTORY, O_EXCL, O_NONBLOCK, O_RDONLY, O_WRONLY, errno, fail, fd_ref};
use super::jsv::{self, num_or, str_or, to_js_string};
use super::{Context, StorageService, ToolResult};
use crate::error::{IbaraError, Result};
use crate::ids::id;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::PathBuf;

/// storage.ts:61-69 `ResolvedFileReference`.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedFileReference {
    /// `workspace` or `artifact`.
    pub kind: &'static str,
    pub path: PathBuf,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
}

/// A present, non-null field as `String(value)`.
pub(super) fn field_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        None | Some(Value::Null) => None,
        Some(x) => Some(to_js_string(x)),
    }
}

/// Map an `openat` failure during workspace descent to the TypeScript refusals
/// (storage.ts:1508-1513). With `O_DIRECTORY`, a symlink fails as `ENOTDIR`,
/// so the entry is checked before calling it a non-directory.
fn descent_error(e: io::Error, dir: BorrowedFd<'_>, name: &str, last: bool, want_dir: bool) -> IbaraError {
    let symlink = || fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true);
    match errno(&e) {
        libc::ENOENT => fail(
            "INVALID_ARGUMENT",
            if last { "Path not found in the task workspace." } else { "Workspace path is missing." },
            true,
        ),
        libc::ELOOP | libc::EPERM => symlink(),
        libc::ENOTDIR if fsx::lstat_at(dir, name).is_ok_and(|st| st.is_symlink()) => symlink(),
        libc::ENOTDIR if last && want_dir => fail("INVALID_ARGUMENT", "Path is not a directory.", true),
        libc::ENOTDIR => fail("PERMISSION_DENIED", "Refusing a non-directory path component.", true),
        _ => e.into(),
    }
}

impl StorageService {
    /// storage.ts:860-882 `files`. Kinds: `list`, `read`, `write` (`text`, or
    /// `base64` for contract 4), `publish`, `send` (contract 4),
    /// `import_staged`, `inspect_artifact`, `read_artifact`.
    pub fn files(&self, action: &Value, ctx: &Context) -> Result<ToolResult> {
        self.ensure_open()?;
        let kind = str_or(action, "kind", "");
        if kind == "inspect_artifact" || kind == "read_artifact" {
            ctx.assert_authority()?;
            let artifact_ref = str_or(action, "artifact_ref", "");
            return if kind == "inspect_artifact" {
                Ok(ToolResult::one(self.inspect_artifact(&artifact_ref, &ctx.principal)?))
            } else {
                let max = self.inner.opts.max_output_chars as f64;
                self.read_artifact(&artifact_ref, ctx, num_or(action, "offset", 0.0), num_or(action, "max_chars", max))
            };
        }
        ctx.assert_authority()?;
        match kind.as_str() {
            "list" => self.list_files(ctx, action),
            "read" => self.read_file(ctx, action),
            "write" => self.write_file(ctx, action),
            "publish" => {
                let path = str_or(action, "path", "");
                let name = match action.get("name") {
                    v @ Some(n) if jsv::truthy(v) => to_js_string(n),
                    _ => jsv::basename(&str_or(action, "path", "artifact")).to_string(),
                };
                let evidence: Vec<String> = match action.get("evidence_refs") {
                    Some(Value::Array(items)) => items.iter().map(to_js_string).collect(),
                    _ => Vec::new(),
                };
                let producer = str_or(action, "producer_ref", "");
                let producer = (!producer.is_empty()).then_some(producer.as_str());
                Ok(ToolResult::one(self.publish_file(&path, ctx, producer, Some(&name), &evidence)?))
            }
            "send" => {
                let to = action.get("to").cloned().unwrap_or(Value::Null);
                let host = field_str(&to, "host").or_else(|| field_str(action, "host_id")).unwrap_or_default();
                let dest = field_str(&to, "path").or_else(|| field_str(action, "destination_path")).unwrap_or_default();
                let (artifact, obligation) = self.send_file(&str_or(action, "path", ""), &host, &dest, ctx)?;
                Ok(ToolResult { records: vec![artifact, json!({ "kind": "delivery_obligation", "obligation": obligation })] })
            }
            "import_staged" => self.import_staged(ctx, action),
            _ => Err(fail(
                "INVALID_ARGUMENT",
                format!("Unsupported file action: {}.", if kind.is_empty() { "missing" } else { &kind }),
                true,
            )),
        }
    }

    /// The folder a call's paths start in: the task workspace, or the
    /// controller's `base_dir` (the home folder).
    fn task_dir(&self, ctx: &Context) -> Result<PathBuf> {
        match &ctx.base_dir {
            Some(base) => Ok(base.clone()),
            None => {
                let ws = self.workspace(&ctx.task_ref, false)?;
                fsx::mkdir_p(&ws)?;
                Ok(ws)
            }
        }
    }

    /// The key a written file's writer is remembered under: its path in the
    /// workspace, or `~/` and its path in the home folder.
    fn writer_key(ctx: &Context, rel: &str) -> String {
        if ctx.base_dir.is_some() { format!("~/{rel}") } else { rel.to_string() }
    }

    /// storage.ts:1491-1529 `withWorkspace`: descend the call's folder (see
    /// [`Self::task_dir`]) one component at a time, never following a
    /// symlink, optionally creating directories, and hand the final
    /// descriptor to `f`.
    pub(super) fn with_task_dir<T>(
        &self,
        ctx: &Context,
        parts: &[String],
        last_directory: bool,
        create: bool,
        f: impl FnOnce(BorrowedFd<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut current: OwnedFd = fsx::open_dir(&self.task_dir(ctx)?)?;
        for (i, name) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            if create && (!last || last_directory) {
                fsx::check_name(name)?;
                if let Err(e) = fsx::mkdir_at(fd_ref(&current), name, 0o700)
                    && errno(&e) != libc::EEXIST
                {
                    return Err(e.into());
                }
            }
            let flags = if last && !last_directory { O_RDONLY | O_NONBLOCK } else { O_RDONLY | O_DIRECTORY };
            let fd = fsx::open_child(fd_ref(&current), name, flags, 0)?
                .map_err(|e| descent_error(e, fd_ref(&current), name, last, last_directory))?;
            let st = fsx::fstat(fd_ref(&fd))?;
            if st.is_special() {
                return Err(fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true));
            }
            if last && last_directory && !st.is_dir() {
                return Err(fail("INVALID_ARGUMENT", "Path is not a directory.", true));
            }
            if last && !last_directory && !st.is_file() {
                return Err(fail("INVALID_ARGUMENT", "Path is not a regular file.", true));
            }
            if !last && !st.is_dir() {
                return Err(fail("PERMISSION_DENIED", "Refusing a non-directory path component.", true));
            }
            current = fd;
        }
        f(fd_ref(&current))
    }

    /// storage.ts:1531-1552 `observation`.
    pub(super) fn observation(&self, ctx: &Context, text: &str, extra: Option<(usize, usize, Option<String>)>) -> Value {
        let body = jsv::clip(text, self.inner.opts.max_output_chars);
        let truncated = body.len() < text.len();
        let mut coverage = Map::new();
        coverage.insert("scope".into(), json!(format!("task:{}", ctx.task_ref)));
        coverage.insert("semantics".into(), json!(if truncated { "partial" } else { "complete" }));
        coverage.insert("truncated".into(), json!(truncated));
        if let Some((returned, available, continuation)) = extra {
            coverage.insert("returned_count".into(), json!(returned));
            coverage.insert("available_count".into(), json!(available));
            if let Some(c) = continuation {
                coverage.insert("continuation".into(), json!(c));
            }
        }
        json!({
            "kind": "observation",
            "observation_ref": id("obs"),
            "epoch": ctx.epoch,
            "captured_at": crate::ids::now_iso(),
            "source": "filesystem",
            "coverage": coverage,
            "content": { "untrusted": true, "text": body },
            "blockers": [],
            "conflicts": [],
        })
    }

    /// storage.ts:1554-1581 `listFiles`.
    fn list_files(&self, ctx: &Context, action: &Value) -> Result<ToolResult> {
        let parts = fsx::parse_rel(field_str(action, "path").as_deref(), ".")?;
        let limit = jsv::clamp_or(num_or(action, "limit", 100.0), 1.0, 100.0, 100.0) as usize;
        let start = jsv::parse_int_or_zero(&str_or(action, "continuation", "0"));
        let entries = self.with_task_dir(ctx, &parts, true, false, |fd| {
            let mut names: Vec<String> = fsx::read_dir_fd(fd)?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort_by(|a, b| jsv::utf16_cmp(a, b));
            Ok(names
                .into_iter()
                .map(|name| match fsx::open_child(fd, &name, O_RDONLY | O_NONBLOCK, 0) {
                    Ok(Ok(child)) => match fsx::fstat(fd_ref(&child)) {
                        Ok(st) if st.is_dir() => format!("{name}/"),
                        Ok(st) if st.is_file() => format!("{name} {}", st.size),
                        Ok(_) => format!("{name} skipped-special"),
                        Err(_) => format!("{name} unreadable"),
                    },
                    _ => format!("{name} unreadable"),
                })
                .collect::<Vec<_>>())
        })?;
        let page: Vec<&String> = entries.iter().skip(start).take(limit).collect();
        let more = (start + page.len() < entries.len()).then(|| (start + page.len()).to_string());
        let text = if page.is_empty() {
            "(empty directory)".to_string()
        } else {
            page.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
        };
        Ok(ToolResult::one(self.observation(ctx, &text, Some((page.len(), entries.len(), more)))))
    }

    /// storage.ts:1583-1596 `readFile`.
    fn read_file(&self, ctx: &Context, action: &Value) -> Result<ToolResult> {
        let parts = fsx::parse_rel(Some(&str_or(action, "path", "")), ".")?;
        if parts.is_empty() {
            return Err(fail("INVALID_ARGUMENT", "read requires a file path.", true));
        }
        let offset = num_or(action, "offset", 0.0).max(0.0);
        let offset = if offset.is_finite() { offset as u64 } else { 0 };
        let max_out = self.inner.opts.max_output_chars;
        let max_chars = jsv::clamp_or(num_or(action, "max_chars", max_out as f64), 1.0, max_out as f64, max_out as f64) as usize;
        let max_file = self.inner.opts.max_file_bytes;
        let text = self.with_task_dir(ctx, &parts, false, false, |fd| {
            let st = fsx::fstat(fd)?;
            if st.size > max_file {
                return Err(fsx::fail_not_started("BUDGET_EXCEEDED", "File exceeds the configured size limit."));
            }
            let buf = fsx::read_fd_bytes(fd, offset, st.size.min(max_chars as u64 * 4))?;
            if buf.contains(&0) {
                return Err(fail("WRONG_TOOL", "Binary workspace files use artifact transfer, not text read.", true));
            }
            Ok(String::from_utf8_lossy(&buf).into_owned())
        })?;
        Ok(ToolResult::one(self.observation(ctx, &text, None)))
    }

    /// storage.ts:1598-1626 `writeFile`. Contract 4 also accepts `base64`
    /// content (at most `chunk_bytes` decoded bytes).
    fn write_file(&self, ctx: &Context, action: &Value) -> Result<ToolResult> {
        let parts = fsx::parse_rel(Some(&str_or(action, "path", "")), ".")?;
        if parts.is_empty() {
            return Err(fail("INVALID_ARGUMENT", "write requires a file path.", true));
        }
        let data: Vec<u8> = match (action.get("text").filter(|v| !v.is_null()), action.get("base64")) {
            (Some(_), Some(Value::String(_))) => {
                return Err(fail("INVALID_ARGUMENT", "write takes text or base64, not both.", true));
            }
            (None, Some(Value::String(b64))) => {
                if !jsv::strict_base64_re(b64) {
                    return Err(fail("INVALID_ARGUMENT", "write base64 is not valid padded base64.", true));
                }
                let bytes = jsv::lenient_base64(b64);
                if bytes.len() > self.inner.opts.chunk_bytes {
                    return Err(fail(
                        "INVALID_ARGUMENT",
                        format!("write base64 exceeds {} bytes.", self.inner.opts.chunk_bytes),
                        true,
                    ));
                }
                bytes
            }
            (text, _) => {
                let text = text.map(to_js_string).unwrap_or_default();
                if jsv::utf16_len(&text) > 64000 {
                    return Err(fail("INVALID_ARGUMENT", "write text exceeds 64000 characters.", true));
                }
                text.into_bytes()
            }
        };
        self.assert_free_space(data.len() as u64)?;
        let overwrite = action.get("overwrite") == Some(&Value::Bool(true));
        let expected = str_or(action, "expected_sha256", "");
        let (parent, name) = parts.split_at(parts.len() - 1);
        let name = &name[0];
        self.with_task_dir(ctx, parent, true, true, |dir| {
            match fsx::open_child(dir, name, O_RDONLY | O_NONBLOCK, 0)? {
                Ok(existing) => {
                    if !overwrite {
                        return Err(fail(
                            "REQUEST_CONFLICT",
                            "Refusing to overwrite without overwrite=true and expected_sha256.",
                            true,
                        ));
                    }
                    let (current, _) = fsx::hash_fd(fd_ref(&existing))?;
                    if current != expected {
                        return Err(fail(
                            "REQUEST_CONFLICT",
                            "existing file hash does not match expected_sha256; refusing to discard concurrent edits.",
                            true,
                        ));
                    }
                }
                Err(e) if errno(&e) == libc::ENOENT => {
                    if overwrite {
                        return Err(fail(
                            "INVALID_ARGUMENT",
                            "overwrite requires an existing file with expected_sha256.",
                            true,
                        ));
                    }
                }
                Err(e) if errno(&e) == libc::ELOOP => {
                    return Err(fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true));
                }
                Err(e) => return Err(e.into()),
            }
            fsx::write_atomic_in_dir(dir, name, &data, 0o600)
        })?;
        let rel = parts.join("/");
        self.record_writer(ctx, &Self::writer_key(ctx, &rel), &fsx::sha256_hex(&data), data.len() as u64)?;
        let text = format!("Wrote {rel} ({} bytes).", data.len());
        Ok(ToolResult::one(self.observation(ctx, &text, None)))
    }

    /// Remember the operation that wrote a workspace path, so a later publish
    /// of the same bytes can name it as the producer (contract 4).
    fn record_writer(&self, ctx: &Context, rel: &str, sha256: &str, size: u64) -> Result<()> {
        let Some(op) = ctx.operation_ref.as_deref().filter(|s| !s.is_empty()) else { return Ok(()) };
        let now = self.now_iso();
        self.with_db(|db| {
            db.execute(
                "INSERT INTO file_writers(task_ref, path, operation_ref, sha256, size_bytes, written_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(task_ref, path) DO UPDATE SET operation_ref=excluded.operation_ref, sha256=excluded.sha256, size_bytes=excluded.size_bytes, written_at=excluded.written_at",
                params![ctx.task_ref, rel, op, sha256, size as i64, now],
            )?;
            Ok(())
        })
    }

    /// The recorded writer of `rel` if the file still holds the bytes it wrote.
    fn recorded_writer(&self, task_ref: &str, rel: &str, sha256: &str) -> Result<Option<String>> {
        self.with_db(|db| {
            Ok(db
                .query_row(
                    "SELECT operation_ref FROM file_writers WHERE task_ref = ?1 AND path = ?2 AND sha256 = ?3",
                    params![task_ref, rel, sha256],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
        })
    }

    /// storage.ts:1628-1659 `importStaged`.
    fn import_staged(&self, ctx: &Context, action: &Value) -> Result<ToolResult> {
        let staged = self.require_staged(&ctx.principal, &str_or(action, "staged_ref", ""))?;
        if !staged.complete {
            return Err(fail("INVALID_ARGUMENT", "Staged upload is not complete.", true));
        }
        let dest = fsx::parse_rel(Some(&str_or(action, "destination", "")), ".")?;
        if dest.is_empty() {
            return Err(fail("INVALID_ARGUMENT", "import_staged requires a destination path.", true));
        }
        self.assert_free_space(staged.size_bytes)?;
        let (parent, name) = dest.split_at(dest.len() - 1);
        let name = &name[0];
        let (sha, size) = self.with_staged_file(&staged.file_name, |src| {
            let (sha, size) = fsx::hash_fd(src)?;
            if staged.sha256.as_deref().is_some_and(|s| !s.is_empty() && s != sha) {
                return Err(fail("POSTCONDITION_FAILED", "Staged file hash does not match the declared digest.", false));
            }
            self.with_task_dir(ctx, parent, true, true, |dir| {
                let tmp = format!(".tmp.{}", &id("imp")[..20]);
                let out = fsx::open_child(dir, &tmp, O_WRONLY | O_CREAT | O_EXCL, 0o600)??;
                let copied = fsx::copy_hashing(src, fd_ref(&out), size).and_then(|c| fsx::fsync(fd_ref(&out)).map(|_| c));
                drop(out);
                match copied {
                    Ok((copied_sha, copied_size)) if copied_sha == sha && copied_size == size => {}
                    other => {
                        let _ = fsx::unlink_at(dir, &tmp);
                        other?;
                        return Err(fail("STALE_TARGET", "Staged file changed during import.", true));
                    }
                }
                fsx::check_name(name)?;
                fsx::rename_at(dir, &tmp, name)?;
                fsx::fsync(dir)?;
                Ok(())
            })?;
            Ok((sha, size))
        })?;
        let rel = dest.join("/");
        self.record_writer(ctx, &Self::writer_key(ctx, &rel), &sha, size)?;
        Ok(ToolResult::one(self.observation(ctx, &format!("Imported staged object into {rel}."), None)))
    }

    /// storage.ts:1675-1711 `openPublishSource`: a path relative to the call's
    /// folder (see [`Self::task_dir`]); the controller turns an agent's
    /// absolute path into one.
    fn open_publish_source(&self, file_path: &str, ctx: &Context) -> Result<OwnedFd> {
        let parts = fsx::parse_rel(Some(file_path), ".")?;
        let mut current = fsx::open_dir(&self.task_dir(ctx)?)?;
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            let flags = if last { O_RDONLY | O_NONBLOCK } else { O_RDONLY | O_DIRECTORY };
            current = fsx::open_child(fd_ref(&current), part, flags, 0)?
                .map_err(|e| descent_error(e, fd_ref(&current), part, last, false))?;
        }
        Ok(current)
    }

    /// storage.ts:1149-1214 `publishFile`: copy the source into
    /// `artifacts/<ref>.bin` (mode 0444) and record it as `available`.
    ///
    /// The producer is `operation_ref` when given; otherwise the recorded
    /// writer of those exact bytes at that path (contract 4); otherwise an
    /// opaque `producer_` reference that claims no author.
    pub fn publish_file(
        &self,
        file_path: &str,
        ctx: &Context,
        operation_ref: Option<&str>,
        name: Option<&str>,
        evidence_refs: &[String],
    ) -> Result<Value> {
        self.ensure_open()?;
        ctx.assert_authority()?;
        let fallback_name = jsv::basename(file_path);
        let name = name
            .filter(|n| !n.is_empty())
            .or((!fallback_name.is_empty()).then_some(fallback_name))
            .unwrap_or("artifact");
        let name = jsv::clip(name, 255).to_string();
        let src = self.open_publish_source(file_path, ctx)?;
        let st = fsx::fstat(fd_ref(&src))?;
        let task_limit = ctx.max_download_bytes.unwrap_or(self.inner.opts.max_artifact_bytes);
        if !st.is_file() || st.size > self.inner.opts.max_artifact_bytes.min(task_limit) {
            return Err(fsx::fail_not_started(
                "BUDGET_EXCEEDED",
                "Publish source is missing, not a complete regular file, or exceeds the artifact size limit.",
            ));
        }
        self.assert_free_space(st.size)?;
        let artifact_ref = id("artifact");
        let dest_name = format!("{artifact_ref}.bin");
        let dir = fsx::open_dir(&self.artifact_root())?;
        let tmp = format!(".tmp.{artifact_ref}");
        let copied = (|| -> Result<(String, u64)> {
            let out = fsx::open_child(fd_ref(&dir), &tmp, O_WRONLY | O_CREAT | O_EXCL, 0o600)??;
            let copied = fsx::copy_hashing(fd_ref(&src), fd_ref(&out), st.size)?;
            fsx::fsync(fd_ref(&out))?;
            drop(out);
            let verify = fsx::open_child(fd_ref(&dir), &tmp, O_RDONLY, 0)??;
            fsx::fchmod(fd_ref(&verify), 0o444)?;
            drop(verify);
            fsx::rename_at(fd_ref(&dir), &tmp, &dest_name)?;
            fsx::fsync(fd_ref(&dir))?;
            Ok(copied)
        })();
        let (sha256, size) = match copied {
            Ok(c) => c,
            Err(e) => {
                let _ = fsx::unlink_at(fd_ref(&dir), &tmp);
                return Err(e);
            }
        };
        drop(dir);
        drop(src);
        let explicit = operation_ref.filter(|s| !s.is_empty()).map(str::to_string);
        let linked = match &explicit {
            None => {
                let rel = fsx::parse_rel(Some(file_path), ".")?.join("/");
                self.recorded_writer(&ctx.task_ref, &Self::writer_key(ctx, &rel), &sha256)?
            }
            Some(_) => None,
        };
        let author = explicit.or(linked);
        let head = self.with_artifact_file(&dest_name, |fd| fsx::read_fd_bytes(fd, 0, 16))?;
        let mime = fsx::mime_of(&name, Some(&head));
        let producer_ref = author.clone().unwrap_or_else(|| id("producer"));
        let evidence: Vec<String> = if evidence_refs.is_empty() {
            vec![author.unwrap_or_else(|| id("evidence"))]
        } else {
            evidence_refs.iter().take(20).cloned().collect()
        };
        let record = json!({
            "kind": "artifact",
            "artifact_ref": artifact_ref,
            "task_ref": ctx.task_ref,
            "name": name,
            "mime_type": mime,
            "size_bytes": size,
            "sha256": sha256,
            "producer_ref": producer_ref,
            "evidence_refs": evidence,
            "ready": true,
            "delivery": "available",
        });
        let now = self.now_iso();
        self.with_db(|db| {
            db.execute(
                "INSERT INTO artifacts(artifact_ref, task_ref, principal, name, mime_type, size_bytes, sha256, producer_ref, evidence_json, delivery, file_name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    artifact_ref,
                    ctx.task_ref,
                    ctx.principal,
                    name,
                    mime,
                    size as i64,
                    sha256,
                    producer_ref,
                    serde_json::to_string(&evidence).unwrap_or_else(|_| "[]".into()),
                    "available",
                    dest_name,
                    now
                ],
            )?;
            Ok(())
        })?;
        Ok(record)
    }

    /// Contract 4 `computer_files send`: publish `path` and return the delivery
    /// obligation for `host_id:destination_path`, which the engine adds to the
    /// task (`tasks.deliveries`). The obligation is verified by a matching
    /// collection receipt, exactly like one named at `computer_begin`. The
    /// controller resolves `host_id` to the id collections are recorded under
    /// ([`Self::collector_host`]) before it gets here.
    pub fn send_file(&self, path: &str, host_id: &str, destination_path: &str, ctx: &Context) -> Result<(Value, Value)> {
        if !jsv::id_re(host_id)
            || host_id.len() > 256
            || !destination_path.starts_with('/')
            || jsv::utf16_len(destination_path) > 4096
            || jsv::posix_normalize(destination_path) != destination_path
        {
            return Err(fail("INVALID_ARGUMENT", "Send to a computer's id and a normalized absolute destination.", true));
        }
        let artifact = self.publish_file(path, ctx, None, None, &[])?;
        let obligation = json!({
            "id": id("delivery"),
            "destination": "artifact_collection",
            "path": artifact["name"],
            "host_id": host_id,
            "destination_path": destination_path,
            "required": true,
            "revision": 1,
            "artifact_ref": artifact["artifact_ref"],
            "sha256": artifact["sha256"],
            "size_bytes": artifact["size_bytes"],
            "mime_type": artifact["mime_type"],
        });
        Ok((artifact, obligation))
    }

    /// storage.ts:1334-1369 `resolveFileReference`.
    pub fn resolve_file_reference(&self, reference: &str, ctx: &Context) -> Result<ResolvedFileReference> {
        self.ensure_open()?;
        ctx.assert_authority()?;
        if reference.is_empty() {
            return Err(fail("INVALID_ARGUMENT", "File reference is required.", true));
        }
        if let Some(artifact) = self.lookup_artifact(reference)? {
            if artifact.principal != ctx.principal || artifact.task_ref != ctx.task_ref {
                return Err(fail("PERMISSION_DENIED", "Artifact is not owned by this principal and task.", false));
            }
            self.require_artifact_bytes(&artifact)?;
            let path = self.with_artifact_file(&artifact.file_name, fsx::fd_path)?;
            return Ok(ResolvedFileReference {
                kind: "artifact",
                path,
                name: artifact.name.clone(),
                mime_type: artifact.mime_type.clone(),
                size_bytes: artifact.size_bytes,
                sha256: artifact.sha256.clone(),
                artifact_ref: Some(artifact.artifact_ref.clone()),
            });
        }
        let parts = fsx::parse_rel(Some(reference), ".")?;
        self.with_task_dir(ctx, &parts, false, false, |fd| {
            let (sha256, size) = fsx::hash_fd(fd)?;
            let name = parts.last().cloned().unwrap_or_else(|| reference.to_string());
            let head = fsx::read_fd_bytes(fd, 0, 16)?;
            Ok(ResolvedFileReference {
                kind: "workspace",
                path: fsx::fd_path(fd)?,
                mime_type: fsx::mime_of(&name, Some(&head)).to_string(),
                name,
                size_bytes: size,
                sha256,
                artifact_ref: None,
            })
        })
    }

    /// storage.ts:1728-1734 `withStagedFile`.
    pub(super) fn with_staged_file<T>(&self, file_name: &str, f: impl FnOnce(BorrowedFd<'_>) -> Result<T>) -> Result<T> {
        let dir = fsx::open_dir(&self.staging_root())?;
        let fd = fsx::open_child(fd_ref(&dir), file_name, O_RDONLY | O_NONBLOCK, 0)??;
        f(fd_ref(&fd))
    }
}
