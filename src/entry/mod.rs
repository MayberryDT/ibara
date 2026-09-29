//! `ibara` subcommands that run on a target.
//!
//! - `ibara agent-entry <p> [socket] [gateway.key] [operator key] [fingerprint]`
//!   is what `ops/entry.sh` execs: it reads `SSH_ORIGINAL_COMMAND` and serves
//!   `mcp`, `transfer-v1` or `operator-v1` on stdio, like `entry.sh` did with
//!   `dist/mcp.js`, `dist/transfer.js` and `dist/operator.js`.
//! - `ibara agent-entry --legacy <p>` is Tulip0's `/usr/local/libexec-ibara-entry <p>`
//!   (`ops/legacy-entry.mjs`).
//! - `ibara admin …` is `computerctl`.
//! - `ibara chrome-host <origin>` is the Chrome native messaging host.
//! - `ibara browser-setup <extension dir> <desktop user>` installs the page
//!   reader in Chrome and Chromium by policy (`browser_setup.rs`).
//! - `ibara backup-journals <state dir> <destination dir> <previous release>` is `ops/backup-journals.py`.
//! - `ibara join [--accept CODE | --decline CODE]` answers requests to add this computer (`join.rs`).

pub mod admin;
pub mod browser_setup;
pub mod join;
pub mod mcp;
pub mod operator;
pub mod transfer;

use crate::error::{IbaraError, Result};
use crate::server::authority::valid_principal;
use crate::server::policy::read_trimmed;
use serde_json::Value;
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const DEFAULT_SOCKET: &str = "/run/agent-computer/controller.sock";
pub const DEFAULT_GATEWAY_KEY: &str = "/etc/agent-computer/gateway.key";
pub const DEFAULT_OPERATOR_SOCKET_DIR: &str = "/run/ibara-operator";
const FINGERPRINTS: &str = "/etc/ibara-operator/fingerprints.json";
const LEGACY_AUTHORIZED_KEYS: &str = "/etc/agent-computer/ssh/authorized_keys";
const LEGACY_ENTRY: &str = "/usr/local/libexec-ibara-entry";

/// The controller socket and the bearer file, read afresh for every request
/// so a rotated key takes effect without reconnecting (`rpc.ts:6-10`).
#[derive(Debug, Clone)]
pub struct Gateway {
    pub socket: PathBuf,
    pub key: PathBuf,
}

impl Gateway {
    pub async fn post(&self, body: &Value, timeout: Duration) -> Result<Value> {
        let token = read_trimmed(&self.key)
            .map_err(|e| IbaraError::new("SESSION_UNAVAILABLE", format!("Gateway key unreadable: {e}"), true))?;
        crate::http::post(&self.socket, Some(&token), body, timeout).await
    }
}

/// Run `future` on one current-thread runtime and return its exit code.
pub fn block_on(future: impl Future<Output = i32>) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("ibara: {e}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&runtime, future);
    drop(local);
    // A stdin reader may still sit in read(2); do not wait for it.
    runtime.shutdown_background();
    code
}

