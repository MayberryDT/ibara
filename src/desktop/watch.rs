//! Hyprland's event socket, published as [`DesktopEvent`]s (replaces
//! `ops/output-watch.mjs`).
//!
//! The task reads `$XDG_RUNTIME_DIR/hypr/<signature>/.socket2.sock` for the
//! instance `hyprctl -i 0` talks to, so events and queries describe the same
//! compositor. Lines are `EVENT>>DATA`. A line longer than 64 KiB drops the
//! connection; any error reconnects after 3 s; every (re)connect publishes
//! [`DesktopEvent::Resync`] and [`DesktopEvent::DisplayChanged`], because
//! events may have been missed.
//!
//! Events go to a small `broadcast` channel. Title changes are coalesced per
//! window (at most one every 500 ms, always the latest title) and focus is
//! published only when it moves, so terminal spinners cannot flood it. A
//! receiver that lags gets `RecvError::Lagged` and must re-read state.

use super::hyprland::Hyprland;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::broadcast;

/// Buffered events per receiver.
pub const EVENT_CAPACITY: usize = 64;
const MAX_LINE: usize = 64 * 1024;
const RECONNECT_AFTER: Duration = Duration::from_secs(3);
const TITLE_EVERY: Duration = Duration::from_millis(500);
const MAX_PENDING_TITLES: usize = 256;

/// A meaningful desktop change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopEvent {
    WindowOpened { address: String, workspace: String, class: String, title: String },
    WindowClosed { address: String },
    WindowTitle { address: String, title: String },
    /// Keyboard focus moved to this window, or to none.
    Focus { address: Option<String> },
    MonitorAdded { name: String },
    MonitorRemoved { name: String },
    /// Outputs may differ: reconcile the display (output-watch's trigger).
    DisplayChanged,
    /// The event stream (re)started; anything before it may have been missed.
    Resync,
}

/// `0x`-prefixed address, as `hyprctl` prints it (events omit the prefix).
fn address(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("0x") { raw.to_string() } else { format!("0x{raw}") }
}

/// Parse one `EVENT>>DATA` line into an event, if it is one ibara publishes.
pub fn parse_line(line: &str) -> Option<DesktopEvent> {
    let (event, data) = line.split_once(">>")?;
    match event {
        "openwindow" => {
            let mut parts = data.splitn(4, ',');
            let addr = parts.next()?;
            let workspace = parts.next()?.to_string();
            let class = parts.next()?.to_string();
            let title = parts.next().unwrap_or("").to_string();
            Some(DesktopEvent::WindowOpened { address: address(addr), workspace, class, title })
        }
        "closewindow" => Some(DesktopEvent::WindowClosed { address: address(data) }),
        "windowtitlev2" => {
            let (addr, title) = data.split_once(',')?;
            Some(DesktopEvent::WindowTitle { address: address(addr), title: title.to_string() })
        }
        "activewindowv2" => {
            let addr = data.trim().trim_matches(',');
            Some(DesktopEvent::Focus { address: (!addr.is_empty()).then(|| address(addr)) })
        }
        "monitoradded" => Some(DesktopEvent::MonitorAdded { name: data.to_string() }),
        "monitorremoved" => Some(DesktopEvent::MonitorRemoved { name: data.to_string() }),
        "configreloaded" => Some(DesktopEvent::DisplayChanged),
        _ => None,
    }
}

/// Stateful filter between parsed lines and the channel.
#[derive(Default)]
struct Publisher {
    focus: Option<Option<String>>,
    titles: HashMap<String, String>,
}

impl Publisher {
    /// Events to publish now for one parsed line.
    fn accept(&mut self, event: DesktopEvent) -> Vec<DesktopEvent> {
        match event {
            DesktopEvent::Focus { address } => {
                if self.focus.as_ref() == Some(&address) {
                    return Vec::new();
                }
                self.focus = Some(address.clone());
                vec![DesktopEvent::Focus { address }]
            }
            DesktopEvent::WindowTitle { address, title } => {
                if self.titles.len() < MAX_PENDING_TITLES || self.titles.contains_key(&address) {
                    self.titles.insert(address, title);
                }
                Vec::new()
            }
            DesktopEvent::WindowClosed { address } => {
                self.titles.remove(&address);
                vec![DesktopEvent::WindowClosed { address }]
            }
            DesktopEvent::MonitorAdded { name } => {
                vec![DesktopEvent::MonitorAdded { name }, DesktopEvent::DisplayChanged]
            }
            DesktopEvent::MonitorRemoved { name } => {
                vec![DesktopEvent::MonitorRemoved { name }, DesktopEvent::DisplayChanged]
            }
            other => vec![other],
        }
    }

    fn flush_titles(&mut self) -> Vec<DesktopEvent> {
        self.titles.drain().map(|(address, title)| DesktopEvent::WindowTitle { address, title }).collect()
    }
}

/// One of Hyprland's sockets (`.socket.sock` for requests, `.socket2.sock`
/// for events) of the instance `hypr` talks to.
pub(super) async fn socket_path(hypr: &Hyprland, runtime: &Path, name: &str) -> Option<PathBuf> {
    let signature = match hypr.instance_signature().await {
        Ok(signature) => signature,
        Err(_) => std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?,
    };
    Some(runtime.join("hypr").join(signature).join(name))
}

