//! The argv-only command runner every desktop helper goes through
//! (`run.ts`).
//!
//! - Never a shell: program plus argv, the daemon's environment plus additions.
//! - stdout and stderr together are capped (`MAX_HELPER_OUTPUT` by default).
//! - On timeout or cancellation the child gets SIGTERM, then SIGKILL after
//!   300 ms; if it has not closed 1800 ms after cancellation began the call
//!   fails with `CONTROL_UNSETTLED`.
//! - A cancelled or timed-out *mutating* helper is `OUTCOME_UNKNOWN` (input may
//!   already have been injected); a read-only one is a retry-safe `TIMEOUT`.
//! - Every child is supervised by its own task, so dropping the caller's future
//!   never orphans a half-run helper and never cancels it: only an explicit
//!   [`Cancel`] or the timeout stops a helper. This is what keeps a typing piece
//!   from being cut short (design: a cancelled type crashed GTK apps).
//! - Mutating helpers are counted globally until they close;
//!   [`wait_for_helpers`] is the 2.5 s "input settled" barrier.
//! - Apps ([`spawn_detached`]) start in a desktop scope of their own, outside
//!   ibarad's unit, so restarting ibara never closes them.

use crate::error::{IbaraError, Result, internal};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{oneshot, watch};

/// Default combined stdout+stderr bound (`MAX_HELPER_OUTPUT`, 32 MiB).
pub const MAX_HELPER_OUTPUT: usize = 32 * 1024 * 1024;
/// Grace between SIGTERM and SIGKILL.
pub const KILL_AFTER: Duration = Duration::from_millis(300);
/// How long after cancellation began a child may take to close.
pub const CONFIRM_WITHIN: Duration = Duration::from_millis(1800);
/// The "helpers settled" barrier used before releasing held input.
pub const HELPER_SETTLE: Duration = Duration::from_millis(2500);

static ACTIVE_MUTATING: AtomicUsize = AtomicUsize::new(0);
static MUTATING_SPAWNED: AtomicU64 = AtomicU64::new(0);

/// An explicit cancellation signal (the `AbortSignal` of the TypeScript runner).
#[derive(Clone, Debug)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(Arc::new(watch::Sender::new(false)))
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|cancelled| *cancelled).await;
    }
}

/// One helper invocation.
#[derive(Debug)]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_output: usize,
    expect_output: usize,
    mutates: bool,
    cancel: Option<Cancel>,
}

impl Cmd {
    /// A command with a 4 s timeout and the default output bound.
    pub fn new(program: impl Into<OsString>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(4),
            max_output: MAX_HELPER_OUTPUT,
            expect_output: 0,
            mutates: false,
            cancel: None,
        }
    }
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    /// Add or override one environment variable.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    pub fn envs(mut self, vars: &[(OsString, OsString)]) -> Self {
        self.env.extend(vars.iter().cloned());
        self
    }
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }
    /// Pre-size the stdout buffer (for example a known frame size), so large
    /// outputs are not copied while growing.
    pub fn expect_output(mut self, bytes: usize) -> Self {
        self.expect_output = bytes;
        self
    }
    /// The helper may inject input or change the desktop.
    pub fn mutates(mut self, mutates: bool) -> Self {
        self.mutates = mutates;
        self
    }
    pub fn cancel(mut self, cancel: &Cancel) -> Self {
        self.cancel = Some(cancel.clone());
        self
    }
    fn label(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }
}

/// A finished helper.
#[derive(Debug)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status.success()
    }
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    /// Trimmed stderr, else trimmed stdout, else `"<program> exited <code>"`.
    pub fn failure_text(&self, program: &str) -> String {
        let err = self.stderr_text();
        let err = err.trim();
        if !err.is_empty() {
            return err.to_string();
        }
        let out = self.stdout_text();
        let out = out.trim();
        if !out.is_empty() {
            return out.to_string();
        }
        match self.code() {
            Some(code) => format!("{program} exited {code}"),
            None => format!("{program} was killed by a signal"),
        }
    }
}

