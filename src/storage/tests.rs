//! Failure-case tests for storage. The cases, written first:
//!
//! Path confinement
//! - read_refuses_parent_traversal_and_absolute_paths
//! - read_refuses_symlink_leaf
//! - write_refuses_symlinked_directory_component_and_symlink_leaf
//! - publish_refuses_symlink_source_and_absolute_path_outside_data_root
//! - operator_files_refuse_parent_traversal_and_symlinked_root
//!
//! Exec
//! - exec_refuses_cwd_outside_or_not_a_workspace_directory
//! - exec_runs_in_workspace_with_only_allowlisted_env
//! - exec_output_cap_truncates_and_marks_output
//! - exec_missing_program_fails_127_without_a_live_job
//! - cancel_jobs_terminates_the_whole_process_group
//! - recovery_confirms_dead_groups_and_never_signals_a_reused_group
//!
//! Uploads
//! - transfer_upload_chunk_refuses_offset_mismatch
//! - transfer_upload_refuses_sha_mismatch_on_last_chunk
//! - operator_upload_refuses_offset_mismatch_and_changed_prefix
//! - operator_publish_refuses_bytes_that_do_not_match_the_declared_sha
//! - operator_publication_reconciles_only_the_bound_inode
//! - operator_files_over_the_limit_are_refused_with_both_sizes
//!
//! Artifacts
//! - download_reader_pins_are_capped
//! - expiry_never_removes_uncollected_bytes
//! - expiry_keeps_collected_bytes_until_the_journal_allows
//! - send_records_an_obligation_verified_only_by_a_matching_receipt
//! - publish_links_the_recorded_writer_only_for_the_same_bytes
//!
//! Procedures
//! - forbidden_refs_match_the_typescript_pattern
//! - candidate_validation_refuses_self_approval_and_element_refs
//! - procedure_digest_keeps_stored_key_order
//!
//! Migration
//! - real_storage_databases_open_and_keep_their_rows

use super::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI64;
use parking_lot::Mutex;
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-storage-test"));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod").args(["-R", "u+w"]).arg(&self.0).status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn options(root: &Path) -> StorageOptions {
    let mut o = StorageOptions::new(root.join("data"));
    o.state_dir = Some(root.join("state"));
    o.approved_procedures_dir = Some(root.join("approved"));
    o.candidate_procedures_dir = Some(root.join("candidates"));
    o.min_free_bytes = 0;
    o.max_exec_timeout_ms = 10_000;
    o.chunk_bytes = 32;
    o
}

fn open(root: &Path) -> StorageService {
    StorageService::open(options(root)).unwrap()
}

fn ctx(task: &str, principal: &str) -> Context {
    Context::new(task, principal, "epoch1")
}

fn code<T: std::fmt::Debug>(r: Result<T>) -> &'static str {
    r.expect_err("expected an error").code
}

fn sha(bytes: &[u8]) -> String {
    fsx::sha256_hex(bytes)
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A journal the tests control.
#[derive(Default)]
struct FakeJournal {
    tasks: Mutex<HashMap<String, TaskView>>,
    collectors: Mutex<HashMap<String, String>>,
    unresolved: Mutex<HashSet<String>>,
    broken: Mutex<bool>,
}

impl FakeJournal {
    fn task(&self, task_ref: &str, state: &str, deliveries: Vec<Value>) {
        self.tasks.lock().insert(task_ref.into(), TaskView { state: state.into(), deliveries });
    }
}

impl JournalView for FakeJournal {
    fn read_task(&self, _principal: &str, task_ref: &str) -> Result<TaskView> {
        self.tasks.lock().get(task_ref).cloned().ok_or_else(|| fail("PERMISSION_DENIED", "no task", true))
    }
    fn collector(&self, principal: &str) -> Option<String> {
        self.collectors.lock().get(principal).cloned()
    }
    fn task_state(&self, task_ref: &str) -> Result<Option<TaskState>> {
        Ok(self
            .tasks.lock()
            .get(task_ref)
            .map(|t| TaskState { state: t.state.clone(), updated_at: "2000-01-01T00:00:00.000Z".into() }))
    }
    fn has_unresolved(&self, task_ref: &str) -> Result<bool> {
        if *self.broken.lock() {
            return Err(fail("INTERNAL_ERROR", "journal unreadable", false));
        }
        Ok(self.unresolved.lock().contains(task_ref))
    }
    fn has_unsettled(&self, task_ref: &str) -> Result<bool> {
        self.has_unresolved(task_ref)
    }
}

fn write(s: &StorageService, c: &Context, path: &str, text: &str) {
    s.files(&json!({ "kind": "write", "path": path, "text": text }), c).unwrap();
}

// ---------------------------------------------------------------- paths

#[test]
fn read_refuses_parent_traversal_and_absolute_paths() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    std::fs::write(t.0.join("data/secret"), "secret").unwrap();
    for path in ["../secret", "a/../../secret", "..", "/etc/passwd", "a\\b", "x\u{1}y"] {
        assert_eq!(code(s.files(&json!({ "kind": "read", "path": path }), &c)), "INVALID_ARGUMENT", "{path}");
    }
    assert_eq!(code(s.files(&json!({ "kind": "list", "path": "../.." }), &c)), "INVALID_ARGUMENT");
    assert_eq!(code(s.workspace("../task", true)), "INVALID_ARGUMENT");
}

#[test]
fn read_refuses_symlink_leaf() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let ws = s.workspace("task_1", true).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", ws.join("link")).unwrap();
    assert_eq!(code(s.files(&json!({ "kind": "read", "path": "link" }), &c)), "PERMISSION_DENIED");
    assert_eq!(code(s.resolve_file_reference("link", &c)), "PERMISSION_DENIED");
}

