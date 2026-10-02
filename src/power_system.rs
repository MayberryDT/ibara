//! `ibara power-system`: the root helper that lets the desktop user's `ibarad`
//! restart, shut down or sleep this computer, update ibara or Omarchy on it,
//! set up or remove ibara's extension in the desktop account's browser, and
//! report whether it will come back without a person typing a disk
//! passphrase.
//!
//! systemd runs it per connection (`ibara-power.socket`, `Accept=yes`, a socket
//! only the desktop user can open): stdin and stdout are that connection. It
//! reads one JSON line, writes one JSON line and exits. Power commands run only
//! after the reply is flushed, because the computer may be gone before they
//! return. An update starts first, as a unit of its own (`ibara-update.service`
//! running `ibara system update-latest`, or `ibara-omarchy-update.service`
//! running `ibara system omarchy-update`), because it outlives this helper and
//! an ibara update restarts its socket; the reply says whether it started. One
//! update of either kind at a time. Requests name an operation and at most a
//! network adapter or a browser; programs and paths come from the unit's
//! environment, never from the request, and nothing runs through a shell.
//!
//! `browser_setup {browser, profile: "Default"}` and `browser_remove {browser}`
//! run `ibara browser-setup` (`IBARA_POWER_IBARA`, with the extension from
//! `IBARA_POWER_EXTENSION_DIR`) for the account on the other end of the socket
//! (`SO_PEERCRED`, named through `/etc/passwd`), and relay its JSON report.
use crate::wake::{ETHTOOL_GWOL, ETHTOOL_SWOL, WAKE_MAGIC, WolInfo, ethtool_wol};
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_REQUEST: usize = 64 * 1024;
/// Block devices stacked under `/` (LVM on LUKS and the like) never go this deep.
const MAX_STACK: usize = 8;

/// Where programs and kernel files are found. Only the unit sets these, so
/// tests can use fake programs and fixture trees.
struct Roots {
    bin: PathBuf,
    sys: PathBuf,
    proc: PathBuf,
    /// The `ibara` program that sets up the browser.
    ibara: PathBuf,
    /// The extension it installs.
    extension: PathBuf,
}

impl Roots {
    fn from_env() -> Self {
        let dir = |name: &str, default: &str| {
            PathBuf::from(std::env::var_os(name).filter(|v| !v.is_empty()).unwrap_or_else(|| default.into()))
        };
        let lib = Path::new(crate::install::LIB);
        Roots {
            bin: dir("IBARA_POWER_BIN_DIR", "/usr/bin"),
            sys: dir("IBARA_POWER_SYSFS", "/sys"),
            proc: dir("IBARA_POWER_PROC", "/proc"),
            ibara: dir("IBARA_POWER_IBARA", &lib.join("bin/ibara").to_string_lossy()),
            extension: dir("IBARA_POWER_EXTENSION_DIR", &lib.join("chrome-extension").to_string_lossy()),
        }
    }

    fn program(&self, name: &str) -> Command {
        let mut command = Command::new(self.bin.join(name));
        command.env("LC_ALL", "C").stdin(Stdio::null());
        command
    }
}

enum Op {
    Disk,
    Restart,
    Shutdown,
    Sleep(Option<String>),
    Update(&'static Job),
    BrowserSetup(&'static str),
    BrowserRemove(&'static str),
}

/// Browsers `ibara browser-setup` can set up for sharing logins.
const BROWSERS: [&str; 4] = ["chromium", "chrome", "brave", "brave-origin"];

/// An update that runs as its own unit.
struct Job {
    action: &'static str,
    /// How the reply names what is updating.
    name: &'static str,
    unit: &'static str,
    command: &'static str,
    description: &'static str,
    /// Units that run after the command ends or is stopped (`ExecStopPost=`).
    stop_post: Option<&'static str>,
    started: &'static str,
}

const IBARA_UPDATE: Job = Job {
    action: "update_ibara",
    name: "ibara",
    unit: crate::install::update::UPDATE_UNIT,
    command: "update-latest",
    description: "ibara update, asked for from another computer",
    stop_post: Some("update-end"),
    started: "ibara is updating to the latest release. It may restart its bar when it finishes.",
};

const OMARCHY_UPDATE: Job = Job {
    action: "update_omarchy",
    name: "Omarchy",
    unit: crate::install::omarchy_update::UNIT,
    command: "omarchy-update",
    description: "Omarchy update, asked for from another computer",
    stop_post: Some("omarchy-update-end"),
    started: "Omarchy is updating. The computer stays usable; it says when a restart is needed.",
};

const JOBS: [&Job; 2] = [&IBARA_UPDATE, &OMARCHY_UPDATE];

enum Wake {
    Enabled,
    NotSupported,
    Failed(String),
}

fn refusal(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": {"code": "INVALID_ARGUMENT", "message": message.into()}})
}

fn read_request() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST as u64 + 1)
        .read_line(&mut line)
        .map_err(|_| "The request could not be read as text.".to_string())?;
    if line.len() > MAX_REQUEST {
        return Err("The request is too large.".into());
    }
    Ok(line)
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(format!("The request has an unexpected field \"{key}\".")),
        None => Ok(()),
    }
}

