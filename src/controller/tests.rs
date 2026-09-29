//! Failure cases first: replay, conflicting reuse, stopped sequences, unknown
//! outcomes, control conflicts, operator binding, lease expiry and restart.
//! The desktop and the stream are fakes; the journal and storage are real.

use super::ports::*;
use super::*;
use crate::desktop::atspi::ElementPage;
use crate::desktop::watch::DesktopEvent;
use crate::error::IbaraError;
use crate::ids::id;
use crate::storage::StorageOptions;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::{Notify, broadcast};

mod access;
mod approval;
mod approvals_off;
mod delivery;
mod home;
mod questions;
mod reconnect;
mod recovery;
mod stream;
mod waiting;

/// The viewer certificate `vesper` registered.
const CERT: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// A desktop that records effects and answers from scripted state.
#[derive(Default)]
struct FakeDesktop {
    session: Cell<bool>,
    windows: RefCell<Vec<Win>>,
    acts: RefCell<Vec<String>>,
    /// The next effect fails with this error.
    fail_next: RefCell<Option<IbaraError>>,
    /// The first effect whose description contains the text fails with this error.
    fail_on: RefCell<Option<(String, IbaraError)>>,
    /// Effects never finish (a crash mid-step).
    hang: Cell<bool>,
    /// The next effect runs until it is cancelled, then ends like this.
    until_cancelled: RefCell<Option<crate::error::Result<Done>>>,
    entered: Notify,
    /// A window that appears with the next effect.
    spawn: RefCell<Option<Win>>,
    /// A window that maps this long after the next launch returns: the
    /// program runs at once, its window comes later. Then the same window
    /// and when it maps.
    maps_late: RefCell<Option<(std::time::Duration, Win)>>,
    mapping: RefCell<Option<(tokio::time::Instant, Win)>>,
    /// Processes that run (their parent is init); any other pid has ended.
    running: RefCell<Vec<i64>>,
    /// The accessibility elements every window reports.
    page: RefCell<ElementPage>,
    /// Windows that someone else opens while the next effect runs.
    others: RefCell<Vec<Win>>,
    /// The pid a launch reports.
    launch_pid: Cell<Option<u32>>,
    /// Windows that close with the next effect.
    closing: RefCell<Vec<String>>,
    /// Windows that stay open when asked to close.
    ignores_close: RefCell<Vec<String>>,
    /// The extension's answer to each operation; with any, it is connected
    /// and shows one focused tab. An operation without an answer fails.
    extension: RefCell<HashMap<String, Value>>,
    /// The extension operations asked for, in order.
    extension_ops: RefCell<Vec<String>>,
    /// ibara's page reader is installed for a browser (policy and native
    /// host), whether or not a browser is open.
    reader_installed: Cell<bool>,
    /// The first window listing after the extension answers this operation
    /// fails with this error.
    windows_fail_after: RefCell<Option<(String, IbaraError)>>,
    /// The headless output change the outputs need, and the changes made.
    output_change: Cell<Option<OutputChange>>,
    output_changes: RefCell<Vec<OutputChange>>,
    /// The clipboard, its writes, and the watcher's sender while one runs.
    clipboard: RefCell<Option<Clip>>,
    clipboard_writes: RefCell<Vec<Clip>>,
    clipboard_watch: RefCell<Option<tokio::sync::mpsc::Sender<()>>>,
    /// Reading the clipboard waits for this, when set (control may end meanwhile).
    clipboard_read_hold: RefCell<Option<Rc<Notify>>>,
    /// A text field on the page, answering the extension's `field`.
    field: RefCell<FakeField>,
    /// The clipboard as the paste route sees it, every type; what was set
    /// aside while ibara pastes; whether ibara changed it and someone else
    /// copied since; and why setting it aside or putting it back fails.
    selection: RefCell<SavedClipboard>,
    set_aside: RefCell<Option<SavedClipboard>>,
    touched: Cell<bool>,
    copied: Cell<bool>,
    set_aside_fails: RefCell<Option<IbaraError>>,
    put_back_fails: RefCell<Option<IbaraError>>,
    /// The person copies this when an effect whose description contains the
    /// text is sent.
    person_copies: RefCell<Option<(String, SavedClipboard)>>,
    /// Extension operations (and `paste`, offering paste text) that wait
    /// here once, after telling `entered`.
    holds: RefCell<HashMap<String, Rc<Notify>>>,
    /// Releasing input waits here once, after telling `entered`: a
    /// settlement in progress.
    release_hold: RefCell<Option<Rc<Notify>>>,
    /// Live video reads that reached the desktop.
    video_reads: Cell<u32>,
}

/// A page's text field: whether a click gives it the keyboard focus, whether
/// it has it, its value, whether all of it is selected, and whether it
/// ignores pastes. Keys and text reach it only while it has the focus.
#[derive(Default)]
struct FakeField {
    present: bool,
    focusable: bool,
    focused: bool,
    value: String,
    selected: bool,
    ignores_paste: bool,
}

impl FakeDesktop {
    fn new() -> Rc<FakeDesktop> {
        let fake = FakeDesktop { session: Cell::new(true), ..Default::default() };
        fake.windows.replace(vec![Win {
            address: "0x1".into(),
            pid: 100,
            class: "mousepad".into(),
            title: "Untitled 1 - Mousepad".into(),
            focused: true,
            ..Default::default()
        }]);
        Rc::new(fake)
    }
    fn acts(&self) -> usize {
        self.acts.borrow().len()
    }
}

impl DesktopPort for FakeDesktop {
    fn session_available(&self) -> bool {
        self.session.get()
    }
    fn session_ready(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            if self.session.get() { Ok(()) } else { Err(IbaraError::new("SESSION_UNAVAILABLE", "No graphical session is available.", true)) }
        })
    }
    fn windows(&self) -> LocalFuture<'_, crate::error::Result<Vec<Win>>> {
        Box::pin(async move {
            let due = self.windows_fail_after.borrow().as_ref().is_some_and(|(op, _)| self.extension_ops.borrow().contains(op));
            if due && let Some((_, e)) = self.windows_fail_after.borrow_mut().take() {
                return Err(e);
            }
            let mapped = self.mapping.borrow().as_ref().is_some_and(|(at, _)| tokio::time::Instant::now() >= *at);
            if mapped && let Some((_, win)) = self.mapping.borrow_mut().take() {
                self.windows.borrow_mut().push(win);
            }
            Ok(self.windows.borrow().clone())
        })
    }
    fn elements<'a>(&'a self, _: &'a WinKey, _: Option<&'a str>, _: u32, _: Option<u32>) -> LocalFuture<'a, crate::error::Result<ElementPage>> {
        Box::pin(async move { Ok(self.page.borrow().clone()) })
    }
    fn act<'a>(&'a self, effect: &'a Effect, cancel: &'a Cancel) -> LocalFuture<'a, crate::error::Result<Done>> {
        Box::pin(async move {
            let said = format!("{effect:?}");
            let copies = self.person_copies.borrow().as_ref().is_some_and(|(when, _)| said.contains(when.as_str()));
            if copies && let Some((_, copy)) = self.person_copies.borrow_mut().take() {
                self.selection.replace(copy);
                self.copied.set(self.set_aside.borrow().is_some());
            }
            self.acts.borrow_mut().push(said.clone());
            self.page_input(effect);
            let until_cancelled = self.until_cancelled.borrow_mut().take();
            if let Some(end) = until_cancelled {
                self.entered.notify_one();
                cancel.cancelled().await;
                return end;
            }
            if let Some(win) = self.spawn.borrow_mut().take() {
                self.windows.borrow_mut().push(win);
            }
            if matches!(effect, Effect::Launch { .. })
                && let Some((after, win)) = self.maps_late.borrow_mut().take()
            {
                self.mapping.replace(Some((tokio::time::Instant::now() + after, win)));
            }
            let closing: Vec<String> = self.closing.borrow_mut().drain(..).collect();
            self.windows.borrow_mut().retain(|w| !closing.contains(&w.address));
            if let Effect::Close(key) = effect
                && !self.ignores_close.borrow().contains(&key.address)
            {
                self.windows.borrow_mut().retain(|w| w.address != key.address);
            }
            let others: Vec<Win> = self.others.borrow_mut().drain(..).collect();
            self.windows.borrow_mut().extend(others);
            if self.hang.get() {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            let due = self.fail_on.borrow().as_ref().is_some_and(|(when, _)| said.contains(when.as_str()));
            if due && let Some((_, e)) = self.fail_on.borrow_mut().take() {
                return Err(e);
            }
            match self.fail_next.borrow_mut().take() {
                Some(e) => Err(e),
                None if matches!(effect, Effect::Launch { .. }) => Ok(Done { pid: self.launch_pid.get(), ..Done::default() }),
                // The fake's screen is 1920x1080 at the origin.
                None => Ok(Done { point: match effect { Effect::ClickPoint { x, y, .. } => Some((x / 1920.0, y / 1080.0)), _ => None }, ..Done::default() }),
            }
        })
    }
    /// A window's picture is half its size on screen; the screen's is the
    /// 1920x1080 screen at half size.
    fn capture<'a>(&'a self, what: &'a Capture, _: usize) -> LocalFuture<'a, crate::error::Result<Image>> {
        Box::pin(async move {
            let region = match what {
                Capture::Surface(key) => self.windows.borrow().iter().find(|w| w.address == key.address).map_or(Rect::default(), |w| w.rect),
                Capture::Screen => Rect { x: 0, y: 0, width: 1920, height: 1080 },
                Capture::Replay(_) => return Ok(Image { bytes: vec![1, 2, 3], mime: "image/webp".into(), width: 1, height: 1, ..Default::default() }),
            };
            let (width, height) = ((region.width / 2).max(1) as u32, (region.height / 2).max(1) as u32);
            Ok(Image { bytes: vec![1, 2, 3], mime: "image/webp".into(), width, height, region })
        })
    }
    fn browser_connected(&self) -> bool {
        !self.extension.borrow().is_empty()
    }
    fn browser_reader_installed(&self) -> bool {
        self.reader_installed.get()
    }
    /// One focused tab, at the address and with the title the page
    /// observation gives.
    fn tabs(&self) -> LocalFuture<'_, crate::error::Result<Vec<Tab>>> {
        Box::pin(async move {
            let observed = self.extension.borrow().get("observe").cloned().unwrap_or(Value::Null);
            let url = observed["url"].as_str().unwrap_or("https://shop.example/").to_string();
            let title = observed["title"].as_str().unwrap_or("Shop").to_string();
            Ok(match self.browser_connected() {
                true => vec![Tab { id: 7, title, url, focused: true }],
                false => Vec::new(),
            })
        })
    }
    fn browser_call<'a>(&'a self, op: &'a str, args: Value, _: bool) -> LocalFuture<'a, crate::error::Result<Value>> {
        Box::pin(async move {
            self.extension_ops.borrow_mut().push(op.to_string());
            self.hold(op).await;
            let field = self.field.borrow();
            if op == "field" && field.present {
                // Like the page reader: whether it holds the text, never its value.
                return Ok(match args.get("expected").and_then(Value::as_str) {
                    Some(expected) => json!({ "focused": field.focused, "matches": field.value == expected }),
                    None => json!({ "focused": field.focused }),
                });
            }
            self.extension.borrow().get(op).cloned().ok_or_else(|| crate::error::unavailable("no extension"))
        })
    }
    fn browser_cancel(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move { Ok(()) })
    }
    fn outputs(&self) -> LocalFuture<'_, crate::error::Result<Vec<DisplayInfo>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn preview<'a>(&'a self, _: &'a str, _: &'a str, _: &'a str) -> LocalFuture<'a, crate::error::Result<Preview>> {
        Box::pin(async move { Err(crate::error::unavailable("no preview")) })
    }
    fn observe_video<'a>(&'a self, _: &'a str, _: &'a str, _: Option<&'a str>, width: u32, height: u32, cursor: Option<u64>) -> LocalFuture<'a, crate::error::Result<super::ports::VideoChunk>> {
        Box::pin(async move {
            self.video_reads.set(self.video_reads.get() + 1);
            Ok(super::ports::VideoChunk { cursor: cursor.unwrap_or(0) + 4, data: b"\x47abc".to_vec(), reset: false, ended: false, width, height })
        })
    }
    fn memory_pressure(&self) -> Option<String> {
        None
    }
    fn parent_pid(&self, pid: i64) -> Option<i64> {
        self.running.borrow().contains(&pid).then_some(1)
    }
    fn subscribe(&self) -> Option<broadcast::Receiver<DesktopEvent>> {
        None
    }
    fn release_input(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            let hold = self.release_hold.borrow_mut().take();
            if let Some(hold) = hold {
                self.entered.notify_one();
                hold.notified().await;
            }
            Ok(())
        })
    }
    fn reset_input(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move { Ok(()) })
    }
    fn set_idle_inhibited(&self, _: bool) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move { Ok(()) })
    }
    fn output_change(&self) -> LocalFuture<'_, crate::error::Result<Option<OutputChange>>> {
        Box::pin(async move { Ok(self.output_change.get()) })
    }
    fn apply_output_change(&self, change: OutputChange) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            self.output_changes.borrow_mut().push(change);
            self.output_change.set(None);
            Ok(())
        })
    }
    fn capabilities(&self) -> LocalFuture<'_, Vec<Value>> {
        Box::pin(async move { vec![json!({ "name": "native.hyprland", "status": "available", "backend": "fake" })] })
    }
    fn clipboard_read(&self, limit: usize) -> LocalFuture<'_, crate::error::Result<Option<Clip>>> {
        Box::pin(async move {
            let hold = self.clipboard_read_hold.borrow().clone();
            if let Some(hold) = hold {
                hold.notified().await;
            }
            Ok(self.clipboard.borrow().clone().filter(|c| c.data.len() <= limit))
        })
    }
    fn clipboard_write<'a>(&'a self, clip: &'a Clip) -> LocalFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            self.clipboard_writes.borrow_mut().push(clip.clone());
            self.copy(clip.clone());
            Ok(())
        })
    }
    fn clipboard_watch(&self) -> crate::error::Result<tokio::sync::mpsc::Receiver<()>> {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.clipboard_watch.replace(Some(tx));
        Ok(rx)
    }
    fn clipboard_set_aside(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            if let Some(e) = self.set_aside_fails.borrow_mut().take() {
                return Err(e);
            }
            self.set_aside.replace(Some(self.selection.borrow().clone()));
            self.touched.set(false);
            self.copied.set(false);
            Ok(())
        })
    }
    fn clipboard_paste_text<'a>(&'a self, text: &'a str) -> LocalFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            self.hold("paste").await;
            if self.copied.get() {
                return Err(crate::error::unavailable("Something was copied while ibara typed, so ibara stopped rather than paste over it.").with("reason", "clipboard_copied"));
            }
            let types = vec![(PASTED.to_string(), text.as_bytes().to_vec()), (SECRET.0.into(), SECRET.1.to_vec())];
            self.selection.replace(SavedClipboard(types));
            self.touched.set(true);
            Ok(())
        })
    }
    fn clipboard_put_back(&self) -> LocalFuture<'_, crate::error::Result<PutBack>> {
        Box::pin(async move {
            let saved = self.set_aside.borrow_mut().take().expect("set aside first");
            if let Some(e) = self.put_back_fails.borrow_mut().take() {
                return Err(e);
            }
            Ok(if self.copied.get() {
                PutBack::Copied
            } else if !self.touched.get() {
                PutBack::Unchanged
            } else if saved.0.iter().any(|(mime, data)| (mime.as_str(), data.as_slice()) == SECRET) {
                self.selection.replace(SavedClipboard::default());
                PutBack::Emptied
            } else {
                self.selection.replace(saved);
                PutBack::Restored
            })
        })
    }
}

