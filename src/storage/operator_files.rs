//! Operator file roots: the human Files tab's listing, chunked uploads with
//! inode-identity reconciliation, and chunked downloads (storage.ts:309-661).
//!
//! Upload jobs live in `operator_file_jobs` and are read on demand instead of
//! all being loaded at start. Downloads hold an open descriptor and live only
//! in memory; idle ones are dropped after five minutes and at most
//! [`MAX_DOWNLOADS`] exist at once.

use super::fsx::{self, O_CREAT, O_DIRECTORY, O_EXCL, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, errno, fail, fd_ref};
use super::jsv::{self, str_or};
use super::StorageService;
use crate::error::Result;
use crate::ids::{id, iso_from_millis};
use base64::Engine;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Concurrent operator downloads (each holds one descriptor).
pub const MAX_DOWNLOADS: usize = 64;
/// A download untouched for this long is dropped.
pub const DOWNLOAD_IDLE_MS: i64 = 300_000;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_CHUNK_BASE64: usize = 1_400_000;

/// storage.ts:309-328 `OperatorFileJob` for uploads.
#[derive(Debug, Clone)]
struct UploadJob {
    job_id: String,
    owner: String,
    root_id: String,
    relative: String,
    name: String,
    size: u64,
    sha256: String,
    staging: PathBuf,
    offset: u64,
    state: String,
    dest_dev: Option<String>,
    dest_ino: Option<String>,
    staging_dev: Option<String>,
    staging_ino: Option<String>,
    pub_dev: Option<String>,
    pub_ino: Option<String>,
    pub_birth_ns: Option<String>,
}