/// The request's `browser`, one of [`BROWSERS`].
fn browser(object: &Map<String, Value>) -> Result<&'static str, String> {
    let name = object.get("browser").and_then(Value::as_str).ok_or("The request needs a \"browser\".")?;
    BROWSERS.into_iter().find(|b| *b == name).ok_or_else(|| format!("\"{name}\" is not a browser ibara can set up. Use chromium, chrome, brave or brave-origin."))
}

fn parse(line: &str) -> Result<Op, String> {
    let value: Value = serde_json::from_str(line).map_err(|_| "The request is not valid JSON.".to_string())?;
    let object = value.as_object().ok_or("The request must be a JSON object.")?;
    let op = object.get("op").and_then(Value::as_str).ok_or("The request needs an \"op\".")?;
    match op {
        "disk" | "restart" | "shutdown" | "update_ibara" | "update_omarchy" => {
            only_keys(object, &["op"])?;
            Ok(match op {
                "disk" => Op::Disk,
                "restart" => Op::Restart,
                "update_ibara" => Op::Update(&IBARA_UPDATE),
                "update_omarchy" => Op::Update(&OMARCHY_UPDATE),
                _ => Op::Shutdown,
            })
        }
        "sleep" => {
            only_keys(object, &["op", "wake"])?;
            match object.get("wake") {
                None | Some(Value::Null) => Ok(Op::Sleep(None)),
                Some(Value::Object(wake)) => {
                    only_keys(wake, &["ifname"])?;
                    let ifname = wake.get("ifname").and_then(Value::as_str).ok_or("\"wake\" needs an \"ifname\".")?;
                    Ok(Op::Sleep(Some(ifname.to_string())))
                }
                Some(_) => Err("\"wake\" must be null or an object with an \"ifname\".".into()),
            }
        }
        "browser_setup" => {
            only_keys(object, &["op", "browser", "profile"])?;
            let browser = browser(object)?;
            match object.get("profile").and_then(Value::as_str) {
                Some("Default") => Ok(Op::BrowserSetup(browser)),
                None => Err("The request needs a \"profile\".".into()),
                Some(_) => Err("Only the browser's Default profile can share logins.".into()),
            }
        }
        "browser_remove" => {
            only_keys(object, &["op", "browser"])?;
            Ok(Op::BrowserRemove(browser(object)?))
        }
        other => Err(format!(
            "\"{other}\" is not something this computer can do. Use disk, restart, shutdown, sleep, update_ibara, update_omarchy, browser_setup or browser_remove."
        )),
    }
}

/// The reply, and the `systemctl` verb to run once it is delivered.
fn handle(roots: &Roots, op: Op) -> (Value, Option<&'static str>) {
    match op {
        Op::Disk => match disk(roots) {
            Ok((encrypted, tpm_unlock)) => (json!({"ok": true, "encrypted": encrypted, "tpm_unlock": tpm_unlock}), None),
            Err(message) => (json!({"ok": false, "error": {"code": "INTERNAL_ERROR", "message": message}}), None),
        },
        Op::Restart => (json!({"ok": true, "action": "restart", "state": "started"}), Some("reboot")),
        Op::Shutdown => (json!({"ok": true, "action": "shutdown", "state": "started"}), Some("poweroff")),
        Op::Sleep(wake) => {
            let wake = match wake {
                None => None,
                Some(ifname) => match adapter(roots, &ifname) {
                    Err(message) => return (refusal(message), None),
                    Ok(Some(phy)) => Some(wifi_wake(roots, &phy)),
                    Ok(None) => Some(ethernet_wake(&ifname)),
                },
            };
            let mut reply = json!({"ok": true, "action": "sleep", "state": "started"});
            reply["wake"] = match wake {
                None => "off".into(),
                Some(Wake::Enabled) => "enabled".into(),
                Some(Wake::NotSupported) => "not_supported".into(),
                Some(Wake::Failed(message)) => {
                    reply["message"] = message.into();
                    "failed".into()
                }
            };
            (reply, Some("suspend"))
        }
        Op::Update(job) => (update(roots, job), None),
        Op::BrowserSetup(browser) => (browser_setup(roots, "browser_setup", browser), None),
        Op::BrowserRemove(browser) => (browser_setup(roots, "browser_remove", browser), None),
    }
}