/// The type a paste into a page reads, and a password manager's mark.
const PASTED: &str = "text/plain;charset=utf-8";
const SECRET: (&str, &[u8]) = ("x-kde-passwordManagerHint", b"secret");

impl FakeDesktop {
    /// Wait at `point` once, if a hold is set there.
    async fn hold(&self, point: &str) {
        let hold = self.holds.borrow_mut().remove(point);
        if let Some(hold) = hold {
            self.entered.notify_one();
            hold.notified().await;
        }
    }

    /// What an effect does to the page's field.
    fn page_input(&self, effect: &Effect) {
        let mut field = self.field.borrow_mut();
        if !field.present {
            return;
        }
        let insert = |f: &mut FakeField, text: &str| {
            if std::mem::take(&mut f.selected) {
                f.value.clear();
            }
            f.value.push_str(text);
        };
        match effect {
            Effect::ClickPoint { .. } => field.focused = field.focusable,
            Effect::Key { combo, .. } if field.focused && combo == "ctrl+a" => field.selected = true,
            Effect::Key { combo, .. } if field.focused && combo == "ctrl+v" && !field.ignores_paste => {
                let pasted = self.selection.borrow().0.iter().find(|(mime, _)| mime == PASTED).map(|(_, data)| String::from_utf8_lossy(data).into_owned());
                if let Some(text) = pasted {
                    insert(&mut field, &text);
                }
            }
            Effect::Type { text, .. } if field.focused => insert(&mut field, text),
            _ => {}
        }
    }

    /// Something is copied on this computer; a running watcher sees it.
    fn copy(&self, clip: Clip) {
        self.clipboard.replace(Some(clip));
        if let Some(tx) = self.clipboard_watch.borrow().as_ref() {
            let _ = tx.try_send(());
        }
    }
}

/// `ibara-stream` as the controller sees it: a process that starts on
/// demand, admits one generation, holds one ticket, and on revoke closes
/// admission, ends the stream and settles. Every call is logged in order; the
/// next `failures` calls fail, as a stream that hangs.
#[derive(Clone, Default)]
struct FakeStream(Rc<StreamState>);

#[derive(Default)]
struct StreamState {
    running: Cell<bool>,
    generation: Cell<u64>,
    open: Cell<bool>,
    /// The live ticket and the certificate it is for.
    ticket: RefCell<Option<(String, String)>>,
    /// A viewer is connected.
    viewing: Cell<bool>,
    /// Revoke answers that held keys were not released.
    unsettled: Cell<bool>,
    failures: Cell<u32>,
    calls: RefCell<Vec<String>>,
    revokes: Cell<u32>,
}

impl FakeStream {
    fn call(&self, what: String) -> crate::error::Result<()> {
        self.0.calls.borrow_mut().push(what);
        if self.0.failures.get() > 0 {
            self.0.failures.set(self.0.failures.get() - 1);
            return Err(crate::error::unavailable("Screen sharing did not answer in time."));
        }
        Ok(())
    }
    fn calls(&self) -> Vec<String> {
        self.0.calls.borrow().clone()
    }
    fn status_now(&self) -> StreamStatus {
        StreamStatus {
            generation: self.0.generation.get(),
            admission_closed: !self.0.open.get(),
            idle: self.0.ticket.borrow().is_none() && !self.0.viewing.get(),
            encoder: "vaapi".into(),
            software_cap: None,
            server_cert_sha256: "5e".repeat(32),
            pid: 4242,
            http_port: 47989,
            https_port: 47984,
        }
    }
    /// The viewer presents `ticket` with `cert`: admitted once.
    fn connect(&self, ticket: &str, cert: &str) -> bool {
        // A matching ticket is spent even when the certificate is wrong.
        let live = self.0.ticket.borrow_mut().take_if(|(t, _)| t == ticket);
        let admitted = self.0.open.get() && live.is_some_and(|(_, c)| c == cert);
        self.0.viewing.set(admitted);
        admitted
    }
}

impl StreamPort for FakeStream {
    fn available(&self) -> bool {
        true
    }
    fn start(&self) -> LocalFuture<'_, crate::error::Result<StreamStatus>> {
        Box::pin(async move {
            if !self.0.running.get() {
                self.call("start".into())?;
                self.0.running.set(true);
                self.0.generation.set(0);
                self.0.open.set(false);
            }
            Ok(self.status_now())
        })
    }
    fn status(&self) -> LocalFuture<'_, crate::error::Result<Option<StreamStatus>>> {
        Box::pin(async move { Ok(self.0.running.get().then(|| self.status_now())) })
    }
    fn open(&self, generation: u64) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            self.call(format!("open {generation}"))?;
            assert!(self.0.running.get() && generation > self.0.generation.get(), "a stale generation was opened");
            self.0.generation.set(generation);
            self.0.open.set(true);
            Ok(())
        })
    }
    fn issue_ticket<'a>(&'a self, generation: u64, ticket: &'a str, cert: &'a str, _: u64) -> LocalFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            self.call(format!("ticket {generation} {}", &cert[..6]))?;
            assert!(self.0.open.get() && generation == self.0.generation.get(), "a ticket for a generation not admitted");
            self.0.ticket.replace(Some((ticket.into(), cert.into())));
            Ok(())
        })
    }
    fn revoke(&self) -> LocalFuture<'_, crate::error::Result<Option<Settlement>>> {
        Box::pin(async move {
            self.0.revokes.set(self.0.revokes.get() + 1);
            self.call("revoke".into())?;
            if !self.0.running.get() {
                return Ok(None);
            }
            self.0.open.set(false);
            self.0.generation.set(self.0.generation.get() + 1);
            self.0.ticket.replace(None);
            self.0.viewing.set(false);
            Ok(Some(Settlement { fence_generation: self.0.generation.get(), settled: !self.0.unsettled.get() }))
        })
    }
    fn stop(&self) -> LocalFuture<'_, crate::error::Result<()>> {
        Box::pin(async move {
            if self.0.running.get() {
                self.call("stop".into())?;
            }
            self.0.running.set(false);
            self.0.ticket.replace(None);
            self.0.viewing.set(false);
            Ok(())
        })
    }
}

struct Rig {
    dir: PathBuf,
    clock: Arc<AtomicI64>,
    desktop: Rc<FakeDesktop>,
    stream: FakeStream,
    controller: Rc<Controller>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn open(dir: &Path, clock: &Arc<AtomicI64>, desktop: &Rc<FakeDesktop>, stream: bool) -> Rc<Controller> {
    open_with(dir, clock, desktop, stream.then(FakeStream::default))
}

/// `open` with a stream, and `vesper` paired (generation 3) with its viewer registered.
fn open_with(dir: &Path, clock: &Arc<AtomicI64>, desktop: &Rc<FakeDesktop>, stream: Option<FakeStream>) -> Rc<Controller> {
    // ibara's data and state folders are in the home folder, as on a computer.
    open_at(dir, dir, clock, desktop, stream)
}

/// What `vesper` calls itself: its endpoint id, as paired.
const VESPER_ENDPOINT: &str = "host_vesper_1234";

/// [`open_with`] for the desktop person whose home folder is `home`.
fn open_at(dir: &Path, home: &Path, clock: &Arc<AtomicI64>, desktop: &Rc<FakeDesktop>, stream: Option<FakeStream>) -> Rc<Controller> {
    let mut storage = StorageOptions::new(dir.join("data"));
    storage.state_dir = Some(dir.join("state"));
    storage.min_free_bytes = 0;
    storage.warn_free_bytes = 0;
    let storage = crate::storage::StorageService::open(storage).expect("storage");
    let port: Rc<dyn DesktopPort> = desktop.clone();
    let mut options = ControllerOptions::new(dir.join("state"), storage, port);
    let at = clock.clone();
    options.journal.now = Some(Arc::new(move || at.load(Ordering::SeqCst)));
    options.computer = ComputerIdentity { name: "Tulip1".into(), labels: vec!["tulip1".into()], user: Some("tulip1".into()) };
    options.home_dir = home.to_path_buf();
    options.operator_grants = Rc::new(|| {
        let mut grants = Map::new();
        grants.insert(
            "vesper".into(),
            json!({ "enabled": true, "generation": 3, "observe": true, "files": true, "viewer": { "cert_sha256": CERT, "generation": 3 },
                    "operator_endpoint_id": VESPER_ENDPOINT, "tailscale": { "node": "vesper-1.tail5d.ts.net", "host_name": "Vesper" } }),
        );
        grants
    });
    options.stream = stream.map(|s| Rc::new(s) as Rc<dyn StreamPort>);
    Rc::new(Controller::new(options).expect("controller"))
}

fn rig(stream: bool) -> Rig {
    let dir = std::env::temp_dir().join(id("ibara-controller-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = Arc::new(AtomicI64::new(1_790_000_000_000));
    let desktop = FakeDesktop::new();
    let fake = FakeStream::default();
    let controller = open_with(&dir, &clock, &desktop, stream.then(|| fake.clone()));
    Rig { dir, clock, desktop, stream: fake, controller }
}

async fn call(c: &Controller, tool: &str, args: Value) -> Value {
    c.call("vesper", "connection_a", "codex", tool, args, Cancel::new()).await.envelope
}

fn code(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap_or("")
}

async fn begin(c: &Controller) -> String {
    let env = call(c, "computer_begin", json!({ "goal": "Save a note in the editor", "request_id": id("req") })).await;
    assert_eq!(env["status"], "ok", "{env}");
    env["result"]["task_ref"].as_str().unwrap().to_string()
}

fn run<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(tokio::task::LocalSet::new().run_until(f))
}

#[test]
fn a_failed_begin_replays_its_original_error() {
    run(async {
        let rig = rig(false);
        rig.desktop.session.set(false);
        let args = json!({ "goal": "Write", "request_id": "req-1" });
        let first = call(&rig.controller, "computer_begin", args.clone()).await;
        assert_eq!(code(&first), "SESSION_UNAVAILABLE");
        rig.desktop.session.set(true);
        let replay = call(&rig.controller, "computer_begin", args).await;
        assert_eq!(code(&replay), "SESSION_UNAVAILABLE", "replay must return the original error, not begin again: {replay}");
        assert!(rig.controller.journal.get_active_lease().unwrap().is_none());
    });
}

#[test]
fn begin_with_changed_arguments_under_the_same_request_id_conflicts() {
    run(async {
        let rig = rig(false);
        let first = call(&rig.controller, "computer_begin", json!({ "goal": "Write", "request_id": "req-1" })).await;
        assert_eq!(first["status"], "ok");
        let changed = call(&rig.controller, "computer_begin", json!({ "goal": "Something else", "request_id": "req-1" })).await;
        assert_eq!(code(&changed), "REQUEST_CONFLICT");
        let same = call(&rig.controller, "computer_begin", json!({ "goal": "Write", "request_id": "req-1" })).await;
        assert_eq!(same["result"]["task_ref"], first["result"]["task_ref"], "replay returns the original result");
    });
}

#[test]
fn an_act_replay_returns_the_stored_result_and_dispatches_nothing() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+s" } });
        let first = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(first["result"]["steps"][0]["outcome"], "done", "{first}");
        assert_eq!(rig.desktop.acts(), 1);
        let replay = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(replay["result"]["steps"], first["result"]["steps"]);
        assert_eq!(rig.desktop.acts(), 1, "a replay never dispatches again");
        let mut changed = args;
        changed["action"]["keys"] = json!("ctrl+q");
        let conflict = call(&rig.controller, "computer_act", changed).await;
        assert_eq!(code(&conflict), "REQUEST_CONFLICT");
        assert_eq!(rig.desktop.acts(), 1);
    });
}

#[test]
fn a_step_after_an_unmet_expectation_does_not_run() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let env = call(
            &rig.controller,
            "computer_act",
            json!({ "task_ref": task, "request_id": "act-1", "steps": [
                { "action": { "kind": "key", "keys": "ctrl+s" }, "expect": { "kind": "dialog", "title": "Save As", "within_ms": 50 } },
                { "action": { "kind": "type", "text": "notes.txt" } },
            ] }),
        )
        .await;
        let steps = env["result"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["outcome"], "unmet", "{env}");
        assert_eq!(steps[1]["outcome"], "not_run");
        assert!(steps[1].get("op_ref").is_none(), "a step that never ran has no operation");
        assert_eq!(rig.desktop.acts(), 1, "only the first step was dispatched");
        assert!(env["result"]["frame"]["frame_ref"].as_str().is_some_and(|f| f.starts_with("frame_")), "a frame follows the last expectation");
    });
}

#[test]
fn an_unknown_outcome_stops_the_sequence_and_is_never_replayed() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.fail_next.replace(Some(IbaraError::new("OUTCOME_UNKNOWN", "ydotool failed after input may have begun", false)));
        let args = json!({ "task_ref": task, "request_id": "act-1", "steps": [
            { "action": { "kind": "key", "keys": "ctrl+s" } },
            { "action": { "kind": "key", "keys": "Return" } },
        ] });
        let first = call(&rig.controller, "computer_act", args.clone()).await;
        let steps = first["result"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["outcome"], "unknown", "{first}");
        assert_eq!(steps[1]["outcome"], "not_run");
        assert_eq!(rig.desktop.acts(), 1);
        assert!(first["situation"].as_str().unwrap().contains("1 unknown"), "{}", first["situation"]);
        let replay = call(&rig.controller, "computer_act", args).await;
        assert_eq!(replay["result"]["steps"][0]["outcome"], "unknown");
        assert_eq!(rig.desktop.acts(), 1, "an unknown step is never dispatched again");
        let op = steps[0]["op_ref"].as_str().unwrap();
        let status = call(&rig.controller, "computer_status", json!({ "ref": op })).await;
        assert_eq!(status["result"]["state"], "unknown");
    });
}

