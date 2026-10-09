//! Storage: task workspaces, `computer_exec` jobs, published artifacts and their
//! delivery, the agent transfer protocol, operator file roots and procedures.
//!
//! A port of the TypeScript `src/storage.ts`, `src/procedures.ts` and
//! `src/transfer.ts` over the same `storage.sqlite` and data directory layout.
//! Public methods keep the TypeScript names in snake case and cite the lines
//! they mirror. Requests and records are JSON in the TypeScript shapes.
//!
//! Storage never opens `journal.sqlite`: the controller engine installs a
//! [`JournalView`] for the cross-store checks (task authority, collector
//! identities, and the retention rules that depend on journal receipts).
//!
//! Synchronous methods are atomic only when called from one thread at a time,
//! as the TypeScript was on Node's event loop; `ibarad` calls them from its
//! single runtime thread.

mod artifacts;
mod files;
pub mod fsx;
mod jobs;
pub mod jsv;
mod operator_files;
pub mod procedures;
mod schema;
#[cfg(test)]
mod tests;

use crate::error::{IbaraError, Result};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

pub use files::ResolvedFileReference;
pub use operator_files::{file_too_large, size_text};
pub use jobs::{
    ProcessGroupVerdict, ProcessIdentity, current_boot_id, identity_valid, inspect_process_group, owned_process_ids,
    pgid_has_live_members, read_process_stat,
};
pub use procedures::{EvidenceState, ProcedureEnvironment};

use fsx::fail;

/// storage.ts:74-77.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "TERM",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
    "HYPRLAND_INSTANCE_SIGNATURE",
];

pub type NowFn = Arc<dyn Fn() -> String + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;
/// Free bytes available to an unprivileged writer on the filesystem holding a path.
pub type StatfsFn = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// `StorageOptions` (storage.ts:36-59) with the defaults of the constructor
/// (storage.ts:685-711). `journalPath` is gone: see [`JournalView`].
#[derive(Clone)]
pub struct StorageOptions {
    /// Approved operator file roots, `root_id → absolute path`, in policy order.
    pub operator_roots: Vec<(String, String)>,
    pub root_dir: PathBuf,
    /// Default `<root_dir>/state`.
    pub state_dir: Option<PathBuf>,
    /// Default `<root_dir>/procedures/approved`.
    pub approved_procedures_dir: Option<PathBuf>,
    /// Default `<root_dir>/candidates`.
    pub candidate_procedures_dir: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub max_artifact_bytes: u64,
    pub max_output_chars: usize,
    pub max_exec_timeout_ms: u64,
    pub chunk_bytes: usize,
    pub min_free_bytes: u64,
    pub warn_free_bytes: u64,
    pub env_allowlist: Vec<String>,
    pub allowed_programs: Option<Vec<String>>,
    pub initial_wait_ms: u64,
    pub artifact_retention_ms: i64,
    pub job_log_retention_ms: i64,
    pub now: Option<NowFn>,
    pub clock: Option<ClockFn>,
    pub statfs: Option<StatfsFn>,
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// The largest file a computer sends or takes unless its policy says otherwise:
/// a round 250 MB (250,000,000 bytes), as people read it.
pub const DEFAULT_FILE_LIMIT: u64 = 250_000_000;

impl StorageOptions {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        StorageOptions {
            operator_roots: Vec::new(),
            root_dir: root_dir.into(),
            state_dir: None,
            approved_procedures_dir: None,
            candidate_procedures_dir: None,
            max_file_bytes: DEFAULT_FILE_LIMIT,
            max_artifact_bytes: DEFAULT_FILE_LIMIT,
            max_output_chars: 12_000,
            // No implicit build deadline. A caller may set one per command;
            // an explicitly configured host cap remains enforceable.
            max_exec_timeout_ms: 0,
            chunk_bytes: 1024 * 1024,
            min_free_bytes: 5 * 1024 * 1024 * 1024,
            warn_free_bytes: 15 * 1024 * 1024 * 1024,
            env_allowlist: DEFAULT_ENV.iter().map(|s| s.to_string()).collect(),
            allowed_programs: None,
            initial_wait_ms: 250,
            artifact_retention_ms: 30 * DAY_MS,
            job_log_retention_ms: 30 * DAY_MS,
            now: None,
            clock: None,
            statfs: None,
        }
    }

