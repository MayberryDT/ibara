//! The operator directory: the private `operator.sqlite` (schema 2, unchanged).
//!
//! Port of `controller/agent/operator-directory.mjs`. The database, its parent
//! directory and its SQLite sidecars must be private and owned by this user;
//! ancestors must not be writable by anyone else (root sticky directories
//! excepted). Nothing here ever trusts a descriptor as endpoint evidence.

use super::{current_uid, fail, js, pattern, resolve_path, unsafe_file};
use crate::error::{IbaraError, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `SCHEMA_VERSION` (operator-directory.mjs:7).
pub const SCHEMA_VERSION: u32 = 2;
/// `LABEL_LIMIT` (operator-directory.mjs:10), in UTF-16 code units like JavaScript.
pub const LABEL_LIMIT: usize = 128;

fn operator_label_key(computer_id: &str) -> String {
    format!("operator_label:{computer_id}")
}

/// `LISTED_COLUMNS` (operator-directory.mjs:16): never the key or known-hosts references.
const LISTED_COLUMNS: &str = "computer_id,endpoint_id,label,host,user,trust_state,binding_revision,authorization_generation,updated_at,\
     (SELECT value FROM directory_meta WHERE key = 'wake:' || computer_id)";

fn wake_key(computer_id: &str) -> String {
    format!("wake:{computer_id}")
}
const ALL_COLUMNS: &str = "computer_id,endpoint_id,label,transport,host,user,port,identity_file_ref,known_hosts_file_ref,trust_state,binding_revision,authorization_generation,created_at,updated_at";

/// The operator's state directory: `$XDG_STATE_HOME` when absolute, else
/// `~/.local/state`, then `ibara`.
pub fn operator_state_dir() -> PathBuf {
    let root = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() && Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => super::home_dir().join(".local").join("state"),
    };
    root.join("ibara")
}

/// `defaultOperatorDirectoryPath()` (operator-directory.mjs:25): `operator.sqlite` in
/// [`operator_state_dir`].
pub fn default_operator_directory_path() -> PathBuf {
    operator_state_dir().join("operator.sqlite")
}

/// `explicit || $IBARA_OPERATOR_DIRECTORY_DB || default` (ibara-client.mjs:192 and friends).
pub fn directory_path(explicit: Option<&str>) -> PathBuf {
    match explicit.filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => super::env_or("IBARA_OPERATOR_DIRECTORY_DB")
            .map(PathBuf::from)
            .unwrap_or_else(default_operator_directory_path),
    }
}

fn lstat_or_missing(target: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(target) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// operator-directory.mjs:34
fn assert_safe_directory_component(directory: &Path, stat: Option<&Metadata>) -> Result<()> {
    let shown = directory.display();
    let Some(stat) = stat.filter(|s| !s.file_type().is_symlink() && s.is_dir()) else {
        return Err(unsafe_file(format!("Unsafe directory path: {shown}")));
    };
    if stat.uid() != 0 && stat.uid() != current_uid() {
        return Err(unsafe_file(format!("Untrusted directory owner in operator path: {shown}")));
    }
    let writable_by_others = stat.mode() & 0o022 != 0;
    let root_sticky = stat.uid() == 0 && stat.mode() & 0o1000 != 0;
    if writable_by_others && !root_sticky {
        return Err(unsafe_file(format!("Unsafe writable directory ancestor in operator path: {shown}")));
    }
    Ok(())
}

/// operator-directory.mjs:42
fn ensure_directory_tree(directory: &Path) -> Result<PathBuf> {
    let absolute = resolve_path(directory);
    let mut current = PathBuf::from("/");
    assert_safe_directory_component(&current, lstat_or_missing(&current)?.as_ref())?;
    for part in absolute.iter().skip(1) {
        // The current component controls whether the next entry can be replaced.
        assert_safe_directory_component(&current, lstat_or_missing(&current)?.as_ref())?;
        current.push(part);
        let mut stat = lstat_or_missing(&current)?;
        if stat.is_none() {
            match fs::DirBuilder::new().mode(0o700).create(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            stat = lstat_or_missing(&current)?;
        }
        assert_safe_directory_component(&current, stat.as_ref())?;
    }
    let parent = fs::symlink_metadata(&absolute)?;
    if parent.uid() != current_uid() || parent.mode() & 0o077 != 0 {
        return Err(unsafe_file("Operator directory parent must be owned by this user and private (mode 0700)."));
    }
    Ok(absolute)
}

fn private_regular(stat: &Metadata) -> bool {
    !stat.file_type().is_symlink()
        && stat.is_file()
        && stat.uid() == current_uid()
        && stat.nlink() == 1
        && stat.mode() & 0o077 == 0
}

/// operator-directory.mjs:67. Returns whether the database already existed.
fn validate_database_file(database_path: &Path, create: bool) -> Result<bool> {
    let Some(stat) = lstat_or_missing(database_path)? else {
        if !create {
            return Ok(false);
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(database_path)?;
        return Ok(false);
    };
    if !private_regular(&stat) {
        return Err(unsafe_file("Operator directory database must be a private, user-owned regular file."));
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = database_path.as_os_str().to_owned();
        sidecar.push(suffix);
        if let Some(stat) = lstat_or_missing(Path::new(&sidecar))?
            && !private_regular(&stat)
        {
            return Err(unsafe_file(format!("Unsafe SQLite sidecar: {suffix}")));
        }
    }
    Ok(true)
}

/// The index definition normalisation of operator-directory.mjs:112-121.
fn normalize_index_sql(sql: &str) -> String {
    let lowered: String = sql.to_lowercase().chars().filter(|c| !matches!(c, '"' | '`' | '[' | ']')).collect();
    // /\s+/g -> ' '
    let mut collapsed = String::with_capacity(lowered.len());
    let mut in_space = false;
    for c in lowered.chars() {
        if js::is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    // /\s*\(\s*/g -> '(' then /\s*\)/g -> ')' then /\s*=\s*/g -> ' = '
    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' | ')' | '=' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                if c == '=' {
                    out.push_str(" = ");
                } else {
                    out.push(c);
                }
                if c != ')' {
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
            }
            _ => out.push(c),
        }
    }
    let trimmed = js::trim(&out);
    trimmed.strip_suffix(';').unwrap_or(trimmed).to_string()
}