/// Clip to at most `max` characters, never splitting a character.
pub fn clip(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// Mutating helpers spawned since the process started. The desktop layer
/// compares this before and after an effect to tell whether anything that
/// could have injected input actually ran (`nativeEffectStarted`, §2).
pub fn mutating_spawn_count() -> u64 {
    MUTATING_SPAWNED.load(Ordering::SeqCst)
}

/// Mutating helpers that have not closed yet.
pub fn active_mutating_helpers() -> usize {
    ACTIVE_MUTATING.load(Ordering::SeqCst)
}

/// Wait until no mutating helper is running, polling every 20 ms up to
/// `limit` (2500 ms in production); otherwise `CONTROL_UNSETTLED`.
pub async fn wait_for_helpers(limit: Duration) -> Result<()> {
    let start = Instant::now();
    while active_mutating_helpers() > 0 && start.elapsed() < limit {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if active_mutating_helpers() > 0 {
        return Err(IbaraError::new(
            "CONTROL_UNSETTLED",
            "An input helper is still running; manual control is not yet safe.",
            false,
        )
        .requires_reconciliation());
    }
    Ok(())
}

struct HelperGuard;

impl HelperGuard {
    fn enter() -> Self {
        ACTIVE_MUTATING.fetch_add(1, Ordering::SeqCst);
        MUTATING_SPAWNED.fetch_add(1, Ordering::SeqCst);
        HelperGuard
    }
}

impl Drop for HelperGuard {
    fn drop(&mut self) {
        ACTIVE_MUTATING.fetch_sub(1, Ordering::SeqCst);
    }
}

fn spawn_error(label: &str, error: std::io::Error) -> IbaraError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return IbaraError::new("CAPABILITY_UNAVAILABLE", format!("Command not found: {label}."), true)
            .with("command", label)
            .with("execution_not_started", true);
    }
    internal(format!("Could not start {label}: {error}"))
        .with("command", label)
        .with("execution_not_started", true)
}

fn cancelled_before_start(label: &str) -> IbaraError {
    IbaraError::new("TIMEOUT", "Native command cancelled before start.", true)
        .with("command", label)
        .with("execution_not_started", true)
}

fn cancellation(label: &str, mutates: bool, timed_out: bool) -> IbaraError {
    if mutates {
        let message = if timed_out {
            "Native command timed out after possible input; reconcile before retry."
        } else {
            "Native command cancelled after possible input; reconcile before retry."
        };
        IbaraError::new("OUTCOME_UNKNOWN", message, false)
            .requires_reconciliation()
            .with("command", label)
    } else {
        let message = if timed_out { "Native command timed out." } else { "Native command cancelled." };
        IbaraError::new("TIMEOUT", message, true).with("command", label)
    }
}

/// Run a helper to completion (or cancellation) and collect its output.
pub async fn run(cmd: Cmd) -> Result<Output> {
    let label = cmd.label();
    if cmd.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
        return Err(cancelled_before_start(&label));
    }
    let mut command = tokio::process::Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|e| spawn_error(&label, e))?;
    let guard = cmd.mutates.then(HelperGuard::enter);
    let (tx, rx) = oneshot::channel();
    tokio::spawn(supervise(child, cmd, label, guard, tx));
    rx.await
        .unwrap_or_else(|_| Err(internal("Native helper supervisor ended without a result.")))
}

