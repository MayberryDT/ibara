//! `ibara browser-setup <extension dir> <desktop user>` (root): install ibara's
//! page reader in Google Chrome and Chromium without a click.
//!
//! - The extension is packed as a CRX3 signed with this computer's own key
//!   (`/etc/agent-computer/browser-extension.pem`, made with `openssl` on first
//!   run), so its id is this computer's and no signing secret ships anywhere.
//! - Managed policy (`ExtensionInstallForcelist`) in `/etc/chromium` and
//!   `/etc/opt/chrome` points at a local update manifest; Linux is the only
//!   platform where Chrome installs extensions from outside its store this
//!   way. The browser shows "Managed by your organization" (Omarchy's own
//!   Chromium theme policy already does).
//! - The native messaging host is registered system-wide for both browsers;
//!   per-user copies of it are removed, since a user copy would shadow it.
//! - The unpacked extension an operator once loaded by hand
//!   (`dhkcapcpkaiiinmodpkfbkjhbjgjigmn`) is blocked, which removes it.
//!
//! A changed extension gets the next version, which the browser installs on
//! its next update check (at start-up and every few hours).
//!
//! `ibara browser-setup --remove [--delete-key]` (root, on uninstall) undoes
//! what setup wrote outside the install root: both policies and both system
//! host manifests, so every profile drops the extension and no browser starts
//! the host again. `--delete-key` also removes the key and the build record.

use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const KEY: &str = "/etc/agent-computer/browser-extension.pem";
const STATE: &str = "/etc/agent-computer/browser-extension.json";
const PACKAGE_DIR: &str = "/opt/agent-computer/browser";
const HOST_NAME: &str = "io.ibara.chrome";
const HOST_PROGRAM: &str = "/opt/agent-computer/current/ops/ibara-chrome-native";
const HAND_LOADED: &str = "dhkcapcpkaiiinmodpkfbkjhbjgjigmn";
const POLICY_FILE: &str = "ibara.json";
const FILES: [&str; 3] = ["manifest.json", "worker.js", "document.js"];
/// `(policy dir, native host dir, per-user host dir under the home)`.
const BROWSERS: [(&str, &str, &str); 2] = [
    ("/etc/chromium/policies/managed", "/etc/chromium/native-messaging-hosts", ".config/chromium/NativeMessagingHosts"),
    ("/etc/opt/chrome/policies/managed", "/etc/opt/chrome/native-messaging-hosts", ".config/google-chrome/NativeMessagingHosts"),
];

pub fn main(args: Vec<OsString>) -> i32 {
    let text: Vec<Option<&str>> = args.iter().map(|a| a.to_str()).collect();
    let result = match text.as_slice() {
        [Some("--remove")] => as_root().and_then(|()| remove(Path::new("/"), false)),
        [Some("--remove"), Some("--delete-key")] => as_root().and_then(|()| remove(Path::new("/"), true)),
        [first, _] if *first != Some("--remove") => setup(Path::new(&args[0]), &args[1].to_string_lossy()),
        _ => {
            eprintln!("Usage: ibara browser-setup <extension dir> <desktop user>\n       ibara browser-setup --remove [--delete-key]");
            return 64;
        }
    };
    match result {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(e) => {
            eprintln!("ibara browser-setup: {e}");
            1
        }
    }
}

fn as_root() -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("run as root".into());
    }
    Ok(())
}

