//! What the controller needs from the desktop and from the stream.
//!
//! The controller talks to these traits only, so tests drive it with fakes.
//! `live.rs` implements the desktop over `crate::desktop`; `stream.rs`
//! implements the stream over an `ibara-stream` child and its control socket.

use crate::desktop::atspi::{Element, ElementPage};
use crate::desktop::watch::DesktopEvent;
use crate::error::Result;
use serde_json::Value;
use std::pin::Pin;
use tokio::sync::broadcast;

pub use crate::desktop::{OutputChange, TypingCursor};
pub use crate::desktop::run::Cancel;

/// A boxed future on the controller's single thread.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A rectangle in logical desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// What identifies a window while it lives: Hyprland address, pid and class.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct WinKey {
    pub address: String,
    pub pid: i64,
    pub class: String,
}

/// One mapped window.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Win {
    pub address: String,
    pub pid: i64,
    pub class: String,
    pub title: String,
    pub focused: bool,
    /// Floating windows are how dialogs usually appear on Hyprland.
    pub floating: bool,
    pub workspace: String,
    pub rect: Rect,
}

impl Win {
    pub fn key(&self) -> WinKey {
        WinKey { address: self.address.clone(), pid: self.pid, class: self.class.clone() }
    }
}

/// A Chrome tab seen through the extension.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tab {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub focused: bool,
}

/// An encoded crop or screen image, with the logical region it shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub region: Rect,
}

impl Image {
    /// Map an image pixel to logical desktop coordinates.
    pub fn to_logical(&self, px: f64, py: f64) -> (f64, f64) {
        let sx = if self.width == 0 { 1.0 } else { self.region.width as f64 / self.width as f64 };
        let sy = if self.height == 0 { 1.0 } else { self.region.height as f64 / self.height as f64 };
        (self.region.x as f64 + px * sx, self.region.y as f64 + py * sy)
    }
}

/// An operator preview frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Preview {
    /// Encoded as `png` or `jpeg`, as asked.
    pub picture: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub display_revision: String,
}

/// One output as operators see it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DisplayInfo {
    pub display_id: String,
    pub label: String,
    pub display_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// One desktop effect. Every variant is dispatched by the desktop layer, which
/// tags errors with `execution_not_started` when nothing mutating ran.
#[derive(Debug, Clone)]
pub enum Effect {
    Launch { app_id: String },
    Focus(WinKey),
    Close(WinKey),
    ClickElement { surface: WinKey, element: Box<Element>, button: Button, double: bool },
    ClickPoint { x: f64, y: f64, surface: Option<WinKey>, button: Button, double: bool },
    /// `cursor`: whether the agent's named cursor glides to the field first.
    Type { surface: WinKey, cursor: TypingCursor, text: String },
    Key { surface: WinKey, combo: String },
    Scroll { at: Option<(f64, f64)>, dx: i32, dy: i32 },
}

/// What an effect reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Done {
    /// Pid of a launched program, when known.
    pub pid: Option<u32>,
    /// What else happened, in a few words (where a clicked page went).
    pub note: Option<String>,
    /// Where a click landed, as a fraction of the screen showing it.
    pub point: Option<(f64, f64)>,
}

/// What to capture.
#[derive(Debug, Clone)]
pub enum Capture {
    Surface(WinKey),
    Screen,
    /// A replay picture of a window: at most 640 pixels on its long edge.
    Replay(WinKey),
}

