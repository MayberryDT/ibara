//! A small MCP client over a child process's stdio: newline-delimited
//! JSON-RPC, one request at a time, each with a deadline. The child's stderr
//! passes through to ours.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// One request and its reply, as the harness measures it.
pub struct Exchange {
    /// The JSON-RPC `result`, or why there is none (a JSON-RPC error, a
    /// timeout, or the server closing its output).
    pub result: Result<Value, String>,
    pub ms: u64,
    pub request_bytes: usize,
    pub response_bytes: usize,
}

pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Client {
    pub fn spawn(argv: &[String]) -> std::io::Result<Client> {
        let (program, args) = argv.split_first().ok_or_else(|| std::io::Error::other("no server command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Ok(Client { stdin: child.stdin.take(), child, lines: rx, next_id: 1 })
    }

    fn write(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin already closed")?;
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).map_err(|e| format!("write to server: {e}"))
    }

    /// Send one request and wait for the reply with its id; notifications and
    /// replies to other ids are skipped.
    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Exchange {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        let started = Instant::now();
        let request_bytes = line.len();
        let mut response_bytes = 0;
        let elapsed = |s: Instant| s.elapsed().as_millis() as u64;
        if let Err(e) = self.write(&line) {
            return Exchange { result: Err(e), ms: elapsed(started), request_bytes, response_bytes };
        }
        let deadline = started + timeout;
        let result = loop {
            match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(text) => {
                    let Ok(message) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if message.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    response_bytes = text.trim_end_matches(['\n', '\r']).len();
                    break match (message.get("result"), message.get("error")) {
                        (Some(result), _) => Ok(result.clone()),
                        (None, Some(error)) => Err(format!("JSON-RPC error: {error}")),
                        (None, None) => Err("reply without result or error".into()),
                    };
                }
                Err(RecvTimeoutError::Timeout) => break Err(format!("no reply to {method} within {} s", timeout.as_secs())),
                Err(RecvTimeoutError::Disconnected) => break Err(format!("the server closed its output before answering {method}")),
            }
        };
        Exchange { result, ms: elapsed(started), request_bytes, response_bytes }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let mut line = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        line.push('\n');
        let _ = self.write(&line);
    }

    /// Close stdin and wait up to `grace` for the server to exit, then kill it.
    pub fn close(mut self, grace: Duration) -> Option<i32> {
        drop(self.stdin.take());
        let until = Instant::now() + grace;
        while Instant::now() < until {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        self.child.wait().ok().and_then(|s| s.code())
    }
}
