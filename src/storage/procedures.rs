//! Procedures: candidates, promotion, approved files and their projection
//! (procedures.ts, and storage.ts:1127-1147, 1216-1294, 1371-1446, 2288-2338).
//!
//! Records are JSON objects kept in insertion order (serde_json
//! `preserve_order`), because digests hash `JSON.stringify(definition)` in
//! stored key order.

use super::fsx::{self, O_RDONLY, fail, fd_ref};
use super::jsv::{self, spread, str_or, to_js_string};
use super::{Context, StorageService, ToolResult};
use crate::error::Result;
use crate::ids::{id, now_iso, now_millis};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

/// procedures.ts:16.
pub const MAX_PROCEDURE_BYTES: u64 = 256 * 1024;

/// A procedure record (procedures.ts:35-54) as an ordered JSON object.
pub type Record = Map<String, Value>;

/// procedures.ts:60: availability of one piece of evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceState {
    /// `current`, `expired` or `missing`.
    pub state: String,
    /// `satisfied` or `unsatisfied`.
    pub outcome: Option<String>,
}

impl EvidenceState {
    fn is_current_satisfied(&self) -> bool {
        self.state == "current" && self.outcome.as_deref() == Some("satisfied")
    }
}

/// procedures.ts:56-61 `ProcedureEnvironment`.
#[derive(Debug, Clone, Default)]
pub struct ProcedureEnvironment {
    pub capabilities: Vec<String>,
    pub scopes: Vec<String>,
    pub versions: BTreeMap<String, String>,
    pub evidence: BTreeMap<String, EvidenceState>,
}

/// procedures.ts:64-73 `procedureEvidenceState`: availability is separate from
/// a successful check; failed checks can contradict.
pub fn procedure_evidence_state(item: Option<&Value>, current_ms: i64) -> EvidenceState {
    let missing = EvidenceState { state: "missing".into(), outcome: None };
    let Some(item) = item.filter(|i| i.get("kind") == Some(&json!("check"))) else { return missing };
    if jsv::truthy(item.get("expired")) {
        return EvidenceState { state: "expired".into(), outcome: None };
    }
    let record = item.get("record").cloned().unwrap_or(json!({}));
    let source = record.get("source").and_then(Value::as_str).unwrap_or("");
    let outcome = record.get("outcome").and_then(Value::as_str).unwrap_or("");
    if !["playwright", "chrome_extension", "at_spi", "screen", "process", "filesystem", "fixture"].contains(&source)
        || !["satisfied", "unsatisfied"].contains(&outcome)
    {
        return missing;
    }
    let Some(checked) = record.get("checked_at").and_then(Value::as_str).and_then(jsv::parse_iso_millis) else {
        return missing;
    };
    let age = current_ms - checked;
    if age < 0 {
        return missing;
    }
    EvidenceState { state: if age <= 300_000 { "current" } else { "expired" }.into(), outcome: Some(outcome.into()) }
}

fn s<'a>(r: &'a Record, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str)
}

fn truthy(r: &Record, k: &str) -> bool {
    jsv::truthy(r.get(k))
}

fn def(r: &Record) -> Record {
    match r.get("definition") {
        Some(Value::Object(d)) => d.clone(),
        _ => Map::new(),
    }
}

fn str_list(d: &Record, k: &str) -> Vec<String> {
    match d.get(k) {
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn invalid(message: impl Into<String>) -> crate::error::IbaraError {
    fail("INVALID_ARGUMENT", message, true)
}

/// procedures.ts:124-136 `asStringArray`.
fn as_string_array(value: Option<&Value>, field: &str, min: usize, max: usize, item_max: usize) -> Result<Vec<Value>> {
    let Some(Value::Array(items)) = value else { return Err(invalid(format!("{field} must be an array."))) };
    if items.len() < min || items.len() > max {
        return Err(invalid(format!("{field} is outside the allowed length.")));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str().filter(|t| !t.is_empty() && jsv::utf16_len(t) <= item_max) else {
            return Err(invalid(format!("{field}[{index}] is invalid.")));
        };
        if jsv::forbidden_ref(text) {
            return Err(invalid(format!("{field} cannot store element or frame references; resolve targets afresh.")));
        }
        out.push(json!(text));
    }
    Ok(out)
}

/// `raw.key ?? []`.
fn or_empty(raw: &Value, key: &str) -> Value {
    match raw.get(key) {
        None | Some(Value::Null) => json!([]),
        Some(v) => v.clone(),
    }
}

/// procedures.ts:175-182 `typedRequirements`.
fn typed_requirements(raw: &Value, field: &str) -> Result<Vec<Value>> {
    let Value::Array(items) = raw else { return Err(invalid(format!("{field} must be a bounded array."))) };
    if items.len() > 20 {
        return Err(invalid(format!("{field} must be a bounded array.")));
    }
    items
        .iter()
        .map(|item| {
            let bad = || invalid("Invalid typed hard prerequisite.");
            let Value::Object(o) = item else { return Err(bad()) };
            let kind = o.get("kind").and_then(Value::as_str);
            let value = o.get("value").and_then(Value::as_str);
            match (kind, value) {
                (Some(k @ ("capability" | "scope")), Some(v))
                    if o.keys().all(|key| key == "kind" || key == "value")
                        && !v.is_empty()
                        && jsv::utf16_len(v) <= 128
                        && !jsv::forbidden_ref(v) =>
                {
                    Ok(json!({ "kind": k, "value": v }))
                }
                _ => Err(bad()),
            }
        })
        .collect()
}

/// procedures.ts:184-191 `typedVersions`.
fn typed_versions(raw: &Value) -> Result<Vec<Value>> {
    let Value::Array(items) = raw else { return Err(invalid("tested_versions must be a bounded array.")) };
    if items.len() > 20 {
        return Err(invalid("tested_versions must be a bounded array."));
    }
    items
        .iter()
        .map(|item| {
            let bad = || invalid("Invalid tested version.");
            let Value::Object(o) = item else { return Err(bad()) };
            let ok = |k: &str| {
                o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty() && jsv::utf16_len(v) <= 128 && !jsv::forbidden_ref(v))
            };
            match (ok("component"), ok("version")) {
                (Some(c), Some(v)) if o.keys().all(|key| key == "component" || key == "version") => {
                    Ok(json!({ "component": c, "version": v }))
                }
                _ => Err(bad()),
            }
        })
        .collect()
}

/// procedures.ts:138-173 `validateCandidate`. Refuses self-approval, element and
/// frame references, and out-of-bounds lists; returns the definition in
/// canonical key order.
pub fn validate_candidate(raw: &Value) -> Result<Record> {
    let Value::Object(obj) = raw else { return Err(invalid("Procedure candidate must be an object.")) };
    if obj.contains_key("approved") || obj.get("status") == Some(&json!("approved")) {
        return Err(invalid("A candidate cannot carry an approved field or self-promotion status."));
    }
    let title = match obj.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() && jsv::utf16_len(t) <= 160 => t,
        _ => return Err(invalid("Procedure title is required and must be at most 160 characters.")),
    };
    if jsv::forbidden_ref(title) {
        return Err(invalid("Procedure title cannot store element or frame references."));
    }
    let mut d = Map::new();
    d.insert("title".into(), json!(title));
    d.insert("inputs".into(), Value::Array(as_string_array(Some(&or_empty(raw, "inputs")), "inputs", 0, 20, 500)?));
    for (key, min, max, item_max) in [
        ("applicability", 1, 20, 500),
        ("steps", 1, 30, 1000),
        ("expected_outputs", 1, 20, 500),
        ("verification", 1, 20, 1000),
        ("stop_conditions", 1, 20, 1000),
        ("evidence_refs", 1, 20, 128),
    ] {
        d.insert(key.into(), Value::Array(as_string_array(obj.get(key), key, min, max, item_max)?));
    }
    d.insert(
        "prerequisites".into(),
        Value::Array(as_string_array(Some(&or_empty(raw, "prerequisites")), "prerequisites", 0, 20, 500)?),
    );
    d.insert("contradicts".into(), Value::Array(as_string_array(Some(&or_empty(raw, "contradicts")), "contradicts", 0, 20, 128)?));
    d.insert(
        "hard_prerequisites".into(),
        Value::Array(typed_requirements(&or_empty(raw, "hard_prerequisites"), "hard_prerequisites")?),
    );
    d.insert("tested_versions".into(), Value::Array(typed_versions(&or_empty(raw, "tested_versions"))?));
    match obj.get("expires_at") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let ms = v.as_str().and_then(jsv::parse_iso_millis).ok_or_else(|| invalid("expires_at must be an ISO timestamp."))?;
            d.insert("expires_at".into(), json!(crate::ids::iso_from_millis(ms)));
        }
    }
    if str_list(&d, "evidence_refs").iter().any(|r| !jsv::id_re(r)) {
        return Err(invalid("evidence_refs must be opaque controller identifiers."));
    }
    Ok(d)
}

