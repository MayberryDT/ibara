//! `ibara setup` and `ibara uninstall`, run by the person who uses this
//! computer. Everything in their home is written by their own account; the
//! system parts go through one `sudo ibara system …` (one password prompt).

use super::{Account, LIB, PLUGIN, PLUGIN_ID, USER_UNITS, interactive, is_root, output, program, sha256_hex};
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The line `cua-plugin.sh` adds to the person's Hyprland configuration.
const HYPRLAND_INCLUDE: &str = "/opt/agent-computer/cua/hyprland.lua";

pub(super) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("No async runtime: {e}"))
}

fn person() -> Result<Account, String> {
    if is_root() {
        return Err("Run this as yourself, not as root (no sudo). It asks for your password when it needs it.".into());
    }
    Account::current()
}

fn installed() -> Result<(), String> {
    if Path::new(LIB).join("bin/ibara").is_file() {
        Ok(())
    } else {
        Err(format!("The ibara package is not installed ({LIB} is missing). Install ibara with its one-line installer first."))
    }
}

/// `sudo ibara system ARGS…` with this terminal, so sudo asks once.
pub(super) fn as_root(args: &[&str]) -> Result<(), String> {
    interactive(Command::new("sudo").arg("--").arg(Path::new(LIB).join("bin/ibara")).arg("system").args(args))
}

pub fn setup(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("Usage: ibara setup".into());
    }
    let me = person()?;
    installed()?;
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
    println!("Setting up ibara on {host} for {}.", me.name);
    println!("This computer becomes a console for your other computers and a computer they (and your agents) can use.\n");

    runtime()?.block_on(crate::console::pairing::operator_key()).map_err(|e| format!("Your ibara key: {e}"))?;
    println!("  Your ibara key: {}", me.home.join(".ssh/ibara_agent_ed25519").display());
    let transfers = me.home.join("Downloads/Ibara");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&transfers).map_err(|e| format!("{}: {e}", transfers.display()))?;
    replace_old_copies(&me)?;
    let plugin_before = plugin_digest();

    println!("\nThe next part needs your password once (sudo).");
    as_root(&["setup", &me.name])?;

    println!("\nStarting ibara:");
    services(&["daemon-reload"])?;
    services(&["reenable", USER_UNITS[0], USER_UNITS[1]])?;
    services(&["restart", USER_UNITS[0], USER_UNITS[1]])?;
    println!("  {} and {} are running.", USER_UNITS[0], USER_UNITS[1]);

    println!("\nThe ibara bar icon:");
    let changed = link_plugin(&me)? || plugin_digest() != plugin_before;
    show_plugin(changed)?;

    println!("\nTailscale:");
    println!("  {}", tailscale_line());
    println!("\nibara is set up. Open it from its icon in the bar; Add Computer lists your other computers.");
    println!("To add this computer from another one, install ibara there too.");
    println!("To connect an agent, copy the prompt from Connect an Agent in the console and paste it to your agent; it connects itself.");
    Ok(())
}

fn services(args: &[&str]) -> Result<(), String> {
    output(Command::new("systemctl").arg("--user").args(args))
        .map(|_| ())
        .map_err(|e| format!("{e}\nibara's services run in your desktop session; run ibara setup from a terminal on this computer's desktop."))
}