#[test]
fn a_refused_first_step_is_an_error_that_replays() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "click", "target": "e99" } });
        let first = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(code(&first), "STALE_TARGET", "{first}");
        let replay = call(&rig.controller, "computer_act", args).await;
        assert_eq!(code(&replay), "STALE_TARGET");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

#[test]
fn a_send_step_waits_for_a_person_and_runs_once_approved() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "Return" }, "effect": "send" });
        let held = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(held["status"], "pending", "{held}");
        assert_eq!(held["result"]["steps"][0]["outcome"], "not_run");
        let att = held["result"]["attention"].as_str().unwrap().to_string();
        assert_eq!(rig.desktop.acts(), 0, "a held step does not run");
        let still = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(still["status"], "pending");
        assert_eq!(rig.desktop.acts(), 0);
        rig.controller.admin(json!({ "op": "answer_attention", "att_ref": att, "answer": "approve" })).await.unwrap();
        let ran = call(&rig.controller, "computer_act", args.clone()).await;
        assert_eq!(ran["status"], "ok", "{ran}");
        assert_eq!(ran["result"]["steps"][0]["outcome"], "done");
        assert_eq!(rig.desktop.acts(), 1);
        call(&rig.controller, "computer_act", args).await;
        assert_eq!(rig.desktop.acts(), 1, "an approval covers that step once");
    });
}

/// The approval a held reply names, after checking it is open and asks
/// first as `class`.
fn held_as(c: &Controller, reply: &Value, class: &str) -> String {
    assert_eq!(reply["status"], "pending", "held for a person: {reply}");
    let att = reply["result"]["attention"].as_str().unwrap().to_string();
    let item = c.journal.get_attention(&att).unwrap().unwrap();
    assert_eq!((item.state.as_str(), item.details["effect"].as_str()), ("open", Some(class)), "{reply}");
    att
}

async fn answer(c: &Controller, att: &str, answer: &str) {
    c.admin(json!({ "op": "answer_attention", "att_ref": att, "answer": answer })).await.unwrap();
}

/// The same step under a new request, declared as less (or not at all).
/// Failure cases:
/// 1. while its approval is open it runs at once (the benchmark bypass),
///    or raises a second approval instead of being refused;
/// 2. after the approved request ran, a copy runs without asking;
/// 3. after a person declined it, a copy runs, or is refused for the rest
///    of the task instead of asking again;
/// 4. a copy asks as a milder class than the step was held under.
/// The approved request itself still runs once.
#[test]
fn a_held_step_sent_again_waits_for_its_answer_then_asks_again() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let step = |request_id: &str, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "key", "keys": "Return" } });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        let att = held_as(c, &call(c, "computer_act", step("act-1", Some("send"))).await, "send");

        for (request_id, effect) in [("act-2", None), ("act-3", Some("change")), ("act-4", Some("send"))] {
            let again = call(c, "computer_act", step(request_id, effect)).await;
            assert_eq!(code(&again), "PERMISSION_DENIED", "{again}");
            assert!(again["error"]["message"].as_str().unwrap().contains(&att), "the refusal names the open approval: {again}");
            assert!(again["error"]["next"].as_str().is_some_and(|n| n.contains("computer_wait")), "{again}");
        }
        assert_eq!(rig.desktop.acts(), 0, "nothing is sent while the approval is open");
        assert_eq!(c.journal.count_open_attention(Some(&task)).unwrap(), 1, "a refused copy asks nobody");

        answer(c, &att, "approve").await;
        let ran = call(c, "computer_act", step("act-1", Some("send"))).await;
        assert_eq!(ran["result"]["steps"][0]["outcome"], "done", "{ran}");
        assert_eq!(rig.desktop.acts(), 1);
        let mut asked = held_as(c, &call(c, "computer_act", step("act-5", None)).await, "send");
        assert_eq!(rig.desktop.acts(), 1, "an approval does not cover a later copy of the step");

        for request_id in ["act-6", "act-7"] {
            answer(c, &asked, "deny").await;
            asked = held_as(c, &call(c, "computer_act", step(request_id, None)).await, "send");
        }
        assert_eq!(rig.desktop.acts(), 1, "a declined step is never sent without a person");

        let exec = |request_id: &str, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "command": ["true"] });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        held_as(c, &call(c, "computer_exec", exec("exec-1", Some("send"))).await, "send");
        let again = call(c, "computer_exec", exec("exec-2", None)).await;
        assert_eq!(code(&again), "PERMISSION_DENIED", "a held command does not run under a new request either: {again}");
    });
}

/// A click is where it lands, not how. Failure case: after a person
/// declined a click, a double or right click a few pixels away runs as a
/// change.
#[test]
fn a_declined_click_asks_again_however_it_clicks_there() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        rig.desktop.windows.replace(vec![Win { rect: Rect { x: 0, y: 0, width: 1000, height: 800 }, ..win("0x1", 100, "mousepad", "Untitled 1 - Mousepad", true, false) }]);
        let task = begin(c).await;
        let pictured = call(c, "computer_observe", json!({ "task_ref": task, "view": "image" })).await;
        assert_eq!(pictured["status"], "ok", "{pictured}");
        let click = |request_id: &str, kind: &str, x: i32, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "action": { "kind": kind, "target": { "x": x, "y": 300 } } });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        let mut asked = held_as(c, &call(c, "computer_act", click("click-1", "click", 400, Some("send"))).await, "send");
        for (request_id, kind) in [("click-2", "double_click"), ("click-3", "right_click")] {
            answer(c, &asked, "deny").await;
            asked = held_as(c, &call(c, "computer_act", click(request_id, kind, 404, None)).await, "send");
        }
        assert_eq!(rig.desktop.acts(), 0);
        let away = call(c, "computer_act", click("click-4", "double_click", 480, None)).await;
        assert_eq!(away["status"], "ok", "a click elsewhere runs as its own class: {away}");
    });
}

/// Point coordinates are the picture's pixels, whatever frame came after
/// it. Failure cases: a point read from a picture is clicked as raw screen
/// pixels once a frame without a picture (an elements observe, the frame
/// an act returns) is the latest; with two windows pictured, a point for
/// the earlier picture is mapped through the later one.
#[test]
fn a_point_lands_where_the_picture_showed_it() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let editor = Win { rect: Rect { x: 100, y: 50, width: 800, height: 600 }, ..win("0x1", 100, "mousepad", "Untitled 1 - Mousepad", true, false) };
        let files = Win { rect: Rect { x: 1000, y: 200, width: 400, height: 300 }, ..win("0x2", 200, "nautilus", "Home", false, false) };
        rig.desktop.windows.replace(vec![editor, files]);
        let task = begin(c).await;
        let observe = |args: Value| {
            let mut args = args;
            args["task_ref"] = json!(task);
            async move {
                let env = call(c, "computer_observe", args).await;
                assert_eq!(env["status"], "ok", "{env}");
                env["result"]["frame"]["frame_ref"].as_str().unwrap().to_string()
            }
        };
        let click = |request_id: &str, target: Value| json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "click", "target": target } });
        let editor_picture = observe(json!({ "surface": "w1", "view": "image" })).await;
        observe(json!({ "surface": "w1", "view": "elements" })).await;

        // The editor's picture is 400x300 for 800x600 on screen at 100,50.
        let env = call(c, "computer_act", click("act-1", json!({ "x": 200, "y": 100 }))).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let effect = env["result"]["steps"][0]["effect"].as_str().unwrap();
        assert!(effect.contains("500,250") && effect.contains(&editor_picture), "the effect names the screen point and the picture: {effect}");
        assert!(rig.desktop.acts.borrow()[0].contains("x: 500.0, y: 250.0"), "{:?}", rig.desktop.acts.borrow());

        // The act returned a frame without a picture; the picture still holds.
        let env = call(c, "computer_act", click("act-2", json!({ "x": 10, "y": 20 }))).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        assert!(rig.desktop.acts.borrow()[1].contains("x: 120.0, y: 90.0"), "{:?}", rig.desktop.acts.borrow());

        // Without a frame the point is the latest picture's; with one, that picture's.
        observe(json!({ "surface": "w2", "view": "image" })).await;
        call(c, "computer_act", click("act-3", json!({ "x": 10, "y": 20 }))).await;
        assert!(rig.desktop.acts.borrow()[2].contains("x: 1020.0, y: 240.0"), "{:?}", rig.desktop.acts.borrow());
        call(c, "computer_act", click("act-4", json!({ "x": 10, "y": 20, "frame": editor_picture }))).await;
        assert!(rig.desktop.acts.borrow()[3].contains("x: 120.0, y: 90.0"), "{:?}", rig.desktop.acts.borrow());
        assert_eq!(rig.desktop.acts(), 4);
    });
}

/// A point needs a picture to mean anything. Failure case: with no picture
/// taken in the task, the point is clicked as screen pixels and reported done.
#[test]
fn a_point_without_a_picture_is_refused_and_nothing_is_sent() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let elements = call(c, "computer_observe", json!({ "task_ref": task, "view": "elements" })).await;
        assert_eq!(elements["status"], "ok", "{elements}");
        let env = call(c, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "click", "target": { "x": 400, "y": 300 } } })).await;
        assert_eq!(code(&env), "STALE_TARGET", "{env}");
        assert_eq!(env["error"]["execution_not_started"], true, "{env}");
        assert!(env["error"]["next"].as_str().is_some_and(|n| n.contains("\"image\"")), "the refusal says to take a picture: {env}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

/// A picture maps to the screen only where its window still is. Failure
/// case: after the window moved or was resized, a point from the old
/// picture clicks where the window no longer shows that spot.
#[test]
fn a_point_in_a_picture_of_a_window_that_moved_since_is_refused() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let at = Rect { x: 100, y: 50, width: 800, height: 600 };
        rig.desktop.windows.replace(vec![Win { rect: at, ..win("0x1", 100, "mousepad", "Untitled 1 - Mousepad", true, false) }]);
        let task = begin(c).await;
        for (request_id, now) in [("act-1", Rect { x: 300, ..at }), ("act-2", Rect { x: 300, width: 700, ..at })] {
            let pictured = call(c, "computer_observe", json!({ "task_ref": task, "view": "image" })).await;
            assert_eq!(pictured["status"], "ok", "{pictured}");
            rig.desktop.windows.borrow_mut()[0].rect = now;
            let env = call(c, "computer_act", json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "click", "target": { "x": 200, "y": 100 } } })).await;
            assert_eq!(code(&env), "STALE_TARGET", "{env}");
            assert_eq!(env["error"]["execution_not_started"], true, "{env}");
            assert!(env["error"]["next"].as_str().is_some_and(|n| n.contains("\"image\"")), "{env}");
            assert_eq!(rig.desktop.acts(), 0);
        }
    });
}

/// ibara holds what it sees submit a page's form, though the agent declared
/// nothing: Return in a form's field, from either tool, and a click on a
/// submit button. Return where the page says it submits nothing (the address
/// bar, a field outside any form) is not held.
#[test]
fn a_press_the_page_says_submits_a_form_waits_for_approval() {
    run(async {
        let rig = browser_rig();
        let nodes = json!([
            { "role": "button", "name": "Create account", "actions": ["click"], "states": ["enabled", "submits"], "token": "t1" },
            { "role": "textbox", "name": "Email", "actions": ["click", "fill"], "states": ["enabled", "enter_submits"], "token": "t2" },
        ]);
        let observed = json!({ "capture": "c1", "documentId": "d1", "url": "https://shop.example/signup", "title": "Sign up", "count": 2, "nodes": nodes });
        rig.desktop.extension.borrow_mut().insert("observe".into(), observed);
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": true, "hit": true }));
        rig.desktop.extension.borrow_mut().insert("keys".into(), json!({ "page": true, "submits": true }));
        let task = observe_page(&rig.controller).await;
        let c = &rig.controller;

        let desktop = call(c, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "Return" } })).await;
        assert_eq!(desktop["status"], "pending", "{desktop}");
        let page = call(c, "browser_act", json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "key", "keys": "ctrl+Return" } })).await;
        assert_eq!(page["status"], "pending", "{page}");
        let click = call(c, "browser_act", click_next(&task, "act-3")).await;
        assert_eq!(click["status"], "pending", "{click}");
        let typed = call(c, "browser_act", json!({ "task_ref": task, "request_id": "act-4", "action": { "kind": "type", "target": "b2", "text": "zoe@example.com\n" } })).await;
        assert_eq!(typed["status"], "pending", "{typed}");
        assert_eq!(rig.desktop.acts(), 0, "nothing that submits the form is sent without approval");

        rig.desktop.extension.borrow_mut().insert("keys".into(), json!({ "page": true, "submits": false }));
        let plain = call(c, "computer_act", json!({ "task_ref": task, "request_id": "act-5", "action": { "kind": "key", "keys": "shift+Return" } })).await;
        assert_eq!(plain["status"], "ok", "Return that submits nothing runs at once: {plain}");
        assert_eq!(rig.desktop.acts(), 1);
    });
}

/// A held command is the same action whichever way its folder is named: no
/// cwd, `.`, `./` and the workspace's absolute path are one folder. Failure
/// cases: sent again with another spelling of the folder and no effect, it
/// runs as a change while its approval is open, or without a person after
/// one declined it.
#[test]
fn a_held_command_is_the_same_whichever_way_its_folder_is_named() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let exec = |request_id: &str, cwd: Option<&str>, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "command": ["rm", "-rf", "out"] });
            if let Some(cwd) = cwd {
                args["cwd"] = json!(cwd);
            }
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        let out = c.storage.workspace(&task, false).unwrap().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let att = held_as(c, &call(c, "computer_exec", exec("exec-1", None, Some("destructive"))).await, "destructive");
        let workspace = c.storage.workspace(&task, false).unwrap().to_string_lossy().into_owned();
        let spellings = [".", "./", workspace.as_str(), &format!("{workspace}/")];
        for (n, cwd) in spellings.iter().enumerate() {
            let again = call(c, "computer_exec", exec(&format!("exec-open-{n}"), Some(cwd), None)).await;
            assert_eq!(code(&again), "PERMISSION_DENIED", "cwd {cwd:?} names the held command's folder: {again}");
            assert!(again["error"]["message"].as_str().unwrap().contains(&att), "{again}");
        }
        let mut asked = att;
        for (n, cwd) in spellings.iter().enumerate() {
            answer(c, &asked, "deny").await;
            let again = call(c, "computer_exec", exec(&format!("exec-declined-{n}"), Some(cwd), Some("change"))).await;
            asked = held_as(c, &again, "destructive");
        }
        assert!(out.is_dir(), "the command never ran");
    });
}

