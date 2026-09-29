//! `storage.sqlite` schema: the TypeScript tables verbatim (storage.ts:724-819),
//! then additive migrations numbered by `PRAGMA user_version`. The TypeScript
//! never set `user_version`, so its databases are version 0. Every migration
//! only adds, so the Node controller can still open a migrated file.

use crate::error::Result;
use rusqlite::Connection;

/// The latest additive migration this code applies.
pub const SCHEMA_VERSION: i64 = 1;

const OPERATOR_FILE_JOBS: &str = "CREATE TABLE IF NOT EXISTS operator_file_jobs (job_id TEXT PRIMARY KEY, owner TEXT NOT NULL, root_id TEXT NOT NULL, relative_directory TEXT NOT NULL, name TEXT NOT NULL, size_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, staging TEXT NOT NULL, offset_bytes INTEGER NOT NULL, state TEXT NOT NULL)";

const TABLES: &str = "
      CREATE TABLE IF NOT EXISTS artifacts (
        artifact_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        principal TEXT NOT NULL,
        name TEXT NOT NULL,
        mime_type TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        sha256 TEXT NOT NULL,
        producer_ref TEXT NOT NULL,
        evidence_json TEXT NOT NULL,
        delivery TEXT NOT NULL,
        file_name TEXT NOT NULL,
        created_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS artifact_readers (
        principal TEXT NOT NULL,
        connection_id TEXT NOT NULL,
        artifact_ref TEXT NOT NULL,
        expires_ms INTEGER NOT NULL,
        PRIMARY KEY (principal, connection_id, artifact_ref)
      );
      CREATE TABLE IF NOT EXISTS staged (
        staged_ref TEXT PRIMARY KEY,
        principal TEXT NOT NULL,
        name TEXT NOT NULL,
        mime_type TEXT,
        size_bytes INTEGER NOT NULL,
        sha256 TEXT,
        received_bytes INTEGER NOT NULL,
        complete INTEGER NOT NULL,
        file_name TEXT NOT NULL,
        created_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS jobs (
        job_ref TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        request_id TEXT NOT NULL,
        state TEXT NOT NULL,
        pid INTEGER,
        pgid INTEGER,
        exit_code INTEGER,
        stdout TEXT,
        stderr TEXT,
        output_truncated INTEGER NOT NULL DEFAULT 0,
        termination_confirmed INTEGER,
        record TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS procedures (
        procedure_ref TEXT PRIMARY KEY,
        version TEXT NOT NULL,
        status TEXT NOT NULL,
        candidate_name TEXT,
        record TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS collection_receipts (
        receipt_id TEXT PRIMARY KEY,
        task_ref TEXT NOT NULL,
        artifact_ref TEXT NOT NULL,
        obligation_id TEXT,
        revision INTEGER,
        principal TEXT NOT NULL,
        host_id TEXT,
        destination_path TEXT,
        sha256 TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        collected_at TEXT NOT NULL
      );
";

/// Version 1: the step that last wrote each workspace path through storage, so
/// a contract-4 `publish` by path can link the producing operation.
const V1_FILE_WRITERS: &str = "
      CREATE TABLE IF NOT EXISTS file_writers (
        task_ref TEXT NOT NULL,
        path TEXT NOT NULL,
        operation_ref TEXT NOT NULL,
        sha256 TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        written_at TEXT NOT NULL,
        PRIMARY KEY (task_ref, path)
      );
";

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

fn add_missing(conn: &Connection, table: &str, wanted: &[(&str, &str)]) -> Result<()> {
    let have = columns(conn, table)?;
    for (name, ty) in wanted {
        if !have.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"))?;
        }
    }
    Ok(())
}

/// Open-time schema work, in the TypeScript order, then numbered migrations.
pub fn migrate(conn: &Connection, now_iso: &str) -> Result<()> {
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous = FULL")?;
    conn.execute_batch(OPERATOR_FILE_JOBS)?;
    add_missing(
        conn,
        "operator_file_jobs",
        &[
            ("dest_dev", "TEXT"),
            ("dest_ino", "TEXT"),
            ("staging_dev", "TEXT"),
            ("staging_ino", "TEXT"),
            ("pub_dev", "TEXT"),
            ("pub_ino", "TEXT"),
            ("pub_birth_ns", "TEXT"),
        ],
    )?;
    // storage.ts:738 loads `publishing` jobs as `uncertain`: a lost response is
    // not permission to replay publication. Jobs load from the table on demand
    // here, so the restart rule is written through.
    conn.execute("UPDATE operator_file_jobs SET state = 'uncertain' WHERE state = 'publishing'", [])?;
    conn.execute_batch(TABLES)?;
    add_missing(conn, "jobs", &[("principal", "TEXT"), ("boot_id", "TEXT"), ("starttime", "INTEGER")])?;
    add_missing(conn, "artifacts", &[("collected_at", "TEXT")])?;
    conn.execute(
        "UPDATE artifacts SET collected_at = COALESCE(collected_at, ?1) WHERE delivery = 'collected' AND collected_at IS NULL",
        [now_iso],
    )?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(V1_FILE_WRITERS)?;
    }
    if version < SCHEMA_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }
    Ok(())
}