/// One line of at most `max` bytes (without the newline and a trailing `\r`).
/// `Ok(None)` at end of input, `Ok(Some(false))` for an over-long line (the
/// rest of it is discarded), `Ok(Some(true))` with the line in `buf`.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, max: usize, buf: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        any = true;
        let newline = available.iter().position(|b| *b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if !too_long {
            // Allow one extra byte for a trailing '\r'.
            if buf.len() + chunk.len() > max + 1 {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = newline.map_or(available.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if !too_long && buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if !too_long && buf.len() > max {
        too_long = true;
        buf.clear();
    }
    Ok(Some(!too_long))
}

/// Write one line to stdout and flush it.
pub async fn write_line<W: tokio::io::AsyncWrite + Unpin>(out: &mut W, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    out.write_all(text.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await
}

enum Command {
    Mcp,
    Transfer,
    Operator,
}

fn command() -> std::result::Result<Command, i32> {
    match std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default().as_str() {
        "mcp" => Ok(Command::Mcp),
        "transfer-v1" => Ok(Command::Transfer),
        "operator-v1" => Ok(Command::Operator),
        _ => {
            eprintln!("Only mcp, transfer-v1 and operator-v1 are supported.");
            Err(64)
        }
    }
}

struct Entry {
    principal: String,
    command: Command,
    gateway: Gateway,
    /// For `operator-v1`: the route and the pinned key fingerprint (empty when none).
    operator: Option<(operator::Route, String)>,
}

fn nonempty(arg: Option<&String>) -> Option<&str> {
    arg.map(String::as_str).filter(|a| !a.is_empty())
}

/// `ops/entry.sh`: `$1` principal, `$2` socket, `$3` gateway key, `$4` the
/// test/admin operator bearer path (switches `operator-v1` to bearer mode),
/// `$5` the pinned operator key fingerprint.
fn standard(args: &[String]) -> std::result::Result<Entry, i32> {
    let principal = args.first().cloned().unwrap_or_default();
    if !valid_principal(&principal) {
        return Err(64);
    }
    let command = command()?;
    let gateway = Gateway {
        socket: nonempty(args.get(1)).unwrap_or(DEFAULT_SOCKET).into(),
        key: nonempty(args.get(2)).unwrap_or(DEFAULT_GATEWAY_KEY).into(),
    };
    let operator = match command {
        Command::Operator => {
            let bearer = nonempty(args.get(3));
            let operator = match bearer {
                Some(key) => operator::Route::Bearer(Gateway { socket: gateway.socket.clone(), key: key.into() }),
                None => {
                    let dir = crate::server::env_nonempty("IBARA_OPERATOR_SOCKET_DIR").unwrap_or_else(|| DEFAULT_OPERATOR_SOCKET_DIR.into());
                    operator::Route::Peer(PathBuf::from(format!("{dir}/{principal}.sock")))
                }
            };
            let fingerprint = match (nonempty(args.get(4)), bearer) {
                (Some(pinned), _) => pinned.to_string(),
                (None, None) => published_fingerprint(&principal),
                (None, Some(_)) => String::new(),
            };
            Some((operator, fingerprint))
        }
        _ => None,
    };
    Ok(Entry { principal, command, gateway, operator })
}

/// `ops/legacy-entry.mjs`: exactly one principal, fixed paths, and the
/// operator fingerprint from the root-owned `authorized_keys` binding.
fn legacy(args: &[String]) -> std::result::Result<Entry, i32> {
    let [principal] = args else { return Err(64) };
    if !crate::server::peer::valid_peer_principal(principal) {
        return Err(64);
    }
    let command = command()?;
    let gateway = Gateway { socket: DEFAULT_SOCKET.into(), key: DEFAULT_GATEWAY_KEY.into() };
    let operator = match command {
        Command::Operator => Some((
            operator::Route::Peer(PathBuf::from(format!("{DEFAULT_OPERATOR_SOCKET_DIR}/{principal}.sock"))),
            legacy_fingerprint(principal)?,
        )),
        _ => None,
    };
    Ok(Entry { principal: principal.clone(), command, gateway, operator })
}

/// `fingerprints.json[p]` from a root-owned file that group and other cannot
/// write, when it has the `SHA256:<43 base64>` shape; otherwise empty.
fn published_fingerprint(principal: &str) -> String {
    let Ok(meta) = std::fs::symlink_metadata(FINGERPRINTS) else { return String::new() };
    if !meta.file_type().is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return String::new();
    }
    let value = std::fs::read(FINGERPRINTS).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    value
        .as_ref()
        .and_then(|v| v.get("fingerprints"))
        .and_then(|f| f.get(principal))
        .and_then(Value::as_str)
        .filter(|f| valid_fingerprint(f))
        .unwrap_or("")
        .to_string()
}

/// `^SHA256:[A-Za-z0-9+/]{43}$`.
fn valid_fingerprint(f: &str) -> bool {
    f.strip_prefix("SHA256:")
        .is_some_and(|b| b.len() == 43 && b.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/'))
}

fn is_base64_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/'
}

/// The one `restrict,command="/usr/local/libexec-ibara-entry <p>" ssh-ed25519 <key>`
/// line's key, as an OpenSSH `SHA256:` fingerprint. Falls back to
/// `fingerprints.json` only when the file cannot be read at all.
fn legacy_fingerprint(principal: &str) -> std::result::Result<String, i32> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    let keys = match std::fs::read(LEGACY_AUTHORIZED_KEYS) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => {
            return Ok(published_fingerprint(principal));
        }
        Err(e) => {
            eprintln!("Restricted gateway dispatch failed: {e}");
            return Err(1);
        }
    };
    let prefix = format!("restrict,command=\"{LEGACY_ENTRY} {principal}\" ssh-ed25519 ");
    let matches: Vec<&str> = keys.split('\n').filter(|line| line.starts_with(&prefix)).collect();
    let key = match matches.as_slice() {
        [line] => {
            let rest = &line[prefix.len()..];
            let (key, comment) = rest.split_once(' ').map_or((rest, None), |(k, c)| (k, Some(c)));
            let body = key.trim_end_matches('=');
            let shaped = !body.is_empty()
                && key.len() - body.len() <= 2
                && body.bytes().all(is_base64_char)
                && comment.is_none_or(|c| !c.contains(['\r', '\n']));
            shaped.then_some(key)
        }
        _ => None,
    };
    let Some(encoded) = key else {
        eprintln!("Restricted operator key binding is unavailable or ambiguous.");
        return Err(64);
    };
    let public_key = STANDARD.decode(encoded).map_err(|_| 64)?;
    if public_key.len() < 32 || STANDARD.encode(&public_key) != encoded {
        return Err(64);
    }
    let digest = crate::server::policy::sha256(&public_key);
    Ok(format!("SHA256:{}", STANDARD.encode(digest).trim_end_matches('=')))
}

/// `ibara agent-entry …`: see the module documentation.
pub fn agent_entry(args: Vec<OsString>) -> i32 {
    if asks_help(&args) {
        println!("ibara agent-entry is started by ibara itself when an agent connects over SSH, not by people.");
        return 0;
    }
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let entry = match args.first().map(String::as_str) {
        Some("--legacy") => legacy(&args[1..]),
        _ => standard(&args),
    };
    let entry = match entry {
        Ok(entry) => entry,
        Err(code) => return code,
    };
    match entry.command {
        Command::Mcp => block_on(mcp::run(&entry.principal, &entry.gateway)),
        Command::Transfer => block_on(transfer::run(&entry.principal, &entry.gateway)),
        Command::Operator => match &entry.operator {
            Some((route, fingerprint)) if route.acceptable() => block_on(operator::run(&entry.principal, route, fingerprint)),
            _ => 64,
        },
    }
}

/// `ibara admin …` (`computerctl`).
pub fn admin(args: Vec<OsString>) -> i32 {
    admin::main(args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect())
}

/// `ibara chrome-host <origin>`: the Chrome native messaging host.
pub fn chrome_host(args: Vec<OsString>) -> i32 {
    if asks_help(&args) {
        println!("ibara chrome-host is started by Chrome for ibara's browser extension, not by people.");
        return 0;
    }
    block_on(crate::desktop::chrome::chrome_host_main())
}

/// `--help`, `-h` or `help` as the only argument.
fn asks_help(args: &[OsString]) -> bool {
    matches!(args, [one] if matches!(one.to_str(), Some("--help" | "-h" | "help")))
}

/// `ibara backup-journals <source state dir> <destination dir> <previous release>`.
pub fn backup_journals(args: Vec<OsString>) -> i32 {
    let [source, destination, previous] = args.as_slice() else {
        eprintln!("Usage: ibara backup-journals SOURCE_STATE_DIR DESTINATION_DIR PREVIOUS_RELEASE");
        return 64;
    };
    let previous = previous.to_string_lossy();
    match crate::store::backup::backup_into(source.as_ref(), destination.as_ref(), &previous) {
        Ok(report) => {
            println!("{}", serde_json::to_string(&report).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