pub trait DesktopPort {
    /// A graphical session exists (`WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`).
    fn session_available(&self) -> bool;
    /// The session is unlocked and Hyprland answers.
    fn session_ready(&self) -> LocalFuture<'_, Result<()>>;
    fn windows(&self) -> LocalFuture<'_, Result<Vec<Win>>>;
    fn elements<'a>(&'a self, surface: &'a WinKey, query: Option<&'a str>, limit: u32, cursor: Option<u32>) -> LocalFuture<'a, Result<ElementPage>>;
    fn act<'a>(&'a self, effect: &'a Effect, cancel: &'a Cancel) -> LocalFuture<'a, Result<Done>>;
    fn capture<'a>(&'a self, target: &'a Capture, max_bytes: usize) -> LocalFuture<'a, Result<Image>>;
    /// The Chrome extension (ibara's page reader) is connected: a browser
    /// that has it is open.
    fn browser_connected(&self) -> bool;
    /// The page reader is installed for Chromium or Google Chrome on this
    /// computer (browser policy and native host), whether or not a browser is
    /// open. It connects when such a browser opens.
    fn browser_reader_installed(&self) -> bool;
    fn tabs(&self) -> LocalFuture<'_, Result<Vec<Tab>>>;
    /// One extension request (`tabs`, `observe`, `click`, `check`).
    fn browser_call<'a>(&'a self, op: &'a str, args: Value, effect: bool) -> LocalFuture<'a, Result<Value>>;
    fn browser_cancel(&self) -> LocalFuture<'_, Result<()>>;
    /// Close the page reader's connection and wait briefly for it to connect
    /// afresh; true once it has.
    fn browser_reconnect(&self) -> LocalFuture<'_, bool>;
    fn outputs(&self) -> LocalFuture<'_, Result<Vec<DisplayInfo>>>;
    /// `format` is `png` or `jpeg`.
    fn preview<'a>(&'a self, display_id: &'a str, quality: &'a str, format: &'a str) -> LocalFuture<'a, Result<Preview>>;
    /// Whether this computer streams live video: `{capable, reason}`.
    fn video_capability(&self) -> VideoCapability {
        VideoCapability { capable: false, reason: Some(crate::desktop::video::UNSUPPORTED.into()) }
    }
    /// Live video of `display` (else the first) at `width`×`height` from
    /// `cursor`, on `viewer`'s own encoder, started again whenever `access`
    /// (what the viewer may see) differs.
    fn observe_video<'a>(&'a self, _viewer: &'a str, _access: &'a str, _display: Option<&'a str>, _width: u32, _height: u32, _cursor: Option<u64>) -> LocalFuture<'a, Result<VideoChunk>> {
        Box::pin(async { Err(crate::desktop::video::unsupported(crate::desktop::video::UNSUPPORTED)) })
    }
    /// Stop every live video encoder now; the next read starts one again.
    fn stop_video(&self) {}
    /// A short phrase when memory is tight, e.g. `memory tight · close tabs`.
    fn memory_pressure(&self) -> Option<String>;
    /// Live window and focus events; `None` when there is no watcher.
    fn subscribe(&self) -> Option<broadcast::Receiver<DesktopEvent>>;
    /// Start the event watcher (once, from `Controller::start`).
    fn start_watch(&self) {}
    fn release_input(&self) -> LocalFuture<'_, Result<()>>;
    fn reset_input(&self) -> LocalFuture<'_, Result<()>>;
    fn set_idle_inhibited(&self, on: bool) -> LocalFuture<'_, Result<()>>;
    /// Whether a window shows a password field (replay keeps no picture of it).
    fn has_password_field<'a>(&'a self, _key: &'a WinKey) -> LocalFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    /// The agent whose named cursor this computer shows while it holds control.
    fn set_agent(&self, _label: Option<String>) {}
    /// The headless output change (§12.3) the outputs need, if any.
    fn output_change(&self) -> LocalFuture<'_, Result<Option<OutputChange>>>;
    /// Create or remove `IbaraVirtual`.
    fn apply_output_change(&self, change: OutputChange) -> LocalFuture<'_, Result<()>>;
    /// Give an existing `IbaraVirtual` the size in settings; whether it changed.
    fn resize_virtual(&self) -> LocalFuture<'_, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    /// Capability rows `{name, status, backend, last_tested_at?, reason?}`.
    fn capabilities(&self) -> LocalFuture<'_, Vec<Value>>;
    /// The parent of a process, for attributing a launched app's windows.
    fn parent_pid(&self, _pid: i64) -> Option<i64> {
        None
    }
    /// Version of the program behind a pid, for app notes.
    fn app_version(&self, _pid: i64) -> Option<String> {
        None
    }
    /// What the clipboard holds: text or a PNG picture of at most `limit`
    /// bytes. `None` when it is empty, holds anything else, or is larger.
    fn clipboard_read(&self, _limit: usize) -> LocalFuture<'_, Result<Option<Clip>>> {
        Box::pin(async { Err(crate::error::unavailable("This computer has no clipboard.")) })
    }
    /// Put `clip` on the clipboard.
    fn clipboard_write<'a>(&'a self, _clip: &'a Clip) -> LocalFuture<'a, Result<()>> {
        Box::pin(async { Err(crate::error::unavailable("This computer has no clipboard.")) })
    }
    /// A tick each time the clipboard changes, until the receiver is dropped.
    fn clipboard_watch(&self) -> Result<tokio::sync::mpsc::Receiver<()>> {
        Err(crate::error::unavailable("This computer has no clipboard."))
    }
    /// Set everything the clipboard holds aside, every type, and watch it
    /// until it is put back.
    fn clipboard_set_aside(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(async { Err(crate::error::unavailable("This computer has no clipboard.")) })
    }
    /// Put `text` on the clipboard for one paste, marked so clipboard
    /// histories skip it. Refused, with `reason: clipboard_copied`, once
    /// something else was copied since the clipboard was set aside.
    fn clipboard_paste_text<'a>(&'a self, _text: &'a str) -> LocalFuture<'a, Result<()>> {
        Box::pin(async { Err(crate::error::unavailable("This computer has no clipboard.")) })
    }
    /// Put the clipboard back, unless it was never changed or something else
    /// was copied meanwhile; one marked secret is left empty instead.
    fn clipboard_put_back(&self) -> LocalFuture<'_, Result<PutBack>> {
        Box::pin(async { Err(crate::error::unavailable("This computer has no clipboard.")) })
    }
}

