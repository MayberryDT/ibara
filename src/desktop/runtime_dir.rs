//! The session's runtime directory (`$XDG_RUNTIME_DIR`), a small tmpfs
//! (logind gives it 10% of RAM). When it fills, the user's service manager
//! cannot write transient units and every app launch fails; one computer sat
//! `control_unsettled` for hours that way on 17 November 2026
//! (`docs/runtime-dir-failure-cases.md`). This reports the space, and reclaims
//! the one thing in it that is provably abandoned: the logs Quickshell keeps
//! for instances that have exited (they stay until logout).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Below this, launches fail now.
pub const FULL_BYTES: u64 = 1024 * 1024;
/// Low: below a tenth of the directory, and never less than this.
const LOW_FLOOR_BYTES: u64 = 32 * 1024 * 1024;
/// An instance this new may still be taking its lock.
const STARTING: Duration = Duration::from_secs(60);

/// This process's session runtime directory, when it has one.
pub fn session_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?);
    dir.is_dir().then_some(dir)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub total: u64,
}

impl Space {
    pub fn low(&self) -> bool {
        self.free < LOW_FLOOR_BYTES.max(self.total / 10)
    }

    pub fn full(&self) -> bool {
        self.free < FULL_BYTES
    }
}

pub fn space(dir: &Path) -> Option<Space> {
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct we own; the path is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    Some(Space { free: block.saturating_mul(stat.f_bavail as u64), total: block.saturating_mul(stat.f_blocks as u64) })
}

/// The `storage.runtime` capability row, when this process has a runtime directory.
pub fn capability(dir: &Path, tested: &str) -> serde_json::Value {
    let mib = |b: u64| b / (1024 * 1024);
    let mut row = serde_json::json!({ "name": "storage.runtime", "status": "available", "backend": "statvfs", "last_tested_at": tested });
    match space(dir) {
        Some(space) if space.low() => {
            row["status"] = "degraded".into();
            row["reason"] = format!(
                "The session runtime directory {} has {} MiB free of {} MiB; apps cannot start when it is full. Closed shells' logs there are removed automatically; the rest belongs to running apps.",
                dir.display(), mib(space.free), mib(space.total)).into();
        }
        Some(_) => {}
        None => {
            row["status"] = "degraded".into();
            row["reason"] = format!("The session runtime directory {} could not be measured.", dir.display()).into();
        }
    }
    row
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// The removed instance ids.
    pub instances: Vec<String>,
    pub bytes: u64,
}

/// Remove the log directories of Quickshell instances that have exited:
/// `quickshell/by-id/<id>` owned by this user, whose `instance.lock` no
/// process holds (Quickshell's own liveness test) and is over a minute old,
/// and the `by-pid`/`by-shell`/`by-path` links to them. Symlinks are never
/// followed. Call it only when [`Space::low`]; otherwise these logs stay
/// readable with `qs log`.
pub fn reclaim_closed_shells(dir: &Path) -> Reclaimed {
    let root = dir.join("quickshell");
    let mut done = Reclaimed::default();
    let Ok(entries) = std::fs::read_dir(root.join("by-id")) else { return done };
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() || meta.uid() != me || !exited(&path.join("instance.lock"), me) {
            continue;
        }
        let bytes = flat_size(&path);
        if std::fs::remove_dir_all(&path).is_ok() {
            done.bytes += bytes;
            done.instances.push(entry.file_name().to_string_lossy().into_owned());
            removed.push(path);
        }
    }
    if !removed.is_empty() {
        for index in ["by-pid", "by-shell", "by-path"] {
            unlink_to(&root.join(index), &removed, 2);
        }
    }
    done
}

/// No process holds a lock on this regular, settled lock file.
fn exited(lock: &Path, me: u32) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(lock) else { return false };
    if !meta.is_file() || meta.uid() != me {
        return false;
    }
    if meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_none_or(|age| age < STARTING) {
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(lock) else {
        return false;
    };
    // SAFETY: F_GETLK fills the flock we own; it only tests, never locks.
    let mut probe: libc::flock = unsafe { std::mem::zeroed() };
    probe.l_type = libc::F_WRLCK as libc::c_short;
    probe.l_whence = libc::SEEK_SET as libc::c_short;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) } != 0 {
        return false;
    }
    probe.l_type == libc::F_UNLCK as libc::c_short
}

/// Bytes in the regular files directly inside `dir` (instance directories are flat).
fn flat_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map(|entries| entries.flatten()
        .filter_map(|e| std::fs::symlink_metadata(e.path()).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()).unwrap_or(0)
}

/// Remove symlinks under `dir` (to `depth` levels) that point at a removed instance.
fn unlink_to(dir: &Path, removed: &[PathBuf], depth: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_symlink() {
            if std::fs::read_link(&path).is_ok_and(|target| removed.iter().any(|r| target == *r || (target.file_name() == r.file_name() && !target.exists()))) {
                let _ = std::fs::remove_file(&path);
            }
        } else if meta.is_dir() && depth > 1 {
            unlink_to(&path, removed, depth - 1);
        }
    }
}
