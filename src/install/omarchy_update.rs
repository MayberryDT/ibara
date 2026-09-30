//! Omarchy's own update, with nobody at the computer (power action
//! `update_omarchy`).
//!
//! The root power helper starts `ibara system omarchy-update` as the transient
//! unit [`UNIT`], so the run outlives ibarad and the helper. `omarchy update`
//! calls `sudo` throughout, so for the run only the desktop account may use
//! sudo without a password ([`SUDOERS`], checked with `visudo`). The file goes
//! when the run ends or fails; when the unit is stopped (its `ExecStopPost`
//! runs `ibara system omarchy-update-end`); on every power-helper request and
//! every setup when no run holds [`LOCK`]; and at boot (`packaging/ibara.tmpfiles`).
//! `omarchy-update -y` runs as the desktop account in its session, the way
//! `ibara system update-latest` runs its account's part, with no terminal, so
//! nothing on the screen waits for an answer. Its output goes to the journal.
//! The last run's result is [`STATE`], which operator status reports.

use super::update::{as_desktop, refuse_while_busy};
use super::{Account, root_dir, write_root};
use serde_json::{Value, json};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The unit a remote Omarchy update runs in (`ibara power-system` starts it).
pub const UNIT: &str = "ibara-omarchy-update.service";
/// The desktop account's sudo without a password, for one run. Sorted last, so
/// it wins over Omarchy's own sudoers files.
pub const SUDOERS: &str = "/etc/sudoers.d/zz-ibara-omarchy-update";
/// The last run: `{state, started_at, finished_at, restart_needed, message, boot_id}`.
const STATE: &str = "/var/lib/ibara/omarchy-update.json";
/// Held by a run from start to end; free means no run needs [`SUDOERS`].
const LOCK: &str = "/run/ibara-omarchy-update.lock";

fn lock(wait: bool) -> Option<File> {
    let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(LOCK).ok()?;
    let how = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    // SAFETY: flock on a descriptor this process owns.
    (unsafe { libc::flock(file.as_raw_fd(), how) } == 0).then_some(file)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

fn remove_sudoers() {
    match std::fs::remove_file(SUDOERS) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => eprintln!("Could not remove {SUDOERS}: {e}"),
        _ => {}
    }
}

/// Remove [`SUDOERS`] when no run is using it (a run that crashed left it).
pub fn clear_leftover_sudoers() {
    if std::fs::symlink_metadata(SUDOERS).is_ok() && lock(false).is_some() {
        remove_sudoers();
    }
}

fn allow_sudo(desktop: &Account) -> Result<(), String> {
    let rule = format!(
        "# Written by ibara while Omarchy's update runs, asked for from another computer.\n\
         # ibara removes it when the update ends.\n{} ALL=(ALL) NOPASSWD: ALL\n",
        desktop.name
    );
    let mut check = Command::new("/usr/bin/visudo")
        .args(["-c", "-q", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("visudo could not start ({e})."))?;
    let written = check.stdin.take().map(|mut stdin| stdin.write_all(rule.as_bytes()));
    let valid = check.wait().is_ok_and(|s| s.success());
    if !valid || !matches!(written, Some(Ok(()))) {
        return Err("visudo did not accept the sudo rule for the update, so nothing was changed.".into());
    }
    write_root(Path::new(SUDOERS), rule.as_bytes(), 0o440)
}

fn record(state: &Value) {
    let written = root_dir(Path::new(STATE).parent().unwrap_or(Path::new("/var/lib")), 0o755)
        .and_then(|()| write_root(Path::new(STATE), format!("{state}\n").as_bytes(), 0o644));
    if let Err(e) = written {
        eprintln!("{e}");
    }
}

/// Whether the computer must restart to finish, by Omarchy's own rule
/// (`omarchy-update-restart`): the running kernel's files are gone, Omarchy
/// asked for a reboot, or the running Hyprland was replaced.
fn restart_needed(desktop: &Account) -> bool {
    let running = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let kernel_gone = !Path::new("/usr/lib/modules").join(running.trim()).join("vmlinuz").is_file();
    let asked = desktop.home.join(".local/state/omarchy/reboot-required").exists();
    kernel_gone || asked || hyprland_replaced(desktop.uid)
}

fn hyprland_replaced(uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    entries.flatten().any(|entry| {
        let dir = entry.path();
        std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == "Hyprland")
            && entry.metadata().is_ok_and(|m| m.uid() == uid)
            && std::fs::read_link(dir.join("exe")).is_ok_and(|exe| exe.to_string_lossy().ends_with(" (deleted)"))
    })
}

