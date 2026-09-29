//! Private, consistent backups of the controller state. Never restores.
//!
//! A port of `controller/ops/backup-journals.py`, extended with the small
//! controller state files a cutover rollback needs:
//!
//! ```text
//! <destination>/journal.sqlite          0600, SQLite online backup (WAL-safe), integrity_check = ok
//! <destination>/storage.sqlite          0600, same
//! <destination>/operator-authority.json 0600
//! <destination>/operator-keys/          0700, files 0600
//! <destination>/operator-challenges/    0700, files 0600
//! <destination>/idle-owned.json         0600
//! <destination>/release.json            0600, {"previous_release": …, "restore_policy": …}
//! ```
//!
//! Sources that do not exist are skipped; symlinks and non-regular files are refused.

use crate::error::{IbaraError, Result, internal, invalid};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const DATABASES: &[&str] = &["journal.sqlite", "storage.sqlite"];
const STATE_FILES: &[&str] = &["operator-authority.json", "idle-owned.json"];
const STATE_DIRS: &[&str] = &["operator-keys", "operator-challenges"];
/// State directories hold small key and challenge files; anything larger is refused.
const MAX_STATE_FILE_BYTES: u64 = 1024 * 1024;
const RESTORE_POLICY: &str =
    "Preserve current receipts on code rollback. Restoring historical journals requires explicit reconciliation of later effects.";

#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub directory: PathBuf,
    /// Paths written, relative to `directory`.
    pub files: Vec<String>,
}

/// Create `dest_root` (0700) and `dest_root/<name>` (0700, must not exist), then [`backup_into`].
pub fn backup_journals(source_state_dir: &Path, dest_root: &Path, name: &str, previous_release: &str) -> Result<BackupReport> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b));
    if !valid {
        return Err(invalid("Backup name must match ^[A-Za-z0-9_.:-]{1,128}$."));
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dest_root)?;
    let destination = dest_root.join(name);
    fs::DirBuilder::new().mode(0o700).create(&destination).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            invalid(format!("Backup directory {} already exists.", destination.display()))
        } else {
            e.into()
        }
    })?;
    backup_into(source_state_dir, &destination, previous_release)
}

/// `backup-journals.py <source_state_dir> <destination_dir> <previous_release>`:
/// `destination_dir` must already exist; no file in it is overwritten.
pub fn backup_into(source_state_dir: &Path, destination: &Path, previous_release: &str) -> Result<BackupReport> {
    let meta = fs::symlink_metadata(destination)?;
    if !meta.is_dir() {
        return Err(invalid("Backup destination must be a directory."));
    }
    let mut files = Vec::new();
    for name in DATABASES {
        let source = source_state_dir.join(name);
        if !regular_source(&source)? {
            continue;
        }
        backup_database(&source, &destination.join(name))?;
        files.push(name.to_string());
    }
    for name in STATE_FILES {
        let source = source_state_dir.join(name);
        if regular_source(&source)? {
            copy_private(&source, &destination.join(name))?;
            files.push(name.to_string());
        }
    }
    for dir in STATE_DIRS {
        let source = source_state_dir.join(dir);
        let meta = match fs::symlink_metadata(&source) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if !meta.is_dir() {
            return Err(internal(format!("Refusing nondirectory state source {}", source.display())));
        }
        let target_dir = destination.join(dir);
        fs::DirBuilder::new().mode(0o700).create(&target_dir)?;
        files.push(format!("{dir}/"));
        let mut entries: Vec<_> = fs::read_dir(&source)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if !regular_source(&path)? {
                continue;
            }
            copy_private(&path, &target_dir.join(entry.file_name()))?;
            files.push(format!("{dir}/{}", entry.file_name().to_string_lossy()));
        }
        fsync_dir(&target_dir)?;
    }
    let release = format!(
        "{{\"previous_release\": {}, \"restore_policy\": {}}}\n",
        python_json_string(previous_release),
        python_json_string(RESTORE_POLICY)
    );
    let mut out = create_private(&destination.join("release.json"))?;
    out.write_all(release.as_bytes())?;
    out.sync_all()?;
    files.push("release.json".into());
    fsync_dir(destination)?;
    Ok(BackupReport { directory: destination.to_path_buf(), files })
}