async fn session(stream: &mut UnixStream, tx: &broadcast::Sender<DesktopEvent>) {
    let mut publisher = Publisher::default();
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut tick = tokio::time::interval(TITLE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = tx.send(DesktopEvent::Resync);
    let _ = tx.send(DesktopEvent::DisplayChanged);
    loop {
        tokio::select! {
            read = stream.read(&mut chunk) => {
                let n = match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buffer.extend_from_slice(&chunk[..n]);
                while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&buffer[..end]).into_owned();
                    buffer.drain(..=end);
                    if let Some(event) = parse_line(&line) {
                        for event in publisher.accept(event) {
                            let _ = tx.send(event);
                        }
                    }
                }
                if buffer.len() > MAX_LINE {
                    // A missing newline or a flood cannot reserve unbounded
                    // memory; reconnecting resynchronises.
                    return;
                }
            }
            _ = tick.tick() => {
                for event in publisher.flush_titles() {
                    let _ = tx.send(event);
                }
            }
        }
    }
}

/// Run forever: connect, publish, reconnect after 3 s on any failure.
pub async fn run_watch(hypr: Hyprland, runtime_dir: PathBuf, tx: broadcast::Sender<DesktopEvent>) {
    loop {
        if let Some(path) = socket_path(&hypr, &runtime_dir, ".socket2.sock").await
            && let Ok(mut stream) = UnixStream::connect(&path).await
        {
            session(&mut stream, &tx).await;
        }
        tokio::time::sleep(RECONNECT_AFTER).await;
    }
}

/// Mirror of output-watch's loop, in-process: call `reconcile` once at start
/// and after every [`DesktopEvent::DisplayChanged`] (or missed events).
/// `reconcile` returns `true` to be retried in 5 s (deferred or failed); a
/// display change during a call also schedules another run.
pub async fn reconcile_on_display_change<F, Fut>(mut rx: broadcast::Receiver<DesktopEvent>, mut reconcile: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    use broadcast::error::{RecvError, TryRecvError};
    const RETRY: Duration = Duration::from_secs(5);
    let mut pending = true;
    loop {
        if pending {
            let retry = reconcile().await;
            let mut changed = false;
            loop {
                match rx.try_recv() {
                    Ok(DesktopEvent::DisplayChanged) | Err(TryRecvError::Lagged(_)) => changed = true,
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => return,
                }
            }
            pending = retry || changed;
        }
        if pending {
            let sleep = tokio::time::sleep(RETRY);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    received = rx.recv() => if let Err(RecvError::Closed) = received { return },
                }
            }
        } else {
            match rx.recv().await {
                Ok(DesktopEvent::DisplayChanged) | Err(RecvError::Lagged(_)) => pending = true,
                Ok(_) => {}
                Err(RecvError::Closed) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_with_commas_in_titles_and_normalised_addresses() {
        assert_eq!(
            parse_line("openwindow>>612017a016f0,2,org.xfce.mousepad,Save As, notes, draft"),
            Some(DesktopEvent::WindowOpened {
                address: "0x612017a016f0".into(),
                workspace: "2".into(),
                class: "org.xfce.mousepad".into(),
                title: "Save As, notes, draft".into(),
            })
        );
        assert_eq!(
            parse_line("windowtitlev2>>612017841930,π ⠧ a, b"),
            Some(DesktopEvent::WindowTitle { address: "0x612017841930".into(), title: "π ⠧ a, b".into() })
        );
        assert_eq!(parse_line("activewindowv2>>"), Some(DesktopEvent::Focus { address: None }));
        assert_eq!(parse_line("activewindow>>foot,title"), None);
        assert_eq!(parse_line("windowtitle>>612017a016f0"), None);
        assert_eq!(parse_line("garbage"), None);
    }

    #[test]
    fn focus_is_published_only_when_it_moves_and_titles_coalesce() {
        let mut p = Publisher::default();
        let focus = |a: &str| DesktopEvent::Focus { address: Some(a.into()) };
        assert_eq!(p.accept(focus("0x1")).len(), 1);
        assert!(p.accept(focus("0x1")).is_empty());
        assert_eq!(p.accept(focus("0x2")).len(), 1);
        for n in 0..10 {
            let title = format!("spinner {n}");
            assert!(p.accept(DesktopEvent::WindowTitle { address: "0x1".into(), title }).is_empty());
        }
        assert_eq!(p.flush_titles(), vec![DesktopEvent::WindowTitle { address: "0x1".into(), title: "spinner 9".into() }]);
        assert!(p.flush_titles().is_empty());
        let added = p.accept(DesktopEvent::MonitorAdded { name: "HDMI-A-1".into() });
        assert_eq!(added.last(), Some(&DesktopEvent::DisplayChanged));
    }

    #[tokio::test]
    async fn events_flow_from_the_socket_to_subscribers() {
        let dir = std::env::temp_dir().join(crate::ids::id("ibara-watch-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".socket2.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, mut rx) = broadcast::channel(EVENT_CAPACITY);
        let server = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut peer, _) = listener.accept().await.unwrap();
            peer.write_all(b"closewindow>>abc\nmonitorrem").await.unwrap();
            peer.write_all(b"oved>>HDMI-A-1\n").await.unwrap();
        });
        let mut stream = UnixStream::connect(&path).await.unwrap();
        session(&mut stream, &tx).await;
        server.await.unwrap();
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event);
        }
        assert_eq!(
            seen,
            vec![
                DesktopEvent::Resync,
                DesktopEvent::DisplayChanged,
                DesktopEvent::WindowClosed { address: "0xabc".into() },
                DesktopEvent::MonitorRemoved { name: "HDMI-A-1".into() },
                DesktopEvent::DisplayChanged,
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