/// Copies an earlier hand installation left in the person's home would hide
/// the package: binaries in `~/.local/bin` (copies, or links into that
/// installation's own tree) become links to the package's, and user units in
/// `~/.config/systemd/user` are set aside.
fn replace_old_copies(me: &Account) -> Result<(), String> {
    let aside = me.home.join(format!(".local/state/ibara/replaced-{}", crate::ids::now_millis()));
    let mut moved = Vec::new();
    for (path, link) in [
        (me.home.join(".local/bin/ibara"), Some("/usr/bin/ibara")),
        (me.home.join(".local/bin/ibarad"), Some("/usr/bin/ibarad")),
        (me.home.join(".config/systemd/user").join(USER_UNITS[0]), None),
        (me.home.join(".config/systemd/user").join(USER_UNITS[1]), None),
    ] {
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        let old = match link {
            // A link that resolves to the package's program is already right.
            Some(target) if meta.file_type().is_symlink() => {
                std::fs::canonicalize(&path).ok().is_none_or(|to| std::fs::canonicalize(target).ok() != Some(to))
            }
            _ => meta.is_file(),
        };
        if !old {
            continue;
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&aside).map_err(|e| format!("{}: {e}", aside.display()))?;
        std::fs::rename(&path, aside.join(path.file_name().unwrap_or_default())).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(target) = link {
            std::os::unix::fs::symlink(target, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        moved.push(path.display().to_string());
    }
    if !moved.is_empty() {
        println!("  Replaced copies from an earlier installation ({}); the old files are in {}.", moved.join(", "), aside.display());
    }
    Ok(())
}

fn plugins_dir(me: &Account) -> PathBuf {
    me.home.join(".config/omarchy/plugins")
}

/// Every file of the packaged plugin, so setup and update know whether the
/// shell has to load it again.
pub(super) fn plugin_digest() -> String {
    fn walk(dir: &Path, into: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, into);
            } else if let Ok(bytes) = std::fs::read(&path) {
                into.push((path.display().to_string(), bytes));
            }
        }
    }
    let mut files = Vec::new();
    walk(Path::new(PLUGIN), &mut files);
    files.sort();
    let mut all = Vec::new();
    for (name, bytes) in files {
        all.extend(name.into_bytes());
        all.push(0);
        all.extend(sha256_hex(&bytes).into_bytes());
    }
    sha256_hex(&all)
}

/// Omarchy's plugin folder gets a link to the packaged plugin, so it always
/// matches the installed ibara and updates with it. A copy installed from the
/// plugin marketplace is set aside (dot folders are not loaded). True when
/// something changed.
pub fn link_plugin(me: &Account) -> Result<bool, String> {
    let dir = plugins_dir(me);
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let link = dir.join(PLUGIN_ID);
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(&link).ok().as_deref() == Some(Path::new(PLUGIN)) {
                println!("  Already linked to the installed ibara.");
                return Ok(false);
            }
            std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
        Ok(meta) if meta.is_dir() && meta.uid() == me.uid => {
            let aside = dir.join(format!(".{PLUGIN_ID}.marketplace-{}", crate::ids::now_millis()));
            std::fs::rename(&link, &aside).map_err(|e| format!("{}: {e}", link.display()))?;
            println!("  The plugin copy from the marketplace is replaced by the installed one (kept in {}).", aside.display());
        }
        Ok(_) => return Err(format!("{} is in the way; move it away and run ibara setup again.", link.display())),
        Err(_) => {}
    }
    std::os::unix::fs::symlink(PLUGIN, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    println!("  Linked {} to the installed ibara.", link.display());
    Ok(true)
}

/// Where Omarchy's commands are: [`program`], or stand-ins in tests.
type Find<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// Omarchy's shell commands, with `OMARCHY_PATH` set as they expect.
fn omarchy(name: &str, args: &[&str]) -> Option<Result<String, String>> {
    omarchy_in(&program, name, args)
}

fn omarchy_in(find: Find, name: &str, args: &[&str]) -> Option<Result<String, String>> {
    let path = find(name)?;
    let mut command = Command::new(path);
    command.args(args).env("OMARCHY_SHELL_IPC_TIMEOUT", "2s");
    if std::env::var_os("OMARCHY_PATH").is_none() {
        command.env("OMARCHY_PATH", crate::theme::omarchy_path());
    }
    Some(output(&mut command))
}

fn shell_running() -> bool {
    matches!(omarchy("omarchy-shell", &["shell", "ping"]), Some(Ok(_)))
}