const DEFINITION_KEYS: [&str; 13] = [
    "title",
    "inputs",
    "applicability",
    "steps",
    "expected_outputs",
    "verification",
    "stop_conditions",
    "evidence_refs",
    "prerequisites",
    "contradicts",
    "hard_prerequisites",
    "tested_versions",
    "expires_at",
];

/// procedures.ts:194-214 `normalizeProcedureRecord`: normalize durable legacy
/// records without turning missing facts into proof.
pub fn normalize_procedure_record(raw: &Value) -> Record {
    let raw_obj = match raw {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    let mut d: Record = match raw_obj.get("definition") {
        Some(Value::Object(o)) => o.iter().filter(|(k, _)| DEFINITION_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => Map::new(),
    };
    let mut legacy = truthy(&raw_obj, "legacy_unverified");
    for key in ["inputs", "applicability", "steps", "expected_outputs", "verification", "stop_conditions", "evidence_refs", "prerequisites", "contradicts"] {
        match d.get(key) {
            Some(Value::Array(items)) => {
                let kept: Vec<Value> = items.iter().filter(|v| v.is_string()).cloned().collect();
                d.insert(key.into(), Value::Array(kept));
            }
            _ => {
                d.insert(key.into(), json!([]));
                legacy = true;
            }
        }
    }
    if !d.get("title").is_some_and(Value::is_string) {
        d.insert("title".into(), json!("Legacy procedure"));
        legacy = true;
    }
    for key in ["hard_prerequisites", "tested_versions"] {
        if !d.get(key).is_some_and(Value::is_array) {
            d.insert(key.into(), json!([]));
            legacy = true;
        }
    }
    match (
        typed_requirements(&d["hard_prerequisites"], "hard_prerequisites"),
        typed_versions(&d["tested_versions"]),
    ) {
        (Ok(hard), Ok(versions)) => {
            d.insert("hard_prerequisites".into(), Value::Array(hard));
            d.insert("tested_versions".into(), Value::Array(versions));
        }
        _ => {
            d.insert("hard_prerequisites".into(), json!([]));
            d.insert("tested_versions".into(), json!([]));
            legacy = true;
        }
    }
    if jsv::truthy(d.get("expires_at")) && d.get("expires_at").and_then(Value::as_str).and_then(jsv::parse_iso_millis).is_none() {
        d.shift_remove("expires_at");
        legacy = true;
    }
    let mut over = Map::new();
    over.insert("kind".into(), json!("procedure"));
    over.insert("definition".into(), Value::Object(d));
    over.insert("legacy_unverified".into(), json!(legacy));
    let mut record = spread(&raw_obj, &over);
    if !matches!(s(&record, "status"), Some("candidate" | "approved" | "quarantined")) {
        record.insert("status".into(), json!("quarantined"));
        record.insert("legacy_unverified".into(), json!(true));
    }
    if s(&record, "status") != Some("approved") && (!truthy(&record, "owner_task_ref") || !truthy(&record, "owner_principal")) {
        if s(&record, "status") == Some("candidate") {
            record.insert("legacy_unowned".into(), json!(true));
        }
        record.insert("status".into(), json!("quarantined"));
        record.insert("legacy_unverified".into(), json!(true));
    }
    record
}

/// procedures.ts:216-218 `procedureDigest`: SHA-256 of `JSON.stringify(definition)`.
pub fn procedure_digest(record: &Record) -> String {
    let text = serde_json::to_string(record.get("definition").unwrap_or(&Value::Null)).unwrap_or_default();
    fsx::sha256_hex(text.as_bytes())
}

/// procedures.ts:221-230 `publicProcedureRecord`: guidance only, never private provenance.
pub fn public_procedure_record(record: &Record) -> Record {
    let mut d = def(record);
    d.insert("evidence_refs".into(), json!([]));
    d.insert("contradicts".into(), json!([]));
    let mut p = Map::new();
    p.insert("kind".into(), json!("procedure"));
    p.insert("procedure_ref".into(), record.get("procedure_ref").cloned().unwrap_or(Value::Null));
    p.insert("version".into(), record.get("version").cloned().unwrap_or(Value::Null));
    p.insert("status".into(), json!("approved"));
    p.insert("definition".into(), Value::Object(d));
    for (to, from) in [("approval_source_digest", "approval_digest"), ("approval_actor", "approval_actor"), ("approved_at", "approved_at")] {
        if let Some(v) = record.get(from) {
            p.insert(to.into(), v.clone());
        }
    }
    let digest = procedure_digest(&p);
    p.insert("approval_digest".into(), json!(digest));
    p
}

/// Options of `projectProcedure`.
#[derive(Default)]
pub struct Projection<'a> {
    pub environment: Option<&'a ProcedureEnvironment>,
    pub task_ref: Option<&'a str>,
    pub private_evidence: bool,
    pub summary: bool,
}

/// procedures.ts:232-259 `projectProcedure`: the public view with applicability,
/// expiry and contradiction state. Every result requires fresh verification.
pub fn project_procedure(record: &Record, input: &Projection<'_>) -> Value {
    let digest = procedure_digest(record);
    let env = input.environment;
    let d = def(record);
    let ev = |r: &str| env.and_then(|e| e.evidence.get(r));
    let mut reasons: Vec<String> = Vec::new();
    let hard = match d.get("hard_prerequisites") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let failed = hard.iter().any(|item| {
        env.is_some_and(|e| {
            let list = if item.get("kind") == Some(&json!("capability")) { &e.capabilities } else { &e.scopes };
            !item.get("value").and_then(Value::as_str).is_some_and(|v| list.iter().any(|x| x == v))
        })
    });
    if failed {
        reasons.push("hard_prerequisite_failed".into());
    }
    if env.is_none() {
        reasons.push("current_environment_unavailable".into());
    }
    if truthy(record, "legacy_unverified") || s(record, "approval_digest") != Some(digest.as_str()) {
        reasons.push("legacy_or_unverified_approval".into());
    }
    let last_validation = s(record, "last_validation_ref").filter(|v| !v.is_empty());
    match last_validation {
        None => reasons.push("validation_record_missing".into()),
        Some(v) if !input.private_evidence || !ev(v).is_some_and(EvidenceState::is_current_satisfied) => {
            reasons.push("validation_record_requires_revalidation".into());
        }
        Some(_) => {}
    }
    if !str_list(&d, "prerequisites").is_empty() {
        reasons.push("prose_prerequisites_require_validation".into());
    }
    let tested = match d.get("tested_versions") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    if tested.is_empty() {
        reasons.push("tested_versions_missing".into());
    }
    for t in &tested {
        let component = t.get("component").map(to_js_string).unwrap_or_else(|| "undefined".into());
        let wanted = t.get("version").and_then(Value::as_str);
        if env.and_then(|e| e.versions.get(&component)).map(String::as_str) != wanted {
            reasons.push(format!("version_revalidation:{component}"));
        }
    }
    let evidence_refs = str_list(&d, "evidence_refs");
    let expires = d.get("expires_at").and_then(Value::as_str).and_then(jsv::parse_iso_millis);
    let expired = expires.is_some_and(|ms| ms <= now_millis())
        || (input.private_evidence && evidence_refs.iter().any(|r| ev(r).is_some_and(|e| e.state == "expired")));
    if expired {
        reasons.push("evidence_expired".into());
    }
    if evidence_refs.is_empty()
        || !input.private_evidence
        || evidence_refs.iter().any(|r| !ev(r).is_some_and(EvidenceState::is_current_satisfied))
    {
        reasons.push("supporting_evidence_requires_validation".into());
    }
    let contradictions = match record.get("contradictions") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|c| {
                input.task_ref.is_some_and(|t| c.get("task_ref") == Some(&json!(t)))
                    && c.get("content_sha256") == Some(&json!(digest))
            })
            .count(),
        _ => 0,
    };
    if contradictions > 0 {
        reasons.push("scoped_contradiction_requires_validation".into());
    }
    let status = s(record, "status").unwrap_or("");
    let quarantined = status == "quarantined" || contradictions >= 2;
    let state = if quarantined {
        "quarantined"
    } else if status != "approved" {
        "not_approved"
    } else if failed {
        "prerequisite_failed"
    } else if !reasons.is_empty() {
        "revalidation_required"
    } else {
        "applicable"
    };
    let mut definition = d.clone();
    if !input.private_evidence {
        definition.insert("evidence_refs".into(), json!([]));
        definition.insert("contradicts".into(), json!([]));
    } else {
        definition.insert("evidence_refs".into(), d.get("evidence_refs").cloned().unwrap_or(Value::Null));
        definition.insert("contradicts".into(), d.get("contradicts").cloned().unwrap_or(Value::Null));
    }
    if input.summary {
        let steps: Vec<Value> = str_list(&d, "steps").iter().take(3).map(|st| json!(jsv::clip(st, 200))).collect();
        definition.insert("steps".into(), Value::Array(steps));
    }
    let mut seen = HashSet::new();
    reasons.retain(|r| seen.insert(r.clone()));
    let mut out = Map::new();
    out.insert("kind".into(), json!("procedure"));
    out.insert("procedure_ref".into(), record.get("procedure_ref").cloned().unwrap_or(Value::Null));
    out.insert("version".into(), record.get("version").cloned().unwrap_or(Value::Null));
    out.insert("status".into(), if quarantined { json!("quarantined") } else { record.get("status").cloned().unwrap_or(Value::Null) });
    out.insert("definition".into(), Value::Object(definition));
    out.insert("content_sha256".into(), json!(digest));
    out.insert("applicability_state".into(), json!(state));
    out.insert("applicability_reasons".into(), json!(reasons));
    out.insert("fresh_verification_required".into(), json!(true));
    if !input.private_evidence {
        out.insert("evidence_redacted".into(), json!(true));
    }
    out.insert("expiry_state".into(), json!(if expired { "expired" } else { "current" }));
    out.insert("contradiction_state".into(), json!(if contradictions > 0 { "declared" } else { "none" }));
    if input.private_evidence
        && let Some(v) = last_validation
    {
        out.insert("last_validation_ref".into(), json!(v));
    }
    Value::Object(out)
}