/// Start a program detached (app launch, §3.8): no stdio, its own process
/// group, and no waiting for it to end; the runtime reaps it when it exits.
///
/// Where a systemd user manager runs (every Omarchy session), the program
/// starts the way the desktop starts apps: in a transient scope of its own in
/// `app-graphical.slice`, named `app-ibara-<program>-<random>.scope` as uwsm
/// names Omarchy's. That keeps it out of ibarad's unit, whose `KillMode=mixed`
/// kills everything left in it when ibara restarts (an update, the Restart
/// ibara repair, a crash), so the app and a person's unsaved work in it
/// outlive ibara; ibara's own helpers stay in the unit and still end with it.
/// `systemd-run --scope` moves itself into the scope and then execs the
/// program, so the PID returned is the program's and the program keeps the
/// environment given here. Returns once the program runs in its scope.
pub async fn spawn_detached(cmd: Cmd) -> Result<u32> {
    let label = cmd.label();
    if cmd.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
        return Err(cancelled_before_start(&label));
    }
    let program = find_program(&cmd.program, var(&cmd.env, "PATH").as_deref())
        .ok_or_else(|| spawn_error(&label, std::io::ErrorKind::NotFound.into()))?;
    let scope = scope_launcher(&cmd.env).map(|systemd_run| (systemd_run, scope_unit(&program)));
    let mut command = match &scope {
        Some((systemd_run, unit)) => {
            let mut command = tokio::process::Command::new(systemd_run);
            let name = program.file_name().unwrap_or(program.as_os_str()).to_string_lossy();
            command
                .args(["--user", "--scope", "--quiet", "--collect"])
                .arg(format!("--slice={APP_SLICE}"))
                .arg(format!("--unit={unit}"))
                .arg(format!("--description={name}"))
                .arg("--")
                .arg(&program);
            command
        }
        None => tokio::process::Command::new(&program),
    };
    // An app starts in the person's home folder, as the desktop starts it, not
    // in ibarad's working directory (the package's own folder).
    let home = var(&cmd.env, "HOME").map(std::path::PathBuf::from).filter(|p| p.is_dir());
    command
        .current_dir(home.unwrap_or_else(|| "/".into()))
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(false);
    let mut child = command.spawn().map_err(|e| spawn_error(&label, e))?;
    if cmd.mutates {
        MUTATING_SPAWNED.fetch_add(1, Ordering::SeqCst);
    }
    let pid = child.id().unwrap_or(0);
    if let Some((systemd_run, unit)) = &scope {
        in_scope(&mut child, pid, systemd_run, unit, cmd.timeout, &label).await?;
    }
    Ok(pid)
}

/// Where the desktop starts apps (systemd's and uwsm's `app-graphical.slice`).
const APP_SLICE: &str = "app-graphical.slice";
/// How often a launch looks for its program in its scope.
const SCOPE_POLL: Duration = Duration::from_millis(5);

/// `key` as the program will see it: the command's own value, else the daemon's.
fn var(env: &[(OsString, OsString)], key: &str) -> Option<OsString> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).or_else(|| std::env::var_os(key))
}

/// `program` as `exec` finds it: a path with a slash as it is, a bare name on
/// `path` (else the daemon's `PATH`); executable files only.
fn find_program(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let executable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let named = Path::new(program);
    if program.as_bytes().contains(&b'/') {
        return executable(named).then(|| named.to_path_buf());
    }
    let paths = path.map(OsStr::to_os_string).or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&paths).filter(|dir| dir.is_absolute()).map(|dir| dir.join(named)).find(|p| executable(p))
}

/// `systemd-run`, when a systemd user manager serves the program's session
/// (`$XDG_RUNTIME_DIR/systemd/private` is its socket). Without one there is no
/// unit to outlive, and programs start directly.
fn scope_launcher(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let runtime = var(env, "XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty())?;
    let socket = Path::new(&runtime).join("systemd/private");
    std::fs::metadata(socket).ok().filter(|m| m.file_type().is_socket())?;
    find_program(OsStr::new("systemd-run"), var(env, "PATH").as_deref())
}

/// `app-ibara-<program>-<random>.scope`: systemd's name for an app a launcher
/// started (`app-<launcher>-<app>-<random>.scope`), the program's file name
/// escaped as `systemd-escape` does.
fn scope_unit(program: &Path) -> String {
    let name = program.file_name().unwrap_or(program.as_os_str()).as_bytes();
    let mut unit = String::from("app-ibara-");
    for (i, &b) in name.iter().take(32).enumerate() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b':' || (b == b'.' && i > 0) {
            unit.push(char::from(b));
        } else {
            unit.push_str(&format!("\\x{b:02x}"));
        }
    }
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("{unit}-{}.scope", &random[..8])
}

