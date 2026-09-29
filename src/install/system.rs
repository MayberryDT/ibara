//! `ibara system …`: the root foundation, in place of the controller's Node
//! `ops/foundation.sh`. Only root runs it: `ibara setup` and `ibara uninstall`
//! through `sudo`, and the package's pacman hook (`refresh`) after ibara or
//! Hyprland is installed or upgraded.
//!
//! Every step is safe to repeat; a failed step names itself and nothing after
//! it runs. Setup writes only root-owned paths; files in the person's home are
//! written by their own account (`user.rs`, and `cua-plugin.sh` through
//! `runuser`). What earlier releases wrote and this keeps: `/etc/agent-computer`
//! (keys, policy, the agent entry's sshd), `/etc/ibara-operator`, the
//! `ibara-op-*` accounts the access projection makes, `/etc/ibara/station.json`.
//! Take Control's stream and viewer are packages of their own (ibara-stream, ibara-view).

use super::{
    AGENT_ACCOUNT, Account, BACKUPS, CONFIG_DIR, INSTALL_ROOT, LIB, OPERATOR_PUBLIC, OPERATOR_SHELL, RUNTIME_GROUP, STATION, create_root,
    getent, installed_version, is_root, output, random_hex, relink, root_dir, run, sha256_hex, write_root,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "Usage: ibara system setup USER | refresh | rebuild-cua | uninstall USER [--delete-data] | update USER (PACKAGE SHA256)… | rollback USER | unattended-boot enable [--dry-run] | disable | boot-check";

/// System units the package ships; setup enables them for the desktop user.
const SYSTEM_UNITS: [&str; 3] = ["ibara-agent-sshd.service", "ibara-access.socket", "ibara-power.socket"];
/// Unit files earlier releases wrote under /etc, which would hide the package's own.
const SHADOWING_UNITS: [&str; 5] =
    ["ibara-agent-sshd.service", "ibara-access.socket", "ibara-access@.service", "ibara-power.socket", "ibara-power@.service"];
/// Units of earlier releases that the package no longer has (Sunshine's viewer
/// helpers and the output watch, both retired). Uninstall removes them.
const LEGACY_UNITS: [&str; 4] =
    ["ibara-viewer-control.socket", "ibara-viewer-control@.service", "ibara-viewer-enroll.socket", "ibara-viewer-enroll@.service"];

/// The agent entry, the pairing listener and streaming, on the tailnet interface only.
pub const FIREWALL_RULES: [(&str, &str, &str); 4] = [
    ("2222", "tcp", "ibara agent entry"),
    ("24247", "tcp", "ibara pairing"),
    ("47984,47989,48010", "tcp", "ibara streaming"),
    ("47998,47999,48000,48002", "udp", "ibara streaming"),
];

pub fn main(args: &[String]) -> Result<(), String> {
    if !is_root() {
        return Err("This part of ibara runs as root. Run ibara setup instead; it asks for your password.".into());
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["setup", user] => setup(&Account::desktop(user)?),
        ["refresh"] => refresh(),
        ["rebuild-cua"] => rebuild_cua(),
        ["uninstall", user, rest @ ..] => {
            let delete_data = match rest {
                [] => false,
                ["--delete-data"] => true,
                _ => return Err(USAGE.into()),
            };
            uninstall(&Account::desktop(user)?, delete_data)
        }
        ["update", user, pairs @ ..] if !pairs.is_empty() => super::update::system_update(&Account::desktop(user)?, pairs),
        ["rollback", user] => super::update::system_rollback(&Account::desktop(user)?),
        ["unattended-boot", rest @ ..] => super::unattended_boot::system(&rest.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        _ => Err(USAGE.into()),
    }
}

/// Run one named step; a failure names it.
fn step(name: &str, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    println!("  {name}");
    f().map_err(|e| format!("Setup stopped at \"{name}\": {e}\nNothing after that step ran. Fix it, then run ibara setup again."))
}

/// The desktop account ibara on this computer belongs to, from the station file.
pub fn station_owner() -> Option<String> {
    let station: Value = serde_json::from_slice(&std::fs::read(STATION).ok()?).ok()?;
    station.get("agent_account").and_then(Value::as_str).filter(|a| !a.is_empty()).map(str::to_string)
}

pub fn setup(desktop: &Account) -> Result<(), String> {
    set_up(desktop, true)
}

/// Setup's steps. `by_person`: the person ran `ibara setup`, so Tailscale's
/// daemon is turned on; the pacman hook leaves it as the person left it.
fn set_up(desktop: &Account, by_person: bool) -> Result<(), String> {
    println!("Setting up this computer for {} (as root):", desktop.name);
    step("Checking this computer", || preconditions(desktop))?;
    step("Accounts", accounts)?;
    step("Folders", || folders(desktop))?;
    step("Keys and policy", || keys(desktop))?;
    step("Agent entry (SSH on port 2222)", || gateway(desktop))?;
    step("System services", || services(desktop))?;
    step("Firewall", firewall)?;
    if by_person {
        step("Tailscale", || tailscale(desktop))?;
    }
    step("Browser page reader", || browser(desktop))?;
    step("Cua's Hyprland plugin", || cua_plugin(desktop))?;
    Ok(())
}

/// After the ibara package was installed or upgraded (pacman hook): set up
/// again for the person who set this computer up; nothing when setup never ran.
fn refresh() -> Result<(), String> {
    let Some(owner) = station_owner() else {
        println!("ibara is installed. To finish, run as yourself: ibara setup");
        return Ok(());
    };
    set_up(&Account::desktop(&owner)?, false)
}

/// After Hyprland or GCC was installed or upgraded (pacman hook): only Cua's
/// Hyprland plugin is built again; services, and agents working through them,
/// are left alone.
fn rebuild_cua() -> Result<(), String> {
    let Some(owner) = station_owner() else { return Ok(()) };
    let desktop = Account::desktop(&owner)?;
    step("Cua's Hyprland plugin", || cua_plugin(&desktop))
}

fn preconditions(desktop: &Account) -> Result<(), String> {
    if !Path::new(LIB).join("bin/ibara").is_file() {
        return Err(format!("The ibara package is not installed ({LIB} is missing)."));
    }
    for (program, package) in [("/usr/bin/sshd", "openssh"), ("/usr/bin/setfacl", "acl"), ("/usr/bin/openssl", "openssl")] {
        if !Path::new(program).is_file() {
            return Err(format!("{program} is missing; install the {package} package."));
        }
    }
    match station_owner() {
        Some(owner) if owner != desktop.name => Err(format!(
            "ibara on this computer belongs to {owner}. Run ibara uninstall --delete-data as {owner} first, or run ibara setup as {owner}."
        )),
        _ => Ok(()),
    }
}

/// `ibara-runtime` and `ibara-agent` (the package's sysusers.d file), with a
/// random unusable password: sshd runs `UsePAM no`, and refuses public keys for
/// a locked account.
fn accounts() -> Result<(), String> {
    run("systemd-sysusers", &[])?;
    let agent = getent("passwd", AGENT_ACCOUNT).ok_or_else(|| format!("The {AGENT_ACCOUNT} account was not created."))?;
    let uid: u32 = agent.split(':').nth(2).and_then(|u| u.parse().ok()).unwrap_or(0);
    if uid == 0 || uid >= 1000 {
        return Err(format!("{AGENT_ACCOUNT} exists but is not a system account; review it separately."));
    }
    let members = getent("group", RUNTIME_GROUP).ok_or_else(|| format!("The {RUNTIME_GROUP} group was not created."))?;
    if !members.rsplit(':').next().unwrap_or("").split(',').any(|m| m == AGENT_ACCOUNT) {
        run("usermod", &["-a", "-G", RUNTIME_GROUP, AGENT_ACCOUNT])?;
    }
    if run("passwd", &["-S", AGENT_ACCOUNT])?.split_whitespace().nth(1) != Some("P") {
        let hash = password_hash()?;
        run("usermod", &["-p", &hash, AGENT_ACCOUNT])?;
    }
    Ok(())
}

/// A SHA-512 crypt hash of 32 random bytes nobody ever sees.
fn password_hash() -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("openssl: {e}"))?;
    child.stdin.take().ok_or("openssl: no input")?.write_all(random_hex()?.as_bytes()).map_err(|e| format!("openssl: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("openssl: {e}"))?;
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !hash.starts_with("$6$") {
        return Err("openssl could not make a password hash.".into());
    }
    Ok(hash)
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid)).map_err(|e| format!("{}: {e}", path.display()))
}

