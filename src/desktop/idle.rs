//! Idle inhibition and session-lock detection.
//!
//! ibara keeps a computer that takes agent work awake for good, through
//! Omarchy's `omarchy-toggle-idle` ([`Idle::keep_awake`] at start), so the
//! screensaver and idle lock never shut agents out. Stay-awake it turns on
//! for a single agent lease is recorded in `idle-owned.json` and undone at the
//! lease's end; stay-awake it did not turn on for a lease is never undone.

use super::hyprland::Monitor;
use super::run::{Cmd, run};
use crate::error::{IbaraError, Result};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Locked,
    Unlocked,
    Unknown,
}

/// Lock state from `monitors` (`sessionLockState`): no monitors, or any
/// monitor without `solitaryBlockedBy`, is unknown; `LOCK` on any monitor is
/// locked; unlocked needs at least one monitor not blocked by `WORKSPACE`.
pub fn lock_state(monitors: &[Monitor]) -> LockState {
    if monitors.is_empty() {
        return LockState::Unknown;
    }
    let mut readable = false;
    for monitor in monitors {
        let Some(blockers) = &monitor.solitary_blocked_by else {
            return LockState::Unknown;
        };
        if blockers.iter().any(|b| b == "LOCK") {
            return LockState::Locked;
        }
        if !blockers.iter().any(|b| b == "WORKSPACE") {
            readable = true;
        }
    }
    if readable { LockState::Unlocked } else { LockState::Unknown }
}

/// `assertSessionReady`: unlocked, or the reason work cannot start.
pub fn require_unlocked(monitors: Result<Vec<Monitor>>) -> Result<()> {
    let monitors = monitors.map_err(|_| {
        IbaraError::new("SESSION_UNAVAILABLE", "Hyprland session lock state could not be read.", true)
            .with("recovery", "Inspect the graphical session, then retry computer_begin.")
    })?;
    match lock_state(&monitors) {
        LockState::Unlocked => Ok(()),
        LockState::Locked => Err(IbaraError::new(
            "HUMAN_CONTROL",
            "The graphical session is locked and requires a human unlock.",
            false,
        )
        .with("recovery", "Unlock the session at the console; ibara will not enter credentials.")),
        LockState::Unknown => Err(IbaraError::new(
            "SESSION_UNAVAILABLE",
            "Session lock state is unknown; no readable Hyprland lock evidence.",
            true,
        )
        .with("recovery", "Inspect capability diagnostics; do not assume an unlocked compositor.")),
    }
}

/// A person's Take Control: unlocked, or locked (they unlock it through the
/// viewer); only an unreadable lock state refuses.
pub fn require_known(monitors: Result<Vec<Monitor>>) -> Result<()> {
    match require_unlocked(monitors) {
        Err(e) if e.code == "HUMAN_CONTROL" => Ok(()),
        other => other,
    }
}

/// `omarchy-toggle-idle status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IdleStatus {
    /// Stay-awake is on (idle locking is inhibited).
    pub enabled: bool,
    pub class: Option<String>,
    pub tooltip: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Idle {
    binary: PathBuf,
    state_path: PathBuf,
    env: Arc<[(OsString, OsString)]>,
}

impl Idle {
    pub fn new(binary: PathBuf, state_path: PathBuf, env: Arc<[(OsString, OsString)]>) -> Self {
        Idle { binary, state_path, env }
    }

    async fn call(&self, arg: &str) -> Result<Vec<u8>> {
        let out = run(Cmd::new(&self.binary).arg(arg).envs(&self.env).timeout(IDLE_TIMEOUT)).await?;
        if !out.success() {
            let message = if arg == "status" {
                "Idle inhibitor status is unavailable.".to_string()
            } else {
                format!("Idle inhibitor {arg} failed.")
            };
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", message, true));
        }
        Ok(out.stdout)
    }

    pub async fn status(&self) -> Result<IdleStatus> {
        let raw = self.call("status").await?;
        serde_json::from_slice(&raw)
            .map_err(|_| IbaraError::new("CAPABILITY_UNAVAILABLE", "Idle inhibitor status was not JSON.", true))
    }

    /// Turn stay-awake on for good unless it already is, owned by nobody, so
    /// no lease's end turns it off. Whether it turned it on now.
    pub async fn keep_awake(&self) -> Result<bool> {
        let turn_on = !self.status().await?.enabled;
        if turn_on {
            self.call("stay-awake").await?;
        }
        self.write_owned(false).await?;
        Ok(turn_on)
    }

