//! This session's clipboard through wl-clipboard: read it (`wl-paste`), set
//! it (`wl-copy`) and watch it change (`wl-paste --watch`). The target daemon
//! uses it while a person holds control; the console uses it for the computer
//! it controls. Only text and PNG pictures are read.

use super::run::{Cmd, run};
use crate::error::{IbaraError, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// One clipboard entry: [`TEXT`] or [`PNG`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub mime: String,
    pub data: Vec<u8>,
}

pub const TEXT: &str = "text/plain;charset=utf-8";
pub const PNG: &str = "image/png";
/// Text as programs offer it, best first.
const TEXT_TYPES: [&str; 4] = [TEXT, "UTF8_STRING", "text/plain", "STRING"];

fn missing() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "The clipboard needs wl-clipboard (wl-copy and wl-paste).", true)
}

/// What the clipboard holds: a PNG picture when one is offered, else text.
/// `None` when it is empty, holds anything else, or is larger than `limit`.
pub async fn read(limit: usize) -> Result<Option<Clip>> {
    let types = run(Cmd::new("wl-paste").arg("--list-types").timeout(Duration::from_secs(2))).await.map_err(|_| missing())?;
    if !types.success() {
        // "Nothing is copied".
        return Ok(None);
    }
    let text = types.stdout_text();
    let offered: Vec<&str> = text.lines().map(str::trim).collect();
    let (asked, mime) = if offered.contains(&PNG) {
        (PNG, PNG)
    } else if let Some(t) = TEXT_TYPES.iter().find(|t| offered.contains(t)) {
        (*t, TEXT)
    } else {
        return Ok(None);
    };
    let content = run(Cmd::new("wl-paste").args(["--no-newline", "--type", asked]).timeout(Duration::from_secs(5)).max_output(limit)).await;
    Ok(match content {
        Ok(out) if out.success() && !out.stdout.is_empty() => Some(Clip { mime: mime.into(), data: out.stdout }),
        // Too large, gone meanwhile, or unreadable: nothing to share.
        _ => None,
    })
}

/// Put `clip` on the clipboard. `wl-copy` stays behind to serve it, so only
/// its first process, which exits once it has the content, is waited for.
pub async fn write(clip: &Clip) -> Result<()> {
    let mut child = tokio::process::Command::new("wl-copy")
        .args(["--type", clip.mime.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| missing())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&clip.data).await?;
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => {
            let _ = child.kill().await;
            Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "The clipboard could not be set.", true))
        }
    }
}

/// A tick each time the clipboard changes (and possibly once at the start),
/// until the receiver is dropped; then the watcher ends.
pub fn watch() -> Result<mpsc::Receiver<()>> {
    // The watch command takes each new content on its stdin and discards it.
    let mut child = tokio::process::Command::new("wl-paste")
        .args(["--watch", "sh", "-c", "cat >/dev/null; echo"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| missing())?;
    let stdout = child.stdout.take().ok_or_else(missing)?;
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(_)) => {
                        // A full channel already holds a tick for the next read.
                        if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(()) {
                            break;
                        }
                    }
                    _ => break,
                },
                _ = tx.closed() => break,
            }
        }
        drop(child);
    });
    Ok(rx)
}
