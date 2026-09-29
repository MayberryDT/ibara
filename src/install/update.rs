//! `ibara update` and `ibara rollback`: ibara's own release channel.
//!
//! A release is at [`Channel::base_url`]: `stable.json` (the manifest),
//! `stable.json.sig` (an SSH signature over it, namespace `ibara-release`, made
//! by `packaging/release.sh`) and the three packages it names, built together
//! at one version: `ibara`, `ibara-stream` (Take Control of this computer) and
//! `ibara-view` (the viewer for taking control of another). The manifest is
//! trusted only when the signature verifies against the release key built into
//! this program; each package only when its SHA-256 and size match the
//! manifest. Both values live in `packaging/release.env`.
//!
//! ```json
//! {"schema_version": 2, "name": "ibara", "version": "0.2.0-1", "released_at": "2026-10-01T12:00:00Z",
//!  "packages": [{"name": "ibara", "file": "ibara-0.2.0-1-x86_64.pkg.tar.zst", "sha256": "…", "size": 12345678},
//!               {"name": "ibara-stream", …}, {"name": "ibara-view", …}],
//!  "notes": ["Plain sentences about what changed."]}
//! ```
//!
//! Updating installs the three together and keeps them in
//! `/var/cache/ibara/packages`, with a copy of the journals from before the
//! update in `~/.local/state/ibara/backups`, which is what `ibara rollback`
//! goes back to. Once an update is installed, the console shows its notes once
//! ("What's New").

use super::user::as_root;
use super::{
    Account, LIB, PACKAGE_CACHE, USER_UNITS, installed_version, interactive, is_root, output, root_dir, sha256_hex, vercmp,
    write_root,
};
use serde_json::{Map, Value, json};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_ENV: &str = include_str!("../../packaging/release.env");
/// The SSH signature namespace and signer identity of release manifests.
pub const NAMESPACE: &str = "ibara-release";
/// Packages are far smaller; anything bigger is refused before hashing.
const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST: usize = 256 * 1024;
/// Releases kept for going back (each its three packages).
const KEEP_RELEASES: usize = 3;
/// The packages of every release, installed, kept and gone back to together.
pub const PACKAGES: [&str; 3] = ["ibara", "ibara-stream", "ibara-view"];

/// Where releases come from and the key they must be signed with.
#[derive(Debug, Clone)]
pub struct Channel {
    pub base_url: String,
    pub key: String,
}

impl Channel {
    /// From `packaging/release.env` as built in; none while either is unset.
    pub fn built_in() -> Option<Channel> {
        Channel::parse(RELEASE_ENV)
    }

    fn parse(text: &str) -> Option<Channel> {
        let value = |name: &str| {
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == name).map(|(_, v)| v.trim().trim_matches('"').to_string()))
                .filter(|v| !v.is_empty())
        };
        let base_url = value("IBARA_BASE_URL")?.trim_end_matches('/').to_string();
        let key = value("IBARA_RELEASE_KEY")?;
        (base_url.starts_with("https://") || base_url.starts_with("http://")).then_some(Channel { base_url, key })
    }

    fn url(&self, file: &str) -> String {
        format!("{}/{file}", self.base_url)
    }

    /// curl, limited to the channel's own scheme, following redirects on it only.
    fn fetch(&self, file: &str, to: &Path) -> Result<(), String> {
        let proto = if self.base_url.starts_with("https://") { "=https" } else { "=http" };
        output(
            Command::new("curl")
                .args(["-fsSL", "--proto", proto, "--proto-redir", proto, "--connect-timeout", "15", "--max-time", "900", "-o"])
                .arg(to)
                .arg(self.url(file)),
        )
        .map(|_| ())
        .map_err(|e| format!("Could not download {}: {e}", self.url(file)))
    }
}

/// One package of a verified release.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

/// A verified release manifest: every one of [`PACKAGES`], in that order.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub packages: Vec<Package>,
    pub notes: Vec<String>,
}

