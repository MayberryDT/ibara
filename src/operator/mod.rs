//! The operator-machine side of ibara (Vesper, Hazel, or a target acting as an operator).
//!
//! Ports of the Node client release in `controller/agent/`:
//!
//! | Module | Replaces |
//! |---|---|
//! | [`directory`] | `operator-directory.mjs` (the private `operator.sqlite`, schema 2) |
//! | [`transport`] | `ibara-transport` (the exact ssh argv for every route) |
//! | [`client`] | `ibara-client.mjs` + `ibara-client` wrapper (`ibara client …`) |
//! | [`sessions`] | `ibara-operator.mjs` (`ibara operator …`, `--serve`, [`sessions::OperatorSessions`]) |
//! | [`control`] | `ibara-control` (`ibara control …`) |
//! | [`mcp_route`] | `ibara-mcp` + `ibara-mcp-selected.mjs` (`ibara mcp …`, now a lazy relay for one computer or all of them) |
//! | [`onboarding`] | first-run: the prompt that connects an agent (`ibara prompt`), and milestones (`onboarding.json`) |
//!
//! Output is JSON with the same key order as the Node tools: `serde_json` is
//! built with `preserve_order`, so every `json!` literal and every relayed
//! target reply keeps its order.

pub mod client;
pub mod control;
pub mod directory;
pub mod mcp_route;
pub mod onboarding;
pub mod sessions;
pub mod transport;
pub mod work;

use crate::error::{IbaraError, is_known_code};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

/// Entry point for `ibara client|operator|control|mcp|prompt ARGS…`.
/// `args` excludes the program name and the subcommand.
pub fn run_cli(sub: &str, args: Vec<OsString>) -> ExitCode {
    // Node decodes argv as UTF-8 with replacement; do the same.
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    match sub {
        "client" => client::main(args),
        "work" => work::main(args),
        "operator" => sessions::main(args),
        "control" => control::main(args),
        "mcp" => mcp_route::main(args),
        "prompt" => onboarding::prompt_main(args),
        _ => {
            eprintln!("Usage: ibara client|operator|control|mcp|prompt …");
            ExitCode::from(64)
        }
    }
}

/// A current-thread runtime for one short-lived command.
pub(crate) fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}

// ---------------------------------------------------------------------------
// Errors. The Node tools print `error.message`; the message text is the
// compatibility surface, the code classifies it for in-process callers.

pub(crate) fn fail(message: impl Into<String>) -> IbaraError {
    IbaraError::new("INVALID_ARGUMENT", message, false)
}
pub(crate) fn unsafe_file(message: impl Into<String>) -> IbaraError {
    IbaraError::new("PERMISSION_DENIED", message, false)
}
pub(crate) fn unreachable_route(message: impl Into<String>) -> IbaraError {
    IbaraError::new("SESSION_UNAVAILABLE", message, true)
}
/// A refusal reported by the target with its own code (kept in the message).
pub(crate) fn refused(code: &str, message: String) -> IbaraError {
    let registered = crate::error::CODES.iter().find(|(c, _)| *c == code).map(|(c, _)| *c);
    match registered {
        Some(code) if is_known_code(code) => IbaraError::new(code, message, false),
        _ => IbaraError::new("SESSION_UNAVAILABLE", message, false),
    }
}

/// Print a failure the way the Node tools do (`console.error(message)`) and exit 1.
pub(crate) fn exit_with(error: &IbaraError) -> ExitCode {
    eprintln!("{}", error.message);
    ExitCode::from(1)
}

// ---------------------------------------------------------------------------
// Process identity and paths.

pub(crate) fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `os.homedir()`: `$HOME` when set, else the passwd entry.
pub(crate) fn home_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// `path.resolve(p)`: absolute against the working directory, lexically normalised.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    let joined;
    let path = if path.is_absolute() {
        path
    } else {
        joined = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path);
        &joined
    };
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// `value.replace(/^~(?=\/)/, home)`.
pub(crate) fn expand_home(value: &str) -> String {
    match value.strip_prefix('~') {
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home_dir().display()),
        _ => value.to_string(),
    }
}

