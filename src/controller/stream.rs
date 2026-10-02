//! `ibara-stream` as a child of `ibarad`: the live [`StreamPort`].
//!
//! Its files live in `<state>/stream/` (0700): `ibara-stream.conf`,
//! `apps.json`, its state and credential files, the TLS key and certificate it
//! makes on first start, `ibara-stream.log`, and `control.sock`, the private
//! control socket (newline JSON, `v: 1`). It listens on this computer's
//! Tailscale address only and encrypts every stream. The child dies with
//! `ibarad` (`PR_SET_PDEATHSIG`) and is killed when the port is dropped. It
//! runs with glibc's mmap threshold held at its default (see
//! [`MALLOC_TUNABLES`]).
//!
//! `IBARA_STREAM_BIN`, `IBARA_STREAM_BIND` and `IBARA_STREAM_PORT` override the
//! program, the address and the base port.

use super::ports::{LocalFuture, Settlement, StreamPort, StreamStatus};
use crate::error::{IbaraError, Result};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Child;

const BIN: &str = "/usr/bin/ibara-stream";
/// The usual base port: HTTP on it, HTTPS five below, RTSP 21 above.
const BASE_PORT: u16 = 47989;
/// Probing the encoders takes a few seconds on a slow computer.
const START_DEADLINE: Duration = Duration::from_secs(20);
const REQUEST_DEADLINE: Duration = Duration::from_secs(3);
/// A revoke waits up to 2 s for held keys to be released.
const REVOKE_DEADLINE: Duration = Duration::from_secs(8);
const STOP_GRACE: Duration = Duration::from_secs(5);
const MAX_REPLY: u64 = 64 * 1024;
/// The stream allocates and frees frame-sized buffers all the time. glibc
/// raises its mmap threshold to the size of each large block freed, after
/// which such blocks come from per-thread arenas that keep their memory once
/// freed: on Tulip1 (1920×1080, VA-API) the stream held 138 MiB, 82 MiB of it
/// freed memory. Holding the threshold at glibc's default of 128 KiB returns
/// large blocks to the system when they are freed.
const MALLOC_TUNABLES: &str = "glibc.malloc.mmap_threshold=131072";

fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", message, true)
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The live stream port.
pub struct LiveStream {
    desktop: Rc<dyn super::ports::DesktopPort>,
    program: PathBuf,
    dir: PathBuf,
    base_port: u16,
    child: RefCell<Option<Child>>,
    /// Requests on the control socket, for `id`.
    sequence: Cell<u64>,
}