/// Wait until the program runs in `unit`: systemd-run moves itself there,
/// waits for systemd to start the scope, then execs the program in its place
/// (so `/proc/PID/exe` stops being systemd-run). When it ends first with a
/// failure, the manager refused the scope (or the program) and nothing
/// started; a program that already ended cleanly (a launcher handing off) did
/// start.
async fn in_scope(child: &mut Child, pid: u32, systemd_run: &Path, unit: &str, within: Duration, label: &str) -> Result<()> {
    let suffix = format!("/{unit}");
    let launcher = std::fs::canonicalize(systemd_run).unwrap_or_else(|_| systemd_run.to_path_buf());
    let until = Instant::now() + within;
    loop {
        let cgroups = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        if cgroups.lines().any(|line| line.ends_with(&suffix))
            && std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe != launcher)
        {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", format!("The session's service manager did not start {label} ({status})."), true)
                    .with("command", label)
                    .with("execution_not_started", true));
            }
            Ok(None) => {}
            Err(e) => return Err(internal(format!("Could not follow {label} as it started: {e}")).with("command", label)),
        }
        if Instant::now() >= until {
            let message = format!("{label} did not start within {} s; it may still open.", within.as_secs());
            return Err(IbaraError::new("OUTCOME_UNKNOWN", message, false).requires_reconciliation().with("command", label));
        }
        tokio::time::sleep(SCOPE_POLL).await;
    }
}

async fn read_some<R: AsyncRead + Unpin>(pipe: &mut Option<R>, buf: &mut [u8]) -> std::io::Result<usize> {
    match pipe {
        Some(pipe) => pipe.read(buf).await,
        None => std::future::pending().await,
    }
}

fn sigterm(child: &Child) {
    // `id()` is None once the child has been reaped, so a recycled PID is never signalled.
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) with a PID we own and have not reaped; no memory is touched.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

async fn supervise(
    mut child: Child,
    cmd: Cmd,
    label: String,
    guard: Option<HelperGuard>,
    tx: oneshot::Sender<Result<Output>>,
) {
    let mut tx = Some(tx);
    let stdin_pipe = child.stdin.take();
    let input = cmd.stdin;
    let writer = async move {
        if let (Some(mut pipe), Some(bytes)) = (stdin_pipe, input) {
            // EPIPE after the child exits is expected; its exit stays authoritative.
            let _ = pipe.write_all(&bytes).await;
            let _ = pipe.shutdown().await;
        }
    };
    tokio::pin!(writer);
    let mut writer_done = false;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let mut stdout = Vec::with_capacity(cmd.expect_output.min(cmd.max_output));
    let mut stderr = Vec::new();
    let mut obuf = vec![0u8; 64 * 1024];
    let mut ebuf = vec![0u8; 4096];
    let mut size = 0usize;
    let mut status: Option<ExitStatus> = None;
    let mut failure: Option<IbaraError> = None;

    let far = tokio::time::Instant::now() + Duration::from_secs(86_400 * 365);
    let deadline = tokio::time::sleep(cmd.timeout);
    let kill_at = tokio::time::sleep_until(far);
    let confirm_at = tokio::time::sleep_until(far);
    tokio::pin!(deadline, kill_at, confirm_at);
    let cancel = cmd.cancel;
    let cancelled = async move {
        match cancel {
            Some(cancel) => cancel.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(cancelled);
    let mut cancelling = false;
    let mut killed = false;

    loop {
        if status.is_some() && out_pipe.is_none() && err_pipe.is_none() {
            break;
        }
        let mut begin: Option<IbaraError> = None;
        tokio::select! {
            _ = &mut writer, if !writer_done => writer_done = true,
            read = read_some(&mut out_pipe, &mut obuf) => match read {
                Ok(0) | Err(_) => out_pipe = None,
                Ok(n) => {
                    size += n;
                    if size <= cmd.max_output {
                        stdout.extend_from_slice(&obuf[..n]);
                    } else if !cancelling {
                        begin = Some(internal("Native helper output exceeded its configured limit.").with("command", &label));
                    }
                }
            },
            read = read_some(&mut err_pipe, &mut ebuf) => match read {
                Ok(0) | Err(_) => err_pipe = None,
                Ok(n) => {
                    size += n;
                    if size <= cmd.max_output {
                        stderr.extend_from_slice(&ebuf[..n]);
                    } else if !cancelling {
                        begin = Some(internal("Native helper output exceeded its configured limit.").with("command", &label));
                    }
                }
            },
            exit = child.wait(), if status.is_none() => match exit {
                Ok(exit) => status = Some(exit),
                Err(e) => {
                    // The child cannot be waited on; treat it as closed and report.
                    status = Some(ExitStatus::default());
                    if failure.is_none() {
                        failure = Some(internal(format!("Could not wait for {label}: {e}")));
                    }
                }
            },
            _ = &mut deadline, if !cancelling => begin = Some(cancellation(&label, cmd.mutates, true)),
            _ = &mut cancelled, if !cancelling => begin = Some(cancellation(&label, cmd.mutates, false)),
            _ = &mut kill_at, if cancelling && !killed => {
                killed = true;
                if status.is_none() {
                    let _ = child.start_kill();
                }
            },
            _ = &mut confirm_at, if cancelling && tx.is_some() => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(Err(IbaraError::new(
                        "CONTROL_UNSETTLED",
                        "Native helper termination could not be confirmed. Do not hand over control.",
                        false,
                    )
                    .requires_reconciliation()
                    .with("command", &label)));
                }
            },
        }
        if let Some(error) = begin {
            cancelling = true;
            failure = Some(error);
            sigterm(&child);
            let now = tokio::time::Instant::now();
            kill_at.as_mut().reset(now + KILL_AFTER);
            confirm_at.as_mut().reset(now + CONFIRM_WITHIN);
        }
    }
    // The helper has closed: it no longer counts as in flight.
    drop(guard);
    if let Some(tx) = tx {
        let result = match (failure, status) {
            (Some(error), _) => Err(error),
            (None, Some(status)) => Ok(Output { stdout, stderr, status }),
            (None, None) => Err(internal(format!("{label} closed without an exit status."))),
        };
        let _ = tx.send(result);
    }
}