/// How long ibara waits for Omarchy's old shell to exit, then for a new one to answer.
struct ShellWaits {
    stop: Duration,
    start: Duration,
}

const SHELL_WAITS: ShellWaits = ShellWaits { stop: Duration::from_secs(20), start: Duration::from_secs(10) };

/// Restart Omarchy's shell and make sure a new one answers. `Ok(true)` when
/// ibara had to start it again.
///
/// `omarchy-restart-shell` waits 5 s for the old shell to exit, then starts the
/// new one. An old shell slower than that (one that reloaded a changed plugin
/// many times took 7.5 s) still holds Quickshell's instance lock, so the new
/// one exits at once as "already running". The restart can still report
/// success, because the old shell answers its ping, and once the old shell
/// exits there is no shell and no bar. So ibara waits for the old shell to be
/// gone and for a new one to answer, and starts it once more the way Omarchy
/// does when none does.
fn restart_shell() -> Result<bool, String> {
    restart_shell_in(&program, &SHELL_WAITS)
}

fn restart_shell_in(find: Find, waits: &ShellWaits) -> Result<bool, String> {
    const BY_HAND: &str = "Run: omarchy restart shell";
    let old = shell_pids(find);
    let restarted = omarchy_in(find, "omarchy-restart-shell", &[]).ok_or("Omarchy's restart command is missing, so the shell was not restarted.")?;
    if let Err(e) = &restarted && old.iter().any(|&pid| alive(pid)) {
        return Err(format!("The shell did not restart ({e}). {BY_HAND}"));
    }
    if !within(waits.stop, || !old.iter().any(|&pid| alive(pid))) {
        return Err(match restarted {
            Err(e) => format!("The shell did not restart ({e}). {BY_HAND}"),
            Ok(_) => format!("Omarchy's old shell did not stop, so it still runs the old plugin. {BY_HAND}"),
        });
    }
    let answers = || matches!(omarchy_in(find, "omarchy-shell", &["shell", "ping"]), Some(Ok(_)));
    if within(waits.start, answers) {
        return Ok(false);
    }
    let started = launch_shell(find);
    if within(waits.start, answers) {
        return Ok(true);
    }
    let why = started.err().map(|e| format!(" ({e})")).unwrap_or_default();
    Err(format!("Omarchy's shell did not come back after the restart{why}, so there is no bar. {BY_HAND}"))
}

/// The running Omarchy shells' process ids, as Quickshell lists them.
fn shell_pids(find: Find) -> Vec<i32> {
    let config = crate::theme::omarchy_path().join("shell");
    let Some(Ok(list)) = omarchy_in(find, "qs", &["list", "--json", "--any-display", "--path", &config.to_string_lossy()]) else { return Vec::new() };
    let list: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap_or_default();
    list.iter().filter_map(|shell| i32::try_from(shell["pid"].as_i64()?).ok()).collect()
}

