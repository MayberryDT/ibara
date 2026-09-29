//! `ibara client …` (replaces `agent/ibara-client` + `agent/ibara-client.mjs`).
//!
//! Directory commands (`--list-computers`, `--rename-computer`, `--import-legacy`),
//! and the `transfer-v1` line protocol: `stat`, `fetch` (safe, resumable collection
//! into a private destination) and `upload`, over the selected route
//! (`--computer`), the legacy station route, or the administrator control route
//! (`--operator`, `computerctl transfer-session`).

use super::directory::{OperatorDirectory, directory_path};
use super::transport::{self, RouteKind};
use super::{LineRead, current_uid, exit_with, expand_home, fail, home_dir, js, pattern, read_line_bounded, resolve_path, runtime};
use crate::error::Result;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Transfer chunk size (`CHUNK_BYTES`).
pub const CHUNK_BYTES: usize = 1_048_576;
/// Longest transfer reply line: a 1 MiB chunk is ~1.4 MB of base64.
const REPLY_LINE_LIMIT: usize = 8 * 1024 * 1024;
/// Collection sidecars are small JSON documents.
const METADATA_LIMIT: u64 = 16384;

const TRANSFER_USAGE: &str = "Usage: ibara-client [--directory-db FILE] --computer NAME fetch REF DEST | stat REF";
const RENAME_USAGE: &str = "Usage: ibara-client [--directory-db FILE] --rename-computer NAME LABEL";

// ---------------------------------------------------------------------------
// The transfer-v1 line protocol.

/// One transfer session (`class Transfer`, ibara-client.mjs:159-168).
pub struct Transfer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

/// Which route a transfer takes.
pub enum TransferRoute<'a> {
    Selected(&'a super::directory::SelectedEnvelope),
    /// `ibara-control transfer-session`.
    Operator,
    Legacy,
}

impl Transfer {
    pub fn open(route: TransferRoute<'_>) -> Result<Self> {
        let command = match route {
            TransferRoute::Selected(envelope) => transport::selected_command(RouteKind::Transfer, envelope)?,
            TransferRoute::Operator => super::control::transfer_session_command()?,
            TransferRoute::Legacy => {
                let node = transport::legacy_station_node(&transport::station_descriptor_path())?;
                let mut command = std::process::Command::new(transport::SSH);
                command.args(transport::legacy_transfer_ssh_args(&home_dir(), &node));
                command
            }
        };
        let mut command = tokio::process::Command::from(command);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Transfer { child, stdin, stdout })
    }

    /// Send one request line and read one reply line: `response.result ?? response`,
    /// failing on `response.error ?? result.error`.
    pub async fn call(&mut self, request: Value) -> Result<Value> {
        let mut line = request.to_string();
        line.push('\n');
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(line.as_bytes()).await;
        }
        let line = match read_line_bounded(&mut self.stdout, REPLY_LINE_LIMIT, true).await {
            Ok(LineRead::Line(line)) => line,
            Ok(LineRead::Oversize) => return Err(fail("Transfer response exceeds bound.")),
            Ok(LineRead::Eof) | Err(_) => return Err(fail("Incomplete transfer response")),
        };
        let mut response = super::parse_json(&String::from_utf8_lossy(&line))?;
        let result = match response.get_mut("result") {
            Some(result) if !result.is_null() => result.take(),
            _ => response.clone(),
        };
        let error = match response.get("error") {
            Some(error) if !error.is_null() => Some(error),
            _ => result.get("error"),
        };
        if js::truthy(error) {
            let error = error.cloned().unwrap_or(Value::Null);
            let code = error.get("code").filter(|c| js::truthy(Some(c))).map(js::string).unwrap_or_else(|| "Transfer failed".into());
            let message = error.get("message").filter(|m| js::truthy(Some(m))).map(js::string).unwrap_or_default();
            return Err(fail(format!("{code}: {message}")));
        }
        Ok(result)
    }

    /// End stdin; wait up to 10 s for the transport, then SIGTERM it.
    pub async fn close(mut self) {
        drop(self.stdin.take());
        if tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await.is_err() {
            transport::kill_pid(self.child.id().unwrap_or(0), libc::SIGTERM);
            let _ = self.child.wait().await;
        }
    }
}