impl UploadJob {
    fn relative_path(&self) -> String {
        [self.relative.as_str(), self.name.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("/")
    }
}

/// An operator download: an open, verified descriptor.
pub(super) struct DownloadJob {
    owner: String,
    root_id: String,
    relative: String,
    name: String,
    size: u64,
    sha256: String,
    offset: u64,
    fd: OwnedFd,
    file_version: String,
    last_used_ms: i64,
}

impl StorageService {
    fn load_upload_job(&self, job_id: &str) -> Result<Option<UploadJob>> {
        if job_id.is_empty() {
            return Ok(None);
        }
        self.with_db(|db| {
            Ok(db
                .query_row(
                    "SELECT job_id, owner, root_id, relative_directory, name, size_bytes, sha256, staging, offset_bytes, state, dest_dev, dest_ino, staging_dev, staging_ino, pub_dev, pub_ino, pub_birth_ns FROM operator_file_jobs WHERE job_id = ?1",
                    [job_id],
                    |r| {
                        let pair = |a: Option<String>, b: Option<String>| match (a, b) {
                            (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (Some(a), Some(b)),
                            _ => (None, None),
                        };
                        let (dest_dev, dest_ino) = pair(r.get(10)?, r.get(11)?);
                        let (staging_dev, staging_ino) = pair(r.get(12)?, r.get(13)?);
                        let (pub_dev, pub_ino, pub_birth_ns): (Option<String>, Option<String>, Option<String>) =
                            (r.get(14)?, r.get(15)?, r.get(16)?);
                        let published = pub_dev.is_some() && pub_ino.is_some() && pub_birth_ns.is_some();
                        Ok(UploadJob {
                            job_id: r.get(0)?,
                            owner: r.get(1)?,
                            root_id: r.get(2)?,
                            relative: r.get(3)?,
                            name: r.get(4)?,
                            size: r.get::<_, i64>(5)?.max(0) as u64,
                            sha256: r.get(6)?,
                            staging: PathBuf::from(r.get::<_, String>(7)?),
                            offset: r.get::<_, i64>(8)?.max(0) as u64,
                            state: r.get(9)?,
                            dest_dev,
                            dest_ino,
                            staging_dev,
                            staging_ino,
                            pub_dev: if published { pub_dev } else { None },
                            pub_ino: if published { pub_ino } else { None },
                            pub_birth_ns: if published { pub_birth_ns } else { None },
                        })
                    },
                )
                .optional()?
                .filter(|job| matches!(job.state.as_str(), "uploading" | "publishing" | "uncertain" | "published")))
        })
    }

    /// storage.ts:334-342 `saveOperatorJob`.
    fn save_upload_job(&self, job: &UploadJob) -> Result<()> {
        self.with_db(|db| {
            db.execute(
                "INSERT INTO operator_file_jobs (job_id, owner, root_id, relative_directory, name, size_bytes, sha256, staging, offset_bytes, state, dest_dev, dest_ino, staging_dev, staging_ino, pub_dev, pub_ino, pub_birth_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(job_id) DO UPDATE SET offset_bytes=excluded.offset_bytes, state=excluded.state, pub_dev=excluded.pub_dev, pub_ino=excluded.pub_ino, pub_birth_ns=excluded.pub_birth_ns",
                params![
                    job.job_id,
                    job.owner,
                    job.root_id,
                    job.relative,
                    job.name,
                    job.size as i64,
                    job.sha256,
                    job.staging.to_string_lossy(),
                    job.offset as i64,
                    job.state,
                    job.dest_dev,
                    job.dest_ino,
                    job.staging_dev,
                    job.staging_ino,
                    job.pub_dev,
                    job.pub_ino,
                    job.pub_birth_ns
                ],
            )?;
            Ok(())
        })
    }

    /// storage.ts:344-523 `operatorFiles`: the `files_*` operator actions. The
    /// caller has authenticated `operator_id` and checked its current grant.
    pub fn operator_files(&self, operator_id: &str, action: &Value) -> Result<Value> {
        self.ensure_open()?;
        let op = str_or(action, "op", "");
        if op == "files_roots" {
            let roots: Vec<Value> = self.inner.operator_roots.iter().map(|(root_id, _)| json!({ "root_id": root_id })).collect();
            return Ok(json!({ "roots": roots, "max_file_bytes": self.inner.opts.max_artifact_bytes }));
        }
        let root_id = str_or(action, "root_id", "");
        let job_id = str_or(action, "job_id", "");
        let now = self.clock();
        {
            // Downloads first: they are memory-only.
            let mut downloads = self.inner.downloads.lock();
            downloads.retain(|_, d| now - d.last_used_ms < DOWNLOAD_IDLE_MS);
            if let Some(d) = downloads.get_mut(&job_id) {
                if d.owner != operator_id {
                    return Err(fail("PERMISSION_DENIED", "Unknown operator file job.", true));
                }
                match op.as_str() {
                    "files_status" => {
                        return Ok(json!({
                            "job_id": job_id,
                            "state": "downloading",
                            "root_id": d.root_id,
                            "relative_path": ([d.relative.as_str(), d.name.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("/")),
                            "offset": d.offset,
                            "size_bytes": d.size,
                            "sha256": d.sha256,
                        }));
                    }
                    "files_resume_upload" => {
                        return Err(fail("PERMISSION_DENIED", "Unknown resumable operator upload.", true));
                    }
                    "files_download_chunk" => {
                        d.last_used_ms = now;
                        let (bytes, complete) = self.download_chunk_op(d, action)?;
                        let offset = d.offset;
                        if complete {
                            downloads.remove(&job_id);
                        }
                        return Ok(json!({
                            "job_id": job_id,
                            "offset": offset,
                            "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                            "complete": complete,
                        }));
                    }
                    "files_upload_chunk" => {
                        return Err(fail("INVALID_ARGUMENT", "Invalid upload chunk or offset.", true));
                    }
                    "files_publish" => return Err(fail("INVALID_ARGUMENT", "Upload is incomplete.", true)),
                    _ => {}
                }
            }
        }
        let job = self.load_upload_job(&job_id)?;
        match op.as_str() {
            "files_status" => {
                let job = job.filter(|j| j.owner == operator_id).ok_or_else(|| fail("PERMISSION_DENIED", "Unknown operator file job.", true))?;
                return Ok(json!({
                    "job_id": job_id,
                    "state": job.state,
                    "root_id": job.root_id,
                    "relative_path": job.relative_path(),
                    "offset": job.offset,
                    "size_bytes": job.size,
                    "sha256": job.sha256,
                }));
            }
            "files_resume_upload" => {
                let mut job = job
                    .filter(|j| j.owner == operator_id && matches!(j.state.as_str(), "uploading" | "uncertain" | "publishing"))
                    .ok_or_else(|| fail("PERMISSION_DENIED", "Unknown resumable operator upload.", true))?;
                let prefix = match action.get("prefix_sha256") {
                    Some(Value::String(s)) => s.clone(),
                    _ => return Err(fail("STALE_TARGET", "Upload source or chosen destination changed.", true)),
                };
                if action.get("root_id") != Some(&json!(job.root_id))
                    || action.get("relative_path") != Some(&json!(job.relative_path()))
                    || !jsv::strict_eq_num(action.get("size"), job.size as f64)
                    || action.get("sha256") != Some(&json!(job.sha256))
                {
                    return Err(fail("STALE_TARGET", "Upload source or chosen destination changed.", true));
                }
                if job.state != "uploading" {
                    return self.reconcile_operator_publication(&mut job, &prefix);
                }
                self.assert_destination_available(&job)?;
                self.verified_staging_offset(&job, &prefix)?;
                return Ok(json!({ "job_id": job_id, "state": job.state, "offset": job.offset, "size_bytes": job.size, "sha256": job.sha256 }));
            }
            "files_upload_chunk" | "files_publish" | "files_download_chunk" => {
                let mut job = job.filter(|j| j.owner == operator_id).ok_or_else(|| fail("PERMISSION_DENIED", "Unknown operator file job.", true))?;
                return match op.as_str() {
                    "files_upload_chunk" => self.upload_chunk_op(&mut job, action),
                    "files_publish" => self.publish_op(&mut job),
                    _ => Err(fail("INVALID_ARGUMENT", "Invalid download chunk or offset.", true)),
                };
            }
            _ => {}
        }
        let root = self.operator_root_fd(&root_id)?;
        match op.as_str() {
            "files_list" => self.list_op(&root_id, fd_ref(&root), action),
            "files_begin_upload" => self.begin_upload_op(operator_id, &root_id, fd_ref(&root), action),
            "files_begin_download" => self.begin_download_op(operator_id, &root_id, fd_ref(&root), action),
            _ => Err(fail("INVALID_ARGUMENT", "Unknown operator file action.", true)),
        }
    }

    /// storage.ts:471-481 `files_list`.
    fn list_op(&self, root_id: &str, root: BorrowedFd<'_>, action: &Value) -> Result<Value> {
        let parent = self.operator_relative_fd(root, &str_or(action, "relative_directory", "."))?;
        let mut entries = Vec::new();
        for entry in fsx::read_dir_fd(fd_ref(&parent))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(ft) = entry.file_type() else { continue };
            if name.starts_with('.') || !(ft.is_dir() || ft.is_file()) {
                continue;
            }
            entries.push((name, ft.is_dir()));
            if entries.len() > 100 {
                return Err(fail("BUDGET_EXCEEDED", "Directory listing exceeds 100 entries; narrow the folder.", true));
            }
        }
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|(name, is_dir)| {
                // Size and change time for a human file list; lstat never follows a link.
                let (mut size, mut modified) = (Value::Null, String::new());
                if let Ok(st) = fsx::lstat_at(fd_ref(&parent), &name) {
                    if !is_dir {
                        size = json!(st.size);
                    }
                    modified = iso_from_millis(st.mtime_ns.div_euclid(1_000_000) as i64);
                }
                json!({ "name": name, "kind": if is_dir { "directory" } else { "file" }, "size": size, "modified": modified })
            })
            .collect();
        Ok(json!({ "root_id": root_id, "entries": entries }))
    }