/// Check `signature` over `manifest` with `key` (an `ssh-ed25519 …` line), then
/// read the manifest. `dir` is a private scratch folder.
pub fn verify(manifest: &[u8], signature: &[u8], key: &str, dir: &Path) -> Result<Release, String> {
    if manifest.len() > MAX_MANIFEST {
        return Err("The release manifest is too large.".into());
    }
    let key = crate::sshkey::ed25519_line(key).ok_or("This build's release key is not an Ed25519 public key.")?;
    let signers = dir.join("allowed_signers");
    let sig = dir.join("stable.json.sig");
    let body = dir.join("stable.json");
    std::fs::write(&signers, format!("{NAMESPACE} namespaces=\"{NAMESPACE}\" {}\n", key.line)).map_err(|e| e.to_string())?;
    std::fs::write(&sig, signature).map_err(|e| e.to_string())?;
    std::fs::write(&body, manifest).map_err(|e| e.to_string())?;
    let checked = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-f"])
        .arg(&signers)
        .args(["-I", NAMESPACE, "-n", NAMESPACE, "-s"])
        .arg(&sig)
        .stdin(std::fs::File::open(&body).map_err(|e| e.to_string())?)
        .output()
        .map_err(|e| format!("ssh-keygen could not start ({e})."))?;
    if !checked.status.success() {
        return Err("The release manifest is not signed with ibara's release key; nothing was installed.".into());
    }
    let value: Value = serde_json::from_slice(manifest).map_err(|_| "The release manifest is not JSON.")?;
    let incomplete = || "The release manifest is signed but incomplete; nothing was installed.".to_string();
    let text = |v: &Value| v.as_str().map(str::to_string);
    let version = text(&value["version"]).unwrap_or_default();
    let version_ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-:".contains(&b));
    if value["schema_version"] != json!(2) || value["name"] != json!("ibara") || !version_ok {
        return Err(incomplete());
    }
    let listed = value["packages"].as_array().filter(|list| list.len() == PACKAGES.len()).ok_or_else(incomplete)?;
    let mut packages = Vec::with_capacity(PACKAGES.len());
    for name in PACKAGES {
        let entry = listed.iter().find(|p| p["name"] == json!(name)).ok_or_else(incomplete)?;
        let package = Package {
            name: name.to_string(),
            file: text(&entry["file"]).unwrap_or_default(),
            sha256: text(&entry["sha256"]).unwrap_or_default(),
            size: entry["size"].as_u64().unwrap_or(0),
        };
        // This package, at this release's version, and nothing that could be a path.
        let file_ok = package_version(name, &package.file).as_deref() == Some(version.as_str())
            && package.file.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b));
        let sha_ok = package.sha256.len() == 64 && package.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !file_ok || !sha_ok || package.size == 0 {
            return Err(incomplete());
        }
        packages.push(package);
    }
    let notes = value["notes"].as_array().map(|n| n.iter().filter_map(text).collect()).unwrap_or_default();
    Ok(Release { version, packages, notes })
}