#[test]
fn write_refuses_symlinked_directory_component_and_symlink_leaf() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let outside = t.0.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim.txt"), "original").unwrap();
    let ws = s.workspace("task_1", true).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("dir")).unwrap();
    std::os::unix::fs::symlink(outside.join("victim.txt"), ws.join("leaf.txt")).unwrap();

    assert_eq!(code(s.files(&json!({ "kind": "write", "path": "dir/new.txt", "text": "x" }), &c)), "PERMISSION_DENIED");
    assert!(!outside.join("new.txt").exists());
    let overwrite = json!({ "kind": "write", "path": "leaf.txt", "text": "x", "overwrite": true, "expected_sha256": sha(b"original") });
    assert_eq!(code(s.files(&overwrite, &c)), "PERMISSION_DENIED");
    assert_eq!(std::fs::read_to_string(outside.join("victim.txt")).unwrap(), "original");
    // An unguarded overwrite of a real file is refused too.
    write(&s, &c, "notes.txt", "one");
    assert_eq!(code(s.files(&json!({ "kind": "write", "path": "notes.txt", "text": "two" }), &c)), "REQUEST_CONFLICT");
}

#[test]
fn publish_refuses_a_symlink_source_and_any_path_that_is_not_relative() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let outside = t.0.join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    let ws = s.workspace("task_1", true).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("link.txt")).unwrap();

    assert_eq!(code(s.publish_file("link.txt", &c, None, None, &[])), "PERMISSION_DENIED");
    // Storage takes only paths relative to the call's folder; the controller
    // resolves an agent's absolute path first.
    assert_eq!(code(s.publish_file(outside.to_str().unwrap(), &c, None, None, &[])), "INVALID_ARGUMENT");
    assert_eq!(code(s.publish_file("../../outside.txt", &c, None, None, &[])), "INVALID_ARGUMENT");
    assert!(s.artifacts("task_1", "hazel").unwrap().is_empty());
}

#[test]
fn operator_files_refuse_parent_traversal_and_symlinked_root() {
    let t = TempDir::new();
    let real = t.0.join("roots/real");
    std::fs::create_dir_all(real.join("sub")).unwrap();
    std::fs::write(t.0.join("roots/secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(&real, t.0.join("roots/linked")).unwrap();
    let mut o = options(&t.0);
    o.operator_roots = vec![
        ("files".into(), real.to_str().unwrap().into()),
        ("linked".into(), t.0.join("roots/linked").to_str().unwrap().into()),
        ("Bad".into(), real.to_str().unwrap().into()),
        ("unnormal".into(), format!("{}/../real", real.display())),
    ];
    let s = StorageService::open(o).unwrap();
    let roots = s.operator_files("vesper", &json!({ "op": "files_roots" })).unwrap();
    assert_eq!(roots["roots"], json!([{ "root_id": "files" }, { "root_id": "linked" }]));

    let list = |dir: &str| s.operator_files("vesper", &json!({ "op": "files_list", "root_id": "files", "relative_directory": dir }));
    assert!(list("sub").is_ok());
    assert_eq!(code(list("..")), "INVALID_ARGUMENT");
    assert_eq!(code(list("sub/../..")), "INVALID_ARGUMENT");
    assert_eq!(code(list("/etc")), "INVALID_ARGUMENT");
    assert!(s.operator_files("vesper", &json!({ "op": "files_list", "root_id": "linked" })).is_err());
    assert_eq!(code(s.operator_files("vesper", &json!({ "op": "files_list", "root_id": "Bad" }))), "PERMISSION_DENIED");
    let download = json!({ "op": "files_begin_download", "root_id": "files", "relative_path": "../secret.txt" });
    assert_eq!(code(s.operator_files("vesper", &download)), "INVALID_ARGUMENT");
    std::os::unix::fs::symlink(t.0.join("roots/secret.txt"), real.join("link.txt")).unwrap();
    let download = json!({ "op": "files_begin_download", "root_id": "files", "relative_path": "link.txt" });
    assert!(s.operator_files("vesper", &download).is_err());
}

// ---------------------------------------------------------------- exec

#[tokio::test]
async fn exec_refuses_cwd_outside_or_not_a_workspace_directory() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let ws = s.workspace("task_1", true).unwrap();
    std::os::unix::fs::symlink(&t.0, ws.join("up")).unwrap();
    write(&s, &c, "file.txt", "x");
    let run = |cwd: &str| json!({ "program": "true", "cwd": cwd, "timeout_ms": 5000 });
    assert_eq!(code(s.exec(&run(".."), &c).await), "INVALID_ARGUMENT");
    assert_eq!(code(s.exec(&run("/tmp"), &c).await), "INVALID_ARGUMENT");
    assert_eq!(code(s.exec(&run("up"), &c).await), "PERMISSION_DENIED");
    assert_eq!(code(s.exec(&run("file.txt"), &c).await), "INVALID_ARGUMENT");
    assert_eq!(code(s.exec(&run("missing"), &c).await), "INVALID_ARGUMENT");
    assert_eq!(code(s.exec(&json!({ "program": "true", "timeout_ms": "soon" }), &c).await), "INVALID_ARGUMENT");
}

#[tokio::test]
async fn exec_runs_in_workspace_with_only_allowlisted_env() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let ws = s.workspace("task_1", true).unwrap().canonicalize().unwrap();
    let out = s.exec(&json!({ "program": "sh", "args": ["-c", "pwd; env"], "timeout_ms": 5000 }), &c).await.unwrap();
    let job = &out.records[0];
    assert_eq!(job["state"], "completed", "{job}");
    let stdout = job["stdout"].as_str().unwrap();
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some(ws.to_str().unwrap()));
    let names: HashSet<&str> = lines.filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    assert!(names.contains("LC_ALL"), "{stdout}");
    let allowed: HashSet<&str> = DEFAULT_ENV.iter().copied().chain(["LC_ALL", "PWD", "SHLVL", "_", "OLDPWD"]).collect();
    let leaked: Vec<&&str> = names.iter().filter(|n| !allowed.contains(**n)).collect();
    assert!(leaked.is_empty(), "leaked {leaked:?}");
    assert!(stdout.contains("LC_ALL=C"));
    let outside = std::env::vars().map(|(k, _)| k).find(|k| !allowed.contains(k.as_str()));
    if let Some(k) = outside {
        assert!(!names.contains(k.as_str()), "{k} leaked");
    }
}

