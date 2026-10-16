//! Private, content-free event archive. Control never waits on compression or upload.
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::{collections::HashMap, fs::{self, OpenOptions}, io::Write, os::unix::fs::{DirBuilderExt, OpenOptionsExt}, path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicU64, Ordering}, mpsc::{self, SyncSender}}, thread, time::{Duration, Instant}};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_SPOOL: u64 = 64 * 1024 * 1024;
const FLUSH: Duration = Duration::from_secs(300);
enum Message { Fact(Value, bool), Stop }

pub struct Recorder {
    sender: Option<SyncSender<Message>>,
    worker: Option<thread::JoinHandle<()>>,
    lost: Arc<AtomicU64>,
    sessions: HashMap<String, (String, HashMap<String, usize>, u64)>,
    sequence: u64,
    protected: Option<String>,
    build: String,
}
impl Recorder {
    pub fn new(state: &Path) -> Self {
        let lost = Arc::new(AtomicU64::new(0));
        let build = option_env!("IBARA_FACTS_BUILD").unwrap_or("unattributed-development").to_owned();
        if std::env::var("IBARA_FACTS_DISABLED").as_deref() == Ok("1") {
            return Self { sender: None, worker: None, lost, sessions: HashMap::new(), sequence: 0, protected: None, build };
        }
        let dir = std::env::var_os("IBARA_FACTS_DIR").map(PathBuf::from).unwrap_or_else(|| state.join("facts"));
        let (sender, receiver) = mpsc::sync_channel(1024);
        let losses = lost.clone();
        let worker = thread::spawn(move || {
            // Recover complete lines left by a killed process before accepting new work.
            for raw in pending_files(&dir).unwrap_or_default() {
                if let Ok(mut bytes) = fs::read(&raw) {
                    if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') { bytes.truncate(end + 1); }
                    else { continue; }
                    if seal(&dir, &raw, &bytes).is_err() { eprintln!("ibara facts: recovery retained pending archive"); }
                }
            }
            let mut bytes = Vec::new();
            let mut raw: Option<PathBuf> = None;
            let mut since = Instant::now();
            let mut writable = spool_size(&dir).unwrap_or(MAX_SPOOL) < MAX_SPOOL;
            loop {
                let next = receiver.recv_timeout(Duration::from_secs(1));
                let stop = matches!(next, Ok(Message::Stop) | Err(mpsc::RecvTimeoutError::Disconnected));
                let mut end = false;
                if let Ok(Message::Fact(fact, finish)) = next {
                    end = finish;
                    if raw.is_none() && writable {
                        let date = crate::ids::now_iso();
                        raw = Some(dir.join(format!("raw/v1/{}/{}/{}/{}-{}.jsonl.pending", &date[..4], &date[5..7], &date[8..10], date.replace(':', "-"), uuid::Uuid::new_v4().simple())));
                    }
                    let line = serde_json::to_vec(&fact).unwrap_or_default();
                    if let Some(path) = raw.as_ref().filter(|_| writable) {
                        let mut line = line; line.push(b'\n');
                        match append(path, &line) {
                            Ok(()) => bytes.extend(line),
                            Err(_) => { losses.fetch_add(1, Ordering::Relaxed); writable = false; eprintln!("ibara facts: spool unavailable; facts loss counted"); }
                        }
                    } else { losses.fetch_add(1, Ordering::Relaxed); }
                }
                if !bytes.is_empty() && (stop || end || bytes.len() >= MAX_BYTES || since.elapsed() >= FLUSH) {
                    let count = losses.swap(0, Ordering::Relaxed);
                    if count > 0 {
                        let loss = json!({"kind":"recorder.loss", "t":crate::ids::now_millis(), "session":"recorder", "actor":"0", "who":"system", "build":option_env!("IBARA_FACTS_BUILD").unwrap_or("unattributed-development"), "lines":count});
                        let mut line = serde_json::to_vec(&loss).unwrap(); line.push(b'\n');
                        if append(raw.as_ref().unwrap(), &line).is_ok() { bytes.extend(line); }
                        else { losses.fetch_add(count, Ordering::Relaxed); }
                    }
                    if seal(&dir, raw.as_ref().unwrap(), &bytes).is_err() { eprintln!("ibara facts: compression failed; pending JSONL retained for restart"); }
                    bytes.clear(); raw = None; since = Instant::now();
                    writable = spool_size(&dir).unwrap_or(MAX_SPOOL) < MAX_SPOOL;
                }
                if !writable && since.elapsed() >= FLUSH {
                    writable = spool_size(&dir).unwrap_or(MAX_SPOOL) < MAX_SPOOL;
                    since = Instant::now();
                }
                if stop {
                    let n = losses.load(Ordering::Relaxed);
                    if n > 0 { eprintln!("ibara facts: {n} facts lost at shutdown"); }
                    break;
                }
            }
        });
        Self { sender: Some(sender), worker: Some(worker), lost, sessions: HashMap::new(), sequence: 0, protected: None, build }
    }
    /// Caller supplies only internal categories, never request/response bodies.
    pub fn event(&mut self, kind: &str, session: &str, actor: &str, who: &str, fields: Value, finish: bool) {
        if self.sender.is_none() { return; }
        self.sequence += 1;
        if self.sessions.len() >= 1024 && !self.sessions.contains_key(session) {
            let oldest = self.sessions.iter().filter(|(key,_)| Some(key.as_str()) != self.protected.as_deref()).min_by_key(|(_,v)| v.2).map(|(key,_)|key.clone());
            if let Some(key) = oldest { self.sessions.remove(&key); }
        }
        let (session_id, actors, seen) = self.sessions.entry(session.to_owned()).or_insert_with(|| (uuid::Uuid::new_v4().to_string(), HashMap::new(), 0));
        *seen = self.sequence;
        if actors.len() >= 256 && !actors.contains_key(&format!("{who}:{actor}")) {
            *session_id = uuid::Uuid::new_v4().to_string(); actors.clear();
        }
        let next = actors.len() + 1;
        let number = *actors.entry(format!("{who}:{actor}")).or_insert(next);
        let who = if who == "bot" && std::env::var("IBARA_FACTS_TEST_AGENT").as_deref() == Ok("1") { "agent" } else { who };
        let mut fact = json!({"kind":kind,"t":crate::ids::now_millis(),"session":session_id,"actor":number.to_string(),"who":who,"build":self.build,"schema":1,"fields":fields});
        if let Some(n) = fact.get_mut("fields") { if n.is_null() { *n = json!({}); } }
        if self.sender.as_ref().unwrap().try_send(Message::Fact(fact, finish)).is_err() { self.lost.fetch_add(1, Ordering::Relaxed); }
        // Retired episodes use a fresh UUID if read later, never reused actor slots.
        if finish && kind == "call" { self.sessions.remove(session); }
    }
    pub fn protect(&mut self, session: Option<String>) { self.protected = session; }
    pub fn retire(&mut self, session: &str) { self.sessions.remove(session); }
    pub fn close(&mut self) {
        if let Some(sender) = self.sender.take() { let _ = sender.send(Message::Stop); }
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}
impl Drop for Recorder { fn drop(&mut self) { self.close(); } }

fn spool_size(path: &Path) -> std::io::Result<u64> {
    if !path.exists() { return Ok(0); }
    let mut size = 0;
    for entry in fs::read_dir(path)? { let e = entry?; let m = e.metadata()?; size += if m.is_dir() { spool_size(&e.path())? } else { m.len() }; }
    Ok(size)
}
fn save(root: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Sealing replaces an existing raw chunk, even when the spool is full.
    // New raw writes enforce the budget; recovery needs one chunk of temporary headroom.
    let _ = root;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap())?;
    let temporary = path.with_extension("partial");
    // The target lock ensures one recorder owns this spool. A killed compressor
    // may leave its temporary file; the raw chunk is still authoritative.
    match fs::remove_file(&temporary) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e) }
    let result = (|| {
        let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        let mut gz = GzEncoder::new(file, Compression::fast());
        gz.write_all(bytes)?;
        gz.finish()?.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(path.parent().unwrap())?.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_file(temporary); }
    result
}

fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap())?;
    OpenOptions::new().append(true).create(true).mode(0o600).open(path)?.write_all(line)
}
fn seal(root: &Path, raw: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::File::open(raw)?.sync_all()?;
    let archive = raw.with_extension("gz");
    if !archive.exists() { save(root, &archive, bytes)?; }
    fs::remove_file(raw)
}
fn pending_files(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !path.exists() { return Ok(Vec::new()); }
    let mut found = Vec::new();
    for e in fs::read_dir(path)? { let e=e?; if e.file_type()?.is_dir() { found.extend(pending_files(&e.path())?); }
        else if e.path().extension().is_some_and(|s| s=="pending") { found.push(e.path()); } }
    Ok(found)
}

pub fn category(value: &str) -> &str {
    match value {
        "status" | "session" | "observe" | "observe_video" | "task_status" | "take_control" | "handback" | "pause" | "resume" |
        "access" | "access_set" | "access_remove" | "access_unpair" | "attention" | "answer_attention" | "viewer_register" | "viewer_ticket" |
        "warm" | "join" | "screen_failed" | "clipboard_get" | "clipboard_set" | "login_configure" | "login_pending" | "login_deliver" |
        "login_report" | "login_answer" | "login_remove" | "login_probe" | "login_assist" | "login_authorize_once" | "health" | "settings" |
        "logs" | "windows" | "window_close" | "window_move" | "files_list" | "files_read" | "files_publish" | "files_begin_download" |
        "shutdown" | "restart" | "sleep" | "lock" | "repair" | "task" | "receipts" | "events" => value,
        _ => "unknown",
    }
}