/// Start the shell the way `omarchy-restart-shell` does: from Hyprland, so it
/// has the session's environment rather than this terminal's. Outside the
/// session (over SSH), Hyprland is the newest one in the runtime folder, as
/// Omarchy finds it.
fn launch_shell(find: Find) -> Result<(), String> {
    let hyprctl = find("hyprctl").ok_or("hyprctl is missing")?;
    let mut command = Command::new(hyprctl);
    command.args(["dispatch", r#"hl.dsp.exec_cmd("omarchy-launch-shell")"#]);
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
        && let Some(instance) = newest_hyprland()
    {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", instance);
    }
    output(&mut command).map(drop)
}

fn newest_hyprland() -> Option<std::ffi::OsString> {
    // SAFETY: getuid has no preconditions.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }).into());
    let instances = std::fs::read_dir(runtime.join("hypr")).ok()?.flatten();
    let dirs = instances.filter_map(|entry| {
        let meta = entry.metadata().ok().filter(std::fs::Metadata::is_dir)?;
        Some((meta.modified().ok()?, entry.file_name()))
    });
    dirs.max_by_key(|(modified, _)| *modified).map(|(_, name)| name)
}

/// Whether `done` holds within `limit`, asked every 200 ms.
fn within(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether a process still runs. One that exited and waits to be reaped has
/// already let go of its instance lock.
fn alive(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    !matches!(stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next()), Some('Z' | 'X'))
}

/// Whether the running shell has the plugin enabled.
fn plugin_enabled() -> bool {
    let Some(Ok(list)) = omarchy("omarchy-shell", &["shell", "listPlugins"]) else { return false };
    let list: serde_json::Value = serde_json::from_str(&list).unwrap_or_default();
    list.as_array().is_some_and(|all| all.iter().any(|p| p["id"] == PLUGIN_ID && p["enabled"] == true))
}

/// Enable the plugin through Omarchy's plugin mechanism (its bar icon goes to
/// the bar only if it is not there already), then restart the shell once when
/// the plugin is new or changed, since a rescan does not reload loaded QML.
fn show_plugin(changed: bool) -> Result<(), String> {
    if program("omarchy-shell").is_none() {
        println!("  Omarchy is not on this computer, so there is no bar icon. The console plugin is in {PLUGIN}.");
        return Ok(());
    }
    if !shell_running() {
        println!("  Omarchy's shell is not running here. Run ibara setup again from the desktop to show the icon.");
        return Ok(());
    }
    let was_enabled = plugin_enabled();
    if let Some(Err(e)) = omarchy("omarchy-shell", &["shell", "rescanPlugins"]) {
        return Err(format!("Omarchy could not look for new plugins: {e}"));
    }
    if !was_enabled {
        let placed = omarchy("omarchy-shell", &["shell", "putBarWidget", PLUGIN_ID, "{}"]).and_then(Result::ok);
        if placed.as_deref().map(str::trim) != Some("ok") {
            match omarchy("omarchy-plugin-enable", &[PLUGIN_ID]) {
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("Omarchy could not enable the ibara plugin: {e}")),
                None => return Err("Omarchy's plugin commands are missing.".into()),
            }
        }
        println!("  Enabled in Omarchy's bar.");
    }
    if changed || !was_enabled {
        println!("  Restarting Omarchy's shell once to load it…");
        match restart_shell() {
            Ok(false) => println!("  Done."),
            Ok(true) => println!("  Done. The new shell closed right away, so ibara started it again."),
            Err(e) => println!("  {e}"),
        }
    } else {
        println!("  Already showing.");
    }
    Ok(())
}

/// After an update or rollback: restart the shell once when the installed
/// plugin changed, so it loads the new one.
pub(super) fn reload_plugin(before: String) -> Result<(), String> {
    if plugin_digest() == before || !shell_running() {
        shell_outcome("unchanged", "")?;
        return Ok(());
    }
    if session_locked() {
        let path = shell_restart_path();
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(path, format!("{}\n", serde_json::json!({"plugin_digest": plugin_digest(), "since": crate::ids::now_millis(), "reason": "locked"}))).map_err(|e| e.to_string())?;
        shell_outcome("deferred_locked", "Its bar restarts after unlock.")?;
        return Ok(());
    }
    println!("Restarting Omarchy's shell once to load the new ibara plugin…");
    match restart_shell() {
        Ok(_) => {
            let _ = std::fs::remove_file(shell_restart_path());
            shell_outcome("restarted", "")?;
        }
        Err(e) => { shell_outcome("failed", &e)?; return Err(e); }
    }
    Ok(())
}

fn shell_restart_path() -> PathBuf {
    crate::operator::directory::operator_state_dir().join("shell-restart.json")
}

fn session_locked() -> bool {
    program("omarchy-hyprland-session-locked").is_some_and(|p| Command::new(p).status().is_ok_and(|s| s.success()))
}

fn shell_outcome(state: &str, message: &str) -> Result<(), String> {
    let path = crate::operator::directory::operator_state_dir().join("shell-restart-result.json");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(path, format!("{}\n", serde_json::json!({"state": state, "message": message, "at": crate::ids::now_millis()}))).map_err(|e| e.to_string())
}

pub(super) fn shell_outcome_at(home: &Path) -> Option<serde_json::Value> {
    std::fs::read(home.join(".local/state/ibara/shell-restart-result.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Only poll the lock when an update left a restart pending. The target owns
/// this loop; console-only installations start it in their operator daemon.
pub(crate) fn start_shell_retry() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            if !shell_restart_path().is_file() { continue; }
            let _ = tokio::task::spawn_blocking(|| {
                if session_locked() { return; }
                match restart_shell() {
                    Ok(_) => { let _ = shell_outcome("restarted", ""); let _ = std::fs::remove_file(shell_restart_path()); }
                    Err(e) => {
                        // An unlock/lock race should stay pending, not break a live lock.
                        if !session_locked() { let _ = shell_outcome("failed", &e); let _ = std::fs::remove_file(shell_restart_path()); }
                    }
                }
            }).await;
        }
    });
}

