//! Replay pictures: each desktop step keeps a small
//! before-and-after crop of the window it acted on, taken around the step's
//! dispatch and expectation.
//!
//! - Files live in `<state>/replay/`, named `<ms>-<keep>-<operation>-<phase>.<ext>`.
//!   `keep` is `s` (14 days) or `l` (90 days: send, spend, destructive and access
//!   steps, and steps whose outcome was unknown or failed).
//! - The cap is 1 GiB or 5 % of the file system, whichever is smaller; the
//!   oldest pictures go first when it is reached, whatever their age.
//! - Steps that type into a window showing a password field keep no picture.
//!   Typed text is never stored (receipts keep its SHA-256 only).
//! - The console's Activity tab shows them; this module only records and prunes.

use super::Controller;
use super::ports::{Capture, Image, WinKey};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Largest encoded crop kept per phase.
const MAX_BYTES: usize = 48 * 1024;
const SHORT: Duration = Duration::from_secs(14 * 24 * 3600);
const LONG: Duration = Duration::from_secs(90 * 24 * 3600);
const CAP: u64 = 1024 * 1024 * 1024;
/// Pruning runs at most this often.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

pub(crate) struct Replay {
    dir: PathBuf,
    pruned_at: std::cell::Cell<Option<std::time::Instant>>,
}

impl Replay {
    pub fn new(state_dir: &Path) -> Replay {
        Replay { dir: state_dir.join("replay"), pruned_at: std::cell::Cell::new(None) }
    }
}

impl Controller {
    /// A small crop of the window a step acts on, or `None` when it cannot be
    /// taken (hidden, gone, no session). Never fails the step.
    pub(crate) async fn replay_capture(&self, surface: Option<&WinKey>) -> Option<Image> {
        let key = match surface {
            Some(k) => k.clone(),
            None => self.desktop.windows().await.ok()?.into_iter().find(|w| w.focused)?.key(),
        };
        self.desktop.capture(&Capture::Replay(key), MAX_BYTES).await.ok()
    }

    /// Write the step's pictures; returns their file names for the timeline.
    pub(crate) fn replay_save(&self, operation: &str, before: Option<Image>, after: Option<Image>, long: bool) -> Vec<String> {
        let replay = &self.replay;
        if std::fs::create_dir_all(&replay.dir).is_err() {
            return Vec::new();
        }
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let keep = if long { "l" } else { "s" };
        let mut names = Vec::new();
        for (phase, image) in [("before", before), ("after", after)] {
            let Some(image) = image else { continue };
            let ext = if image.mime == "image/webp" { "webp" } else { "jpg" };
            let name = format!("{ms}-{keep}-{operation}-{phase}.{ext}");
            let path = replay.dir.join(&name);
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f| f.write_all(&image.bytes));
            if written.is_ok() {
                names.push(name);
            }
        }
        self.replay_prune(false);
        names
    }

    /// Age limits and the size cap. `force` ignores the hourly throttle.
    pub(crate) fn replay_prune(&self, force: bool) {
        let replay = &self.replay;
        if !force && replay.pruned_at.get().is_some_and(|at| at.elapsed() < PRUNE_EVERY) {
            return;
        }
        replay.pruned_at.set(Some(std::time::Instant::now()));
        let Ok(entries) = std::fs::read_dir(&replay.dir) else { return };
        let now = SystemTime::now();
        let mut kept: Vec<(u128, u64, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = name.splitn(3, '-');
            let (Some(ms), Some(keep)) = (parts.next().and_then(|m| m.parse::<u128>().ok()), parts.next()) else { continue };
            let limit = if keep == "l" { LONG } else { SHORT };
            let age = now
                .duration_since(UNIX_EPOCH + Duration::from_millis(ms as u64))
                .unwrap_or_default();
            if age > limit {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            kept.push((ms, size, entry.path()));
        }
        let cap = filesystem_bytes(&replay.dir).map_or(CAP, |total| (total / 20).min(CAP));
        let mut used: u64 = kept.iter().map(|(_, size, _)| size).sum();
        if used <= cap {
            return;
        }
        kept.sort_by_key(|(ms, _, _)| *ms);
        for (_, size, path) in kept {
            if used <= cap {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                used = used.saturating_sub(size);
            }
        }
    }
}

/// The size of the file system holding `path`.
fn filesystem_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `stat` a writable struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_blocks as u64 * stat.f_frsize as u64)
}