#[tokio::test]
async fn exec_output_cap_truncates_and_marks_output() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let script = "i=0; while [ $i -lt 300 ]; do printf 0123456789; i=$((i+1)); done; printf '\\001\\002ok' >&2";
    let out = s
        .exec(&json!({ "program": "sh", "args": ["-c", script], "timeout_ms": 5000, "max_output_chars": 10 }), &c)
        .await
        .unwrap();
    let job = &out.records[0];
    assert_eq!(job["state"], "completed");
    assert_eq!(job["output_truncated"], true);
    assert_eq!(job["stdout"], "0123456789");
    // The cap (4 bytes per character over both streams) was spent on stdout.
    assert_eq!(job["stderr"], "");
    let stored = s.get_job(job["job_ref"].as_str().unwrap(), Some("task_1"), Some("hazel")).unwrap().unwrap();
    assert_eq!(stored["stdout"], "0123456789");
    assert!(s.get_job(job["job_ref"].as_str().unwrap(), Some("task_1"), Some("vesper")).unwrap().is_none());
}

#[tokio::test]
async fn exec_missing_program_fails_127_without_a_live_job() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let out = s.exec(&json!({ "program": "ibara-no-such-program", "timeout_ms": 5000 }), &c).await.unwrap();
    assert_eq!(out.records[0]["state"], "failed");
    assert_eq!(out.records[0]["exit_code"], 127);
    assert_eq!(out.records[0]["termination_confirmed"], true);
    assert!(!s.has_active_jobs(Some("task_1")));
}

#[tokio::test]
async fn cancel_jobs_terminates_the_whole_process_group() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let out = s
        .exec(&json!({ "program": "sh", "args": ["-c", "sleep 30 & sleep 30"], "timeout_ms": 10000, "background": true }), &c)
        .await
        .unwrap();
    let job_ref = out.records[0]["job_ref"].as_str().unwrap().to_string();
    assert_eq!(out.records[0]["state"], "running");
    assert!(s.has_active_jobs(Some("task_1")));
    let pgid: i64 = s.with_db(|db| Ok(db.query_row("SELECT pgid FROM jobs WHERE job_ref = ?1", [&job_ref], |r| r.get(0))?)).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(pgid_has_live_members(pgid), Some(true));
    assert!(s.cancel_jobs(Some("task_1")).await.unwrap());
    assert!(!s.has_active_jobs(None));
    assert_eq!(pgid_has_live_members(pgid), Some(false));
    let job = s.get_job(&job_ref, None, None).unwrap().unwrap();
    assert_eq!(job["state"], "cancelled");
    assert_eq!(job["termination_confirmed"], true);
    let check = s.check(&json!({ "kind": "job_finished", "job_ref": job_ref }), &c).unwrap();
    assert_eq!(check["outcome"], "satisfied");
}

#[tokio::test]
async fn recovery_confirms_dead_groups_and_never_signals_a_reused_group() {
    let t = TempDir::new();
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i64;
        child.wait().unwrap();
        pid
    };
    let me = std::process::id() as i64;
    let my_stat = read_process_stat(me).unwrap();
    let boot = current_boot_id();
    {
        let s = open(&t.0);
        s.with_db(|db| {
            let insert = "INSERT INTO jobs(job_ref, task_ref, request_id, state, pid, pgid, output_truncated, termination_confirmed, record, updated_at, principal, boot_id, starttime)
                          VALUES (?1, 'task_1', ?2, 'running', ?3, ?4, 0, 0, ?5, '2026-01-01T00:00:00.000Z', 'hazel', ?6, ?7)";
            let rec = |j: &str| json!({ "kind": "job", "job_ref": j, "state": "running", "stdout": "" }).to_string();
            db.execute(insert, rusqlite::params!["job_dead", "r1", dead_pid, dead_pid, rec("job_dead"), boot, 12345])?;
            // Our own process group, with a start time that is not ours: a reused group.
            db.execute(
                insert,
                rusqlite::params!["job_reused", "r2", me, my_stat.pgrp, rec("job_reused"), boot, my_stat.starttime + 1_000_000],
            )?;
            db.execute(insert, rusqlite::params!["job_old_boot", "r3", me, my_stat.pgrp, rec("job_old_boot"), "not-this-boot", 1])?;
            Ok(())
        })
        .unwrap();
        s.close();
    }
    let s = open(&t.0);
    let job = |r: &str| s.get_job(r, None, None).unwrap().unwrap();
    assert_eq!(job("job_dead")["state"], "unknown");
    assert_eq!(job("job_dead")["termination_confirmed"], true);
    assert_eq!(job("job_old_boot")["termination_confirmed"], true);
    assert_eq!(job("job_reused")["state"], "unknown");
    assert_eq!(job("job_reused")["termination_confirmed"], false);
    assert!(s.has_active_jobs(Some("task_1")));
    // Cancelling must not signal the reused group (it contains this test process).
    assert!(!s.cancel_jobs(Some("task_1")).await.unwrap());
    assert_eq!(job("job_reused")["termination_confirmed"], false);
    let c = ctx("task_1", "hazel");
    assert_eq!(s.check(&json!({ "kind": "request_settled", "request_id": "r1" }), &c).unwrap()["outcome"], "unknown");
    assert_eq!(s.check(&json!({ "kind": "request_settled", "request_id": "nope" }), &c).unwrap()["outcome"], "pending");
}