    /// The paths and limits `server.ts` passes (server.ts:15-19, 132-136):
    /// `IBARA_DATA_DIR`, `IBARA_STATE_DIR`, `IBARA_INSTALL_ROOT`,
    /// `IBARA_PROCEDURES_DIR`, and the policy's `operator_file_roots` and limits.
    pub fn from_env(policy: &Value) -> Result<Self> {
        let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = env("HOME").unwrap_or_else(|| PathBuf::from("/"));
        let state_dir = env("IBARA_STATE_DIR").unwrap_or_else(|| home.join(".local/state/agent-computer"));
        let data_dir = env("IBARA_DATA_DIR").unwrap_or_else(|| home.join(".local/share/agent-computer"));
        let install_root = env("IBARA_INSTALL_ROOT").unwrap_or_else(|| PathBuf::from("/opt/agent-computer"));
        let mut o = StorageOptions::new(&data_dir);
        o.state_dir = Some(state_dir);
        o.candidate_procedures_dir = Some(data_dir.join("candidates"));
        o.approved_procedures_dir =
            Some(env("IBARA_PROCEDURES_DIR").unwrap_or_else(|| install_root.join("procedures-approved")));
        if let Some(Value::Object(roots)) = policy.get("operator_file_roots") {
            o.operator_roots =
                roots.iter().filter_map(|(k, v)| v.as_str().map(|p| (k.clone(), p.to_string()))).collect();
        }
        let limit = |key: &str| -> Result<Option<u64>> {
            match policy.get(key) {
                None | Some(Value::Null) => Ok(None),
                v => match jsv::safe_integer(v) {
                    Some(n) if n >= 1 => Ok(Some(n as u64)),
                    _ => Err(fail("INVALID_ARGUMENT", format!("Invalid configured limit: {key}"), true)),
                },
            }
        };
        if let Some(n) = limit("max_process_output_chars")? {
            o.max_output_chars = n as usize;
        }
        if let Some(n) = limit("max_artifact_bytes")? {
            o.max_artifact_bytes = n;
        }
        if let Some(n) = limit("min_free_bytes")? {
            o.min_free_bytes = n;
        }
        if let Some(n) = limit("warn_free_bytes")? {
            o.warn_free_bytes = n;
        }
        if let Some(n) = limit("artifact_retention_ms")? {
            o.artifact_retention_ms = n as i64;
        }
        if let Some(n) = limit("job_log_retention_ms")? {
            o.job_log_retention_ms = n as i64;
        }
        Ok(o)
    }
}

/// The options after defaults and path resolution (`this.options`).
#[derive(Clone)]
pub struct ResolvedOptions {
    pub root_dir: PathBuf,
    pub state_dir: PathBuf,
    pub approved_procedures_dir: PathBuf,
    pub candidate_procedures_dir: PathBuf,
    pub max_file_bytes: u64,
    pub max_artifact_bytes: u64,
    pub max_output_chars: usize,
    pub max_exec_timeout_ms: u64,
    pub chunk_bytes: usize,
    pub min_free_bytes: u64,
    pub warn_free_bytes: u64,
    pub env_allowlist: Vec<String>,
    pub allowed_programs: Option<Vec<String>>,
    pub initial_wait_ms: u64,
    pub artifact_retention_ms: i64,
    pub job_log_retention_ms: i64,
}

/// A task as the journal holds it, after the engine's authority check.
#[derive(Debug, Clone, Default)]
pub struct TaskView {
    pub state: String,
    /// The task's delivery obligations (`tasks.deliveries`), unchanged JSON.
    pub deliveries: Vec<Value>,
}

/// A journal `tasks` row without authority checks.
#[derive(Debug, Clone)]
pub struct TaskState {
    pub state: String,
    /// ISO timestamp as stored.
    pub updated_at: String,
}