/// operator-directory.mjs:87
fn verify_existing_schema(database_path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let table: Option<i64> = db
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'directory_meta'", [], |r| r.get(0))
        .optional()?;
    if table.is_none() {
        return Err(fail("Existing operator directory has no supported schema; refusing to initialize over it."));
    }
    let version: Option<String> = db
        .query_row("SELECT value FROM directory_meta WHERE key = 'schema_version'", [], |r| r.get(0))
        .optional()?;
    let Some(version) = version.filter(|v| pattern::digits(v)) else {
        return Err(fail("Operator directory schema version is missing or invalid."));
    };
    let number: f64 = version.parse().unwrap_or(f64::INFINITY);
    if number > f64::from(SCHEMA_VERSION) {
        return Err(fail(format!("Operator directory schema {} is newer than this client supports.", js::string(&json!(number)))));
    }
    if number != f64::from(SCHEMA_VERSION) {
        return Err(fail(format!("Operator directory schema {} requires an explicit migration.", js::string(&json!(number)))));
    }
    let required: [(&str, &[&str]); 2] = [
        ("directory_meta", &["key", "value"]),
        (
            "computers",
            &[
                "computer_id", "endpoint_id", "label", "transport", "host", "user", "port", "identity_file_ref",
                "known_hosts_file_ref", "trust_state", "binding_revision", "authorization_generation", "created_at",
                "updated_at",
            ],
        ),
    ];
    for (name, columns) in required {
        let mut stmt = db.prepare(&format!("PRAGMA table_info('{name}')"))?;
        let actual: Vec<String> = stmt.query_map([], |r| r.get::<_, String>("name"))?.collect::<Result<_, _>>()?;
        if columns.iter().any(|c| !actual.iter().any(|a| a == c)) {
            return Err(fail(format!("Operator directory table {name} is incomplete.")));
        }
    }
    const INDEX: &str = "computers_verified_endpoint_id_unique";
    let mut stmt = db.prepare("PRAGMA index_list('computers')")?;
    let index: Option<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>("name")?, r.get::<_, i64>("unique")?, r.get::<_, i64>("partial")?)))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|(name, _, _)| name == INDEX)
        .map(|(_, unique, partial)| (unique, partial));
    let indexed: Vec<Option<String>> = if index.is_some() {
        let mut stmt = db.prepare(&format!("PRAGMA index_info('{INDEX}')"))?;
        stmt.query_map([], |r| r.get::<_, Option<String>>("name"))?.collect::<Result<_, _>>()?
    } else {
        Vec::new()
    };
    let definition: Option<Option<String>> = db
        .query_row("SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?", [INDEX], |r| r.get(0))
        .optional()?;
    let normalized = definition.flatten().map(|sql| normalize_index_sql(&sql)).unwrap_or_default();
    const EXPECTED: &str =
        "create unique index computers_verified_endpoint_id_unique on computers(endpoint_id) where trust_state = 'verified'";
    if index != Some((1, 1)) || indexed.len() != 1 || indexed[0].as_deref() != Some("endpoint_id") || normalized != EXPECTED {
        return Err(fail("Operator directory verified endpoint index is missing or malformed."));
    }
    let operator: Option<String> = db
        .query_row("SELECT value FROM directory_meta WHERE key = 'operator_instance_id'", [], |r| r.get(0))
        .optional()?;
    if !operator.is_some_and(|id| pattern::id(&id)) {
        return Err(fail("Operator directory instance identity is invalid."));
    }
    Ok(())
}

/// operator-directory.mjs:131
fn create_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "
    CREATE TABLE directory_meta (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE computers (
      computer_id TEXT PRIMARY KEY,
      endpoint_id TEXT NOT NULL,
      label TEXT NOT NULL,
      transport TEXT NOT NULL CHECK (transport = 'ssh'),
      host TEXT NOT NULL,
      user TEXT NOT NULL,
      port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
      identity_file_ref TEXT NOT NULL,
      known_hosts_file_ref TEXT NOT NULL,
      trust_state TEXT NOT NULL CHECK (trust_state IN ('verified', 'unverified', 'removed')),
      binding_revision INTEGER NOT NULL CHECK (binding_revision >= 1),
      authorization_generation INTEGER NOT NULL DEFAULT 0 CHECK (authorization_generation >= 0),
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
    CREATE UNIQUE INDEX computers_verified_endpoint_id_unique ON computers(endpoint_id) WHERE trust_state = 'verified';
  ",
    )?;
    let now = crate::ids::now_iso();
    let insert = "INSERT INTO directory_meta(key, value) VALUES (?, ?)";
    db.execute(insert, params!["schema_version", SCHEMA_VERSION.to_string()])?;
    db.execute(insert, params!["operator_instance_id", format!("operator_{}", uuid::Uuid::new_v4().hyphenated())])?;
    db.execute(insert, params!["created_at", now])?;
    Ok(())
}

/// A validated computer record (`validateRecord`, operator-directory.mjs:161).
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerInput {
    pub computer_id: String,
    pub endpoint_id: String,
    pub label: String,
    pub transport: String,
    pub host: String,
    pub user: String,
    pub port: i64,
    pub identity_file_ref: String,
    pub known_hosts_file_ref: String,
    pub trust_state: String,
    pub binding_revision: i64,
    pub authorization_generation: i64,
}

/// `validateRecord(record)` (operator-directory.mjs:161), with JavaScript coercions.
pub fn validate_record(record: &Value) -> Result<ComputerInput> {
    if !record.is_object() {
        return Err(fail("Computer record is required."));
    }
    let field = |name: &str| record.get(name);
    let computer_id = js::string_or(field("computer_id"), "");
    let endpoint_id = js::string_or(field("endpoint_id"), "");
    let label = js::trim(&js::string_or(field("label"), "")).to_string();
    let host = js::string_or(field("host"), "");
    let user = js::string_or(field("user"), "");
    let transport = js::string_or(field("transport"), "ssh");
    let port = js::number(field("port"));
    let identity_ref = js::string_or(field("identity_file_ref"), "");
    let known_hosts_ref = js::string_or(field("known_hosts_file_ref"), "");
    let binding_revision = js::number(field("binding_revision"));
    let authorization_generation = match field("authorization_generation") {
        None | Some(Value::Null) => 0.0,
        value => js::number(value),
    };
    if !pattern::id(&computer_id) {
        return Err(fail("Invalid computer ID."));
    }
    if !pattern::endpoint_id(&endpoint_id) {
        return Err(fail("Invalid endpoint identity."));
    }
    if label.is_empty() || js::length(&label) > LABEL_LIMIT {
        return Err(fail("Computer label must contain 1–128 characters."));
    }
    if transport != "ssh" {
        return Err(fail("Unsupported selected-target transport."));
    }
    if !pattern::host(&host) {
        return Err(fail("Invalid selected-target host."));
    }
    if !pattern::account(&user) {
        return Err(fail("Invalid selected-target user."));
    }
    if !js::is_safe_integer(port) || !(1.0..=65535.0).contains(&port) {
        return Err(fail("Invalid selected-target port."));
    }
    if !pattern::file_ref(&identity_ref) {
        return Err(fail("Selected target requires an explicit identity-key reference."));
    }
    if !pattern::file_ref(&known_hosts_ref) {
        return Err(fail("Selected target requires an explicit known-hosts reference."));
    }
    if !js::is_safe_integer(binding_revision) || binding_revision < 1.0 {
        return Err(fail("Invalid endpoint binding revision."));
    }
    if !js::is_safe_integer(authorization_generation) || authorization_generation < 0.0 {
        return Err(fail("Invalid authorization generation."));
    }
    Ok(ComputerInput {
        computer_id,
        endpoint_id,
        label,
        transport,
        host,
        user,
        port: port as i64,
        identity_file_ref: identity_ref,
        known_hosts_file_ref: known_hosts_ref,
        trust_state: js::string_or(field("trust_state"), "verified"),
        binding_revision: binding_revision as i64,
        authorization_generation: authorization_generation as i64,
    })
}