// ---------------------------------------------------------------- uploads

#[test]
fn transfer_upload_chunk_refuses_offset_mismatch() {
    let t = TempDir::new();
    let s = open(&t.0);
    let body = b"hello-staged-file";
    let begin = s.transfer("hazel", &json!({ "kind": "begin_upload", "name": "a.txt", "size_bytes": body.len() }), None).unwrap();
    let staged = begin["staged_ref"].as_str().unwrap();
    let chunk = |offset: usize, data: &[u8]| json!({ "kind": "upload_chunk", "staged_ref": staged, "offset": offset, "data": b64(data) });
    assert_eq!(code(s.transfer("hazel", &chunk(3, &body[3..6]), None)), "INVALID_ARGUMENT");
    s.transfer("hazel", &chunk(0, &body[..6]), None).unwrap();
    assert_eq!(code(s.transfer("hazel", &chunk(0, &body[..6]), None)), "INVALID_ARGUMENT");
    assert_eq!(code(s.transfer("hazel", &chunk(6, &[b'x'; 40]), None)), "BUDGET_EXCEEDED");
    assert_eq!(code(s.transfer("vesper", &chunk(6, &body[6..]), None)), "PERMISSION_DENIED");
    let done = s.transfer("hazel", &chunk(6, &body[6..]), None).unwrap();
    assert_eq!(done["kind"], "upload_receipt");
    assert_eq!(done["sha256"], sha(body));
    assert_eq!(code(s.transfer("hazel", &chunk(body.len(), b"x"), None)), "REQUEST_CONFLICT");
}

#[test]
fn transfer_upload_refuses_sha_mismatch_on_last_chunk() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let begin = s
        .transfer("hazel", &json!({ "kind": "begin_upload", "name": "a.txt", "size_bytes": 4, "sha256": sha(b"good") }), None)
        .unwrap();
    let staged = begin["staged_ref"].as_str().unwrap();
    let last = json!({ "kind": "upload_chunk", "staged_ref": staged, "offset": 0, "data": b64(b"evil"), "last": true });
    assert_eq!(code(s.transfer("hazel", &last, None)), "POSTCONDITION_FAILED");
    let stat = s.transfer("hazel", &json!({ "kind": "stat_staged", "staged_ref": staged }), None).unwrap();
    assert_eq!(stat["complete"], false);
    assert_eq!(stat["received_bytes"], 0);
    let import = json!({ "kind": "import_staged", "staged_ref": staged, "destination": "a.txt" });
    assert_eq!(code(s.files(&import, &c)), "INVALID_ARGUMENT");
    let short = s.transfer("hazel", &json!({ "kind": "begin_upload", "name": "b", "size_bytes": 4 }), None).unwrap();
    let early = json!({ "kind": "upload_chunk", "staged_ref": short["staged_ref"], "offset": 0, "data": b64(b"ab"), "last": true });
    assert_eq!(code(s.transfer("hazel", &early, None)), "POSTCONDITION_FAILED");
}

fn operator_service(t: &TempDir) -> (StorageService, PathBuf) {
    let root = t.0.join("Downloads");
    std::fs::create_dir_all(&root).unwrap();
    let mut o = options(&t.0);
    o.operator_roots = vec![("transfers".into(), root.to_str().unwrap().into())];
    o.max_artifact_bytes = 1 << 20;
    (StorageService::open(o).unwrap(), root)
}

fn begin_operator_upload(s: &StorageService, job: &str, body: &[u8], declared_sha: &str) {
    let begin = json!({ "op": "files_begin_upload", "root_id": "transfers", "name": "in.bin", "size": body.len(), "sha256": declared_sha, "job_id": job });
    s.operator_files("vesper", &begin).unwrap();
}

fn operator_chunk(job: &str, offset: usize, bytes: &[u8]) -> Value {
    json!({ "op": "files_upload_chunk", "job_id": job, "offset": offset, "base64": b64(bytes) })
}