/// The guard reads every approval of the task, however many other items the
/// task raised. Failure case: 500 checkpoint questions push a declined step
/// out of what the guard reads, and the step then runs as a change.
#[test]
fn a_declined_step_asks_again_after_many_questions() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let exec = |request_id: &str, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "command": ["rm", "-rf", "out"] });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        let out = c.storage.workspace(&task, false).unwrap().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let att = held_as(c, &call(c, "computer_exec", exec("exec-1", Some("destructive"))).await, "destructive");
        answer(c, &att, "deny").await;
        for n in 0..500 {
            // Within the per-connection rate: 80 calls in 10 s.
            rig.clock.fetch_add(150, Ordering::SeqCst);
            let asked = call(c, "computer_checkpoint", json!({ "task_ref": task, "ask": { "question": format!("Question {n}?") } })).await;
            assert_eq!(asked["status"], "pending", "{asked}");
        }
        held_as(c, &call(c, "computer_exec", exec("exec-2", None)).await, "destructive");
        assert!(out.is_dir(), "the command never ran");
    });
}

/// A key is the same action in the same window on the same page (without
/// its query); when ibara cannot tell which page the window shows, on any
/// page. Failure cases:
/// 1. the declined Return, sent again on its page (another query), runs,
///    or is refused instead of asking again;
/// 2. a declined or approved Return on one page makes Return on another
///    page of the window ask;
/// 3. a copy of an approved Return runs on its page without asking;
/// 4. with the page reader's tab in another browser window, the declined
///    Return runs because that tab shows another page.
#[test]
fn a_return_a_person_answered_asks_again_on_its_page() {
    run(async {
        let rig = browser_rig();
        let reader = |url: &str, title: &str| {
            let mut extension = rig.desktop.extension.borrow_mut();
            let observed = extension.get_mut("observe").unwrap();
            observed["url"] = json!(url);
            observed["title"] = json!(title);
            extension.insert("keys".into(), json!({ "page": true, "submits": false }));
        };
        let task = observe_page(&rig.controller).await;
        let c = &rig.controller;
        let enter = |request_id: &str, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "key", "keys": "Return" } });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };

        reader("https://shop.example/signup?step=1", "Shop");
        let declined = held_as(c, &call(c, "computer_act", enter("act-1", Some("send"))).await, "send");
        answer(c, &declined, "deny").await;
        reader("https://shop.example/signup?step=2", "Shop");
        let again = held_as(c, &call(c, "browser_act", enter("act-2", None)).await, "send");
        answer(c, &again, "deny").await;
        assert_eq!(rig.desktop.acts(), 0);

        reader("https://shop.example/search", "Shop");
        let other_page = call(c, "computer_act", enter("act-3", None)).await;
        assert_eq!(other_page["status"], "ok", "another page: {other_page}");
        assert_eq!(rig.desktop.acts(), 1);

        reader("https://shop.example/checkout", "Shop");
        let approved = held_as(c, &call(c, "computer_act", enter("act-4", Some("send"))).await, "send");
        answer(c, &approved, "approve").await;
        let ran = call(c, "computer_act", enter("act-4", Some("send"))).await;
        assert_eq!(ran["result"]["steps"][0]["outcome"], "done", "{ran}");
        assert_eq!(rig.desktop.acts(), 2);
        let copy = held_as(c, &call(c, "browser_act", enter("act-5", None)).await, "send");
        answer(c, &copy, "deny").await;
        reader("https://shop.example/search", "Shop");
        let later = call(c, "browser_act", enter("act-6", None)).await;
        assert_eq!(later["status"], "ok", "an approved Return does not make Return on other pages ask: {later}");
        assert_eq!(rig.desktop.acts(), 3);

        // The reader's tab is in another browser window: which page this
        // one shows is not known.
        rig.desktop.windows.borrow_mut().push(win("0xa", 300, "chromium", "Mail - Chromium", false, false));
        reader("https://mail.example/inbox", "Mail");
        held_as(c, &call(c, "computer_act", enter("act-7", None)).await, "send");
        assert_eq!(rig.desktop.acts(), 3, "a Return a person answered is not sent without one");
    });
}

/// In a window without a page, a key is the same action in the same window,
/// whatever was typed or pressed before it. Failure cases:
/// 1. after a person declined Return, another key (End, Tab) or a paste
///    first lets the same Return run without a person;
/// 2. the declined Return is refused for the rest of the task, so the
///    terminal is lost, instead of asking each time;
/// 3. Return in another window, where nothing was held, asks or is refused.
#[test]
fn a_declined_return_in_a_terminal_asks_again_after_any_key() {
    run(async {
        let rig = rig(false);
        rig.desktop.windows.replace(vec![win("0x5", 400, "foot", "~ - Foot", true, false)]);
        let task = begin(&rig.controller).await;
        let c = &rig.controller;
        let step = |request_id: &str, action: Value, effect: Option<&str>| {
            let mut args = json!({ "task_ref": task, "request_id": request_id, "action": action });
            if let Some(effect) = effect {
                args["effect"] = json!(effect);
            }
            args
        };
        let key = |keys: &str| json!({ "kind": "key", "keys": keys });

        let typed = call(c, "computer_act", step("type-1", json!({ "kind": "type", "text": "rm -rf x" }), None)).await;
        assert_eq!(typed["status"], "ok", "{typed}");
        let mut asked = held_as(c, &call(c, "computer_act", step("enter-1", key("Return"), Some("destructive"))).await, "destructive");
        for (n, first) in ["End", "Tab", "ctrl+shift+v"].into_iter().enumerate() {
            answer(c, &asked, "deny").await;
            let pressed = call(c, "computer_act", step(&format!("first-{n}"), key(first), None)).await;
            assert_eq!(pressed["status"], "ok", "{first}: {pressed}");
            asked = held_as(c, &call(c, "computer_act", step(&format!("enter-again-{n}"), key("Return"), None)).await, "destructive");
        }
        assert_eq!(rig.desktop.acts(), 4, "the typing and the three keys ran, no Return");

        rig.desktop.windows.replace(vec![win("0x5", 400, "foot", "~ - Foot", false, false), win("0x6", 401, "foot", "~/notes - Foot", true, false)]);
        let other = call(c, "computer_act", step("enter-other", key("Return"), None)).await;
        assert_eq!(other["status"], "ok", "Return in another window runs as its own class: {other}");
        assert_eq!(rig.desktop.acts(), 5);
    });
}

#[test]
fn a_typed_check_that_cannot_pass_here_is_refused_at_begin() {
    run(async {
        let rig = rig(false);
        let url = call(
            &rig.controller,
            "computer_begin",
            json!({ "goal": "Open", "request_id": "r1", "checks": [{ "id": "c1", "description": "on the page", "check": { "kind": "url", "contains": "example" } }] }),
        )
        .await;
        assert_eq!(code(&url), "CAPABILITY_UNAVAILABLE", "{url}");
        assert_eq!(
            url["error"]["message"],
            "checks[0].check: url checks need ibara's page reader in Chromium or Google Chrome, and it is not installed on this computer; use another check or your assessment",
            "{url}"
        );
        let status = call(&rig.controller, "computer_status", json!({})).await;
        assert!(status["result"]["computers"][0]["capabilities"].as_str().unwrap().contains("browser page reader (not installed)"), "{status}");
        let outside = call(
            &rig.controller,
            "computer_begin",
            json!({ "goal": "Save", "request_id": "r2", "checks": [{ "id": "c1", "description": "saved", "check": { "kind": "file_exists", "path": "/etc/passwd" } }] }),
        )
        .await;
        assert_eq!(code(&outside), "INVALID_ARGUMENT");
        assert!(outside["error"]["message"].as_str().unwrap().contains("checks[0].check.path"), "{outside}");
    });
}

#[test]
fn a_url_check_waits_for_the_browser_when_the_page_reader_is_installed() {
    run(async {
        let rig = rig(false);
        rig.desktop.reader_installed.set(true);
        let begin_url = |request: &str| {
            json!({ "goal": "Open the shop", "request_id": request, "checks": [{ "id": "c1", "description": "on the shop", "check": { "kind": "url", "contains": "shop.example" } }] })
        };
        let status = call(&rig.controller, "computer_status", json!({})).await;
        let line = status["result"]["computers"][0]["capabilities"].as_str().unwrap().to_string();
        assert!(line.contains("no browser open yet") && !line.contains("browser_semantics") && !line.contains("not installed"), "{status}");

        // Never opened: the check is unmet at finish, and says why.
        let begun = call(&rig.controller, "computer_begin", begin_url("r1")).await;
        assert_eq!(begun["status"], "ok", "the browser is not open yet, which is how a browser task starts: {begun}");
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "complete", "summary": "done" })).await;
        let check = &finish["result"]["checks"][0];
        assert_eq!(check["state"], "unmet", "{finish}");
        assert_eq!(check["detail"], "no browser with ibara's page reader was open, so no tab's address contains shop.example", "{finish}");
        assert_eq!(finish["result"]["complete"], false, "{finish}");

        // Opened during the task: the page reader connects and the check is met.
        let begun = call(&rig.controller, "computer_begin", begin_url("r2")).await;
        assert_eq!(begun["status"], "ok", "{begun}");
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        rig.desktop.extension.borrow_mut().insert("tabs".into(), json!({}));
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f2", "outcome": "complete", "summary": "done" })).await;
        let check = &finish["result"]["checks"][0];
        assert_eq!(check["state"], "met", "{finish}");
        assert_eq!(finish["result"]["complete"], true, "{finish}");
        let status = call(&rig.controller, "computer_status", json!({})).await;
        assert_eq!(status["result"]["computers"][0]["capabilities"], "all available", "{status}");
    });
}

#[test]
fn control_expires_after_five_idle_minutes() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.clock.fetch_add(IDLE_EXPIRY_MS - 1000, Ordering::SeqCst);
        rig.controller.heartbeat("vesper", "connection_a", true).await.unwrap();
        rig.clock.fetch_add(IDLE_EXPIRY_MS - 1000, Ordering::SeqCst);
        let alive = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        assert_eq!(alive["status"], "ok", "a heartbeat extends control: {alive}");
        rig.clock.fetch_add(IDLE_EXPIRY_MS + 1, Ordering::SeqCst);
        let expired = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        assert_eq!(code(&expired), "LEASE_EXPIRED", "{expired}");
        let lease = rig.controller.journal.list_leases_for_task(&task).unwrap();
        assert_eq!(lease[0].reason.as_deref(), Some("idle_expired"));
    });
}

#[test]
fn a_restart_makes_a_running_step_unknown() {
    let dir = std::env::temp_dir().join(id("ibara-controller-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = Arc::new(AtomicI64::new(1_790_000_000_000));
    let desktop = FakeDesktop::new();
    let controller = open(&dir, &clock, &desktop, false);
    let (task, op) = run(async {
        let task = begin(&controller).await;
        desktop.hang.set(true);
        let act = call(&controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+s" } }));
        tokio::select! {
            _ = act => panic!("the step must not finish"),
            _ = desktop.entered.notified() => {}
        }
        let op = controller.journal.get_mutation_operation("vesper", &task, "act-1").unwrap().unwrap();
        assert_eq!(op.receipt["execution"], "running", "intent is written before the effect");
        (task, op.operation_ref)
    });
    drop(controller);
    run(async {
        let restarted = open(&dir, &clock, &desktop, false);
        let op = restarted.journal.get_operation_by_ref(&op).unwrap().unwrap();
        assert_eq!(op.receipt["execution"], "unknown");
        assert_eq!(op.receipt["error"]["code"], "OUTCOME_UNKNOWN");
        let status = restarted.call("vesper", "connection_b", "codex", "computer_status", json!({ "ref": op.operation_ref }), Cancel::new()).await.envelope;
        assert_eq!(status["result"]["state"], "unknown");
        let old = restarted
            .call("vesper", "connection_a", "codex", "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+s" } }), Cancel::new())
            .await
            .envelope;
        assert_eq!(code(&old), "LEASE_EXPIRED", "the old task's control ended with the restart: {old}");
        assert!(old["error"]["message"].as_str().unwrap().contains("ibara restarted"), "{old}");
        assert_eq!(desktop.acts(), 1);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn operator_action(c: &Controller, op: &str) -> Value {
    json!({ "op": op, "endpoint_id": c.endpoint_id(), "controller_epoch": c.epoch(), "expected_authorization_generation": 3 })
}

#[test]
fn operator_calls_bound_to_another_epoch_generation_or_endpoint_are_denied() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let mut wrong_epoch = operator_action(c, "status");
        wrong_epoch["controller_epoch"] = json!("epoch_old");
        let mut wrong_generation = operator_action(c, "status");
        wrong_generation["expected_authorization_generation"] = json!(2);
        let mut wrong_endpoint = operator_action(c, "status");
        wrong_endpoint["endpoint_id"] = json!("ibara_other");
        for action in [wrong_epoch, wrong_generation, wrong_endpoint] {
            let err = c.operator_call("vesper", action.clone()).await.unwrap_err();
            assert_eq!(err.code, "PERMISSION_DENIED", "{action}");
        }
        let unknown = c.operator_call("hazel", operator_action(c, "status")).await.unwrap_err();
        assert_eq!(unknown.code, "PERMISSION_DENIED");
        let ok = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        assert_eq!(ok["controller_epoch"], json!(c.epoch()));
        assert_eq!(ok["owner"], "none");
    });
}

#[test]
fn take_control_against_a_stale_owner_or_revision_conflicts() {
    run(async {
        let rig = rig(true);
        let c = &rig.controller;
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        let (owner, revision) = (status["owner"].as_str().unwrap().to_string(), status["ownership_revision"].as_str().unwrap().to_string());
        let mut stale_owner = operator_action(c, "take_control");
        stale_owner["expected_owner"] = json!("human");
        stale_owner["expected_ownership_revision"] = json!(revision);
        assert_eq!(c.operator_call("vesper", stale_owner).await.unwrap_err().code, "REQUEST_CONFLICT");
        let mut stale_revision = operator_action(c, "take_control");
        stale_revision["expected_owner"] = json!(owner);
        stale_revision["expected_ownership_revision"] = json!(format!("{}:9:none", c.epoch()));
        assert_eq!(c.operator_call("vesper", stale_revision).await.unwrap_err().code, "REQUEST_CONFLICT");
        let mut current = operator_action(c, "take_control");
        current["expected_owner"] = json!(owner);
        current["expected_ownership_revision"] = json!(revision);
        let taken = c.operator_call("vesper", current).await.unwrap();
        assert_eq!(taken["owner"], "operator:vesper");
        let begin = call(c, "computer_begin", json!({ "goal": "Write", "request_id": "r1" })).await;
        assert_eq!(code(&begin), "HUMAN_CONTROL", "an agent cannot begin while a person holds control");
    });
}