    /// Begin (`true`): turn stay-awake on unless it already is, and record
    /// that ibara owns it. End (`false`): only if ibara owns it, turn it off
    /// if still on, and give up ownership.
    pub async fn set_inhibited(&self, active: bool) -> Result<()> {
        let current = self.status().await?;
        if active {
            if current.enabled {
                return Ok(());
            }
            self.call("stay-awake").await?;
            return self.write_owned(true).await;
        }
        if !self.read_owned().await {
            return Ok(());
        }
        if self.status().await?.enabled {
            self.call("allow-idle").await?;
        }
        self.write_owned(false).await
    }

    async fn read_owned(&self) -> bool {
        let Ok(raw) = tokio::fs::read(&self.state_path).await else {
            return false;
        };
        serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("owned").and_then(|o| o.as_bool()))
            .unwrap_or(false)
    }

    async fn write_owned(&self, owned: bool) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        use std::io::Write;
        let path = self.state_path.clone();
        let body = serde_json::json!({"owned": owned, "updated_at": crate::ids::now_iso()}).to_string();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            let mut tmp = path.clone().into_os_string();
            tmp.push(".tmp");
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)
        })
        .await
        .map_err(|e| crate::error::internal(format!("idle state write: {e}")))??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(blockers: Option<&[&str]>) -> Monitor {
        Monitor { solitary_blocked_by: blockers.map(|b| b.iter().map(|s| s.to_string()).collect()), ..Monitor::default() }
    }

    #[test]
    fn lock_state_needs_positive_evidence() {
        assert_eq!(lock_state(&[]), LockState::Unknown);
        assert_eq!(lock_state(&[monitor(Some(&["WINDOWED", "CANDIDATE"]))]), LockState::Unlocked);
        assert_eq!(lock_state(&[monitor(Some(&["WINDOWED"])), monitor(Some(&["LOCK"]))]), LockState::Locked);
        assert_eq!(lock_state(&[monitor(Some(&["WORKSPACE"]))]), LockState::Unknown);
        assert_eq!(lock_state(&[monitor(Some(&["WINDOWED"])), monitor(None)]), LockState::Unknown);
        assert_eq!(require_unlocked(Ok(vec![monitor(Some(&["LOCK"]))])).unwrap_err().code, "HUMAN_CONTROL");
    }

    /// A fake `omarchy-toggle-idle` keeping its flag in a file, like the real one.
    fn fake_idle(dir: &std::path::Path, enabled: bool) -> PathBuf {
        let flag = dir.join("stay-awake");
        if enabled {
            std::fs::write(&flag, "").unwrap();
        }
        let script = dir.join("toggle-idle");
        super::super::write_script(
            &script,
            &format!(
                "#!/bin/sh\nflag='{}'\ncase \"$1\" in\n status) if [ -e \"$flag\" ]; then echo '{{\"enabled\":true}}'; else echo '{{\"enabled\":false}}'; fi;;\n stay-awake) touch \"$flag\";;\n allow-idle) rm -f \"$flag\";;\n *) exit 2;;\nesac\n",
                flag.display()
            ),
        );
        script
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-idle-test"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_persons_stay_awake_is_never_turned_off() {
        let dir = temp_dir();
        let idle = Idle::new(fake_idle(&dir, true), dir.join("state/idle-owned.json"), Arc::from(Vec::new()));
        idle.set_inhibited(true).await.unwrap();
        idle.set_inhibited(false).await.unwrap();
        assert!(idle.status().await.unwrap().enabled);
        assert!(!dir.join("state/idle-owned.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stay_awake_ibara_turned_on_is_turned_off_again() {
        let dir = temp_dir();
        let idle = Idle::new(fake_idle(&dir, false), dir.join("state/idle-owned.json"), Arc::from(Vec::new()));
        idle.set_inhibited(true).await.unwrap();
        assert!(idle.status().await.unwrap().enabled);
        idle.set_inhibited(false).await.unwrap();
        assert!(!idle.status().await.unwrap().enabled);
        let owned: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("state/idle-owned.json")).unwrap()).unwrap();
        assert_eq!(owned["owned"], serde_json::json!(false));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