/// Whether a unit is running.
fn active(roots: &Roots, unit: &str) -> bool {
    let Ok(out) = roots.program("systemctl").args(["show", "--property=ActiveState", "--value", unit]).output() else { return false };
    out.status.success() && matches!(String::from_utf8_lossy(&out.stdout).trim(), "active" | "activating" | "deactivating" | "reloading")
}

/// The reply when an update of either kind is already running, else `None`.
fn running(roots: &Roots, job: &Job) -> Option<Value> {
    let busy = JOBS.into_iter().find(|other| active(roots, other.unit))?;
    let message = if busy.unit == job.unit {
        format!("{} is already updating on this computer.", job.name)
    } else {
        format!("{} is updating on this computer. Update {} once it has finished.", busy.name, job.name)
    };
    Some(json!({"ok": true, "action": job.action, "state": if busy.unit == job.unit { "running" } else { "busy" }, "message": message}))
}

/// Start an update apart from this helper, which it outlives; one update at a time.
fn update(roots: &Roots, job: &Job) -> Value {
    if let Some(reply) = running(roots, job) {
        return reply;
    }
    // Test roots exercise dispatch without touching a live installation.
    if job.action == "update_ibara" && roots.bin == Path::new("/usr/bin") {
        match crate::install::update::preflight() {
            Ok(Some(reply)) => return reply,
            Ok(None) => {}
            Err(message) => return json!({"ok": false, "error": {"code": "INTERNAL_ERROR", "message": message}}),
        }
    }
    let ibara = Path::new(crate::install::LIB).join("bin/ibara");
    let mut command = roots.program("systemd-run");
    command.args(["--quiet", "--collect", "--property=Type=exec", "--unit", job.unit]);
    if job.action == "update_ibara" && let Ok(url) = std::env::var("IBARA_UPDATE_BASE_URL") {
        command.arg(format!("--setenv=IBARA_UPDATE_BASE_URL={url}"));
    }
    if let Some(end) = job.stop_post {
        command.arg(format!("--property=ExecStopPost=+{} system {end}", ibara.display()));
    }
    command.args(["--description", job.description, "--"]).arg(&ibara).args(["system", job.command]).stdout(Stdio::null());
    let started = command.output();
    if matches!(&started, Ok(out) if out.status.success()) {
        return json!({"ok": true, "action": job.action, "state": "started", "message": job.started});
    }
    // Another request started one a moment ago.
    if let Some(reply) = running(roots, job) {
        return reply;
    }
    let detail = match started {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given").to_string()
        }
        Err(_) => "systemd-run could not run.".into(),
    };
    json!({"ok": false, "error": {"code": "INTERNAL_ERROR", "message": format!("The update did not start: {detail}")}})
}

/// The desktop account on the other end of the socket on stdin.
fn peer_account() -> Result<String, String> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given.
    let got = unsafe { libc::getsockopt(0, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    if got != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err("This request did not come over the power socket, so its account is unknown.".into());
    }
    if cred.uid == 0 {
        return Err("The browser belongs to a desktop account, not root.".into());
    }
    let passwd = std::fs::read_to_string("/etc/passwd").map_err(|_| "The account list could not be read.".to_string())?;
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 7 && f[2].parse::<u32>().ok() == Some(cred.uid))
        .map(|f| f[0].to_string())
        .ok_or_else(|| format!("Account {} is not in /etc/passwd.", cred.uid))
}