#[test]
fn operator_upload_refuses_offset_mismatch_and_changed_prefix() {
    let t = TempDir::new();
    let (s, root) = operator_service(&t);
    let body = b"0123456789abcdef";
    begin_operator_upload(&s, "job_up_1", body, &sha(body));
    assert_eq!(code(s.operator_files("vesper", &operator_chunk("job_up_1", 4, &body[4..8]))), "INVALID_ARGUMENT");
    assert_eq!(code(s.operator_files("hazel", &operator_chunk("job_up_1", 0, &body[..8]))), "PERMISSION_DENIED");
    let bad = json!({ "op": "files_upload_chunk", "job_id": "job_up_1", "offset": 0, "base64": "not base64!" });
    assert_eq!(code(s.operator_files("vesper", &bad)), "INVALID_ARGUMENT");
    s.operator_files("vesper", &operator_chunk("job_up_1", 0, &body[..8])).unwrap();
    assert_eq!(code(s.operator_files("vesper", &operator_chunk("job_up_1", 8, &[0u8; 20]))), "BUDGET_EXCEEDED");
    let resume = |prefix: &str| {
        json!({ "op": "files_resume_upload", "job_id": "job_up_1", "root_id": "transfers", "relative_path": "in.bin", "size": body.len(), "sha256": sha(body), "prefix_sha256": prefix })
    };
    assert_eq!(code(s.operator_files("vesper", &resume(&sha(b"wrong")))), "STALE_TARGET");
    let mut moved = resume(&sha(&body[..8]));
    moved["relative_path"] = json!("other.bin");
    assert_eq!(code(s.operator_files("vesper", &moved)), "STALE_TARGET");
    let resumed = s.operator_files("vesper", &resume(&sha(&body[..8]))).unwrap();
    assert_eq!(resumed["offset"], 8);
    assert_eq!(code(s.operator_files("vesper", &json!({ "op": "files_publish", "job_id": "job_up_1" }))), "INVALID_ARGUMENT");
    s.operator_files("vesper", &operator_chunk("job_up_1", 8, &body[8..])).unwrap();
    let published = s.operator_files("vesper", &json!({ "op": "files_publish", "job_id": "job_up_1" })).unwrap();
    assert_eq!(published["state"], "published");
    assert_eq!(std::fs::read(root.join("in.bin")).unwrap(), body);
    // The same identity cannot be reused.
    let again = json!({ "op": "files_begin_upload", "root_id": "transfers", "name": "x.bin", "size": 1, "sha256": sha(b"x"), "job_id": "job_up_1" });
    assert_eq!(code(s.operator_files("vesper", &again)), "INVALID_ARGUMENT");
}

#[test]
fn operator_files_over_the_limit_are_refused_with_both_sizes() {
    let t = TempDir::new();
    let (s, root) = operator_service(&t);
    let roots = s.operator_files("vesper", &json!({ "op": "files_roots" })).unwrap();
    assert_eq!(roots["max_file_bytes"], 1 << 20, "a console can refuse before sending");
    let over = (1 << 20) + (1 << 19);
    let begin = json!({ "op": "files_begin_upload", "root_id": "transfers", "name": "big.bin", "size": over, "sha256": sha(b"x"), "job_id": "job_big" });
    let refused = s.operator_files("vesper", &begin).unwrap_err();
    assert_eq!((refused.code, refused.message.as_str()), ("BUDGET_EXCEEDED", "This file is 1.6 MB; ibara sends files up to 1 MB."));
    assert!(std::fs::read_dir(t.0.join("state/operator-upload")).map_or(true, |mut dir| dir.next().is_none()), "nothing staged");
    // A file to fetch is refused by its size, before it is read.
    std::fs::write(root.join("big.bin"), vec![7u8; over]).unwrap();
    let refused = s.operator_files("vesper", &json!({ "op": "files_begin_download", "root_id": "transfers", "relative_path": "big.bin" })).unwrap_err();
    assert_eq!((refused.code, refused.message.as_str()), ("BUDGET_EXCEEDED", "This file is 1.6 MB; ibara sends files up to 1 MB."));
    // Sizes as a person reads them, in decimal units counted from the bytes (1 MB is 1,000,000 bytes).
    // The default limit is a round 250 MB; a file just over it is not called the limit's size.
    let limit = options(&t.0).max_artifact_bytes;
    assert_eq!(options(&t.0).max_file_bytes, limit);
    assert_eq!(file_too_large(1_288_490_189, limit), "This file is 1.3 GB; ibara sends files up to 250 MB.");
    assert_eq!(file_too_large(300 << 20, limit), "This file is 315 MB; ibara sends files up to 250 MB.");
    assert_eq!(file_too_large(256 << 20, limit), "This file is 268 MB; ibara sends files up to 250 MB.");
    assert_eq!(file_too_large(limit + 1, limit), "This file is a little over 250 MB; ibara sends files up to 250 MB.");
    assert_eq!(size_text(1), "1 byte");
    assert_eq!(size_text(999), "999 bytes");
    assert_eq!(size_text(1000), "1 KB");
    assert_eq!(size_text(9_960), "10 KB");
    // Rounding up to 1000 of a unit is the next unit.
    assert_eq!(size_text(999_999), "1 MB");
    assert_eq!(size_text(999_600_000), "1 GB");
    assert_eq!(size_text(2_500_000_000_000), "2.5 TB");
}

#[test]
fn operator_publish_refuses_bytes_that_do_not_match_the_declared_sha() {
    let t = TempDir::new();
    let (s, root) = operator_service(&t);
    let body = b"these are not the declared bytes";
    begin_operator_upload(&s, "job_up_2", body, &sha(b"something else entirely!!!!!!!!!!"));
    s.operator_files("vesper", &operator_chunk("job_up_2", 0, &body[..16])).unwrap();
    s.operator_files("vesper", &operator_chunk("job_up_2", 16, &body[16..])).unwrap();
    assert_eq!(code(s.operator_files("vesper", &json!({ "op": "files_publish", "job_id": "job_up_2" }))), "STALE_TARGET");
    assert!(!root.join("in.bin").exists());
    let status = s.operator_files("vesper", &json!({ "op": "files_status", "job_id": "job_up_2" })).unwrap();
    assert_eq!(status["state"], "uploading");
}