fn gid_of(group: &str) -> Result<u32, String> {
    getent("group", group).and_then(|g| g.split(':').nth(2)?.parse().ok()).ok_or_else(|| format!("The {group} group is missing."))
}

fn folders(desktop: &Account) -> Result<(), String> {
    let runtime = gid_of(RUNTIME_GROUP)?;
    // The install root keeps `current` → the package tree, so paths earlier
    // releases wrote keep working; a release tree `current` named becomes `previous`.
    let root = Path::new(INSTALL_ROOT);
    root_dir(root, 0o755)?;
    let current = root.join("current");
    match std::fs::symlink_metadata(&current) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let old = std::fs::read_link(&current).map_err(|e| e.to_string())?;
            if old != Path::new(LIB) && old.starts_with(root.join("releases")) {
                relink(&old, &root.join("previous"))?;
            }
        }
        Ok(_) => return Err(format!("{} is not a link; move it away and run setup again.", current.display())),
        Err(_) => {}
    }
    relink(Path::new(LIB), &current)?;

    let config = Path::new(CONFIG_DIR);
    root_dir(config, 0o750)?;
    chown(config, 0, runtime)?;
    std::fs::set_permissions(config, std::os::unix::fs::PermissionsExt::from_mode(0o750)).map_err(|e| e.to_string())?;
    run("setfacl", &["-m", &format!("u:{}:rx", desktop.name), CONFIG_DIR])?;
    root_dir(&config.join("ssh"), 0o755)?;
    root_dir(&Path::new(OPERATOR_PUBLIC).join("authorized_keys"), 0o755)?;
    root_dir(Path::new("/var/lib/ibara-operator"), 0o755)?;
    root_dir(Path::new(STATION).parent().unwrap_or(Path::new("/etc")), 0o755)?;
    // Every ibara-op-* account's login shell (access_system.rs names this path).
    let shell = Path::new(OPERATOR_SHELL);
    root_dir(shell.parent().unwrap_or(Path::new("/usr/local/sbin")), 0o755)?;
    relink(&Path::new(LIB).join("ops/ibara-op-shell"), shell)?;
    Ok(())
}