/// procedures.ts:261-281 `procedureRecord`.
pub fn procedure_record(procedure_ref: Option<&str>, version: &str, status: &str, definition: Record, last_validation_ref: Option<&str>) -> Record {
    let mut r = Map::new();
    r.insert("kind".into(), json!("procedure"));
    r.insert("procedure_ref".into(), json!(procedure_ref.filter(|p| !p.is_empty()).map_or_else(|| id("procedure"), str::to_string)));
    r.insert("version".into(), json!(version));
    r.insert("status".into(), json!(status));
    r.insert("definition".into(), Value::Object(definition));
    if let Some(v) = last_validation_ref.filter(|v| !v.is_empty()) {
        r.insert("last_validation_ref".into(), json!(v));
    }
    r
}

/// procedures.ts:283-285 `tokenize`.
pub fn tokenize(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|p| p.len() >= 2)
        .map(str::to_string)
        .collect()
}

/// procedures.ts:287-304 `scoreProcedure`.
pub fn score_procedure(record: &Record, tokens: &[String]) -> i64 {
    if tokens.is_empty() {
        return 0;
    }
    let d = def(record);
    let title = d.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let mut hay = vec![title.clone()];
    for k in ["applicability", "steps", "expected_outputs"] {
        hay.extend(str_list(&d, k));
    }
    hay.push(record.get("version").map(to_js_string).unwrap_or_default());
    hay.push(record.get("status").map(to_js_string).unwrap_or_default());
    let hay = hay.join("\n").to_lowercase();
    tokens.iter().map(|t| i64::from(title.contains(t.as_str())) * 5 + i64::from(hay.contains(t.as_str()))).sum()
}