#[test]
fn operator_publication_reconciles_only_the_bound_inode() {
    let t = TempDir::new();
    let body = b"published bytes";
    let resume = |job: &str| {
        json!({ "op": "files_resume_upload", "job_id": job, "root_id": "transfers", "relative_path": "in.bin", "size": body.len(), "sha256": sha(body), "prefix_sha256": sha(body) })
    };
    let root = {
        let (s, root) = operator_service(&t);
        begin_operator_upload(&s, "job_pub", body, &sha(body));
        s.operator_files("vesper", &operator_chunk("job_pub", 0, body)).unwrap();
        s.operator_files("vesper", &json!({ "op": "files_publish", "job_id": "job_pub" })).unwrap();
        // Simulate a crash after the link but before `published` was recorded.
        s.with_db(|db| Ok(db.execute("UPDATE operator_file_jobs SET state = 'publishing' WHERE job_id = 'job_pub'", [])?)).unwrap();
        s.close();
        root
    };
    {
        let (s, _) = operator_service(&t);
        let status = s.operator_files("vesper", &json!({ "op": "files_status", "job_id": "job_pub" })).unwrap();
        assert_eq!(status["state"], "uncertain");
        let reconciled = s.operator_files("vesper", &resume("job_pub")).unwrap();
        assert_eq!(reconciled["state"], "published");
        assert_eq!(reconciled["retry_safe"], false);
    }
    // Same bytes under the chosen name, but a different inode: not this publication.
    {
        let (s, _) = operator_service(&t);
        s.with_db(|db| Ok(db.execute("UPDATE operator_file_jobs SET state = 'uncertain' WHERE job_id = 'job_pub'", [])?)).unwrap();
        std::fs::remove_file(root.join("in.bin")).unwrap();
        std::fs::write(root.join("in.bin"), body).unwrap();
        assert_eq!(code(s.operator_files("vesper", &resume("job_pub"))), "STALE_TARGET");
    }
}

// ---------------------------------------------------------------- artifacts

fn publish(s: &StorageService, c: &Context, path: &str, text: &str) -> Value {
    write(s, c, path, text);
    s.files(&json!({ "kind": "publish", "path": path }), c).unwrap().records.remove(0)
}