fn keys(desktop: &Account) -> Result<(), String> {
    let config = Path::new(CONFIG_DIR);
    let runtime = gid_of(RUNTIME_GROUP)?;
    // The gateway bearer: the agent entry (ibara-runtime) and the desktop controller read it.
    let gateway = config.join("gateway.key");
    create_root(&gateway, format!("{}\n", random_hex()?).as_bytes(), 0o640)?;
    chown(&gateway, 0, runtime)?;
    run("setfacl", &["-m", &format!("u:{}:r", desktop.name), &gateway.to_string_lossy()])?;
    // The administrator bearer (`ibara admin`, root only) and its digest for the controller.
    let admin = config.join("admin.key");
    create_root(&admin, format!("{}\n", random_hex()?).as_bytes(), 0o600)?;
    let admin_key = std::fs::read_to_string(&admin).map_err(|e| format!("{}: {e}", admin.display()))?;
    write_root(&config.join("admin.sha256"), format!("{}\n", sha256_hex(admin_key.trim().as_bytes())).as_bytes(), 0o644)?;
    let policy_path = config.join("policy.json");
    create_root(&policy_path, new_policy(&desktop.home).as_bytes(), 0o644)?;
    if let Some(rounded) = std::fs::read_to_string(&policy_path).ok().as_deref().and_then(round_old_file_limit) {
        write_root(&policy_path, rounded.as_bytes(), 0o644)?;
    }
    create_root(&config.join("operator-accounts.json"), b"{\"schema_version\":1,\"accounts\":{}}\n", 0o644)?;
    create_root(&Path::new(OPERATOR_PUBLIC).join("fingerprints.json"), b"{\"schema_version\":1,\"fingerprints\":{}}\n", 0o644)?;
    create_root(Path::new(STATION), format!("{:#}\n", json!({"schema_version": 1, "agent_account": desktop.name})).as_bytes(), 0o644)?;
    Ok(())
}

/// `policy.json` for a new computer: files a person sends arrive in their
/// `Downloads/Ibara` (made by setup as that person), up to 250 MB.
fn new_policy(home: &Path) -> String {
    let policy = json!({
        "max_process_output_chars": 12000,
        "max_artifact_bytes": crate::storage::DEFAULT_FILE_LIMIT,
        "principals": [],
        "operator_file_roots": { "transfers": home.join("Downloads/Ibara") },
    });
    format!("{policy:#}\n")
}

/// The file limit earlier releases wrote into `policy.json`: 256 MiB, which reads "268 MB".
const OLD_FILE_LIMIT: u64 = 268_435_456;

/// `policy.json` with that old default rounded to 250 MB, or `None` when it
/// names any other limit, none at all, or is not JSON: a limit a person set stays.
fn round_old_file_limit(text: &str) -> Option<String> {
    let mut policy: Value = serde_json::from_str(text).ok()?;
    let limit = policy.get_mut("max_artifact_bytes")?;
    if limit.as_u64() != Some(OLD_FILE_LIMIT) {
        return None;
    }
    *limit = json!(crate::storage::DEFAULT_FILE_LIMIT);
    Some(format!("{policy:#}\n"))
}

