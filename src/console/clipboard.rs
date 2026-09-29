//! The shared clipboard for a computer this console controls: copy on one,
//! paste on the other, both ways, text and PNG pictures up to 8 MiB. It
//! travels over the computer's pairing route (`clipboard_get`,
//! `clipboard_set`), never the stream, from Take Control or Open Viewer until
//! Hand Back, or until the computer says this console no longer has control.
//! While that computer's `shared_clipboard` setting is off nothing is sent,
//! and the console asks every ten seconds whether it is back on.
//!
//! Only copies made after control began are shared. A clip that came from
//! the other computer is not sent back: the hash of what the clipboard holds
//! is kept, and the same content again is no change.

use super::Console;
use crate::desktop::clipboard::{self, Clip};
use crate::error::IbaraError;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

const CLIP_MAX: usize = 8 * 1024 * 1024;
/// The largest piece per request (1 MiB as base64, under the route's bound).
const CHUNK: usize = 768 * 1024;
const POLL: Duration = Duration::from_secs(1);
const POLL_OFF: Duration = Duration::from_secs(10);
/// Transport failures in a row before the console gives up (about two minutes).
const MAX_FAILURES: u32 = 120;

/// Share the clipboard with `computer` (at controller epoch `epoch`), unless
/// that already runs.
pub(crate) fn start(console: &Arc<Console>, computer: &str, epoch: &str) {
    let mut running = console.clipboards.lock().unwrap_or_else(|e| e.into_inner());
    if running.get(computer).is_some_and(|(e, task)| e == epoch && !task.is_finished()) {
        return;
    }
    if let Some((_, task)) = running.remove(computer) {
        task.abort();
    }
    let task = tokio::spawn(share(console.clone(), computer.to_string(), epoch.to_string()));
    running.insert(computer.to_string(), (epoch.to_string(), task));
}

/// Stop sharing with `computer`.
pub(crate) fn stop(console: &Console, computer: &str) {
    if let Some((_, task)) = console.clipboards.lock().unwrap_or_else(|e| e.into_inner()).remove(computer) {
        task.abort();
    }
}

fn sha256_hex(data: &[u8]) -> String {
    crate::store::canonical::hex_encode(&Sha256::digest(data))
}

/// Why a call did not work: control ended (or the computer changed), or the
/// route failed for now.
enum Failed {
    Ended,
    Retry,
}

/// Only the computer saying this console no longer holds control, or that it
/// changed, ends sharing; anything else (a refused piece included) is tried again.
fn classify(error: &IbaraError) -> Failed {
    match error.code {
        "PERMISSION_DENIED" | "STALE_TARGET" => Failed::Ended,
        _ => Failed::Retry,
    }
}

/// What the computer has for this console.
enum Fetched {
    Off,
    Nothing,
    Clip(u64, Clip, String),
}

async fn call(console: &Console, computer: &str, epoch: &str, op: &str, fields: Value) -> Result<Value, Failed> {
    match tokio::time::timeout(Duration::from_secs(20), console.sessions.call(computer, Some(epoch), op, fields)).await {
        Ok(Ok(data)) => Ok(data.get("result").cloned().unwrap_or(Value::Null)),
        Ok(Err(error)) => Err(classify(&error)),
        Err(_) => Err(Failed::Retry),
    }
}