    /// storage.ts:482-502 `files_begin_upload`: bind the chosen destination
    /// directory and a fresh staging file by `(dev, ino)`.
    fn begin_upload_op(&self, operator_id: &str, root_id: &str, root: BorrowedFd<'_>, action: &Value) -> Result<Value> {
        let name = str_or(action, "name", "");
        fsx::check_name(&name)?;
        let size = jsv::safe_integer(action.get("size"));
        let sha = str_or(action, "sha256", "");
        let limit = self.inner.opts.max_artifact_bytes;
        let size = match size {
            Some(s) if s >= 1 && s as u64 > limit => return Err(fail("BUDGET_EXCEEDED", file_too_large(s as u64, limit), true)),
            Some(s) if !name.starts_with('.') && s >= 1 && jsv::sha256_re(&sha) => s as u64,
            _ => return Err(fail("INVALID_ARGUMENT", "Invalid upload metadata.", true)),
        };
        let relative = str_or(action, "relative_directory", ".");
        let parent = self.operator_relative_fd(root, &relative)?;
        if destination_occupied(fd_ref(&parent), &name)? {
            return Err(fail("INVALID_ARGUMENT", "Destination already exists.", true));
        }
        let parent_stat = fsx::fstat(fd_ref(&parent))?;
        let dir = self.inner.opts.state_dir.join("operator-upload");
        fsx::mkdir_p(&dir)?;
        self.assert_operator_space(&fsx::proc_fd(fd_ref(&parent)), size)?;
        self.assert_operator_space(&dir, size)?;
        let identity = str_or(action, "job_id", "");
        fsx::assert_id(&identity, "job_id")?;
        let used = self.inner.downloads.lock().contains_key(&identity)
            || self.with_db(|db| {
                Ok(db
                    .query_row("SELECT 1 FROM operator_file_jobs WHERE job_id = ?1", [&identity], |_| Ok(()))
                    .optional()?
                    .is_some())
            })?;
        if used {
            return Err(fail("INVALID_ARGUMENT", "File job identity already used.", true));
        }
        let staging = dir.join(format!("{identity}.part"));
        let staging_stat = {
            let fd = fsx::open_path(&staging, O_WRONLY | O_CREAT | O_EXCL | fsx::O_NOFOLLOW, 0o600)?;
            fsx::fstat(fd_ref(&fd))?
        };
        let job = UploadJob {
            job_id: identity.clone(),
            owner: operator_id.to_string(),
            root_id: root_id.to_string(),
            relative: if relative == "." { String::new() } else { relative },
            name,
            size,
            sha256: sha,
            staging,
            offset: 0,
            state: "uploading".into(),
            dest_dev: Some(parent_stat.dev.to_string()),
            dest_ino: Some(parent_stat.ino.to_string()),
            staging_dev: Some(staging_stat.dev.to_string()),
            staging_ino: Some(staging_stat.ino.to_string()),
            pub_dev: None,
            pub_ino: None,
            pub_birth_ns: None,
        };
        self.save_upload_job(&job)?;
        Ok(json!({ "job_id": identity, "state": "uploading", "offset": 0 }))
    }