/// The sshd configuration of the agent entry. Addresses come from Tailscale
/// when it starts (`ops/ibara-agent-sshd`); one `Match` covers every account
/// the access projection makes for a paired computer.
pub fn sshd_config() -> String {
    let ssh = format!("{CONFIG_DIR}/ssh");
    let entry = format!("{INSTALL_ROOT}/current/ops/entry.sh");
    let restrictions = [
        "PermitTTY no",
        "DisableForwarding yes",
        "AllowTcpForwarding no",
        "AllowAgentForwarding no",
        "X11Forwarding no",
        "PermitTunnel no",
        "AllowStreamLocalForwarding no",
        "GatewayPorts no",
        "PermitUserRC no",
        "PasswordAuthentication no",
        "KbdInteractiveAuthentication no",
        "PubkeyAuthentication yes",
    ];
    let mut text = format!(
        "# ibara's agent entry, written by ibara setup (rewritten on every setup).\n\
         Port 2222\nHostKey {ssh}/host_ed25519\nPidFile /run/ibara-agent-sshd.pid\nAuthorizedKeysFile {ssh}/authorized_keys\n\
         AllowUsers {AGENT_ACCOUNT} ibara-op-*\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\n\
         PermitRootLogin no\nPermitTTY no\nDisableForwarding yes\nAllowAgentForwarding no\nPermitUserEnvironment no\nUsePAM no\n\
         PrintMotd no\nLogLevel VERBOSE\nMaxAuthTries 3\nMaxSessions 8\n\
         # ibara-operator-peer-match-v2\nMatch User ibara-op-*\n    AuthorizedKeysFile {OPERATOR_PUBLIC}/authorized_keys/%u\n    ForceCommand {entry}\n"
    );
    for option in restrictions {
        text.push_str(&format!("    {option}\n"));
    }
    text
}

fn gateway(desktop: &Account) -> Result<(), String> {
    let ssh = Path::new(CONFIG_DIR).join("ssh");
    let host_key = ssh.join("host_ed25519");
    if std::fs::symlink_metadata(&host_key).is_err() {
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
        run("ssh-keygen", &["-q", "-t", "ed25519", "-N", "", "-C", &format!("ibara-{}", host.trim()), "-f", &host_key.to_string_lossy()])?;
    }
    create_root(&ssh.join("authorized_keys"), b"", 0o644)?;
    let config = ssh.join("sshd_config");
    write_root(&config, sshd_config().as_bytes(), 0o600)?;
    run("/usr/bin/sshd", &["-t", "-f", &config.to_string_lossy()])?;
    // Seal the access projection's inventory for this desktop account once.
    if std::fs::symlink_metadata(Path::new(CONFIG_DIR).join("access-transport.json")).is_err() {
        output(Command::new(Path::new(LIB).join("bin/ibara")).args(["access-system", "init", &desktop.name]))?;
    }
    Ok(())
}