impl LiveStream {
    pub fn new(state_dir: &Path, desktop: Rc<dyn super::ports::DesktopPort>) -> LiveStream {
        let program = std::env::var_os("IBARA_STREAM_BIN").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| BIN.into());
        let base_port = std::env::var("IBARA_STREAM_PORT").ok().and_then(|p| p.parse().ok()).filter(|p: &u16| *p > 5).unwrap_or(BASE_PORT);
        LiveStream {
            desktop,
            program, dir: state_dir.join("stream"), base_port, child: RefCell::new(None), sequence: Cell::new(0) }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    /// The address viewers reach: `IBARA_STREAM_BIND`, else the first line of
    /// `tailscale ip -4`.
    async fn bind_address(&self) -> Result<String> {
        if let Some(address) = std::env::var("IBARA_STREAM_BIND").ok().filter(|a| !a.is_empty()) {
            return Ok(address);
        }
        let out = crate::desktop::run::run(crate::desktop::run::Cmd::new("tailscale").args(["ip", "-4"]).timeout(Duration::from_secs(5)))
            .await
            .map_err(|_| unavailable("Tailscale is not running on this computer, so it cannot be controlled from another."))?;
        let address = out.stdout_text().lines().next().unwrap_or("").trim().to_string();
        if !out.success() || address.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(unavailable("Tailscale is not running on this computer, so it cannot be controlled from another."));
        }
        Ok(address)
    }

    /// Write the configuration for this start.
    fn configure(&self, address: &str) -> Result<PathBuf> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        let file = |name: &str| self.dir.join(name).display().to_string();
        let apps = self.dir.join("apps.json");
        if !apps.exists() {
            std::fs::write(&apps, json!({"env": {}, "apps": [{"name": "Desktop", "image-path": "desktop.png"}]}).to_string())?;
        }
        let mut config = format!(
            "port = {port}\nbind_address = {address}\naddress_family = ipv4\ncapture = wlr\n\
             hevc_mode = 1\nav1_mode = 1\n\
             lan_encryption_mode = 2\nwan_encryption_mode = 2\n\
             file_apps = {apps}\nfile_state = {state}\ncredentials_file = {credentials}\n\
             pkey = {key}\ncert = {cert}\nlog_path = {log}\nmin_log_level = info\n",
            port = self.base_port,
            apps = file("apps.json"),
            state = file("state.json"),
            credentials = file("credentials.json"),
            key = file("key.pem"),
            cert = file("cert.pem"),
            log = file("ibara-stream.log"),
        );
        if let Some(node) = self.desktop.vaapi_render_node() {
            config.push_str(&format!(
                "encoder = vaapi\nadapter_name = {}\n",
                node.display()
            ));
        }
        let path = self.dir.join("ibara-stream.conf");
        std::fs::write(&path, config)?;
        Ok(path)
    }

    /// One request on the control socket; the reply's payload once `ok`.
    async fn request(&self, op: &str, mut body: Value, deadline: Duration) -> Result<Value> {
        let id = self.sequence.get() + 1;
        self.sequence.set(id);
        body["v"] = json!(1);
        body["id"] = json!(id);
        body["op"] = json!(op);
        let exchange = async {
            let mut stream = UnixStream::connect(self.socket()).await?;
            stream.write_all(format!("{body}\n").as_bytes()).await?;
            let mut line = String::new();
            BufReader::new(stream.take(MAX_REPLY)).read_line(&mut line).await?;
            Ok::<_, std::io::Error>(line)
        };
        let line = tokio::time::timeout(deadline, exchange)
            .await
            .map_err(|_| unavailable("Screen sharing did not answer in time."))?
            .map_err(|_| unavailable("Screen sharing is not running."))?;
        let reply: Value = serde_json::from_str(&line).map_err(|_| unavailable("Screen sharing answered something unreadable."))?;
        if reply["ok"] != json!(true) {
            let message = reply["error"]["message"].as_str().unwrap_or("Screen sharing refused.");
            return Err(unavailable(format!("Screen sharing refused: {message}")));
        }
        Ok(reply)
    }

    fn parse_status(&self, reply: &Value) -> Result<StreamStatus> {
        let s = &reply["status"];
        let status = StreamStatus {
            generation: s["generation"].as_u64().unwrap_or(0),
            admission_closed: s["admission_closed"].as_bool().unwrap_or(true),
            idle: s["idle"].as_bool().unwrap_or(false),
            encoder: s["encoder"].as_str().unwrap_or("").to_string(),
            software_cap: s["software_cap"].as_str().map(str::to_string),
            server_cert_sha256: s["server_cert_sha256"].as_str().unwrap_or("").to_string(),
            pid: s["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).unwrap_or(0),
            http_port: self.base_port,
            https_port: self.base_port - 5,
        };
        if !is_sha256_hex(&status.server_cert_sha256) {
            return Err(unavailable("Screen sharing has no certificate yet."));
        }
        Ok(status)
    }

    /// Our child has exited (or there is none).
    fn child_gone(&self) -> bool {
        let mut child = self.child.borrow_mut();
        match child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => false,
            Some(_) => {
                *child = None;
                true
            }
            None => true,
        }
    }

    async fn status_now(&self) -> Result<Option<StreamStatus>> {
        match self.request("status", json!({}), REQUEST_DEADLINE).await {
            Ok(reply) => self.parse_status(&reply).map(Some),
            // Nothing listens: no stream runs. A child that is still starting
            // counts as running and not yet answering.
            Err(_) if self.child_gone() => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn spawn_and_wait(&self) -> Result<StreamStatus> {
        if !self.available() {
            return Err(unavailable("Screen sharing (ibara-stream) is not installed on this computer."));
        }
        let address = self.bind_address().await?;
        let config = self.configure(&address)?;
        let tunables = match std::env::var("GLIBC_TUNABLES") {
            Ok(own) if !own.is_empty() => format!("{own}:{MALLOC_TUNABLES}"),
            _ => MALLOC_TUNABLES.to_string(),
        };
        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg(&config)
            .current_dir(&self.dir)
            .env("IBARA_STREAM_CONTROL_SOCKET", self.socket())
            .env("GLIBC_TUNABLES", tunables)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // SAFETY: prctl(2) in the forked child before exec touches no memory of the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| unavailable(format!("Screen sharing could not start: {e}")))?;
        *self.child.borrow_mut() = Some(child);
        let started = tokio::time::Instant::now();
        loop {
            if let Ok(reply) = self.request("status", json!({}), REQUEST_DEADLINE).await
                && let Ok(status) = self.parse_status(&reply)
            {
                return Ok(status);
            }
            if self.child_gone() {
                return Err(unavailable("Screen sharing stopped as it started; see its log on this computer."));
            }
            if started.elapsed() > START_DEADLINE {
                self.kill_child().await;
                return Err(unavailable("Screen sharing did not start in time; see its log on this computer."));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn kill_child(&self) {
        let child = self.child.borrow_mut().take();
        if let Some(mut child) = child {
            terminate(child.id());
            if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
    }
}

/// SIGTERM to a process we started or found on our own socket.
fn terminate(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) {
        // SAFETY: kill(2) touches no memory.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

impl StreamPort for LiveStream {
    fn available(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        self.program.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    fn start(&self) -> LocalFuture<'_, Result<StreamStatus>> {
        Box::pin(async move {
            if let Some(status) = self.status_now().await? {
                return Ok(status);
            }
            self.spawn_and_wait().await
        })
    }

    fn status(&self) -> LocalFuture<'_, Result<Option<StreamStatus>>> {
        Box::pin(self.status_now())
    }

    fn open(&self, generation: u64) -> LocalFuture<'_, Result<()>> {
        Box::pin(async move {
            self.request("set_generation", json!({"generation": generation}), REQUEST_DEADLINE).await?;
            Ok(())
        })
    }

    fn issue_ticket<'a>(&'a self, generation: u64, ticket: &'a str, client_cert_sha256: &'a str, expires_in_ms: u64) -> LocalFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = json!({"generation": generation, "ticket": ticket, "client_cert_sha256": client_cert_sha256, "expires_in_ms": expires_in_ms});
            self.request("issue_ticket", body, REQUEST_DEADLINE).await?;
            Ok(())
        })
    }

    fn revoke(&self) -> LocalFuture<'_, Result<Option<Settlement>>> {
        Box::pin(async move {
            if self.status_now().await?.is_none() {
                return Ok(None);
            }
            let reply = self.request("revoke", json!({}), REVOKE_DEADLINE).await?;
            let s = &reply["settlement"];
            Ok(Some(Settlement {
                fence_generation: s["fence_generation"].as_u64().unwrap_or(0),
                settled: s["settled"] == json!(true),
            }))
        })
    }

    fn stop(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(async move {
            if !self.child_gone() {
                self.kill_child().await;
                return Ok(());
            }
            // One left by an earlier ibarad: it answers on our socket with its pid.
            let Some(status) = self.status_now().await? else { return Ok(()) };
            terminate(Some(status.pid));
            let since = tokio::time::Instant::now();
            while self.status_now().await.ok().flatten().is_some() {
                if since.elapsed() > STOP_GRACE {
                    if let Ok(pid) = i32::try_from(status.pid) {
                        // SAFETY: kill(2) touches no memory.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
    }
}