/// One plain line about Tailscale: signed in, or how to sign in.
fn tailscale_line() -> String {
    let status = runtime().ok().map(|rt| rt.block_on(crate::tailnet::status()));
    match status {
        Some(Ok(status)) if status.running() => {
            let login = status.login().map(|l| format!(" as {l}")).unwrap_or_default();
            format!("Signed in{login}. Your other computers show up in Add Computer.")
        }
        Some(Ok(status)) if status.backend == "NeedsLogin" || status.backend == "NoState" || status.backend == "NeedsMachineAuth" => {
            let page = status.auth_url.map(|u| format!(" or open {u}")).unwrap_or_default();
            format!(
                "Not signed in yet. Sign in so ibara can find your other computers: open ibara from the bar and choose Sign In to Tailscale, or run: tailscale up{page}"
            )
        }
        Some(Ok(status)) => format!("Tailscale is {}. Turn it on with: tailscale up", status.backend.to_lowercase()),
        Some(Err(crate::tailnet::CliError::NotInstalled)) => {
            "Tailscale is not installed. Install it with: omarchy-install-service-tailscale".into()
        }
        _ => "Tailscale is not answering. Start it with: sudo systemctl enable --now tailscaled, then run: tailscale up".into(),
    }
}

pub fn uninstall(args: &[String]) -> Result<(), String> {
    let mut delete_data = false;
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--delete-data" => delete_data = true,
            "--yes" | "-y" => yes = true,
            _ => return Err("Usage: ibara uninstall [--delete-data] [--yes]".into()),
        }
    }
    let me = person()?;
    if let Some(owner) = super::system::station_owner().filter(|o| *o != me.name) {
        return Err(format!("ibara on this computer belongs to {owner}; run ibara uninstall as {owner}."));
    }
    println!("This removes ibara from this computer: its services, the bar icon, access for your paired computers,");
    println!("the ibara, ibara-stream and ibara-view packages and setup's ibara and ibarad links in ~/.local/bin.");
    println!("Files you received stay in ~/Downloads/Ibara. cua-driver-bin stays installed, since other software may use it.");
    if delete_data {
        println!("With --delete-data it also deletes this computer's ibara keys, identity, pairings and history, and the viewer's settings and cache.");
    } else {
        println!("This computer's keys, pairings and history are kept for a later install (--delete-data removes them).");
    }
    if Path::new(super::unattended_boot::STATE).exists() {
        println!("This computer starts without its disk password, and keeps doing so without ibara. To ask for the password again, run ibara unattended-boot disable first.");
    }
    if !yes && !confirm("Remove ibara?")? {
        println!("Nothing was removed.");
        return Ok(());
    }

    println!("\nThe ibara bar icon:");
    unlink_plugin(&me);
    let _ = services(&["disable", "--now", USER_UNITS[0], USER_UNITS[1]]);
    remove_hyprland_include(&me)?;

    println!("\nThe next part needs your password once (sudo).");
    let mut root_args = vec!["uninstall", me.name.as_str()];
    if delete_data {
        root_args.push("--delete-data");
    }
    as_root(&root_args)?;

    remove_setup_links(&me);
    if delete_data {
        for path in super::HOME_DATA.iter().chain(&VIEW_DATA) {
            let path = me.home.join(path);
            let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            if let Err(e) = removed
                && e.kind() != std::io::ErrorKind::NotFound
            {
                println!("  Could not remove {}: {e}", path.display());
            }
        }
        // The viewer's folder, if its settings were all it held.
        let _ = std::fs::remove_dir(me.home.join(".config/Ibara"));
    }
    let _ = services(&["daemon-reload"]);
    println!("\nibara is removed from this computer. Agents you connected keep a server named ibara in their own settings,");
    println!("a link named ibara in their skills folder and an ibara block in their instructions file: ask each one to remove them.");
    Ok(())
}

