//! Files sent and fetched from the Files tab (`operator-file-send` and
//! `operator-file-receive`), end to end.
//!
//! The same world as `tests/pairing.rs`: real target daemons and real
//! consoles, a fake tailnet, a fake sshd that runs the real forced command.
//! Tulip1 keeps files up to 4 MiB, and its final check of a sent file
//! (`IBARA_TEST_PUBLISH_DELAY_MS`) takes 8 s, as a big file does on a slow disk.
//!
//! Failure cases this must catch:
//! 1. A send whose final check on the other computer takes longer than an
//!    ordinary request is reported as failed although the file arrived whole.
//! 2. The reply to that final check is lost, and the console calls the send
//!    partial or uncertain without asking the other computer whether it finished.
//! 3. A file over the other computer's limit is refused with a transport
//!    error, or with words that do not name the file's size and the limit, or
//!    only after its bytes went over.
//! 4. The same for a file fetched from the other computer: refused plainly,
//!    before anything is saved here.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange as JSON lines in DIR.

mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::*;

const MIB: usize = 1024 * 1024;
/// Tulip1's `max_artifact_bytes`.
const LIMIT: usize = 4 * MIB;
/// Longer than the other computer's relay allowed an ordinary request (6 s).
const PUBLISH_DELAY: Duration = Duration::from_secs(8);

fn private_dir(path: &Path) -> PathBuf {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    path.to_path_buf()
}

/// Bytes that differ from one file to the next.
fn body(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn send(console: &mut Console, computer: &str, epoch: &str, local: &Path, remote: &str) -> Value {
    let local = local.to_str().unwrap();
    console.ask("operator-file-send", &["--computer", computer, "--epoch", epoch, "--root", "transfers", "--remote", remote, "--local", local])
}

fn receive(console: &mut Console, computer: &str, epoch: &str, remote: &str, local: &Path) -> Value {
    let local = local.to_str().unwrap();
    console.ask("operator-file-receive", &["--computer", computer, "--epoch", epoch, "--root", "transfers", "--remote", remote, "--local", local])
}

/// Upload jobs Tulip1's storage holds in `state`.
fn jobs_in(target: &Target, state: &str) -> usize {
    let db = rusqlite::Connection::open_with_flags(target.root.join("state/storage.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.query_row("SELECT COUNT(*) FROM operator_file_jobs WHERE state = ?1", [state], |row| row.get::<_, i64>(0)).unwrap() as usize
}

/// Kill every forced command (`agent-entry`) serving `target`: the reply of a
/// request in flight is lost.
fn cut_routes(target: &Target) -> usize {
    let root = target.root.to_string_lossy().into_owned();
    let mut cut = 0;
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if cmdline.contains(" agent-entry ") && cmdline.contains(&root) {
            // SAFETY: signalling a process this test's world started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            cut += 1;
        }
    }
    cut
}

fn staged(target: &Target) -> Vec<String> {
    fs::read_dir(target.root.join("state/operator-upload"))
        .map(|dir| dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

#[test]
fn big_files_are_reported_as_they_are() {
    let world = World::new("files");
    let shared = private_dir(&world.root.join("tulip1-shared"));
    let policy = world.root.join("tulip1-policy.json");
    fs::write(&policy, json!({"principals": [], "operator_file_roots": {"transfers": shared}, "max_artifact_bytes": LIMIT}).to_string()).unwrap();
    let delay = PUBLISH_DELAY.as_millis().to_string();
    let tulip1 = Target::start_with(
        &world,
        node("tulip1"),
        Some("Tulip1"),
        300_000,
        &[("IBARA_POLICY", policy.as_os_str()), ("IBARA_TEST_PUBLISH_DELAY_MS", delay.as_ref())],
    );
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let started = vesper.ok("pair-start", &["tulip1"]);
    let paired = vesper.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    let id = paired["computer_id"].as_str().unwrap().to_string();
    let e = vesper.ok("operator-session", &["--computer", &id])["controller_epoch"].as_str().unwrap().to_string();
    let local = private_dir(&world.root.join("vesper-files"));

    // 1. Tulip1 takes 8 s to check the whole file before it names it: verified.
    let big = body(3 * MIB, 1);
    fs::write(local.join("big.bin"), &big).unwrap();
    let began = Instant::now();
    let sent = send(&mut vesper, &id, &e, &local.join("big.bin"), "big.bin");
    assert!(sent["error"].is_null(), "{sent}");
    assert_eq!((sent["data"]["state"].as_str(), sent["data"]["size_bytes"].as_u64()), (Some("verified"), Some(big.len() as u64)), "{sent}");
    assert!(began.elapsed() >= PUBLISH_DELAY, "the final check was slow");
    assert_eq!(fs::read(shared.join("big.bin")).unwrap(), big);

    // 2. The reply to that check is lost: the console asks Tulip1, which finished.
    let lost = body(2 * MIB, 2);
    fs::write(local.join("lost.bin"), &lost).unwrap();
    let cutter = std::thread::scope(|scope| {
        let cutter = scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while jobs_in(&tulip1, "publishing") == 0 {
                assert!(Instant::now() < deadline, "Tulip1 never began its final check");
                std::thread::sleep(Duration::from_millis(100));
            }
            cut_routes(&tulip1)
        });
        let sent = send(&mut vesper, &id, &e, &local.join("lost.bin"), "lost.bin");
        assert!(sent["error"].is_null(), "{sent}");
        assert_eq!(sent["data"]["state"], "verified", "{sent}");
        cutter.join().unwrap()
    });
    assert!(cutter >= 1, "a reply was in flight when the route was cut");
    assert_eq!(fs::read(shared.join("lost.bin")).unwrap(), lost);
    assert_eq!(jobs_in(&tulip1, "published"), 2);

    // What was sent comes back whole.
    let back = receive(&mut vesper, &id, &e, "big.bin", &local.join("big-back.bin"));
    assert!(back["error"].is_null(), "{back}");
    assert_eq!(fs::read(local.join("big-back.bin")).unwrap(), big);

    // 3. Over Tulip1's limit: refused with both sizes, before any byte goes over.
    fs::write(local.join("over.bin"), body(LIMIT + LIMIT / 2, 3)).unwrap();
    let refused = send(&mut vesper, &id, &e, &local.join("over.bin"), "over.bin");
    assert_eq!(refused["error"]["code"], "BUDGET_EXCEEDED", "{refused}");
    assert_eq!(refused["error"]["message"], "This file is 6.3 MB; ibara sends files up to 4.2 MB.", "{refused}");
    assert!(staged(&tulip1).is_empty(), "nothing staged on Tulip1: {:?}", staged(&tulip1));
    assert!(!shared.join("over.bin").exists());
    assert_eq!(jobs_in(&tulip1, "uploading"), 0);

    // 4. Fetching a file over the limit: refused the same way, nothing saved here.
    fs::write(shared.join("far.bin"), body(LIMIT + LIMIT / 2, 4)).unwrap();
    fs::set_permissions(shared.join("far.bin"), fs::Permissions::from_mode(0o600)).unwrap();
    let refused = receive(&mut vesper, &id, &e, "far.bin", &local.join("far.bin"));
    assert_eq!(refused["error"]["code"], "BUDGET_EXCEEDED", "{refused}");
    assert_eq!(refused["error"]["message"], "This file is 6.3 MB; ibara sends files up to 4.2 MB.", "{refused}");
    let left: Vec<String> = fs::read_dir(&local).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!left.iter().any(|name| name.starts_with("far") || name.starts_with(".ibara-")), "{left:?}");
}