/// Where a collection's requests go: a transfer-v1 session, or a person's
/// operator route to one computer (its `artifact_transfer` operation).
pub trait TransferLink: Send {
    /// One transfer request and its result (`response.result ?? response`).
    fn request(&mut self, request: Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>>;
}

impl TransferLink for Transfer {
    fn request(&mut self, request: Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(self.call(request))
    }
}

// ---------------------------------------------------------------------------
// Safe collection (ibara-client.mjs:24-157). Linux procfs anchors every operation
// to held directory descriptors even if an ancestor is renamed.

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn absent(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// SHA-256 of a whole file read by offset in 1 MiB blocks.
fn hash_file(file: &File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut offset = 0u64;
    loop {
        let count = file.read_at(&mut buffer, offset)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn write_all_at(file: &File, data: &[u8], offset: u64) -> Result<()> {
    let mut written = 0;
    while written < data.len() {
        let count = file.write_at(&data[written..], offset + written as u64)?;
        if count == 0 {
            return Err(fail("Destination write made no progress."));
        }
        written += count;
    }
    Ok(())
}

/// `privateFile(fd, entry, expectedLinks)`.
fn private_file(file: &File, entry: &Path, expected_links: u64) -> Result<Metadata> {
    let stat = file.metadata()?;
    let entry_stat = fs::symlink_metadata(entry);
    if !stat.is_file()
        || stat.uid() != current_uid()
        || stat.nlink() != expected_links
        || stat.mode() & 0o077 != 0
        || !entry_stat.is_ok_and(|e| same_file(&stat, &e))
    {
        return Err(fail("Partial destination must remain a regular private file with one link."));
    }
    Ok(stat)
}

fn open_read(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?)
}

fn create_private(path: &Path, read: bool) -> Result<File> {
    Ok(OpenOptions::new().read(read).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?)
}

/// The destination directory chain, held open (`destination(target)`).
struct Destination {
    dirs: Vec<(File, Metadata, PathBuf)>,
}

impl Destination {
    fn open(target: &Path) -> Result<Self> {
        let parent = target.parent().unwrap_or(Path::new("/"));
        let mut dirs: Vec<(File, Metadata, PathBuf)> = Vec::new();
        let components = std::iter::once(std::ffi::OsStr::new("/")).chain(parent.iter().skip(1));
        for component in components {
            let location = match dirs.last() {
                None => PathBuf::from("/"),
                Some((fd, _, _)) => PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd())).join(component),
            };
            match fs::DirBuilder::new().mode(0o700).create(&location) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let fd = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(&location)?;
            let stat = fd.metadata()?;
            let owner_ok = stat.uid() == 0 || stat.uid() == current_uid();
            if !owner_ok || (stat.mode() & 0o022 != 0 && stat.mode() & 0o1000 == 0) {
                return Err(fail("Destination has an unsafe parent directory."));
            }
            dirs.push((fd, stat, location));
        }
        let parent = &dirs.last().expect("root is always held").1;
        if parent.uid() != current_uid() || parent.mode() & 0o022 != 0 {
            return Err(fail("Destination directory must be owned by you and not writable by others."));
        }
        let destination = Destination { dirs };
        destination.verify()?;
        Ok(destination)
    }

    fn parent(&self) -> &File {
        &self.dirs.last().expect("root is always held").0
    }

    /// A name inside the held parent directory.
    fn entry(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/{name}", self.parent().as_raw_fd()))
    }

    fn verify(&self) -> Result<()> {
        for (_, stat, location) in &self.dirs {
            let current = fs::symlink_metadata(location)?;
            if !same_file(stat, &current) || current.uid() != stat.uid() || current.mode() != stat.mode() {
                return Err(fail("Destination parent was replaced or permissions changed."));
            }
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(self.parent().sync_all()?)
    }

    /// `linkat(AT_FDCWD, "/proc/self/fd/<file>", parent, name, AT_SYMLINK_FOLLOW)`: links the
    /// held file descriptor, never a replaceable pathname; creation is exclusive.
    fn link(&self, file: &File, name: &str) -> Result<()> {
        let source = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|e| fail(e.to_string()))?;
        let target = CString::new(name).map_err(|e| fail(e.to_string()))?;
        // SAFETY: both paths are valid NUL-terminated strings and both descriptors are open.
        let status = unsafe {
            libc::linkat(libc::AT_FDCWD, source.as_ptr(), self.parent().as_raw_fd(), target.as_ptr(), libc::AT_SYMLINK_FOLLOW)
        };
        if status != 0 {
            return Err(fail(format!("Artifact publication failed: {}", std::io::Error::last_os_error())));
        }
        Ok(())
    }
}

fn read_metadata(file: &File, entry: &Path, too_large: &str) -> Result<String> {
    let stat = private_file(file, entry, 1)?;
    if stat.size() > METADATA_LIMIT {
        return Err(fail(too_large.to_string()));
    }
    let mut data = vec![0u8; stat.size() as usize];
    let mut read = 0;
    while read < data.len() {
        let n = file.read_at(&mut data[read..], read as u64)?;
        if n == 0 {
            break;
        }
        read += n;
    }
    if read != data.len() {
        return Err(fail("Incomplete collection metadata."));
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Artifact identity reported by `stat_artifact`.
struct ArtifactInfo {
    size: u64,
    size_value: Value,
    sha256: String,
    obligation: Option<Value>,
}

/// `collect(tx, artifactRef, target)`: resumable, verified collection into `target`.
pub async fn collect(tx: &mut dyn TransferLink, artifact_ref: &str, target: &Path) -> Result<Value> {
    let info = tx.request(json!({"kind": "stat_artifact", "artifact_ref": artifact_ref})).await?;
    let size = js::safe_integer(info.get("size_bytes")).filter(|s| *s >= 0);
    let sha256 = js::string_or(info.get("sha256"), "undefined");
    let (Some(size), true) = (size, pattern::lower_hex(&sha256, 64)) else {
        return Err(fail("Invalid artifact metadata."));
    };
    let info = ArtifactInfo {
        size: size as u64,
        size_value: info["size_bytes"].clone(),
        sha256,
        obligation: info.get("delivery_obligation").filter(|o| !o.is_null()).cloned(),
    };
    let target_text = target.to_str().ok_or_else(|| fail("Destination path must be UTF-8."))?.to_string();
    let dir = Destination::open(target)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Collection { tx, artifact_ref, target: target_text, info, dir, name }.run().await
}

struct Collection<'a> {
    tx: &'a mut dyn TransferLink,
    artifact_ref: &'a str,
    target: String,
    info: ArtifactInfo,
    dir: Destination,
    name: String,
}

impl Collection<'_> {
    fn identity(&self) -> Value {
        json!({
            "version": 1,
            "artifact_ref": self.artifact_ref,
            "size_bytes": self.info.size_value,
            "sha256": self.info.sha256,
            "destination_path": self.target,
            "delivery_obligation": self.info.obligation.clone().unwrap_or(Value::Null),
        })
    }

    async fn acknowledge(&mut self, file: &File, published: &File) -> Result<Value> {
        let (final_path, published_path) = (self.dir.entry(&self.name), self.dir.entry(&format!("{}.ibara-published.json", self.name)));
        self.dir.verify()?;
        private_file(file, &final_path, 1)?;
        private_file(published, &published_path, 1)?;
        let mut ack = serde_json::Map::new();
        ack.insert("kind".into(), json!("ack_collected"));
        ack.insert("artifact_ref".into(), json!(self.artifact_ref));
        ack.insert("size_bytes".into(), self.info.size_value.clone());
        ack.insert("sha256".into(), json!(self.info.sha256));
        ack.insert("destination_path".into(), json!(self.target));
        if let Some(obligation) = self.info.obligation.as_ref().filter(|o| js::truthy(Some(o))) {
            if let Some(id) = obligation.get("obligation_id") {
                ack.insert("obligation_id".into(), id.clone());
            }
            if let Some(revision) = obligation.get("revision") {
                ack.insert("obligation_revision".into(), revision.clone());
            }
        }
        let receipt = self.tx.request(Value::Object(ack)).await?;
        self.dir.verify()?;
        private_file(published, &published_path, 1)?;
        fs::remove_file(&published_path)?;
        self.dir.sync()?;
        let mut out = serde_json::Map::new();
        out.insert("local_path".into(), json!(self.target));
        out.insert("size_bytes".into(), self.info.size_value.clone());
        out.insert("sha256".into(), json!(self.info.sha256));
        if let Some(delivery) = receipt.get("delivery") {
            out.insert("delivery".into(), delivery.clone());
        }
        Ok(Value::Object(out))
    }

    async fn run(mut self) -> Result<Value> {
        let final_path = self.dir.entry(&self.name);
        let partial = self.dir.entry(&format!("{}.ibara-part", self.name));
        let metadata = self.dir.entry(&format!("{}.ibara-part.json", self.name));
        let published = self.dir.entry(&format!("{}.ibara-published.json", self.name));
        let identity = self.identity();
        let identity_text = identity.to_string();

        if !absent(&final_path)? {
            // A previous run published the file; finish its cleanup and acknowledgment.
            if absent(&published)? {
                return Err(fail("Destination already exists; choose a new file."));
            }
            let published_fd = open_read(&published)?;
            let retained = super::parse_json(&read_metadata(&published_fd, &published, "Invalid collection metadata.")?)?;
            if retained.get("identity").map(Value::to_string).as_deref() != Some(identity_text.as_str()) {
                return Err(fail("Published file belongs to a different artifact or delivery obligation."));
            }
            let file = open_read(&final_path)?;
            let has_partial = !absent(&partial)?;
            let stat = private_file(&file, &final_path, if has_partial { 2 } else { 1 })?;
            if !js::same_number(retained.get("dev"), stat.dev() as f64)
                || !js::same_number(retained.get("ino"), stat.ino() as f64)
                || stat.size() != self.info.size
                || hash_file(&file)? != self.info.sha256
            {
                return Err(fail("Published file identity or checksum changed."));
            }
            self.dir.verify()?;
            if has_partial {
                if !fs::symlink_metadata(&partial).is_ok_and(|p| same_file(&stat, &p)) {
                    return Err(fail("Partial publication identity changed."));
                }
                fs::remove_file(&partial)?;
            }
            if !absent(&metadata)? {
                let meta_fd = open_read(&metadata)?;
                if read_metadata(&meta_fd, &metadata, "Invalid collection metadata.")? != identity_text {
                    return Err(fail("Partial publication metadata changed."));
                }
                fs::remove_file(&metadata)?;
            }
            self.dir.sync()?;
            return self.acknowledge(&file, &published_fd).await;
        }

        let (meta_fd, file) = if absent(&partial)? && absent(&metadata)? {
            let meta_fd = create_private(&metadata, false)?;
            write_all_at(&meta_fd, identity_text.as_bytes(), 0)?;
            meta_fd.sync_all()?;
            (meta_fd, create_private(&partial, true)?)
        } else {
            let meta_fd = open_read(&metadata)?;
            let saved = read_metadata(&meta_fd, &metadata, "Invalid partial metadata.")?;
            if saved != identity_text {
                return Err(fail("Partial belongs to a different artifact or delivery obligation."));
            }
            let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&partial)?;
            (meta_fd, file)
        };
        let mut offset = private_file(&file, &partial, 1)?.size();
        if offset > self.info.size {
            return Err(fail("Partial file exceeds artifact size."));
        }
        // An identity sidecar is not proof of a valid prefix: compare the retained
        // bytes with current authorised chunks before appending anything.
        let mut checked = 0u64;
        while checked < offset {
            let want = (CHUNK_BYTES as u64).min(offset - checked);
            let data = self.chunk_at(checked, want, &meta_fd, &file, offset, &metadata, &partial, &final_path).await?;
            let mut local = vec![0u8; data.len()];
            let n = file.read_at(&mut local, checked)?;
            if n != local.len() || local != data {
                return Err(fail("Partial prefix does not match artifact."));
            }
            checked += data.len() as u64;
        }
        while offset < self.info.size {
            let want = (CHUNK_BYTES as u64).min(self.info.size - offset);
            let data = self.chunk_at(offset, want, &meta_fd, &file, offset, &metadata, &partial, &final_path).await?;
            write_all_at(&file, &data, offset)?;
            offset += data.len() as u64;
        }
        self.verify_partial(&meta_fd, &file, offset, &metadata, &partial, &final_path)?;
        file.sync_all()?;
        if hash_file(&file)? != self.info.sha256 {
            return Err(fail("Artifact checksum failed; partial file retained."));
        }
        self.verify_partial(&meta_fd, &file, offset, &metadata, &partial, &final_path)?;
        // Keep a durable identity across publication and an uncertain acknowledgment.
        let stat = file.metadata()?;
        let publication = json!({"identity": identity, "dev": stat.dev(), "ino": stat.ino()}).to_string();
        let published_fd = if absent(&published)? {
            let fd = create_private(&published, false)?;
            write_all_at(&fd, publication.as_bytes(), 0)?;
            fd.sync_all()?;
            self.dir.sync()?;
            fd
        } else {
            let fd = open_read(&published)?;
            if read_metadata(&fd, &published, "Invalid collection metadata.")? != publication {
                return Err(fail("Publication identity does not match the verified partial."));
            }
            fd
        };
        self.dir.link(&file, &self.name)?;
        self.dir.verify()?;
        private_file(&file, &partial, 2)?;
        if !fs::symlink_metadata(&final_path).is_ok_and(|f| same_file(&file.metadata().expect("held"), &f)) {
            return Err(fail("Published destination was replaced."));
        }
        self.dir.sync()?;
        fs::remove_file(&partial)?;
        fs::remove_file(&metadata)?;
        self.dir.sync()?;
        self.dir.verify()?;
        private_file(&file, &final_path, 1)?;
        self.acknowledge(&file, &published_fd).await
    }

