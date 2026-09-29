//! `computer_exec` jobs: spawning in an own process group, bounded output,
//! cancellation that only signals a group verified as ours, and recovery of
//! jobs across a restart by `(boot_id, pid, pgid, starttime)`
//! (storage.ts:201-302, 884-1096, 2141-2286, 1819-1863).

use super::fsx::{self, fail};
use super::jsv::{self, num_or, str_or, to_js_string};
use super::{Context, StorageService, ToolResult, is_journal_terminal_state};
use crate::error::Result;
use crate::ids::{id, iso_from_millis};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::process::Stdio;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// storage.ts:201.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub boot_id: String,
    pub pid: i64,
    pub pgid: i64,
    /// Field 22 of `/proc/<pid>/stat` (clock ticks since boot).
    pub starttime: i64,
}

/// storage.ts:202.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessGroupVerdict {
    Dead,
    Ours,
    Foreign,
    Uncertain,
}

/// storage.ts:204.
#[derive(Debug, Clone)]
pub struct ProcStat {
    pub pid: i64,
    pub state: String,
    pub pgrp: i64,
    pub starttime: i64,
}

/// storage.ts:206-209 `currentBootId` (empty when unreadable).
pub fn current_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// storage.ts:211-225 `readProcessStat`.
pub fn read_process_stat(pid: i64) -> Option<ProcStat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest: Vec<&str> = stat.get(close + 2..)?.split(' ').collect();
    let state = rest.first().filter(|s| !s.is_empty())?.to_string();
    let pgrp = rest.get(2)?.parse().ok()?;
    let starttime = rest.get(19)?.trim().parse().ok()?;
    Some(ProcStat { pid, state, pgrp, starttime })
}

fn list_proc_pids() -> Option<Vec<i64>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    Some(entries.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i64>().ok()).collect())
}

fn live(stat: &ProcStat) -> bool {
    stat.state != "Z" && stat.state != "X"
}

/// storage.ts:257-265 `identityValid`.
pub fn identity_valid(identity: Option<&ProcessIdentity>) -> bool {
    identity.is_some_and(|i| !i.boot_id.is_empty() && i.pid > 0 && i.pgid > 0 && i.starttime > 0)
}

/// storage.ts:235-255 `inspectProcessGroup`: `dead` when no live member of the
/// group exists (or the boot changed), `ours` when the original leader or only
/// later-started members remain, `foreign` when the group id was reused.
pub fn inspect_process_group(identity: &ProcessIdentity) -> ProcessGroupVerdict {
    use ProcessGroupVerdict::*;
    if !identity_valid(Some(identity)) {
        return Uncertain;
    }
    let boot = current_boot_id();
    if boot.is_empty() {
        return Uncertain;
    }
    if boot != identity.boot_id {
        return Dead;
    }
    let Some(pids) = list_proc_pids() else { return Uncertain };
    let members: Vec<ProcStat> =
        pids.into_iter().filter_map(read_process_stat).filter(|s| live(s) && s.pgrp == identity.pgid).collect();
    if members.is_empty() {
        return Dead;
    }
    if members.iter().any(|m| m.pid == identity.pid && m.starttime == identity.starttime) {
        return Ours;
    }
    if members.iter().any(|m| m.pid == identity.pgid && m.starttime != identity.starttime) {
        return Foreign;
    }
    if members.iter().all(|m| m.starttime >= identity.starttime) { Ours } else { Foreign }
}

/// storage.ts:268-277 `pgidHasLiveMembers` (`None` if `/proc` cannot be scanned).
pub fn pgid_has_live_members(pgid: i64) -> Option<bool> {
    if pgid <= 0 {
        return Some(false);
    }
    let pids = list_proc_pids()?;
    Some(pids.into_iter().filter_map(read_process_stat).any(|s| s.pgrp == pgid && live(&s)))
}

/// storage.ts:279-292 `ownedProcessIds`.
pub fn owned_process_ids(identity: Option<&ProcessIdentity>) -> Vec<i64> {
    let Some(identity) = identity else { return Vec::new() };
    if inspect_process_group(identity) != ProcessGroupVerdict::Ours {
        return Vec::new();
    }
    list_proc_pids()
        .unwrap_or_default()
        .into_iter()
        .filter_map(read_process_stat)
        .filter(|s| {
            live(s)
                && s.pgrp == identity.pgid
                && !(s.pid == identity.pid && s.starttime != identity.starttime)
                && s.starttime >= identity.starttime
        })
        .map(|s| s.pid)
        .collect()
}