/// What storage needs from the journal. The controller engine implements it;
/// storage never opens `journal.sqlite` (the TypeScript did, read-only, at
/// storage.ts:1788-1803 and 1840-1863).
pub trait JournalView: Send + Sync {
    /// core.ts:954 `requireReadableTask`: the task if `principal` may read it,
    /// otherwise `PERMISSION_DENIED` or `INVALID_ARGUMENT`.
    fn read_task(&self, principal: &str, task_ref: &str) -> Result<TaskView>;
    /// journal.ts:357 `collectorIdentity`.
    fn collector(&self, principal: &str) -> Option<String>;
    /// The journal's task state and `updated_at`, without authority. `None` if absent.
    fn task_state(&self, task_ref: &str) -> Result<Option<TaskState>>;
    /// storage.ts:1794-1797: any operation of the task whose receipt has
    /// `execution ∈ {unknown, running}`, `verification ∈ {unknown, pending}` or
    /// `effect == "unknown"`. An error means "keep the bytes".
    fn has_unresolved(&self, task_ref: &str) -> Result<bool>;
    /// storage.ts:1847-1850: any operation of the task whose receipt has
    /// `execution ∈ {unknown, pending}` or `error.code == "CONTROL_UNSETTLED"`.
    /// An error means "keep the logs".
    fn has_unsettled(&self, task_ref: &str) -> Result<bool>;
}

/// Set when the lease or request that started a job is aborted (`context.signal`).
pub type AbortSignal = tokio::sync::watch::Receiver<bool>;
pub type AuthorityFn = Arc<dyn Fn() -> Result<()> + Send + Sync>;
pub type EvidenceResolver = Arc<dyn Fn(&[String]) -> BTreeMap<String, EvidenceState> + Send + Sync>;

/// shared.ts:10-20 `Context`.
#[derive(Clone)]
pub struct Context {
    pub task_ref: String,
    pub principal: String,
    pub epoch: String,
    pub request_id: Option<String>,
    /// The operation this call runs as, recorded as the writer of files it writes.
    pub operation_ref: Option<String>,
    /// `budgets.max_download_bytes`.
    pub max_download_bytes: Option<u64>,
    pub signal: Option<AbortSignal>,
    /// `assertAuthority()`; `None` always passes.
    pub authority: Option<AuthorityFn>,
    pub procedure_environment: Option<ProcedureEnvironment>,
    pub procedure_evidence_resolver: Option<EvidenceResolver>,
    /// Where this call's file paths and exec cwd start instead of the task
    /// workspace: the home folder, for a path the controller resolved there.
    pub base_dir: Option<PathBuf>,
}

impl Context {
    pub fn new(task_ref: impl Into<String>, principal: impl Into<String>, epoch: impl Into<String>) -> Self {
        Context {
            task_ref: task_ref.into(),
            principal: principal.into(),
            epoch: epoch.into(),
            request_id: None,
            operation_ref: None,
            max_download_bytes: None,
            signal: None,
            authority: None,
            procedure_environment: None,
            procedure_evidence_resolver: None,
            base_dir: None,
        }
    }

    pub fn assert_authority(&self) -> Result<()> {
        self.authority.as_ref().map_or(Ok(()), |f| f())
    }
}

/// shared.ts:4 `ToolResult` (storage never returns images).
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub records: Vec<Value>,
}

impl ToolResult {
    pub fn one(record: Value) -> Self {
        ToolResult { records: vec![record] }
    }
}

/// storage.ts:330 `StorageService`. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct StorageService {
    inner: Arc<Inner>,
}