    /// storage.ts:366-394 `files_upload_chunk`: exactly at the retained offset,
    /// into the bound staging inode, rewound if the write fails part-way.
    fn upload_chunk_op(&self, job: &mut UploadJob, action: &Value) -> Result<Value> {
        let b64 = match action.get("base64") {
            Some(Value::String(s)) => s.as_str(),
            _ => return Err(fail("INVALID_ARGUMENT", "Invalid upload chunk or offset.", true)),
        };
        if job.state != "uploading"
            || !jsv::strict_eq_num(action.get("offset"), job.offset as f64)
            || b64.len() > MAX_CHUNK_BASE64
            || !jsv::strict_base64_re(b64)
        {
            return Err(fail("INVALID_ARGUMENT", "Invalid upload chunk or offset.", true));
        }
        let bytes = jsv::lenient_base64(b64);
        if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES || job.offset + bytes.len() as u64 > job.size {
            return Err(fail("BUDGET_EXCEEDED", "Upload exceeds declared bound.", true));
        }
        self.assert_destination_available(job)?;
        let fd = fsx::open_path(&job.staging, O_WRONLY | fsx::O_NOFOLLOW, 0)?;
        let mut writing = false;
        let result = (|| -> Result<()> {
            assert_staging_identity(fd_ref(&fd), job)?;
            if fsx::fstat(fd_ref(&fd))?.size != job.offset {
                return Err(fail("STALE_TARGET", "Upload staging changed.", true));
            }
            self.assert_operator_space(&fsx::proc_fd(fd_ref(&fd)), bytes.len() as u64)?;
            writing = true;
            fsx::pwrite_all(fd_ref(&fd), &bytes, job.offset)
                .map_err(|e| if e.raw_os_error() == Some(libc::EIO) { fail("STALE_TARGET", "Upload write made no progress.", true) } else { e.into() })?;
            fsx::fsync(fd_ref(&fd))?;
            job.offset += bytes.len() as u64;
            self.save_upload_job(job)
        })();
        if let Err(e) = result {
            if writing {
                rewind_staging(fd_ref(&fd), job);
            }
            return Err(e);
        }
        Ok(json!({ "job_id": job.job_id, "state": job.state, "offset": job.offset }))
    }