/// One `computers` row (`SELECT *`, column order preserved in [`ComputerRecord::to_json`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerRecord {
    pub computer_id: String,
    pub endpoint_id: String,
    pub label: String,
    pub transport: String,
    pub host: String,
    pub user: String,
    pub port: i64,
    pub identity_file_ref: String,
    pub known_hosts_file_ref: String,
    pub trust_state: String,
    pub binding_revision: i64,
    pub authorization_generation: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl ComputerRecord {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(ComputerRecord {
            computer_id: r.get(0)?,
            endpoint_id: r.get(1)?,
            label: r.get(2)?,
            transport: r.get(3)?,
            host: r.get(4)?,
            user: r.get(5)?,
            port: r.get(6)?,
            identity_file_ref: r.get(7)?,
            known_hosts_file_ref: r.get(8)?,
            trust_state: r.get(9)?,
            binding_revision: r.get(10)?,
            authorization_generation: r.get(11)?,
            created_at: r.get(12)?,
            updated_at: r.get(13)?,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "computer_id": self.computer_id, "endpoint_id": self.endpoint_id, "label": self.label,
            "transport": self.transport, "host": self.host, "user": self.user, "port": self.port,
            "identity_file_ref": self.identity_file_ref, "known_hosts_file_ref": self.known_hosts_file_ref,
            "trust_state": self.trust_state, "binding_revision": self.binding_revision,
            "authorization_generation": self.authorization_generation,
            "created_at": self.created_at, "updated_at": self.updated_at,
        })
    }
}

/// A listing row (`LISTED_COLUMNS`): `user` is the route principal, the operator's
/// identity on that computer.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedComputer {
    pub computer_id: String,
    pub endpoint_id: String,
    pub label: String,
    pub host: String,
    pub user: String,
    pub trust_state: String,
    pub binding_revision: i64,
    pub authorization_generation: i64,
    pub updated_at: String,
    /// How to wake the computer, as it last reported (`{mac, ifname, kind, subnet, from_off}`), or null.
    pub wake: Value,
}

impl ListedComputer {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let wake: Option<String> = r.get(9)?;
        Ok(ListedComputer {
            computer_id: r.get(0)?,
            endpoint_id: r.get(1)?,
            label: r.get(2)?,
            host: r.get(3)?,
            user: r.get(4)?,
            trust_state: r.get(5)?,
            binding_revision: r.get(6)?,
            authorization_generation: r.get(7)?,
            updated_at: r.get(8)?,
            wake: wake.and_then(|w| serde_json::from_str(&w).ok()).unwrap_or(Value::Null),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "computer_id": self.computer_id, "endpoint_id": self.endpoint_id, "label": self.label,
            "host": self.host, "user": self.user, "trust_state": self.trust_state,
            "binding_revision": self.binding_revision, "authorization_generation": self.authorization_generation,
            "updated_at": self.updated_at, "wake": self.wake,
        })
    }
}

/// The pinned route inside a selected envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedRoute {
    pub host: String,
    /// The route principal: the operator's logical identity, not an SSH login.
    pub user: String,
    pub port: u16,
    pub identity_file_ref: String,
    pub known_hosts_file_ref: String,
}

/// The immutable per-operation route envelope (`bindOperation`, operator-directory.mjs:355).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedEnvelope {
    pub environment_id: String,
    pub computer_id: String,
    pub endpoint_id: String,
    pub binding_revision: i64,
    pub request_id: String,
    pub expected_authorization_generation: i64,
    pub record_id: String,
    pub route: SelectedRoute,
}

impl SelectedEnvelope {
    /// The envelope JSON in the key order `bindOperation` produced.
    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": 1,
            "environment_id": self.environment_id,
            "computer_id": self.computer_id,
            "endpoint_id": self.endpoint_id,
            "binding_revision": self.binding_revision,
            "request_id": self.request_id,
            "expected_authorization_generation": self.expected_authorization_generation,
            "record_id": self.record_id,
            "route": {
                "protocol": "ssh",
                "host": self.route.host,
                "user": self.route.user,
                "port": self.route.port,
                "identity_file_ref": self.route.identity_file_ref,
                "known_hosts_file_ref": self.route.known_hosts_file_ref,
            },
        })
    }

    /// `JSON.stringify([environment_id, computer_id, endpoint_id, binding_revision,
    /// expected_authorization_generation, route])` (ibara-operator.mjs:56).
    pub fn session_key(&self) -> String {
        let value = self.to_json();
        json!([
            self.environment_id,
            self.computer_id,
            self.endpoint_id,
            self.binding_revision,
            self.expected_authorization_generation,
            value["route"],
        ])
        .to_string()
    }
}

/// `validateSelectedEnvelope(envelope)` (operator-directory.mjs:378).
pub fn validate_selected_envelope(envelope: &Value) -> Result<SelectedEnvelope> {
    if !envelope.is_object() {
        return Err(fail("Selected route envelope is required."));
    }
    const KEYS: [&str; 9] = [
        "schema_version", "environment_id", "computer_id", "endpoint_id", "binding_revision", "request_id",
        "expected_authorization_generation", "record_id", "route",
    ];
    if !js::has_exact_keys(envelope, &KEYS) {
        return Err(fail("Selected route envelope has unknown or missing fields."));
    }
    let text = |name: &str| js::string_or(envelope.get(name), "");
    if !js::same_number(envelope.get("schema_version"), 1.0)
        || !pattern::id(&text("environment_id"))
        || !pattern::id(&text("computer_id"))
        || !pattern::endpoint_id(&text("endpoint_id"))
        || !pattern::id(&text("request_id"))
        || !pattern::id(&text("record_id"))
    {
        return Err(fail("Selected route envelope identity is invalid."));
    }
    let binding_revision = js::safe_integer(envelope.get("binding_revision")).filter(|r| *r >= 1);
    let generation = js::safe_integer(envelope.get("expected_authorization_generation")).filter(|g| *g >= 0);
    let (Some(binding_revision), Some(generation)) = (binding_revision, generation) else {
        return Err(fail("Selected route envelope generation is invalid."));
    };
    const ROUTE_KEYS: [&str; 6] = ["protocol", "host", "user", "port", "identity_file_ref", "known_hosts_file_ref"];
    let route = &envelope["route"];
    if !js::has_exact_keys(route, &ROUTE_KEYS) {
        return Err(fail("Selected route is incomplete."));
    }
    if route.get("protocol") != Some(&json!("ssh")) {
        return Err(fail("Selected route transport is unsupported."));
    }
    let mut record = route.as_object().cloned().unwrap_or_default();
    record.insert("computer_id".into(), envelope["computer_id"].clone());
    record.insert("endpoint_id".into(), envelope["endpoint_id"].clone());
    record.insert("label".into(), json!("Selected target"));
    record.insert("trust_state".into(), json!("verified"));
    record.insert("binding_revision".into(), json!(binding_revision));
    record.insert("authorization_generation".into(), json!(generation));
    let checked = validate_record(&Value::Object(record))?;
    Ok(SelectedEnvelope {
        environment_id: text("environment_id"),
        computer_id: text("computer_id"),
        endpoint_id: text("endpoint_id"),
        binding_revision,
        request_id: text("request_id"),
        expected_authorization_generation: generation,
        record_id: text("record_id"),
        route: SelectedRoute {
            host: checked.host,
            user: checked.user,
            port: checked.port as u16,
            identity_file_ref: checked.identity_file_ref,
            known_hosts_file_ref: checked.known_hosts_file_ref,
        },
    })
}

