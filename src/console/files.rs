//! Human file transfers over the selected operator grant, never the agent-task
//! collector (ibara-bridge.mjs:208-331). Each call rebinds and must keep the
//! endpoint, binding revision and grant generation pinned by the first one.

use super::envelope::{Fault, Handled, clip};
use super::{Ctx, option, validated_id};
use crate::error::IbaraError;
use crate::operator::{current_uid, js, pattern};
use crate::storage::file_too_large;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

const CHUNK: usize = 256 * 1024;
/// This console's own ceiling, a round 500 MB; the target may name a lower one.
const MAX_FILE: u64 = 500_000_000;
const MAX_DOWNLOAD_CHUNK: usize = 512 * 1024;
const CALL_DEADLINE: Duration = Duration::from_secs(20);
/// A sent file's final check and a fetched file's digest read the whole file:
/// the session's long transport deadline (170 s) plus margin.
const WHOLE_FILE_DEADLINE: Duration = Duration::from_secs(180);
/// How often a send whose final check went unanswered asks the target about it.
const STATUS_POLL: Duration = Duration::from_secs(2);

/// The identity the first reply pinned, and the largest file both ends take.
struct Pinned {
    endpoint: Value,
    revision: Value,
    generation: Value,
    limit: u64,
}

struct Files<'a> {
    ctx: &'a Ctx,
    computer: String,
    epoch: String,
    pinned: Option<Pinned>,
}

/// A failed call, and whether the target's answer is unknown (a lost or late
/// reply, an unreachable route) rather than its refusal.
struct Failure {
    fault: Fault,
    unanswered: bool,
}

impl From<Failure> for Fault {
    fn from(failure: Failure) -> Fault {
        failure.fault
    }
}

impl From<Fault> for Failure {
    fn from(fault: Fault) -> Failure {
        Failure { fault, unanswered: false }
    }
}

fn refusal(error: IbaraError) -> Failure {
    // A size refusal is said as it is, without the target's code in front.
    if error.code == "BUDGET_EXCEEDED" {
        let message = error.message.strip_prefix("BUDGET_EXCEEDED: ").unwrap_or(&error.message);
        return Fault::Coded("BUDGET_EXCEEDED", clip(message, 200)).into();
    }
    let message = if error.message.is_empty() { "Selected file operation refused.".into() } else { clip(&error.message, 200) };
    Failure { fault: Fault::Plain(message), unanswered: error.code == "SESSION_UNAVAILABLE" }
}

impl Files<'_> {
    /// `humanFileCall(...)`: one selected operation whose reply must echo this
    /// computer, epoch, endpoint and grant generation.
    async fn call_raw(&self, op: &str, payload: Value) -> Result<Value, Failure> {
        let deadline = if crate::controller::SLOW_FILE_OPS.contains(&op) { WHOLE_FILE_DEADLINE } else { CALL_DEADLINE };
        let call = self.ctx.console.sessions.call(&self.computer, Some(&self.epoch), op, payload);
        let parsed = match tokio::time::timeout(deadline, call).await {
            Err(_) => return Err(Failure { fault: Fault::Timeout("Selected operator transport timed out.".into()), unanswered: true }),
            Ok(Err(error)) => return Err(refusal(error)),
            Ok(Ok(parsed)) => parsed,
        };
        let value = &parsed["result"];
        let same = parsed.get("computer_id").and_then(Value::as_str) == Some(self.computer.as_str())
            && parsed.get("controller_epoch").and_then(Value::as_str) == Some(self.epoch.as_str())
            && js::truthy(Some(value))
            && value.get("endpoint_id") == parsed.get("endpoint_id")
            && value.get("controller_epoch").and_then(Value::as_str) == Some(self.epoch.as_str())
            && value.get("authorization_generation") == parsed.get("expected_authorization_generation");
        if !same {
            return Err(Fault::plain("Selected file response identity changed.").into());
        }
        Ok(parsed)
    }

    /// `call(op, payload)`: the target's result, refused if the pinned destination moved.
    async fn call(&self, op: &str, payload: Value) -> Result<Value, Failure> {
        let mut response = self.call_raw(op, payload).await?;
        let pinned = self.pinned.as_ref().expect("pinned before transfer calls");
        if response.get("endpoint_id") != Some(&pinned.endpoint)
            || response.get("binding_revision") != Some(&pinned.revision)
            || response.get("expected_authorization_generation") != Some(&pinned.generation)
        {
            return Err(Fault::plain("Selected file destination changed during transfer.").into());
        }
        Ok(response["result"].take())
    }

    /// A refusal naming the file's size and the limit, when `size` is over it.
    fn fits(&self, size: u64) -> Result<(), Fault> {
        let limit = self.pinned.as_ref().expect("pinned").limit;
        if size > limit {
            return Err(Fault::Coded("BUDGET_EXCEEDED", file_too_large(size, limit)));
        }
        Ok(())
    }
}

