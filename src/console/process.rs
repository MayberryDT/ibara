//! Local helpers the console still runs: bounded one-shot programs (`spawnSync`
//! in the bridge) and detached launches (Moonlight, terminals).

use super::envelope::Fault;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Default stdout and stderr bound (`MAX_STDOUT`).
pub const MAX_OUTPUT: usize = 512 * 1024;

/// `command -v NAME`: the first executable regular file on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let executable = |path: &Path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).filter(|dir| dir.is_absolute()).map(|dir| dir.join(name)).find(|p| executable(p))
}

/// One finished program.
#[derive(Debug)]
pub struct Output {
    /// The exit code; `None` when a signal ended it.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// `spawnSync` options.
pub struct Run {
    pub timeout: Duration,
    pub input: Option<Vec<u8>>,
    pub env: Vec<(&'static str, OsString)>,
    /// Bound on stdout and on stderr, each (`maxBuffer`).
    pub max_output: usize,
}

impl Default for Run {
    fn default() -> Self {
        Run { timeout: Duration::from_secs(15), input: None, env: Vec::new(), max_output: MAX_OUTPUT }
    }
}

impl Run {
    pub fn timeout(timeout: Duration) -> Self {
        Run { timeout, ..Run::default() }
    }
}

fn signal(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        // SAFETY: kill(2) on a child we spawned and have not reaped.
        unsafe { libc::kill(pid, signal) };
    }
}

/// Read to EOF; `None` once more than `limit` bytes arrive.
async fn read_capped<R: AsyncRead + Unpin>(reader: &mut R, limit: usize, pid: Option<u32>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut block = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut block).await {
            Ok(0) | Err(_) => return Some(out),
            Ok(n) if out.len() + n > limit => {
                signal(pid, libc::SIGKILL);
                return None;
            }
            Ok(n) => out.extend_from_slice(&block[..n]),
        }
    }
}

/// Run `program ARGS…` to completion, as `spawnSync` did: stdin is the input (or
/// empty), each output stream is bounded, and a program that passes its deadline
/// is terminated and reported as `spawnSync NAME ETIMEDOUT`.
pub async fn run<S: AsRef<OsStr>>(program: impl AsRef<OsStr>, args: &[S], options: Run) -> Result<Output, Fault> {
    let program = program.as_ref();
    let name = program.to_string_lossy().into_owned();
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(if options.input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &options.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|e| {
        let reason = if e.kind() == std::io::ErrorKind::NotFound { "ENOENT".to_string() } else { e.to_string() };
        Fault::Plain(format!("spawnSync {name} {reason}"))
    })?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let input = options.input;
    let limit = options.max_output;
    let work = async {
        let feed = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                let _ = stdin.write_all(&input).await;
            }
        };
        let (_, out, err) = tokio::join!(feed, read_capped(&mut stdout, limit, pid), read_capped(&mut stderr, limit, pid));
        (out, err, child.wait().await)
    };
    match tokio::time::timeout(options.timeout, work).await {
        Err(_) => {
            signal(pid, libc::SIGTERM);
            Err(Fault::Timeout(format!("spawnSync {name} ETIMEDOUT")))
        }
        Ok((Some(stdout), Some(stderr), status)) => Ok(Output { status: status?.code(), stdout, stderr }),
        Ok(_) => Err(Fault::Plain(format!("spawnSync {name} ENOBUFS"))),
    }
}

/// `launch(argv)`: start a program detached, in its own process group, with no
/// stdio, and return its pid. When `systemd-run` is available the program runs in
/// its own transient scope, so restarting `ibarad` never closes a viewer or
/// terminal the person opened (`systemd-run --scope` execs the program itself,
/// so the pid is the program's).
pub fn launch<S: AsRef<OsStr>>(argv: &[S]) -> Result<u32, Fault> {
    spawn_detached(argv, false).map(|(pid, _)| pid)
}

/// `launch`, with `input` on the program's stdin, which is closed after it
/// (the viewer reads its ticket that way, never from its arguments).
pub async fn launch_with_input<S: AsRef<OsStr>>(argv: &[S], input: &[u8]) -> Result<u32, Fault> {
    let (pid, stdin) = spawn_detached(argv, true)?;
    if let Some(mut stdin) = stdin {
        let written = tokio::time::timeout(Duration::from_secs(5), async {
            stdin.write_all(input).await?;
            stdin.shutdown().await
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            return Err(Fault::plain("The program did not take its input."));
        }
    }
    Ok(pid)
}

fn spawn_detached<S: AsRef<OsStr>>(argv: &[S], piped: bool) -> Result<(u32, Option<tokio::process::ChildStdin>), Fault> {
    let (program, rest) = argv.split_first().ok_or_else(|| Fault::plain("Nothing to launch."))?;
    let mut command = match which("systemd-run") {
        Some(systemd_run) => {
            let mut command = tokio::process::Command::new(systemd_run);
            command.args(["--user", "--scope", "--quiet", "--collect", "--"]).arg(program);
            command
        }
        None => tokio::process::Command::new(program),
    };
    let stdin = if piped { Stdio::piped() } else { Stdio::null() };
    command.args(rest).stdin(stdin).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let mut child = command.spawn().map_err(|e| Fault::Plain(format!("spawn {} {e}", program.as_ref().to_string_lossy())))?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take();
    // Reap it whenever it exits; the program itself is never waited for.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok((pid, stdin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn a_program_past_its_deadline_is_a_timeout_fault() {
        let fault = rt().block_on(run("sleep", &["5"], Run::timeout(Duration::from_millis(100)))).unwrap_err();
        assert!(matches!(fault, Fault::Timeout(m) if m == "spawnSync sleep ETIMEDOUT"));
    }

    #[test]
    fn output_over_the_bound_is_enobufs_and_a_missing_program_is_enoent() {
        let options = Run { max_output: 1024, ..Run::default() };
        let fault = rt().block_on(run("head", &["-c", "100000", "/dev/zero"], options)).unwrap_err();
        assert!(matches!(fault, Fault::Plain(m) if m == "spawnSync head ENOBUFS"));
        let fault = rt().block_on(run("/nonexistent/ibara-helper", &[] as &[&str], Run::default())).unwrap_err();
        assert!(matches!(fault, Fault::Plain(m) if m.ends_with("ENOENT")));
    }

    #[test]
    fn input_reaches_stdin_and_the_exit_code_is_kept() {
        let options = Run { input: Some(b"hello\n".to_vec()), ..Run::default() };
        let out = rt().block_on(run("sh", &["-c", "cat; exit 3"], options)).unwrap();
        assert_eq!((out.status, out.stdout.as_slice()), (Some(3), b"hello\n".as_slice()));
    }
}
