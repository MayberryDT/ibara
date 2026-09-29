//! The shared clipboard during Take Control: `clipboard_get` and
//! `clipboard_set` on the operator route, for the person holding control
//! only, while this computer's `shared_clipboard` setting is on.
//!
//! A watcher on this computer's clipboard runs only while someone holds
//! control (from their first `clipboard_get` until control ends). Each change
//! it sees becomes the latest entry, numbered; the holder's console polls for
//! entries after the last one it took. Text and PNG pictures up to 8 MiB
//! travel in chunks of at most 768 KiB. What the console sets is not sent
//! back: the hash of the clipboard as last known here is kept, and a change
//! to the same content is no change. Nothing here is journalled, logged or
//! shown in the timeline.

use super::Controller;
use super::ports::Clip;
use crate::error::{IbaraError, Result, denied, invalid};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The largest clipboard entry either way.
pub const CLIP_MAX: usize = 8 * 1024 * 1024;
/// The largest piece of one in a request or reply (1 MiB as base64).
pub const CHUNK: usize = 768 * 1024;
/// The clipboard kinds shared: text, and pictures as PNG.
pub const MIMES: [&str; 2] = ["text/plain;charset=utf-8", "image/png"];

fn sha256_hex(data: &[u8]) -> String {
    crate::store::canonical::hex_encode(&Sha256::digest(data))
}

/// A clip arriving in pieces.
struct Incoming {
    mime: String,
    size: usize,
    sha256: String,
    data: Vec<u8>,
}

/// The shared clipboard's state; empty while nobody holds control.
#[derive(Default)]
pub(crate) struct Clipboard {
    /// Numbers entries; it only grows, so a console's `since` stays valid.
    seq: u64,
    /// The latest change seen here: number, clip, hash.
    latest: Option<(u64, Clip, String)>,
    /// The hash of what this computer's clipboard holds, as far as known.
    seen: Option<String>,
    incoming: Option<Incoming>,
    watch: Option<tokio::task::JoinHandle<()>>,
}

impl Controller {
    fn clipboard_holder(&self, operator_id: &str) -> Result<()> {
        let viewer = self.viewer_state.borrow();
        if viewer.fault || viewer.owner.as_deref() != Some(operator_id) {
            return Err(denied("You no longer have control of this computer."));
        }
        Ok(())
    }

    fn clipboard_enabled(&self) -> bool {
        crate::settings::current().bool("shared_clipboard")
    }

    /// Stop watching and forget every entry (control ended).
    pub(crate) fn clipboard_stop(&self) {
        let mut clipboard = self.clipboard.borrow_mut();
        if let Some(watch) = clipboard.watch.take() {
            watch.abort();
        }
        clipboard.latest = None;
        clipboard.seen = None;
        clipboard.incoming = None;
    }

    /// Start the watcher unless it runs. What the clipboard holds now is the
    /// starting point: only later copies are shared. Control may end while the
    /// clipboard is read; then no watcher starts.
    async fn clipboard_watch(&self, operator_id: &str) -> Result<()> {
        if self.clipboard.borrow().watch.as_ref().is_some_and(|w| !w.is_finished()) {
            return Ok(());
        }
        let revision = self.viewer_state.borrow().revision;
        let mut changes = self.desktop.clipboard_watch()?;
        let now = self.desktop.clipboard_read(CLIP_MAX).await.ok().flatten();
        if self.viewer_state.borrow().revision != revision {
            return Err(denied("You no longer have control of this computer."));
        }
        self.clipboard_holder(operator_id)?;
        let me = self.me.borrow().clone();
        let watch = tokio::task::spawn_local(async move {
            while changes.recv().await.is_some() {
                let Some(controller) = me.upgrade() else { return };
                controller.clipboard_changed().await;
            }
        });
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard.watch.as_ref().is_some_and(|w| !w.is_finished()) {
            watch.abort();
            return Ok(());
        }
        clipboard.seen = now.map(|clip| sha256_hex(&clip.data));
        clipboard.watch = Some(watch);
        Ok(())
    }

    /// The watcher saw a change: a new entry unless it holds what is known.
    async fn clipboard_changed(&self) {
        let Ok(Some(clip)) = self.desktop.clipboard_read(CLIP_MAX).await else { return };
        let sha = sha256_hex(&clip.data);
        let mut clipboard = self.clipboard.borrow_mut();
        if clipboard.watch.is_none() || clipboard.seen.as_deref() == Some(sha.as_str()) {
            return;
        }
        clipboard.seq += 1;
        clipboard.seen = Some(sha.clone());
        clipboard.latest = Some((clipboard.seq, clip, sha));
    }