/// Set up (`browser_setup`) or remove (`browser_remove`) ibara's extension in
/// the calling account's browser, relaying `ibara browser-setup`'s report.
fn browser_setup(roots: &Roots, action: &'static str, browser: &'static str) -> Value {
    let user = match peer_account() {
        Ok(user) => user,
        Err(message) => return refusal(message),
    };
    let mut command = Command::new(&roots.ibara);
    command.env("LC_ALL", "C").stdin(Stdio::null()).arg("browser-setup");
    if action == "browser_setup" {
        command.arg(&roots.extension).args([user.as_str(), "--browser", browser, "--operator"]);
    } else {
        command.args(["--remove", "--browser", browser, user.as_str()]);
    }
    let failed = |detail: &str| {
        let what = if action == "browser_setup" { "The browser was not set up" } else { "ibara was not removed from the browser" };
        json!({"ok": false, "error": {"code": "INTERNAL_ERROR", "message": format!("{what}: {detail}")}})
    };
    let out = match command.output() {
        Ok(out) => out,
        Err(_) => return failed("ibara could not run."),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given");
        return failed(detail.strip_prefix("ibara browser-setup: ").unwrap_or(detail));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report = stdout.lines().find(|l| !l.trim().is_empty()).and_then(|l| serde_json::from_str::<Value>(l).ok());
    let Some(Value::Object(mut reply)) = report else {
        return failed("ibara gave no report.");
    };
    reply.insert("ok".into(), true.into());
    reply.insert("action".into(), action.into());
    reply.insert("browser".into(), browser.into());
    Value::Object(reply)
}

pub fn main(args: Vec<OsString>) -> i32 {
    if !args.is_empty() {
        eprintln!("Usage: ibara power-system (started by ibara-power.socket, one request on stdin)");
        return 64;
    }
    let roots = Roots::from_env();
    // A run that crashed must not leave the desktop account's sudo open.
    crate::install::omarchy_update::clear_leftover_sudoers();
    let (reply, verb) = match read_request().and_then(|line| parse(&line)) {
        Ok(op) => handle(&roots, op),
        Err(message) => (refusal(message), None),
    };
    let mut out = std::io::stdout().lock();
    if let Err(e) = writeln!(out, "{reply}").and_then(|()| out.flush()) {
        // Nobody heard the answer, so nothing is started on its behalf.
        eprintln!("ibara power: the reply could not be sent ({e}); nothing was started");
        return 1;
    }
    drop(out);
    // Let the caller see the end of the answer now; the socket may outlive us.
    unsafe { libc::shutdown(1, libc::SHUT_WR) };
    let Some(verb) = verb else { return 0 };
    match roots.program("systemctl").arg(verb).stdout(Stdio::null()).status() {
        Ok(status) if status.success() => 0,
        Ok(status) => {
            eprintln!("ibara power: systemctl {verb} failed ({status})");
            1
        }
        Err(e) => {
            eprintln!("ibara power: systemctl {verb} could not run ({e})");
            1
        }
    }
}

/// A kernel name used as one path component under sysfs or /dev.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().all(|c| c == '.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Checks the adapter that should wake the computer. `Some(phy)` for Wi-Fi
/// (its own wireless device), `None` for a wired adapter.
fn adapter(roots: &Roots, ifname: &str) -> Result<Option<String>, String> {
    let valid = (1..=15).contains(&ifname.len())
        && ifname.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        && !ifname.chars().all(|c| c == '.');
    if !valid {
        return Err("The network adapter name is not valid.".into());
    }
    if ifname == "lo" {
        return Err("The loopback adapter cannot wake this computer.".into());
    }
    let dir = roots.sys.join("class/net").join(ifname);
    if !dir.exists() {
        return Err(format!("This computer has no network adapter named {ifname}."));
    }
    if !dir.join("device").exists() {
        return Err(format!("{ifname} is not a physical network adapter, so it cannot wake this computer."));
    }
    match read_trimmed(&dir.join("phy80211/name")) {
        Some(phy) if plain_name(&phy) => Ok(Some(phy)),
        Some(_) => Err(format!("{ifname} reports a Wi-Fi device ibara does not recognize.")),
        None => Ok(None),
    }
}

fn wifi_wake(roots: &Roots, phy: &str) -> Wake {
    let output = roots.program("iw").args(["phy", phy, "wowlan", "enable", "magic-packet"]).output();
    match output {
        Ok(out) if out.status.success() => Wake::Enabled,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.to_ascii_lowercase().contains("not supported") {
                Wake::NotSupported
            } else {
                let detail = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given");
                Wake::Failed(format!("Turning on wake over Wi-Fi failed: {detail}"))
            }
        }
        Err(_) => Wake::Failed("Turning on wake over Wi-Fi failed: the iw program could not run.".into()),
    }
}