/// `SQLITE_CONSTRAINT_UNIQUE` on `computers.endpoint_id` (operator-directory.mjs:320).
fn endpoint_conflict(error: rusqlite::Error) -> IbaraError {
    match &error {
        rusqlite::Error::SqliteFailure(e, Some(message))
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE && message.contains("computers.endpoint_id") =>
        {
            fail("Verified endpoint identity is already registered.")
        }
        _ => error.into(),
    }
}

/// The open operator directory (`class OperatorDirectory`, operator-directory.mjs:209).
pub struct OperatorDirectory {
    database_path: PathBuf,
    db: Connection,
    operator_instance_id: String,
}

impl OperatorDirectory {
    /// `new OperatorDirectory(databasePath)` (operator-directory.mjs:210): validates the
    /// parent tree and file, creates schema 2 when absent, refuses anything else.
    pub fn open(database_path: &Path) -> Result<Self> {
        let resolved = resolve_path(database_path);
        let name = resolved.file_name().map(|n| n.to_owned()).unwrap_or_default();
        let parent = ensure_directory_tree(resolved.parent().unwrap_or(Path::new("/")))?;
        let database_path = parent.join(name);
        let existing = validate_database_file(&database_path, true)?;
        if existing {
            verify_existing_schema(&database_path)?;
        }
        let mut db = Connection::open(&database_path)?;
        db.busy_timeout(std::time::Duration::from_millis(5000))?;
        if !existing {
            let tx = db.transaction()?;
            create_schema(&tx)?;
            tx.commit()?;
        }
        db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.pragma_update_and_check(None, "busy_timeout", 5000, |_| Ok(()))?;
        let mode = fs::metadata(&database_path)?.mode() & 0o777;
        if mode & 0o077 != 0 {
            fs::set_permissions(&database_path, fs::Permissions::from_mode(0o600))?;
        }
        let operator_instance_id: Option<String> = db
            .query_row("SELECT value FROM directory_meta WHERE key = 'operator_instance_id'", [], |r| r.get(0))
            .optional()?;
        let operator_instance_id = operator_instance_id.unwrap_or_default();
        if !pattern::id(&operator_instance_id) {
            return Err(fail("Operator directory instance identity is invalid."));
        }
        Ok(OperatorDirectory { database_path, db, operator_instance_id })
    }

    /// The resolved database path.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// `operatorInstanceId`: this operator environment's identity (`operator_<uuid>`).
    pub fn operator_instance_id(&self) -> &str {
        &self.operator_instance_id
    }

    /// `close()` (operator-directory.mjs:236).
    pub fn close(self) {
        drop(self.db);
    }

    /// `importLegacyDescriptor(descriptor)` (operator-directory.mjs:238). The descriptor is
    /// configuration, not endpoint evidence: the row is `unverified` and carries no route.
    pub fn import_legacy_descriptor(&mut self, descriptor: &Value) -> Result<ComputerRecord> {
        const ALLOWED: [&str; 7] =
            ["schema_version", "station_id", "node", "display_label", "agent_account", "operator_account", "services"];
        let invalid = || fail("Invalid legacy station descriptor.");
        let Some(map) = descriptor.as_object() else { return Err(invalid()) };
        let text = |name: &str| js::string_or(descriptor.get(name), "");
        let label_ok = matches!(descriptor.get("display_label"), Some(Value::String(label))
            if !js::trim(label).is_empty() && js::length(label) <= LABEL_LIMIT);
        let services_ok = match descriptor.get("services") {
            Some(Value::Object(services)) => services.iter().all(|(name, service)| {
                pattern::id(name)
                    && service.is_object()
                    && js::has_exact_keys(service, &["scope", "unit"])
                    && matches!(service.get("scope"), Some(Value::String(s)) if s == "user" || s == "system")
                    && pattern::service_unit(&js::string_or(service.get("unit"), ""))
            }),
            _ => false,
        };
        if map.keys().any(|k| !ALLOWED.contains(&k.as_str()))
            || !js::same_number(descriptor.get("schema_version"), 1.0)
            || !pattern::id(&text("station_id"))
            || !pattern::node(&text("node"))
            || !label_ok
            || !pattern::account(&text("agent_account"))
            || !pattern::account(&text("operator_account"))
            || !services_ok
        {
            return Err(invalid());
        }
        // Preserve the exact preimage for repair.
        let raw = js::stringify(descriptor);
        if raw.len() > 16384 {
            return Err(fail("Legacy descriptor is too large."));
        }
        let digest = hex(&Sha256::digest(raw.as_bytes()));
        let station_id = text("station_id");
        let key = format!("legacy_import:{station_id}");
        let now = crate::ids::now_iso();
        let label = js::slice_units(js::trim(&text("display_label")), LABEL_LIMIT);
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<String> =
            tx.query_row("SELECT value FROM directory_meta WHERE key=?", [&key], |r| r.get(0)).optional()?;
        let existing = get_computer(&tx, &station_id)?;
        if let Some(prior) = prior {
            let saved: Value = serde_json::from_str(&prior).unwrap_or(Value::Null);
            return match existing {
                Some(existing) if saved.get("sha256") == Some(&json!(digest)) => {
                    tx.commit()?;
                    Ok(existing)
                }
                _ => Err(fail("Legacy descriptor conflicts with the saved import.")),
            };
        }
        if existing.is_some() {
            return Err(fail("Legacy descriptor conflicts with an existing computer."));
        }
        tx.execute(
            "INSERT INTO computers(computer_id,endpoint_id,label,transport,host,user,port,identity_file_ref,known_hosts_file_ref,trust_state,binding_revision,authorization_generation,created_at,updated_at)
        VALUES (?,?,?,?,?,?,?,?,?,'unverified',1,0,?,?)",
            params![
                station_id,
                format!("unverified:{station_id}"),
                label,
                "ssh",
                text("node"),
                text("agent_account"),
                22,
                "",
                "",
                now,
                now
            ],
        )?;
        let saved = json!({"sha256": digest, "descriptor": descriptor});
        tx.execute("INSERT INTO directory_meta(key,value) VALUES (?,?)", params![key, js::stringify(&saved)])?;
        let record = get_computer(&tx, &station_id)?.ok_or_else(|| fail("Legacy import did not persist."))?;
        tx.commit()?;
        Ok(record)
    }