/// The computer's latest entry after `since`, in pieces.
async fn fetch(console: &Console, computer: &str, epoch: &str, since: u64) -> Result<Fetched, Failed> {
    let mut data: Vec<u8> = Vec::new();
    let mut first: Option<(u64, String, usize, String)> = None;
    loop {
        let reply = call(console, computer, epoch, "clipboard_get", json!({"since": since, "offset": data.len()})).await?;
        if reply["enabled"] != json!(true) {
            return Ok(Fetched::Off);
        }
        let Some(clip) = reply.get("clip").filter(|c| c.is_object()) else { return Ok(Fetched::Nothing) };
        let seq = reply["seq"].as_u64().unwrap_or(0);
        let mime = clip["mime"].as_str().unwrap_or("").to_string();
        let size = clip["size"].as_u64().and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
        let sha = clip["sha256"].as_str().unwrap_or("").to_string();
        let piece = base64::engine::general_purpose::STANDARD.decode(clip["data_b64"].as_str().unwrap_or("")).map_err(|_| Failed::Retry)?;
        let this = (seq, mime, size, sha);
        match &first {
            None => first = Some(this),
            // A newer copy replaced it meanwhile: start again with that one.
            Some(expected) if *expected != this => return Ok(Fetched::Nothing),
            Some(_) => {}
        }
        if piece.is_empty() || size == 0 || size > CLIP_MAX || clip["offset"].as_u64() != Some(data.len() as u64) {
            return Err(Failed::Retry);
        }
        data.extend_from_slice(&piece);
        let (seq, mime, size, sha) = first.clone().expect("first piece");
        if data.len() >= size {
            if data.len() != size || sha256_hex(&data) != sha {
                return Err(Failed::Retry);
            }
            return Ok(Fetched::Clip(seq, Clip { mime, data }, sha));
        }
    }
}

/// Send `clip` in pieces; whether the computer has sharing on.
async fn send(console: &Console, computer: &str, epoch: &str, clip: &Clip, sha: &str) -> Result<bool, Failed> {
    let mut offset = 0;
    while offset < clip.data.len() {
        let end = (offset + CHUNK).min(clip.data.len());
        let fields = json!({
            "mime": clip.mime,
            "size": clip.data.len(),
            "sha256": sha,
            "offset": offset,
            "data_b64": base64::engine::general_purpose::STANDARD.encode(&clip.data[offset..end]),
        });
        let reply = call(console, computer, epoch, "clipboard_set", fields).await?;
        if reply["enabled"] != json!(true) {
            return Ok(false);
        }
        offset = end;
    }
    Ok(true)
}

async fn next_change(changes: &mut Option<mpsc::Receiver<()>>) -> Option<()> {
    match changes {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn share(console: Arc<Console>, computer: String, epoch: String) {
    let mut seen = clipboard::read(CLIP_MAX).await.ok().flatten().map(|clip| sha256_hex(&clip.data));
    let mut changes = match clipboard::watch() {
        Ok(rx) => Some(rx),
        Err(e) => {
            eprintln!("ibarad: this computer's clipboard is not shared: {}", e.message);
            None
        }
    };
    let mut since = 0u64;
    let mut enabled = true;
    let mut failures = 0u32;
    let mut poll_at = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(poll_at) => {
                let fetched = fetch(&console, &computer, &epoch, since).await;
                poll_at = Instant::now() + if matches!(fetched, Ok(Fetched::Off)) { POLL_OFF } else { POLL };
                match fetched {
                    Ok(Fetched::Off) => enabled = false,
                    Ok(Fetched::Nothing) => enabled = true,
                    Ok(Fetched::Clip(seq, clip, sha)) => {
                        enabled = true;
                        since = seq;
                        if seen.as_deref() != Some(sha.as_str()) {
                            seen = Some(sha);
                            if let Err(e) = clipboard::write(&clip).await {
                                eprintln!("ibarad: a copy from another computer could not be put on the clipboard: {}", e.message);
                            }
                        }
                    }
                    Err(Failed::Ended) => break,
                    Err(Failed::Retry) => {
                        failures += 1;
                        if failures >= MAX_FAILURES {
                            break;
                        }
                        continue;
                    }
                }
                failures = 0;
            }
            change = next_change(&mut changes) => {
                if change.is_none() {
                    changes = None;
                    continue;
                }
                let Ok(Some(clip)) = clipboard::read(CLIP_MAX).await else { continue };
                let sha = sha256_hex(&clip.data);
                if seen.as_deref() == Some(sha.as_str()) {
                    continue;
                }
                seen = Some(sha.clone());
                if !enabled {
                    continue;
                }
                match send(&console, &computer, &epoch, &clip, &sha).await {
                    Ok(on) => enabled = on,
                    Err(Failed::Ended) => break,
                    Err(Failed::Retry) => {}
                }
            }
        }
    }
}