/// procedures.ts:306-313 `safeProcedureFileName`.
pub fn safe_procedure_file_name(procedure_ref: &str, version: &str) -> Result<String> {
    if !jsv::id_re(procedure_ref) || jsv::utf16_len(procedure_ref) > 128 {
        return Err(invalid("procedure_ref is not a safe identifier."));
    }
    let mut ver = String::new();
    let mut in_run = false;
    for c in version.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            ver.push(c);
            in_run = false;
        } else if !in_run {
            ver.push('_');
            in_run = true;
        }
    }
    let ver: String = jsv::clip(&ver, 50).to_string();
    if ver.is_empty() {
        return Err(invalid("Procedure version is invalid."));
    }
    Ok(format!("{procedure_ref}.{ver}.json"))
}

fn read_all(fd: BorrowedFd<'_>, size: u64) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size as usize];
    let mut off = 0;
    while off < buf.len() {
        let n = fsx::pread(fd, &mut buf[off..], off as u64)?;
        if n == 0 {
            break;
        }
        off += n;
    }
    buf.truncate(off);
    Ok(buf)
}

/// procedures.ts:315-332 `readJsonFileFd`.
pub fn read_json_file_fd(fd: BorrowedFd<'_>, max_bytes: u64) -> Result<Value> {
    let st = fsx::fstat(fd)?;
    if !st.is_file() || st.size > max_bytes {
        return Err(invalid("Procedure file is missing, not a regular file, or too large."));
    }
    let bytes = read_all(fd, st.size)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("Procedure file is not valid JSON."))
}

/// procedures.ts:351-381 `loadCandidateFile`: the parsed JSON, its SHA-256 and size.
pub fn load_candidate_file(candidate_dir: &Path, candidate_name: &str) -> Result<(Value, String, u64)> {
    if !jsv::rel_re(candidate_name) || candidate_name.contains('/') || !candidate_name.ends_with(".json") {
        return Err(invalid("Candidate name must be a json file in the candidate directory."));
    }
    let dir = fsx::open_dir(&std::path::absolute(candidate_dir).unwrap_or_else(|_| candidate_dir.to_path_buf()))?;
    let loaded = (|| -> Result<(Value, String, u64)> {
        let fd = fsx::open_child(fd_ref(&dir), candidate_name, O_RDONLY | fsx::O_NONBLOCK, 0)??;
        let st = fsx::fstat(fd_ref(&fd))?;
        if !st.is_file() || st.size > MAX_PROCEDURE_BYTES {
            return Err(invalid("Candidate is not a regular file within size limits."));
        }
        let data = read_all(fd_ref(&fd), st.size)?;
        let json = serde_json::from_slice(&data).map_err(|_| invalid("Unable to read candidate procedure file."))?;
        Ok((json, fsx::sha256_hex(&data), data.len() as u64))
    })();
    loaded.map_err(|e| if e.code == "INTERNAL_ERROR" { invalid("Unable to read candidate procedure file.") } else { e })
}

fn mkdir_mode(dir: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(mode).create(dir)?;
    Ok(())
}

fn pretty(v: &impl serde::Serialize) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(v).unwrap_or_default();
    text.push('\n');
    text.into_bytes()
}