/// Undo `setup` outside the install root: both browsers' policy (the
/// browsers then remove the extension from every profile) and system native
/// host manifests. With `delete_key`, also this computer's signing key and
/// build record, so a later setup makes a new extension id. The package in
/// `/opt/agent-computer/browser` goes with the install root. Other files in
/// those directories (Omarchy's own Chromium policy) stay. `root` is `/`
/// except in tests.
fn remove(root: &Path, delete_key: bool) -> Result<serde_json::Value, String> {
    let at = |path: &str| root.join(path.trim_start_matches('/'));
    let mut files: Vec<PathBuf> =
        BROWSERS.iter().flat_map(|(policy_dir, host_dir, _)| [at(policy_dir).join(POLICY_FILE), at(host_dir).join(format!("{HOST_NAME}.json"))]).collect();
    if delete_key {
        files.extend([at(KEY), at(STATE)]);
    }
    let mut removed = Vec::new();
    for file in files {
        match std::fs::remove_file(&file) {
            Ok(()) => removed.push(file.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", file.display())),
        }
    }
    Ok(serde_json::json!({ "removed": removed }))
}

/// The page reader is installed for at least one browser: that browser's
/// policy and system native host manifest are both in place, as `setup`
/// writes them. `root` is `/` except in tests.
pub fn reader_installed(root: &Path) -> bool {
    let at = |path: &str| root.join(path.trim_start_matches('/'));
    BROWSERS.iter().any(|(policy_dir, host_dir, _)| at(policy_dir).join(POLICY_FILE).is_file() && at(host_dir).join(format!("{HOST_NAME}.json")).is_file())
}

fn setup(source: &Path, desktop: &str) -> Result<serde_json::Value, String> {
    as_root()?;
    let home = home_of(desktop)?;
    let public = public_key()?;
    let id = extension_id(&public);
    let files: Vec<(&str, Vec<u8>)> =
        FILES.iter().map(|f| std::fs::read(source.join(f)).map(|b| (*f, b)).map_err(|e| format!("{}: {e}", source.join(f).display()))).collect::<Result<_, _>>()?;

    // Version: bumped whenever the extension or the key changes.
    let mut digest = Sha256::new();
    digest.update(&public);
    for (name, bytes) in &files {
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    let content = hex(&digest.finalize());
    let state: serde_json::Value = std::fs::read(STATE).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let package = Path::new(PACKAGE_DIR);
    // The build never goes back. Browsers do not downgrade, so a lost record
    // that restarted at 1.0.0.1 would leave every later release unapplied.
    let published = std::fs::read_to_string(package.join("updates.xml")).ok().and_then(|xml| published_build(&xml));
    let mut build = state["build"].as_u64().unwrap_or(0).max(published.unwrap_or(0));
    let changed = state["content"].as_str() != Some(content.as_str()) || !package.join("ibara.crx").is_file();
    if changed {
        build += 1;
    }
    let version = format!("1.0.0.{build}");
    let update_url = format!("file://{PACKAGE_DIR}/updates.xml");

    if changed {
        let mut entries = Vec::new();
        for (name, bytes) in files {
            let bytes = if name == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("manifest.json: {e}"))?;
                manifest["version"] = version.clone().into();
                manifest["update_url"] = update_url.clone().into();
                manifest["key"] = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &public).into();
                serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?
            } else {
                bytes
            };
            entries.push((name, bytes));
        }
        let archive = zip_stored(&entries);
        let crx = crx3(&public, &archive)?;
        std::fs::create_dir_all(package).map_err(|e| e.to_string())?;
        std::fs::set_permissions(package, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        write_file(&package.join("ibara.crx"), &crx, 0o644)?;
        let updates = format!(
            "<?xml version='1.0' encoding='UTF-8'?>\n<gupdate xmlns='http://www.google.com/update2/response' protocol='2.0'>\n  <app appid='{id}'>\n    <updatecheck codebase='file://{PACKAGE_DIR}/ibara.crx' version='{version}' />\n  </app>\n</gupdate>\n"
        );
        write_file(&package.join("updates.xml"), updates.as_bytes(), 0o644)?;
        let record = serde_json::json!({ "build": build, "content": content, "id": id });
        write_file(Path::new(STATE), &serde_json::to_vec(&record).map_err(|e| e.to_string())?, 0o600)?;
    }

    let policy = serde_json::json!({
        "ExtensionInstallForcelist": [format!("{id};{update_url}")],
        "ExtensionInstallBlocklist": [HAND_LOADED],
    });
    let host = serde_json::json!({
        "name": HOST_NAME,
        "description": "ibara page reader",
        "path": HOST_PROGRAM,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{id}/")],
    });
    // `ibara chrome-host` accepts only the origins listed here.
    write_file(&package.join("native-host.json"), &pretty(&host)?, 0o644)?;
    let mut removed = Vec::new();
    for (policy_dir, host_dir, user_dir) in BROWSERS {
        write_file(&Path::new(policy_dir).join(POLICY_FILE), &pretty(&policy)?, 0o644)?;
        write_file(&Path::new(host_dir).join(format!("{HOST_NAME}.json")), &pretty(&host)?, 0o644)?;
        let user_copy = home.join(user_dir).join(format!("{HOST_NAME}.json"));
        if user_copy.exists() {
            std::fs::remove_file(&user_copy).map_err(|e| format!("{}: {e}", user_copy.display()))?;
            removed.push(user_copy.display().to_string());
        }
    }
    // The hand-loaded copy's directory; the blocklist removes it from profiles.
    let legacy = Path::new("/opt/agent-computer/chrome-extension");
    if legacy.is_dir() {
        std::fs::remove_dir_all(legacy).map_err(|e| format!("{}: {e}", legacy.display()))?;
        removed.push(legacy.display().to_string());
    }
    Ok(serde_json::json!({ "extension_id": id, "version": version, "repacked": changed, "removed": removed }))
}