fn services(desktop: &Account) -> Result<(), String> {
    // Unit files earlier releases wrote under /etc hide the package's; set them aside.
    let mut moved = Vec::new();
    for name in SHADOWING_UNITS.iter().chain(&["agent-computer.service", "ibara-operator.service"]) {
        let system = !name.starts_with("agent-computer") && !name.starts_with("ibara-operator");
        let path = Path::new(if system { "/etc/systemd/system" } else { "/etc/systemd/user" }).join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            if system && !name.contains('@') {
                let _ = run("systemctl", &["disable", name]);
            }
            moved.push(path);
        }
    }
    if !moved.is_empty() {
        let aside = PathBuf::from(BACKUPS).join(format!("units-{}", crate::ids::now_millis()));
        root_dir(&aside, 0o700)?;
        for path in &moved {
            std::fs::rename(path, aside.join(path.file_name().unwrap_or_default())).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        println!("    Earlier unit files set aside in {}", aside.display());
        // What runs now was started from those files: restart it from the package's.
        for record in ["root-sockets", "agent-entry"] {
            let _ = std::fs::remove_file(Path::new(INSTALL_ROOT).join(format!("{record}.sha256")));
        }
    }
    // Only the desktop account's own ibarad may open the root helpers.
    let dropin = |unit: &str, body: String| -> Result<(), String> {
        let dir = Path::new("/etc/systemd/system").join(format!("{unit}.d"));
        root_dir(&dir, 0o755)?;
        write_root(&dir.join("desktop.conf"), body.as_bytes(), 0o644)
    };
    dropin("ibara-access.socket", format!("[Socket]\nSocketUser={}\n", desktop.name))?;
    dropin("ibara-power.socket", format!("[Socket]\nSocketUser={}\nSocketGroup={}\n", desktop.name, desktop.group))?;
    // Controller sockets: the agent entry reaches them through ibara-runtime,
    // the desktop account through its ACL; per-computer sockets live in /run/ibara-operator.
    let tmpfiles = Path::new("/etc/tmpfiles.d");
    root_dir(tmpfiles, 0o755)?;
    let runtime = tmpfiles.join("agent-computer.conf");
    let peers = tmpfiles.join("ibara-operator-peer.conf");
    write_root(
        &runtime,
        // The mask is named: every later `d` line resets the mode (2750), which alone
        // would cut the desktop account's ACL to r-x and stop its controller creating sockets.
        format!("d /run/agent-computer 2750 root {RUNTIME_GROUP} -\na+ /run/agent-computer - - - - u:{}:rwx,m::rwx\n", desktop.name).as_bytes(),
        0o644,
    )?;
    write_root(&peers, format!("d /run/ibara-operator 0711 {} {} -\n", desktop.name, desktop.group).as_bytes(), 0o644)?;
    run("systemd-tmpfiles", &["--create", &runtime.to_string_lossy(), &peers.to_string_lossy()])?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "ibara-agent-sshd.service", "ibara-access.socket", "ibara-power.socket"])?;
    let units = Path::new("/usr/lib/systemd/system");
    let dropins = Path::new("/etc/systemd/system");
    let sockets = [
        units.join("ibara-access.socket"),
        units.join("ibara-access@.service"),
        units.join("ibara-power.socket"),
        units.join("ibara-power@.service"),
        dropins.join("ibara-access.socket.d/desktop.conf"),
        dropins.join("ibara-power.socket.d/desktop.conf"),
    ];
    let restarted = start_or_restart("root-sockets", &sockets, &["ibara-access.socket", "ibara-power.socket"])?;
    println!("    Root helpers {}.", if restarted { "restarted: they changed" } else { "unchanged, left running" });
    // The agent entry listens on this computer's Tailscale addresses; until
    // Tailscale is signed in it retries on its own. A restart ends no session
    // (KillMode=process), but it drops the listener, so only a change restarts it.
    let entry = [
        units.join("ibara-agent-sshd.service"),
        Path::new(LIB).join("ops/ibara-agent-sshd"),
        Path::new(CONFIG_DIR).join("ssh/sshd_config"),
    ];
    let restarted = start_or_restart("agent-entry", &entry, &["--no-block", "ibara-agent-sshd.service"])?;
    println!("    Agent entry {}.", if restarted { "restarted: it changed" } else { "unchanged, left running" });
    Ok(())
}

/// Start `units` (a no-op for running ones), or restart them when a file they
/// run from changed since setup last did; `name` keeps the digest of `files`
/// in the install root. True when they were restarted.
fn start_or_restart(name: &str, files: &[PathBuf], units: &[&str]) -> Result<bool, String> {
    let mut seen = Vec::new();
    for file in files {
        seen.extend_from_slice(file.as_os_str().as_encoded_bytes());
        seen.push(0);
        seen.extend(std::fs::read(file).unwrap_or_default());
        seen.push(0);
    }
    let digest = sha256_hex(&seen);
    let record = Path::new(INSTALL_ROOT).join(format!("{name}.sha256"));
    let changed = std::fs::read_to_string(&record).map(|r| r.trim() != digest).unwrap_or(true);
    let mut args = vec![if changed { "restart" } else { "start" }];
    args.extend(units);
    run("systemctl", &args)?;
    if changed {
        write_root(&record, format!("{digest}\n").as_bytes(), 0o644)?;
    }
    Ok(changed)
}

/// The numbers of ufw rules this setup wrote (or an earlier release wrote for
/// the same ports) on the tailnet interface, highest first, so deleting them in
/// order keeps the rest valid. `all`: every ibara rule (uninstall); otherwise
/// only rules limited to one source address, which earlier releases wrote.
pub fn ibara_rules(status_numbered: &str, all: bool) -> Vec<u32> {
    let mut numbers: Vec<u32> = status_numbered
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('[')?;
            let (number, rest) = rest.split_once(']')?;
            let number: u32 = number.trim().parse().ok()?;
            let (rule, comment) = rest.split_once(" # ")?;
            let (to, from) = rule.split_once("ALLOW IN")?;
            let to = to.trim();
            let comment = comment.trim();
            let ours = to.ends_with("on tailscale0")
                && comment.get(..6).is_some_and(|word| word.eq_ignore_ascii_case("ibara "))
                && FIREWALL_RULES.iter().any(|(ports, proto, _)| to.starts_with(&format!("{ports}/{proto} ")));
            (ours && (all || !from.trim().starts_with("Anywhere"))).then_some(number)
        })
        .collect();
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    numbers
}

fn ufw_active() -> Option<bool> {
    if !Path::new("/usr/bin/ufw").is_file() {
        return None;
    }
    Some(run("ufw", &["status"]).is_ok_and(|s| s.lines().any(|l| l.trim() == "Status: active")))
}

