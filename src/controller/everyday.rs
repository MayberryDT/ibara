//! Everyday operator operations: what a person does with a computer from
//! another one over the pairing route. Logs, health, power, this computer's
//! settings and theme, repairs, the timeline, sending a wake packet for a
//! sleeping neighbour, and the task, result and procedure reads and actions
//! that used to need the administrator route; and this computer's windows
//! (`windows.rs`).
//!
//! Reads (`health`, `timeline`, tasks, results, procedures, windows) need
//! watch; closing or moving a window needs control; everything else that
//! changes this computer needs administer. Anything needing
//! root (restart, shut down, sleep, turning on wake-up, the disk check) goes
//! through the root power helper (`ibara power-system`, socket
//! `/run/ibara-power/power.sock`, or `IBARA_POWER_SOCKET`).

use super::{Controller, log_event};
use crate::desktop::run::{Cmd, run};
use crate::error::{IbaraError, Result, invalid, unavailable};
use crate::settings::{self, Scope};
use crate::{theme, wake};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Everyday operations on the operator route.
pub const EVERYDAY_OPS: &[&str] = &[
    "logs", "health", "power", "settings", "theme_apply", "theme_upload", "repair", "timeline", "send_wake", "tasks", "task",
    "artifacts", "procedures", "procedure", "task_extend", "task_revoke", "procedure_review", "artifact_transfer", "windows",
    "window_close", "window_move",
];

/// Everyday operations that may take long: their own transport and the long relay deadline.
pub const SLOW_OPS: &[&str] = &["theme_apply", "repair"];

/// The access capability an everyday operation needs.
pub(crate) fn capability(op: &str) -> &'static str {
    match op {
        "health" | "timeline" | "tasks" | "task" | "artifacts" | "procedures" | "procedure" | "windows" => "watch",
        "window_close" | "window_move" => "control",
        _ => "administer",
    }
}

const POWER_SOCKET: &str = "/run/ibara-power/power.sock";
const POWER_DEADLINE: Duration = Duration::from_secs(5);
const WAKE_CACHE_MS: i64 = 60_000;
const DISK_CACHE_MS: i64 = 600_000;
const THEME_CHUNK: usize = 768 * 1024;
/// The end of the stream's log this read looks at.
const STREAM_LOG_TAIL: u64 = 512 * 1024;
const IBARA_UNIT: &str = "agent-computer.service";

fn text<'a>(action: &'a Value, key: &str) -> &'a str {
    action.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A number given as a JSON number or as decimal text.
pub(super) fn number(action: &Value, key: &str) -> Option<i64> {
    match action.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn power_socket() -> PathBuf {
    std::env::var_os("IBARA_POWER_SOCKET").map(PathBuf::from).unwrap_or_else(|| POWER_SOCKET.into())
}

/// Whether this computer's root power helper is installed (its socket exists).
pub(crate) fn power_socket_exists() -> bool {
    power_socket().exists()
}

/// One request to the root power helper: a JSON line out, a JSON line back.
async fn power_helper(request: Value) -> Result<Value> {
    power_request(request, POWER_DEADLINE).await
}

/// One request to the root power helper within `deadline`; its reply when
/// `ok`, else its message as `CAPABILITY_UNAVAILABLE`.
pub(crate) async fn power_request(request: Value, deadline: Duration) -> Result<Value> {
    let socket = power_socket();
    let missing = || {
        IbaraError::new(
            "CAPABILITY_UNAVAILABLE",
            "This computer's power helper is not installed or not running. Update ibara on it, then try again.",
            true,
        )
    };
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.map_err(|_| missing())?;
        let mut line = request.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).await.map_err(|_| missing())?;
        let mut reply = String::new();
        BufReader::new(stream.take(64 * 1024)).read_line(&mut reply).await.map_err(|_| missing())?;
        serde_json::from_str::<Value>(&reply).map_err(|_| missing())
    };
    let reply = tokio::time::timeout(deadline, exchange).await.map_err(|_| missing())??;
    if reply["ok"] != json!(true) {
        let message = reply["error"]["message"].as_str().unwrap_or("The power helper refused.");
        return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", message, true));
    }
    Ok(reply)
}

/// A program Omarchy ships, on `PATH` or in `$OMARCHY_PATH/bin`.
fn omarchy_program(name: &str) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file()));
    on_path.or_else(|| Some(theme::omarchy_path().join("bin").join(name)).filter(|p| p.is_file()))
}