/// storage.ts:124-127 `sanitizeLog`: strip control characters other than tab,
/// newline and carriage return, then clip to `max` UTF-16 units.
fn sanitize_log(bytes: &[u8], max: usize) -> (String, bool) {
    let cleaned: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f))
        .collect();
    let truncated = jsv::utf16_len(&cleaned) > max;
    (jsv::clip(&cleaned, max).to_string(), truncated)
}

/// A job tracked in memory (storage.ts:103-118 `TrackedJob`).
pub(super) struct JobHandle {
    pub job_ref: String,
    pub task_ref: String,
    pub request_id: String,
    pub principal: Option<String>,
    max_chars: usize,
    st: Mutex<JobState>,
    settled: watch::Sender<bool>,
}

struct JobState {
    state: String,
    /// `job.record`; `None` is the TypeScript's empty `{}`.
    record: Option<Map<String, Value>>,
    pgid: Option<i64>,
    /// The pid written with each persist (`child.pid`, or the recovered pid).
    pid: Option<i64>,
    identity: Option<ProcessIdentity>,
    has_child: bool,
    /// The child has been reaped (`child.exitCode != null`).
    exited: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stored: usize,
    truncated: bool,
    settled: bool,
}

impl JobState {
    fn active(&self) -> bool {
        self.state == "running"
            || (self.state == "unknown"
                && !self.record.as_ref().is_some_and(|r| jsv::truthy(r.get("termination_confirmed"))))
    }
}

impl JobHandle {
    fn new(job_ref: String, task_ref: String, request_id: String, principal: Option<String>, max_chars: usize, st: JobState) -> Self {
        JobHandle { job_ref, task_ref, request_id, principal, max_chars, st: Mutex::new(st), settled: watch::channel(false).0 }
    }

    fn base_record(&self, state: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".into(), json!("job"));
        m.insert("job_ref".into(), json!(self.job_ref));
        m.insert("task_ref".into(), json!(self.task_ref));
        m.insert("request_id".into(), json!(self.request_id));
        m.insert("state".into(), json!(state));
        m
    }

    /// `settle(record)`: the first settlement wins and wakes `exec`.
    fn mark_settled(&self, st: &mut JobState) {
        st.settled = true;
        self.settled.send_replace(true);
    }
}

fn filtered_env(allow: &[String]) -> Vec<(String, std::ffi::OsString)> {
    let mut env: Vec<(String, std::ffi::OsString)> =
        allow.iter().filter_map(|k| std::env::var_os(k).map(|v| (k.clone(), v))).collect();
    match env.iter_mut().find(|(k, _)| k == "LC_ALL") {
        Some((_, v)) if !v.is_empty() => {}
        Some((_, v)) => *v = "C".into(),
        None => env.push(("LC_ALL".into(), "C".into())),
    }
    env
}

async fn pump(stream: Option<impl AsyncRead + Unpin>, h: &JobHandle, is_err: bool) {
    let Some(mut s) = stream else { return };
    let mut buf = vec![0u8; 8192];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut st = h.st.lock();
                let room = (h.max_chars * 4).saturating_sub(st.stored);
                let keep = room.min(n);
                if keep > 0 {
                    let target = if is_err { &mut st.stderr } else { &mut st.stdout };
                    target.extend_from_slice(&buf[..keep]);
                    st.stored += keep;
                }
                if keep < n {
                    st.truncated = true;
                }
            }
        }
    }
}

async fn abort_fired(signal: &mut Option<watch::Receiver<bool>>) {
    match signal {
        Some(rx) => loop {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
            if *rx.borrow() {
                return;
            }
        },
        None => std::future::pending().await,
    }
}

impl StorageService {
    /// storage.ts:2155-2170 `persistJob`.
    fn persist_job(&self, h: &JobHandle, st: &JobState) -> Result<()> {
        let record = st.record.clone().unwrap_or_else(|| {
            let mut r = h.base_record(&st.state);
            r.insert("stdout".into(), json!(""));
            r.insert("stderr".into(), json!(""));
            r.insert("output_truncated".into(), json!(false));
            r
        });
        let text = |k: &str| record.get(k).filter(|v| !v.is_null()).map_or_else(String::new, to_js_string);
        let now = self.now_iso();
        self.with_db(|db| {
            db.execute(
                "INSERT INTO jobs(job_ref, task_ref, request_id, state, pid, pgid, exit_code, stdout, stderr, output_truncated, termination_confirmed, record, updated_at, principal, boot_id, starttime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(job_ref) DO UPDATE SET state=excluded.state, pid=excluded.pid, pgid=excluded.pgid, exit_code=excluded.exit_code, stdout=excluded.stdout, stderr=excluded.stderr, output_truncated=excluded.output_truncated, termination_confirmed=excluded.termination_confirmed, record=excluded.record, updated_at=excluded.updated_at, principal=COALESCE(excluded.principal, jobs.principal), boot_id=COALESCE(excluded.boot_id, jobs.boot_id), starttime=CASE WHEN excluded.starttime IS NOT NULL AND excluded.starttime != 0 THEN excluded.starttime ELSE jobs.starttime END",
                params![
                    h.job_ref,
                    h.task_ref,
                    h.request_id,
                    st.state,
                    st.pid,
                    st.pgid,
                    fsx::bind_json(record.get("exit_code")),
                    text("stdout"),
                    text("stderr"),
                    i64::from(jsv::truthy(record.get("output_truncated"))),
                    i64::from(jsv::truthy(record.get("termination_confirmed"))),
                    serde_json::to_string(&record).unwrap_or_default(),
                    now,
                    h.principal,
                    st.identity.as_ref().map(|i| i.boot_id.clone()),
                    st.identity.as_ref().map(|i| i.starttime),
                ],
            )?;
            Ok(())
        })
    }