/// A transfer receipt as the bridge returned it.
fn receipt(files: &Files<'_>, root: &str, remote: &str, extra: Value) -> Value {
    let pinned = files.pinned.as_ref().expect("pinned");
    let mut data = json!({
        "computer_id": files.computer, "endpoint_id": pinned.endpoint, "binding_revision": pinned.revision,
        "root_id": root, "remote_path": remote,
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        data[key] = value.clone();
    }
    data
}

/// SHA-256 of the first `length` bytes, read by offset in 256 KiB blocks, off the runtime.
async fn hash_prefix(file: &File, length: u64, short: &'static str) -> Result<String, Fault> {
    let file = file.try_clone()?;
    tokio::task::spawn_blocking(move || {
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; CHUNK];
        let mut position = 0u64;
        while position < length {
            let want = (CHUNK as u64).min(length - position) as usize;
            let count = file.read_at(&mut buffer[..want], position)?;
            if count == 0 {
                return Err(Fault::plain(short));
            }
            hash.update(&buffer[..count]);
            position += count as u64;
        }
        Ok(hex(&hash.finalize()))
    })
    .await
    .map_err(|e| Fault::Plain(e.to_string()))?
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The largest file this console sends to or takes from a target: the target's
/// own limit (`max_file_bytes`), held to this console's ceiling. An older target
/// names none (`None` or 0), and only the ceiling applies.
fn file_limit(named: Option<u64>) -> u64 {
    named.filter(|limit| *limit > 0).map_or(MAX_FILE, |limit| limit.min(MAX_FILE))
}

/// `operator-file-send|receive|resume --computer ID --epoch E --root R --remote PATH --local PATH [--job ID]`.
pub async fn human_files(ctx: &Ctx) -> Handled {
    let args = &ctx.args;
    let computer = validated_id(option(args, "--computer"), "computer_id")?;
    let epoch = validated_id(option(args, "--epoch"), "controller_epoch")?;
    let root = validated_id(option(args, "--root"), "root_id")?;
    let receive = ctx.head.command == "operator-file-receive";
    let resume_id = if ctx.head.command == "operator-file-resume" { Some(validated_id(option(args, "--job"), "job_id")?) } else { None };
    let local = option(args, "--local").unwrap_or("");
    let remote = option(args, "--remote").unwrap_or("");
    let unsafe_remote = remote.is_empty()
        || remote.starts_with('/')
        || remote.split('/').any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with('.'));
    if !local.starts_with('/') || local.contains('\0') || unsafe_remote {
        return Err(Fault::plain("Choose an absolute local path and a relative file under an approved remote root."));
    }
    let (folder, name) = match remote.rsplit_once('/') {
        Some((folder, name)) => (folder, name),
        None => (".", remote),
    };
    let mut files = Files { ctx, computer, epoch, pinned: None };
    let first = files.call_raw("files_roots", json!({})).await?;
    let approved = first["result"]["roots"].as_array().is_some_and(|roots| roots.iter().any(|r| r["root_id"] == json!(root)));
    if !approved {
        return Err(Fault::plain("Remote root is no longer approved."));
    }
    files.pinned = Some(Pinned {
        endpoint: first["endpoint_id"].clone(),
        revision: first["binding_revision"].clone(),
        generation: first["expected_authorization_generation"].clone(),
        limit: file_limit(first["result"]["max_file_bytes"].as_u64()),
    });
    if receive {
        receive_file(ctx, &files, &root, remote, local).await
    } else {
        send_file(ctx, &files, &root, remote, folder, name, local, resume_id).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_file(
    ctx: &Ctx,
    files: &Files<'_>,
    root: &str,
    remote: &str,
    folder: &str,
    name: &str,
    local: &str,
    resume_id: Option<String>,
) -> Handled {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(local)?;
    let before = file.metadata()?;
    let size = before.len();
    if !before.is_file() || before.nlink() != 1 {
        return Err(Fault::plain("Choose one regular local file."));
    }
    if size < 1 {
        return Err(Fault::plain("This file is empty; choose one with something in it."));
    }
    files.fits(size)?;
    let sha256 = hash_prefix(&file, size, "Source changed.").await?;
    let job = resume_id.clone().unwrap_or_else(|| format!("operator_file_{}", uuid::Uuid::new_v4().hyphenated()));
    let verified = |files: &Files<'_>| {
        ctx.ready(receipt(files, root, remote, json!({"job_id": job, "state": "verified", "size_bytes": size, "sha256": sha256})))
    };
    let mut offset = 0u64;
    if resume_id.is_some() {
        let status = files.call("files_status", json!({"job_id": job})).await?;
        let same = status["root_id"] == json!(root)
            && status["relative_path"] == json!(remote)
            && js::same_number(status.get("size_bytes"), size as f64)
            && status["sha256"] == json!(sha256);
        if !same {
            return Err(Fault::plain("Retained upload belongs to a different source or destination."));
        }
        let status = if status["state"] == json!("publishing") {
            // The final check may still be running: wait for it before calling it uncertain.
            published_record(files, &job, Instant::now() + publish_patience(size)).await?.unwrap_or(status)
        } else {
            status
        };
        if status["state"] == json!("published") {
            return Ok(verified(files));
        }
        let retained = js::safe_integer(status.get("offset")).filter(|o| *o >= 0 && (*o as u64) <= size);
        let Some(retained) = retained.filter(|_| status["state"] == json!("uploading")) else {
            return Err(Fault::plain("Retained publication is uncertain. Inspect the target; do not repeat the upload."));
        };
        let prefix = hash_prefix(&file, retained as u64, "Source prefix changed.").await?;
        let resumed = files
            .call(
                "files_resume_upload",
                json!({"job_id": job, "root_id": root, "relative_path": remote, "size": size, "sha256": sha256, "prefix_sha256": prefix}),
            )
            .await?;
        if resumed["state"] != json!("uploading") || !js::same_number(resumed.get("offset"), retained as f64) {
            return Err(Fault::plain("Retained prefix did not verify."));
        }
        offset = retained as u64;
    } else {
        let start = files
            .call(
                "files_begin_upload",
                json!({"root_id": root, "relative_directory": folder, "name": name, "size": size, "sha256": sha256, "job_id": job}),
            )
            .await?;
        if start["job_id"] != json!(job) || !js::same_number(start.get("offset"), 0.0) {
            return Err(Fault::plain("Target upload job identity changed."));
        }
    }
    let upload = async {
        let mut buffer = vec![0u8; CHUNK];
        while offset < size {
            let want = (CHUNK as u64).min(size - offset) as usize;
            let count = file.read_at(&mut buffer[..want], offset)?;
            if count == 0 {
                return Err(Fault::plain("Source changed during upload."));
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&buffer[..count]);
            let next = files.call("files_upload_chunk", json!({"job_id": job, "offset": offset, "base64": data})).await?;
            offset += count as u64;
            if !js::same_number(next.get("offset"), offset as f64) {
                return Err(Fault::plain("Target upload progress differs from sent bytes."));
            }
        }
        let after = file.metadata()?;
        let unchanged = after.dev() == before.dev()
            && after.ino() == before.ino()
            && after.len() == size
            && (after.mtime(), after.mtime_nsec()) == (before.mtime(), before.mtime_nsec())
            && (after.ctime(), after.ctime_nsec()) == (before.ctime(), before.ctime_nsec());
        if !unchanged {
            return Err(Fault::plain("Source changed during upload; retained target job needs explicit inspection."));
        }
        let result = publish(files, &job, size).await?;
        let published = result["state"] == json!("published")
            && js::same_number(result.get("size_bytes"), size as f64)
            && result["sha256"] == json!(sha256)
            && result["relative_path"] == json!(remote);
        if !published {
            return Err(Fault::plain("Published receipt did not match chosen file."));
        }
        Ok(())
    };
    match upload.await {
        Ok(()) => Ok(verified(files)),
        Err(fault) => {
            let message = match fault {
                Fault::Coded(_, m) | Fault::Timeout(m) | Fault::Plain(m) => m,
            };
            Err(Fault::Plain(format!(
                "Upload {job} is partial or uncertain. Inspect its status; resume explicitly only after verifying original source and destination. {message}"
            )))
        }
    }
}

/// How long a sent file's final check may take before the send is called
/// uncertain: the target reads the file three times and writes it once. A
/// minute, and a second per MiB (250 MB: about five minutes).
fn publish_patience(size: u64) -> Duration {
    Duration::from_secs(60 + size / (1024 * 1024))
}

/// `files_publish`; when its reply is lost or late, the target's own record of
/// the job once its final check ends. A refusal stays a refusal.
async fn publish(files: &Files<'_>, job: &str, size: u64) -> Result<Value, Failure> {
    let until = Instant::now() + publish_patience(size);
    match files.call("files_publish", json!({"job_id": job})).await {
        Err(failure) if failure.unanswered => published_record(files, job, until).await?.ok_or(failure),
        answered => answered,
    }
}

/// Ask the target about `job` until its final check ends or `until`: the
/// record once published; `None` when it is neither published nor still being
/// checked, or the target did not answer in time.
async fn published_record(files: &Files<'_>, job: &str, until: Instant) -> Result<Option<Value>, Failure> {
    loop {
        match files.call("files_status", json!({"job_id": job})).await {
            Ok(status) if status["state"] == json!("published") => return Ok(Some(status)),
            Ok(status) if status["state"] != json!("publishing") => return Ok(None),
            // Still checking, or too busy checking to answer.
            Ok(_) => {}
            Err(failure) if failure.unanswered => {}
            Err(failure) => return Err(failure),
        }
        if Instant::now() + STATUS_POLL >= until {
            return Ok(None);
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

fn open_directory(path: &str) -> Result<File, Fault> {
    Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?)
}

/// `localDestinationParent(directory)`: walk from `/` by held descriptors, refusing an
/// ancestor that is not root's or ours or is writable by others (sticky excepted), and
/// a destination folder that is not ours or is group/other writable.
fn local_destination_parent(directory: &str) -> Result<File, Fault> {
    if !directory.starts_with('/') {
        return Err(Fault::plain("Safe local publication requires a Linux absolute path."));
    }
    let uid = current_uid();
    let mut dir = open_directory("/")?;
    for part in directory.split('/').filter(|p| !p.is_empty()) {
        let meta = dir.metadata()?;
        let writable = meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0;
        if (meta.uid() != 0 && meta.uid() != uid) || writable {
            return Err(Fault::plain("Unsafe local destination ancestor."));
        }
        dir = open_directory(&format!("/proc/self/fd/{}/{part}", dir.as_raw_fd()))?;
    }
    let meta = dir.metadata()?;
    if meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(Fault::plain("Local destination folder must be owned by you and not writable by others."));
    }
    Ok(dir)
}

async fn receive_file(ctx: &Ctx, files: &Files<'_>, root: &str, remote: &str, local: &str) -> Handled {
    let info = files.call("files_begin_download", json!({"root_id": root, "relative_path": remote})).await?;
    let size = js::safe_integer(info.get("size_bytes")).filter(|s| *s >= 0);
    let digest = js::string_or(info.get("sha256").filter(|s| js::truthy(Some(s))), "");
    let Some(size) = size.filter(|_| pattern::lower_hex(&digest, 64)) else {
        return Err(Fault::plain("Invalid remote file identity."));
    };
    files.fits(size as u64)?;
    let size = size as u64;
    let path = Path::new(local);
    let directory = path.parent().map(|p| p.display().to_string()).unwrap_or_else(|| "/".into());
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = local_destination_parent(&directory)?;
    let at = |name: &str| format!("/proc/self/fd/{}/{name}", parent.as_raw_fd());
    let temp = at(&format!(".ibara-{}.part", uuid::Uuid::new_v4().hyphenated()));
    let outcome = async {
        if Path::new(&at(&base)).exists() {
            return Err(Fault::plain("Local destination already exists."));
        }
        let out = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temp)?;
        let mut hash = Sha256::new();
        let mut offset = 0u64;
        while offset < size {
            let chunk = files.call("files_download_chunk", json!({"job_id": info["job_id"], "offset": offset})).await?;
            let text = js::string_or(chunk.get("base64").filter(|b| js::truthy(Some(b))), "");
            let raw = base64::engine::general_purpose::STANDARD.decode(&text).unwrap_or_default();
            let end = offset + raw.len() as u64;
            let sound = !raw.is_empty()
                && raw.len() <= MAX_DOWNLOAD_CHUNK
                && base64::engine::general_purpose::STANDARD.encode(&raw) == text
                && js::same_number(chunk.get("offset"), end as f64)
                && end <= size;
            if !sound {
                return Err(Fault::plain("Download chunk changed or made no progress."));
            }
            out.write_all_at(&raw, offset)?;
            hash.update(&raw);
            offset = end;
        }
        if hex(&hash.finalize()) != digest {
            return Err(Fault::plain("Remote bytes failed the signed file digest."));
        }
        out.sync_all()?;
        std::fs::hard_link(&temp, at(&base))?;
        parent.sync_all()?;
        Ok(ctx.ready(receipt(
            files,
            root,
            remote,
            json!({
                "local_path": local, "job_id": info["job_id"], "state": "verified-collected",
                "size_bytes": info["size_bytes"], "sha256": info["sha256"],
            }),
        )))
    }
    .await;
    let _ = std::fs::remove_file(&temp);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    /// A target names its own limit, and this console caps it at a round 500 MB. Failure cases:
    /// an older target naming none gets no cap; a target naming more than 500 MB is let past it;
    /// a target naming less is raised to 500 MB; the refusal names the cap as anything but 500 MB.
    #[test]
    fn a_send_is_held_to_the_lower_of_the_two_limits() {
        let refusal = |named: Option<u64>| file_too_large(600_000_000, file_limit(named));
        assert_eq!(refusal(None), "This file is 600 MB; ibara sends files up to 500 MB.");
        assert_eq!(refusal(Some(0)), "This file is 600 MB; ibara sends files up to 500 MB.");
        assert_eq!(refusal(Some(1_000_000_000)), "This file is 600 MB; ibara sends files up to 500 MB.");
        assert_eq!(refusal(Some(250_000_000)), "This file is 600 MB; ibara sends files up to 250 MB.");
        assert_eq!(file_limit(Some(500_000_001)), 500_000_000);
    }

    #[test]
    fn a_destination_writable_by_others_is_refused() {
        let dir = std::env::temp_dir().join(format!("ibara-console-files-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let path = dir.display().to_string();
        // /tmp is root's and sticky, so the walk reaches the folder itself.
        assert!(local_destination_parent(&path).is_ok());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o722)).unwrap();
        assert!(matches!(local_destination_parent(&path), Err(Fault::Plain(m)) if m.starts_with("Local destination folder")));
        let inner = dir.join("inner");
        std::fs::DirBuilder::new().mode(0o700).create(&inner).unwrap();
        assert!(matches!(local_destination_parent(&inner.display().to_string()), Err(Fault::Plain(m)) if m == "Unsafe local destination ancestor."));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(local_destination_parent("relative/dir"), Err(Fault::Plain(_))));
    }
}