/// Take Control's viewer's settings and cache, removed with --delete-data.
const VIEW_DATA: [&str; 2] = [".config/Ibara/ibara-view.conf", ".cache/Ibara"];

/// The `~/.local/bin` links setup makes (see `replace_old_copies`), only while
/// they still point at the package's programs.
fn remove_setup_links(me: &Account) {
    for (name, target) in [("ibara", "/usr/bin/ibara"), ("ibarad", "/usr/bin/ibarad")] {
        let link = me.home.join(".local/bin").join(name);
        if std::fs::read_link(&link).is_ok_and(|to| to == Path::new(target))
            && let Err(e) = std::fs::remove_file(&link)
        {
            println!("  Could not remove {}: {e}", link.display());
        }
    }
}

fn confirm(question: &str) -> Result<bool, String> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(0) } != 1 {
        return Err("Run ibara uninstall in a terminal to confirm, or add --yes.".into());
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn unlink_plugin(me: &Account) {
    let link = plugins_dir(me).join(PLUGIN_ID);
    if shell_running() {
        let _ = omarchy("omarchy-shell", &["shell", "setPluginEnabled", PLUGIN_ID, "false"]);
    }
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::remove_file(&link) {
            Ok(()) => println!("  Removed."),
            Err(e) => println!("  Could not remove {}: {e}", link.display()),
        },
        Ok(_) => println!("  {} is a separate copy; remove it with: omarchy plugin remove {PLUGIN_ID}", link.display()),
        Err(_) => println!("  Not installed."),
    }
    if shell_running() {
        let _ = omarchy("omarchy-shell", &["shell", "rescanPlugins"]);
    }
}