    fn retire_if_inactive(&self, h: &JobHandle) {
        let active = h.st.lock().active();
        if !active {
            self.inner.jobs.lock().remove(&h.job_ref);
        }
    }

    /// storage.ts:884-1037 `exec`: spawn `program args…` in the task workspace
    /// (own process group, allow-listed environment plus `LC_ALL=C`), wait up to
    /// `initial_wait_ms` (none with `background: true`), then return the
    /// finished job or a `running` one.
    pub async fn exec(&self, input: &Value, ctx: &Context) -> Result<ToolResult> {
        self.ensure_open()?;
        ctx.assert_authority()?;
        let opts = &self.inner.opts;
        let mut program = str_or(input, "program", "");
        let mut args: Vec<String> = match input.get("args") {
            Some(Value::Array(items)) => items.iter().map(to_js_string).collect(),
            _ => Vec::new(),
        };
        if program.is_empty()
            && let Some(Value::Array(command)) = input.get("command")
            && let Some((first, rest)) = command.split_first()
        {
            program = to_js_string(first);
            args = rest.iter().map(to_js_string).collect();
        }
        if program.is_empty() || jsv::utf16_len(&program) > 512 {
            return Err(fail("INVALID_ARGUMENT", "exec requires a program name.", true));
        }
        if args.len() > 100 || args.iter().any(|a| jsv::utf16_len(a) > 4096) {
            return Err(fail("INVALID_ARGUMENT", "exec arguments exceed limits.", true));
        }
        if let Some(allowed) = &opts.allowed_programs {
            let base = jsv::basename(&program);
            if !allowed.iter().any(|p| *p == program || p == base) {
                return Err(fail("PERMISSION_DENIED", "Program is not in the task allowlist.", true));
            }
        }
        let max_timeout = opts.max_exec_timeout_ms as f64;
        let timeout = num_or(input, "timeout_ms", max_timeout);
        let timeout = if timeout.is_nan() { timeout } else { timeout.min(max_timeout) };
        if !timeout.is_finite() || timeout < 1.0 {
            return Err(fail("INVALID_ARGUMENT", "timeout_ms is required.", true));
        }
        let max_out = opts.max_output_chars as f64;
        let max_chars = num_or(input, "max_output_chars", max_out).min(max_out);
        let max_chars = if max_chars.is_nan() { opts.max_output_chars } else { max_chars.max(0.0) as usize };
        let cwd_parts = fsx::parse_rel(Some(&str_or(input, "cwd", ".")), ".")?;
        let cwd = self.with_task_dir(ctx, &cwd_parts, true, false, |fd| {
            if !fsx::fstat(fd)?.is_dir() {
                return Err(fail("INVALID_ARGUMENT", "exec cwd must be a directory.", true));
            }
            fsx::fd_path(fd)
        })?;
        let job_ref = id("job");
        let request_id = match input.get("request_id") {
            v @ Some(r) if jsv::truthy(v) => to_js_string(r),
            _ => id("request"),
        };
        let stdin = match input.get("stdin") {
            None | Some(Value::Null) => None,
            Some(v) => Some(jsv::clip(&to_js_string(v), 16000).as_bytes().to_vec()),
        };
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .env_clear()
            .envs(filtered_env(&opts.env_allowlist))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(false);
        let mk = |st: JobState| {
            Arc::new(JobHandle::new(job_ref.clone(), ctx.task_ref.clone(), request_id.clone(), Some(ctx.principal.clone()), max_chars, st))
        };
        let blank = JobState {
            state: "running".into(),
            record: None,
            pgid: None,
            pid: None,
            identity: None,
            has_child: false,
            exited: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stored: 0,
            truncated: false,
            settled: false,
        };
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // Nothing ran. ENOENT is 127 as in the TypeScript; any other
                // spawn failure (EACCES, ENOEXEC…) is 126, like a shell.
                let code = if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
                let h = mk(blank);
                let mut st = h.st.lock();
                let mut r = h.base_record("failed");
                r.insert("output_truncated".into(), json!(false));
                r.insert("stdout".into(), json!(""));
                r.insert("stderr".into(), json!(""));
                r.insert("exit_code".into(), json!(code));
                r.insert("termination_confirmed".into(), json!(true));
                st.state = "failed".into();
                st.record = Some(r.clone());
                self.persist_job(&h, &st)?;
                return Ok(ToolResult::one(Value::Object(r)));
            }
        };
        let pid = child.id().map(i64::from);
        let identity = pid.map(|pid| ProcessIdentity {
            boot_id: current_boot_id(),
            pid,
            pgid: pid,
            starttime: read_process_stat(pid).map_or(0, |s| s.starttime),
        });
        let h = mk(JobState { pgid: pid, pid, identity, has_child: true, ..blank });
        self.inner.jobs.lock().insert(job_ref.clone(), h.clone());
        {
            let st = h.st.lock();
            self.persist_job(&h, &st)?;
        }
        let mut settled_rx = h.settled.subscribe();
        tokio::spawn(self.clone().run_job(
            h.clone(),
            child,
            stdin,
            Duration::from_secs_f64(timeout / 1000.0),
            ctx.signal.clone(),
        ));
        let background = input.get("background") == Some(&Value::Bool(true));
        let wait_ms = if background { 0 } else { opts.initial_wait_ms.min(timeout as u64) };
        let _ = tokio::time::timeout(Duration::from_millis(wait_ms), settled_rx.wait_for(|v| *v)).await;
        let mut st = h.st.lock();
        if st.settled {
            return Ok(ToolResult::one(Value::Object(st.record.clone().unwrap_or_default())));
        }
        let mut running = h.base_record("running");
        running.insert("stdout".into(), json!(""));
        running.insert("stderr".into(), json!(""));
        running.insert("output_truncated".into(), json!(false));
        st.record = Some(running.clone());
        self.persist_job(&h, &st)?;
        Ok(ToolResult::one(Value::Object(running)))
    }

    /// The child's lifetime: feed stdin, collect bounded output, reap, and
    /// settle on close (storage.ts:951-1015). A timeout or abort starts
    /// `kill_tracked` alongside, as the TypeScript timers did.
    async fn run_job(
        self,
        h: Arc<JobHandle>,
        mut child: Child,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        mut signal: Option<watch::Receiver<bool>>,
    ) {
        let stdin_pipe = child.stdin.take();
        let out = child.stdout.take();
        let err = child.stderr.take();
        let hh = h.clone();
        let io = async move {
            let feed = async move {
                if let Some(mut pipe) = stdin_pipe {
                    if let Some(data) = stdin {
                        let _ = pipe.write_all(&data).await;
                    }
                    let _ = pipe.shutdown().await;
                }
            };
            let reap = async {
                let status = child.wait().await;
                hh.st.lock().exited = true;
                status
            };
            let (_, _, _, status) = tokio::join!(feed, pump(out, &hh, false), pump(err, &hh, true), reap);
            status
        };
        tokio::pin!(io);
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        let mut timer_fired = false;
        let status = loop {
            tokio::select! {
                status = &mut io => break status,
                _ = &mut sleep, if !timer_fired => {
                    timer_fired = true;
                    tokio::spawn(self.clone().kill_tracked_owned(h.clone(), true));
                }
                _ = abort_fired(&mut signal) => {
                    signal = None;
                    tokio::spawn(self.clone().kill_tracked_owned(h.clone(), false));
                }
            }
        };
        self.settle_close(&h, status);
    }

    /// storage.ts:998-1005: the `close` event.
    fn settle_close(&self, h: &JobHandle, status: std::io::Result<std::process::ExitStatus>) {
        use std::os::unix::process::ExitStatusExt;
        {
            let mut st = h.st.lock();
            if !st.settled {
                let still_ours =
                    st.identity.as_ref().is_some_and(|i| inspect_process_group(i) == ProcessGroupVerdict::Ours);
                let (state, code, confirmed) = match &status {
                    Err(_) => ("unknown", None, false),
                    Ok(s) if still_ours => ("unknown", s.code(), false),
                    Ok(s) => match s.signal() {
                        Some(sig) if sig == libc::SIGTERM || sig == libc::SIGKILL => ("cancelled", s.code(), true),
                        Some(_) => ("failed", s.code(), true),
                        None => (if s.code() == Some(0) { "completed" } else { "failed" }, s.code(), true),
                    },
                };
                let (stdout, t1) = sanitize_log(&st.stdout, h.max_chars);
                let (stderr, t2) = sanitize_log(&st.stderr, h.max_chars);
                let mut r = h.base_record(state);
                r.insert("output_truncated".into(), json!(t1 || t2 || st.truncated));
                r.insert("stdout".into(), json!(stdout));
                r.insert("stderr".into(), json!(stderr));
                if let Some(code) = code {
                    r.insert("exit_code".into(), json!(code.clamp(-255, 255)));
                }
                r.insert("termination_confirmed".into(), json!(confirmed));
                st.state = state.into();
                st.record = Some(r);
                let _ = self.persist_job(h, &st);
                h.mark_settled(&mut st);
            }
        }
        self.retire_if_inactive(h);
    }

    async fn kill_tracked_owned(self, h: Arc<JobHandle>, timeout: bool) -> bool {
        self.kill_tracked(&h, timeout).await
    }

    /// storage.ts:2172-2177 `groupVerdict`.
    fn group_verdict(h: &JobHandle) -> ProcessGroupVerdict {
        let st = h.st.lock();
        match &st.identity {
            Some(identity) => inspect_process_group(identity),
            None if st.has_child && !st.exited => ProcessGroupVerdict::Ours,
            None if st.pgid.is_some() && !st.has_child => ProcessGroupVerdict::Uncertain,
            None => ProcessGroupVerdict::Dead,
        }
    }

    /// storage.ts:2198-2218 `signalOwned`: signal only a group verified as ours.
    fn signal_owned(h: &JobHandle, signal: i32) -> bool {
        use ProcessGroupVerdict::*;
        match Self::group_verdict(h) {
            Foreign | Uncertain => return false,
            Dead => return true,
            Ours => {}
        }
        let (identity, child_pid) = {
            let st = h.st.lock();
            (st.identity.clone(), if st.has_child { st.pid } else { None })
        };
        let send = |target: i64| -> bool {
            // SAFETY: plain kill(2).
            let rc = unsafe { libc::kill(target as libc::pid_t, signal) };
            rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        };
        let leader_alive = identity
            .as_ref()
            .and_then(|i| read_process_stat(i.pid).map(|s| s.starttime == i.starttime && s.pgrp == i.pgid))
            .unwrap_or(false);
        match (&identity, leader_alive) {
            (Some(i), true) => send(-i.pgid),
            (Some(i), false) => owned_process_ids(Some(i)).into_iter().all(send),
            (None, _) => child_pid.into_iter().all(send),
        }
    }

    /// storage.ts:2187-2197 `markUnknown`.
    fn mark_unknown(&self, h: &JobHandle, confirmed: bool) {
        let mut st = h.st.lock();
        let prev = st.record.clone().unwrap_or_default();
        let mut r = h.base_record("unknown");
        r.insert("stdout".into(), json!(prev.get("stdout").filter(|v| jsv::truthy(Some(v))).cloned().unwrap_or(json!(""))));
        r.insert("stderr".into(), json!(prev.get("stderr").filter(|v| jsv::truthy(Some(v))).cloned().unwrap_or(json!(""))));
        r.insert("output_truncated".into(), json!(jsv::truthy(prev.get("output_truncated"))));
        r.insert("termination_confirmed".into(), json!(confirmed));
        st.state = "unknown".into();
        st.record = Some(r);
        let _ = self.persist_job(h, &st);
        h.mark_settled(&mut st);
    }

    /// storage.ts:2186-2255 `killTracked`: SIGTERM the verified group, wait up
    /// to 2 s, SIGKILL, wait 100 ms. Anything not provably dead afterwards
    /// becomes `unknown` with `termination_confirmed: false`.
    async fn kill_tracked(&self, h: &Arc<JobHandle>, timeout: bool) -> bool {
        use ProcessGroupVerdict::*;
        if !Self::signal_owned(h, libc::SIGTERM) {
            self.mark_unknown(h, false);
            return false;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
        while tokio::time::Instant::now() < deadline {
            let verdict = Self::group_verdict(h);
            let (has_child, exited) = {
                let st = h.st.lock();
                (st.has_child, st.exited)
            };
            if verdict == Dead && (exited || !has_child) {
                break;
            }
            if verdict == Foreign || verdict == Uncertain {
                self.mark_unknown(h, false);
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if Self::group_verdict(h) == Ours {
            if !Self::signal_owned(h, libc::SIGKILL) {
                self.mark_unknown(h, false);
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if Self::group_verdict(h) != Dead {
            self.mark_unknown(h, false);
            return false;
        }
        {
            let mut st = h.st.lock();
            if st.state == "running" {
                let state = if timeout { "failed" } else { "cancelled" };
                let max = self.inner.opts.max_output_chars;
                let mut r = h.base_record(state);
                r.insert("stdout".into(), json!(sanitize_log(&st.stdout, max).0));
                r.insert("stderr".into(), json!(sanitize_log(&st.stderr, max).0));
                r.insert("output_truncated".into(), json!(st.truncated));
                r.insert("termination_confirmed".into(), json!(true));
                st.state = state.into();
                st.record = Some(r);
                let _ = self.persist_job(h, &st);
            }
            if st.state == "unknown" {
                // Quiescence is separate from whether its past external effect is known.
                let mut r = st.record.clone().unwrap_or_default();
                r.insert("termination_confirmed".into(), json!(true));
                st.record = Some(r);
                let _ = self.persist_job(h, &st);
            }
            if !st.settled {
                h.mark_settled(&mut st);
            }
        }
        self.retire_if_inactive(h);
        true
    }

    fn active_jobs(&self, task_ref: Option<&str>) -> Vec<Arc<JobHandle>> {
        self.inner.jobs.lock()
            .values()
            .filter(|h| task_ref.is_none_or(|t| h.task_ref == t) && h.st.lock().active())
            .cloned()
            .collect()
    }

    /// storage.ts:1062-1071 `cancelJobs`: true when every active job's
    /// termination is confirmed.
    pub async fn cancel_jobs(&self, task_ref: Option<&str>) -> Result<bool> {
        self.ensure_open()?;
        let mut confirmed = true;
        for h in self.active_jobs(task_ref) {
            if !self.kill_tracked(&h, false).await {
                confirmed = false;
            }
        }
        Ok(confirmed)
    }

    /// storage.ts:1073-1075 `hasActiveJobs`.
    pub fn has_active_jobs(&self, task_ref: Option<&str>) -> bool {
        !self.active_jobs(task_ref).is_empty()
    }

    /// storage.ts:1453-1459: SIGKILL the owned processes of running jobs.
    pub(super) fn kill_running_jobs_now(&self) {
        for h in self.inner.jobs.lock().values() {
            let st = h.st.lock();
            if st.has_child && st.state == "running" {
                for pid in owned_process_ids(st.identity.as_ref()) {
                    // SAFETY: plain kill(2); the pid was verified as ours just now.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
            }
        }
    }

    /// storage.ts:2179-2184 `jobRecord`.
    fn job_record(&self, job_ref: &str) -> Result<Option<Value>> {
        if let Some(h) = self.inner.jobs.lock().get(job_ref).cloned()
            && let Some(r) = h.st.lock().record.clone()
        {
            return Ok(Some(Value::Object(r)));
        }
        self.with_db(|db| {
            let text: Option<String> =
                db.query_row("SELECT record FROM jobs WHERE job_ref = ?1", [job_ref], |r| r.get(0)).optional()?;
            Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
        })
    }

    /// storage.ts:1077-1088 `getJob`.
    pub fn get_job(&self, job_ref: &str, task_ref: Option<&str>, principal: Option<&str>) -> Result<Option<Value>> {
        self.ensure_open()?;
        let Some(record) = self.job_record(job_ref)? else { return Ok(None) };
        if let Some(t) = task_ref
            && record.get("task_ref") != Some(&json!(t))
        {
            return Ok(None);
        }
        if let Some(p) = principal {
            let live = self.inner.jobs.lock().get(job_ref).and_then(|h| h.principal.clone());
            let stored: Option<String> = self
                .with_db(|db| {
                    Ok(db
                        .query_row("SELECT principal FROM jobs WHERE job_ref = ?1", [job_ref], |r| r.get::<_, Option<String>>(0))
                        .optional()?
                        .flatten())
                })?;
            let other = |x: &Option<String>| x.as_deref().is_some_and(|x| !x.is_empty() && x != p);
            if other(&live) || other(&stored) {
                return Ok(None);
            }
        }
        Ok(Some(record))
    }

    /// storage.ts:1090-1096 `taskJobs`.
    pub fn task_jobs(&self, task_ref: &str, principal: Option<&str>) -> Result<Vec<Value>> {
        self.ensure_open()?;
        let rows: Vec<(String, Option<String>, String)> = self.with_db(|db| {
            let mut stmt = db.prepare("SELECT job_ref, principal, record FROM jobs WHERE task_ref = ?1")?;
            let rows = stmt.query_map([task_ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        let mut out = Vec::new();
        for (job_ref, owner, record) in rows {
            if principal.is_some_and(|p| owner.as_deref().is_some_and(|o| !o.is_empty() && o != p)) {
                continue;
            }
            match self.job_record(&job_ref)? {
                Some(r) => out.push(r),
                None => out.push(serde_json::from_str(&record).unwrap_or(Value::Null)),
            }
        }
        Ok(out)
    }

    /// storage.ts:2141-2153 `checkRecord`.
    fn check_record(&self, ctx: &Context, predicate: &Value, outcome: &str, source: &str, evidence: Vec<String>, summary: &str) -> Value {
        json!({
            "kind": "check",
            "check_ref": id("check"),
            "task_ref": ctx.task_ref,
            "checked_at": crate::ids::now_iso(),
            "predicate": predicate,
            "outcome": outcome,
            "source": source,
            "evidence_refs": evidence.into_iter().take(20).collect::<Vec<_>>(),
            "summary": jsv::clip(summary, 1000),
        })
    }

    /// storage.ts:1039-1060 `check`: `job_finished`, `artifact_ready` and
    /// `request_settled`. `request_settled` reads the store, so a job finished
    /// before a restart still settles its request.
    pub fn check(&self, condition: &Value, ctx: &Context) -> Result<Value> {
        self.ensure_open()?;
        ctx.assert_authority()?;
        match str_or(condition, "kind", "").as_str() {
            "job_finished" => {
                let job = self.job_record(&str_or(condition, "job_ref", ""))?;
                let state = job.as_ref().and_then(|j| j.get("state")).and_then(Value::as_str).map(str::to_string);
                let outcome = match state.as_deref() {
                    Some("completed" | "failed" | "cancelled") => "satisfied",
                    Some("unknown") => "unknown",
                    _ => "pending",
                };
                let evidence = job.as_ref().and_then(|j| j.get("job_ref")).map(to_js_string).into_iter().collect();
                let summary = format!("Job state is {}.", state.as_deref().unwrap_or("absent"));
                Ok(self.check_record(ctx, condition, outcome, "process", evidence, &summary))
            }
            "artifact_ready" => {
                let found = self.find_artifact(ctx, condition.get("name"), condition.get("producing_request_id"))?;
                let ready = found.as_ref().is_some_and(|a| jsv::truthy(a.get("ready")));
                let evidence = found.as_ref().and_then(|a| a.get("artifact_ref")).map(to_js_string).into_iter().collect();
                let summary = if found.is_some() { "Artifact is ready." } else { "Artifact is not ready." };
                Ok(self.check_record(ctx, condition, if ready { "satisfied" } else { "pending" }, "filesystem", evidence, summary))
            }
            "request_settled" => {
                let request = fsx::bind_json(condition.get("request_id"));
                let latest: Option<(String, String)> = self.with_db(|db| {
                    Ok(db
                        .query_row(
                            "SELECT job_ref, state FROM jobs WHERE task_ref = ?1 AND request_id = ?2 ORDER BY rowid DESC LIMIT 1",
                            params![ctx.task_ref, request],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?)
                })?;
                let job = latest.map(|(job_ref, stored)| {
                    let live = self.inner.jobs.lock().get(&job_ref).map(|h| h.st.lock().state.clone());
                    (job_ref, live.unwrap_or(stored))
                });
                let outcome = match job.as_ref().map(|(_, s)| s.as_str()) {
                    None | Some("running") => "pending",
                    Some("unknown") => "unknown",
                    Some(_) => "satisfied",
                };
                let summary = format!("Request execution is {}.", job.as_ref().map_or("absent", |(_, s)| s.as_str()));
                let evidence = job.map(|(r, _)| r).into_iter().collect();
                Ok(self.check_record(ctx, condition, outcome, "process", evidence, &summary))
            }
            _ => Err(fail("WRONG_TOOL", "Storage check only evaluates job, artifact, and request predicates.", true)),
        }
    }

    /// storage.ts:2257-2286 `recoverJobs`: every `running` or `unknown` job
    /// becomes `unknown`; its termination is confirmed only when its process
    /// group is provably dead. Unconfirmed jobs stay tracked so they can be
    /// killed, but only if the group is still verifiably ours.
    pub(super) fn recover_jobs(&self) -> Result<()> {
        type Row = (String, String, String, Option<i64>, Option<i64>, i64, String, Option<String>, Option<String>, Option<i64>);
        let rows: Vec<Row> = self.with_db(|db| {
            let mut stmt = db.prepare(
                "SELECT job_ref, task_ref, request_id, pid, pgid, output_truncated, record, principal, boot_id, starttime FROM jobs WHERE state IN ('running', 'unknown')",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        for (job_ref, task_ref, request_id, pid, pgid, truncated, record, principal, boot_id, starttime) in rows {
            use ProcessGroupVerdict::*;
            let record = match serde_json::from_str::<Value>(&record) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            };
            let candidate = ProcessIdentity {
                boot_id: boot_id.unwrap_or_default(),
                pid: pid.unwrap_or(0),
                pgid: pgid.unwrap_or(0),
                starttime: starttime.unwrap_or(0),
            };
            let identity = identity_valid(Some(&candidate)).then_some(candidate);
            let verdict = match (&identity, pgid) {
                (Some(i), _) => inspect_process_group(i),
                (None, Some(g)) if g != 0 => {
                    if pgid_has_live_members(g) == Some(false) { Dead } else { Uncertain }
                }
                _ => Dead,
            };
            let confirmed = verdict == Dead;
            let mut next = record;
            next.insert("state".into(), json!("unknown"));
            next.insert("termination_confirmed".into(), json!(confirmed));
            let keeps_process = matches!(verdict, Ours | Uncertain);
            let st = JobState {
                state: "unknown".into(),
                record: Some(next),
                pgid: if keeps_process { pgid } else { None },
                pid: if keeps_process { pid } else { None },
                identity: if verdict == Dead { None } else { identity },
                has_child: false,
                exited: false,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stored: 0,
                truncated: truncated != 0,
                settled: true,
            };
            let h = Arc::new(JobHandle::new(job_ref.clone(), task_ref, request_id, principal, self.inner.opts.max_output_chars, st));
            {
                let st = h.st.lock();
                self.persist_job(&h, &st)?;
            }
            if !confirmed {
                self.inner.jobs.lock().insert(job_ref, h);
            }
        }
        Ok(())
    }

    /// storage.ts:1819-1863 `pruneResolvedJobLogs`: clear the output of settled
    /// jobs in tasks the journal shows terminal for longer than the retention
    /// window with no unsettled operation. Rows are kept; journal errors keep logs.
    pub(super) fn prune_resolved_job_logs(&self) -> Result<()> {
        let Some(view) = self.journal() else { return Ok(()) };
        let cutoff = iso_from_millis(self.clock() - self.inner.opts.job_log_retention_ms);
        let tasks: Vec<String> = self.with_db(|db| {
            let mut stmt = db.prepare(
                "SELECT task_ref FROM jobs GROUP BY task_ref
                 HAVING SUM(CASE WHEN state IN ('unknown', 'running') OR termination_confirmed = 0 THEN 1 ELSE 0 END) = 0
                 AND SUM(CASE WHEN COALESCE(stdout, '') != '' OR COALESCE(stderr, '') != ''
                   OR (json_valid(record) AND (COALESCE(json_extract(record, '$.stdout'), '') != '' OR COALESCE(json_extract(record, '$.stderr'), '') != ''))
                   THEN 1 ELSE 0 END) > 0",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        for task_ref in tasks {
            let Ok(state) = view.task_state(&task_ref) else { return Ok(()) };
            let Some(state) = state else { continue };
            if !is_journal_terminal_state(&state.state) || state.updated_at.as_str() > cutoff.as_str() {
                continue;
            }
            match view.has_unsettled(&task_ref) {
                Ok(false) => {}
                Ok(true) => continue,
                Err(_) => return Ok(()),
            }
            self.with_db(|db| {
                type JobLog = (String, String, Option<i64>, Option<String>, Option<String>, String);
                let jobs: Vec<JobLog> = {
                    let mut stmt = db.prepare(
                        "SELECT job_ref, state, termination_confirmed, stdout, stderr, record FROM jobs WHERE task_ref = ?1",
                    )?;
                    stmt.query_map([&task_ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                        .collect::<rusqlite::Result<_>>()?
                };
                for (job_ref, state, confirmed, stdout, stderr, record) in jobs {
                    if state == "unknown" || state == "running" || confirmed == Some(0) {
                        continue;
                    }
                    let mut record = match serde_json::from_str::<Value>(if record.is_empty() { "{}" } else { &record }) {
                        Ok(Value::Object(m)) => m,
                        _ => Map::new(),
                    };
                    let rstate = record.get("state").map(to_js_string).unwrap_or_default();
                    if rstate == "unknown" || rstate == "running" || record.get("termination_confirmed") == Some(&Value::Bool(false)) {
                        continue;
                    }
                    let has = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.is_empty());
                    if !(has(&stdout) || has(&stderr) || jsv::truthy(record.get("stdout")) || jsv::truthy(record.get("stderr"))) {
                        continue;
                    }
                    record.insert("stdout".into(), json!(""));
                    record.insert("stderr".into(), json!(""));
                    record.insert("output_truncated".into(), json!(true));
                    db.execute(
                        "UPDATE jobs SET stdout = '', stderr = '', output_truncated = 1, record = ?1 WHERE job_ref = ?2",
                        params![serde_json::to_string(&record).unwrap_or_default(), job_ref],
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