    /// `clipboard_get {since, offset?}`: the latest entry after `since`, as the
    /// piece starting at `offset`; nothing new is `{seq}` alone.
    pub(crate) async fn clipboard_get(&self, operator_id: &str, action: &Value) -> Result<Value> {
        self.clipboard_holder(operator_id)?;
        if !self.clipboard_enabled() {
            self.clipboard_stop();
            return Ok(json!({"enabled": false}));
        }
        let since = action.get("since").and_then(Value::as_u64).ok_or_else(|| invalid("Expected since."))?;
        let offset = action.get("offset").map_or(Some(0), Value::as_u64).ok_or_else(|| invalid("Expected a whole offset."))?;
        self.clipboard_watch(operator_id).await?;
        let clipboard = self.clipboard.borrow();
        let Some((seq, clip, sha)) = clipboard.latest.as_ref().filter(|(seq, ..)| *seq > since) else {
            return Ok(json!({"enabled": true, "seq": clipboard.seq}));
        };
        // A newer, shorter copy replaced the entry being fetched: nothing is at
        // this offset any more, and the console starts the new entry from 0.
        let Some(start) = usize::try_from(offset).ok().filter(|o| *o == 0 || *o < clip.data.len()) else {
            return Ok(json!({"enabled": true, "seq": clipboard.seq}));
        };
        let end = (start + CHUNK).min(clip.data.len());
        Ok(json!({
            "enabled": true,
            "seq": seq,
            "clip": {
                "mime": clip.mime,
                "size": clip.data.len(),
                "sha256": sha,
                "offset": start,
                "data_b64": base64::engine::general_purpose::STANDARD.encode(&clip.data[start..end]),
            },
        }))
    }

    /// `clipboard_set {mime, size, sha256, offset, data_b64}`: one piece of a
    /// clip for this computer's clipboard, in order from offset 0; the last
    /// piece puts it there once its hash matches.
    pub(crate) async fn clipboard_set(&self, operator_id: &str, action: &Value) -> Result<Value> {
        self.clipboard_holder(operator_id)?;
        if !self.clipboard_enabled() {
            self.clipboard_stop();
            return Ok(json!({"enabled": false}));
        }
        let text = |key: &str| action.get(key).and_then(Value::as_str).unwrap_or("");
        let mime = text("mime");
        let sha256 = text("sha256");
        let size = action.get("size").and_then(Value::as_u64).and_then(|s| usize::try_from(s).ok()).unwrap_or(usize::MAX);
        let offset = action.get("offset").and_then(Value::as_u64).and_then(|s| usize::try_from(s).ok()).unwrap_or(usize::MAX);
        if !MIMES.contains(&mime) {
            return Err(invalid("Only text and PNG pictures are shared."));
        }
        if size == 0 || size > CLIP_MAX {
            return Err(invalid("Clipboard entries are shared up to 8 MiB."));
        }
        if !crate::server::authority::is_hex_lower(sha256, 64) {
            return Err(invalid("Expected the entry's SHA-256."));
        }
        let piece = base64::engine::general_purpose::STANDARD
            .decode(text("data_b64"))
            .map_err(|_| invalid("The piece is not base64."))?;
        if piece.is_empty() || piece.len() > CHUNK {
            return Err(invalid("A piece holds 1 byte to 768 KiB."));
        }
        let complete = {
            let mut clipboard = self.clipboard.borrow_mut();
            if offset == 0 {
                clipboard.incoming = Some(Incoming { mime: mime.into(), size, sha256: sha256.into(), data: Vec::with_capacity(size) });
            }
            let incoming = clipboard
                .incoming
                .as_mut()
                .filter(|i| i.sha256 == sha256 && i.mime == mime && i.size == size && i.data.len() == offset)
                .ok_or_else(|| invalid("This piece does not follow the last one; send the entry again from the start."))?;
            if offset + piece.len() > size {
                clipboard.incoming = None;
                return Err(invalid("The pieces are larger than the entry."));
            }
            incoming.data.extend_from_slice(&piece);
            if incoming.data.len() < size {
                return Ok(json!({"enabled": true, "received": offset + piece.len(), "done": false}));
            }
            let incoming = clipboard.incoming.take().expect("incoming");
            if sha256_hex(&incoming.data) != incoming.sha256 {
                return Err(invalid("The entry arrived changed; send it again."));
            }
            // What arrives is known: the watcher will not send it back.
            clipboard.seen = Some(incoming.sha256.clone());
            Clip { mime: incoming.mime, data: incoming.data }
        };
        self.desktop.clipboard_write(&complete).await.map_err(|e| IbaraError::new("CAPABILITY_UNAVAILABLE", e.message, true))?;
        Ok(json!({"enabled": true, "received": size, "done": true}))
    }
}