/// Take out the lines `cua-plugin.sh` added to `~/.config/hypr/hyprland.lua`.
pub fn remove_hyprland_include(me: &Account) -> Result<(), String> {
    let config = me.home.join(".config/hypr/hyprland.lua");
    let Ok(text) = std::fs::read_to_string(&config) else { return Ok(()) };
    if !text.contains(HYPRLAND_INCLUDE) {
        return Ok(());
    }
    let kept: Vec<&str> = text.lines().filter(|l| !l.contains(HYPRLAND_INCLUDE)).collect();
    let mut body = kept.join("\n").trim_end().to_string();
    body.push('\n');
    let temp = config.with_extension("lua.ibara-tmp");
    let mode = std::fs::metadata(&config).map(|m| m.permissions()).map_err(|e| format!("{}: {e}", config.display()))?;
    std::fs::write(&temp, body)
        .and_then(|()| std::fs::set_permissions(&temp, mode))
        .and_then(|()| std::fs::rename(&temp, &config))
        .map_err(|e| format!("{}: {e}", config.display()))?;
    println!("  Hyprland no longer loads Cua's plugin at sign-in.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ShellWaits, restart_shell_in, within};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // What can go wrong when ibara restarts Omarchy's shell to load a changed plugin:
    // 1. The old shell is slower to exit than Omarchy's restart waits (5 s). The new shell
    //    starts while the old one still holds Quickshell's instance lock and exits as
    //    "already running"; the old one answers the restart's ping, so the restart reports
    //    success; then the old one exits, leaving no shell and no bar.
    // 2. The shell does not start again even when ibara starts it once more: the person must
    //    be told, and ibara must not keep starting it.
    // 3. An ordinary restart gets a second start anyway, a second shell.
    // 4. Omarchy refuses to restart (the session is locked): ibara starts a second shell
    //    beside the old one, or the person never learns why.

    const WAITS: ShellWaits = ShellWaits { stop: Duration::from_secs(4), start: Duration::from_secs(2) };

    /// Stand-ins for Quickshell and Omarchy's shell commands that behave towards one another
    /// as the real ones do: one shell at a time (Quickshell's instance lock, `-n`), started
    /// from Hyprland by `omarchy-launch-shell`, answering `omarchy-shell shell ping` until it
    /// has exited, `exit-delay` seconds after it is asked to.
    const STAND_INS: &[(&str, &str)] = &[
        (
            "shell",
            r#"#!/bin/bash
d=${0%/*}
exec 9>>"$d/instance.lock"
flock -n 9 || { echo "An instance of this configuration is already running." >>"$d/log"; exit 0; }
[[ -e $d/broken ]] && exit 1
echo $$ >"$d/running"
trap 'sleep "$(<"$d/exit-delay")" 9>&-; rm -f "$d/running"; exit 0' TERM
for _ in {1..600}; do sleep 0.1 9>&-; done
rm -f "$d/running"
"#,
        ),
        (
            "omarchy-launch-shell",
            r#"#!/bin/bash
d=${0%/*}
echo started >>"$d/launches"
exec "$d/shell"
"#,
        ),
        (
            "hyprctl",
            r#"#!/bin/bash
d=${0%/*}
[[ $1 == dispatch && $2 == 'hl.dsp.exec_cmd("omarchy-launch-shell")' ]] || { echo "Invalid dispatcher" >&2; exit 1; }
setsid -f "$d/omarchy-launch-shell" </dev/null >/dev/null 2>&1
echo ok
"#,
        ),
        (
            "omarchy-shell",
            r#"#!/bin/bash
d=${0%/*}
[[ "$1 $2" == "shell ping" ]] || exit 1
pid=$(cat "$d/running" 2>/dev/null) && kill -0 "$pid" 2>/dev/null || { echo "omarchy-shell is not running" >&2; exit 1; }
echo ok
"#,
        ),
        (
            "qs",
            r#"#!/bin/bash
d=${0%/*}
if pid=$(cat "$d/running" 2>/dev/null) && kill -0 "$pid" 2>/dev/null; then echo "[{\"pid\": $pid}]"; else echo "No running instances"; fi
"#,
        ),
        (
            // Omarchy 4's, with its 5 s wait for the old shell as `kill-wait`.
            "omarchy-restart-shell",
            r#"#!/bin/bash
d=${0%/*}
[[ -e $d/locked ]] && { echo "Refusing to restart Omarchy shell while the session is locked." >&2; exit 1; }
if pid=$(cat "$d/running" 2>/dev/null); then
  kill -TERM "$pid"
  timeout "$(<"$d/kill-wait")" tail --pid="$pid" -f /dev/null
fi
"$d/hyprctl" dispatch 'hl.dsp.exec_cmd("omarchy-launch-shell")' >/dev/null
for _ in {1..20}; do "$d/omarchy-shell" shell ping >/dev/null 2>&1 && exit 0; sleep 0.1; done
echo "Omarchy shell did not become ready after restart." >&2
exit 1
"#,
        ),
    ];

    /// A desktop of stand-ins with its shell running.
    struct Desktop(PathBuf);

    impl Desktop {
        /// Its shell takes `exit_delay` seconds to exit; Omarchy's restart waits `kill_wait`.
        fn new(name: &str, exit_delay: &str, kill_wait: &str) -> Desktop {
            let dir = std::env::temp_dir().join(format!("ibara-shell-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (file, body) in STAND_INS {
                std::fs::write(dir.join(file), body).unwrap();
                std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::fs::write(dir.join("exit-delay"), exit_delay).unwrap();
            std::fs::write(dir.join("kill-wait"), kill_wait).unwrap();
            let desktop = Desktop(dir);
            let started = Command::new("setsid").arg("-f").arg(desktop.0.join("shell")).stdin(Stdio::null()).stdout(Stdio::null()).status();
            assert!(started.unwrap().success());
            assert!(within(Duration::from_secs(5), || desktop.shell().is_some()), "the first shell started");
            desktop
        }

        fn find(&self) -> impl Fn(&str) -> Option<PathBuf> + '_ {
            |name| Some(self.0.join(name)).filter(|p| p.is_file())
        }

        /// The shell that is running now.
        fn shell(&self) -> Option<i32> {
            let pid = std::fs::read_to_string(self.0.join("running")).ok()?.trim().parse().ok()?;
            super::alive(pid).then_some(pid)
        }

        /// How many times `omarchy-launch-shell` ran.
        fn launches(&self) -> usize {
            std::fs::read_to_string(self.0.join("launches")).map(|l| l.lines().count()).unwrap_or(0)
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.0.join("log")).unwrap_or_default()
        }
    }

    impl Drop for Desktop {
        fn drop(&mut self) {
            if let Some(pid) = self.shell() {
                // SAFETY: kill only sends a signal to this test's own stand-in.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_new_shell_that_exits_as_already_running_is_started_again_once_the_old_one_is_gone() {
        // The old shell takes 1.5 s to exit; Omarchy's restart waits 0.3 s for it.
        let desktop = Desktop::new("slow", "1.5", "0.3");
        let old = desktop.shell().unwrap();
        assert_eq!(restart_shell_in(&desktop.find(), &WAITS), Ok(true));
        let new = desktop.shell().expect("a shell runs after the restart");
        assert_ne!(new, old);
        assert!(desktop.log().contains("already running"), "Omarchy's own new shell met the old one's lock");
        assert_eq!(desktop.launches(), 2, "Omarchy's start, then ibara's one more");
    }

    #[test]
    fn a_shell_that_does_not_start_again_is_reported_after_one_more_start() {
        let desktop = Desktop::new("broken", "0.2", "5");
        std::fs::write(desktop.0.join("broken"), "").unwrap();
        let message = restart_shell_in(&desktop.find(), &WAITS).expect_err("no shell runs");
        assert!(message.contains("no bar") && message.ends_with("Run: omarchy restart shell"), "{message}");
        assert_eq!(desktop.shell(), None);
        assert_eq!(desktop.launches(), 2, "Omarchy's start, then ibara's one more, not a loop");
    }

    #[test]
    fn an_ordinary_restart_starts_one_new_shell() {
        let desktop = Desktop::new("ordinary", "0.2", "5");
        let old = desktop.shell().unwrap();
        assert_eq!(restart_shell_in(&desktop.find(), &WAITS), Ok(false));
        assert_ne!(desktop.shell().expect("a shell runs after the restart"), old);
        assert_eq!(desktop.launches(), 1);
    }

    #[test]
    fn a_refused_restart_leaves_the_old_shell_alone_and_says_why() {
        let desktop = Desktop::new("locked", "0.2", "5");
        std::fs::write(desktop.0.join("locked"), "").unwrap();
        let old = desktop.shell();
        let message = restart_shell_in(&desktop.find(), &WAITS).expect_err("Omarchy refused");
        assert!(message.contains("Refusing to restart Omarchy shell while the session is locked."), "{message}");
        assert_eq!(desktop.shell(), old);
        assert_eq!(desktop.launches(), 0);
    }
}