/// Open the agent entry, pairing and streaming to the tailnet only. Tailscale
/// has already authenticated every computer there; SSH still needs an enrolled
/// key, pairing another person's Accept, and streaming a paired viewer.
fn firewall() -> Result<(), String> {
    match ufw_active() {
        None => {
            println!("    No ufw on this computer; nothing to open.");
            return Ok(());
        }
        Some(false) => {
            println!("    ufw is off; nothing to open.");
            return Ok(());
        }
        Some(true) => {}
    }
    for number in ibara_rules(&run("ufw", &["status", "numbered"])?, false) {
        run("ufw", &["--force", "delete", &number.to_string()])?;
    }
    for (ports, proto, comment) in FIREWALL_RULES {
        run("ufw", &["allow", "in", "on", "tailscale0", "to", "any", "port", ports, "proto", proto, "comment", comment])?;
    }
    Ok(())
}

/// Tailscale's daemon on, and the desktop account allowed to sign in and out
/// without sudo (as Omarchy's own Tailscale install does), so the console's
/// Sign In to Tailscale works. Signing in is the person's step.
fn tailscale(desktop: &Account) -> Result<(), String> {
    if !Path::new("/usr/bin/tailscale").is_file() {
        println!("    Tailscale is not installed.");
        return Ok(());
    }
    if run("systemctl", &["is-enabled", "tailscaled.service"]).map(|s| s.trim() != "enabled").unwrap_or(true) {
        run("systemctl", &["enable", "--now", "tailscaled.service"])?;
    }
    for _ in 0..20 {
        if let Ok(prefs) = run("tailscale", &["debug", "prefs"]) {
            let prefs: Value = serde_json::from_str(&prefs).unwrap_or(Value::Null);
            if prefs.get("OperatorUser").and_then(Value::as_str).is_some_and(|u| !u.is_empty()) {
                return Ok(());
            }
            return run("tailscale", &["set", &format!("--operator={}", desktop.name)]).map(|_| ());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("    Tailscale's daemon did not answer yet; Sign In to Tailscale may ask for your password.");
    Ok(())
}

/// The browser page reader, installed by browser policy (entry/browser_setup.rs).
fn browser(desktop: &Account) -> Result<(), String> {
    let extension = Path::new(LIB).join("chrome-extension");
    output(Command::new(Path::new(LIB).join("bin/ibara")).arg("browser-setup").arg(&extension).arg(&desktop.name)).map(|_| ())
}

/// What `cua/build.json` records a build against.
fn cua_inputs() -> Option<Value> {
    let versions = run("pacman", &["-Q", "hyprland", "gcc"]).ok()?;
    let version = |name: &str| versions.lines().find_map(|l| l.strip_prefix(&format!("{name} ")).map(str::to_string));
    Some(json!({"hyprland": version("hyprland")?, "gcc": version("gcc")?, "ibara": installed_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").into())}))
}

/// Cua's Hyprland plugin, built on this computer for its exact Hyprland and
/// GCC, again whenever either (or ibara) changed since the last build.
fn cua_plugin(desktop: &Account) -> Result<(), String> {
    let Some(inputs) = cua_inputs() else {
        println!("    Hyprland is not installed; the plugin is built once it is.");
        return Ok(());
    };
    let dir = Path::new(INSTALL_ROOT).join("cua");
    let built: Value = std::fs::read(dir.join("build.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let same = ["hyprland", "gcc", "ibara"].iter().all(|k| built.get(*k) == inputs.get(*k));
    if same && dir.join("cua-hyprland-plugin.so").is_file() {
        println!("    Already built for Hyprland {}.", inputs["hyprland"].as_str().unwrap_or(""));
        return Ok(());
    }
    let script = Path::new(LIB).join("ops/cua-plugin.sh");
    let out = output(
        Command::new("bash")
            .arg(&script)
            .arg(Path::new(LIB).join("cua-hyprland-plugin"))
            .arg(&desktop.name)
            .env("IBARA_VERSION", inputs["ibara"].as_str().unwrap_or("")),
    )?;
    if let Some(last) = out.lines().rev().find(|l| !l.trim().is_empty()) {
        println!("    {}", last.trim());
    }
    Ok(())
}

fn uninstall(desktop: &Account, delete_data: bool) -> Result<(), String> {
    if let Some(owner) = station_owner().filter(|o| *o != desktop.name) {
        return Err(format!("ibara on this computer belongs to {owner}; run ibara uninstall as {owner}."));
    }
    println!("Removing ibara's system parts (as root):");
    step("System services", || {
        let names: Vec<&str> = SYSTEM_UNITS.iter().chain(&LEGACY_UNITS).copied().filter(|n| !n.contains('@')).collect();
        for name in &names {
            let _ = run("systemctl", &["disable", "--now", name]);
        }
        // Stopping the agent entry leaves its sessions (KillMode=process); end them.
        if getent("passwd", AGENT_ACCOUNT).is_some() {
            let _ = run("pkill", &["-KILL", "-u", AGENT_ACCOUNT]);
        }
        for dir in ["ibara-access.socket.d", "ibara-power.socket.d"] {
            let _ = std::fs::remove_dir_all(Path::new("/etc/systemd/system").join(dir));
        }
        for name in LEGACY_UNITS {
            let _ = std::fs::remove_file(Path::new("/etc/systemd/system").join(name));
        }
        for file in ["/etc/tmpfiles.d/agent-computer.conf", "/etc/tmpfiles.d/ibara-operator-peer.conf"] {
            let _ = std::fs::remove_file(file);
        }
        run("systemctl", &["daemon-reload"]).map(|_| ())
    })?;
    step("Accounts of paired computers", || {
        let passwd = run("getent", &["passwd"])?;
        for name in passwd.lines().filter_map(|l| l.split(':').next()).filter(|n| n.starts_with("ibara-op-")) {
            let _ = run("pkill", &["-KILL", "-u", name]);
            run("userdel", &[name])?;
        }
        let keys = Path::new(OPERATOR_PUBLIC).join("authorized_keys");
        if let Ok(entries) = std::fs::read_dir(&keys) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    })?;
    step("Firewall", || {
        if ufw_active() == Some(true) {
            for number in ibara_rules(&run("ufw", &["status", "numbered"])?, true) {
                run("ufw", &["--force", "delete", &number.to_string()])?;
            }
        }
        Ok(())
    })?;
    step("Browser page reader", || {
        let mut args = vec!["browser-setup", "--remove"];
        if delete_data {
            args.push("--delete-key");
        }
        output(Command::new(Path::new(LIB).join("bin/ibara")).args(&args)).map(|_| ())
    })?;
    step("Links and built plugin", || {
        let root = Path::new(INSTALL_ROOT);
        let _ = std::fs::remove_dir_all(root.join("cua"));
        for link in [root.join("current"), PathBuf::from(OPERATOR_SHELL)] {
            if std::fs::read_link(&link).is_ok_and(|to| to.starts_with(LIB)) {
                std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
            }
        }
        Ok(())
    })?;
    if delete_data {
        step("Keys, identity and history kept by root", || {
            for path in [CONFIG_DIR, OPERATOR_PUBLIC, "/var/lib/ibara-operator", "/var/lib/ibara-agent", INSTALL_ROOT, "/var/cache/ibara", BACKUPS] {
                if std::fs::symlink_metadata(path).is_ok() {
                    std::fs::remove_dir_all(path).map_err(|e| format!("{path}: {e}"))?;
                }
            }
            let _ = std::fs::remove_file(STATION);
            let _ = std::fs::remove_dir(Path::new(STATION).parent().unwrap_or(Path::new("/etc/ibara")));
            if getent("passwd", AGENT_ACCOUNT).is_some() {
                let _ = run("pkill", &["-KILL", "-u", AGENT_ACCOUNT]);
                run("userdel", &[AGENT_ACCOUNT])?;
            }
            if getent("group", RUNTIME_GROUP).is_some() {
                run("groupdel", &[RUNTIME_GROUP])?;
            }
            Ok(())
        })?;
    } else {
        println!("  Kept: this computer's keys and identity ({CONFIG_DIR}, {OPERATOR_PUBLIC}, {STATION}) for a later install.");
    }
    step("The ibara packages", || {
        let installed: Vec<&str> =
            super::update::PACKAGES.iter().copied().filter(|name| run("pacman", &["-Q", name]).is_ok()).collect();
        if installed.is_empty() {
            return Ok(());
        }
        let status = Command::new("pacman").args(["-R", "--noconfirm"]).args(&installed).status().map_err(|e| format!("pacman: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("pacman could not remove {}.", installed.join(", "))) }
    })?;
    Ok(())
}

/// Failure cases for the firewall rules this setup deletes (the rest of the
/// foundation is proven end to end in a fresh container, see packaging/e2e):
/// 1. A person's own rule on the same port, or on another interface, is deleted.
/// 2. Rules earlier releases wrote for one source address stay beside the new ones.
/// 3. Deleting in ascending order renumbers later rules, so the wrong ones go.
/// 4. Uninstall leaves one of ibara's rules (the tailnet-wide ones, v6 copies) behind.
/// 5. Rules earlier releases commented "Ibara …" (capitalized) are no longer found.
#[cfg(test)]
mod tests {
    use super::ibara_rules;

    const STATUS: &str = "Status: active

     To                         Action      From
     --                         ------      ----
[ 1] 22/tcp                     ALLOW IN    Anywhere                   # ssh
[ 2] 2222/tcp on tailscale0     ALLOW IN    100.101.102.103            # Ibara agent entry
[ 3] 2222/tcp on tailscale0     ALLOW IN    Anywhere                   # ibara agent entry
[ 4] 24247/tcp on tailscale0    ALLOW IN    Anywhere                   # Ibara pairing
[ 5] 47984,47989,48010/tcp on tailscale0 ALLOW IN    100.64.0.9    # Ibara station_x Sunshine streaming
[ 6] 2222/tcp on wlan0          ALLOW IN    Anywhere                   # ibara agent entry
[ 7] 24247/tcp on tailscale0    ALLOW IN    Anywhere                   # my own pairing experiment
[ 8] 2222/tcp (v6) on tailscale0 ALLOW IN    Anywhere (v6)             # ibara agent entry
[ 9] 47998,47999,48000,48002/udp on tailscale0 ALLOW IN    Anywhere    # ibara streaming
";

    #[test]
    fn setup_replaces_only_single_source_rules_of_ibara() {
        assert_eq!(ibara_rules(STATUS, false), vec![5, 2]);
    }

    #[test]
    fn uninstall_takes_every_ibara_rule_on_the_tailnet_and_nothing_else() {
        assert_eq!(ibara_rules(STATUS, true), vec![9, 8, 5, 4, 3, 2]);
    }
}

/// `policy.json` names 250 MB on a new computer, and setup on an update rounds
/// the file limit earlier releases wrote (256 MiB, which reads "268 MB") to it.
/// Failure cases:
/// 1. A new computer is given anything but 250,000,000, or a second setup run rewrites it.
/// 2. The old default stays, and refusals keep saying "up to 268 MB".
/// 3. A limit a person set is changed, including one a byte away from the old default.
/// 4. The rewrite loses or reorders the policy's other settings.
/// 5. A policy with no limit gains one, or one that is not JSON is overwritten.
#[cfg(test)]
mod file_limit_tests {
    use super::{new_policy, round_old_file_limit};
    use crate::storage::{StorageOptions, file_too_large};
    use serde_json::{Value, json};
    use std::path::Path;

    /// The refusal of a 300 MiB file on a computer run from this policy text.
    fn refusal(text: &str) -> String {
        let policy: Value = serde_json::from_str(text).unwrap();
        file_too_large(300 << 20, StorageOptions::from_env(&policy).unwrap().max_artifact_bytes)
    }

    #[test]
    fn a_new_computer_sends_files_up_to_250_mb() {
        let text = new_policy(Path::new("/home/riley"));
        let policy: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(policy["max_artifact_bytes"], json!(250_000_000u64));
        assert_eq!(policy["operator_file_roots"], json!({"transfers": "/home/riley/Downloads/Ibara"}));
        assert_eq!(refusal(&text), "This file is 315 MB; ibara sends files up to 250 MB.");
        assert_eq!(round_old_file_limit(&text), None, "a second setup run leaves it");
    }

    #[test]
    fn only_the_old_default_file_limit_is_rounded() {
        let old = "{\n  \"max_process_output_chars\": 12000,\n  \"max_artifact_bytes\": 268435456,\n  \"principals\": [\"hazel\"],\n  \"operator_file_roots\": {\n    \"transfers\": \"/home/riley/Downloads/Ibara\"\n  }\n}\n";
        assert_eq!(refusal(old), "This file is 315 MB; ibara sends files up to 268 MB.");
        let new = round_old_file_limit(old).expect("the old default is rounded");
        assert_eq!(refusal(&new), "This file is 315 MB; ibara sends files up to 250 MB.");
        let policy: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(
            policy,
            json!({"max_process_output_chars": 12000, "max_artifact_bytes": 250_000_000u64, "principals": ["hazel"],
                   "operator_file_roots": {"transfers": "/home/riley/Downloads/Ibara"}})
        );
        let keys: Vec<&String> = policy.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["max_process_output_chars", "max_artifact_bytes", "principals", "operator_file_roots"]);
        for kept in [
            "{\"max_artifact_bytes\": 1073741824}",
            "{\"max_artifact_bytes\": 268435457}",
            "{\"max_artifact_bytes\": 250000000}",
            "{\"principals\": []}",
            "not json",
        ] {
            assert_eq!(round_old_file_limit(kept), None, "{kept}");
        }
        assert_eq!(refusal("{\"max_artifact_bytes\": 1073741824}"), "This file is 315 MB; ibara sends files up to 1.1 GB.");
    }
}