/// Omarchy's scripts call each other by name and read `OMARCHY_PATH`.
fn omarchy_cmd(program: &Path) -> Cmd {
    let omarchy = theme::omarchy_path();
    let mut path = std::ffi::OsString::from(omarchy.join("bin"));
    if let Some(current) = std::env::var_os("PATH") {
        path.push(":");
        path.push(current);
    }
    Cmd::new(program).env("OMARCHY_PATH", omarchy).env("PATH", path)
}

fn meminfo_mb(meminfo: &str, key: &str) -> Option<f64> {
    meminfo.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().trim_end_matches(" kB").trim().parse::<f64>().ok()).flatten().map(|kb| kb / 1024.0)
    })
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

impl Controller {
    /// One everyday operation for `operator_id`, after its access gate.
    pub(crate) async fn everyday(&self, operator_id: &str, op: &str, action: &Value) -> Result<Value> {
        let reference = |key: &str| {
            let value = text(action, key);
            if super::is_ref(value) { Ok(value.to_string()) } else { Err(invalid(format!("Expected {key}."))) }
        };
        match op {
            "logs" => self.op_logs(action).await,
            "health" => Ok(self.op_health().await),
            "power" => self.op_power(action).await,
            "settings" => self.op_settings(action).await,
            "theme_apply" => self.op_theme_apply(action).await,
            "theme_upload" => self.op_theme_upload(action),
            "repair" => self.repair_now(text(action, "fix")).await,
            "timeline" => self.op_timeline(action),
            "send_wake" => op_send_wake(action).await,
            "tasks" => self.admin(json!({"op": "tasks"})).await,
            "task" => self.admin(json!({"op": "task", "task_ref": reference("task_ref")?})).await,
            "artifacts" => {
                let limit = number(action, "limit").unwrap_or(20).clamp(1, 100);
                let cursor = action.get("cursor").and_then(Value::as_str).filter(|c| super::is_ref(c));
                self.admin(json!({"op": "artifacts", "limit": limit, "cursor": cursor})).await
            }
            "procedures" => self.admin(json!({"op": "procedures"})).await,
            "procedure" => self.admin(json!({"op": "procedure", "procedure_ref": reference("procedure_ref")?})).await,
            "task_extend" => {
                let seconds = number(action, "extra_seconds").filter(|s| (1..=86_400).contains(s));
                let seconds = seconds.ok_or_else(|| invalid("An extension is 1 to 86400 seconds."))?;
                self.admin(json!({"op": "extend", "task_ref": reference("task_ref")?, "extra_seconds": seconds})).await
            }
            "task_revoke" => self.admin(json!({"op": "revoke", "task_ref": reference("task_ref")?})).await,
            "procedure_review" => {
                let procedure = reference("procedure_ref")?;
                match text(action, "decision") {
                    "approve" => {
                        let digest = text(action, "expected_sha256");
                        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(invalid("Approval needs the reviewed SHA-256 digest."));
                        }
                        self.admin(json!({"op": "approve_procedure", "procedure_ref": procedure, "expected_sha256": digest.to_lowercase()})).await
                    }
                    "quarantine" => self.admin(json!({"op": "quarantine_procedure", "procedure_ref": procedure})).await,
                    _ => Err(invalid("Choose approve or quarantine.")),
                }
            }
            "artifact_transfer" => {
                let request = action.get("request").cloned().unwrap_or(Value::Null);
                let kind = text(&request, "kind");
                if !matches!(kind, "stat_artifact" | "download_artifact" | "ack_collected" | "end_transfer") {
                    return Err(invalid("Only collecting a result is allowed here."));
                }
                self.storage.transfer("operator", &request, Some(&format!("op_{operator_id}")))
            }
            "windows" => self.op_windows().await,
            "window_close" => self.op_window_close(operator_id, action).await,
            "window_move" => self.op_window_move(operator_id, action).await,
            _ => Err(invalid("Unknown operator operation.")),
        }
    }

    /// This computer's name: the one in its settings, else its usual name.
    pub(crate) fn computer_name(&self) -> String {
        settings::current().text("name").unwrap_or_else(|| self.computer.name.clone())
    }

    fn settings_defaults(&self) -> settings::Defaults {
        settings::Defaults { name: self.computer.name.clone() }
    }

    async fn op_logs(&self, action: &Value) -> Result<Value> {
        let which = match text(action, "which") {
            "" | "ibara" => "ibara",
            "viewer" => "viewer",
            _ => return Err(invalid("Choose ibara or viewer logs.")),
        };
        let lines = number(action, "lines").unwrap_or(80).clamp(1, 500);
        if which == "viewer" {
            return Ok(json!({"which": which, "lines": self.stream_log(lines as usize)?}));
        }
        let unit = IBARA_UNIT;
        let out = run(
            Cmd::new("journalctl")
                .args(["--user", "-u", unit, "-n", &lines.to_string(), "--no-pager", "--output=short-iso"])
                .timeout(Duration::from_secs(5))
                .max_output(512 * 1024),
        )
        .await?;
        if !out.success() {
            return Err(unavailable(format!("The logs could not be read: {}", super::clip(&out.failure_text("journalctl"), 300))));
        }
        let text = out.stdout_text();
        let all: Vec<&str> = text.lines().collect();
        let from = all.len().saturating_sub(lines as usize);
        Ok(json!({"which": which, "lines": all[from..]}))
    }

    /// The last `lines` lines of `<state>/stream/ibara-stream.log`; none
    /// before screen sharing first ran.
    fn stream_log(&self, lines: usize) -> Result<Vec<String>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.state_dir.join("stream").join("ibara-stream.log");
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(unavailable(format!("The logs could not be read: {e}"))),
        };
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(STREAM_LOG_TAIL)))?;
        let mut bytes = Vec::new();
        file.take(STREAM_LOG_TAIL).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut all: Vec<&str> = text.lines().collect();
        // A cut first line is not shown.
        if size > STREAM_LOG_TAIL && !all.is_empty() {
            all.remove(0);
        }
        let from = all.len().saturating_sub(lines);
        Ok(all[from..].iter().map(|l| l.to_string()).collect())
    }

    async fn op_health(&self) -> Value {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let total = meminfo_mb(&meminfo, "MemTotal");
        let available = meminfo_mb(&meminfo, "MemAvailable");
        let load = std::fs::read_to_string("/proc/loadavg").ok().and_then(|t| t.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()));
        let uptime = std::fs::read_to_string("/proc/uptime").ok().and_then(|t| t.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()));
        let disk = super::admin::read_disk("/");
        let gb = |bytes: Option<u64>| bytes.map(|b| round1(b as f64 / 1e9));
        let disk_total = disk["total_bytes"].as_u64();
        let disk_used = disk_total.zip(disk["available_bytes"].as_u64()).map(|(t, a)| t.saturating_sub(a));
        let control = self.journal.get_control().ok();
        json!({
            "name": settings::current().text("name"),
            "account": desktop_account(),
            "load": load,
            // The console says how busy the computer is from the load for each processor.
            "cpus": std::thread::available_parallelism().ok().map(|n| n.get()),
            "memory": {"used_mb": total.zip(available).map(|(t, a)| (t - a).round()), "total_mb": total.map(f64::round)},
            "disk": {"used_gb": gb(disk_used), "total_gb": gb(disk_total)},
            "uptime_s": uptime.map(|u| u.floor() as u64),
            "paused": control.as_ref().map(|c| c.paused),
            "pause_origin": control.as_ref().and_then(|c| c.pause_origin).map(|o| o.as_str()),
            "repair": self.repair_status(),
            "wake": self.wake_info().await,
            "disk_password": self.disk_password().await,
        })
    }

    /// How this computer can be woken (cached a minute), or null when it
    /// cannot be or its wake setting is off.
    pub(crate) async fn wake_info(&self) -> Value {
        if !settings::current().bool("wake_on_network") {
            return Value::Null;
        }
        let now = self.now_ms();
        if let Some((at, cached)) = self.wake_cache.borrow().as_ref()
            && now - at < WAKE_CACHE_MS
        {
            return cached.clone();
        }
        let info = wake::detect().await.unwrap_or(Value::Null);
        *self.wake_cache.borrow_mut() = Some((now, info.clone()));
        info
    }

    /// Whether a restart stops at the disk password (encrypted, not unlocked
    /// by the TPM), from the power helper; null when it cannot tell.
    pub(crate) async fn disk_password(&self) -> Value {
        let now = self.now_ms();
        if let Some((at, cached)) = self.disk_cache.borrow().as_ref()
            && now - at < DISK_CACHE_MS
        {
            return cached.clone();
        }
        let answer = match power_helper(json!({"op": "disk"})).await {
            Ok(reply) => json!(reply["encrypted"] == json!(true) && reply["tpm_unlock"] != json!(true)),
            Err(_) => Value::Null,
        };
        if !answer.is_null() {
            *self.disk_cache.borrow_mut() = Some((now, answer.clone()));
        }
        answer
    }

    async fn op_power(&self, action: &Value) -> Result<Value> {
        let action = text(action, "action");
        match action {
            "restart" | "shutdown" => {
                let warning = action == "restart" && self.disk_password().await == json!(true);
                power_helper(json!({"op": action})).await?;
                log_event("power", &format!("{action} at a person's request"));
                let mut out = json!({"action": action, "state": "started"});
                if action == "restart" {
                    out["disk_password_warning"] = json!(warning);
                }
                Ok(out)
            }
            "sleep" => {
                let wake = if settings::current().bool("wake_on_network") {
                    wake::detect().await.map(|w| json!({"ifname": w["ifname"]}))
                } else {
                    None
                };
                let reply = power_helper(json!({"op": "sleep", "wake": wake})).await?;
                log_event("power", "sleep at a person's request");
                Ok(json!({"action": "sleep", "state": "started", "wake": reply["wake"], "message": reply.get("message")}))
            }
            "lock" => {
                self.lock_screen().await?;
                Ok(json!({"action": "lock", "state": "started"}))
            }
            "update_ibara" | "update_omarchy" => {
                let reply = power_helper(json!({"op": action})).await?;
                if reply["state"] == "started" {
                    let what = if action == "update_ibara" { "ibara" } else { "Omarchy" };
                    log_event("power", &format!("{what} update at a person's request"));
                }
                Ok(json!({"action": action, "state": reply["state"], "message": reply["message"]}))
            }
            _ => Err(invalid("Choose restart, shutdown, sleep, lock, update_ibara or update_omarchy.")),
        }
    }

    /// Lock the screen the way Omarchy does, else through logind.
    async fn lock_screen(&self) -> Result<()> {
        if let Some(lock) = omarchy_program("omarchy-system-lock") {
            let out = run(omarchy_cmd(&lock).timeout(Duration::from_secs(10))).await?;
            if out.success() {
                return Ok(());
            }
            return Err(unavailable(format!("The screen did not lock: {}", out.failure_text("omarchy-system-lock"))));
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() }.to_string();
        let session = run(Cmd::new("loginctl").args(["show-user", &uid, "-p", "Display", "--value"])).await?;
        let session = session.stdout_text().trim().to_string();
        if session.is_empty() {
            return Err(unavailable("No desktop session is running on this computer to lock."));
        }
        let out = run(Cmd::new("loginctl").args(["lock-session", &session])).await?;
        if !out.success() {
            return Err(unavailable(format!("The screen did not lock: {}", out.failure_text("loginctl"))));
        }
        Ok(())
    }

    async fn op_settings(&self, action: &Value) -> Result<Value> {
        let args: Vec<&str> = action.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let result = settings::command(Scope::Computer, &args, &self.settings_defaults())?;
        if args.first() != Some(&"get") {
            self.apply_display_setting().await;
        }
        Ok(result)
    }

    /// A changed virtual screen size reaches an existing `IbaraVirtual` at
    /// once (by the console or a hand edit, which the watchdog notices).
    pub(crate) async fn apply_display_setting(&self) {
        let size = settings::current().text("virtual_display_size").unwrap_or_default();
        if self.virtual_size_applied.borrow().as_deref() == Some(size.as_str()) {
            return;
        }
        match self.desktop.resize_virtual().await {
            Ok(_) => *self.virtual_size_applied.borrow_mut() = Some(size),
            Err(e) => log_event("virtual_resize_failed", &e.to_string()),
        }
    }

    async fn op_theme_apply(&self, action: &Value) -> Result<Value> {
        let name = text(action, "name");
        if !theme::valid_name(name) {
            return Err(invalid("Expected a theme name."));
        }
        if theme::current_name().as_deref() == Some(name) {
            return Ok(json!({"theme": name, "state": "applied", "unchanged": true}));
        }
        if theme::source(name).is_none() {
            return Ok(json!({"theme": name, "state": "missing"}));
        }
        let Some(program) = omarchy_program("omarchy-theme-set") else {
            return Err(unavailable("Omarchy's theme tools are not on this computer."));
        };
        let out = run(omarchy_cmd(&program).arg(name).timeout(Duration::from_secs(120))).await?;
        if !out.success() {
            return Err(unavailable(format!("Omarchy could not apply the theme: {}", super::clip(&out.failure_text("omarchy-theme-set"), 300))));
        }
        Ok(json!({"theme": name, "state": "applied"}))
    }

    /// One piece of a theme bundle; the last piece installs the theme.
    fn op_theme_upload(&self, action: &Value) -> Result<Value> {
        let name = text(action, "name");
        let upload = text(action, "upload_id");
        let sha = text(action, "sha256");
        let (offset, total) = (number(action, "offset").unwrap_or(-1), number(action, "total").unwrap_or(-1));
        let upload_ok = (16..=64).contains(&upload.len()) && upload.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let sha_ok = sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !theme::valid_name(name) || !upload_ok || !sha_ok || offset < 0 || total <= 0 || total as u64 > theme::MAX_BUNDLE {
            return Err(invalid("Expected a theme name, upload, offset, total size and SHA-256."));
        }
        let data = base64::engine::general_purpose::STANDARD.decode(text(action, "data")).map_err(|_| invalid("Theme data is not base64."))?;
        if data.len() > THEME_CHUNK || offset as u64 + data.len() as u64 > total as u64 {
            return Err(invalid("Theme data does not fit the announced size."));
        }
        if theme::source(name).is_some() {
            return Ok(json!({"theme": name, "state": "installed"}));
        }
        let dir = self.state_dir.join("theme-uploads");
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        }
        // Uploads nobody finished within an hour are dropped.
        let now = std::time::SystemTime::now();
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > Duration::from_secs(3600));
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let part = dir.join(format!("{upload}.part"));
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if have != offset as u64 {
            return Err(invalid(format!("The theme upload is at byte {have}; send from there.")));
        }
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&part)?;
            file.write_all(&data)?;
        }
        let received = offset as u64 + data.len() as u64;
        if received < total as u64 {
            return Ok(json!({"theme": name, "state": "receiving", "received": received}));
        }
        let bundle = std::fs::read(&part)?;
        let _ = std::fs::remove_file(&part);
        let digest: String = Sha256::digest(&bundle).iter().map(|b| format!("{b:02x}")).collect();
        if digest != sha {
            return Err(invalid("The theme arrived damaged. Try again."));
        }
        let files = theme::unpack(&bundle, &theme::user_themes().join(name))?;
        log_event("theme_installed", &format!("{name} ({files} files) from another computer"));
        Ok(json!({"theme": name, "state": "installed", "files": files}))
    }

    fn op_timeline(&self, action: &Value) -> Result<Value> {
        let after = number(action, "after").unwrap_or(0).max(0);
        let limit = number(action, "limit").unwrap_or(50).clamp(1, 100);
        let events: Vec<Value> = self
            .journal
            .events_for_person(after, limit)?
            .into_iter()
            .map(|e| json!({"id": e.id, "at": e.at, "kind": e.kind, "actor": e.actor, "summary": e.summary}))
            .collect();
        Ok(json!({"events": events, "revision": self.journal.timeline_revision()?}))
    }
}

/// The desktop account ibarad runs as (a person opens a terminal as it).
fn desktop_account() -> Option<String> {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return Some(user);
    }
    // SAFETY: getpwuid returns a pointer into libc's static buffer or null; the
    // name is copied out at once and nothing else runs in between on this thread.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned())
    }
}

/// Send a magic packet onto one of this computer's own networks: the subnet,
/// and the network whose gateway has `gateway_mac` when one is given.
async fn op_send_wake(action: &Value) -> Result<Value> {
    let mac = wake::parse_mac(text(action, "mac")).ok_or_else(|| invalid("Expected the MAC address to wake."))?;
    let subnet = wake::parse_subnet(text(action, "subnet")).ok_or_else(|| invalid("Expected the network to wake it on."))?;
    let gateway_mac = Some(text(action, "gateway_mac")).filter(|m| !m.is_empty());
    let here = wake::here(text(action, "subnet"), gateway_mac).await;
    if here == wake::Here::Elsewhere {
        return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "This computer is not on that network.", true));
    }
    wake::send(mac, subnet)?;
    Ok(json!({"state": "sent", "sure": here == wake::Here::Same}))
}