#[test]
fn download_reader_pins_are_capped() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let artifact = publish(&s, &c, "big.txt", &"x".repeat(100));
    let r = artifact["artifact_ref"].as_str().unwrap();
    let chunk = |offset: u64| json!({ "kind": "download_artifact", "artifact_ref": r, "offset": offset });
    let first = s.transfer("hazel", &chunk(0), Some("conn_a")).unwrap();
    assert_eq!(first["eof"], false);
    assert_eq!(first["length"], 32);
    assert_eq!(first["reader_idle_timeout_ms"], 300_000);
    let far = s.clock() + 3_600_000;
    s.with_db(|db| {
        for i in 1..MAX_READERS_FOR_TEST {
            db.execute(
                "INSERT INTO artifact_readers(principal, connection_id, artifact_ref, expires_ms) VALUES ('hazel', ?1, ?2, ?3)",
                rusqlite::params![format!("conn_{i}"), r, far],
            )?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(code(s.transfer("hazel", &chunk(0), Some("conn_new"))), "BUDGET_EXCEEDED");
    // An existing pin may be refreshed, and the final chunk needs no pin.
    assert!(s.transfer("hazel", &chunk(32), Some("conn_a")).is_ok());
    assert_eq!(s.transfer("hazel", &chunk(96), Some("conn_new")).unwrap()["eof"], true);
    // Pins retain bytes, never authority.
    assert_eq!(code(s.transfer("vesper", &chunk(0), Some("conn_a"))), "PERMISSION_DENIED");
    s.transfer("hazel", &json!({ "kind": "end_transfer" }), Some("conn_a")).unwrap();
    assert!(s.transfer("hazel", &chunk(0), Some("conn_new2")).is_ok());
}

const MAX_READERS_FOR_TEST: i64 = artifacts::MAX_READERS;

fn aged_service(t: &TempDir) -> (StorageService, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(crate::ids::now_millis()));
    let mut o = options(&t.0);
    let c2 = clock.clone();
    o.clock = Some(Arc::new(move || c2.load(std::sync::atomic::Ordering::SeqCst)));
    (StorageService::open(o).unwrap(), clock)
}

fn advance_days(clock: &AtomicI64, days: i64) {
    clock.fetch_add(days * 24 * 3600 * 1000, std::sync::atomic::Ordering::SeqCst);
}

fn bytes_present(t: &TempDir, artifact: &Value) -> bool {
    t.0.join("data/artifacts").join(format!("{}.bin", artifact["artifact_ref"].as_str().unwrap())).exists()
}

#[test]
fn expiry_never_removes_uncollected_bytes() {
    let t = TempDir::new();
    let (s, clock) = aged_service(&t);
    let journal = Arc::new(FakeJournal::default());
    journal.task("task_1", "completed", vec![]);
    s.set_authority_policy(journal.clone());
    let c = ctx("task_1", "hazel");
    let artifact = publish(&s, &c, "out.txt", "result");
    advance_days(&clock, 400);
    s.capabilities().unwrap();
    s.transfer("hazel", &json!({ "kind": "end_transfer" }), None).unwrap();
    assert!(bytes_present(&t, &artifact));
    let listed = s.list_published_artifacts(20.0, None).unwrap();
    assert_eq!(listed["items"][0]["delivery"], "available");
    assert_eq!(listed["items"][0]["bytes_available"], true);
}

#[test]
fn expiry_keeps_collected_bytes_until_the_journal_allows() {
    let t = TempDir::new();
    let (s, clock) = aged_service(&t);
    let c = ctx("task_1", "hazel");
    let artifact = publish(&s, &c, "out.txt", "result");
    let r = artifact["artifact_ref"].as_str().unwrap();
    let ack = json!({ "kind": "ack_collected", "artifact_ref": r, "size_bytes": 6, "sha256": sha(b"result") });
    assert_eq!(code(s.transfer("hazel", &json!({ "kind": "ack_collected", "artifact_ref": r, "size_bytes": 6, "sha256": sha(b"other") }), None)), "POSTCONDITION_FAILED");
    s.transfer("hazel", &ack, None).unwrap();
    advance_days(&clock, 31);

    // Without the journal view nothing expires.
    s.capabilities().unwrap();
    assert!(bytes_present(&t, &artifact));

    let journal = Arc::new(FakeJournal::default());
    let pending = json!({ "id": "dlv_1", "destination": "artifact_collection", "path": "out.txt", "host_id": "host_vesper", "destination_path": "/home/riley/out.txt", "revision": 1, "artifact_ref": r, "sha256": sha(b"result"), "size_bytes": 6 });
    // A task that is still active keeps the bytes.
    journal.task("task_1", "active", vec![]);
    s.set_authority_policy(journal.clone());
    assert!(bytes_present(&t, &artifact));
    // A terminal task with a required delivery not yet verified keeps them.
    journal.task("task_1", "completed", vec![pending.clone()]);
    s.capabilities().unwrap();
    assert!(bytes_present(&t, &artifact));
    // An unresolved operation keeps them, and so does a journal error.
    journal.task("task_1", "completed", vec![]);
    journal.unresolved.lock().insert("task_1".into());
    s.capabilities().unwrap();
    assert!(bytes_present(&t, &artifact));
    journal.unresolved.lock().clear();
    *journal.broken.lock() = true;
    s.capabilities().unwrap();
    assert!(bytes_present(&t, &artifact));
    // A reader pin (taken while the journal still said "keep") keeps them
    // while the reader is active.
    s.transfer("hazel", &json!({ "kind": "download_artifact", "artifact_ref": r, "max_bytes": 1 }), Some("conn_a")).unwrap();
    *journal.broken.lock() = false;
    s.capabilities().unwrap();
    assert!(bytes_present(&t, &artifact));
    s.transfer("hazel", &json!({ "kind": "end_transfer" }), Some("conn_a")).unwrap();
    assert!(!bytes_present(&t, &artifact));
    let stat = s.transfer("hazel", &json!({ "kind": "stat_artifact", "artifact_ref": r }), None).unwrap();
    assert_eq!(stat["delivery"], "unavailable");
    assert_eq!(stat["ready"], false);
    assert_eq!(code(s.transfer("hazel", &json!({ "kind": "download_artifact", "artifact_ref": r }), None)), "DELIVERY_UNAVAILABLE");
}

#[test]
fn send_records_an_obligation_verified_only_by_a_matching_receipt() {
    let t = TempDir::new();
    let s = open(&t.0);
    let journal = Arc::new(FakeJournal::default());
    journal.task("task_1", "active", vec![]);
    journal.collectors.lock().insert("hazel".into(), "host_vesper".into());
    s.set_authority_policy(journal.clone());
    let c = ctx("task_1", "hazel");
    write(&s, &c, "report.txt", "report");
    for dest in ["relative/path", "/home/riley/../etc/passwd", "/home//riley/x"] {
        let send = json!({ "kind": "send", "path": "report.txt", "to": { "host": "host_vesper", "path": dest } });
        assert_eq!(code(s.files(&send, &c)), "INVALID_ARGUMENT", "{dest}");
    }
    let send = json!({ "kind": "send", "path": "report.txt", "to": { "host": "host_vesper", "path": "/home/riley/Downloads/report.txt" } });
    let records = s.files(&send, &c).unwrap().records;
    let obligation = records[1]["obligation"].clone();
    assert_eq!(obligation["destination"], "artifact_collection");
    assert_eq!(obligation["revision"], 1);
    assert!(!s.delivery_verified("task_1", &obligation));
    journal.task("task_1", "active", vec![obligation.clone()]);
    let r = obligation["artifact_ref"].as_str().unwrap();
    let ack = |dest: &str, rev: i64| {
        json!({ "kind": "ack_collected", "artifact_ref": r, "size_bytes": 6, "sha256": sha(b"report"), "obligation_id": obligation["id"], "obligation_revision": rev, "destination_path": dest })
    };
    assert_eq!(code(s.transfer("hazel", &ack("/home/riley/Downloads/report.txt", 2), None)), "POSTCONDITION_FAILED");
    assert_eq!(code(s.transfer("hazel", &ack("tmp/report.txt", 1), None)), "POSTCONDITION_FAILED");
    // Collected somewhere else: a receipt, but not this delivery.
    s.transfer("hazel", &ack("/tmp/report.txt", 1), None).unwrap();
    assert!(!s.delivery_verified("task_1", &obligation));
    s.transfer("hazel", &ack("/home/riley/Downloads/report.txt", 1), None).unwrap();
    assert!(s.delivery_verified("task_1", &obligation));
    let stat = s.transfer("hazel", &json!({ "kind": "stat_artifact", "artifact_ref": r }), None).unwrap();
    assert_eq!(stat["delivery_obligation"]["obligation_revision"], 1);
}

#[test]
fn publish_links_the_recorded_writer_only_for_the_same_bytes() {
    let t = TempDir::new();
    let s = open(&t.0);
    let mut c = ctx("task_1", "hazel");
    c.operation_ref = Some("op_write".into());
    write(&s, &c, "notes/out.txt", "v1");
    c.operation_ref = None;
    let linked = s.files(&json!({ "kind": "publish", "path": "notes//./out.txt" }), &c).unwrap().records.remove(0);
    assert_eq!(linked["producer_ref"], "op_write");
    assert_eq!(linked["evidence_refs"], json!(["op_write"]));
    // Changed by something ibara did not record: no author is claimed.
    std::fs::write(s.workspace("task_1", false).unwrap().join("notes/out.txt"), "v2").unwrap();
    let unlinked = s.files(&json!({ "kind": "publish", "path": "notes/out.txt" }), &c).unwrap().records.remove(0);
    assert!(unlinked["producer_ref"].as_str().unwrap().starts_with("producer_"));
    assert_eq!(unlinked["sha256"], sha(b"v2"));
}

// ---------------------------------------------------------------- procedures

#[test]
fn forbidden_refs_match_the_typescript_pattern() {
    for text in ["use el_12 now", "frame_ab", "the elementRef", "observation-ref", "OBS_REF", "an el-ref", "frameref"] {
        assert!(jsv::forbidden_ref(text), "{text}");
    }
    for text in ["label_12", "reframe_x", "elements refer", "obsref2", "el_", "frame_ ", "tel_12", "modelref"] {
        assert!(!jsv::forbidden_ref(text), "{text}");
    }
}

fn candidate() -> Value {
    json!({
        "title": "Save a note",
        "applicability": ["Mousepad is open"],
        "steps": ["Press ctrl+s", "Type the name", "Press enter", "Confirm"],
        "expected_outputs": ["note.txt exists"],
        "verification": ["file exists"],
        "stop_conditions": ["dialog errors"],
        "evidence_refs": ["check_1"],
    })
}

#[test]
fn candidate_validation_refuses_self_approval_and_element_refs() {
    let t = TempDir::new();
    let s = open(&t.0);
    let c = ctx("task_1", "hazel");
    let mut approved = candidate();
    approved["status"] = json!("approved");
    let mut element = candidate();
    element["steps"] = json!(["click el_42"]);
    let mut bad_ref = candidate();
    bad_ref["evidence_refs"] = json!(["not a ref"]);
    let mut late = candidate();
    late["expires_at"] = json!("next tuesday");
    for (name, cand) in [("approved", approved), ("element", element), ("bad_ref", bad_ref), ("late", late), ("array", json!([]))] {
        assert_eq!(code(s.procedures(&json!({ "kind": "propose", "candidate": cand }), &c)), "INVALID_ARGUMENT", "{name}");
    }
    let proposed = s.procedures(&json!({ "kind": "propose", "candidate": candidate() }), &c).unwrap().records.remove(0);
    assert_eq!(proposed["status"], "candidate");
    assert_eq!(proposed["applicability_state"], "not_approved");
    let approve = json!({ "kind": "approve", "procedure_ref": proposed["procedure_ref"], "expected_sha256": "0".repeat(64) });
    assert_eq!(code(s.procedures(&approve, &c)), "PERMISSION_DENIED");
    let op = ctx("task_1", "operator");
    assert_eq!(code(s.procedures(&approve, &op)), "REQUEST_CONFLICT");
}

#[test]
fn procedure_digest_keeps_stored_key_order() {
    let stored = r#"{"kind":"procedure","procedure_ref":"procedure_1","version":"1","status":"approved","definition":{"steps":["b"],"title":"T","applicability":["a"],"inputs":[],"expected_outputs":["o"],"verification":["v"],"stop_conditions":["s"],"evidence_refs":[],"prerequisites":[],"contradicts":[],"hard_prerequisites":[],"tested_versions":[]}}"#;
    let record = procedures::normalize_procedure_record(&serde_json::from_str(stored).unwrap());
    let definition = &stored[stored.find("{\"steps\"").unwrap()..stored.len() - 1];
    assert_eq!(procedures::procedure_digest(&record), sha(definition.as_bytes()));
    assert_eq!(serde_json::to_string(&record["definition"]).unwrap(), definition);
    // Normalizing again changes nothing, so reads do not rewrite the row.
    let again = procedures::normalize_procedure_record(&Value::Object(record.clone()));
    assert_eq!(serde_json::to_string(&again).unwrap(), serde_json::to_string(&record).unwrap());
}

// ---------------------------------------------------------------- migration

fn snapshot(db: &rusqlite::Connection) -> HashMap<String, Vec<Vec<Value>>> {
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut out = HashMap::new();
    for table in tables {
        let mut stmt = db.prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid")).unwrap();
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let rows = stmt
            .query_map([], |r| {
                Ok((0..columns.len())
                    .map(|i| {
                        if table == "jobs" && columns[i] == "updated_at" {
                            Value::Null
                        } else {
                            fsx::sql_to_json(r.get_ref(i).unwrap())
                        }
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        out.insert(table, rows);
    }
    out
}

#[test]
fn real_storage_databases_open_and_keep_their_rows() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/private");
    let mut opened = 0;
    let mut machines: Vec<String> =
        std::fs::read_dir(&fixtures).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    machines.sort();
    for machine in machines {
        let source = fixtures.join(&machine).join("storage.sqlite");
        if !source.exists() {
            eprintln!("skipping {machine}: no fixture at {}", source.display());
            continue;
        }
        let t = TempDir::new();
        std::fs::create_dir_all(t.0.join("state")).unwrap();
        let copy = t.0.join("state/storage.sqlite");
        std::fs::copy(&source, &copy).unwrap();
        let before = snapshot(&rusqlite::Connection::open(&copy).unwrap());
        let s = open(&t.0);
        assert!(!s.has_active_jobs(None), "{machine}: recovered jobs are dead after a reboot or migration");
        s.close();
        let db = rusqlite::Connection::open(&copy).unwrap();
        let after = snapshot(&db);
        for (table, rows) in &before {
            assert_eq!(after.get(table), Some(rows), "{machine}: table {table} changed");
        }
        assert!(after.contains_key("file_writers"));
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);
        let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        opened += 1;
    }
    eprintln!("opened {opened} real storage databases");
}