    fn verify_partial(&self, meta_fd: &File, file: &File, offset: u64, metadata: &Path, partial: &Path, final_path: &Path) -> Result<()> {
        self.dir.verify()?;
        private_file(meta_fd, metadata, 1)?;
        if private_file(file, partial, 1)?.size() != offset {
            return Err(fail("Partial file changed during collection."));
        }
        if !absent(final_path)? {
            return Err(fail("Destination already exists; choose a new file."));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn chunk_at(
        &mut self,
        start: u64,
        max_bytes: u64,
        meta_fd: &File,
        file: &File,
        offset: u64,
        metadata: &Path,
        partial: &Path,
        final_path: &Path,
    ) -> Result<Vec<u8>> {
        let chunk = self
            .tx
            .request(json!({"kind": "download_artifact", "artifact_ref": self.artifact_ref, "offset": start, "max_bytes": max_bytes}))
            .await?;
        self.verify_partial(meta_fd, file, offset, metadata, partial, final_path)?;
        let invalid = || fail("Invalid transfer chunk.");
        let Some(Value::String(text)) = chunk.get("data") else { return Err(invalid()) };
        if text.len() as u64 > max_bytes.div_ceil(3) * 4 {
            return Err(invalid());
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let data = engine.decode(text).map_err(|_| invalid())?;
        if !js::same_number(chunk.get("offset"), start as f64)
            || !js::same_number(chunk.get("length"), data.len() as f64)
            || data.is_empty()
            || data.len() as u64 > max_bytes
            || start + data.len() as u64 > self.info.size
            || engine.encode(&data) != *text
        {
            return Err(invalid());
        }
        Ok(data)
    }
}

// ---------------------------------------------------------------------------
// The CLI (ibara-client.mjs:170-242).

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn open_directory(path: Option<&str>) -> Result<OperatorDirectory> {
    OperatorDirectory::open(&directory_path(path))
}

async fn upload(tx: &mut Transfer, argv: &[String]) -> Result<Value> {
    let source = resolve_path(Path::new(&expand_home(&argv[1])));
    let stat = fs::metadata(&source)?;
    if !stat.is_file() {
        return Err(fail("Upload requires a regular file."));
    }
    let file = File::open(&source)?;
    let sha256 = hash_file(&file)?;
    let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stage = match argv.iter().position(|a| a == "--staged-ref") {
        Some(at) => {
            let mut request = serde_json::Map::new();
            request.insert("kind".into(), json!("stat_staged"));
            if let Some(staged) = argv.get(at + 1) {
                request.insert("staged_ref".into(), json!(staged));
            }
            tx.call(Value::Object(request)).await?
        }
        None => tx.call(json!({"kind": "begin_upload", "name": name, "size_bytes": stat.len(), "sha256": sha256})).await?,
    };
    if !js::same_number(stage.get("size_bytes"), stat.len() as f64) {
        return Err(fail("Staged size differs from local file."));
    }
    let staged_ref = stage.get("staged_ref").cloned().unwrap_or(Value::Null);
    // `Number(stage.received_bytes || 0)`
    let received = stage.get("received_bytes").filter(|r| js::truthy(Some(r)));
    let mut offset = if received.is_some() { js::number(received).max(0.0) as u64 } else { 0 };
    let mut buffer = vec![0u8; CHUNK_BYTES];
    while offset < stat.len() {
        let want = (CHUNK_BYTES as u64).min(stat.len() - offset) as usize;
        let count = file.read_at(&mut buffer[..want], offset)?;
        if count == 0 {
            return Err(fail("Upload source shrank while reading."));
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&buffer[..count]);
        let last = offset + count as u64 == stat.len();
        tx.call(json!({"kind": "upload_chunk", "staged_ref": staged_ref, "offset": offset, "data": data, "last": last})).await?;
        offset += count as u64;
    }
    tx.call(json!({"kind": "complete_inbox", "staged_ref": staged_ref, "name": name})).await
}

async fn transfer(route: TransferRoute<'_>, argv: &[String]) -> Result<Value> {
    let mut tx = Transfer::open(route)?;
    let mode = argv[0].as_str();
    let outcome = match mode {
        "stat" => tx.call(json!({"kind": "stat_artifact", "artifact_ref": argv[1]})).await,
        "fetch" => {
            let target = resolve_path(Path::new(&expand_home(&argv[2])));
            collect(&mut tx, &argv[1], &target).await
        }
        _ => upload(&mut tx, argv).await,
    };
    tx.close().await;
    outcome
}

fn run(raw: Vec<String>) -> Result<Option<String>> {
    let mut argv: Vec<String> = Vec::new();
    let (mut computer, mut database, mut rename): (Option<String>, Option<String>, Option<(String, String)>) = (None, None, None);
    let mut i = 0;
    while i < raw.len() {
        let next_ok = raw.get(i + 1).is_some_and(|v| !v.is_empty());
        match raw[i].as_str() {
            "--computer" => {
                if computer.is_some() || !next_ok {
                    return Err(fail(TRANSFER_USAGE));
                }
                computer = Some(raw[i + 1].clone());
                i += 2;
            }
            "--directory-db" => {
                if database.is_some() || !next_ok {
                    return Err(fail(TRANSFER_USAGE));
                }
                database = Some(raw[i + 1].clone());
                i += 2;
            }
            // The label is taken verbatim, so a name that looks like an option is still a name.
            "--rename-computer" => {
                if rename.is_some() || i + 2 >= raw.len() {
                    return Err(fail(RENAME_USAGE));
                }
                rename = Some((raw[i + 1].clone(), raw[i + 2].clone()));
                i += 3;
            }
            _ => {
                argv.push(raw[i].clone());
                i += 1;
            }
        }
    }
    if let Some((name, label)) = rename {
        if computer.is_some() || !argv.is_empty() {
            return Err(fail(RENAME_USAGE));
        }
        let mut directory = open_directory(database.as_deref())?;
        let id = directory.resolve_computer(&name)?.computer_id;
        return Ok(Some(directory.rename_computer(&id, &label)?.to_json().to_string()));
    }
    if argv.first().map(String::as_str) == Some("--import-legacy") {
        if computer.is_some() || argv.len() != 2 || !Path::new(&argv[1]).is_absolute() {
            return Err(fail("Usage: ibara-client [--directory-db FILE] --import-legacy ABSOLUTE_DESCRIPTOR_FILE"));
        }
        let stat = fs::symlink_metadata(&argv[1])?;
        if !stat.is_file() || stat.uid() != current_uid() || stat.mode() & 0o022 != 0 || stat.size() > METADATA_LIMIT {
            return Err(fail("Legacy descriptor must be a bounded user-owned regular file not writable by others."));
        }
        let descriptor = super::parse_json(&fs::read_to_string(&argv[1])?)?;
        let mut directory = open_directory(database.as_deref())?;
        return Ok(Some(directory.import_legacy_descriptor(&descriptor)?.to_json().to_string()));
    }
    if argv.first().map(String::as_str) == Some("--list-computers") {
        if computer.is_some() || argv.len() != 1 {
            return Err(fail("Usage: ibara-client [--directory-db FILE] --list-computers"));
        }
        let directory = open_directory(database.as_deref())?;
        let rows: Vec<Value> = directory.list_computers()?.iter().map(|r| r.to_json()).collect();
        return Ok(Some(Value::Array(rows).to_string()));
    }
    if argv.len() == 1 && argv[0] == "--capabilities" {
        return Ok(Some(json!({"safe_collection": 1, "operator_transfer": 1}).to_string()));
    }
    let operator = argv.first().map(String::as_str) == Some("--operator");
    if operator {
        argv.remove(0);
    }
    let mode = argv.first().map(String::as_str).unwrap_or("");
    let arg1_ok = argv.get(1).is_some_and(|a| !a.is_empty());
    let arg2_ok = argv.get(2).is_some_and(|a| !a.is_empty());
    if !["fetch", "upload", "stat"].contains(&mode) || !arg1_ok || (mode == "fetch" && !arg2_ok) {
        return Err(fail(
            "Usage: ibara-client [--directory-db FILE] [--computer NAME] [--operator] fetch REF DEST | upload FILE [--staged-ref REF] | stat REF",
        ));
    }
    if database.is_some() && computer.is_none() {
        return Err(fail("--directory-db is only valid with an explicit --computer target."));
    }
    let envelope = match &computer {
        Some(computer) => {
            if operator {
                return Err(fail("Selected-target transfers cannot use the legacy local operator route."));
            }
            if !["fetch", "stat"].contains(&mode) {
                return Err(fail("Selected-target upload is not supported by this routing slice."));
            }
            let directory = open_directory(database.as_deref())?;
            let envelope = directory.resolve_computer(computer).and_then(|row| directory.bind_operation(&row.computer_id, None, &argv[1]));
            directory.close();
            Some(envelope?)
        }
        None => None,
    };
    let route = match (&envelope, operator) {
        (Some(envelope), _) => TransferRoute::Selected(envelope),
        (None, true) => TransferRoute::Operator,
        (None, false) => TransferRoute::Legacy,
    };
    let value = runtime()?.block_on(transfer(route, &argv))?;
    Ok(Some(pretty(&value)))
}

/// `ibara client ARGS…`.
pub fn main(args: Vec<String>) -> ExitCode {
    match run(args) {
        Ok(Some(output)) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => exit_with(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::super::directory::tests::TempDir;
    use super::*;

    #[test]
    fn destination_under_a_group_writable_parent_is_refused() {
        let tmp = TempDir::new("dest");
        let open = tmp.0.join("open");
        fs::DirBuilder::new().mode(0o775).create(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o775)).unwrap();
        let err = Destination::open(&open.join("file.bin")).err().unwrap();
        assert_eq!(err.message, "Destination has an unsafe parent directory.");
    }

    #[test]
    fn destination_through_a_symlinked_directory_is_refused() {
        let tmp = TempDir::new("dest-link");
        let real = tmp.0.join("real");
        fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.0.join("alias")).unwrap();
        assert!(Destination::open(&tmp.0.join("alias").join("file.bin")).is_err());
    }

    #[test]
    fn link_publishes_the_held_descriptor_exclusively() {
        let tmp = TempDir::new("dest-link-at");
        let target = tmp.0.join("out").join("file.bin");
        let dir = Destination::open(&target).unwrap();
        let partial = dir.entry("file.bin.ibara-part");
        let file = create_private(&partial, true).unwrap();
        write_all_at(&file, b"hello", 0).unwrap();
        dir.link(&file, "file.bin").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        assert!(dir.link(&file, "file.bin").is_err(), "an existing name is never replaced");
        assert_eq!(private_file(&file, &partial, 2).unwrap().size(), 5);
    }
}