/// procedures.ts:387-470 `installApprovedProcedure`: the root-only installer.
/// Copies a validated candidate into the approved store under its derived
/// name; the request cannot name an arbitrary path. Never opens SQLite.
pub fn install_approved_procedure(request_path: &Path, approved_dir: &Path, candidate_dir: &Path) -> Result<Value> {
    let request_path = std::path::absolute(request_path).unwrap_or_else(|_| request_path.to_path_buf());
    let request_dir = request_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    let request_name = request_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let request = {
        let dir = fsx::open_dir(&request_dir)?;
        let fd = fsx::open_child(fd_ref(&dir), &request_name, O_RDONLY | fsx::O_NONBLOCK, 0)??;
        read_json_file_fd(fd_ref(&fd), MAX_PROCEDURE_BYTES)?
    };
    let field = |k: &str| request.get(k).map_or_else(|| "undefined".to_string(), to_js_string);
    if request.get("kind") != Some(&json!("promotion_request"))
        || !jsv::id_re(&field("procedure_ref"))
        || !jsv::truthy(request.get("source_sha256"))
        || !jsv::truthy(request.get("candidate_name"))
    {
        return Err(invalid("Promotion request is malformed."));
    }
    let procedure_ref = field("procedure_ref");
    let version = field("version");
    let dest_name = safe_procedure_file_name(&procedure_ref, &version)?;
    if request.get("destination_name") != Some(&json!(dest_name)) {
        return Err(invalid("Promotion destination name is not the derived safe filename."));
    }
    let (raw, sha, size) = load_candidate_file(candidate_dir, &field("candidate_name"))?;
    if request.get("source_sha256") != Some(&json!(sha)) || !jsv::strict_eq_num(request.get("source_bytes"), size as f64) {
        return Err(fail("POSTCONDITION_FAILED", "Candidate changed after the promotion request was prepared.", false));
    }
    let definition = validate_candidate(match raw.get("definition") {
        None | Some(Value::Null) => &raw,
        Some(d) => d,
    })?;
    let raw_ref = str_or(&raw, "procedure_ref", &procedure_ref);
    if raw_ref != procedure_ref {
        return Err(invalid("Candidate procedure_ref does not match the request."));
    }
    let validation = request.get("validation_ref").filter(|v| jsv::truthy(Some(v))).map(to_js_string);
    let mut approved = procedure_record(Some(&procedure_ref), &version, "approved", definition, validation.as_deref());
    let digest = procedure_digest(&approved);
    approved.insert("approval_digest".into(), json!(digest));
    approved.insert("approval_actor".into(), json!("operator"));
    approved.insert("approved_at".into(), json!(now_iso()));
    if let (Some(Value::String(t)), Some(Value::String(p))) = (raw.get("owner_task_ref"), raw.get("owner_principal")) {
        approved.insert("owner_task_ref".into(), json!(t));
        approved.insert("owner_principal".into(), json!(p));
    }
    let payload = pretty(&public_procedure_record(&approved));
    mkdir_mode(approved_dir, 0o755)?;
    let receipts_dir = approved_dir.join(".receipts");
    mkdir_mode(&receipts_dir, 0o755)?;
    {
        let dir = fsx::open_dir(&std::path::absolute(approved_dir).unwrap_or_else(|_| approved_dir.to_path_buf()))?;
        match fsx::open_child(fd_ref(&dir), &dest_name, O_RDONLY | fsx::O_NONBLOCK, 0)? {
            Ok(_) => return Err(fail("REQUEST_CONFLICT", "An approved procedure file with this identity already exists.", true)),
            Err(e) if fsx::errno(&e) == libc::ENOENT => {}
            Err(e) => return Err(e.into()),
        }
        fsx::write_atomic_in_dir(fd_ref(&dir), &dest_name, &payload, 0o444)?;
    }
    let receipt = json!({
        "kind": "approval_receipt",
        "procedure_ref": approved["procedure_ref"],
        "version": approved["version"],
        "destination_name": dest_name,
        "sha256": fsx::sha256_hex(&payload),
        "installed_at": now_iso(),
    });
    {
        let dir = fsx::open_dir(&std::path::absolute(&receipts_dir).unwrap_or(receipts_dir.clone()))?;
        fsx::write_atomic_in_dir(fd_ref(&dir), &format!("{procedure_ref}.json"), &pretty(&receipt), 0o444)?;
    }
    Ok(json!({ "ok": true, "record": approved, "receipt": receipt }))
}

/// procedures.ts:472-497 `listApprovedProcedureFiles`: unreadable or invalid
/// files are skipped rather than failing search.
pub fn list_approved_procedure_files(approved_dir: &Path) -> Result<Vec<Record>> {
    if !approved_dir.exists() {
        return Ok(Vec::new());
    }
    let dir = fsx::open_dir(approved_dir)?;
    let mut out = Vec::new();
    for entry in fsx::read_dir_fd(fd_ref(&dir))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('.') {
            continue;
        }
        let Ok(Ok(fd)) = fsx::open_child(fd_ref(&dir), &name, O_RDONLY | fsx::O_NONBLOCK, 0) else { continue };
        let Ok(Value::Object(mut json)) = read_json_file_fd(fd_ref(&fd), MAX_PROCEDURE_BYTES) else { continue };
        if json.get("kind") != Some(&json!("procedure")) || json.get("status") != Some(&json!("approved")) {
            continue;
        }
        let r = json.get("procedure_ref").map_or_else(|| "undefined".into(), to_js_string);
        let v = match json.get("version") {
            v @ Some(x) if jsv::truthy(v) => to_js_string(x),
            _ => "1".into(),
        };
        json.insert("procedure_ref".into(), json!(r));
        json.insert("version".into(), json!(v));
        out.push(normalize_procedure_record(&Value::Object(json)));
    }
    Ok(out)
}

/// `String.prototype.localeCompare` approximated without ICU: case-insensitive
/// first, lower case before upper case on ties.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.chars().map(char::is_uppercase).cmp(b.chars().map(char::is_uppercase)))
}

/// procedures.ts:499-506 `searchRecords`.
pub fn search_records(records: Vec<Record>, query: &str, limit: usize) -> Vec<Record> {
    let tokens = tokenize(query);
    let mut ranked: Vec<(i64, Record)> = records
        .into_iter()
        .map(|r| (score_procedure(&r, &tokens), r))
        .filter(|(score, _)| *score > 0 || tokens.is_empty())
        .collect();
    let title = |r: &Record| def(r).get("title").map(to_js_string).unwrap_or_default();
    ranked.sort_by(|(sa, ra), (sb, rb)| sb.cmp(sa).then_with(|| locale_cmp(&title(ra), &title(rb))));
    ranked.into_iter().take(limit.clamp(1, 5)).map(|(_, r)| r).collect()
}

fn evidence_ok(env: Option<&ProcedureEnvironment>, r: &str) -> bool {
    env.and_then(|e| e.evidence.get(r)).is_some_and(EvidenceState::is_current_satisfied)
}

impl StorageService {
    /// storage.ts:2288-2293 `putProcedure`.
    fn put_procedure(&self, record: &Record, candidate_name: Option<&str>) -> Result<()> {
        let text = serde_json::to_string(record).unwrap_or_default();
        let now = now_iso();
        self.with_db(|db| {
            db.execute(
                "INSERT INTO procedures(procedure_ref, version, status, candidate_name, record, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(procedure_ref) DO UPDATE SET version=excluded.version, status=excluded.status, candidate_name=COALESCE(excluded.candidate_name, procedures.candidate_name), record=excluded.record, updated_at=excluded.updated_at",
                params![
                    record.get("procedure_ref").map(to_js_string),
                    record.get("version").map(to_js_string),
                    record.get("status").map(to_js_string),
                    candidate_name,
                    text,
                    now
                ],
            )?;
            Ok(())
        })
    }

    /// storage.ts:2295-2301 `getProcedure`: normalizes and writes back a record
    /// whose stored JSON differs.
    fn get_procedure(&self, procedure_ref: &str) -> Result<Option<Record>> {
        let text: Option<String> = self.with_db(|db| {
            Ok(db.query_row("SELECT record FROM procedures WHERE procedure_ref = ?1", [procedure_ref], |r| r.get(0)).optional()?)
        })?;
        let Some(text) = text else { return Ok(None) };
        let raw: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        let record = normalize_procedure_record(&raw);
        if serde_json::to_string(&record).unwrap_or_default() != text {
            self.put_procedure(&record, None)?;
        }
        Ok(Some(record))
    }