fn ethernet_wake(ifname: &str) -> Wake {
    let info = WolInfo { cmd: ETHTOOL_GWOL, ..WolInfo::default() };
    let mut wol = match ethtool_wol(ifname, info) {
        Ok(wol) => wol,
        Err(e) => return Wake::Failed(format!("ibara could not read the wake settings of {ifname}: {e}")),
    };
    if wol.supported & WAKE_MAGIC == 0 {
        return Wake::NotSupported;
    }
    if wol.wolopts & WAKE_MAGIC != 0 {
        return Wake::Enabled;
    }
    wol.cmd = ETHTOOL_SWOL;
    wol.wolopts |= WAKE_MAGIC;
    match ethtool_wol(ifname, wol) {
        Ok(_) => Wake::Enabled,
        Err(e) => Wake::Failed(format!("Turning on wake for {ifname} failed: {e}")),
    }
}

/// The sysfs block directory of the device mounted at `/`.
fn root_device(roots: &Roots) -> Option<PathBuf> {
    let mountinfo = std::fs::read_to_string(roots.proc.join("self/mountinfo")).ok()?;
    // `ID PARENT MAJ:MIN ROOT MOUNTPOINT … - FSTYPE SOURCE OPTIONS`; the last
    // mount at `/` is the one in use.
    let line = mountinfo.lines().rfind(|l| l.split(' ').nth(4) == Some("/"))?;
    let numbers = line.split(' ').nth(2)?;
    if !numbers.starts_with("0:") {
        let dir = roots.sys.join("dev/block").join(numbers);
        return dir.exists().then_some(dir);
    }
    // Filesystems such as btrfs report an anonymous device; use the source.
    let source = line.split(" - ").nth(1)?.split(' ').nth(1)?;
    if let Some(name) = source.strip_prefix("/dev/mapper/") {
        let blocks = std::fs::read_dir(roots.sys.join("block")).ok()?;
        return blocks
            .flatten()
            .map(|entry| entry.path())
            .find(|dir| read_trimmed(&dir.join("dm/name")).as_deref() == Some(name));
    }
    let resolved = std::fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
    let name = resolved.strip_prefix("/dev").ok()?.to_str()?;
    let dir = roots.sys.join("class/block").join(name);
    (plain_name(name) && dir.exists()).then_some(dir)
}

/// Whether `dir` is, or is stacked on, a dm-crypt device; collects the
/// devices each dm-crypt layer decrypts.
fn crypt_layers(dir: &Path, depth: usize, sealed: &mut Vec<String>) -> bool {
    if depth > MAX_STACK {
        return false;
    }
    let mut below: Vec<String> = std::fs::read_dir(dir.join("slaves"))
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    below.sort();
    let crypt = read_trimmed(&dir.join("dm/uuid")).is_some_and(|uuid| uuid.starts_with("CRYPT-"));
    if crypt {
        sealed.extend(below);
        return true;
    }
    let mut any = false;
    for name in below {
        any |= crypt_layers(&dir.join("slaves").join(name), depth + 1, sealed);
    }
    any
}

/// Whether `systemd-cryptenroll DEVICE` lists a tpm2 key slot.
fn has_tpm_slot(roots: &Roots, device: &str) -> bool {
    if !plain_name(device) {
        return false;
    }
    let Ok(out) = roots.program("systemd-cryptenroll").arg(format!("/dev/{device}")).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = text.lines().skip_while(|l| !l.split_whitespace().eq(["SLOT", "TYPE"]));
    rows.next();
    rows.any(|row| row.split_whitespace().nth(1) == Some("tpm2"))
}

/// Whether the disk this computer starts from is encrypted, read from sysfs
/// without root; `None` when that disk cannot be found.
pub(crate) fn root_encrypted() -> Option<bool> {
    let roots = Roots::from_env();
    Some(crypt_layers(&root_device(&roots)?, 0, &mut Vec::new()))
}

/// `(encrypted, tpm_unlock)` for the disk this computer starts from.
fn disk(roots: &Roots) -> Result<(bool, bool), String> {
    let root = root_device(roots).ok_or("ibara could not find the disk this computer starts from.")?;
    let mut sealed = Vec::new();
    if !crypt_layers(&root, 0, &mut sealed) {
        return Ok((false, false));
    }
    // The mkinitcpio `encrypt` hook always asks for the passphrase. Once
    // `ibara unattended-boot enable` has switched starting to systemd's, its
    // status says whether the boot files still do that at the next start.
    let ibara = crate::install::unattended_boot::status();
    let asks = if ibara.on() {
        ibara.next_start_asks != Some(false)
    } else {
        let cmdline = std::fs::read_to_string(roots.proc.join("cmdline")).unwrap_or_default();
        cmdline.split_whitespace().any(|arg| arg.starts_with("cryptdevice="))
    };
    let tpm = !asks && !sealed.is_empty() && sealed.iter().all(|device| has_tpm_slot(roots, device));
    Ok((true, tpm))
}