struct Inner {
    opts: ResolvedOptions,
    operator_roots: Vec<(String, String)>,
    now: Option<NowFn>,
    clock: Option<ClockFn>,
    statfs: Option<StatfsFn>,
    db: Mutex<Option<Connection>>,
    /// Live jobs only: running, or unknown without confirmed termination.
    jobs: Mutex<HashMap<String, Arc<jobs::JobHandle>>>,
    /// Operator downloads hold an open descriptor and live only in memory.
    downloads: Mutex<HashMap<String, operator_files::DownloadJob>>,
    journal: RwLock<Option<Arc<dyn JournalView>>>,
    closed: AtomicBool,
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

impl StorageService {
    /// storage.ts:685-823 `constructor`: create directories, open and migrate
    /// `storage.sqlite`, recover jobs and reindex approved procedures.
    /// Job-log pruning needs the journal and runs from [`Self::set_authority_policy`].
    pub fn open(options: StorageOptions) -> Result<StorageService> {
        if options.root_dir.as_os_str().is_empty() {
            return Err(fail("INVALID_ARGUMENT", "StorageService requires rootDir or dataDir.", true));
        }
        let root_dir = absolute(&options.root_dir);
        let state_dir = absolute(&options.state_dir.clone().unwrap_or_else(|| root_dir.join("state")));
        let opts = ResolvedOptions {
            approved_procedures_dir: absolute(
                &options.approved_procedures_dir.clone().unwrap_or_else(|| root_dir.join("procedures").join("approved")),
            ),
            candidate_procedures_dir: absolute(
                &options.candidate_procedures_dir.clone().unwrap_or_else(|| root_dir.join("candidates")),
            ),
            root_dir,
            state_dir,
            max_file_bytes: options.max_file_bytes,
            max_artifact_bytes: options.max_artifact_bytes,
            max_output_chars: options.max_output_chars,
            max_exec_timeout_ms: options.max_exec_timeout_ms,
            chunk_bytes: options.chunk_bytes,
            min_free_bytes: options.min_free_bytes,
            warn_free_bytes: options.warn_free_bytes,
            env_allowlist: options.env_allowlist.clone(),
            allowed_programs: options.allowed_programs.clone(),
            initial_wait_ms: options.initial_wait_ms,
            artifact_retention_ms: options.artifact_retention_ms,
            job_log_retention_ms: options.job_log_retention_ms,
        };
        // storage.ts:689: ids /^[a-z][a-z0-9_-]{0,63}$/, absolute normalized roots.
        let operator_roots = options
            .operator_roots
            .iter()
            .filter(|(id, root)| jsv::principal_re(id) && root.starts_with('/') && jsv::posix_normalize(root) == *root)
            .cloned()
            .collect();
        for dir in [
            opts.root_dir.clone(),
            opts.state_dir.clone(),
            opts.root_dir.join("workspaces"),
            opts.root_dir.join("artifacts"),
            opts.root_dir.join("staging"),
            opts.candidate_procedures_dir.clone(),
            opts.state_dir.join("pending-promotions"),
            opts.state_dir.join("quarantine"),
        ] {
            fsx::mkdir_p(&dir)?;
        }
        let service = StorageService {
            inner: Arc::new(Inner {
                opts,
                operator_roots,
                now: options.now.clone(),
                clock: options.clock.clone(),
                statfs: options.statfs.clone(),
                db: Mutex::new(None),
                jobs: Mutex::new(HashMap::new()),
                downloads: Mutex::new(HashMap::new()),
                journal: RwLock::new(None),
                closed: AtomicBool::new(false),
            }),
        };
        let conn = Connection::open(service.inner.opts.state_dir.join("storage.sqlite"))?;
        schema::migrate(&conn, &service.now_iso())?;
        *service.inner.db.lock() = Some(conn);
        service.recover_jobs()?;
        service.reindex_approved()?;
        Ok(service)
    }

    /// `this.options`.
    pub fn options(&self) -> &ResolvedOptions {
        &self.inner.opts
    }

    /// storage.ts:664-667 `setAuthorityPolicy`. Installing the journal view also
    /// runs the retention passes that need it.
    pub fn set_authority_policy(&self, view: Arc<dyn JournalView>) {
        *self.inner.journal.write() = Some(view);
        let _ = self.expire_collected_artifacts();
        let _ = self.prune_resolved_job_logs();
    }