fn home_of(user: &str) -> Result<PathBuf, String> {
    let passwd = std::fs::read_to_string("/etc/passwd").map_err(|e| e.to_string())?;
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 7 && f[0] == user)
        .map(|f| PathBuf::from(f[5]))
        .ok_or_else(|| format!("no such user: {user}"))
}

/// This computer's signing key (made on first use), as DER SubjectPublicKeyInfo.
fn public_key() -> Result<Vec<u8>, String> {
    if !Path::new(KEY).is_file() {
        let fresh = format!("{KEY}.new");
        let _ = std::fs::remove_file(&fresh);
        openssl(&["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", &fresh], None)?;
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        std::fs::rename(&fresh, KEY).map_err(|e| e.to_string())?;
    }
    openssl(&["pkey", "-in", KEY, "-pubout", "-outform", "DER"], None)
}

fn openssl(args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("openssl")
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("openssl: {e}"))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(bytes).map_err(|e| format!("openssl: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("openssl: {e}"))?;
    if !out.status.success() {
        return Err(format!("openssl {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// Chrome's id: the first 16 bytes of the key's SHA-256, as letters a-p.
fn extension_id(public: &[u8]) -> String {
    Sha256::digest(public)[..16].iter().flat_map(|b| [b >> 4, b & 15]).map(|n| (b'a' + n) as char).collect()
}

/// A CRX3 file: `Cr24`, version 3, a `CrxFileHeader` with one RSA proof and
/// the signed `SignedData { crx_id }`, then the zip.
fn crx3(public: &[u8], archive: &[u8]) -> Result<Vec<u8>, String> {
    let crx_id = &Sha256::digest(public)[..16];
    let signed_data = field(1, crx_id);
    let mut message = b"CRX3 SignedData\x00".to_vec();
    message.extend((signed_data.len() as u32).to_le_bytes());
    message.extend(&signed_data);
    message.extend(archive);
    let signature = openssl(&["dgst", "-sha256", "-sign", KEY], Some(&message))?;
    let mut proof = field(1, public);
    proof.extend(field(2, &signature));
    let mut header = field(2, &proof); // sha256_with_rsa
    header.extend(field(10000, &signed_data)); // signed_header_data
    let mut out = b"Cr24".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend((header.len() as u32).to_le_bytes());
    out.extend(header);
    out.extend(archive);
    Ok(out)
}

/// One length-delimited protobuf field.
fn field(number: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = varint((number << 3) | 2);
    out.extend(varint(bytes.len() as u64));
    out.extend(bytes);
    out
}

fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// A zip of stored (uncompressed) entries, dated 1980-01-01.
fn zip_stored(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    const DATE: u16 = 0x21;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        let common = |sig: u32, central: bool| {
            let mut h = sig.to_le_bytes().to_vec();
            if central {
                h.extend(20u16.to_le_bytes()); // made by
            }
            for v in [20u16, 0, 0, 0, DATE] {
                h.extend(v.to_le_bytes()); // needed, flags, method (stored), time, date
            }
            h.extend(crc.to_le_bytes());
            h.extend(size.to_le_bytes());
            h.extend(size.to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // extra
            h
        };
        out.extend(common(0x0403_4b50, false));
        out.extend(name.as_bytes());
        out.extend(data);
        let mut c = common(0x0201_4b50, true);
        for v in [0u16, 0, 0] {
            c.extend(v.to_le_bytes()); // comment, disk, internal attributes
        }
        c.extend(0u32.to_le_bytes()); // external attributes
        c.extend(offset.to_le_bytes());
        c.extend(name.as_bytes());
        central.extend(c);
    }
    let start = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend(v.to_le_bytes());
    }
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn pretty(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write through a temporary file and rename, creating the directory.
fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let temp = path.with_extension("ibara-new");
    std::fs::write(&temp, bytes).map_err(|e| format!("{}: {e}", temp.display()))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The build of the version an `updates.xml` last published (`1.0.0.N`).
fn published_build(updates: &str) -> Option<u64> {
    let rest = &updates[updates.find("version='1.0.0.")? + "version='1.0.0.".len()..];
    rest[..rest.find('\'')?].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_resumes_from_the_published_version() {
        let xml = format!("<gupdate>\n  <app appid='x'>\n    <updatecheck codebase='file://{PACKAGE_DIR}/ibara.crx' version='1.0.0.17' />\n  </app>\n</gupdate>\n");
        assert_eq!(published_build(&xml), Some(17));
        assert_eq!(published_build("<gupdate/>"), None);
        assert_eq!(published_build("version='1.0.0.x'"), None);
    }

    #[test]
    fn remove_takes_only_what_setup_wrote_and_the_key_on_request() {
        let root = std::env::temp_dir().join(format!("ibara-browser-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let at = |path: &str| root.join(path.trim_start_matches('/'));
        let mut ours = vec![at(KEY), at(STATE)];
        for (policy_dir, host_dir, _) in BROWSERS {
            ours.push(at(policy_dir).join(POLICY_FILE));
            ours.push(at(host_dir).join(format!("{HOST_NAME}.json")));
        }
        let theirs = at(BROWSERS[0].0).join("omarchy.json");
        for file in ours.iter().chain([&theirs]) {
            write_file(file, b"{}", 0o644).unwrap();
        }
        assert!(reader_installed(&root));
        let report = remove(&root, false).unwrap();
        assert_eq!(report["removed"].as_array().unwrap().len(), 4);
        assert!(at(KEY).is_file() && at(STATE).is_file(), "the key stays unless asked");
        assert!(theirs.is_file(), "another policy in the same directory stays");
        assert!(ours[2..].iter().all(|f| !f.exists()));
        assert!(!reader_installed(&root), "another browser policy is not the page reader");
        // One browser's policy without its native host cannot connect; both,
        // for either browser alone, can.
        write_file(&at(BROWSERS[0].0).join(POLICY_FILE), b"{}", 0o644).unwrap();
        assert!(!reader_installed(&root));
        write_file(&at(BROWSERS[1].0).join(POLICY_FILE), b"{}", 0o644).unwrap();
        write_file(&at(BROWSERS[1].1).join(format!("{HOST_NAME}.json")), b"{}", 0o644).unwrap();
        assert!(reader_installed(&root));
        remove(&root, false).unwrap();
        let report = remove(&root, true).unwrap();
        assert_eq!(report["removed"].as_array().unwrap().len(), 2, "already-removed files are not an error");
        assert!(!at(KEY).exists() && !at(STATE).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