    /// storage.ts:395-453 `files_publish`: mark `publishing` durably, copy into
    /// a temporary name in the destination, bind its inode, hard-link it to the
    /// chosen name, and only then mark `published`.
    fn publish_op(&self, job: &mut UploadJob) -> Result<Value> {
        if job.state != "uploading" || job.offset != job.size {
            return Err(fail("INVALID_ARGUMENT", "Upload is incomplete.", true));
        }
        let parent = self.open_operator_destination(job)?;
        let parent = fd_ref(&parent);
        if destination_occupied(parent, &job.name)? {
            return Err(fail("INVALID_ARGUMENT", "Destination already exists.", true));
        }
        self.assert_operator_space(&fsx::proc_fd(parent), job.size)?;
        self.verified_staging_offset(job, &job.sha256.clone())?;
        // A lost response is not permission to replay publication. Durably mark
        // the uncertain phase before the first irreversible link operation.
        job.state = "publishing".into();
        self.save_upload_job(job)?;
        if let Some(delay) = std::env::var("IBARA_TEST_PUBLISH_DELAY_MS").ok().and_then(|ms| ms.parse::<u64>().ok()) {
            // Tests: a final check as slow as a big file's on a slow disk.
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        let temp_name = format!(".ibara-upload-{}", id("tmp"));
        fsx::check_name(&temp_name)?;
        let source = fsx::open_path(&job.staging, O_RDONLY | fsx::O_NOFOLLOW, 0)?;
        let mut created = false;
        let outcome = (|| -> Result<()> {
            assert_staging_identity(fd_ref(&source), job)?;
            let destination = fsx::open_child(parent, &temp_name, O_RDWR | O_CREAT | O_EXCL, 0o600)??;
            created = true;
            let (_, copied) = fsx::copy_hashing(fd_ref(&source), fd_ref(&destination), job.size)?;
            if copied != job.size {
                return Err(fail("STALE_TARGET", "Upload changed during publication.", true));
            }
            fsx::fsync(fd_ref(&destination))?;
            let (sha, size) = fsx::hash_fd(fd_ref(&destination))?;
            if size != job.size || sha != job.sha256 {
                return Err(fail("STALE_TARGET", "Published bytes did not match verified upload.", true));
            }
            check_dest_identity(parent, job)?;
            if destination_occupied(parent, &job.name)? {
                return Err(fail("INVALID_ARGUMENT", "Destination already exists.", true));
            }
            let linked = fsx::fstat(fd_ref(&destination))?;
            if !linked.is_file() {
                return Err(fail("STALE_TARGET", "Upload changed during publication.", true));
            }
            // Bind the temp inode before link. A later same-byte name is not this
            // publication unless it is this inode.
            job.pub_dev = Some(linked.dev.to_string());
            job.pub_ino = Some(linked.ino.to_string());
            job.pub_birth_ns = Some(linked.birth_ns.to_string());
            self.save_upload_job(job)?;
            check_dest_identity(parent, job)?;
            if destination_occupied(parent, &job.name)? {
                return Err(fail("INVALID_ARGUMENT", "Destination already exists.", true));
            }
            let still = fsx::fstat(fd_ref(&destination))?;
            if Some(still.dev.to_string()) != job.pub_dev
                || Some(still.ino.to_string()) != job.pub_ino
                || Some(still.birth_ns.to_string()) != job.pub_birth_ns
            {
                return Err(fail("STALE_TARGET", "Upload changed during publication.", true));
            }
            fsx::check_name(&job.name)?;
            fsx::link_at(parent, &temp_name, &job.name)?;
            fsx::fsync(parent)?;
            std::fs::remove_file(&job.staging)?;
            Ok(())
        })();
        if created {
            let _ = fsx::unlink_at(parent, &temp_name);
        }
        outcome?;
        job.state = "published".into();
        self.save_upload_job(job)?;
        Ok(json!({ "job_id": job.job_id, "state": job.state, "relative_path": job.relative_path(), "size_bytes": job.size, "sha256": job.sha256 }))
    }

    /// storage.ts:503-520 `files_begin_download`: a single-link regular file,
    /// hashed now and pinned by its version for every chunk.
    fn begin_download_op(&self, operator_id: &str, root_id: &str, root: BorrowedFd<'_>, action: &Value) -> Result<Value> {
        let mut parts = fsx::parse_rel(Some(&str_or(action, "relative_path", "")), ".")?;
        let Some(name) = parts.pop() else { return Err(fail("INVALID_ARGUMENT", "Expected file path.", true)) };
        let joined = parts.join("/");
        let parent = self.operator_relative_fd(root, if joined.is_empty() { "." } else { &joined })?;
        let fd = fsx::open_child(fd_ref(&parent), &name, O_RDONLY | O_NONBLOCK, 0)??;
        let st = fsx::fstat(fd_ref(&fd))?;
        if st.nlink != 1 {
            return Err(fail("PERMISSION_DENIED", "Refusing a hard-linked file.", true));
        }
        // Refused by its size before reading it: hashing a file of gigabytes outlasts the request.
        let limit = self.inner.opts.max_artifact_bytes;
        if st.size > limit {
            return Err(fail("BUDGET_EXCEEDED", file_too_large(st.size, limit), true));
        }
        let (sha, size) = fsx::hash_fd(fd_ref(&fd))?;
        if fsx::fstat(fd_ref(&fd))?.nlink != 1 {
            return Err(fail("PERMISSION_DENIED", "Refusing a hard-linked file.", true));
        }
        if size > limit {
            return Err(fail("BUDGET_EXCEEDED", file_too_large(size, limit), true));
        }
        let now = self.clock();
        let mut downloads = self.inner.downloads.lock();
        downloads.retain(|_, d| now - d.last_used_ms < DOWNLOAD_IDLE_MS);
        if downloads.len() >= MAX_DOWNLOADS {
            return Err(fail("BUSY", "Too many open operator downloads; finish or abandon one first.", true));
        }
        let identity = id("operator_file");
        downloads.insert(
            identity.clone(),
            DownloadJob {
                owner: operator_id.to_string(),
                root_id: root_id.to_string(),
                relative: joined,
                name,
                size,
                sha256: sha.clone(),
                offset: 0,
                fd,
                file_version: st.version(),
                last_used_ms: now,
            },
        );
        Ok(json!({ "job_id": identity, "state": "downloading", "size_bytes": size, "sha256": sha }))
    }

    /// storage.ts:454-466 `files_download_chunk`.
    fn download_chunk_op(&self, d: &mut DownloadJob, action: &Value) -> Result<(Vec<u8>, bool)> {
        if !jsv::strict_eq_num(action.get("offset"), d.offset as f64) {
            return Err(fail("INVALID_ARGUMENT", "Invalid download chunk or offset.", true));
        }
        let st = fsx::fstat(fd_ref(&d.fd))?;
        if st.version() != d.file_version || st.nlink != 1 {
            return Err(fail("STALE_TARGET", "Download source changed.", true));
        }
        let bytes = fsx::read_fd_bytes(fd_ref(&d.fd), d.offset, self.inner.opts.chunk_bytes.min(512 * 1024) as u64)?;
        d.offset += bytes.len() as u64;
        Ok((bytes, d.offset == d.size))
    }

    /// storage.ts:525-535 `assertOperatorSpace`.
    fn assert_operator_space(&self, target: &Path, needed: u64) -> Result<()> {
        let min = self.inner.opts.min_free_bytes;
        if min == 0 {
            return Ok(());
        }
        let free = self.statfs(target).ok();
        if free.is_none_or(|free| free < needed.saturating_add(min)) {
            return Err(super::insufficient_space());
        }
        Ok(())
    }

    /// storage.ts:537-551 `openOperatorDestination`: the chosen directory, still
    /// the same inode.
    fn open_operator_destination(&self, job: &UploadJob) -> Result<OwnedFd> {
        if job.dest_dev.is_none() || job.dest_ino.is_none() {
            return Err(fail("STALE_TARGET", "Chosen destination identity was not retained.", true));
        }
        let root = self.operator_root_fd(&job.root_id)?;
        let parent =
            self.operator_relative_fd(fd_ref(&root), if job.relative.is_empty() { "." } else { &job.relative })?;
        let st = fsx::fstat(fd_ref(&parent))?;
        if !st.is_dir() || Some(st.dev.to_string()) != job.dest_dev || Some(st.ino.to_string()) != job.dest_ino {
            return Err(fail("STALE_TARGET", "Chosen destination directory changed.", true));
        }
        Ok(parent)
    }

    /// storage.ts:553-561 `assertDestinationAvailable`.
    fn assert_destination_available(&self, job: &UploadJob) -> Result<()> {
        let parent = self.open_operator_destination(job)?;
        if destination_occupied(fd_ref(&parent), &job.name)? {
            return Err(fail("INVALID_ARGUMENT", "Destination already exists.", true));
        }
        Ok(())
    }

    /// storage.ts:589-603 `verifiedStagingOffset`.
    fn verified_staging_offset(&self, job: &UploadJob, prefix_sha: &str) -> Result<()> {
        let fd = fsx::open_path(&job.staging, O_RDWR | fsx::O_NOFOLLOW, 0)?;
        verified_staging_offset_fd(fd_ref(&fd), job, prefix_sha)
    }

    /// storage.ts:605-647 `reconcileOperatorPublication`: after a lost publish
    /// response, the destination name counts as this publication only if it is
    /// the bound inode with the verified bytes. Otherwise a complete staging
    /// file returns the job to `uploading`; anything else stays `uncertain`.
    fn reconcile_operator_publication(&self, job: &mut UploadJob, prefix_sha: &str) -> Result<Value> {
        {
            let parent = self.open_operator_destination(job)?;
            if destination_occupied(fd_ref(&parent), &job.name)? {
                let changed = || fail("STALE_TARGET", "Chosen destination changed.", true);
                let fd = match fsx::open_child(fd_ref(&parent), &job.name, O_RDONLY | O_NONBLOCK, 0)? {
                    Ok(fd) => fd,
                    Err(e) if matches!(errno(&e), libc::ELOOP | libc::EPERM | libc::ENOTDIR) => return Err(changed()),
                    Err(e) => return Err(e.into()),
                };
                let st = fsx::fstat(fd_ref(&fd))?;
                let (sha, size) = fsx::hash_fd(fd_ref(&fd))?;
                let same_inode = job.pub_dev.as_deref() == Some(st.dev.to_string().as_str())
                    && job.pub_ino.as_deref() == Some(st.ino.to_string().as_str())
                    && job.pub_birth_ns.as_deref() == Some(st.birth_ns.to_string().as_str());
                if !same_inode || size != job.size || sha != job.sha256 {
                    return Err(fail("STALE_TARGET", "Chosen destination changed.", false));
                }
                job.state = "published".into();
                job.offset = job.size;
                self.save_upload_job(job)?;
                return Ok(json!({ "job_id": job.job_id, "state": "published", "offset": job.size, "size_bytes": job.size, "sha256": job.sha256, "retry_safe": false }));
            }
        }
        let staging_ready = match fsx::open_path(&job.staging, O_RDWR | fsx::O_NOFOLLOW, 0) {
            Ok(fd) => {
                verified_staging_offset_fd(fd_ref(&fd), job, prefix_sha)?;
                job.offset == job.size
            }
            Err(e) if errno(&e) == libc::ENOENT => false,
            Err(e) => return Err(e.into()),
        };
        if !staging_ready {
            return Ok(json!({ "job_id": job.job_id, "state": "uncertain", "offset": job.offset, "size_bytes": job.size, "sha256": job.sha256, "retry_safe": false }));
        }
        job.state = "uploading".into();
        job.pub_dev = None;
        job.pub_ino = None;
        job.pub_birth_ns = None;
        self.save_upload_job(job)?;
        Ok(json!({ "job_id": job.job_id, "state": "uploading", "offset": job.offset, "size_bytes": job.size, "sha256": job.sha256, "retry_safe": false }))
    }

    /// storage.ts:649-655 `operatorRootFd`: walk from `/` without following any symlink.
    fn operator_root_fd(&self, root_id: &str) -> Result<OwnedFd> {
        let root = self
            .inner
            .operator_roots
            .iter()
            .find(|(id, _)| id == root_id)
            .map(|(_, p)| p.clone())
            .ok_or_else(|| fail("PERMISSION_DENIED", "File root not approved.", true))?;
        let mut current = fsx::open_path(Path::new("/"), O_RDONLY | O_DIRECTORY, 0)?;
        for part in root.split('/').filter(|p| !p.is_empty()) {
            current = fsx::open_child(fd_ref(&current), part, O_RDONLY | O_DIRECTORY, 0)??;
        }
        Ok(current)
    }

    /// storage.ts:657-661 `operatorRelativeFd`.
    fn operator_relative_fd(&self, root: BorrowedFd<'_>, relative: &str) -> Result<OwnedFd> {
        let mut current = fsx::reopen_dir(root)?;
        for part in fsx::parse_rel(Some(relative), ".")? {
            current = fsx::open_child(fd_ref(&current), &part, O_RDONLY | O_DIRECTORY, 0)??;
        }
        Ok(current)
    }
}

/// A file size as a person reads it, in decimal units counted from the bytes
/// (1 MB is 1,000,000 bytes): "900 bytes", "1.6 MB", "250 MB", "1.3 GB". The
/// console's `bytesLabel` (StatusModel.js) words sizes the same way.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let (mut value, mut unit) = (bytes as f64 / 1000.0, 0);
    // 999.6 KB reads "1 MB", not "1000 KB".
    while value.round() >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    // One decimal below 10, else whole numbers: "1.6 MB", "10 KB", "250 MB".
    let tenths = (value * 10.0).round();
    let figure = if tenths < 100.0 { tenths / 10.0 } else { value.round() };
    format!("{figure} {}", UNITS[unit])
}

/// The refusal for a file over the size limit, naming both sizes.
pub fn file_too_large(size: u64, limit: u64) -> String {
    let (size_text, limit_text) = (size_text(size), size_text(limit));
    if size_text == limit_text {
        format!("This file is a little over {limit_text}; ibara sends files up to {limit_text}.")
    } else {
        format!("This file is {size_text}; ibara sends files up to {limit_text}.")
    }
}

/// storage.ts:563-573 `destinationOccupied`.
fn destination_occupied(parent: BorrowedFd<'_>, name: &str) -> Result<bool> {
    fsx::check_name(name)?;
    match fsx::lstat_at(parent, name) {
        Ok(_) => Ok(true),
        Err(e) if errno(&e) == libc::ENOENT => Ok(false),
        Err(e) if matches!(errno(&e), libc::ELOOP | libc::EPERM | libc::ENOTDIR) => {
            Err(fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true))
        }
        Err(e) => Err(e.into()),
    }
}

