//! Own-screen sender: one child, versioned JSON lines on stdin/stdout.
use crate::error::{IbaraError, Result};
use serde_json::{Value, json};
use std::{cell::{Cell, RefCell}, collections::VecDeque, path::{Path, PathBuf}, process::Stdio, rc::Rc, time::Duration};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, process::{Child, ChildStdin}, sync::Mutex};

#[derive(Default)]
struct State {
    ready: Option<Value>,
    encoder: Option<String>,
    status: Option<Value>,
    failure: Option<String>,
    settled: Option<bool>,
    command_result: Option<Value>,
    events: VecDeque<Value>,
}

pub struct ScreenStream {
    program: PathBuf,
    next_command: Cell<u64>,
    dir: PathBuf,
    child: RefCell<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    state: Rc<RefCell<State>>,
    reader: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

fn unavailable(message: impl Into<String>) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", message, true)
}

impl ScreenStream {
    pub fn new(state_dir: &Path) -> Self {
        Self { next_command: Cell::new(0), program: std::env::var_os("IBARA_SCREEN_BIN").map(PathBuf::from).unwrap_or_else(|| "/usr/bin/ibara-screen".into()),
            dir: state_dir.join("screen"), child: RefCell::new(None), stdin: Mutex::new(None), state: Rc::new(RefCell::new(State::default())), reader: RefCell::new(None) }
    }
    // Retain the capability receipt when an unused sender stops. A new start replaces it.
    pub fn hardware_encoder(&self) -> Option<bool> { self.state.borrow().encoder.as_deref().map(|e| e == "h264_vaapi") }
    pub fn encoder(&self) -> Option<String> { self.state.borrow().encoder.clone() }
    pub fn available(&self) -> bool { self.program.is_file() }
    pub fn events(&self) -> Vec<Value> { self.state.borrow_mut().events.drain(..).collect() }
    pub fn status(&self) -> Option<Value> { self.state.borrow().status.clone() }
    pub fn failure(&self) -> Option<String> { self.state.borrow().failure.clone() }
    pub fn running(&self) -> bool { self.child.borrow().is_some() }

    pub async fn command(&self, mut value: Value) -> Result<()> {
        value["v"] = json!(1);
        let id = self.next_command.get().checked_add(1).ok_or_else(|| unavailable("Screen command sequence exhausted."))?;
        self.next_command.set(id);
        value["id"] = json!(id);
        let answered = self.state.borrow().ready.as_ref().is_some_and(|v| v["command_results"] == true) && value["t"] != "stop";
        let mut slot = self.stdin.lock().await;
        let stdin = slot.as_mut().ok_or_else(|| unavailable("The screen sender is not running."))?;
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_millis(500), async { stdin.write_all(&bytes).await?; stdin.flush().await }).await
            .map_err(|_| unavailable("The screen sender stopped answering."))??;
        if answered {
            tokio::time::timeout(Duration::from_millis(500), async {
                loop {
                    let reply = self.state.borrow().command_result.clone();
                    if let Some(reply) = reply.filter(|r| r["id"] == id) {
                        return if reply["ok"] == true { Ok(()) } else { Err(unavailable(reply["reason"].as_str().unwrap_or("Screen command refused."))) };
                    }
                    if let Some(reason) = self.failure() { return Err(unavailable(reason)); }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.map_err(|_| unavailable("The screen sender did not answer its command."))??;
        }
        Ok(())
    }

    pub async fn start(&self) -> Result<Value> {
        if let Some(ready) = self.state.borrow().ready.clone() {
            if self.failure().is_none() { return Ok(ready); }
        }
        self.stop().await;
        *self.state.borrow_mut() = State::default();
        std::fs::create_dir_all(&self.dir)?;
        let mut command = tokio::process::Command::new(&self.program);
        command.args(["send", "--state-dir"]).arg(&self.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe { command.pre_exec(|| {
            let parent = libc::getppid();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::getppid() != parent { libc::_exit(1); }
            Ok(())
        }); }
        let mut child = command.spawn().map_err(|e| unavailable(format!("The screen sender could not start: {e}")))?;
        *self.stdin.lock().await = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        *self.child.borrow_mut() = Some(child);
        let state = self.state.clone();
        let reader_task = tokio::task::spawn_local(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                // Bound untrusted output before parsing or retaining it.
                use tokio::io::AsyncReadExt;
                let result = (&mut reader).take(65_537).read_until(b'\n', &mut bytes).await;
                if !matches!(result, Ok(n) if n > 0 && n <= 65_536) { break; }
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { break };
                if value["v"] != 1 { break; }
                let mut state = state.borrow_mut();
                match value["t"].as_str().or_else(|| value["type"].as_str()) {
                    Some("ready") => {
                        state.encoder = Some(value["encoder"].as_str().unwrap_or("").to_owned());
                        state.ready = Some(value);
                    },
                    Some("status") => state.status = Some(value),
                    Some("command_result") => state.command_result = Some(value),
                    Some("error") => eprintln!("screen command refused: {}", value["reason"]),
                    Some("settled") => state.settled = value["ok"].as_bool(),
                    Some("failed") => { state.failure = Some(value["reason"].as_str().unwrap_or("The screen sender failed.").into()); },
                    Some("turn_request" | "person_input" | "viewer") => {
                        if state.events.len() >= 1024 { state.failure = Some("The screen sender's event queue overflowed.".into()); break; }
                        state.events.push_back(value);
                    }
                    _ => {}
                }
            }
            let mut state = state.borrow_mut();
            state.ready = None;
            state.failure.get_or_insert_with(|| "The screen sender exited or sent an invalid message.".into());
        });
        *self.reader.borrow_mut() = Some(reader_task);
        let wait = async {
            loop {
                if let Some(reason) = self.failure() { return Err(unavailable(reason)); }
                if let Some(ready) = self.state.borrow().ready.clone() {
                    let pin = ready["cert_sha256"].as_str().unwrap_or("");
                    if ready["port"] != 47910 || pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                        return Err(unavailable("The screen sender returned an invalid address or certificate pin."));
                    }
                    return Ok(ready);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(3), wait).await.map_err(|_| unavailable("The screen sender did not become ready within 3 seconds."))?
    }

    pub async fn settle(&self) -> Result<()> {
        self.state.borrow_mut().settled = None;
        self.command(json!({"t":"settle"})).await?;
        let wait = async {
            loop {
                if let Some(reason) = self.failure() { return Err(unavailable(reason)); }
                if let Some(ok) = self.state.borrow().settled { return if ok { Ok(()) } else { Err(unavailable("The screen sender could not release held input.")) }; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), wait).await.map_err(|_| unavailable("The screen sender did not release held input within 2 seconds."))?
    }
    pub async fn stop(&self) {
        if self.running() { let _ = self.command(json!({"t":"stop"})).await; }
        self.stdin.lock().await.take();
        let child = self.child.borrow_mut().take();
        if let Some(mut child) = child {
            if tokio::time::timeout(Duration::from_millis(200), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        if let Some(reader) = self.reader.borrow_mut().take() { reader.abort(); }
        self.state.borrow_mut().ready = None;
    }
}
