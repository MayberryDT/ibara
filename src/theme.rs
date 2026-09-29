//! Omarchy themes shared between computers.
//!
//! A computer applies a theme with Omarchy's own `omarchy-theme-set NAME`.
//! A theme it lacks arrives as a bundle: the theme's folder, packed as
//! `IBTHEME1` followed by `(u32 path length, path, u64 size, bytes)` per file
//! (big-endian), unpacked into `~/.config/omarchy/themes/NAME`. Only regular
//! files travel; symlinks and `.git` never do. A theme that came from a git
//! repository (`omarchy theme install`) keeps Omarchy's rule for strangers'
//! themes: no Lua, terminal configurations or `vscode.json`, since those run
//! code; the receiving computer would otherwise treat them as the person's own.

use crate::error::{IbaraError, Result, invalid};
use std::path::{Component, Path, PathBuf};

const MAGIC: &[u8; 8] = b"IBTHEME1";
/// A bundle is at most 64 MiB and 2 000 files (Omarchy's largest theme is about 20 MiB).
pub const MAX_BUNDLE: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_PATH: usize = 255;
const MAX_DEPTH: usize = 4;
/// Files Omarchy does not take from a theme installed from a git repository.
const DENIED_FROM_REPO: [&str; 5] = ["alacritty.toml", "foot.ini", "ghostty.conf", "kitty.conf", "vscode.json"];

/// `^[a-z0-9][a-z0-9._-]{0,63}$`: the names `omarchy-theme-set` stores.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
        && !name.contains("..")
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `$OMARCHY_PATH`, else `/usr/share/omarchy`.
pub fn omarchy_path() -> PathBuf {
    std::env::var_os("OMARCHY_PATH").filter(|p| Path::new(p).is_absolute()).map(PathBuf::from).unwrap_or_else(|| "/usr/share/omarchy".into())
}

/// The person's own themes: `~/.config/omarchy/themes`.
pub fn user_themes() -> PathBuf {
    home().join(".config/omarchy/themes")
}

/// The theme this computer shows now (`~/.local/state/omarchy/current/theme.name`).
pub fn current_name() -> Option<String> {
    let text = std::fs::read_to_string(home().join(".local/state/omarchy/current/theme.name")).ok()?;
    Some(text.trim().to_string()).filter(|n| valid_name(n))
}

/// Where the theme's own files are: the person's folder first (Omarchy lays
/// it over the built-in one), else Omarchy's.
pub fn source(name: &str) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    [user_themes().join(name), omarchy_path().join("themes").join(name)].into_iter().find(|p| p.is_dir())
}

/// Pack a theme folder.
pub fn pack(dir: &Path) -> Result<Vec<u8>> {
    let from_repo = !dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) && dir.join(".git").is_dir();
    let mut files = Vec::new();
    collect(dir, Path::new(""), from_repo, &mut files)?;
    let mut out = MAGIC.to_vec();
    for (relative, path) in files {
        let bytes = std::fs::read(&path)?;
        let text = relative.to_string_lossy();
        out.extend_from_slice(&(text.len() as u32).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(&bytes);
        if out.len() as u64 > MAX_BUNDLE {
            return Err(invalid("This theme is too large to send (over 64 MiB)."));
        }
    }
    Ok(out)
}

fn collect(dir: &Path, relative: &Path, from_repo: bool, out: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let kind = entry.file_type()?;
        if kind.is_symlink() || text == ".git" || text.starts_with(".ibara-") {
            continue;
        }
        let rel = relative.join(text);
        if kind.is_dir() {
            if rel.components().count() < MAX_DEPTH {
                collect(&entry.path(), &rel, from_repo, out)?;
            }
        } else if kind.is_file() {
            if from_repo && (text.ends_with(".lua") || DENIED_FROM_REPO.contains(&text)) {
                continue;
            }
            if out.len() >= MAX_FILES {
                return Err(invalid("This theme has too many files to send."));
            }
            out.push((rel, entry.path()));
        }
    }
    Ok(())
}

fn safe_relative(text: &str) -> Option<PathBuf> {
    let path = Path::new(text);
    let ok = !text.is_empty()
        && text.len() <= MAX_PATH
        && !text.contains('\0')
        && path.components().count() <= MAX_DEPTH
        && path.components().all(|c| matches!(c, Component::Normal(n) if n.to_str().is_some_and(|n| !n.starts_with(".git"))));
    ok.then(|| path.to_path_buf())
}

/// Unpack a bundle into a new theme folder `dest` (which must not exist).
pub fn unpack(bundle: &[u8], dest: &Path) -> Result<usize> {
    let broken = || IbaraError::new("INVALID_ARGUMENT", "The theme that arrived is damaged. Try again.", true);
    if !bundle.starts_with(MAGIC) {
        return Err(broken());
    }
    let parent = dest.parent().ok_or_else(broken)?;
    if dest.symlink_metadata().is_ok() {
        return Err(invalid("That theme is already on this computer."));
    }
    std::fs::create_dir_all(parent)?;
    let name = dest.file_name().and_then(|n| n.to_str()).ok_or_else(broken)?;
    let staging = parent.join(format!(".ibara-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&staging)?;
    let written = (|| {
        let mut at = MAGIC.len();
        let mut count = 0;
        let take = |at: &mut usize, n: usize| -> Result<&[u8]> {
            let end = at.checked_add(n).filter(|end| *end <= bundle.len()).ok_or_else(broken)?;
            let slice = &bundle[*at..end];
            *at = end;
            Ok(slice)
        };
        while at < bundle.len() {
            let len = u32::from_be_bytes(take(&mut at, 4)?.try_into().map_err(|_| broken())?) as usize;
            let text = std::str::from_utf8(take(&mut at, len)?).map_err(|_| broken())?;
            let relative = safe_relative(text).ok_or_else(broken)?;
            let size = u64::from_be_bytes(take(&mut at, 8)?.try_into().map_err(|_| broken())?);
            let bytes = take(&mut at, usize::try_from(size).map_err(|_| broken())?)?;
            count += 1;
            if count > MAX_FILES {
                return Err(broken());
            }
            let target = staging.join(&relative);
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            file.write_all(bytes)?;
        }
        Ok(count)
    })();
    match written {
        Ok(count) => match std::fs::rename(&staging, dest) {
            Ok(()) => Ok(count),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}
