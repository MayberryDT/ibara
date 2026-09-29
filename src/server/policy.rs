//! `policy.json`, the gateway key and the admin digest (`server.ts:37-38,132-134,181-188`).
//!
//! Everything here is a startup snapshot except [`operator_grants`], which
//! re-reads the policy file on every operator call exactly as the TypeScript
//! controller did.

use crate::store::canonical::hex_encode;
use anyhow::{Context, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Limit keys that must be positive safe integers when present (`server.ts:132`).
pub const LIMIT_KEYS: [&str; 9] = [
    "max_process_output_chars",
    "max_artifact_bytes",
    "max_metadata_bytes",
    "metadata_headroom_bytes",
    "metadata_retention_ms",
    "min_free_bytes",
    "warn_free_bytes",
    "artifact_retention_ms",
    "job_log_retention_ms",
];

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The startup copy of `policy.json`.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The whole document, handed to the controller.
    pub raw: Value,
    principals: Vec<String>,
    /// Legacy `operator_credentials`: `(operator id, sha256 hex of its bearer)`.
    operator_credentials: Vec<(String, String)>,
}

impl Policy {
    pub fn load(path: &Path) -> anyhow::Result<Policy> {
        let text = std::fs::read(path).with_context(|| format!("read policy {}", path.display()))?;
        let raw: Value = serde_json::from_slice(&text).with_context(|| format!("parse policy {}", path.display()))?;
        Policy::from_value(raw)
    }

    pub fn from_value(raw: Value) -> anyhow::Result<Policy> {
        for key in LIMIT_KEYS {
            match raw.get(key) {
                None | Some(Value::Null) => {}
                Some(value) if safe_integer(value).is_some_and(|n| n >= 1.0) => {}
                Some(_) => bail!("Invalid configured limit: {key}"),
            }
        }
        let principals = raw
            .get("principals")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let operator_credentials = raw
            .get("operator_credentials")
            .and_then(Value::as_object)
            .map(|creds| {
                creds
                    .iter()
                    .filter_map(|(id, digest)| digest.as_str().map(|d| (id.clone(), d.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Policy { raw, principals, operator_credentials })
    }

    /// `policy.principals.includes(principal)`.
    pub fn allows_principal(&self, principal: &str) -> bool {
        self.principals.iter().any(|p| p == principal)
    }

    pub fn operator_credentials(&self) -> &[(String, String)] {
        &self.operator_credentials
    }
}

/// `Number.isSafeInteger(value) ? value : undefined`.
pub fn safe_integer(value: &Value) -> Option<f64> {
    let n = value.as_f64()?;
    (n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER).then_some(n)
}

/// `policy.json.operator_grants`, re-read from disk (`server.ts:144`).
pub fn policy_operator_grants(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read(path).ok()?;
    let policy: Value = serde_json::from_slice(&text).ok()?;
    match policy.get("operator_grants") {
        None | Some(Value::Null) => Some(Map::new()),
        Some(Value::Object(grants)) => Some(grants.clone()),
        Some(_) => Some(Map::new()),
    }
}

/// The two bearer checks for `controller.sock` and `admin.sock`.
#[derive(Clone)]
pub struct Keys {
    gateway: [u8; 32],
    admin: Vec<u8>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

impl Keys {
    /// Read the gateway token and the admin digest once, trimmed (`server.ts:181-182`).
    pub fn load(gateway_key: &Path, admin_hash: &Path) -> anyhow::Result<Keys> {
        let gateway = read_trimmed(gateway_key).with_context(|| format!("read gateway key {}", gateway_key.display()))?;
        let admin = read_trimmed(admin_hash).with_context(|| format!("read admin digest {}", admin_hash.display()))?;
        Ok(Keys::new(&gateway, &admin))
    }

    pub fn new(gateway_token: &str, admin_digest_hex: &str) -> Keys {
        Keys { gateway: sha256(gateway_token.as_bytes()), admin: unhex(admin_digest_hex).unwrap_or_default() }
    }

    /// `sha256(bearer) == sha256(gatewayToken)`, timing-safe.
    pub fn gateway_ok(&self, bearer: &str) -> bool {
        ct_eq(&sha256(bearer.as_bytes()), &self.gateway)
    }

    /// `sha256(bearer) == hex(admin.sha256)`, timing-safe; a malformed digest admits nobody.
    pub fn admin_ok(&self, bearer: &str) -> bool {
        ct_eq(&sha256(bearer.as_bytes()), &self.admin)
    }
}

/// Read a key file as UTF-8 and trim like JavaScript's `String.prototype.trim`.
pub fn read_trimmed(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(js_trim(&String::from_utf8_lossy(&bytes)).to_string())
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&sha256(bytes))
}

/// Constant-time comparison; different lengths never match.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decode lowercase or uppercase hex; `None` for odd length or a non-hex digit.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    s.as_bytes().chunks(2).map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_must_be_positive_safe_integers() {
        for bad in [json!(0), json!(-1), json!(1.5), json!("5"), json!(9_007_199_254_740_992.0f64), json!(true)] {
            let err = Policy::from_value(json!({ "principals": [], "max_artifact_bytes": bad })).unwrap_err();
            assert_eq!(err.to_string(), "Invalid configured limit: max_artifact_bytes");
        }
        Policy::from_value(json!({ "max_artifact_bytes": null, "min_free_bytes": 5.0, "principals": ["a"] })).unwrap();
    }

    #[test]
    fn malformed_admin_digest_admits_nobody() {
        let keys = Keys::new("gateway", "not-hex");
        assert!(!keys.admin_ok(""));
        assert!(!keys.admin_ok("not-hex"));
        let keys = Keys::new("gateway", &sha256_hex(b"admin"));
        assert!(!keys.admin_ok("Admin"));
        assert!(keys.admin_ok("admin"));
        assert!(!keys.gateway_ok("gateway "));
        assert!(keys.gateway_ok("gateway"));
    }
}