fn check_dest_identity(parent: BorrowedFd<'_>, job: &UploadJob) -> Result<()> {
    let held = fsx::fstat(parent)?;
    if Some(held.dev.to_string()) != job.dest_dev || Some(held.ino.to_string()) != job.dest_ino {
        return Err(fail("STALE_TARGET", "Chosen destination directory changed.", true));
    }
    Ok(())
}

/// storage.ts:575-580 `assertStagingIdentity`.
fn assert_staging_identity(fd: BorrowedFd<'_>, job: &UploadJob) -> Result<()> {
    let st = fsx::fstat(fd)?;
    if job.staging_dev.is_none()
        || job.staging_ino.is_none()
        || st.nlink != 1
        || Some(st.dev.to_string()) != job.staging_dev
        || Some(st.ino.to_string()) != job.staging_ino
    {
        return Err(fail("STALE_TARGET", "Upload staging changed.", true));
    }
    Ok(())
}

/// storage.ts:582-587 `rewindStaging`.
fn rewind_staging(fd: BorrowedFd<'_>, job: &UploadJob) {
    if let Ok(st) = fsx::fstat(fd)
        && Some(st.dev.to_string()) == job.staging_dev
        && Some(st.ino.to_string()) == job.staging_ino
        && st.size > job.offset
    {
        let _ = fsx::ftruncate(fd, job.offset);
    }
}

/// storage.ts:590-603: confirm the retained prefix; a longer partial chunk is
/// truncated only after that hash matches.
fn verified_staging_offset_fd(fd: BorrowedFd<'_>, job: &UploadJob, prefix_sha: &str) -> Result<()> {
    assert_staging_identity(fd, job)?;
    let size = fsx::fstat(fd)?.size;
    let stale = || fail("STALE_TARGET", "Retained upload prefix changed.", true);
    if size < job.offset {
        return Err(stale());
    }
    if fsx::hash_fd_range(fd, job.offset)?.0 != prefix_sha {
        return Err(stale());
    }
    if size != job.offset {
        fsx::ftruncate(fd, job.offset)?;
    }
    if job.offset == job.size {
        let (sha, full) = fsx::hash_fd(fd)?;
        if full != job.size || sha != job.sha256 {
            return Err(stale());
        }
    }
    Ok(())
}