/// A private folder in the person's cache for one update.
fn scratch() -> Result<PathBuf, String> {
    let dir = crate::server::home_dir().join(format!(".cache/ibara/update-{}-{}", crate::ids::now_millis(), std::process::id()));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// The newest release, verified.
fn latest(channel: &Channel, dir: &Path) -> Result<Release, String> {
    let manifest = dir.join("download.json");
    let signature = dir.join("download.json.sig");
    channel.fetch("stable.json", &manifest)?;
    channel.fetch("stable.json.sig", &signature)?;
    let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    verify(&read(&manifest)?, &read(&signature)?, &channel.key, dir)
}

pub fn update(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("Usage: ibara update [--check]".into()),
    };
    if is_root() {
        return Err("Run ibara update as yourself, not as root. It asks for your password when it installs.".into());
    }
    let me = Account::current()?;
    let channel = Channel::built_in().ok_or("This build of ibara has no update channel. Install a published release to get updates.")?;
    let installed = installed_version().ok_or("The ibara package is not installed.")?;
    let dir = scratch()?;
    let result = (|| {
        let release = latest(&channel, &dir)?;
        if vercmp(&release.version, &installed)? <= 0 {
            println!("ibara is up to date ({installed}).");
            return Ok(());
        }
        if check {
            println!("ibara {} is available (this computer has {installed}). Install it with: ibara update", release.version);
            return Ok(());
        }
        println!("Downloading ibara {} (this computer has {installed})…", release.version);
        let mut to_root = vec!["update".to_string(), me.name.clone()];
        for package in &release.packages {
            let path = dir.join(&package.file);
            channel.fetch(&package.file, &path)?;
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if bytes.len() as u64 != package.size || sha256_hex(&bytes) != package.sha256 {
                return Err(format!("The downloaded {} package does not match the signed release; nothing was installed.", package.name));
            }
            to_root.extend([path.to_string_lossy().into_owned(), package.sha256.clone()]);
        }
        let plugin_before = super::user::plugin_digest();
        println!("Installing it needs your password once (sudo).");
        as_root(&to_root.iter().map(String::as_str).collect::<Vec<_>>())?;
        restart_services()?;
        super::user::reload_plugin(plugin_before)?;
        record_whats_new(&release.version, &release.notes, Some(&installed))?;
        println!("\nibara {} is installed. What's new:", release.version);
        for note in &release.notes {
            println!("  - {note}");
        }
        println!("To go back to {installed}: ibara rollback");
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn restart_services() -> Result<(), String> {
    output(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    output(Command::new("systemctl").args(["--user", "restart", USER_UNITS[0], USER_UNITS[1]]))?;
    Ok(())
}

/// Whether an agent task holds this computer, from its controller (`ibara admin status`).
fn busy() -> Option<String> {
    let status = output(Command::new(Path::new(LIB).join("bin/ibara")).args(["admin", "status"])).ok()?;
    let status: Value = serde_json::from_str(&status).ok()?;
    let lease = status.pointer("/result/lease").filter(|l| !l.is_null())?;
    Some(lease["client_name"].as_str().or(lease["principal"].as_str()).unwrap_or("An agent").to_string())
}

fn refuse_while_busy() -> Result<(), String> {
    match busy() {
        Some(who) => Err(format!("{who} is working on this computer right now. Try again once it has finished.")),
        None => Ok(()),
    }
}

/// The version in a package file name of `name` (`ibara-0.2.0-1-x86_64.pkg.tar.zst`
/// → `0.2.0-1` for `ibara`; `ibara-stream-…` is not an `ibara` package).
pub fn package_version(name: &str, file: &str) -> Option<String> {
    let rest = file.strip_prefix(name)?.strip_prefix('-')?.strip_suffix(".pkg.tar.zst")?;
    let (version, arch) = rest.rsplit_once('-')?;
    let starts = version.bytes().next().is_some_and(|b| b.is_ascii_digit());
    (starts && !arch.is_empty() && version.contains('-')).then(|| version.to_string())
}

/// Which of [`PACKAGES`] a file is, and its version.
fn release_file(file: &str) -> Option<(&'static str, String)> {
    PACKAGES.iter().find_map(|name| Some((*name, package_version(name, file)?)))
}

/// Root: install the three packages of one release the person's `ibara
/// update` verified, given as `PACKAGE SHA256` pairs.
pub fn system_update(desktop: &Account, pairs: &[&str]) -> Result<(), String> {
    let mut files = Vec::new();
    for pair in pairs.chunks(2) {
        let [package, sha256] = pair else { return Err("Expected each package with its SHA-256.".into()) };
        let package = Path::new(*package);
        let file = package.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let (name, version) = release_file(&file).ok_or("That is not a package of an ibara release.")?;
        let size = std::fs::metadata(package).map_err(|e| format!("{}: {e}", package.display()))?.len();
        if size > MAX_PACKAGE {
            return Err("The package is too large.".into());
        }
        // Hash exactly the bytes root keeps, so the file cannot change after the check.
        let bytes = std::fs::read(package).map_err(|e| format!("{}: {e}", package.display()))?;
        if sha256_hex(&bytes) != *sha256 {
            return Err("The package changed after it was checked; nothing was installed.".into());
        }
        files.push((name, version, file, bytes));
    }
    let version = files.first().map(|f| f.1.clone()).unwrap_or_default();
    let complete = PACKAGES.iter().all(|name| files.iter().filter(|f| f.0 == *name).count() == 1);
    if files.len() != PACKAGES.len() || !complete || files.iter().any(|f| f.1 != version) {
        return Err(format!("An update installs {} of one release together.", PACKAGES.join(", ")));
    }
    refuse_while_busy()?;
    root_dir(Path::new(PACKAGE_CACHE), 0o755)?;
    let mut kept = Vec::new();
    for (_, _, file, bytes) in &files {
        let path = Path::new(PACKAGE_CACHE).join(file);
        write_root(&path, bytes, 0o644)?;
        kept.push(path);
    }
    let from = installed_version().unwrap_or_else(|| "none".into());
    backup_journals(desktop, &from)?;
    println!("Installing ibara {version}…");
    interactive(Command::new("pacman").args(["-U", "--noconfirm"]).args(&kept))?;
    prune_cache();
    Ok(())
}

/// A copy of the controller's journals, made by the desktop account itself
/// (root never opens the person's databases), named after the version that wrote them.
fn backup_journals(desktop: &Account, from: &str) -> Result<(), String> {
    let state = desktop.home.join(".local/state/agent-computer");
    if !state.join("journal.sqlite").exists() {
        return Ok(());
    }
    let name = format!("before-{}-{}", safe(from), crate::ids::now_millis());
    let dest = desktop.home.join(".local/state/ibara/backups").join(name);
    let as_person = |program: &Path, args: &[&std::ffi::OsStr]| {
        output(Command::new("runuser").args(["-u", &desktop.name, "--"]).arg(program).args(args))
    };
    as_person(Path::new("/usr/bin/install"), &["-d".as_ref(), "-m".as_ref(), "0700".as_ref(), dest.as_os_str()])
        .and_then(|_| as_person(&Path::new(LIB).join("bin/ibara"), &["backup-journals".as_ref(), state.as_os_str(), dest.as_os_str(), from.as_ref()]))
        .map(|_| ())
        .map_err(|e| format!("Could not copy the journals before updating: {e}"))
}

/// A version as part of a folder name.
fn safe(version: &str) -> String {
    version.replace(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-', "_")
}

/// Kept releases, newest first: each version with the files of its packages.
fn cached() -> Vec<(String, Vec<PathBuf>)> {
    let mut releases: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for entry in std::fs::read_dir(PACKAGE_CACHE).into_iter().flatten().flatten() {
        let Some((_, version)) = release_file(&entry.file_name().to_string_lossy()) else { continue };
        match releases.iter_mut().find(|(v, _)| *v == version) {
            Some((_, files)) => files.push(entry.path()),
            None => releases.push((version, vec![entry.path()])),
        }
    }
    releases.sort_by(|a, b| vercmp(&b.0, &a.0).unwrap_or(0).cmp(&0));
    releases
}

fn prune_cache() {
    for (_, files) in cached().into_iter().skip(KEEP_RELEASES) {
        for path in files {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn rollback(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("Usage: ibara rollback".into());
    }
    if is_root() {
        return Err("Run ibara rollback as yourself, not as root.".into());
    }
    let me = Account::current()?;
    let installed = installed_version().ok_or("The ibara package is not installed.")?;
    // Said before sudo asks for a password it would not need.
    let (earlier, _) = earlier_release(&installed)?;
    let plugin_before = super::user::plugin_digest();
    println!("Going back from ibara {installed} to {earlier} needs your password once (sudo).");
    as_root(&["rollback", &me.name])?;
    let now = installed_version().unwrap_or_default();
    restore_journals_if_newer(&me, &now)?;
    restart_services()?;
    super::user::reload_plugin(plugin_before)?;
    clear_whats_new();
    println!("ibara {now} is installed again.");
    Ok(())
}

/// Root: install the newest kept release older than the installed one, all of
/// its packages together.
pub fn system_rollback(desktop: &Account) -> Result<(), String> {
    let installed = installed_version().ok_or("The ibara package is not installed.")?;
    let (version, files) = earlier_release(&installed)?;
    refuse_while_busy()?;
    // The controller stops, so its journals can be swapped if they must be.
    let _ = output(&mut desktop.userctl(&["stop", USER_UNITS[0]]));
    println!("Installing ibara {version}…");
    interactive(Command::new("pacman").args(["-U", "--noconfirm"]).args(&files))
}

/// The newest kept release older than `installed` that has the ibara package.
fn earlier_release(installed: &str) -> Result<(String, Vec<PathBuf>), String> {
    let has_ibara = |files: &[PathBuf]| files.iter().any(|f| f.file_name().is_some_and(|n| package_version("ibara", &n.to_string_lossy()).is_some()));
    cached()
        .into_iter()
        .find(|(v, files)| vercmp(v, installed).is_ok_and(|c| c < 0) && has_ibara(files))
        .ok_or_else(|| format!("There is no earlier ibara release to go back to: {installed} is the only one kept on this computer."))
}

/// `meta.core_schema_version` of a journal, opened read-only.
fn core_schema(journal: &Path) -> Option<u32> {
    let db = rusqlite::Connection::open_with_flags(journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.query_row("SELECT value FROM meta WHERE key = 'core_schema_version'", [], |r| r.get::<_, String>(0)).ok()?.parse().ok()
}

/// After going back: when the update had moved the journals to a newer form
/// than the older version reads, put back the copy made just before that
/// update. The newer journals are kept beside it. Pairings ended and
/// permissions removed or narrowed since that update stay so.
fn restore_journals_if_newer(me: &Account, version: &str) -> Result<(), String> {
    let state = me.home.join(".local/state/agent-computer");
    let current = state.join("journal.sqlite");
    let backups = me.home.join(".local/state/ibara/backups");
    let prefix = format!("before-{}-", safe(version));
    let Some(backup) = std::fs::read_dir(&backups)
        .ok()
        .and_then(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix))).max())
    else {
        return Ok(());
    };
    let (Some(now), Some(then)) = (core_schema(&current), core_schema(&backup.join("journal.sqlite"))) else { return Ok(()) };
    if now <= then {
        return Ok(());
    }
    let aside = me.home.join(format!(".local/state/ibara/rolled-back-{}", crate::ids::now_millis()));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&aside).map_err(|e| format!("{}: {e}", aside.display()))?;
    for db in ["journal.sqlite", "storage.sqlite"] {
        for suffix in ["", "-wal", "-shm"] {
            let path = state.join(format!("{db}{suffix}"));
            if path.exists() {
                std::fs::rename(&path, aside.join(format!("{db}{suffix}"))).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        let saved = backup.join(db);
        if saved.exists() {
            std::fs::copy(&saved, state.join(db)).map_err(|e| format!("{}: {e}", saved.display()))?;
        }
    }
    println!(
        "The newer version had changed how history is stored, so this version uses the history saved before that update.\n\
         What happened since is kept in {}.",
        aside.display()
    );
    let kept = keep_later_revocations(&current, &aside.join("journal.sqlite")).map_err(|e| {
        format!(
            "Going back could not keep the access changes made since that update ({e}). ibara is stopped: \
             check Access and remove computers you had removed before starting it again."
        )
    })?;
    if kept {
        println!("Computers removed and permissions taken away since that update stay that way.");
    }
    Ok(())
}

/// The access model of a journal (`meta.access_model`), if it has one.
fn access_model(db: &rusqlite::Connection) -> Result<Option<Value>, String> {
    let raw: Option<String> = rusqlite::OptionalExtension::optional(db.query_row("SELECT value FROM meta WHERE key = 'access_model'", [], |r| r.get(0)))
        .map_err(|e| e.to_string())?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(|e| e.to_string())).transpose()
}

/// Carry into the restored journal what the newer one ended or narrowed; true
/// when anything changed.
fn keep_later_revocations(restored: &Path, newer: &Path) -> Result<bool, String> {
    let open = |path: &Path| rusqlite::Connection::open(path).map_err(|e| format!("{}: {e}", path.display()));
    let Some(later) = access_model(&open(newer)?)? else { return Ok(false) };
    let db = open(restored)?;
    let Some(mut older) = access_model(&db)? else { return Ok(false) };
    if !narrow_access(&mut older, &later) {
        return Ok(false);
    }
    db.execute("UPDATE meta SET value = ?1 WHERE key = 'access_model'", [older.to_string()]).map_err(|e| e.to_string())?;
    Ok(true)
}

/// An access model the older version wrote (`older`), no more permissive than
/// the newer one (`later`): a pairing not active there with the same key and
/// generation ends (its generation moves past both, its grants and its agents'
/// go); a grant gone there goes; a rule narrowed there is narrowed. Nothing
/// is added or widened, and only fields the older version wrote are touched,
/// so it still reads the result. Its revision moves past both, so approvals
/// asked for under either no longer count.
fn narrow_access(older: &mut Value, later: &Value) -> bool {
    let rank = |rule: &Value| match rule.as_str() {
        Some("allow") => 2,
        Some("ask") => 1,
        _ => 0,
    };
    let mut changed = false;
    let mut ended: Vec<String> = Vec::new();
    for (principal, pairing) in older.get_mut("pairings").and_then(Value::as_object_mut).into_iter().flatten() {
        let then = &later["pairings"][principal.as_str()];
        let same = then["active"] == json!(true) && then["key"] == pairing["key"] && then["generation"] == pairing["generation"];
        if pairing["active"] != json!(true) || same {
            continue;
        }
        let generation = pairing["generation"].as_u64().unwrap_or(0).max(then["generation"].as_u64().unwrap_or(0)) + 1;
        pairing["active"] = json!(false);
        pairing["generation"] = json!(generation);
        ended.push(principal.clone());
        changed = true;
    }
    if let Some(grants) = older.get_mut("grants").and_then(Value::as_object_mut) {
        let before = grants.len();
        grants.retain(|id, grant| {
            let subject = grant["subject"].as_str().unwrap_or("");
            let of_ended = ended.iter().any(|p| subject == p || subject.strip_suffix(p.as_str()).is_some_and(|s| s.ends_with('@')));
            !of_ended && later["grants"].get(id).is_some()
        });
        changed |= grants.len() != before;
        for (id, grant) in grants.iter_mut() {
            let then = &later["grants"][id.as_str()]["rule"];
            if rank(then) < rank(&grant["rule"]) {
                grant["rule"] = then.clone();
                changed = true;
            }
        }
    }
    if changed {
        older["revision"] = json!(older["revision"].as_u64().unwrap_or(0).max(later["revision"].as_u64().unwrap_or(0)) + 1);
    }
    changed
}

// ---------------------------------------------------------------------------
// What's new, shown once by the console after an update.

fn whats_new_path() -> PathBuf {
    crate::operator::directory::operator_state_dir().join("whats-new.json")
}

fn record_whats_new(version: &str, notes: &[String], from: Option<&str>) -> Result<(), String> {
    let path = whats_new_path();
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let record = json!({"version": version, "from": from, "notes": notes, "installed_at": crate::ids::now_iso(), "seen": false});
    std::fs::write(&path, format!("{record:#}\n")).map_err(|e| format!("{}: {e}", path.display()))
}

fn clear_whats_new() {
    let _ = std::fs::remove_file(whats_new_path());
}

/// The console's `whats-new`: the notes of the update installed last, until
/// the person has seen them; otherwise `{"version": null}`.
pub fn whats_new() -> Value {
    let record: Map<String, Value> =
        std::fs::read(whats_new_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if record.get("seen") != Some(&Value::Bool(false)) || !record.get("version").is_some_and(Value::is_string) {
        return json!({"version": null});
    }
    json!({"version": record["version"], "from": record.get("from").cloned().unwrap_or(Value::Null), "notes": record.get("notes").cloned().unwrap_or(json!([]))})
}

/// The console's `whats-new-seen`: the notes are not shown again.
pub fn whats_new_seen() -> Result<Value, String> {
    let path = whats_new_path();
    let Some(mut record) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok()) else {
        return Ok(json!({"version": null}));
    };
    record.insert("seen".into(), Value::Bool(true));
    std::fs::write(&path, format!("{:#}\n", Value::Object(record))).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(json!({"version": null}))
}

/// Failure cases for trusting a release, checked with the real ssh-keygen:
/// 1. A manifest signed with another key is accepted, so anyone could publish.
/// 2. A manifest changed after signing (another package digest) is accepted.
/// 3. A signature the release key made for something else (a git commit) is
///    accepted as a release signature.
/// 4. A signed manifest naming a path outside the download folder, a file that
///    is not the package it names, or a package of another version is accepted.
/// 5. A signed manifest without a digest or size for one package, or without
///    ibara-stream or ibara-view (or naming one twice), is accepted, so that
///    package is never checked, or Take Control is left out of the release.
/// 6. Another package's file in the cache is taken for an ibara version by rollback.
///
/// And for going back over a journal the newer version moved on:
/// 7. A pairing ended since the update (or replaced by another key or
///    generation) is active again, or keeps its grants or its agents' grants.
/// 8. A permission removed or narrowed since the update is back as it was.
/// 9. Going back widens or adds anything, or leaves approvals asked for under
///    the older access revision valid.
#[cfg(test)]
mod tests {
    use super::{PACKAGES, narrow_access, package_version, verify};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("ibara-release-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    /// A new Ed25519 key in `dir`; its public line.
    fn key(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"]).arg(&path).status().unwrap().success());
        std::fs::read_to_string(path.with_extension("pub")).unwrap().trim().to_string()
    }

    fn sign(dir: &Path, key: &str, namespace: &str, body: &[u8]) -> Vec<u8> {
        let file = dir.join(format!("body-{namespace}"));
        std::fs::write(&file, body).unwrap();
        let _ = std::fs::remove_file(file.with_extension("sig"));
        assert!(Command::new("ssh-keygen").args(["-q", "-Y", "sign", "-f"]).arg(dir.join(key)).args(["-n", namespace]).arg(&file).status().unwrap().success());
        std::fs::read(format!("{}.sig", file.display())).unwrap()
    }

    const SHA: &str = "4f2c1b0e9d8a7f6e5d4c3b2a1908f7e6d5c4b3a291807f6e5d4c3b2a19080706";

    /// The three packages of release 0.2.0-1, with `change` applied to the one named.
    fn packages(change: Option<(&str, &str, Value)>) -> Vec<Value> {
        PACKAGES
            .iter()
            .map(|name| {
                let mut entry = json!({"name": name, "file": format!("{name}-0.2.0-1-x86_64.pkg.tar.zst"), "sha256": SHA, "size": 1234});
                if let Some((which, field, value)) = &change
                    && which == name
                {
                    entry[*field] = value.clone();
                }
                entry
            })
            .collect()
    }

    fn manifest(packages: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 2, "name": "ibara", "version": "0.2.0-1", "released_at": "2026-10-01T12:00:00Z",
            "packages": packages, "notes": ["Updates install themselves."],
        }))
        .unwrap()
    }

    #[test]
    fn only_the_release_key_signing_this_exact_manifest_is_trusted() {
        let dir = scratch("trust");
        let release = key(&dir.0, "release");
        let _other = key(&dir.0, "other");
        let good = manifest(packages(None));
        let work = dir.0.join("work");
        std::fs::create_dir_all(&work).unwrap();

        let ok = verify(&good, &sign(&dir.0, "release", "ibara-release", &good), &release, &work).unwrap();
        assert_eq!(ok.version, "0.2.0-1");
        let named: Vec<(&str, &str, u64)> = ok.packages.iter().map(|p| (p.name.as_str(), p.sha256.as_str(), p.size)).collect();
        assert_eq!(named, [("ibara", SHA, 1234), ("ibara-stream", SHA, 1234), ("ibara-view", SHA, 1234)]);
        assert_eq!(ok.notes, vec!["Updates install themselves.".to_string()]);

        // 1. Another key.
        assert!(verify(&good, &sign(&dir.0, "other", "ibara-release", &good), &release, &work).is_err());
        // 2. Changed after signing: the viewer's digest.
        let swapped = manifest(packages(Some(("ibara-view", "sha256", json!(SHA.replace('4', "5"))))));
        assert!(verify(&swapped, &sign(&dir.0, "release", "ibara-release", &good), &release, &work).is_err());
        // 3. The release key, signing for another purpose.
        assert!(verify(&good, &sign(&dir.0, "release", "git", &good), &release, &work).is_err());
    }

    #[test]
    fn a_signed_manifest_must_name_all_three_packages_of_its_version_with_digests_and_sizes() {
        let dir = scratch("fields");
        let release = key(&dir.0, "release");
        let work = dir.0.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let without_viewer: Vec<Value> = packages(None).into_iter().filter(|p| p["name"] != "ibara-view").collect();
        let mut stream_twice = without_viewer.clone();
        stream_twice.push(stream_twice[1].clone());
        let mut first_shape = serde_json::from_slice::<Value>(&manifest(packages(None))).unwrap();
        first_shape["schema_version"] = json!(1);
        first_shape["package"] = first_shape["packages"][0].clone();
        for bad in [
            manifest(packages(Some(("ibara", "file", json!("../../etc/ibara-0.2.0-1-x86_64.pkg.tar.zst"))))),
            manifest(packages(Some(("ibara-stream", "file", json!("ibara-stream-0.2.0-1-x86_64.pkg.tar.zst.sh"))))),
            manifest(packages(Some(("ibara-view", "file", json!("other-0.2.0-1-x86_64.pkg.tar.zst"))))),
            manifest(packages(Some(("ibara", "file", json!("ibara-stream-0.2.0-1-x86_64.pkg.tar.zst"))))),
            manifest(packages(Some(("ibara-stream", "file", json!("ibara-stream-0.1.0-1-x86_64.pkg.tar.zst"))))),
            manifest(packages(Some(("ibara-view", "sha256", json!(""))))),
            manifest(packages(Some(("ibara-stream", "size", json!(0))))),
            manifest(without_viewer),
            manifest(stream_twice),
            serde_json::to_vec(&first_shape).unwrap(),
        ] {
            let signature = sign(&dir.0, "release", "ibara-release", &bad);
            assert!(verify(&bad, &signature, &release, &work).is_err(), "{}", String::from_utf8_lossy(&bad));
        }
    }

    #[test]
    fn rollback_reads_versions_only_from_each_packages_own_names() {
        assert_eq!(package_version("ibara", "ibara-0.2.0-1-x86_64.pkg.tar.zst").as_deref(), Some("0.2.0-1"));
        assert_eq!(package_version("ibara", "ibara-1:0.3.0-2-x86_64.pkg.tar.zst").as_deref(), Some("1:0.3.0-2"));
        assert_eq!(package_version("ibara", "ibara-stream-1.0.0-1-x86_64.pkg.tar.zst"), None);
        assert_eq!(package_version("ibara-stream", "ibara-stream-1.0.0-1-x86_64.pkg.tar.zst").as_deref(), Some("1.0.0-1"));
        assert_eq!(package_version("ibara-view", "ibara-stream-1.0.0-1-x86_64.pkg.tar.zst"), None);
        assert_eq!(package_version("ibara", "ibara-0.2.0-1-x86_64.pkg.tar.zst.part"), None);
    }

    fn pairing(key: &str, active: bool, generation: u64) -> Value {
        json!({"key": key, "endpoint": null, "active": active, "generation": generation})
    }

    fn grant(subject: &str, capability: &str, rule: &str) -> (String, Value) {
        (format!("{subject}:{capability}"), json!({"subject": subject, "capability": capability, "rule": rule}))
    }

    fn model(revision: u64, pairings: Value, grants: Vec<(String, Value)>) -> Value {
        json!({"version": 1, "revision": revision, "identities": {}, "pairings": pairings,
               "grants": grants.into_iter().collect::<serde_json::Map<String, Value>>()})
    }

    #[test]
    fn going_back_keeps_what_was_ended_or_narrowed_since_the_update() {
        // Before the update: Dana's computer (Take Control), Lab, Hazel (own) and Sam paired.
        let mut older = model(
            10,
            json!({"command": pairing("SHA256:dana", true, 3), "lab": pairing("SHA256:lab", true, 2),
                   "hazel": pairing("SHA256:hazel", true, 1), "sam": pairing("SHA256:sam", true, 5)}),
            vec![
                grant("command", "watch", "allow"),
                grant("command", "control", "allow"),
                grant("claude@command", "agents", "allow"),
                grant("lab", "watch", "allow"),
                grant("lab", "files", "allow"),
                grant("hazel", "control", "ask"),
                grant("sam", "watch", "allow"),
            ],
        );
        // Since the update: Dana's computer removed (7), Lab's files narrowed to
        // ask (8), Sam paired again with another key (7), Hazel's control widened
        // to allow and Tulip0 added (9: neither comes back).
        let later = model(
            14,
            json!({"command": pairing("SHA256:dana", false, 4), "lab": pairing("SHA256:lab", true, 2),
                   "hazel": pairing("SHA256:hazel", true, 1), "sam": pairing("SHA256:sam-new", true, 6),
                   "tulip0": pairing("SHA256:tulip0", true, 1)}),
            vec![
                grant("lab", "watch", "allow"),
                grant("lab", "files", "ask"),
                grant("hazel", "control", "allow"),
                grant("sam", "watch", "allow"),
                grant("tulip0", "watch", "allow"),
            ],
        );
        assert!(narrow_access(&mut older, &later));

        let pairings = &older["pairings"];
        assert_eq!((&pairings["command"]["active"], &pairings["command"]["generation"]), (&json!(false), &json!(5)), "{older}");
        assert_eq!((&pairings["sam"]["active"], &pairings["sam"]["generation"]), (&json!(false), &json!(7)), "{older}");
        assert_eq!(pairings["lab"], pairing("SHA256:lab", true, 2));
        assert_eq!(pairings["hazel"], pairing("SHA256:hazel", true, 1));
        assert!(pairings.get("tulip0").is_none(), "nothing added: {older}");
        let mut grants: Vec<(&str, &str)> =
            older["grants"].as_object().unwrap().iter().map(|(id, g)| (id.as_str(), g["rule"].as_str().unwrap())).collect();
        grants.sort();
        assert_eq!(grants, [("hazel:control", "ask"), ("lab:files", "ask"), ("lab:watch", "allow")], "{older}");
        assert_eq!(older["revision"], 15, "past both revisions, so older approvals no longer count");

        // The older version still reads it: the same fields, nothing new.
        let access: crate::access::Access = serde_json::from_value(older.clone()).unwrap();
        assert_eq!(access.rule("command", "watch", 0), crate::access::Rule::Deny);
        assert_eq!(access.rule("lab", "files", 0), crate::access::Rule::Ask);

        // Nothing ended or narrowed since: nothing changes.
        let same = older.clone();
        assert!(!narrow_access(&mut older, &same));
        assert_eq!(older, same);
    }
}