#[test]
fn a_paused_or_unsettled_computer_gets_its_headless_output_but_keeps_it_until_settled() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let control = |patch: crate::store::ControlPatch| c.journal.set_control(patch).unwrap();
        let reconcile = || c.admin(json!({ "op": "reconcile_display" }));
        // A computer that started without a display: start pauses (paused and
        // human_control), and its viewer, which needs an output, faulted.
        assert_eq!(c.admin(json!({ "op": "pause" })).await.unwrap()["ok"], true);
        control(crate::store::ControlPatch { unsettled: Some(true), ..Default::default() });
        let state = c.journal.get_control().unwrap();
        assert!(state.paused && state.human_control && state.unsettled, "{state:?}");
        rig.desktop.output_change.set(Some(OutputChange::Create));
        assert_eq!(reconcile().await.unwrap(), json!({ "deferred": false, "changed": true }));
        assert_eq!(*rig.desktop.output_changes.borrow(), [OutputChange::Create]);

        rig.desktop.output_change.set(Some(OutputChange::Remove));
        assert_eq!(reconcile().await.unwrap(), json!({ "deferred": true }), "a display plugged in meanwhile waits");
        control(crate::store::ControlPatch { unsettled: Some(false), ..Default::default() });
        assert_eq!(reconcile().await.unwrap(), json!({ "deferred": true }), "and waits while paused");
        assert_eq!(rig.desktop.output_changes.borrow().len(), 1);

        // A person holding control through the viewer: nothing changes.
        c.viewer_state.borrow_mut().owner = Some("vesper".into());
        rig.desktop.output_change.set(Some(OutputChange::Create));
        assert_eq!(reconcile().await.unwrap(), json!({ "deferred": true }), "nothing changes while a person holds the viewer");
        c.viewer_state.borrow_mut().owner = None;

        control(crate::store::ControlPatch { paused: Some(false), human_control: Some(false), ..Default::default() });
        rig.desktop.output_change.set(Some(OutputChange::Remove));
        assert_eq!(reconcile().await.unwrap(), json!({ "deferred": false, "changed": true }));
        assert_eq!(*rig.desktop.output_changes.borrow(), [OutputChange::Create, OutputChange::Remove]);
    });
}

// App notes. Failure cases, written first: a refusal about one element
// or one step (a field that takes no focus, a window not focused yet) is shown
// to every later agent as a note on the whole app, including notes recorded so
// by 0.1.0-4; a refusal that describes the app is not kept.

fn frame_lines(env: &Value) -> Vec<String> {
    env["result"]["frame"]["lines"].as_array().unwrap().iter().filter_map(Value::as_str).map(str::to_string).collect()
}

#[test]
fn only_a_refusal_that_describes_the_app_becomes_a_note_in_later_frames() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let key = |id: &str| json!({ "task_ref": task, "request_id": id, "action": { "kind": "key", "keys": "ctrl+s" } });
        let one_step = IbaraError::new("STALE_TARGET", "Surface is not focused; send an explicit focus action first.", true).with("execution_not_started", true);
        rig.desktop.fail_next.replace(Some(one_step));
        assert_eq!(code(&call(&rig.controller, "computer_act", key("act-1")).await), "STALE_TARGET");
        // What 0.1.0-4 recorded for one element's refusal.
        let old = json!({ "text": "browser type refused: The click reached that element, but it did not take the keyboard focus", "worked": false, "route": "browser_type" });
        rig.controller.journal.record_app_note("mousepad", "", "main window", "route:browser_type", &old, "2026-09-27T00:00:00.000Z").unwrap();
        let env = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        let lines = frame_lines(&env);
        assert!(lines.iter().any(|l| l.starts_with("w1 mousepad") && l.ends_with("focused")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.starts_with("note:")), "one step's refusal is no note on the app: {lines:?}");

        let about_app = IbaraError::new("CAPABILITY_UNAVAILABLE", "Cua types only ASCII into windows. Text with other characters needs exactly one editable text element in the window; this one has none or several.", true)
            .with("reason", "non_ascii_text")
            .with("about", "app")
            .with("execution_not_started", true);
        rig.desktop.fail_next.replace(Some(about_app));
        assert_eq!(code(&call(&rig.controller, "computer_act", key("act-2")).await), "CAPABILITY_UNAVAILABLE");
        let env = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        let lines = frame_lines(&env);
        assert!(lines.iter().any(|l| l.starts_with("note: mousepad main window: key refused: Cua types only ASCII")), "{lines:?}");
        let choices = env["result"]["frame"]["choices"].as_array().unwrap();
        assert!(choices.iter().any(|c| c["param"] == "text"), "typing into the focused window is offered: {choices:?}");
    });
}

/// Another window maps and takes the keyboard focus, as a viewer of
/// another computer does when it opens.
fn viewer_takes_focus(desktop: &FakeDesktop) {
    let mut windows = desktop.windows.borrow_mut();
    for w in windows.iter_mut() {
        w.focused = false;
    }
    windows.push(Win { address: "0x2".into(), pid: 200, class: "ibara-view".into(), title: "Viewer".into(), focused: true, ..Default::default() });
}

fn choice_id<'a>(frame: &'a Value, wanted: &str) -> &'a str {
    let choices = frame["choices"].as_array().unwrap();
    choices.iter().find(|c| c["label"].as_str().is_some_and(|l| l.starts_with(wanted))).and_then(|c| c["choice_id"].as_str()).unwrap_or_else(|| panic!("no {wanted} choice: {choices:?}"))
}

#[test]
fn a_typing_or_key_choice_sends_nothing_once_another_window_took_the_focus_since_the_frame() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let shortcut = json!({ "keys": "ctrl+s", "opens": "Save As" });
        rig.controller.journal.record_app_note("mousepad", "", "main window", "shortcut", &shortcut, "2026-09-27T00:00:00.000Z").unwrap();
        let env = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        let frame = &env["result"]["frame"];
        let (typing, key) = (choice_id(frame, "type into w1 mousepad (focused)").to_string(), choice_id(frame, "key ctrl+s").to_string());
        viewer_takes_focus(&rig.desktop);
        for (i, act) in [json!({ "choice": typing, "text": "The quick brown fox" }), json!({ "choice": key })].into_iter().enumerate() {
            let mut args = act;
            args["task_ref"] = json!(task);
            args["request_id"] = json!(format!("act-{i}"));
            let env = call(&rig.controller, "computer_act", args).await;
            assert_eq!(code(&env), "STALE_TARGET", "{env}");
            let error = &env["error"];
            assert_eq!(error["execution_not_started"], true, "{env}");
            let message = error["message"].as_str().unwrap();
            assert!(message.contains("w1 mousepad") && message.contains("ibara-view \"Viewer\""), "names the offered window and where the focus went: {message}");
            assert!(error["next"].as_str().is_some_and(|n| n.contains("computer_observe")), "{env}");
        }
        assert_eq!(rig.desktop.acts(), 0, "nothing typed or pressed anywhere: {:?}", rig.desktop.acts.borrow());

        // Once the agent observes again, the typing choice is the viewer's.
        let env = call(&rig.controller, "computer_observe", json!({ "task_ref": task })).await;
        let typing = choice_id(&env["result"]["frame"], "type into w1 ibara-view (focused)").to_string();
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-3", "choice": typing, "text": "abc" })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let acts = rig.desktop.acts.borrow();
        assert!(acts.len() == 1 && acts[0].contains("Type") && acts[0].contains("0x2"), "{acts:?}");
    });
}

#[test]
fn a_page_field_that_takes_no_focus_leaves_no_note_on_the_browser() {
    run(async {
        let rig = field_rig(false, "");
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Zoë")).await;
        assert!(env["error"]["message"].as_str().unwrap().contains("did not take the keyboard focus"), "{env}");
        for observe in [json!({ "task_ref": task }), json!({ "task_ref": task, "surface": "tab", "view": "elements" })] {
            let env = call(&rig.controller, "computer_observe", observe).await;
            let lines = frame_lines(&env);
            assert!(!lines.iter().any(|l| l.starts_with("note:")), "{lines:?}");
        }
    });
}

fn launch_choice(frame: &Value) -> Option<&Value> {
    frame["choices"].as_array()?.iter().find(|c| c["action"]["kind"] == "launch" && c["action"]["app"] == "editor")
}

#[test]
fn a_window_expectation_is_met_only_by_a_window_that_appears() {
    run(async {
        let rig = rig(false);
        let begun = call(&rig.controller, "computer_begin", json!({ "goal": "Save a note in the editor", "request_id": "b1" })).await;
        assert!(launch_choice(&begun["result"]["frame"]).is_some(), "an editor someone else left open is not this task's: {begun}");
        let task = begun["result"]["task_ref"].as_str().unwrap();
        let launch = |request: &str| {
            json!({ "task_ref": task, "request_id": request, "action": { "kind": "launch", "app": "editor" }, "expect": { "kind": "window", "app": "mousepad", "within_ms": 50 } })
        };
        let env = call(&rig.controller, "computer_act", launch("act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unmet", "the editor open before the launch does not count: {env}");
        rig.desktop.spawn.replace(Some(Win { address: "0x2".into(), pid: 101, class: "mousepad".into(), title: "Untitled 2 - Mousepad".into(), focused: true, ..Default::default() }));
        let env = call(&rig.controller, "computer_act", launch("act-2")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        assert!(launch_choice(&env["result"]["frame"]).is_none(), "the task has its own editor now: {env}");
    });
}

/// A window expectation names the browser as a launch does, on a computer
/// whose browser is Google Chrome. Failure cases:
/// 1. The launch reads "not seen" although Chrome's window opened: `chromium`
///    was matched against the class `google-chrome` letter for letter.
/// 2. A browser name is met by another app's window.
/// 3. Part of the window class, or the approved app's name, no longer works.
#[test]
fn a_window_expectation_knows_the_browser_by_every_name_a_launch_takes() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let launch = |request: &str, app: &str| {
            json!({ "task_ref": task, "request_id": request, "action": { "kind": "launch", "app": "browser" }, "expect": { "kind": "window", "app": app, "within_ms": 50 } })
        };
        rig.desktop.spawn.replace(Some(win("0x10", 300, "mousepad", "Untitled 2 - Mousepad", true, false)));
        let env = call(&rig.controller, "computer_act", launch("act-0", "chromium")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unmet", "an editor is not the browser: {env}");
        for (i, app) in ["chromium", "chrome", "Google Chrome", "browser", "google-chrome"].into_iter().enumerate() {
            rig.desktop.spawn.replace(Some(win(&format!("0x2{i}"), 200 + i as i64, "google-chrome", "New Tab - Google Chrome", true, false)));
            let env = call(&rig.controller, "computer_act", launch(&format!("act-{}", i + 1), app)).await;
            assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{app}: {env}");
        }
    });
}

#[test]
fn typing_is_not_offered_while_a_button_has_the_keyboard_focus() {
    run(async {
        let rig = rig(false);
        let button = |id: &str, name: &str, focused: bool| crate::desktop::atspi::Element {
            id: id.into(),
            role: "push button".into(),
            name: name.into(),
            value: None,
            states: if focused { vec!["enabled".into(), "focused".into()] } else { vec!["enabled".into()] },
            actions: vec!["click".into()],
            parent: None,
            context: vec!["alert \"Mousepad\"".into()],
            selector: Value::Null,
        };
        rig.desktop.page.replace(ElementPage { available: true, elements: vec![button("e1", "No", false), button("e2", "Yes", true)], ..Default::default() });
        let env = call(&rig.controller, "computer_begin", json!({ "goal": "Save a note in the editor", "request_id": "b1" })).await;
        let choices = env["result"]["frame"]["choices"].as_array().unwrap();
        assert!(!choices.iter().any(|c| c["action"]["kind"] == "type"), "keys would press the focused button: {choices:?}");
        assert!(choices.iter().any(|c| c["label"].as_str().is_some_and(|l| l.contains("\"No\""))), "{choices:?}");
    });
}

#[test]
fn a_dialog_that_stays_open_is_not_gone() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        {
            let mut windows = rig.desktop.windows.borrow_mut();
            windows[0].focused = false;
            windows.push(Win { address: "0x3".into(), pid: 100, class: "mousepad".into(), title: "Save As".into(), focused: true, floating: true, ..Default::default() });
        }
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "Return" }, "expect": { "kind": "dialog", "gone": true, "within_ms": 50 } });
        let env = call(&rig.controller, "computer_act", args).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unmet", "the dialog in front is still open: {env}");
    });
}

#[test]
fn a_dialog_left_open_before_the_step_does_not_meet_a_titled_dialog_expectation() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.windows.borrow_mut().push(win("0x3", 100, "mousepad", "Save As", false, true));
        let save = |id: &str| json!({ "task_ref": task, "request_id": id, "action": { "kind": "key", "keys": "ctrl+s" }, "expect": { "kind": "dialog", "title": "Save As", "within_ms": 50 } });
        let env = call(&rig.controller, "computer_act", save("act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unmet", "the Save As left from before is not this step's: {env}");
        rig.desktop.spawn.replace(Some(win("0x4", 100, "mousepad", "Save As", true, true)));
        let env = call(&rig.controller, "computer_act", save("act-2")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
    });
}

#[test]
fn a_titled_dialog_is_gone_when_the_one_in_front_closes() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        {
            let mut windows = rig.desktop.windows.borrow_mut();
            windows[0].focused = false;
            windows.push(win("0x3", 100, "mousepad", "Save As", false, true));
            windows.push(win("0x4", 100, "mousepad", "Save As", true, true));
        }
        let enter = |id: &str| json!({ "task_ref": task, "request_id": id, "action": { "kind": "key", "keys": "Return" }, "expect": { "kind": "dialog", "title": "Save As", "gone": true, "within_ms": 50 } });
        let env = call(&rig.controller, "computer_act", enter("act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "unmet", "the dialog in front is still open: {env}");
        rig.desktop.closing.replace(vec!["0x4".into()]);
        let env = call(&rig.controller, "computer_act", enter("act-2")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "another Save As left open does not keep this one open: {env}");
    });
}

#[test]
fn finish_does_not_report_a_window_closed_while_it_is_still_open() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.spawn.replace(Some(win("0x2", 200, "mousepad", "Untitled 2 - Mousepad", true, false)));
        rig.desktop.launch_pid.set(Some(200));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "launch", "app": "editor" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        rig.desktop.windows.borrow_mut().iter_mut().for_each(|w| w.focused = w.address == "0x2");
        rig.desktop.spawn.replace(Some(win("0x4", 200, "mousepad", "Save As", false, true)));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "key", "keys": "ctrl+s" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        rig.desktop.ignores_close.replace(vec!["0x4".into()]);
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        let cleanup = &finish["result"]["cleanup"];
        assert_eq!(cleanup["closed"], json!(["mousepad \"Untitled 2 - Mousepad\""]), "{finish}");
        assert_eq!(cleanup["left"], json!([{ "surface": "mousepad \"Save As\"", "reason": "it was still open after the close request" }]), "{finish}");
    });
}

