//! Installing ibara from its package, and keeping it current.
//!
//! The `ibara` package (packaging/PKGBUILD) puts the programs, units and the
//! console plugin on disk. These commands do the rest, idempotently:
//!
//! - `ibara setup` (the person, once; asks for their password once): their
//!   ibara key, the root foundation (`ibara system setup`), the two user
//!   services, the console plugin through Omarchy's plugin mechanism, one
//!   shell restart, and a Tailscale check that says how to sign in.
//! - `ibara uninstall [--delete-data]`: everything setup made, then the package.
//! - `ibara update [--check]` and `ibara rollback`: the signed release channel
//!   (`update.rs`).
//! - `ibara unattended-boot enable|disable|status|lock-at-sign-in on|off`:
//!   starting this computer without typing its disk password, off unless
//!   turned on, and the screen lock at sign-in it needs when Omarchy signs in
//!   automatically (`unattended_boot.rs`).
//! - `ibara system …` (root only): the foundation (`system.rs`), reached
//!   through `sudo` from the commands above and from the pacman hook.
//!
//! Layout: the package tree is [`LIB`]; [`INSTALL_ROOT`]`/current` points at it,
//! so every path earlier releases wrote (sshd forced commands, the operator
//! shell, browser native hosts) keeps working and a computer that ran a
//! release tree keeps its `previous` release for going back.

pub mod omarchy_update;
pub mod system;
pub mod unattended_boot;
pub mod update;
pub mod user;

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// The package tree: `bin/`, `ops/`, `cua-hyprland-plugin/`, `chrome-extension/`.
pub const LIB: &str = "/usr/lib/ibara";
/// The console plugin as the package ships it; setup links Omarchy's plugin folder here.
pub const PLUGIN: &str = "/usr/share/ibara/omarchy-plugin";
pub const PLUGIN_ID: &str = "io.zet.ibara";
/// Root-owned per-computer state: `current` (→ [`LIB`]), `cua/`, `browser/`, `procedures-approved/`.
pub const INSTALL_ROOT: &str = "/opt/agent-computer";
pub const CONFIG_DIR: &str = "/etc/agent-computer";
pub const OPERATOR_PUBLIC: &str = "/etc/ibara-operator";
/// Which desktop account this computer's ibara belongs to.
pub const STATION: &str = "/etc/ibara/station.json";
/// The login shell of every `ibara-op-*` account (access_system.rs checks this path).
pub const OPERATOR_SHELL: &str = "/usr/local/sbin/ibara-op-shell";
/// Downloaded packages, kept for `ibara rollback`.
pub const PACKAGE_CACHE: &str = "/var/cache/ibara/packages";
/// Journal copies made before each update.
pub const BACKUPS: &str = "/var/backups/ibara";
/// The two user services: this computer for others, and the console.
pub const USER_UNITS: [&str; 2] = ["agent-computer.service", "ibara-operator.service"];
/// The account agents on paired computers reach this one through.
pub const AGENT_ACCOUNT: &str = "ibara-agent";
pub const RUNTIME_GROUP: &str = "ibara-runtime";

/// ibara's private files in the desktop person's home folder, relative to it:
/// the data and state of this computer and of the console, the settings, and
/// ibara's SSH key and host-key pins. `ibara uninstall --delete-data` deletes
/// these.
pub const HOME_DATA: [&str; 7] = [
    ".local/state/agent-computer",
    ".local/share/agent-computer",
    ".local/state/ibara",
    ".config/ibara",
    ".ssh/ibara_agent_ed25519",
    ".ssh/ibara_agent_ed25519.pub",
    ".ssh/known_hosts_ibara",
];

/// What else in the home folder decides how ibara runs, relative to it: the
/// SSH folder (ibara's key and pins, and the configuration its ssh reads),
/// update scratch space, older copies of the programs that would come first on
/// PATH, the user services' units and environment, the page reader's native
/// host manifests, the browsers' flags files and the lock at sign-in.
const HOME_TRUSTED: [&str; 11] = [
    ".ssh",
    ".cache/ibara",
    ".local/bin/ibara",
    ".local/bin/ibarad",
    ".config/systemd",
    ".config/environment.d",
    ".config/chromium/NativeMessagingHosts",
    ".config/google-chrome/NativeMessagingHosts",
    ".config/chromium-flags.conf",
    ".config/chrome-flags.conf",
    unattended_boot::SIGN_IN_LOCK,
];