/// Exists and is a regular file; a symlink or other type is refused.
fn regular_source(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Ok(true),
        Ok(_) => Err(internal(format!("Refusing nonregular state source {}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn create_private(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?)
}

/// SQLite online backup of `source` (opened `mode=ro`) into a new 0600 file,
/// then `PRAGMA integrity_check` must be `ok`, then fsync.
fn backup_database(source: &Path, target: &Path) -> Result<()> {
    drop(create_private(target)?);
    let uri = format!("file:{}?mode=ro", uri_path(source));
    let original = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let mut copy = Connection::open(target)?;
    {
        let backup = rusqlite::backup::Backup::new(&original, &mut copy)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(10), None)?;
    }
    let check: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(IbaraError::new("INTERNAL_ERROR", "Backup integrity check failed", false));
    }
    copy.close().map_err(|(_, e)| IbaraError::from(e))?;
    File::open(target)?.sync_all()?;
    Ok(())
}

/// Percent-encode characters that are special in a SQLite URI path.
fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    for b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'%' | b'?' | b'#' => out.push_str(&format!("%{b:02X}")),
            _ => out.push(*b as char),
        }
    }
    // Non-UTF-8 bytes above were pushed as Latin-1 chars; re-encode them as escapes.
    out.chars()
        .map(|c| if (c as u32) > 0x7f { format!("%{:02X}", c as u32) } else { c.to_string() })
        .collect()
}

fn copy_private(source: &Path, target: &Path) -> Result<()> {
    let mut input = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(source)?;
    if input.metadata()?.len() > MAX_STATE_FILE_BYTES {
        return Err(internal(format!("Refusing oversized state file {}", source.display())));
    }
    let mut output = create_private(target)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Python `json.dumps(str)` with its default `ensure_ascii=True`.
fn python_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for unit in s.encode_utf16() {
        match unit {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x20..=0x7e => out.push(unit as u8 as char),
            _ => out.push_str(&format!("\\u{unit:04x}")),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // Failure cases: WAL-resident rows missing from the copy; copies readable by
    // group/other; an existing backup overwritten; a symlinked source followed;
    // release.json differing from the Python bytes.

    #[test]
    fn backup_copies_wal_resident_rows_and_state_files_privately() {
        let tmp = tempdir();
        let state = tmp.join("state");
        fs::create_dir_all(state.join("operator-keys")).unwrap();
        let db = Connection::open(state.join("journal.sqlite")).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        db.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (42);").unwrap();
        fs::write(state.join("operator-authority.json"), "{}").unwrap();
        fs::write(state.join("operator-keys/vesper.key"), "ab").unwrap();

        let report = backup_journals(&state, &tmp.join("backups"), "update-test", "releases/r1").unwrap();
        drop(db);
        let dir = tmp.join("backups/update-test");
        assert_eq!(report.files, ["journal.sqlite", "operator-authority.json", "operator-keys/", "operator-keys/vesper.key", "release.json"]);
        let copy = Connection::open(dir.join("journal.sqlite")).unwrap();
        let x: i64 = copy.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 42);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("journal.sqlite")), 0o600);
        assert_eq!(mode(&dir.join("operator-keys")), 0o700);
        assert_eq!(mode(&dir.join("operator-keys/vesper.key")), 0o600);
        assert_eq!(mode(&dir.join("release.json")), 0o600);
        assert_eq!(
            fs::read_to_string(dir.join("release.json")).unwrap(),
            format!("{{\"previous_release\": \"releases/r1\", \"restore_policy\": \"{RESTORE_POLICY}\"}}\n")
        );
        assert!(backup_journals(&state, &tmp.join("backups"), "update-test", "r1").is_err(), "existing backup is never overwritten");
    }

    #[test]
    fn backup_refuses_symlinked_sources() {
        let tmp = tempdir();
        let state = tmp.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(tmp.join("elsewhere"), "{}").unwrap();
        std::os::unix::fs::symlink(tmp.join("elsewhere"), state.join("operator-authority.json")).unwrap();
        assert!(backup_journals(&state, &tmp.join("b"), "x", "r").is_err());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-backup-test"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