// A launch returns once its program runs; the window maps later. Failure cases:
// - the window maps after the step, which had no expectation, and is never
//   the task's, so finish leaves it open and does not say so; by an explicit
//   launch and by the offered launch choice;
// - a person's window that opens meanwhile becomes the task's;
// - the app titling its window as it maps, seconds after the launch on a
//   slow computer, counts as a person using it, so finish leaves it;
// - a late window a person did use is closed at finish.
#[test]
fn a_launch_owns_its_window_when_it_maps_after_the_step_and_finish_closes_it() {
    run(async {
        for route in ["action", "choice"] {
            let rig = rig(false);
            let begun = call(&rig.controller, "computer_begin", json!({ "goal": "Save a note in the editor", "request_id": "b1" })).await;
            let task = begun["result"]["task_ref"].as_str().unwrap();
            let mut args = json!({ "task_ref": task, "request_id": "act-1" });
            match route {
                "action" => args["action"] = json!({ "kind": "launch", "app": "editor" }),
                _ => args["choice"] = launch_choice(&begun["result"]["frame"]).unwrap()["choice_id"].clone(),
            }
            rig.desktop.launch_pid.set(Some(200));
            rig.desktop.maps_late.replace(Some((std::time::Duration::from_millis(300), win("0x2", 200, "mousepad", "Untitled 2 - Mousepad", true, false))));
            rig.desktop.others.replace(vec![win("0x3", 300, "foot", "a person's shell", false, false)]);
            // By ibara's clock the window maps 5 s after the launch.
            let slow = async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                rig.clock.fetch_add(5_000, Ordering::SeqCst);
            };
            let (env, ()) = tokio::join!(call(&rig.controller, "computer_act", args), slow);
            assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{route}: {env}");
            assert!(launch_choice(&env["result"]["frame"]).is_none(), "{route}: the frame shows the task's own editor: {env}");
            rig.controller.on_desktop_event(DesktopEvent::WindowTitle { address: "0x2".into(), title: "Untitled 2 - Mousepad".into() });
            let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "complete", "summary": "done" })).await;
            let cleanup = &finish["result"]["cleanup"];
            assert_eq!(cleanup["closed"], json!(["mousepad \"Untitled 2 - Mousepad\""]), "{route}: only the task's editor: {finish}");
            assert_eq!(cleanup["left"], json!([]), "{route}: {finish}");
        }
    });
}

#[test]
fn a_window_a_launch_opened_late_that_a_person_used_is_left_open_at_finish() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.launch_pid.set(Some(200));
        rig.desktop.maps_late.replace(Some((std::time::Duration::from_millis(300), win("0x2", 200, "mousepad", "Untitled 2 - Mousepad", true, false))));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "launch", "app": "editor" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        rig.clock.fetch_add(10_000, Ordering::SeqCst);
        rig.controller.on_desktop_event(DesktopEvent::Focus { address: Some("0x2".into()) });
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        let cleanup = &finish["result"]["cleanup"];
        assert_eq!(cleanup["closed"], json!([]), "{finish}");
        assert_eq!(cleanup["left"], json!([{ "surface": "mousepad \"Untitled 2 - Mousepad\"", "reason": "a person used it" }]), "{finish}");
    });
}

// While a launch waits for its window, a person opens their own window of the
// same app. Failure cases: the person's window is the task's (finish closes
// it) and the agent's own window, mapping later, is not (it is left behind).
// A launcher that hands the app to another process and ends (the browser)
// still owns the app's new window by its class.
#[test]
fn a_same_app_window_a_person_opens_during_a_launch_is_not_the_tasks() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.launch_pid.set(Some(200));
        rig.desktop.running.replace(vec![200]);
        rig.desktop.maps_late.replace(Some((std::time::Duration::from_millis(300), win("0x2", 200, "mousepad", "Untitled 2 - Mousepad", true, false))));
        rig.desktop.others.replace(vec![win("0x3", 300, "mousepad", "a person's notes - Mousepad", false, false)]);
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "launch", "app": "editor" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        let cleanup = &finish["result"]["cleanup"];
        assert_eq!(cleanup["closed"], json!(["mousepad \"Untitled 2 - Mousepad\""]), "only the launched editor: {finish}");
        assert!(rig.desktop.windows.borrow().iter().any(|w| w.address == "0x3"), "the person's editor stays open");

        let task = begin(&rig.controller).await;
        rig.desktop.launch_pid.set(Some(400));
        rig.desktop.maps_late.replace(Some((std::time::Duration::from_millis(300), win("0x4", 401, "chromium", "New Tab - Chromium", true, false))));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "launch", "app": "browser" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f2", "outcome": "partial", "summary": "stop" })).await;
        assert_eq!(finish["result"]["cleanup"]["closed"], json!(["chromium \"New Tab - Chromium\""]), "the launcher ended; the browser's window is the task's: {finish}");
    });
}

fn win(address: &str, pid: i64, class: &str, title: &str, focused: bool, floating: bool) -> Win {
    Win { address: address.into(), pid, class: class.into(), title: title.into(), focused, floating, ..Default::default() }
}

#[test]
fn an_approved_step_whose_target_changed_is_held_again() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "Return" }, "effect": "send" });
        let held = call(&rig.controller, "computer_act", args.clone()).await;
        let att = held["result"]["attention"].as_str().unwrap().to_string();
        rig.desktop.windows.replace(vec![
            win("0x1", 100, "mousepad", "Untitled 1 - Mousepad", false, false),
            win("0x9", 300, "foot", "shell", true, false),
        ]);
        rig.controller.admin(json!({ "op": "answer_attention", "att_ref": att, "answer": "approve" })).await.unwrap();
        let again = call(&rig.controller, "computer_act", args).await;
        assert_eq!(rig.desktop.acts(), 0, "Return must not go to a window the person never approved: {again}");
        assert_eq!(again["status"], "pending", "{again}");
        let second = again["result"]["attention"].as_str().unwrap();
        assert_ne!(second, att, "a new approval is asked for the new target");
        assert!(again["result"]["steps"][0]["effect"].as_str().unwrap().contains("changed since it was approved"), "{again}");
    });
}

#[test]
fn a_repeat_of_a_call_still_running_reports_it_running() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.hang.set(true);
        let args = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "key", "keys": "ctrl+s" } });
        let c = rig.controller.clone();
        let first_args = args.clone();
        let first = tokio::task::spawn_local(async move { call(&c, "computer_act", first_args).await });
        rig.desktop.entered.notified().await;
        let repeat = tokio::time::timeout(std::time::Duration::from_secs(2), call(&rig.controller, "computer_act", args))
            .await
            .expect("a repeat must not wait behind the call it repeats");
        assert_eq!(repeat["status"], "pending", "{repeat}");
        assert_eq!(repeat["result"]["state"], "running", "{repeat}");
        assert!(repeat["result"]["op_ref"].as_str().is_some_and(|o| o.starts_with("op_")), "{repeat}");
        assert_eq!(rig.desktop.acts(), 1);
        first.abort();
    });
}

#[test]
fn only_windows_of_the_launched_app_or_acted_on_process_belong_to_the_task() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.spawn.replace(Some(win("0x2", 200, "mousepad", "Untitled 2 - Mousepad", true, false)));
        rig.desktop.others.replace(vec![win("0x3", 300, "foot", "a person's shell", false, false)]);
        rig.desktop.launch_pid.set(Some(200));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "launch", "app": "editor" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        rig.desktop.windows.borrow_mut().iter_mut().for_each(|w| w.focused = w.address == "0x2");
        rig.desktop.spawn.replace(Some(win("0x4", 200, "mousepad", "Open File", false, true)));
        rig.desktop.others.replace(vec![win("0x5", 301, "foot", "another shell", false, false)]);
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "key", "keys": "ctrl+o" } })).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let finish = call(&rig.controller, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        let closed: Vec<&str> = finish["result"]["cleanup"]["closed"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(closed.len(), 2, "{finish}");
        assert!(closed.iter().all(|c| c.starts_with("mousepad")), "a person's windows are not the task's: {closed:?}");
        let acts = rig.desktop.acts.borrow();
        assert!(!acts.iter().any(|a| a.contains("0x3") || a.contains("0x5")), "{acts:?}");
    });
}

#[test]
fn the_caller_going_away_stops_waiting_and_runs_no_further_step() {
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        let gone = Cancel::new();
        let fire = gone.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            fire.cancel();
        });
        let args = json!({ "task_ref": task, "request_id": "act-1", "steps": [
            { "action": { "kind": "key", "keys": "ctrl+s" }, "expect": { "kind": "dialog", "title": "Save As", "within_ms": 60000 } },
            { "action": { "kind": "key", "keys": "Return" } },
        ] });
        let env = tokio::time::timeout(std::time::Duration::from_secs(5), rig.controller.call("vesper", "connection_a", "codex", "computer_act", args.clone(), gone))
            .await
            .expect("the wait must end when the caller goes away")
            .envelope;
        let steps = env["result"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["outcome"], "unmet", "{env}");
        assert!(steps[0]["effect"].as_str().unwrap().contains("caller went away"), "{env}");
        assert_eq!(steps[1]["outcome"], "not_run");
        assert_eq!(rig.desktop.acts(), 1);
        let replay = call(&rig.controller, "computer_act", args).await;
        assert_eq!(replay["result"]["steps"], env["result"]["steps"], "the stopped call's outcome is stored for replay");
        let asked = call(&rig.controller, "computer_checkpoint", json!({ "task_ref": task, "ask": { "question": "Which file?" } })).await;
        let att = asked["result"]["attention"].as_str().unwrap().to_string();
        let gone = Cancel::new();
        gone.cancel();
        let wait = json!({ "task_ref": task, "for": { "attention": att }, "deadline_ms": 600000 });
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), rig.controller.call("vesper", "connection_a", "codex", "computer_wait", wait, gone))
            .await
            .expect("computer_wait returns when the caller is gone")
            .envelope;
        assert_eq!(waited["status"], "pending", "{waited}");
        assert_eq!(waited["result"]["met"], false);
    });
}

/// A rig whose focused window is Chromium at 0,0 (1000×1000). Its page has
/// a button (b1) and a text field (b2); `locate` answers a point 50 pixels
/// into the page under an 87-pixel toolbar band, as Chromium draws it.
fn browser_rig() -> Rig {
    let rig = rig(false);
    let chromium = Win { rect: Rect { x: 0, y: 0, width: 1000, height: 1000 }, ..win("0x9", 300, "chromium", "Shop - Chromium", true, false) };
    rig.desktop.windows.replace(vec![chromium]);
    let nodes = json!([
        { "role": "button", "name": "Next", "actions": ["click"], "states": [], "token": "t1" },
        { "role": "textbox", "name": "Name", "actions": ["click", "fill"], "states": [], "token": "t2" },
    ]);
    let mut extension = rig.desktop.extension.borrow_mut();
    extension.insert("observe".into(), json!({ "capture": "c1", "documentId": "d1", "url": "https://shop.example/", "title": "Shop", "count": 2, "nodes": nodes }));
    extension.insert("locate".into(), json!({ "x": 100, "y": 50, "outer": [1000, 1000], "inner": [1000, 913], "zoom": 1 }));
    drop(extension);
    rig
}

/// Begin a task and observe the page, so b1 and b2 name its elements.
async fn observe_page(c: &Controller) -> String {
    let task = begin(c).await;
    let env = call(c, "computer_observe", json!({ "task_ref": task, "surface": "tab", "view": "elements" })).await;
    assert_eq!(env["status"], "ok", "{env}");
    task
}

fn click_next(task: &str, request_id: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "click", "target": "b1" } })
}

#[test]
fn a_page_shrunk_from_below_is_never_clicked() {
    run(async {
        let rig = browser_rig();
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": true, "hit": true }));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", click_next(&task, "act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(acts[0].contains("x: 100.0, y: 137.0"), "the page starts below the toolbar band: {acts:?}");
        // Developer tools docked at the bottom shorten the page by 300 pixels;
        // the page's own measure cannot tell that they sit below it.
        rig.desktop.extension.borrow_mut().insert("locate".into(), json!({ "x": 100, "y": 50, "outer": [1000, 1000], "inner": [1000, 613], "zoom": 1 }));
        let env = call(&rig.controller, "browser_act", click_next(&task, "act-2")).await;
        assert_eq!(code(&env), "CAPABILITY_UNAVAILABLE", "{env}");
        assert_eq!(rig.desktop.acts(), 1, "nothing is clicked 300 pixels below the chosen point");
    });
}

#[test]
fn status_shows_the_running_tasks_latest_step_and_click_then_the_finished_task() {
    run(async {
        let rig = browser_rig();
        let c = &rig.controller;
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": true, "hit": true }));
        let before = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        assert_eq!(before["last_task"], Value::Null, "{before}");
        let task = observe_page(c).await;
        let env = call(c, "browser_act", click_next(&task, "act-1")).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        eprintln!("status after a click: {}", json!({ "active_task": status["active_task"], "last_task": status["last_task"] }));
        let active = &status["active_task"];
        assert!(active["last_step"]["summary"].as_str().is_some_and(|s| s.starts_with("Clicking ") && s.chars().count() <= 80), "{status}");
        assert!(active["last_step"]["at"].is_string(), "{status}");
        // The click landed at 100,137 of the 1920x1080 screen.
        assert_eq!((active["last_point"]["x"].as_f64(), active["last_point"]["y"].as_f64()), (Some(100.0 / 1920.0), Some(137.0 / 1080.0)), "{status}");

        let key = json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "key", "keys": "Tab" } });
        let env = call(c, "browser_act", key).await;
        assert_eq!(env["status"], "ok", "{env}");
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        assert_eq!(status["active_task"]["last_step"]["summary"], "Pressing Tab", "{status}");
        assert!(status["active_task"].get("last_point").is_none(), "a key press is not a click: {status}");

        let finished = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "fin-1", "outcome": "cancelled", "summary": "Stopped." })).await;
        assert_eq!(finished["status"], "ok", "{finished}");
        let status = c.operator_call("vesper", operator_action(c, "status")).await.unwrap();
        eprintln!("status after finishing: {}", json!({ "active_task": status["active_task"], "last_task": status["last_task"] }));
        assert_eq!(status["active_task"], Value::Null, "{status}");
        let last = &status["last_task"];
        assert_eq!((last["ref"].as_str(), last["outcome"].as_str()), (Some(task.as_str()), Some("cancelled")), "{status}");
        assert!(last["title"].is_string() && last["finished_at"].is_string(), "{status}");
    });
}