/// Every path in `home` that agents may not name in `computer_files`, the
/// `computer_exec` cwd or a check: [`HOME_DATA`], what ibara trusts there, the
/// console plugin's link, and the same places where `XDG_CONFIG_HOME`,
/// `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` or ibara's own
/// variables move them. Anything new ibara keeps in the home folder belongs
/// in one of the lists above.
pub fn home_paths(home: &Path) -> Vec<PathBuf> {
    let env = |key: &str| std::env::var_os(key).map(PathBuf::from).filter(|p| p.is_absolute());
    let moved = [(".config/", env("XDG_CONFIG_HOME")), (".local/state/", env("XDG_STATE_HOME")), (".cache/", env("XDG_CACHE_HOME"))];
    let mut paths = Vec::new();
    for rel in HOME_DATA.iter().chain(HOME_TRUSTED.iter()) {
        paths.push(home.join(rel));
        for (prefix, dir) in &moved {
            if let (Some(rest), Some(dir)) = (rel.strip_prefix(prefix), dir) {
                paths.push(dir.join(rest));
            }
        }
    }
    paths.push(home.join(".config/omarchy/plugins").join(PLUGIN_ID));
    paths.extend(env("XDG_RUNTIME_DIR").map(|dir| dir.join("ibara")));
    for key in ["IBARA_STATE_DIR", "IBARA_DATA_DIR", "IBARA_RUNTIME_DIR", "IBARA_STATION_DESCRIPTOR"] {
        paths.extend(env(key));
    }
    paths
}

/// `ibara setup`.
pub fn setup_main(args: Vec<OsString>) -> ExitCode {
    finish(user::setup(&strings(args)))
}

/// `ibara uninstall [--delete-data] [--yes]`.
pub fn uninstall_main(args: Vec<OsString>) -> ExitCode {
    finish(user::uninstall(&strings(args)))
}

/// `ibara update [--check]`.
pub fn update_main(args: Vec<OsString>) -> ExitCode {
    let args = strings(args);
    if args.iter().any(|a| a == "--check") {
        return match update::check(&args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => { eprintln!("{e}"); ExitCode::from(2) }
        };
    }
    finish(update::update(&args))
}

/// `ibara rollback`.
pub fn rollback_main(args: Vec<OsString>) -> ExitCode {
    finish(update::rollback(&strings(args)))
}

/// `ibara unattended-boot enable|disable|status|lock-at-sign-in on|off`: starting without the disk password.
pub fn unattended_boot_main(args: Vec<OsString>) -> ExitCode {
    finish(unattended_boot::main(&strings(args)))
}

/// `ibara system setup|refresh|uninstall|update|update-latest|omarchy-update|omarchy-update-end|rollback|unattended-boot …` (root).
pub fn system_main(args: Vec<OsString>) -> ExitCode {
    finish(system::main(&strings(args)))
}

fn strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn finish(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

/// A desktop account from the passwd database.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub group: String,
}

impl Account {
    /// A person's account (uid 1000 or more), never root or a system account.
    pub fn desktop(name: &str) -> Result<Account, String> {
        let valid = !name.is_empty()
            && name.len() <= 32
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !valid {
            return Err(format!("{name} is not an account name."));
        }
        let line = getent("passwd", name).ok_or_else(|| format!("There is no account named {name} on this computer."))?;
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(uid), Some(gid), Some(home)) =
            (fields.get(2).and_then(|u| u.parse::<u32>().ok()), fields.get(3).and_then(|g| g.parse::<u32>().ok()), fields.get(5))
        else {
            return Err(format!("The account {name} has an unreadable passwd entry."));
        };
        if uid < 1000 || uid == 65534 {
            return Err(format!("{name} is a system account; run ibara setup as the person who uses this computer."));
        }
        let group = getent("group", &gid.to_string())
            .and_then(|g| g.split(':').next().map(str::to_string))
            .ok_or_else(|| format!("The primary group of {name} is missing."))?;
        Ok(Account { name: name.to_string(), uid, gid, home: PathBuf::from(home), group })
    }

    /// The account running this process.
    pub fn current() -> Result<Account, String> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let line = getent("passwd", &uid.to_string()).ok_or("This account is not in the passwd database.")?;
        Account::desktop(line.split(':').next().unwrap_or(""))
    }

    /// `systemctl --user …` in this account's own manager, from root.
    pub fn userctl(&self, args: &[&str]) -> Command {
        let mut command = Command::new("runuser");
        command
            .args(["-u", &self.name, "--", "env"])
            .arg(format!("XDG_RUNTIME_DIR=/run/user/{}", self.uid))
            .arg(format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/bus", self.uid))
            .args(["systemctl", "--user"])
            .args(args);
        command
    }
}