pub use crate::desktop::clipboard::Clip;
pub use crate::desktop::selection::{PutBack, SavedClipboard};
pub use crate::desktop::video::{Chunk as VideoChunk, VideoCapability};

/// What `ibara-stream` reports about itself (`status`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamStatus {
    /// The authorization generation it admits (or last admitted).
    pub generation: u64,
    pub admission_closed: bool,
    /// No live ticket, no pending launch and no stream: it may stop.
    pub idle: bool,
    /// `vaapi` or `software`.
    pub encoder: String,
    /// `1280x720@15` when software encoding is capped.
    pub software_cap: Option<String>,
    /// Lowercase hex SHA-256 of its TLS certificate (DER), which viewers pin.
    pub server_cert_sha256: String,
    pub pid: u32,
    pub http_port: u16,
    pub https_port: u16,
}

/// A revoke's acknowledgement: admission is closed, streams ended and, when
/// `settled`, every key and button the viewer held has been released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settlement {
    pub fence_generation: u64,
    pub settled: bool,
}

/// `ibara-stream` for Take Control: started on demand, one viewer at a time,
/// admitted only with a one-time ticket bound to that viewer's certificate.
pub trait StreamPort {
    /// `ibara-stream` is installed here.
    fn available(&self) -> bool;
    /// Start it unless it runs; its status once it answers.
    fn start(&self) -> LocalFuture<'_, Result<StreamStatus>>;
    /// The running stream's status; `None` when none runs.
    fn status(&self) -> LocalFuture<'_, Result<Option<StreamStatus>>>;
    /// Admit `generation`, which must be above every generation used before.
    fn open(&self, generation: u64) -> LocalFuture<'_, Result<()>>;
    /// Let one launch through for the viewer presenting `client_cert_sha256`.
    fn issue_ticket<'a>(&'a self, generation: u64, ticket: &'a str, client_cert_sha256: &'a str, expires_in_ms: u64) -> LocalFuture<'a, Result<()>>;
    /// Close admission, end the streams and wait for held input to be
    /// released; `None` when no stream runs.
    fn revoke(&self) -> LocalFuture<'_, Result<Option<Settlement>>>;
    /// End the process (after a revoke).
    fn stop(&self) -> LocalFuture<'_, Result<()>>;
}