#[test]
fn step_words_say_what_is_happening() {
    use super::operator::step_words;
    assert_eq!(step_words("click the “Sign Up” button"), "Clicking the “Sign Up” button");
    assert_eq!(step_words("type 5 characters into the “Email” field"), "Typing 5 characters into the “Email” field");
    assert_eq!(step_words("press Return"), "Pressing Return");
    assert_eq!(step_words("double-click a spot on the screen"), "Double-clicking a spot on the screen");
    assert_eq!(step_words("open the web browser"), "Opening the web browser");
    assert_eq!(step_words("run the command “ls”"), "Running the command “ls”");
    assert_eq!(step_words("go to example.com"), "Going to example.com");
    assert_eq!(step_words("scroll"), "Scrolling");
    assert!(step_words(&format!("type {}", "word ".repeat(40))).chars().count() <= 80);
}

#[test]
fn a_click_the_page_never_saw_is_an_unknown_outcome() {
    // The press landed outside the page (the browser's own controls,
    // developer tools, another window), or the extension cannot say where.
    for verify in [Some(json!({ "observed": false, "hit": null })), None] {
        run(async {
            let rig = browser_rig();
            if let Some(answer) = &verify {
                rig.desktop.extension.borrow_mut().insert("verify".into(), answer.clone());
            }
            let task = observe_page(&rig.controller).await;
            let typed = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "type", "target": "b2", "text": "Ada" } });
            let env = call(&rig.controller, "browser_act", typed).await;
            assert_eq!(env["result"]["steps"][0]["outcome"], "unknown", "{verify:?}: {env}");
            let acts = rig.desktop.acts.borrow().clone();
            assert_eq!(acts.len(), 1, "nothing is typed after a click the page did not report: {acts:?}");
        });
    }
}

#[test]
fn a_window_listing_that_fails_before_the_click_is_not_started() {
    run(async {
        let rig = browser_rig();
        let task = observe_page(&rig.controller).await;
        rig.desktop.windows_fail_after.replace(Some(("locate".into(), IbaraError::new("SESSION_UNAVAILABLE", "hyprctl failed", true))));
        let env = call(&rig.controller, "browser_act", click_next(&task, "act-1")).await;
        assert_eq!(code(&env), "SESSION_UNAVAILABLE", "a failure before any input is not a possible click: {env}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

// Typing into a page field and clicks that navigate. Failure
// cases, written first:
// - text beyond ASCII does not arrive in the field exactly, or Cua (which
//   types only ASCII) is asked to type it;
// - the person's clipboard is not the same afterwards, every type and byte,
//   also when a paste never arrives;
// - anything is typed, or the clipboard touched, when the clicked element
//   did not take the keyboard focus (the address bar may have it);
// - text beyond ASCII without a field, or through computer_act into a
//   browser window, is sent anywhere;
// - typed text is stored anywhere (timeline, receipts, stored replies);
// - a click that navigates reads as "outcome unknown", does not say where
//   the page went, or repeats what a GET form sent in its address.

/// The browser rig with a Name field (b2) that takes the keyboard focus when
/// clicked and holds `value`, and every press landing on its element.
fn field_rig(focusable: bool, value: &str) -> Rig {
    let rig = browser_rig();
    rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": true, "hit": true }));
    rig.desktop.field.replace(FakeField { present: true, focusable, value: value.into(), ..Default::default() });
    rig
}

fn type_name(task: &str, request_id: &str, text: &str) -> Value {
    json!({ "task_ref": task, "request_id": request_id, "action": { "kind": "type", "target": "b2", "text": text } })
}

/// Whether any of `pieces` is in a file under `dir` (the journal and storage).
fn stored_anywhere(dir: &Path, pieces: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                for piece in pieces {
                    if bytes.windows(piece.len()).any(|w| w == piece.as_bytes()) {
                        found.push(format!("{piece} in {}", path.display()));
                    }
                }
            }
        }
    }
    found
}

#[test]
fn text_beyond_ascii_is_pasted_into_the_focused_field_and_the_clipboard_put_back() {
    run(async {
        let rig = field_rig(true, "old name");
        let person = SavedClipboard(vec![("image/png".into(), vec![137, 80, 78, 71, 0, 255]), ("text/html".into(), b"<img src=\"a.png\">".to_vec())]);
        rig.desktop.selection.replace(person.clone());
        let task = observe_page(&rig.controller).await;
        let text = "Zoë Ångström, 東京駅 ok";
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", text)).await;
        assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{env}");
        assert_eq!(rig.desktop.field.borrow().value, text, "the old name is replaced, every character in place");
        assert_eq!(*rig.desktop.selection.borrow(), person, "the person's clipboard is back, every type and byte");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(acts.iter().all(|a| !a.starts_with("Type") || a.is_ascii()), "Cua is asked to type ASCII only: {acts:?}");
        assert_eq!(acts.iter().filter(|a| a.contains("ctrl+v")).count(), 4, "one paste per run beyond ASCII: {acts:?}");
        let pieces = ["Zoë", "Ångström", "東京駅"];
        assert!(!pieces.iter().any(|p| env.to_string().contains(p)), "{env}");
        assert_eq!(stored_anywhere(&rig.dir, &pieces), Vec::<String>::new(), "typed text is stored nowhere");
    });
}

#[test]
fn a_field_that_does_not_take_the_focus_gets_nothing_typed() {
    for text in ["Zoë", "Ada"] {
        run(async {
            let rig = field_rig(false, "");
            let person = SavedClipboard(vec![(PASTED.into(), b"the person's own copy".to_vec())]);
            rig.desktop.selection.replace(person.clone());
            let task = observe_page(&rig.controller).await;
            let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", text)).await;
            assert_eq!(code(&env), "CAPABILITY_UNAVAILABLE", "{text}: {env}");
            assert!(env["error"]["message"].as_str().unwrap().contains("did not take the keyboard focus"), "{env}");
            let acts = rig.desktop.acts.borrow().clone();
            assert_eq!(acts.len(), 1, "{text}: the click, and no key or text after it: {acts:?}");
            assert_eq!(*rig.desktop.selection.borrow(), person, "{text}: the clipboard is untouched");
        });
    }
}

#[test]
fn the_clipboard_is_put_back_when_a_paste_never_arrives() {
    run(async {
        let rig = field_rig(true, "");
        rig.desktop.field.borrow_mut().ignores_paste = true;
        let person = SavedClipboard(vec![(PASTED.into(), b"the person's own copy".to_vec())]);
        rig.desktop.selection.replace(person.clone());
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Zoë Ångström")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        assert!(step["effect"].as_str().unwrap().contains("does not show the text"), "{env}");
        assert_eq!(*rig.desktop.selection.borrow(), person, "the person's clipboard is back");
        let acts = rig.desktop.acts.borrow().clone();
        assert!(acts.last().is_some_and(|a| a.contains("ctrl+v")), "nothing more is typed after the missing paste: {acts:?}");
    });
}

// The clipboard while ibara pastes, and control changing hands mid-step
// (review of 0.1.0-4). Failure cases, written first:
// - a copy the person makes before the first paste, between two pastes or
//   after the last one is pasted over or replaced by the old clipboard, and
//   the step does not say so;
// - when typing fails and the clipboard cannot be put back, the step, its
//   receipt and the timeline do not say so;
// - a password manager's secret is put back after its manager may have
//   cleared it;
// - after a pause, a step sends Ctrl+A, Ctrl+V, a list's label or Return.

#[test]
fn a_copy_made_while_ibara_types_is_kept_and_the_step_says_so() {
    // (text, the typed piece during which the person copies, pastes sent, outcome)
    let moments = [
        ("Bonjour, je m'appelle José", r#"text: "Bonjour, je m'appelle Jos""#, 0, "unknown"),
        ("Zoë Ångström", r#"text: "ngstr""#, 2, "unknown"),
        ("Zoë Ångström", r#"text: "m" }"#, 3, "done"),
    ];
    for (text, during, pastes, outcome) in moments {
        run(async {
            let rig = field_rig(true, "");
            rig.desktop.selection.replace(SavedClipboard(vec![(PASTED.into(), b"before".to_vec())]));
            let copy = SavedClipboard(vec![(PASTED.into(), b"the person's new copy".to_vec()), ("text/html".into(), b"<b>new</b>".to_vec())]);
            rig.desktop.person_copies.replace(Some((during.to_string(), copy.clone())));
            let task = observe_page(&rig.controller).await;
            let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", text)).await;
            let step = &env["result"]["steps"][0];
            assert_eq!(step["outcome"], outcome, "{during}: {env}");
            assert_eq!(*rig.desktop.selection.borrow(), copy, "{during}: the person's copy stays");
            let acts = rig.desktop.acts.borrow().clone();
            assert_eq!(acts.iter().filter(|a| a.contains("ctrl+v")).count(), pastes, "{during}: nothing pasted over the copy: {acts:?}");
            let said = step["effect"].as_str().unwrap();
            assert!(said.contains("copied while ibara typed"), "{during}: the agent is told: {said}");
        });
    }
}

#[test]
fn when_typing_fails_the_step_still_says_what_happened_to_the_clipboard() {
    run(async {
        let rig = field_rig(true, "");
        rig.desktop.field.borrow_mut().ignores_paste = true;
        rig.desktop.selection.replace(SavedClipboard(vec![(PASTED.into(), b"the person's own copy".to_vec())]));
        let lost = IbaraError::new("CAPABILITY_UNAVAILABLE", "The clipboard could not be put back: the compositor did not answer.", true);
        rig.desktop.put_back_fails.replace(Some(lost));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Zoë")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        assert!(step["effect"].as_str().unwrap().contains("could not be put back"), "the step: {env}");
        let op = rig.controller.journal.get_operation_by_ref(step["op_ref"].as_str().unwrap()).unwrap().unwrap();
        assert!(op.receipt["error"]["message"].as_str().unwrap().contains("could not be put back"), "the receipt: {}", op.receipt);
        let status = call(&rig.controller, "computer_status", json!({ "ref": step["op_ref"] })).await;
        assert!(status.to_string().contains("could not be put back"), "the step's status: {status}");
    });
}

#[test]
fn a_password_on_the_clipboard_is_not_put_back() {
    run(async {
        let rig = field_rig(true, "");
        let password = SavedClipboard(vec![(PASTED.into(), b"correct horse".to_vec()), (SECRET.0.into(), SECRET.1.to_vec())]);
        rig.desktop.selection.replace(password);
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Zoë")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "done", "{env}");
        assert_eq!(*rig.desktop.selection.borrow(), SavedClipboard::default(), "left empty");
        assert!(step["effect"].as_str().unwrap().contains("left it empty"), "and said: {env}");
    });
}

#[test]
fn a_pause_during_a_browser_step_sends_no_further_input() {
    // (action, where the step is when the person pauses, input that must not follow)
    let cases = [
        (json!({ "kind": "type", "target": "b2", "text": "Ada" }), "field", "ctrl+a"),
        (json!({ "kind": "type", "target": "b2", "text": "Zoë" }), "paste", "ctrl+v"),
        (json!({ "kind": "select", "target": "b3", "value": "Blue" }), "verify", "Return"),
    ];
    for (action, at, never) in cases {
        run(async {
            let rig = field_rig(true, "");
            let nodes = json!([
                { "role": "button", "name": "Next", "actions": ["click"], "states": [], "token": "t1" },
                { "role": "textbox", "name": "Name", "actions": ["click", "fill"], "states": [], "token": "t2" },
                { "role": "combobox", "name": "Color", "actions": ["select"], "states": [], "token": "t3" },
            ]);
            let mut extension = rig.desktop.extension.borrow_mut();
            extension.insert("observe".into(), json!({ "capture": "c1", "documentId": "d1", "url": "https://shop.example/", "title": "Shop", "count": 3, "nodes": nodes }));
            extension.insert("locate".into(), json!({ "x": 100, "y": 50, "outer": [1000, 1000], "inner": [1000, 913], "zoom": 1, "label": "Blue" }));
            drop(extension);
            let task = observe_page(&rig.controller).await;
            let hold = Rc::new(Notify::new());
            rig.desktop.holds.borrow_mut().insert(at.to_string(), hold.clone());
            let c = rig.controller.clone();
            let args = json!({ "task_ref": task, "request_id": "act-1", "action": action });
            let step = tokio::task::spawn_local(async move { call(&c, "browser_act", args).await });
            let reached = tokio::time::timeout(std::time::Duration::from_secs(5), rig.desktop.entered.notified()).await;
            assert!(reached.is_ok(), "{at}: the step never got there");
            let c = rig.controller.clone();
            let pause = tokio::task::spawn_local(async move { c.admin(json!({ "op": "pause" })).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            hold.notify_one();
            let env = step.await.unwrap();
            pause.await.unwrap().unwrap();
            let acts = rig.desktop.acts.borrow().clone();
            assert!(!acts.iter().any(|a| a.contains(never)), "{at}: no {never} after the pause: {acts:?} {env}");
            assert!(!acts.iter().any(|a| a.contains("text: \"Blue\"")), "{at}: no label typed: {acts:?}");
        });
    }
}

#[test]
fn text_beyond_ascii_is_never_sent_where_the_address_bar_could_take_it() {
    run(async {
        let rig = field_rig(true, "");
        let task = observe_page(&rig.controller).await;
        let at_focus = json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "type", "text": "Zoë" } });
        let env = call(&rig.controller, "browser_act", at_focus).await;
        assert_eq!(code(&env), "INVALID_ARGUMENT", "without a field: {env}");
        let desktop = json!({ "task_ref": task, "request_id": "act-2", "action": { "kind": "type", "text": "Zoë" } });
        let env = call(&rig.controller, "computer_act", desktop).await;
        assert_eq!(code(&env), "WRONG_TOOL", "into the browser window: {env}");
        assert_eq!(rig.desktop.acts(), 0);
    });
}

// A step stopped after part of its input went through (seen: 400 characters
// typed before Cua was stopped; 16 in a page field when a person moved the
// mouse). Failure cases, written first:
// - the step says ibara sent nothing, or to try again, after part of it went
//   through, so the agent types it all again;
// - it does not say how much was typed when that is known, or to check
//   before typing again when part may have been;
// - a step of which nothing went through no longer says so.

/// The refusal of an input while a person uses the mouse, as the desktop
/// gives it.
fn person_busy() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "A person is using the mouse, so ibara sent nothing. Try again once they stop.", true)
        .with("reason", "person_active")
        .with("execution_not_started", true)
}

#[test]
fn a_typing_step_stopped_part_of_the_way_says_how_much_was_typed() {
    let text = "x".repeat(60);
    // The desktop's account of the typing: what stopped it, the characters
    // typed for sure, those that may have been.
    let hidden = IbaraError::new("CAPABILITY_UNAVAILABLE", "The agent's cursor is not on the screen, so ibara sent nothing. Try again.", false)
        .with("reason", "agent_cursor_hidden");
    let cut = IbaraError::new("OUTCOME_UNKNOWN", "Cua did not answer type_text; it was stopped.", false);
    let cases = [
        // A person took the mouse between two pieces.
        (person_busy(), 16, 0, "16 of the 60", "44"),
        // Cua stopped between two pieces.
        (hidden, 48, 0, "48 of the 60", "12"),
        // Cua stopped in the middle of a piece (a worker that did not answer).
        (cut, 16, 16, "16 of the 60", "next 16"),
    ];
    for (stopped, typed, unsure, said_typed, said_rest) in cases {
        run(async {
            let rig = rig(false);
            let task = begin(&rig.controller).await;
            let error = stopped.with("typed_chars", typed).with("unsure_chars", unsure).with("execution_not_started", false);
            rig.desktop.fail_next.replace(Some(error));
            let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "type", "text": text } })).await;
            let step = &env["result"]["steps"][0];
            assert_eq!(step["outcome"], "unknown", "{env}");
            let said = step["effect"].as_str().unwrap();
            assert!(said.contains(said_typed) && said.contains(said_rest), "how much went through: {said}");
            assert!(!said.contains("sent nothing") && !said.contains("Try again"), "not as if nothing went through: {said}");
        });
    }
    // Nothing went through: the step says so.
    run(async {
        let rig = rig(false);
        let task = begin(&rig.controller).await;
        rig.desktop.fail_next.replace(Some(person_busy().with("typed_chars", 0).with("unsure_chars", 0)));
        let env = call(&rig.controller, "computer_act", json!({ "task_ref": task, "request_id": "act-1", "action": { "kind": "type", "text": text } })).await;
        assert_eq!(code(&env), "CAPABILITY_UNAVAILABLE", "{env}");
        assert!(env["error"]["message"].as_str().unwrap().contains("sent nothing"), "{env}");
    });
}