/// Root, in [`UNIT`] (`ibara system omarchy-update`): Omarchy's update for the
/// desktop account named in the station file.
pub fn system_omarchy_update() -> Result<(), String> {
    let _held = lock(true).ok_or("Could not take the Omarchy update lock.")?;
    let owner = super::system::station_owner().ok_or("ibara is not set up on this computer, so there is nobody to update Omarchy for.")?;
    let desktop = Account::desktop(&owner)?;
    let mut state = json!({
        "state": "running", "started_at": crate::ids::now_millis(), "finished_at": null,
        "restart_needed": false, "message": null, "boot_id": boot_id(),
    });
    record(&state);
    let result = (|| {
        refuse_while_busy()?;
        allow_sudo(&desktop)?;
        println!("Updating Omarchy for {}, asked for from another computer.", desktop.name);
        if as_desktop(&desktop, omarchy_update) {
            Ok(())
        } else {
            Err(format!("Omarchy's update stopped before it finished. Its output is in the journal: journalctl -u {UNIT}"))
        }
    })();
    remove_sudoers();
    state["finished_at"] = json!(crate::ids::now_millis());
    match &result {
        Ok(()) => {
            state["state"] = json!("done");
            state["restart_needed"] = json!(restart_needed(&desktop));
            println!("Omarchy's update finished. {}", if state["restart_needed"] == json!(true) { "A restart is needed." } else { "No restart is needed." });
        }
        Err(message) => {
            state["state"] = json!("failed");
            state["message"] = json!(message);
        }
    }
    record(&state);
    result
}

/// In the child, as the desktop account: `omarchy-update -y`, without the
/// `script` wrapper it adds for a terminal (its output goes to the journal).
fn omarchy_update() -> Result<(), String> {
    let program = super::program("omarchy-update").ok_or("Omarchy's updater is not on this computer.")?;
    let status = Command::new(program)
        .arg("-y")
        .env("OMARCHY_UPDATE_LOGGED", "1")
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("omarchy-update could not start ({e})."))?;
    if status.success() { Ok(()) } else { Err(format!("omarchy-update -y ended with {status}.")) }
}

/// Root, the unit's `ExecStopPost` (`ibara system omarchy-update-end`): the
/// sudo rule goes, and a run stopped part way is recorded as failed.
pub fn system_omarchy_update_end() -> Result<(), String> {
    remove_sudoers();
    if let Some(mut state) = std::fs::read(STATE).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        && state["state"] == "running"
    {
        state["state"] = json!("failed");
        state["finished_at"] = json!(crate::ids::now_millis());
        state["message"] = json!("Omarchy's update was stopped before it finished.");
        record(&state);
    }
    Ok(())
}

/// The last run, for operator status; null when there has been none. A run
/// from before the computer last started no longer needs a restart, and one
/// still marked running then never finished.
pub fn status() -> Value {
    let Some(mut state) = std::fs::read(STATE).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Value::Null };
    let Some(fields) = state.as_object_mut() else { return Value::Null };
    let same_boot = fields.remove("boot_id").and_then(|b| b.as_str().map(str::to_string)) == Some(boot_id());
    if !same_boot {
        fields.insert("restart_needed".into(), json!(false));
        if fields.get("state") == Some(&json!("running")) {
            fields.insert("state".into(), json!("failed"));
            fields.insert("message".into(), json!("The computer restarted before Omarchy's update finished."));
        }
    }
    state
}