    /// `registerVerifiedComputer(input, authenticatedEndpointId, expectedBindingRevision = 0)`
    /// (operator-directory.mjs:284). The binding revision advances exactly once when the
    /// verified route changes; an operator-chosen label survives the rebind.
    pub fn register_verified_computer(
        &mut self,
        input: &Value,
        authenticated_endpoint_id: &str,
        expected_binding_revision: i64,
    ) -> Result<ComputerRecord> {
        let record = validate_record(input)?;
        if record.trust_state != "verified" || authenticated_endpoint_id != record.endpoint_id {
            return Err(fail("A verified computer requires a matching authenticated endpoint identity."));
        }
        if !js::is_safe_integer(expected_binding_revision as f64) || expected_binding_revision < 0 {
            return Err(fail("Expected binding revision must be a non-negative integer."));
        }
        let now = crate::ids::now_iso();
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let current = get_computer(&tx, &record.computer_id)?;
            let Some(current) = current else {
                if expected_binding_revision != 0 || record.binding_revision != 1 {
                    return Err(fail("New computer registration requires expected revision 0 and binding revision 1."));
                }
                tx.execute(
                    "INSERT INTO computers(computer_id, endpoint_id, label, transport, host, user, port, identity_file_ref, known_hosts_file_ref, trust_state, binding_revision, authorization_generation, created_at, updated_at)
          VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'verified', ?, ?, ?, ?)",
                    params![
                        record.computer_id, record.endpoint_id, record.label, record.transport, record.host,
                        record.user, record.port, record.identity_file_ref, record.known_hosts_file_ref,
                        record.binding_revision, record.authorization_generation, now, now
                    ],
                )
                .map_err(endpoint_conflict)?;
                tx.execute("DELETE FROM directory_meta WHERE key=?", [operator_label_key(&record.computer_id)])?;
                let stored = get_computer(&tx, &record.computer_id)?.ok_or_else(|| fail("Registration did not persist."))?;
                tx.commit()?;
                return Ok(stored);
            };
            if current.binding_revision != expected_binding_revision {
                return Err(fail("Stale computer binding revision; reread before rebinding."));
            }
            if current.trust_state == "removed" {
                return Err(fail("Removed computers require an explicit restore workflow."));
            }
            let changed = current.endpoint_id != record.endpoint_id
                || current.transport != record.transport
                || current.host != record.host
                || current.user != record.user
                || current.port != record.port
                || current.identity_file_ref != record.identity_file_ref
                || current.known_hosts_file_ref != record.known_hosts_file_ref;
            let next_revision = expected_binding_revision + i64::from(changed);
            if !js::is_safe_integer(next_revision as f64) || record.binding_revision != next_revision {
                return Err(fail("Computer binding revision must advance exactly once when its verified route changes."));
            }
            // A name the operator chose outlives re-pairing.
            let chosen: Option<i64> = tx
                .query_row("SELECT 1 FROM directory_meta WHERE key=?", [operator_label_key(&record.computer_id)], |r| r.get(0))
                .optional()?;
            let label = if chosen.is_some() { &current.label } else { &record.label };
            let updated = tx.execute(
                "UPDATE computers SET endpoint_id=?, label=?, transport=?, host=?, user=?, port=?, identity_file_ref=?, known_hosts_file_ref=?, trust_state='verified', binding_revision=?, authorization_generation=?, updated_at=?
        WHERE computer_id=? AND binding_revision=? AND trust_state=?",
                params![
                    record.endpoint_id, label, record.transport, record.host, record.user, record.port,
                    record.identity_file_ref, record.known_hosts_file_ref, record.binding_revision,
                    record.authorization_generation, now, record.computer_id, expected_binding_revision,
                    current.trust_state
                ],
            )
            .map_err(endpoint_conflict)?;
            if updated != 1 {
                return Err(fail("Stale computer binding revision; reread before rebinding."));
            }
            let stored = get_computer(&tx, &record.computer_id)?.ok_or_else(|| fail("Registration did not persist."))?;
            tx.commit()?;
            Ok(stored)
        }
    }

    /// `getComputer(computerId)` (operator-directory.mjs:327).
    pub fn get_computer(&self, computer_id: &str) -> Result<Option<ComputerRecord>> {
        get_computer(&self.db, computer_id)
    }

    /// `renameComputer(computerId, label)` (operator-directory.mjs:335): the operator's own
    /// name for a verified computer; trust and route stay; later rebinds keep it.
    pub fn rename_computer(&mut self, computer_id: &str, label: &str) -> Result<ListedComputer> {
        if !pattern::id(computer_id) {
            return Err(fail("An explicit computer ID is required."));
        }
        let name = js::trim(label);
        if name.is_empty() || js::length(name) > LABEL_LIMIT {
            return Err(fail("Computer name must contain 1–128 characters."));
        }
        if name.chars().any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}')) {
            return Err(fail("Computer name cannot contain control characters."));
        }
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE computers SET label=?, updated_at=? WHERE computer_id=? AND trust_state='verified'",
            params![name, crate::ids::now_iso(), computer_id],
        )?;
        if updated != 1 {
            return Err(fail("No verified computer has that ID."));
        }
        tx.execute(
            "INSERT INTO directory_meta(key,value) VALUES (?, 'operator') ON CONFLICT(key) DO NOTHING",
            [operator_label_key(computer_id)],
        )?;
        let row = tx.query_row(
            &format!("SELECT {LISTED_COLUMNS} FROM computers WHERE computer_id = ?"),
            [computer_id],
            ListedComputer::from_row,
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Record how a verified computer can be woken (null forgets it). Only
    /// the reported shape is kept: `{mac, ifname, kind, subnet, from_off, gateway_mac?}`.
    pub fn set_wake(&mut self, computer_id: &str, wake: &Value) -> Result<()> {
        let key = wake_key(computer_id);
        if wake.is_null() {
            self.db.execute("DELETE FROM directory_meta WHERE key=?", [key])?;
            return Ok(());
        }
        let text = |name: &str| wake.get(name).and_then(Value::as_str).unwrap_or("");
        let valid = crate::wake::parse_mac(text("mac")).is_some()
            && crate::wake::parse_subnet(text("subnet")).is_some()
            && matches!(text("kind"), "wifi" | "ethernet")
            && (1..16).contains(&text("ifname").len())
            && wake.get("from_off").is_some_and(Value::is_boolean);
        if !valid || self.get_computer(computer_id)?.is_none_or(|r| r.trust_state != "verified") {
            return Err(fail("Invalid wake information."));
        }
        let mut kept = json!({"mac": text("mac").to_lowercase(), "ifname": text("ifname"), "kind": text("kind"), "subnet": text("subnet"), "from_off": wake["from_off"]});
        if crate::wake::parse_mac(text("gateway_mac")).is_some() {
            kept["gateway_mac"] = json!(text("gateway_mac").to_lowercase());
        }
        self.db.execute(
            "INSERT INTO directory_meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value
             WHERE directory_meta.value != excluded.value",
            params![key, kept.to_string()],
        )?;
        Ok(())
    }

    /// The name a computer gives itself becomes its label here, unless this
    /// operator chose one (`rename_computer`) or another computer here already
    /// has that label (ignoring case). Whether the label changed.
    pub fn adopt_label(&mut self, computer_id: &str, name: &str) -> Result<bool> {
        let name = js::trim(name);
        if name.is_empty() || js::length(name) > LABEL_LIMIT || name.chars().any(|c| c.is_control()) || self.label_taken(computer_id, name)? {
            return Ok(false);
        }
        let changed = self.db.execute(
            "UPDATE computers SET label=?, updated_at=? WHERE computer_id=? AND trust_state='verified' AND label != ?
             AND NOT EXISTS (SELECT 1 FROM directory_meta WHERE key=?)",
            params![name, crate::ids::now_iso(), computer_id, name, operator_label_key(computer_id)],
        )?;
        Ok(changed == 1)
    }

    /// Whether a verified computer other than `computer_id` is labelled
    /// `label` here, ignoring case: two cards would read the same.
    pub fn label_taken(&self, computer_id: &str, label: &str) -> Result<bool> {
        let label = js::trim(label).to_lowercase();
        Ok(self
            .list_computers()?
            .iter()
            .any(|r| r.computer_id != computer_id && r.trust_state == "verified" && js::trim(&r.label).to_lowercase() == label))
    }

    /// `listComputers()` (operator-directory.mjs:350): every computer not removed.
    pub fn list_computers(&self) -> Result<Vec<ListedComputer>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {LISTED_COLUMNS} FROM computers WHERE trust_state != 'removed' ORDER BY created_at, computer_id"
        ))?;
        let rows = stmt.query_map([], ListedComputer::from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove a computer registered moments ago whose route never answered.
    /// Only a first binding (revision 1) goes; a rebound computer is kept.
    pub fn forget_new_computer(&mut self, computer_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM computers WHERE computer_id = ? AND binding_revision = 1", [computer_id])?;
        Ok(())
    }

    /// Take a computer out of this directory: its row, and what was kept here
    /// about it (a chosen name, how to wake it, a legacy import). Pairing it
    /// again adds it as a new computer. The removed row, for its pinned host key.
    pub fn remove_computer(&mut self, computer_id: &str) -> Result<ComputerRecord> {
        if !pattern::id(computer_id) {
            return Err(fail("An explicit computer ID is required."));
        }
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(removed) = get_computer(&tx, computer_id)? else {
            return Err(fail("No computer has that ID."));
        };
        tx.execute("DELETE FROM computers WHERE computer_id = ?", [computer_id])?;
        for key in [operator_label_key(computer_id), wake_key(computer_id), format!("legacy_import:{computer_id}")] {
            tx.execute("DELETE FROM directory_meta WHERE key = ?", [key])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// `bindOperation(computerId, {requestId, recordId})` (operator-directory.mjs:355):
    /// the immutable route envelope for one operation. `request_id` defaults to a fresh
    /// `request_<uuid>`.
    pub fn bind_operation(&self, computer_id: &str, request_id: Option<&str>, record_id: &str) -> Result<SelectedEnvelope> {
        if !pattern::id(computer_id) {
            return Err(fail("An explicit computer ID is required."));
        }
        let request_id = request_id.map(str::to_string).unwrap_or_else(fresh_request_id);
        if !pattern::id(&request_id) || !pattern::id(record_id) {
            return Err(fail("Invalid selected operation identity."));
        }
        let record = self.get_computer(computer_id)?.filter(|r| r.trust_state == "verified");
        let Some(record) = record else {
            return Err(fail("No verified route exists for the selected computer."));
        };
        let port = u16::try_from(record.port).map_err(|_| fail("Invalid selected-target port."))?;
        Ok(SelectedEnvelope {
            environment_id: self.operator_instance_id.clone(),
            computer_id: record.computer_id,
            endpoint_id: record.endpoint_id,
            binding_revision: record.binding_revision,
            request_id,
            expected_authorization_generation: record.authorization_generation,
            record_id: record_id.to_string(),
            route: SelectedRoute {
                host: record.host,
                user: record.user,
                port,
                identity_file_ref: record.identity_file_ref,
                known_hosts_file_ref: record.known_hosts_file_ref,
            },
        })
    }
}

/// `request_${crypto.randomUUID()}`.
pub fn fresh_request_id() -> String {
    format!("request_{}", uuid::Uuid::new_v4().hyphenated())
}

// ---------------------------------------------------------------------------
// Naming a computer. Every command that names a computer (`ibara client`,
// `ibara operator`, `ibara mcp`, the console's commands, `computer_begin`) takes
// the same names, through `pick_computer`.

/// What a person or agent may call a computer.
#[derive(Debug, Clone, Copy)]
pub struct ComputerNames<'a> {
    /// This directory's id, `computer_<digest>`.
    pub computer_id: &'a str,
    pub endpoint_id: &'a str,
    /// The name this console gives it.
    pub label: &'a str,
    pub host: &'a str,
}

impl ComputerNames<'_> {
    /// The id the computer gives itself (`computer_status`, `computer_begin`):
    /// `cmp_` plus the directory id's digest.
    pub fn cmp_id(&self) -> String {
        match self.computer_id.strip_prefix("computer_") {
            Some(digest) => format!("cmp_{digest}"),
            None => self.computer_id.to_string(),
        }
    }

    /// Whether `name` names this computer: its name here or its host, in any
    /// case (the host's first part alone too), its `cmp_` or `computer_` id,
    /// or its endpoint id. An empty name names nothing.
    pub fn answers_to(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let lower = name.to_lowercase();
        let host = self.host.to_lowercase();
        name == self.computer_id
            || name == self.endpoint_id
            || name == self.cmp_id()
            || self.label.to_lowercase() == lower
            || host == lower
            || host.split('.').next() == Some(lower.as_str())
    }

    /// `"Name" (cmp_…, computer_…)`: the name and the ids a command takes
    /// (just the name when the computer has no id yet).
    fn choice(&self) -> String {
        if self.computer_id.is_empty() {
            return format!("\"{}\"", self.label);
        }
        format!("\"{}\" ({}, {})", self.label, self.cmp_id(), self.computer_id)
    }
}