/// Tests that keep a mutating helper in flight hold this for writing; tests
/// whose effects wait for every helper to settle (`release_input`) hold it
/// for reading, so the process-wide count never mixes the two.
#[cfg(test)]
pub(crate) static HELPERS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Cmd {
        Cmd::new("/bin/sh").arg("-c").arg(script)
    }

    #[tokio::test]
    async fn missing_program_is_unavailable_and_never_started() {
        let err = run(Cmd::new("/nonexistent/ibara-helper")).await.unwrap_err();
        assert_eq!(err.code, "CAPABILITY_UNAVAILABLE");
        assert_eq!(err.details.get("execution_not_started"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn pre_cancelled_command_never_starts() {
        let cancel = Cancel::new();
        cancel.cancel();
        let err = run(sh("exit 0").cancel(&cancel)).await.unwrap_err();
        assert_eq!(err.code, "TIMEOUT");
        assert_eq!(err.details.get("execution_not_started"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn read_only_timeout_is_retry_safe() {
        let err = run(sh("exec sleep 5").timeout(Duration::from_millis(100))).await.unwrap_err();
        assert_eq!(err.code, "TIMEOUT");
        assert!(err.retry_safe);
    }

    #[tokio::test]
    async fn output_over_the_cap_fails() {
        let err = run(sh("head -c 100000 /dev/zero").max_output(1000)).await.unwrap_err();
        assert_eq!(err.code, "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn stdin_reaches_the_helper() {
        let out = run(Cmd::new("cat").stdin(b"piece".to_vec())).await.unwrap();
        assert_eq!(out.stdout, b"piece");
    }

    // Apps an agent launches. Failure cases, written first:
    // - the app stays in ibarad's control group, so restarting ibara
    //   (`KillMode=mixed`) kills it and its unsaved work
    // - the PID ibara keeps is systemd-run's rather than the app's, so the
    //   app's windows are not recognised as the task's
    // - the app loses the environment ibara gives apps
    // - a launch the session's service manager refuses, or of a program that
    //   is not there, is reported as started, or starts the app anyway

    fn cgroup(pid: u32) -> String {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        text.lines().last().unwrap().split(':').nth(2).unwrap().to_string()
    }

    #[tokio::test]
    async fn a_launched_app_runs_in_its_own_desktop_scope_as_itself_with_ibaras_environment() {
        if scope_launcher(&[]).is_none() {
            eprintln!("skipped: no systemd user manager here, so there is no unit to outlive");
            return;
        }
        let home = std::env::var("HOME").unwrap();
        let pid = spawn_detached(Cmd::new("sleep").arg("30").env("IBARA_LAUNCH_CHECK", "d16").env("HOME", &home)).await.unwrap();
        let (app, ours) = (cgroup(pid), cgroup(std::process::id()));
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"));
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"));
        // The kernel fills in the new program's environment just after its
        // exe changes, so an empty read right after exec means "not yet".
        let mut environ = Vec::new();
        for _ in 0..200 {
            environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            if !environ.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // SAFETY: kill(2) on the test's own sleep; no memory is touched.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };

        assert_ne!(app, ours, "the app must leave the launcher's control group");
        assert_eq!(cwd.unwrap(), std::path::PathBuf::from(&home), "the app must start in the person's home folder");
        let (slice, unit) = app.rsplit_once('/').unwrap();
        assert!(slice.ends_with("/app-graphical.slice"), "{app}");
        let random = unit.strip_prefix("app-ibara-sleep-").and_then(|u| u.strip_suffix(".scope")).unwrap_or_else(|| panic!("{unit}"));
        assert!(random.len() == 8 && random.bytes().all(|b| b.is_ascii_hexdigit()), "{unit}");
        assert_eq!(exe.unwrap(), std::fs::canonicalize(find_program("sleep".as_ref(), None).unwrap()).unwrap(), "the PID is the app's");
        assert!(environ.split(|b| *b == 0).any(|v| v == b"IBARA_LAUNCH_CHECK=d16"), "the app keeps ibara's environment");
    }

    #[tokio::test]
    async fn a_launch_that_cannot_start_says_so_and_never_starts_the_app() {
        let err = spawn_detached(Cmd::new("/nonexistent/ibara-app")).await.unwrap_err();
        assert_eq!(err.code, "CAPABILITY_UNAVAILABLE");
        assert_eq!(err.details.get("execution_not_started"), Some(&serde_json::json!(true)));

        if find_program("systemd-run".as_ref(), None).is_none() {
            eprintln!("skipped: no systemd-run here");
            return;
        }
        // A user manager that does not answer: its socket is there, nobody listens.
        let dir = std::env::temp_dir().join(format!("ibara-launch-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("systemd")).unwrap();
        drop(std::os::unix::net::UnixListener::bind(dir.join("systemd/private")).unwrap());
        let ran = dir.join("ran");
        let err = spawn_detached(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("touch '{}'", ran.display()))
                .env("XDG_RUNTIME_DIR", &dir)
                .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}/bus", dir.display())),
        )
        .await
        .unwrap_err();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = ran.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(err.code, "CAPABILITY_UNAVAILABLE", "{err:?}");
        assert_eq!(err.details.get("execution_not_started"), Some(&serde_json::json!(true)));
        assert!(!started, "a refused launch must not start the app some other way");
    }

    /// Everything that touches the global in-flight count runs in one test so
    /// parallel tests cannot disturb it, and tests that wait for it to settle
    /// wait for this one ([`HELPERS`]).
    #[tokio::test]
    async fn mutating_helpers_escalate_and_are_tracked_until_closed() {
        let _only = HELPERS.write().await;
        // Ignores SIGTERM: SIGKILL after 300 ms closes it, so the outcome is
        // unknown rather than unsettled.
        let started = Instant::now();
        let err = run(sh("trap '' TERM; exec sleep 5").timeout(Duration::from_millis(100)).mutates(true))
            .await
            .unwrap_err();
        assert_eq!(err.code, "OUTCOME_UNKNOWN");
        assert!(!err.retry_safe);
        assert!(started.elapsed() < CONFIRM_WITHIN, "SIGKILL should have closed it");
        wait_for_helpers(Duration::from_millis(500)).await.unwrap();

        // A grandchild keeps the pipes open: termination cannot be confirmed,
        // and the helper stays in flight until it really closes.
        let err = run(
            sh("(trap '' TERM; sleep 3) & exec sleep 5")
                .timeout(Duration::from_millis(100))
                .mutates(true),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "CONTROL_UNSETTLED");
        let err = wait_for_helpers(Duration::from_millis(100)).await.unwrap_err();
        assert_eq!(err.code, "CONTROL_UNSETTLED");
        wait_for_helpers(Duration::from_secs(5)).await.unwrap();
    }
}