/// One `getent DATABASE KEY` line.
pub fn getent(database: &str, key: &str) -> Option<String> {
    let out = Command::new("getent").args([database, key]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string()).filter(|l| !l.is_empty())
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Run a program to completion; its standard output, or why it failed.
pub fn run(program: &str, args: &[&str]) -> Result<String, String> {
    output(Command::new(program).args(args))
}

pub fn output(command: &mut Command) -> Result<String, String> {
    let label = label(command);
    let out = command.stdin(Stdio::null()).output().map_err(|e| format!("{label} could not start ({e})."))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it gave no reason").trim();
        return Err(format!("{label} failed: {why}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a program with this terminal (sudo's password prompt, pacman's progress).
pub fn interactive(command: &mut Command) -> Result<(), String> {
    let label = label(command);
    let status = command.status().map_err(|e| format!("{label} could not start ({e})."))?;
    if status.success() { Ok(()) } else { Err(format!("{label} did not finish.")) }
}

fn label(command: &Command) -> String {
    let name = |s: &std::ffi::OsStr| Path::new(s).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let program = name(command.get_program());
    if program == "runuser" || program == "sudo" {
        // The program it runs: the first argument after `--`, past `env` and its settings.
        let mut inner = command.get_args().skip_while(|a| *a != "--").skip(1).filter(|a| *a != "env" && !a.to_string_lossy().contains('='));
        if let Some(inner) = inner.next() {
            return name(inner);
        }
    }
    program
}

/// A program on `PATH` (or in Omarchy's own `bin`).
pub fn program(name: &str) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file()));
    on_path.or_else(|| Some(crate::theme::omarchy_path().join("bin").join(name)).filter(|p| p.is_file()))
}

/// Replace a root-owned file atomically. An existing path must be a plain
/// root-owned file with one link, so root never writes through a link or a
/// file another account placed there.
pub fn write_root(path: &Path, body: &[u8], mode: u32) -> Result<(), String> {
    let shown = path.display();
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (!meta.is_file() || meta.uid() != 0 || meta.nlink() != 1)
    {
        return Err(format!("{shown} is not a file root wrote; move it away and run setup again."));
    }
    let parent = path.parent().ok_or_else(|| format!("{shown} has no folder."))?;
    trusted_dir(parent)?;
    let temp = parent.join(format!(".{}.ibara-{}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temp)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(body)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("Could not write {shown}: {e}")
    })
}

/// Write only when the file is missing (keys, identities, first policies).
pub fn create_root(path: &Path, body: &[u8], mode: u32) -> Result<bool, String> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(false);
    }
    write_root(path, body, mode).map(|()| true)
}

/// A root-owned directory nobody else can write, created when missing.
pub fn root_dir(path: &Path, mode: u32) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => trusted_dir(path),
        Err(_) => {
            if let Some(parent) = path.parent() {
                root_dir(parent, 0o755)?;
            }
            std::fs::DirBuilder::new().mode(mode).create(path).map_err(|e| format!("Could not create {}: {e}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| format!("{}: {e}", path.display()))
        }
    }
}

fn trusted_dir(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() || meta.uid() != 0 || (meta.mode() & 0o022 != 0) {
        return Err(format!("{} is not a folder only root can change; setup stopped rather than write there.", path.display()));
    }
    Ok(())
}

/// Point `link` at `target` atomically; true when it changed.
pub fn relink(target: &Path, link: &Path) -> Result<bool, String> {
    if std::fs::read_link(link).ok().as_deref() == Some(target) {
        return Ok(false);
    }
    let next = link.with_file_name(format!(".{}.ibara-{}", link.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), std::process::id()));
    let _ = std::fs::remove_file(&next);
    std::os::unix::fs::symlink(target, &next).map_err(|e| format!("Could not link {}: {e}", link.display()))?;
    std::fs::rename(&next, link).map_err(|e| {
        let _ = std::fs::remove_file(&next);
        format!("Could not link {}: {e}", link.display())
    })?;
    Ok(true)
}

/// 32 random bytes as lowercase hex.
pub fn random_hex() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("No randomness: {e}"))?;
    Ok(crate::store::canonical::hex_encode(&bytes))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    crate::store::canonical::hex_encode(&sha2::Sha256::digest(bytes))
}

/// The installed package version (`0.2.0-1`), if the package is installed.
pub fn installed_version() -> Option<String> {
    run("pacman", &["-Q", "ibara"]).ok()?.split_whitespace().nth(1).map(str::to_string)
}

/// pacman's own version order (`vercmp`): negative, zero or positive.
pub fn vercmp(a: &str, b: &str) -> Result<i32, String> {
    run("vercmp", &[a, b])?.trim().parse().map_err(|_| "vercmp gave no answer.".to_string())
}