/// A computer that can be named.
pub trait Named {
    fn names(&self) -> ComputerNames<'_>;
}

impl Named for ListedComputer {
    fn names(&self) -> ComputerNames<'_> {
        ComputerNames { computer_id: &self.computer_id, endpoint_id: &self.endpoint_id, label: &self.label, host: &self.host }
    }
}

/// Every computer as `"Name" (cmp_…, computer_…)`, comma-separated.
pub fn computer_choices<'a, T: Named + 'a>(computers: impl IntoIterator<Item = &'a T>) -> String {
    computers.into_iter().map(|c| c.names().choice()).collect::<Vec<_>>().join(", ")
}

/// The one computer in `computers` that `name` names. A name no computer
/// answers to, or that more than one does, is refused with the computers it
/// could mean and the names and ids each one takes.
pub fn pick_computer<'a, T: Named>(computers: &'a [T], name: &str) -> Result<&'a T> {
    let shown: String = name.chars().take(60).collect();
    let matches: Vec<&T> = computers.iter().filter(|c| c.names().answers_to(name)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] if computers.is_empty() => Err(IbaraError::new("INVALID_ARGUMENT", "No computers are added yet. Add one in the ibara console.", false)),
        [] => Err(fail(format!(
            "\"{shown}\" is not one of your computers. Name one by its name, host, cmp_ or computer_ id: {}.",
            computer_choices(computers)
        ))),
        several => Err(IbaraError::new(
            "AMBIGUOUS_TARGET",
            format!("\"{shown}\" names more than one computer. Use its cmp_ or computer_ id: {}.", computer_choices(several.iter().copied())),
            false,
        )),
    }
}

impl OperatorDirectory {
    /// The computer `name` names here (see `pick_computer`), among the verified
    /// ones. A `computer_` id of any computer here is taken as it is, so a
    /// computer still being added is refused for its route, not its name.
    pub fn resolve_computer(&self, name: &str) -> Result<ListedComputer> {
        let mut rows = self.list_computers()?;
        if let Some(at) = rows.iter().position(|r| r.computer_id == name) {
            return Ok(rows.swap_remove(at));
        }
        rows.retain(|r| r.trust_state == "verified");
        pick_computer(&rows, name).cloned()
    }
}