    /// storage.ts:2303-2305 `indexedProcedures`.
    fn indexed_procedures(&self) -> Result<Vec<Record>> {
        let refs: Vec<String> = self.with_db(|db| {
            let mut stmt = db.prepare("SELECT procedure_ref FROM procedures")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        let mut out = Vec::new();
        for r in refs {
            if let Some(record) = self.get_procedure(&r)? {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// storage.ts:2307-2324 `reindexApproved`: merge approved files into the
    /// table, keeping scoped private provenance only while the public file still
    /// matches its sealed guidance; mark rows whose file disappeared unverified.
    pub(super) fn reindex_approved(&self) -> Result<()> {
        let approved = list_approved_procedure_files(&self.inner.opts.approved_procedures_dir)?;
        for record in &approved {
            let rref = s(record, "procedure_ref").unwrap_or("").to_string();
            let current = self.get_procedure(&rref)?;
            if let Some(c) = &current {
                if s(c, "status") == Some("quarantined") && !truthy(c, "legacy_unowned") {
                    continue;
                }
                if s(c, "status") == Some("approved")
                    && truthy(c, "approval_digest")
                    && record.get("approval_source_digest") == c.get("approval_digest")
                    && record.get("version") == c.get("version")
                    && procedure_digest(record) == procedure_digest(&public_procedure_record(c))
                {
                    continue;
                }
            }
            let base = current.clone().unwrap_or_default();
            let mut merged = spread(&base, record);
            for k in ["contradictions", "quarantine_history"] {
                if let Some(v) = current.as_ref().and_then(|c| c.get(k)).filter(|v| jsv::truthy(Some(v))) {
                    merged.insert(k.into(), v.clone());
                }
            }
            merged.shift_remove("legacy_unowned");
            if current.as_ref().is_none_or(|c| serde_json::to_string(c).ok() != serde_json::to_string(&merged).ok()) {
                self.put_procedure(&merged, None)?;
            }
        }
        let present: HashSet<&str> = approved.iter().filter_map(|r| s(r, "procedure_ref")).collect();
        for mut current in self.indexed_procedures()? {
            if s(&current, "status") == Some("approved")
                && truthy(&current, "approval_digest")
                && !present.contains(s(&current, "procedure_ref").unwrap_or(""))
            {
                current.shift_remove("approval_digest");
                current.insert("legacy_unverified".into(), json!(true));
                self.put_procedure(&current, None)?;
            }
        }
        Ok(())
    }

    /// storage.ts:2326-2333 `procedureOwnerReadable`.
    fn procedure_owner_readable(&self, record: &Record, principal: &str) -> bool {
        if principal == "operator" {
            return true;
        }
        let (Some(task), Some(owner)) = (s(record, "owner_task_ref").filter(|t| !t.is_empty()), s(record, "owner_principal").filter(|p| !p.is_empty()))
        else {
            return false;
        };
        match self.journal() {
            Some(view) => view.read_task(principal, task).is_ok(),
            None => owner == principal,
        }
    }

    /// storage.ts:2335-2338 `procedureVisible`.
    fn procedure_visible(&self, record: &Record, principal: &str) -> bool {
        let approved = s(record, "status") == Some("approved");
        if !approved && (!truthy(record, "owner_task_ref") || !truthy(record, "owner_principal")) {
            return principal == "operator";
        }
        approved || truthy(record, "approval_digest") || self.procedure_owner_readable(record, principal)
    }

    /// storage.ts:2014-2022 `procedureSourceDigest`.
    fn procedure_source_digest(&self, procedure_ref: &str) -> Result<Option<String>> {
        let Some(name) = self.candidate_name(procedure_ref)? else { return Ok(None) };
        Ok(load_candidate_file(&self.inner.opts.candidate_procedures_dir, &name).ok().map(|(_, sha, _)| sha))
    }

    fn candidate_name(&self, procedure_ref: &str) -> Result<Option<String>> {
        self.with_db(|db| {
            Ok(db
                .query_row("SELECT candidate_name FROM procedures WHERE procedure_ref = ?1", [procedure_ref], |r| r.get::<_, Option<String>>(0))
                .optional()?
                .flatten()
                .filter(|n| !n.is_empty()))
        })
    }

    /// storage.ts:1127-1134 `listProcedures`.
    pub fn list_procedures(&self, principal: &str) -> Result<Vec<Value>> {
        self.ensure_open()?;
        self.reindex_approved()?;
        let mut out = Vec::new();
        for record in self.indexed_procedures()? {
            if !self.procedure_visible(&record, principal) {
                continue;
            }
            let private = self.procedure_owner_readable(&record, principal);
            let mut v = project_procedure(&record, &Projection { private_evidence: private, summary: true, ..Default::default() });
            if principal == "operator" {
                v["source_sha256"] = json!(self.procedure_source_digest(s(&record, "procedure_ref").unwrap_or(""))?);
            }
            out.push(v);
        }
        Ok(out)
    }

    /// storage.ts:1136-1147 `getProcedureRecord`.
    pub fn get_procedure_record(&self, procedure_ref: &str, principal: &str) -> Result<Option<Value>> {
        self.ensure_open()?;
        self.reindex_approved()?;
        fsx::assert_id(procedure_ref, "procedure_ref")?;
        let Some(record) = self.get_procedure(procedure_ref)? else { return Ok(None) };
        if !self.procedure_visible(&record, principal) {
            return Ok(None);
        }
        let private = self.procedure_owner_readable(&record, principal);
        let mut v = project_procedure(&record, &Projection { private_evidence: private, ..Default::default() });
        if principal == "operator" {
            v["candidate_name"] = json!(self.candidate_name(procedure_ref)?);
            v["source_sha256"] = json!(self.procedure_source_digest(procedure_ref)?);
        }
        Ok(Some(v))
    }

    /// storage.ts:1220-1228: project with private evidence resolved for readers
    /// who may see it.
    fn project_for(&self, record: &Record, ctx: &Context, summary: bool) -> Value {
        let private = self.procedure_owner_readable(record, &ctx.principal);
        let mut environment = ctx.procedure_environment.clone();
        if private
            && let (Some(env), Some(resolver)) = (environment.as_mut(), ctx.procedure_evidence_resolver.as_ref())
        {
            let d = def(record);
            let mut refs = str_list(&d, "evidence_refs");
            if let Some(v) = s(record, "last_validation_ref").filter(|v| !v.is_empty()) {
                refs.push(v.to_string());
            }
            env.evidence.extend(resolver(&refs));
        }
        project_procedure(
            record,
            &Projection { environment: environment.as_ref(), task_ref: Some(&ctx.task_ref), private_evidence: private, summary },
        )
    }

    /// storage.ts:1216-1294 `procedures`: `search`, `read`, `propose`,
    /// `contradict`, and the operator's `approve` and `quarantine`.
    pub fn procedures(&self, action: &Value, ctx: &Context) -> Result<ToolResult> {
        self.ensure_open()?;
        ctx.assert_authority()?;
        let kind = str_or(action, "kind", "");
        let env = ctx.procedure_environment.as_ref();
        match kind.as_str() {
            "search" => {
                self.reindex_approved()?;
                let limit = jsv::clamp_or(jsv::num_or(action, "limit", 5.0), 1.0, 5.0, 5.0) as usize;
                let visible: Vec<Record> =
                    self.indexed_procedures()?.into_iter().filter(|r| self.procedure_visible(r, &ctx.principal)).collect();
                let records = search_records(visible, &str_or(action, "query", ""), limit)
                    .iter()
                    .map(|r| self.project_for(r, ctx, true))
                    .collect();
                Ok(ToolResult { records })
            }
            "read" => {
                self.reindex_approved()?;
                match self.get_procedure(&str_or(action, "procedure_ref", ""))? {
                    Some(r) if self.procedure_visible(&r, &ctx.principal) => Ok(ToolResult::one(self.project_for(&r, ctx, false))),
                    _ => Err(fail("PERMISSION_DENIED", "Procedure is unavailable to this principal.", true)),
                }
            }
            "propose" => {
                let candidate = match action.get("candidate") {
                    v @ Some(c) if jsv::truthy(v) => c.clone(),
                    _ => json!({}),
                };
                let definition = validate_candidate(&candidate)?;
                let mut record = procedure_record(None, "0.1-candidate", "candidate", definition.clone(), None);
                record.insert("owner_task_ref".into(), json!(ctx.task_ref));
                record.insert("owner_principal".into(), json!(ctx.principal));
                if jsv::truthy(action.get("revises")) {
                    let previous = self.get_procedure(&str_or(action, "revises", ""))?;
                    let previous = previous
                        .filter(|p| self.procedure_visible(p, &ctx.principal))
                        .ok_or_else(|| fail("PERMISSION_DENIED", "Procedure revision is unavailable.", true))?;
                    if action.get("expected_sha256") != Some(&json!(procedure_digest(&previous))) {
                        return Err(fail("REQUEST_CONFLICT", "The procedure revision changed since review.", true));
                    }
                    record.insert("revision_of".into(), previous.get("procedure_ref").cloned().unwrap_or(Value::Null));
                    let pv = s(&previous, "version").unwrap_or("");
                    let base = pv.strip_suffix("-candidate").unwrap_or(pv);
                    record.insert("version".into(), json!(format!("{base}.1-candidate")));
                } else if jsv::truthy(action.get("expected_sha256")) {
                    return Err(invalid("expected_sha256 requires revises."));
                }
                for r in str_list(&definition, "contradicts") {
                    if !self.get_procedure(&r)?.is_some_and(|o| self.procedure_visible(&o, &ctx.principal)) {
                        return Err(fail("PERMISSION_DENIED", "A contradicted procedure is unavailable.", true));
                    }
                }
                let rref = s(&record, "procedure_ref").unwrap_or("").to_string();
                let candidate_name = format!("{rref}.json");
                {
                    let dir = fsx::open_dir(&self.inner.opts.candidate_procedures_dir)?;
                    fsx::write_atomic_in_dir(fd_ref(&dir), &candidate_name, &pretty(&record), 0o600)?;
                }
                self.put_procedure(&record, Some(&candidate_name))?;
                Ok(ToolResult::one(self.project_for(&record, ctx, false)))
            }
            "contradict" => {
                let mut record = self
                    .get_procedure(&str_or(action, "procedure_ref", ""))?
                    .filter(|r| self.procedure_visible(r, &ctx.principal))
                    .ok_or_else(|| fail("PERMISSION_DENIED", "Procedure is unavailable.", true))?;
                let digest = procedure_digest(&record);
                if action.get("expected_sha256") != Some(&json!(digest)) {
                    return Err(fail("REQUEST_CONFLICT", "The procedure revision changed since inspection.", true));
                }
                let reason = action.get("reason").and_then(Value::as_str).unwrap_or("");
                let refs = match action.get("evidence_refs") {
                    Some(Value::Array(items)) => Some(items),
                    _ => None,
                };
                let refs_ok = refs.is_some_and(|items| {
                    !items.is_empty()
                        && items.len() <= 20
                        && items.iter().all(|r| {
                            r.as_str().is_some_and(|r| jsv::id_re(r) && env.and_then(|e| e.evidence.get(r)).is_some_and(|e| e.state == "current"))
                        })
                });
                if reason.is_empty() || jsv::utf16_len(reason) > 1000 || !refs_ok {
                    return Err(invalid("A contradiction requires a reason and current authorized evidence."));
                }
                let mut evidence_refs: Vec<String> = refs.into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
                evidence_refs.sort_by(|a, b| jsv::utf16_cmp(a, b));
                evidence_refs.dedup();
                let evidence_json = json!(evidence_refs);
                let mut history = match record.get("contradictions") {
                    Some(Value::Array(items)) => items.clone(),
                    _ => Vec::new(),
                };
                let seen = history.iter().any(|item| {
                    item.get("task_ref") == Some(&json!(ctx.task_ref))
                        && item.get("content_sha256") == Some(&json!(digest))
                        && item.get("reason") == Some(&json!(reason))
                        && item.get("evidence_refs") == Some(&evidence_json)
                });
                if !seen {
                    history.push(json!({
                        "task_ref": ctx.task_ref,
                        "principal": ctx.principal,
                        "content_sha256": digest,
                        "reason": reason,
                        "evidence_refs": evidence_json,
                        "at": now_iso(),
                    }));
                    record.insert("contradictions".into(), Value::Array(history));
                    self.put_procedure(&record, None)?;
                }
                Ok(ToolResult::one(self.project_for(&record, ctx, false)))
            }
            "approve" | "quarantine" => {
                if ctx.principal != "operator" {
                    return Err(fail("PERMISSION_DENIED", "Only the operator can approve or globally quarantine guidance.", true));
                }
                let procedure_ref = str_or(action, "procedure_ref", "");
                if kind == "quarantine" {
                    return Ok(ToolResult::one(self.quarantine_procedure(&procedure_ref, &str_or(action, "reason", "operator_quarantine"))?));
                }
                let current = self.get_procedure(&procedure_ref)?;
                let refs = current.as_ref().map(|c| str_list(&def(c), "evidence_refs")).unwrap_or_default();
                let supported = !refs.is_empty() && refs.iter().all(|r| evidence_ok(env, r));
                let requested = action.get("validation_ref").filter(|v| jsv::truthy(Some(v)));
                if let Some(v) = requested
                    && (!supported || !refs.iter().any(|r| Some(r.as_str()) == v.as_str()))
                {
                    return Err(invalid("Validation must reference current authorized supporting evidence."));
                }
                // Reuse an existing passed supporting check; approval never
                // manufactures a validation record or makes absent proof current.
                let validation = supported.then(|| requested.map_or_else(|| refs[0].clone(), to_js_string));
                Ok(ToolResult::one(self.prepare_promotion(&procedure_ref, &str_or(action, "expected_sha256", ""), validation.as_deref())?))
            }
            _ => Err(invalid(format!("Unsupported procedure action: {}.", if kind.is_empty() { "missing" } else { &kind }))),
        }
    }

    /// storage.ts:1371-1420 `preparePromotion`: write a promotion request for
    /// the exact reviewed candidate bytes; install it directly when the
    /// approved directory is writable, otherwise leave it pending.
    pub fn prepare_promotion(&self, procedure_ref: &str, expected_sha256: &str, validation_ref: Option<&str>) -> Result<Value> {
        self.ensure_open()?;
        fsx::assert_id(procedure_ref, "procedure_ref")?;
        let current = self
            .get_procedure(procedure_ref)?
            .filter(|c| s(c, "status") == Some("candidate"))
            .ok_or_else(|| invalid("Only an existing candidate can be prepared for promotion."))?;
        let candidate_name = self.candidate_name(procedure_ref)?.unwrap_or_else(|| format!("{procedure_ref}.json"));
        let (loaded, sha, size) = load_candidate_file(&self.inner.opts.candidate_procedures_dir, &candidate_name)?;
        if !jsv::sha256_re(expected_sha256) || sha != expected_sha256 {
            return Err(fail(
                "REQUEST_CONFLICT",
                "Candidate contents changed since review; inspect its current digest before approving.",
                true,
            ));
        }
        if loaded.get("procedure_ref") != Some(&json!(procedure_ref))
            || loaded.get("version") != current.get("version")
            || loaded.get("owner_task_ref") != current.get("owner_task_ref")
            || loaded.get("owner_principal") != current.get("owner_principal")
        {
            return Err(fail("REQUEST_CONFLICT", "Candidate identity changed since creation.", true));
        }
        let definition = validate_candidate(match loaded.get("definition") {
            None | Some(Value::Null) => &loaded,
            Some(d) => d,
        })?;
        let mut with_definition = current.clone();
        with_definition.insert("definition".into(), Value::Object(definition.clone()));
        if procedure_digest(&with_definition) != procedure_digest(&current) {
            return Err(fail("REQUEST_CONFLICT", "Candidate was edited in place; propose a new revision before approval.", true));
        }
        let version = match loaded.get("version") {
            v @ Some(x) if jsv::truthy(v) => to_js_string(x),
            _ => current.get("version").filter(|v| jsv::truthy(Some(v))).map_or_else(|| "1.0".into(), to_js_string),
        };
        let dest_name = safe_procedure_file_name(procedure_ref, &version)?;
        let mut request = json!({
            "kind": "promotion_request",
            "procedure_ref": procedure_ref,
            "version": version,
            "source_sha256": sha,
            "source_bytes": size,
            "candidate_name": candidate_name,
            "destination_name": dest_name,
            "title": definition.get("title").cloned().unwrap_or(Value::Null),
            "created_at": now_iso(),
        });
        if let Some(v) = validation_ref {
            request["validation_ref"] = json!(v);
        }
        let pending_dir = self.inner.opts.state_dir.join("pending-promotions");
        {
            let dir = fsx::open_dir(&pending_dir)?;
            fsx::write_atomic_in_dir(fd_ref(&dir), &format!("{procedure_ref}.json"), &pretty(&request), 0o600)?;
        }
        let approved_dir = &self.inner.opts.approved_procedures_dir;
        if fsx::writable(approved_dir) {
            let installed = install_approved_procedure(
                &pending_dir.join(format!("{procedure_ref}.json")),
                approved_dir,
                &self.inner.opts.candidate_procedures_dir,
            )?;
            let installed_record = match installed.get("record") {
                Some(Value::Object(r)) => r.clone(),
                _ => Map::new(),
            };
            let record = spread(&current, &installed_record);
            self.put_procedure(&record, Some(&candidate_name))?;
            return Ok(project_procedure(&record, &Projection { private_evidence: true, ..Default::default() }));
        }
        let pending = spread(&current, &procedure_record(Some(procedure_ref), &version, "candidate", definition, validation_ref));
        self.put_procedure(&pending, Some(&candidate_name))?;
        let mut out = project_procedure(&pending, &Projection { private_evidence: true, ..Default::default() });
        out["promotion_request"] = request;
        Ok(out)
    }

    /// storage.ts:1422-1446 `quarantineProcedure`.
    pub fn quarantine_procedure(&self, procedure_ref: &str, reason: &str) -> Result<Value> {
        self.ensure_open()?;
        fsx::assert_id(procedure_ref, "procedure_ref")?;
        let current = self.get_procedure(procedure_ref)?.ok_or_else(|| invalid("Unknown procedure_ref."))?;
        let fresh = procedure_record(
            s(&current, "procedure_ref"),
            s(&current, "version").unwrap_or(""),
            "quarantined",
            def(&current),
            s(&current, "last_validation_ref"),
        );
        let mut record = spread(&current, &fresh);
        let digest = procedure_digest(&record);
        let mut history = match record.get("quarantine_history") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        if !history.iter().any(|h| h.get("content_sha256") == Some(&json!(digest)) && h.get("reason") == Some(&json!(reason))) {
            history.push(json!({ "reason": jsv::clip(reason, 1000), "content_sha256": digest, "at": now_iso(), "principal": "operator" }));
        }
        record.insert("quarantine_history".into(), Value::Array(history.clone()));
        record.shift_remove("legacy_unowned");
        self.put_procedure(&record, None)?;
        {
            let dir = fsx::open_dir(&self.inner.opts.state_dir.join("quarantine"))?;
            let body = json!({ "procedure_ref": procedure_ref, "history": history });
            fsx::write_atomic_in_dir(fd_ref(&dir), &format!("{procedure_ref}.json"), &pretty(&body), 0o600)?;
        }
        Ok(project_procedure(&record, &Projection { private_evidence: true, ..Default::default() }))
    }
}