/// `process.env.NAME || fallback` (an empty value counts as unset).
pub(crate) fn env_or(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// Replace `path`'s contents atomically: write a sibling temporary file, sync it and
/// rename it over the original. A symlinked file is replaced at its target, so the
/// link survives; an existing file keeps its permissions, a new one gets `new_mode`.
pub(crate) fn replace_file(path: &Path, bytes: &[u8], new_mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(new_mode);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.ibara-{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temp)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

// ---------------------------------------------------------------------------
// JavaScript value semantics the Node tools relied on.

pub(crate) mod js {
    use serde_json::Value;

    /// JS `Boolean(value)`; `None` is `undefined`.
    pub fn truthy(value: Option<&Value>) -> bool {
        match value {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(_)) | Some(Value::Object(_)) => true,
        }
    }

    /// JS `String(value)` for a present value.
    pub fn string(value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_string(n),
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .map(|v| if v.is_null() { String::new() } else { string(v) })
                .collect::<Vec<_>>()
                .join(","),
            Value::Object(_) => "[object Object]".into(),
        }
    }

    /// JS `String(value ?? fallback)`.
    pub fn string_or(value: Option<&Value>, fallback: &str) -> String {
        match value {
            None | Some(Value::Null) => fallback.to_string(),
            Some(v) => string(v),
        }
    }

    fn number_string(n: &serde_json::Number) -> String {
        if n.is_i64() || n.is_u64() {
            return n.to_string();
        }
        let f = n.as_f64().unwrap_or(f64::NAN);
        if f == 0.0 {
            "0".into()
        } else if f.fract() == 0.0 && f.abs() < 1e21 {
            format!("{}", f as i128)
        } else if f.abs() >= 1e21 || f.abs() < 1e-6 {
            let e = format!("{f:e}");
            match e.split_once('e') {
                Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x}"),
                _ => e,
            }
        } else {
            format!("{f}")
        }
    }

    /// JS `Number(value)`; `None` is `undefined` (NaN).
    pub fn number(value: Option<&Value>) -> f64 {
        match value {
            None => f64::NAN,
            Some(Value::Null) => 0.0,
            Some(Value::Bool(b)) => f64::from(u8::from(*b)),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
            Some(Value::String(s)) => {
                let t = trim(s);
                if t.is_empty() {
                    0.0
                } else if t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
                    t.parse().unwrap_or(f64::NAN)
                } else {
                    f64::NAN
                }
            }
            Some(_) => f64::NAN,
        }
    }

    pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

    /// `Number.isSafeInteger(n)`.
    pub fn is_safe_integer(n: f64) -> bool {
        n.is_finite() && n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER
    }

    /// `Number.isSafeInteger(value)` without coercion.
    pub fn safe_integer(value: Option<&Value>) -> Option<i64> {
        match value {
            Some(Value::Number(n)) => n.as_f64().filter(|f| is_safe_integer(*f)).map(|f| f as i64),
            _ => None,
        }
    }

    /// `a === b` for numbers.
    pub fn same_number(value: Option<&Value>, expected: f64) -> bool {
        matches!(value, Some(Value::Number(n)) if n.as_f64() == Some(expected))
    }

    fn is_space(c: char) -> bool {
        matches!(
            c,
            '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
        )
    }

    /// `String.prototype.trim`.
    pub fn trim(s: &str) -> &str {
        s.trim_matches(is_space)
    }

    /// `/\s/` membership.
    pub fn is_js_space(c: char) -> bool {
        is_space(c)
    }

    /// `s.length` (UTF-16 code units).
    pub fn length(s: &str) -> usize {
        s.encode_utf16().count()
    }

    /// `s.slice(0, n)` by UTF-16 code units (a split pair becomes U+FFFD, as SQLite would store it).
    pub fn slice_units(s: &str, n: usize) -> String {
        let units: Vec<u16> = s.encode_utf16().take(n).collect();
        String::from_utf16_lossy(&units)
    }

    /// `Object.keys(v).sort().join(',') === keys.sort().join(',')` for a plain object.
    pub fn has_exact_keys(value: &Value, keys: &[&str]) -> bool {
        let Value::Object(map) = value else { return false };
        let mut actual: Vec<&str> = map.keys().map(String::as_str).collect();
        actual.sort_unstable();
        let mut wanted = keys.to_vec();
        wanted.sort_unstable();
        actual == wanted
    }

    /// `JSON.stringify(value)` with integral floats printed the way JavaScript prints them.
    pub fn stringify(value: &Value) -> String {
        fn integral(value: &Value) -> Value {
            match value {
                Value::Number(n) if !n.is_i64() && !n.is_u64() => match n.as_f64() {
                    Some(f) if f.fract() == 0.0 && f.abs() <= super::js::MAX_SAFE_INTEGER => Value::from(f as i64),
                    _ => value.clone(),
                },
                Value::Array(items) => Value::Array(items.iter().map(integral).collect()),
                Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), integral(v))).collect()),
                _ => value.clone(),
            }
        }
        integral(value).to_string()
    }
}

// ---------------------------------------------------------------------------
// The anchored patterns of the Node tools, as predicates (no regex engine).