    fn journal(&self) -> Option<Arc<dyn JournalView>> {
        self.inner.journal.read().clone()
    }

    /// Run `f` with the database; `SESSION_UNAVAILABLE` after [`Self::close`].
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.inner.db.lock();
        match guard.as_ref() {
            Some(conn) => f(conn),
            None => Err(closed_error()),
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) { Err(closed_error()) } else { Ok(()) }
    }

    /// `this.nowIso()` (injectable).
    fn now_iso(&self) -> String {
        self.inner.now.as_ref().map_or_else(crate::ids::now_iso, |f| f())
    }

    /// `this.clock()` (injectable, milliseconds).
    fn clock(&self) -> i64 {
        self.inner.clock.as_ref().map_or_else(crate::ids::now_millis, |f| f())
    }

    fn statfs(&self, target: &Path) -> io::Result<u64> {
        match &self.inner.statfs {
            Some(f) => f(target),
            None => fsx::statfs_free(target),
        }
    }

    fn workspace_root(&self) -> PathBuf {
        self.inner.opts.root_dir.join("workspaces")
    }
    fn artifact_root(&self) -> PathBuf {
        self.inner.opts.root_dir.join("artifacts")
    }
    fn staging_root(&self) -> PathBuf {
        self.inner.opts.root_dir.join("staging")
    }

    /// storage.ts:825-832 `workspace`.
    pub fn workspace(&self, task_ref: &str, prepare: bool) -> Result<PathBuf> {
        fsx::assert_id(task_ref, "task_ref")?;
        let directory = self.workspace_root().join(task_ref);
        if prepare {
            self.ensure_open()?;
            fsx::mkdir_p(&directory)?;
        }
        Ok(directory)
    }

    /// storage.ts:1475-1478 `freeBytes`.
    fn free_bytes(&self) -> Result<u64> {
        Ok(self.statfs(&self.inner.opts.root_dir)?)
    }

    /// storage.ts:1480-1489 `assertFreeSpace`.
    fn assert_free_space(&self, needed: u64) -> Result<()> {
        let min = self.inner.opts.min_free_bytes;
        if min == 0 {
            return Ok(());
        }
        if self.free_bytes()? < needed.saturating_add(min) {
            return Err(insufficient_space());
        }
        Ok(())
    }

    /// storage.ts:834-858 `capabilities`.
    pub fn capabilities(&self) -> Result<Vec<Value>> {
        self.ensure_open()?;
        self.expire_collected_artifacts()?;
        self.prune_resolved_job_logs()?;
        let tested = self.now_iso();
        let free = self.free_bytes()?;
        let (min, warn) = (self.inner.opts.min_free_bytes, self.inner.opts.warn_free_bytes);
        let (status, reason) = if min > 0 && free < min {
            ("unavailable", Some(format!("Free disk is {free} bytes, below the {min}-byte reject threshold.")))
        } else if warn > 0 && free < warn {
            ("degraded", Some(format!("Free disk is {free} bytes, below the {warn}-byte warning threshold.")))
        } else {
            ("available", None)
        };
        let mut cap = json!({ "name": "storage.disk", "status": status, "backend": "statfs", "last_tested_at": tested });
        if let Some(reason) = reason {
            cap["reason"] = json!(jsv::clip(&reason, 1000));
        }
        Ok(vec![cap])
    }

    /// storage.ts:1448-1461 `close`: drop operator downloads, SIGKILL the owned
    /// processes of running jobs, and close the database.
    pub fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.downloads.lock().clear();
        self.kill_running_jobs_now();
        self.inner.db.lock().take();
    }
}

fn closed_error() -> IbaraError {
    fail("SESSION_UNAVAILABLE", "Storage service is closed.", true)
}

fn insufficient_space() -> IbaraError {
    fsx::fail_not_started("BUDGET_EXCEEDED", "Insufficient free space for a new file or artifact.")
        .with("requires_reconciliation", false)
}

/// storage.ts:305-307.
fn is_journal_terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "cancelled" | "partial")
}