#[test]
fn a_page_field_typed_into_part_of_the_way_says_how_much_went_through() {
    // Refused at its third run, after "Zoë Å" went through.
    run(async {
        let rig = field_rig(true, "");
        rig.desktop.fail_on.replace(Some((r#"text: "ngstr""#.into(), person_busy())));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Zoë Ångström")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        let said = step["effect"].as_str().unwrap();
        assert!(said.contains("5 of the 12"), "how much went through: {said}");
        assert!(!said.contains("sent nothing") && !said.contains("Try again"), "{said}");
    });
    // Refused before any character, after the click.
    run(async {
        let rig = field_rig(true, "");
        rig.desktop.fail_on.replace(Some((r#"text: "Ada""#.into(), person_busy())));
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", type_name(&task, "act-1", "Ada")).await;
        let step = &env["result"]["steps"][0];
        let said = step["effect"].as_str().unwrap();
        assert!(said.contains("click") && said.contains("nothing was typed"), "the click went through, the text did not: {said}");
        assert!(!said.contains("sent nothing"), "{said}");
    });
}

#[test]
fn a_click_that_navigates_says_where_the_page_went() {
    run(async {
        let rig = browser_rig();
        let answer = json!({ "observed": true, "hit": true, "navigated": "https://shop.example/thanks?name=Zo%C3%AB#top", "arrived": true });
        rig.desktop.extension.borrow_mut().insert("verify".into(), answer);
        let task = observe_page(&rig.controller).await;
        let env = call(&rig.controller, "browser_act", click_next(&task, "act-1")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "done", "{env}");
        assert_eq!(step["effect"], "click button \"Next\" · the click reached it; the page then went to https://shop.example/thanks", "{env}");

        // The page unloaded without reporting where the press landed.
        rig.desktop.extension.borrow_mut().insert("verify".into(), json!({ "observed": false, "hit": null, "gone": true }));
        let env = call(&rig.controller, "browser_act", click_next(&task, "act-2")).await;
        let step = &env["result"]["steps"][0];
        assert_eq!(step["outcome"], "unknown", "{env}");
        assert!(step["effect"].as_str().unwrap().contains("went away"), "{env}");
    });
}

#[test]
fn an_agent_that_lost_its_context_finds_its_last_note() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let saved = call(c, "computer_checkpoint", json!({ "task_ref": task, "note": "Saved a.txt; next, send it to Bob." })).await;
        let note = saved["result"]["note_ref"].as_str().unwrap().to_string();
        // The same agent on a new connection, without its transcript.
        let status = |args: Value| c.call("vesper", "connection_b", "codex", "computer_status", args, Cancel::new());
        let task_status = status(json!({ "ref": task })).await.envelope;
        assert!(task_status["result"]["children"].as_array().unwrap().contains(&json!(note)), "{task_status}");
        let next: Vec<&str> = task_status["result"]["next"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(next.iter().any(|n| n.contains("Saved a.txt; next, send it to Bob.")), "{task_status}");
        let read = status(json!({ "ref": note })).await.envelope;
        assert_eq!(
            (read["status"].clone(), read["result"]["kind"].clone(), read["result"]["summary"].clone(), read["result"]["parent"].clone()),
            (json!("ok"), json!("note"), json!("Saved a.txt; next, send it to Bob."), json!(task)),
            "{read}"
        );
        // Nobody else reads it.
        let other = c.call("hazel", "connection_c", "claude", "computer_status", json!({ "ref": note }), Cancel::new()).await.envelope;
        assert!(!other.to_string().contains("Bob"), "{other}");
    });
}

#[test]
fn a_job_whose_reply_was_lost_reads_as_finished() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let task = begin(c).await;
        let ran = call(c, "computer_exec", json!({ "task_ref": task, "request_id": "exec-1", "command": ["sleep", "1"] })).await;
        assert_eq!(ran["status"], "pending", "{ran}");
        let op = ran["result"]["op_ref"].as_str().unwrap().to_string();
        // The agent never hears of the job again; all it has later is the op.
        let mut state = String::new();
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let read = call(c, "computer_status", json!({ "ref": op })).await;
            state = read["result"]["state"].as_str().unwrap_or("").to_string();
            if state != "running" {
                break;
            }
        }
        assert_eq!(state, "done");
    });
}

/// An agent that assesses a check ibara evaluates itself still finishes in
/// one call. Failure cases:
/// 1. The whole finish is refused, so the agent needs a second call.
/// 2. The agent's word replaces ibara's: a check it says is unmet reads unmet
///    (or one it says is met reads met) although the file says otherwise.
/// 3. Nothing tells the agent its assessment was ignored.
/// 4. An assessment of a check the task does not have is accepted silently,
///    or refused without naming the checks the agent may assess.
#[test]
fn an_assessment_of_an_automatic_check_is_ignored_and_the_finish_goes_through() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let checks = json!([
            { "id": "saved", "description": "the note exists", "check": { "kind": "file_exists", "path": "note.txt" } },
            { "id": "missing", "description": "never written", "check": { "kind": "file_exists", "path": "never.txt" } },
            { "id": "reads_well", "description": "the note reads well" },
        ]);
        let begun = call(c, "computer_begin", json!({ "goal": "Write a note", "request_id": "b1", "checks": checks })).await;
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        std::fs::write(Path::new(begun["result"]["workspace"].as_str().unwrap()).join("note.txt"), "hello").unwrap();

        let unknown = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f0", "outcome": "complete", "summary": "done",
            "assessments": [{ "check": "tidy", "met": true, "reason": "looks tidy" }] })).await;
        assert_eq!(code(&unknown), "INVALID_ARGUMENT", "{unknown}");
        let message = unknown["error"]["message"].as_str().unwrap();
        assert!(message.contains("'tidy'") && message.contains("reads_well") && !message.contains("saved"), "names what to assess instead: {message}");

        let finish = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "complete", "summary": "done", "assessments": [
            { "check": "saved", "met": false, "reason": "not sure" },
            { "check": "missing", "met": true, "reason": "I wrote it" },
            { "check": "reads_well", "met": true, "reason": "it does" },
        ] })).await;
        assert_eq!(finish["status"], "ok", "{finish}");
        let state = |id: &str| finish["result"]["checks"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap()["state"].clone();
        assert_eq!((state("saved"), state("missing"), state("reads_well")), (json!("met"), json!("unmet"), json!("met")), "{finish}");
        assert_eq!(finish["result"]["complete"], false, "{finish}");
        let notes = finish["result"]["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1, "{finish}");
        let note = notes[0].as_str().unwrap();
        assert!(note.contains("saved") && note.contains("missing") && !note.contains("reads_well"), "{note}");
    });
}

/// `computer_files` paths and the `computer_exec` cwd may be absolute when
/// they are inside what the task may use. Failure cases:
/// 1. The task's own workspace, named by the absolute path begin returned, is refused.
/// 2. A path in the person's home folder, absolute or `~/`, is refused.
/// 3. A path outside both, or in ibara's own folders (another task's
///    workspace, the journal), is accepted.
/// 4. A refusal does not name the places the agent may use.
/// 5. A file written in the home folder does not link its writer when published.
#[test]
fn absolute_paths_inside_the_workspace_or_the_home_folder_are_accepted() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let begun = call(c, "computer_begin", json!({ "goal": "Read the shopping list", "request_id": "b1" })).await;
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        let workspace = begun["result"]["workspace"].as_str().unwrap().to_string();
        let home = rig.dir.to_str().unwrap().to_string();
        std::fs::create_dir_all(rig.dir.join("Documents")).unwrap();
        std::fs::write(rig.dir.join("Documents/shopping.txt"), "milk, eggs").unwrap();
        let files = |request_id: &str, args: Value| {
            let mut args = args;
            args["task_ref"] = json!(task);
            args["request_id"] = json!(request_id);
            call(c, "computer_files", args)
        };
        let exec = |request_id: &str, command: &[&str], cwd: &str| {
            call(c, "computer_exec", json!({ "task_ref": task, "request_id": request_id, "command": command, "cwd": cwd, "timeout_ms": 5000 }))
        };

        let wrote = files("w1", json!({ "op": "write", "path": format!("{workspace}/in-workspace.txt"), "text": "here" })).await;
        assert_eq!(wrote["status"], "ok", "{wrote}");
        assert_eq!(std::fs::read_to_string(Path::new(&workspace).join("in-workspace.txt")).unwrap(), "here");
        let ran = exec("e1", &["pwd"], &workspace).await;
        assert_eq!(ran["status"], "ok", "{ran}");
        assert_eq!(ran["result"]["job"]["stdout"].as_str().unwrap().trim(), std::fs::canonicalize(&workspace).unwrap().to_str().unwrap(), "{ran}");

        for path in [format!("{home}/Documents/shopping.txt"), "~/Documents/shopping.txt".to_string()] {
            let read = files("r1", json!({ "op": "read", "path": path })).await;
            assert_eq!(read["result"]["text"], "milk, eggs", "{path}: {read}");
        }
        let listed = files("l1", json!({ "op": "list", "dir": format!("{home}/Documents") })).await;
        assert_eq!(listed["result"]["entries"][0]["name"], "shopping.txt", "{listed}");
        let ran = exec("e2", &["cat", "shopping.txt"], &format!("{home}/Documents")).await;
        assert_eq!(ran["result"]["job"]["stdout"], "milk, eggs", "{ran}");
        let wrote = files("w2", json!({ "op": "write", "path": "~/Documents/copy.txt", "text": "milk" })).await;
        assert_eq!(std::fs::read_to_string(rig.dir.join("Documents/copy.txt")).unwrap(), "milk", "{wrote}");
        let published = files("p1", json!({ "op": "publish", "path": format!("{home}/Documents/copy.txt") })).await;
        assert_eq!(published["result"]["author_op"], wrote["result"]["op_ref"], "{published}");

        let other_task = format!("{home}/data/workspaces/task_other/secret.txt");
        std::fs::create_dir_all(rig.dir.join("data/workspaces/task_other")).unwrap();
        std::fs::write(&other_task, "not yours").unwrap();
        let journal = format!("{home}/state/journal.db");
        for (path, says) in [("/etc/hostname", "outside"), (other_task.as_str(), "ibara's own"), (journal.as_str(), "ibara's own")] {
            let read = files("r2", json!({ "op": "read", "path": path })).await;
            assert_eq!(code(&read), "INVALID_ARGUMENT", "{path}: {read}");
            let message = read["error"]["message"].as_str().unwrap();
            assert!(message.contains(says) && message.contains(&workspace) && message.contains(&home), "{path}: {message}");
        }
        let ran = exec("e3", &["pwd"], "/tmp").await;
        assert_eq!(code(&ran), "INVALID_ARGUMENT", "{ran}");
        assert!(ran["error"]["message"].as_str().unwrap().starts_with("cwd: '/tmp'"), "{ran}");
        let up = files("r3", json!({ "op": "read", "path": format!("{workspace}/../task_other/secret.txt") })).await;
        assert!(up["error"]["message"].as_str().unwrap().contains("'..'"), "{up}");
    });
}

/// The file manager launches like the editor, terminal and browser. Failure
/// cases:
/// 1. `file manager`, `files` or `nautilus` is refused as not approved.
/// 2. A goal that names the file manager offers no launch choice.
/// 3. Its window, which the launcher hands to a running Files, is not the
///    task's, so finish leaves it open.
#[test]
fn the_file_manager_launches_by_any_of_its_names_and_finish_closes_its_window() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let begun = call(c, "computer_begin", json!({ "goal": "In the file manager, rename IMG_2041.jpg", "request_id": "b1" })).await;
        let offered = begun["result"]["frame"]["choices"].as_array().unwrap().iter().any(|ch| ch["action"]["kind"] == "launch" && ch["action"]["app"] == "files");
        assert!(offered, "{begun}");
        let task = begun["result"]["task_ref"].as_str().unwrap().to_string();
        for (i, name) in ["file manager", "Files", "nautilus"].into_iter().enumerate() {
            rig.desktop.launch_pid.set(Some(400));
            let address = format!("0x{}", 10 + i);
            rig.desktop.maps_late.replace(Some((std::time::Duration::from_millis(100), win(&address, 401, "org.gnome.Nautilus", "Home", true, false))));
            let env = call(c, "computer_act", json!({ "task_ref": task, "request_id": format!("act-{i}"), "action": { "kind": "launch", "app": name },
                "expect": { "kind": "window", "app": name, "within_ms": 2000 } })).await;
            assert_eq!(env["result"]["steps"][0]["outcome"], "done", "{name}: {env}");
        }
        let finish = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        assert_eq!(finish["result"]["cleanup"]["closed"].as_array().unwrap().len(), 3, "{finish}");
    });
}