pub(crate) mod pattern {
    fn id_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-')
    }

    /// `^[A-Za-z0-9_.:-]{1,128}$`
    pub fn id(s: &str) -> bool {
        (1..=128).contains(&s.len()) && s.bytes().all(id_char)
    }

    /// `^[A-Za-z0-9_.:-]{8,256}$`
    pub fn endpoint_id(s: &str) -> bool {
        (8..=256).contains(&s.len()) && s.bytes().all(id_char)
    }

    /// `^[A-Za-z0-9][A-Za-z0-9.:-]{0,252}$`
    pub fn host(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 253
            && b[0].is_ascii_alphanumeric()
            && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b':' | b'-'))
    }

    /// `^[A-Za-z_][A-Za-z0-9_.-]{0,63}$` (route user, station accounts)
    pub fn account(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 64
            && (b[0].is_ascii_alphabetic() || b[0] == b'_')
            && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    }

    /// `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$` (station node)
    pub fn node(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 64
            && b[0].is_ascii_alphanumeric()
            && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    }

    /// `^[a-z][a-z0-9_-]{0,22}$` (operator principal)
    pub fn principal(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= 23
            && b[0].is_ascii_lowercase()
            && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
    }

    /// `^[A-Za-z0-9@_.:-]{1,128}\.service$`
    pub fn service_unit(s: &str) -> bool {
        s.strip_suffix(".service").is_some_and(|stem| {
            (1..=128).contains(&stem.len()) && stem.bytes().all(|b| id_char(b) || b == b'@')
        })
    }

    /// `^[a-f0-9]{n}$`
    pub fn lower_hex(s: &str, n: usize) -> bool {
        s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// `^\d+$`
    pub fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }

    /// `^(?:none|human|agent:[C]+:[C]+|operator:[C]+)$` with C = `[A-Za-z0-9_.:-]`.
    pub fn owner(s: &str) -> bool {
        if s == "none" || s == "human" {
            return true;
        }
        if let Some(rest) = s.strip_prefix("operator:") {
            return !rest.is_empty() && rest.bytes().all(id_char);
        }
        if let Some(rest) = s.strip_prefix("agent:") {
            let b = rest.as_bytes();
            return b.iter().all(|c| id_char(*c)) && b.len() >= 3 && b[1..b.len() - 1].contains(&b':');
        }
        false
    }

    /// `^pair_[a-f0-9]{32}$`
    pub fn challenge_ref(s: &str) -> bool {
        s.strip_prefix("pair_").is_some_and(|hex| lower_hex(hex, 32))
    }

    /// `^[A-Za-z0-9+/_=.:-]{16,128}$`
    pub fn key_fingerprint(s: &str) -> bool {
        (16..=128).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'=' | b'.' | b':' | b'-'))
    }

    /// `/^file:\/.+/.test(s) && !s.includes('\0')`
    pub fn file_ref(s: &str) -> bool {
        s.strip_prefix("file:/")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            && !s.contains('\0')
    }
}

/// `JSON.parse(text)`; the error message is what the Node tools printed in spirit.
pub(crate) fn parse_json(text: &str) -> Result<Value, IbaraError> {
    serde_json::from_str(text).map_err(|e| fail(format!("Invalid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::pattern;

    #[test]
    fn owner_pattern_matches_the_client_regex() {
        for ok in ["none", "human", "operator:vesper", "agent:codex:task_1", "agent:a:b:c"] {
            assert!(pattern::owner(ok), "{ok}");
        }
        for bad in ["", "operator:", "agent:a", "agent::b", "agent:a:", "agent:ab", "root", "agent:a b:c"] {
            assert!(!pattern::owner(bad), "{bad}");
        }
    }
}

/// One newline-delimited read with a hard bound.
#[derive(Debug, PartialEq)]
pub(crate) enum LineRead {
    Line(Vec<u8>),
    /// The line exceeded the bound; the bytes read so far were discarded.
    Oversize,
    Eof,
}

/// Read one line (without its `\n`) holding at most `limit` bytes. With
/// `partial_at_eof`, bytes after the last newline are returned as a final line
/// (Node `readline`); without, they are dropped (a raw `data` splitter).
pub(crate) async fn read_line_bounded<R>(reader: &mut R, limit: usize, partial_at_eof: bool) -> std::io::Result<LineRead>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if partial_at_eof && !line.is_empty() { LineRead::Line(line) } else { LineRead::Eof });
        }
        let (take, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (buf.len(), false),
        };
        if line.len() + take > limit {
            let consumed = if done { take + 1 } else { take };
            reader.consume(consumed);
            return Ok(LineRead::Oversize);
        }
        line.extend_from_slice(&buf[..take]);
        reader.consume(if done { take + 1 } else { take });
        if done {
            return Ok(LineRead::Line(line));
        }
    }
}