fn get_computer(db: &Connection, computer_id: &str) -> Result<Option<ComputerRecord>> {
    Ok(db
        .query_row(&format!("SELECT {ALL_COLUMNS} FROM computers WHERE computer_id = ?"), [computer_id], ComputerRecord::from_row)
        .optional()?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Open, bind one operation, close (`bindFresh`, ibara-operator.mjs:34).
pub fn bind_fresh(database: &Path, computer_id: &str, record_id: &str) -> Result<SelectedEnvelope> {
    let directory = OperatorDirectory::open(database)?;
    directory.bind_operation(computer_id, None, record_id)
}


#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A private temporary directory removed on drop.
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ibara-op-{tag}-{}", uuid::Uuid::new_v4().simple()));
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            TempDir(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn verified_record(dir: &Path, id: &str, endpoint: &str) -> Value {
        json!({
            "computer_id": id, "endpoint_id": endpoint, "label": "Tulip1", "transport": "ssh",
            "host": "tulip1", "user": "vesper", "port": 2222,
            "identity_file_ref": format!("file:{}", dir.join("id_ed25519").display()),
            "known_hosts_file_ref": format!("file:{}", dir.join("known_hosts").display()),
            "trust_state": "verified", "binding_revision": 1, "authorization_generation": 3,
        })
    }

    fn envelope() -> Value {
        json!({
            "schema_version": 1, "environment_id": "operator_x", "computer_id": "computer_a",
            "endpoint_id": "ibara_0123456789", "binding_revision": 1, "request_id": "request_1",
            "expected_authorization_generation": 3, "record_id": "mcp_1",
            "route": {"protocol": "ssh", "host": "tulip1", "user": "vesper", "port": 2222,
                      "identity_file_ref": "file:/k", "known_hosts_file_ref": "file:/h"},
        })
    }

    // Failure cases first: unsafe files and malformed envelopes are refused.

    #[test]
    fn database_file_with_group_or_other_bits_is_refused() {
        let tmp = TempDir::new("mode");
        let db = tmp.0.join("operator.sqlite");
        OperatorDirectory::open(&db).unwrap().close();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o640)).unwrap();
        let err = OperatorDirectory::open(&db).err().unwrap();
        assert_eq!(err.message, "Operator directory database must be a private, user-owned regular file.");
    }

    #[test]
    fn database_symlink_is_refused() {
        let tmp = TempDir::new("link");
        let real = tmp.0.join("real.sqlite");
        OperatorDirectory::open(&real).unwrap().close();
        let link = tmp.0.join("operator.sqlite");
        symlink(&real, &link).unwrap();
        let err = OperatorDirectory::open(&link).err().unwrap();
        assert_eq!(err.message, "Operator directory database must be a private, user-owned regular file.");
    }

    #[test]
    fn database_with_a_second_hard_link_is_refused() {
        let tmp = TempDir::new("nlink");
        let db = tmp.0.join("operator.sqlite");
        OperatorDirectory::open(&db).unwrap().close();
        fs::hard_link(&db, tmp.0.join("copy.sqlite")).unwrap();
        let err = OperatorDirectory::open(&db).err().unwrap();
        assert_eq!(err.message, "Operator directory database must be a private, user-owned regular file.");
    }

    #[test]
    fn database_owned_by_another_user_is_refused() {
        // /etc/shadow is a regular, single-link, mode 0600 file owned by root: only the
        // owner differs from a valid directory database.
        let shadow = Path::new("/etc/shadow");
        let stat = fs::symlink_metadata(shadow).unwrap();
        assert_eq!((stat.mode() & 0o077, stat.nlink()), (0, 1), "fixture assumption");
        let err = validate_database_file(shadow, false).err().unwrap();
        assert_eq!(err.message, "Operator directory database must be a private, user-owned regular file.");
    }

    #[test]
    fn unsafe_sidecar_is_refused() {
        let tmp = TempDir::new("sidecar");
        let db = tmp.0.join("operator.sqlite");
        OperatorDirectory::open(&db).unwrap().close();
        let wal = tmp.0.join("operator.sqlite-wal");
        let _ = fs::remove_file(&wal);
        symlink("/dev/null", &wal).unwrap();
        let err = OperatorDirectory::open(&db).err().unwrap();
        assert_eq!(err.message, "Unsafe SQLite sidecar: -wal");
    }

    #[test]
    fn parent_directory_that_is_not_private_is_refused() {
        let tmp = TempDir::new("parent");
        let parent = tmp.0.join("state");
        fs::DirBuilder::new().mode(0o755).create(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let err = OperatorDirectory::open(&parent.join("operator.sqlite")).err().unwrap();
        assert_eq!(err.message, "Operator directory parent must be owned by this user and private (mode 0700).");
    }

    #[test]
    fn symlinked_ancestor_is_refused() {
        let tmp = TempDir::new("ancestor");
        let real = tmp.0.join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        symlink(&real, tmp.0.join("alias")).unwrap();
        let err = OperatorDirectory::open(&tmp.0.join("alias").join("operator.sqlite")).err().unwrap();
        assert!(err.message.starts_with("Unsafe directory path: "), "{}", err.message);
    }

    #[test]
    fn existing_file_without_schema_is_refused() {
        let tmp = TempDir::new("noschema");
        let db = tmp.0.join("operator.sqlite");
        Connection::open(&db).unwrap().execute_batch("CREATE TABLE other(x);").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let err = OperatorDirectory::open(&db).err().unwrap();
        assert_eq!(err.message, "Existing operator directory has no supported schema; refusing to initialize over it.");
    }

    #[test]
    fn envelope_with_an_extra_key_is_refused() {
        let mut value = envelope();
        value["principal"] = json!("admin");
        assert_eq!(validate_selected_envelope(&value).err().unwrap().message, "Selected route envelope has unknown or missing fields.");
    }

    #[test]
    fn envelope_with_a_missing_key_is_refused() {
        let mut value = envelope();
        value.as_object_mut().unwrap().shift_remove("record_id");
        assert_eq!(validate_selected_envelope(&value).err().unwrap().message, "Selected route envelope has unknown or missing fields.");
    }

    #[test]
    fn envelope_route_with_an_extra_or_missing_key_is_refused() {
        let mut extra = envelope();
        extra["route"]["proxy"] = json!("jump");
        assert_eq!(validate_selected_envelope(&extra).err().unwrap().message, "Selected route is incomplete.");
        let mut missing = envelope();
        missing["route"].as_object_mut().unwrap().shift_remove("known_hosts_file_ref");
        assert_eq!(validate_selected_envelope(&missing).err().unwrap().message, "Selected route is incomplete.");
    }

    #[test]
    fn envelope_with_bad_generation_or_identity_is_refused() {
        let mut generation = envelope();
        generation["expected_authorization_generation"] = json!(-1);
        assert_eq!(validate_selected_envelope(&generation).err().unwrap().message, "Selected route envelope generation is invalid.");
        let mut identity = envelope();
        identity["endpoint_id"] = json!("short");
        assert_eq!(validate_selected_envelope(&identity).err().unwrap().message, "Selected route envelope identity is invalid.");
        let mut transport = envelope();
        transport["route"]["protocol"] = json!("tcp");
        assert_eq!(validate_selected_envelope(&transport).err().unwrap().message, "Selected route transport is unsupported.");
    }

    #[test]
    fn valid_envelope_round_trips_in_bind_order() {
        let parsed = validate_selected_envelope(&envelope()).unwrap();
        assert_eq!(parsed.to_json().to_string(), envelope().to_string());
    }

    #[test]
    fn rename_refuses_unknown_unverified_and_unusable_names() {
        let tmp = TempDir::new("rename");
        let mut dir = OperatorDirectory::open(&tmp.0.join("operator.sqlite")).unwrap();
        dir.register_verified_computer(&verified_record(&tmp.0, "computer_a", "ibara_0123456789"), "ibara_0123456789", 0)
            .unwrap();
        assert_eq!(dir.rename_computer("computer_b", "X").err().unwrap().message, "No verified computer has that ID.");
        assert_eq!(dir.rename_computer("bad id", "X").err().unwrap().message, "An explicit computer ID is required.");
        assert_eq!(dir.rename_computer("computer_a", "  ").err().unwrap().message, "Computer name must contain 1–128 characters.");
        assert_eq!(
            dir.rename_computer("computer_a", "a\u{2028}b").err().unwrap().message,
            "Computer name cannot contain control characters."
        );
        assert_eq!(dir.rename_computer("computer_a", &"é".repeat(129)).err().unwrap().message, "Computer name must contain 1–128 characters.");
        let renamed = dir.rename_computer("computer_a", "  Desk  ").unwrap();
        assert_eq!(renamed.label, "Desk");
        // The chosen name survives a rebind that changes the route.
        let mut rebound = verified_record(&tmp.0, "computer_a", "ibara_0123456789");
        rebound["host"] = json!("tulip1b");
        rebound["binding_revision"] = json!(2);
        let record = dir.register_verified_computer(&rebound, "ibara_0123456789", 1).unwrap();
        assert_eq!((record.label.as_str(), record.binding_revision), ("Desk", 2));
    }

    #[test]
    fn a_removed_computer_leaves_nothing_here_and_can_be_added_again_as_new() {
        let tmp = TempDir::new("remove");
        let mut dir = OperatorDirectory::open(&tmp.0.join("operator.sqlite")).unwrap();
        let record = verified_record(&tmp.0, "computer_a", "ibara_0123456789");
        dir.register_verified_computer(&record, "ibara_0123456789", 0).unwrap();
        dir.rename_computer("computer_a", "Desk").unwrap();
        let wake = json!({"mac": "02:00:00:00:00:01", "ifname": "wlp2s0", "kind": "wifi", "subnet": "10.0.0.0/24", "from_off": false});
        dir.set_wake("computer_a", &wake).unwrap();
        assert_eq!(dir.remove_computer("computer_b").err().unwrap().message, "No computer has that ID.");
        assert_eq!(dir.remove_computer("bad id").err().unwrap().message, "An explicit computer ID is required.");
        let removed = dir.remove_computer("computer_a").unwrap();
        assert_eq!(removed.known_hosts_file_ref, format!("file:{}", tmp.0.join("known_hosts").display()));
        assert!(dir.get_computer("computer_a").unwrap().is_none());
        let kept: i64 = dir.db.query_row("SELECT count(*) FROM directory_meta WHERE key LIKE '%computer_a%'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 0, "the chosen name and wake information go with the computer");
        assert_eq!(dir.remove_computer("computer_a").err().unwrap().message, "No computer has that ID.");
        // Added again: a first binding, with the name the computer gives itself.
        let again = dir.register_verified_computer(&record, "ibara_0123456789", 0).unwrap();
        assert_eq!((again.label.as_str(), again.binding_revision), ("Tulip1", 1));
    }

    #[test]
    fn rebind_must_advance_revision_exactly_once_and_endpoint_is_unique() {
        let tmp = TempDir::new("rebind");
        let mut dir = OperatorDirectory::open(&tmp.0.join("operator.sqlite")).unwrap();
        let first = verified_record(&tmp.0, "computer_a", "ibara_0123456789");
        dir.register_verified_computer(&first, "ibara_0123456789", 0).unwrap();
        let mut unchanged_bump = first.clone();
        unchanged_bump["binding_revision"] = json!(2);
        assert_eq!(
            dir.register_verified_computer(&unchanged_bump, "ibara_0123456789", 1).err().unwrap().message,
            "Computer binding revision must advance exactly once when its verified route changes."
        );
        assert_eq!(
            dir.register_verified_computer(&first, "ibara_0123456789", 0).err().unwrap().message,
            "Stale computer binding revision; reread before rebinding."
        );
        assert_eq!(
            dir.register_verified_computer(&first, "ibara_other_endpoint", 0).err().unwrap().message,
            "A verified computer requires a matching authenticated endpoint identity."
        );
        let second = verified_record(&tmp.0, "computer_b", "ibara_0123456789");
        assert_eq!(
            dir.register_verified_computer(&second, "ibara_0123456789", 0).err().unwrap().message,
            "Verified endpoint identity is already registered."
        );
    }

    #[test]
    fn bind_refuses_unverified_and_listing_hides_credentials() {
        let tmp = TempDir::new("bind");
        let mut dir = OperatorDirectory::open(&tmp.0.join("operator.sqlite")).unwrap();
        let descriptor = json!({"schema_version": 1, "station_id": "station_a", "node": "tulip0",
            "display_label": " Tulip0 ", "agent_account": "tulip0", "operator_account": "riley",
            "services": {"controller": {"scope": "user", "unit": "agent-computer.service"}}});
        let imported = dir.import_legacy_descriptor(&descriptor).unwrap();
        assert_eq!((imported.trust_state.as_str(), imported.label.as_str()), ("unverified", "Tulip0"));
        // Re-import of the same preimage returns the saved row; a changed one conflicts.
        assert_eq!(dir.import_legacy_descriptor(&descriptor).unwrap(), imported);
        let mut changed = descriptor.clone();
        changed["node"] = json!("tulip9");
        assert_eq!(dir.import_legacy_descriptor(&changed).err().unwrap().message, "Legacy descriptor conflicts with the saved import.");
        assert_eq!(
            dir.bind_operation("station_a", None, "status").err().unwrap().message,
            "No verified route exists for the selected computer."
        );
        assert_eq!(dir.bind_operation("station_a", Some("bad id"), "x").err().unwrap().message, "Invalid selected operation identity.");
        let listed = serde_json::to_string(&dir.list_computers().unwrap()[0].to_json()).unwrap();
        assert!(listed.starts_with(r#"{"computer_id":"station_a","endpoint_id":"unverified:station_a","label":"Tulip0","host":"tulip0""#), "{listed}");
        assert!(!listed.contains("file_ref"));
    }

    #[test]
    fn index_definition_normalisation_matches_the_node_rules() {
        assert_eq!(
            normalize_index_sql("CREATE UNIQUE INDEX \"computers_verified_endpoint_id_unique\"\n  ON computers ( endpoint_id )  WHERE trust_state='verified';"),
            "create unique index computers_verified_endpoint_id_unique on computers(endpoint_id) where trust_state = 'verified'"
        );
    }
}
