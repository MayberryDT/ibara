//! The person's clipboard, set aside while ibara pastes text into a web page
//! and put back afterwards: every data type it offered, byte for byte. X11
//! selection control targets exposed by GTK are operations, not payloads. Cua types
//! only ASCII, so `browser_act` pastes the rest.
//!
//! A small client of the Wayland data-control protocol
//! (`ext-data-control-v1`, else `zwlr-data-control-unstable-v1`; Hyprland
//! has both). It reads and offers the clipboard without a window or the
//! keyboard focus. `wl-copy` offers a single type, so it cannot put back a
//! copy that offered several (a browser's copy offers text and HTML). What
//! ibara offers is served by a thread of this process until something else
//! is copied. The text ibara pastes is marked secret
//! (`x-kde-passwordManagerHint`), which clipboard histories skip.
//!
//! From the moment the clipboard is set aside until it is put back, the
//! connection that read it stays open and watches every change. ibara's own
//! paste text carries a private type ([`MARKER`]); any other copy in that
//! time is someone else's, so nothing more is pasted over it and it is not
//! replaced by the old clipboard. A clipboard a password manager marked
//! secret is not read and not put back: its manager may clear only a
//! clipboard that still holds its password (KeePassXC compares the text), so
//! a password put back after the manager's timer would stay. The clipboard
//! is left empty instead.

use crate::error::{IbaraError, Result};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Every type the clipboard offered and its bytes, in the order offered.
/// Empty: nothing was copied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedClipboard(pub Vec<(String, Vec<u8>)>);

/// What the clipboard held when it was set aside.
enum Saved {
    Types(SavedClipboard),
    /// Marked secret by a password manager; not read.
    Secret,
}

/// What putting the clipboard back did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutBack {
    /// ibara never changed the clipboard.
    Unchanged,
    /// The person's clipboard is back, every type and byte.
    Restored,
    /// Something else was copied meanwhile; it stays.
    Copied,
    /// The clipboard held a password manager's secret; it is left empty.
    Emptied,
}

/// The most a clipboard may hold, all types together, to be set aside.
pub const SAVE_MAX: usize = 32 * 1024 * 1024;
/// Text as programs ask for it.
const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
/// KDE's convention, which clipboard histories follow: do not keep this.
const SECRET_HINT: (&str, &[u8]) = ("x-kde-passwordManagerHint", b"secret");
/// Marks ibara's paste text, so a copy made meanwhile is told from it.
const MARKER: &str = "application/x-ibara-paste";
/// How long the compositor, and the program that copied for each type, get
/// to answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(2);
/// How long a program that pastes gets to read what ibara offers.
const SERVE_WITHIN: Duration = Duration::from_secs(10);
const MANAGERS: [&str; 2] = ["ext_data_control_manager_v1", "zwlr_data_control_manager_v1"];
const DISPLAY: u32 = 1;

fn failed(what: &str, e: io::Error) -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", format!("The clipboard could not be {what}: {e}."), true)
}

fn too_large() -> io::Error {
    io::Error::other(format!("it holds more than {} MiB", SAVE_MAX / (1024 * 1024)))
}

/// The compositor's socket: `WAYLAND_DISPLAY`, under `XDG_RUNTIME_DIR` unless absolute.
pub fn display() -> Result<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY").filter(|d| !d.is_empty()).unwrap_or_else(|| "wayland-0".into());
    match PathBuf::from(&display) {
        absolute if absolute.is_absolute() => Ok(absolute),
        name => {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).ok_or_else(|| failed("read", io::Error::other("there is no graphical session")))?;
            Ok(PathBuf::from(runtime).join(name))
        }
    }
}

/// The clipboard set aside for a paste sequence, watched until it is put back.
pub struct SetAside {
    display: PathBuf,
    saved: Saved,
    watch: Watch,
    /// ibara has asked to change the clipboard.
    touched: bool,
}

/// Set the clipboard on `display` aside: everything it offers now, every
/// type as announced (wl-copy announces `text/plain` twice), at most
/// [`SAVE_MAX`] bytes in all. From now on every change is watched.
pub fn set_aside(display: &Path) -> Result<SetAside> {
    let read = || -> io::Result<SetAside> {
        let mut session = Session::open(display)?;
        let saved = match session.selection {
            None => Saved::Types(SavedClipboard::default()),
            Some(offer) => {
                let mimes = session.offers.get(&offer).cloned().unwrap_or_default();
                if mimes.iter().any(|m| m == SECRET_HINT.0) && session.receive(offer, SECRET_HINT.0, 64)? == SECRET_HINT.1 {
                    Saved::Secret
                } else {
                    Saved::Types(session.receive_all(offer, mimes)?)
                }
            }
        };
        session.watching = true;
        Ok(SetAside { display: display.to_path_buf(), saved, watch: Watch::start(session)?, touched: false })
    };
    read().map_err(|e| failed("read", e))
}

impl SetAside {
    /// Put `text` on the clipboard for one paste, marked secret, unless
    /// something else was copied since the clipboard was set aside.
    pub fn offer_text(&mut self, text: &str) -> Result<()> {
        if self.watch.copied().map_err(|e| failed("watched", e))? {
            return Err(IbaraError::new(
                "CAPABILITY_UNAVAILABLE",
                "Something was copied while ibara typed, so ibara stopped rather than paste over it; the field holds only part of the text.",
                false,
            )
            .with("reason", "clipboard_copied"));
        }
        self.touched = true;
        let mut types: Vec<(String, Vec<u8>)> = TEXT_TYPES.iter().map(|t| (t.to_string(), text.as_bytes().to_vec())).collect();
        types.push((SECRET_HINT.0.to_string(), SECRET_HINT.1.to_vec()));
        types.push((MARKER.to_string(), Vec::new()));
        offer(&self.display, types).map_err(|e| failed("set", e))
    }

    /// Put the clipboard back, unless ibara never changed it or something
    /// else was copied meanwhile. A secret one is left empty.
    pub fn put_back(self) -> Result<PutBack> {
        let SetAside { display, saved, watch, touched } = self;
        if watch.finish().map_err(|e| failed("put back", e))? {
            return Ok(PutBack::Copied);
        }
        if !touched {
            return Ok(PutBack::Unchanged);
        }
        let set = || -> io::Result<PutBack> {
            match saved {
                Saved::Types(saved) if !saved.0.is_empty() => offer(&display, saved.0).map(|()| PutBack::Restored),
                empty_or_secret => {
                    let mut session = Session::open(&display)?;
                    session.wire.send(session.device, 0, &[Arg::Uint(0)], None)?;
                    session.roundtrip()?;
                    Ok(if matches!(empty_or_secret, Saved::Secret) { PutBack::Emptied } else { PutBack::Restored })
                }
            }
        };
        set().map_err(|e| failed("put back", e))
    }
}

/// Offer `types` as the clipboard and serve them from a thread until
/// something else is copied.
fn offer(display: &Path, types: Vec<(String, Vec<u8>)>) -> io::Result<()> {
    let mut session = Session::open(display)?;
    let source = session.wire.new_id();
    session.wire.send(session.manager, 0, &[Arg::Uint(source)], None)?;
    for (mime, _) in &types {
        session.wire.send(source, 0, &[Arg::Str(mime)], None)?;
    }
    session.wire.send(session.device, 0, &[Arg::Uint(source)], None)?;
    session.source = Some((source, Arc::new(types)));
    // Once this returns the compositor holds the new clipboard, so a paste
    // that follows reads it.
    session.roundtrip()?;
    if !session.cancelled {
        std::thread::Builder::new().name("ibara-clipboard".into()).spawn(move || {
            while !session.cancelled {
                let Ok(Some(event)) = session.wire.event(None) else { break };
                if session.dispatch(event).is_err() {
                    break;
                }
            }
        })?;
    }
    Ok(())
}

/// A question to the watching thread: whether something else was copied,
/// after reading every event the compositor sent so far; `true` ends it.
type Ask = (bool, mpsc::Sender<io::Result<bool>>);

/// The connection that read the clipboard, kept open on its own thread to
/// see every change until the clipboard is put back.
struct Watch {
    asks: mpsc::Sender<Ask>,
    wake: File,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    fn start(mut session: Session) -> io::Result<Watch> {
        let (read, write) = pipe()?;
        let (asks, answers) = mpsc::channel::<Ask>();
        let thread = std::thread::Builder::new().name("ibara-clipboard-watch".into()).spawn(move || {
            let mut wake = File::from(read);
            let mut serve = || -> io::Result<()> {
                loop {
                    let (events, asked) = ready_either(session.wire.sock.as_raw_fd(), wake.as_raw_fd())?;
                    if events {
                        session.wire.receive()?;
                        while let Some(event) = session.wire.event(Some(Instant::now()))? {
                            session.dispatch(event)?;
                        }
                    }
                    if asked {
                        let _ = wake.read(&mut [0u8; 64])?;
                        while let Ok((last, reply)) = answers.try_recv() {
                            let _ = reply.send(session.roundtrip().map(|()| session.copied));
                            if last {
                                return Ok(());
                            }
                        }
                    }
                }
            };
            if let Err(e) = serve() {
                // Later questions get the reason instead of an answer.
                while let Ok((_, reply)) = answers.recv_timeout(Duration::ZERO) {
                    let _ = reply.send(Err(io::Error::new(e.kind(), e.to_string())));
                }
            }
        })?;
        Ok(Watch { asks, wake: File::from(write), thread: Some(thread) })
    }

    fn ask(&self, last: bool) -> io::Result<bool> {
        let (reply, answer) = mpsc::channel();
        let gone = || io::Error::other("the watch on the clipboard stopped");
        self.asks.send((last, reply)).map_err(|_| gone())?;
        (&self.wake).write_all(&[1])?;
        answer.recv_timeout(ANSWER_WITHIN + Duration::from_secs(1)).map_err(|_| gone())?
    }

    /// Whether something else was copied since the clipboard was set aside.
    fn copied(&self) -> io::Result<bool> {
        self.ask(false)
    }

    /// The same, and the watch ends.
    fn finish(mut self) -> io::Result<bool> {
        let copied = self.ask(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        copied
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let (reply, _) = mpsc::channel();
            let _ = self.asks.send((true, reply));
            let _ = (&self.wake).write_all(&[1]);
        }
    }
}

/// Whether `a` (the compositor) and `b` (the wake pipe) are readable; waits
/// until one is.
fn ready_either(a: RawFd, b: RawFd) -> io::Result<(bool, bool)> {
    let mut fds = [libc::pollfd { fd: a, events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: b, events: libc::POLLIN, revents: 0 }];
    loop {
        // SAFETY: two valid pollfds.
        match unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } {
            n if n > 0 => return Ok((fds[0].revents != 0, fds[1].revents != 0)),
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// One connection bound to the seat's data-control device.
struct Session {
    wire: Wire,
    manager: u32,
    device: u32,
    /// Offers the compositor announced, with their types.
    offers: HashMap<u32, Vec<String>>,
    /// The offer that is the clipboard now; `None` when nothing is copied.
    selection: Option<u32>,
    /// ibara's own offer and what it serves.
    source: Option<(u32, Arc<Vec<(String, Vec<u8>)>>)>,
    cancelled: bool,
    /// Every new clipboard is looked at: one without [`MARKER`] is someone
    /// else's copy (`copied`).
    watching: bool,
    copied: bool,
}

impl Session {
    fn open(display: &Path) -> io::Result<Session> {
        let mut wire = Wire::connect(display)?;
        let registry = wire.new_id();
        wire.send(DISPLAY, 1, &[Arg::Uint(registry)], None)?;
        let done = wire.new_id();
        wire.send(DISPLAY, 0, &[Arg::Uint(done)], None)?;
        let deadline = Instant::now() + ANSWER_WITHIN;
        let mut globals: Vec<(u32, String)> = Vec::new();
        loop {
            let event = wire.event(Some(deadline))?.ok_or_else(|| io::Error::other("the compositor did not answer"))?;
            match (event.object, event.opcode) {
                (DISPLAY, 0) => return Err(refusal(&event)),
                (o, 0) if o == registry => {
                    let mut body = Body(&event.body);
                    globals.push((body.uint()?, body.string()?));
                }
                (o, 0) if o == done => break,
                _ => {}
            }
        }
        let global = |interface: &str| globals.iter().find(|(_, i)| i == interface).map(|(name, _)| *name);
        let (name, interface) = MANAGERS
            .iter()
            .find_map(|i| global(i).map(|name| (name, *i)))
            .ok_or_else(|| io::Error::other("this desktop does not let programs use the clipboard (no data-control protocol)"))?;
        let seat = global("wl_seat").ok_or_else(|| io::Error::other("this desktop has no seat"))?;
        let manager = wire.bind(registry, name, interface)?;
        let seat = wire.bind(registry, seat, "wl_seat")?;
        let device = wire.new_id();
        wire.send(manager, 1, &[Arg::Uint(device), Arg::Uint(seat)], None)?;
        let mut session = Session { wire, manager, device, offers: HashMap::new(), selection: None, source: None, cancelled: false, watching: false, copied: false };
        // The device announces the clipboard as it is now.
        session.roundtrip()?;
        Ok(session)
    }

    /// Wait until the compositor has handled every request sent so far.
    fn roundtrip(&mut self) -> io::Result<()> {
        let done = self.wire.new_id();
        self.wire.send(DISPLAY, 0, &[Arg::Uint(done)], None)?;
        let deadline = Instant::now() + ANSWER_WITHIN;
        loop {
            let event = self.wire.event(Some(deadline))?.ok_or_else(|| io::Error::other("the compositor did not answer"))?;
            if event.object == done {
                return Ok(());
            }
            self.dispatch(event)?;
        }
    }

    fn dispatch(&mut self, event: Event) -> io::Result<()> {
        let mut body = Body(&event.body);
        let source = self.source.as_ref().map(|(id, _)| *id);
        match (event.object, event.opcode) {
            (DISPLAY, 0) => return Err(refusal(&event)),
            // device.data_offer, then offer.offer for each type, then device.selection.
            (o, 0) if o == self.device => {
                self.offers.insert(body.uint()?, Vec::new());
            }
            (o, 1) if o == self.device => {
                self.selection = Some(body.uint()?).filter(|offer| *offer != 0);
                if self.watching {
                    // Nothing copied (a program that copied quit) is no copy.
                    if let Some(offer) = self.selection
                        && !self.offers.get(&offer).is_some_and(|types| types.iter().any(|t| t == MARKER))
                    {
                        self.copied = true;
                    }
                    // Offers that are no longer the clipboard are let go.
                    let stale: Vec<u32> = self.offers.keys().copied().filter(|o| Some(*o) != self.selection).collect();
                    for offer in stale {
                        self.offers.remove(&offer);
                        self.wire.send(offer, 1, &[], None)?;
                    }
                }
            }
            (o, 2) if o == self.device => return Err(io::Error::other("the compositor withdrew clipboard access")),
            (o, 0) if self.offers.contains_key(&o) => {
                let mime = body.string()?;
                self.offers.entry(o).or_default().push(mime);
            }
            // source.send: a program pastes; write that type into its pipe.
            (o, 0) if Some(o) == source => {
                let mime = body.string()?;
                let fd = self.wire.fds.pop_front();
                if let (Some(fd), Some((_, types))) = (fd, &self.source) {
                    serve(types.clone(), &mime, fd);
                }
            }
            // source.cancelled: something else was copied.
            (o, 1) if Some(o) == source => self.cancelled = true,
            _ => {}
        }
        Ok(())
    }

    /// Every type of `offer`, as announced, at most [`SAVE_MAX`] bytes in all.
    fn receive_all(&mut self, offer: u32, mimes: Vec<String>) -> io::Result<SavedClipboard> {
        let mut saved: Vec<(String, Vec<u8>)> = Vec::with_capacity(mimes.len());
        let mut room = SAVE_MAX;
        for mime in mimes {
            // GTK can expose X11 selection protocol targets on Wayland.
            // They describe/manage the selection; requesting one as data can
            // hang (SAVE_TARGETS) or cause effects (DELETE/INSERT_*). Preserve
            // every actual payload type; an unreadable payload still refuses.
            if matches!(mime.as_str(), "SAVE_TARGETS" | "TARGETS" | "TIMESTAMP" | "MULTIPLE" | "DELETE" | "INSERT_SELECTION" | "INSERT_PROPERTY") { continue; }
            let data = match saved.iter().find(|(m, _)| *m == mime) {
                Some((_, data)) if data.len() <= room => data.clone(),
                Some(_) => return Err(too_large()),
                None => self.receive(offer, &mime, room)?,
            };
            room -= data.len();
            saved.push((mime, data));
        }
        Ok(SavedClipboard(saved))
    }

    /// One type of the clipboard, at most `room` bytes.
    fn receive(&mut self, offer: u32, mime: &str, room: usize) -> io::Result<Vec<u8>> {
        let (read, write) = pipe()?;
        self.wire.send(offer, 0, &[Arg::Str(mime)], Some(write.as_raw_fd()))?;
        drop(write);
        let mut file = File::from(read);
        let deadline = Instant::now() + ANSWER_WITHIN;
        let mut data = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if !ready(file.as_raw_fd(), libc::POLLIN, Some(deadline))? {
                return Err(io::Error::other(format!("the program that copied it did not hand over its {mime} in time")));
            }
            let n = match file.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Ok(data);
            }
            if data.len() + n > room {
                return Err(too_large());
            }
            data.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Write the `mime` type of `types` into a pasting program's pipe, from its
/// own thread so a slow reader holds up nothing else.
fn serve(types: Arc<Vec<(String, Vec<u8>)>>, mime: &str, fd: OwnedFd) {
    let Some(index) = types.iter().position(|(m, _)| m == mime) else { return };
    let _ = std::thread::Builder::new().name("ibara-clipboard-send".into()).spawn(move || {
        let raw = fd.as_raw_fd();
        // SAFETY: `raw` is the open pipe `fd` owns.
        unsafe { libc::fcntl(raw, libc::F_SETFL, libc::fcntl(raw, libc::F_GETFL) | libc::O_NONBLOCK) };
        let mut file = File::from(fd);
        let mut rest: &[u8] = &types[index].1;
        let deadline = Instant::now() + SERVE_WITHIN;
        while !rest.is_empty() {
            match ready(raw, libc::POLLOUT, Some(deadline)) {
                Ok(true) => {}
                _ => return,
            }
            match file.write(rest) {
                Ok(n) => rest = &rest[n..],
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(_) => return,
            }
        }
    });
}

/// The compositor's `wl_display.error`.
fn refusal(event: &Event) -> io::Error {
    let mut body = Body(&event.body);
    let message = body.uint().and_then(|_| body.uint()).and_then(|_| body.string()).unwrap_or_default();
    io::Error::other(format!("the compositor refused: {message}"))
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are new and owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Whether `fd` is ready for `events` before `deadline` (`None`: wait).
fn ready(fd: RawFd, events: libc::c_short, deadline: Option<Instant>) -> io::Result<bool> {
    loop {
        let timeout = match deadline {
            None => -1,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                left.as_millis().clamp(1, i32::MAX as u128) as i32
            }
        };
        let mut poll = libc::pollfd { fd, events, revents: 0 };
        // SAFETY: one valid pollfd.
        match unsafe { libc::poll(&mut poll, 1, timeout) } {
            n if n > 0 => return Ok(true),
            0 => continue,
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

enum Arg<'a> {
    Uint(u32),
    Str(&'a str),
}

struct Event {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

/// An event's arguments, read in order.
struct Body<'a>(&'a [u8]);

impl Body<'_> {
    fn uint(&mut self) -> io::Result<u32> {
        let (head, rest) = self.0.split_at_checked(4).ok_or_else(short)?;
        self.0 = rest;
        Ok(u32::from_ne_bytes(head.try_into().unwrap_or_default()))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.uint()? as usize;
        if len == 0 {
            return Ok(String::new());
        }
        let (head, rest) = self.0.split_at_checked(len.div_ceil(4) * 4).ok_or_else(short)?;
        self.0 = rest;
        String::from_utf8(head[..len - 1].to_vec()).map_err(|_| short())
    }
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "a malformed message from the compositor")
}

/// The Wayland wire: messages of 32-bit words in host order, descriptors
/// alongside as `SCM_RIGHTS`.
struct Wire {
    sock: UnixStream,
    next: u32,
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Wire {
    fn connect(display: &Path) -> io::Result<Wire> {
        Ok(Wire { sock: UnixStream::connect(display)?, next: 2, buf: Vec::new(), fds: VecDeque::new() })
    }

    fn new_id(&mut self) -> u32 {
        self.next += 1;
        self.next - 1
    }

    /// `wl_registry.bind` at version 1.
    fn bind(&mut self, registry: u32, name: u32, interface: &str) -> io::Result<u32> {
        let id = self.new_id();
        self.send(registry, 0, &[Arg::Uint(name), Arg::Str(interface), Arg::Uint(1), Arg::Uint(id)], None)?;
        Ok(id)
    }

    fn send(&self, object: u32, opcode: u16, args: &[Arg], fd: Option<RawFd>) -> io::Result<()> {
        let mut message = Vec::with_capacity(64);
        message.extend_from_slice(&object.to_ne_bytes());
        message.extend_from_slice(&[0; 4]);
        for arg in args {
            match arg {
                Arg::Uint(v) => message.extend_from_slice(&v.to_ne_bytes()),
                Arg::Str(s) => {
                    message.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
                    message.extend_from_slice(s.as_bytes());
                    message.push(0);
                    message.resize(message.len().div_ceil(4) * 4, 0);
                }
            }
        }
        if message.len() > 4096 {
            return Err(io::Error::other("a clipboard type name is too long"));
        }
        let word = ((message.len() as u32) << 16) | u32::from(opcode);
        message[4..8].copy_from_slice(&word.to_ne_bytes());
        let mut sent = 0;
        let mut fd = fd;
        while sent < message.len() {
            let rest = &message[sent..];
            let mut iov = libc::iovec { iov_base: rest.as_ptr() as *mut libc::c_void, iov_len: rest.len() };
            let mut control = [0u64; 4];
            // SAFETY: a zeroed msghdr is valid; it points at `iov` and `control`, which outlive the call.
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = &mut iov;
            header.msg_iovlen = 1;
            if let Some(fd) = fd {
                // SAFETY: `control` (32 bytes, 8-aligned) holds one cmsghdr with one descriptor.
                unsafe {
                    header.msg_control = control.as_mut_ptr().cast();
                    header.msg_controllen = libc::CMSG_SPACE(4) as _;
                    let cmsg = libc::CMSG_FIRSTHDR(&header);
                    (*cmsg).cmsg_level = libc::SOL_SOCKET;
                    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                    (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
                    std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
                }
            }
            // SAFETY: `header` is fully initialized above.
            let n = unsafe { libc::sendmsg(self.sock.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            sent += n as usize;
            fd = None;
        }
        Ok(())
    }

    /// The next event; `None` when `deadline` passes first.
    fn event(&mut self, deadline: Option<Instant>) -> io::Result<Option<Event>> {
        loop {
            if self.buf.len() >= 8 {
                let object = u32::from_ne_bytes(self.buf[0..4].try_into().unwrap_or_default());
                let word = u32::from_ne_bytes(self.buf[4..8].try_into().unwrap_or_default());
                let size = (word >> 16) as usize;
                if size < 8 || size % 4 != 0 {
                    return Err(short());
                }
                if self.buf.len() >= size {
                    let body = self.buf[8..size].to_vec();
                    self.buf.drain(..size);
                    return Ok(Some(Event { object, opcode: (word & 0xffff) as u16, body }));
                }
            }
            if !ready(self.sock.as_raw_fd(), libc::POLLIN, deadline)? {
                return Ok(None);
            }
            self.receive()?;
        }
    }

    fn receive(&mut self) -> io::Result<()> {
        let mut data = [0u8; 4096];
        let mut control = [0u64; 16];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        // SAFETY: a zeroed msghdr is valid; it points at `iov` and `control`, which outlive the call.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = loop {
            // SAFETY: `header` describes writable buffers owned by this frame.
            let n = unsafe { libc::recvmsg(self.sock.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        };
        // SAFETY: the kernel filled `control` with well-formed cmsghdrs up to msg_controllen.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&header);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    let first = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..count {
                        self.fds.push_back(OwnedFd::from_raw_fd(std::ptr::read_unaligned(first.add(i))));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&header, cmsg);
            }
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the compositor closed the connection"));
        }
        self.buf.extend_from_slice(&data[..n]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Against a stand-in compositor that speaks the data-control protocol
    //! and lets a person copy. Failure cases, written first:
    //! - the person's clipboard is not the same afterwards, every type and
    //!   byte;
    //! - a copy made after the clipboard was set aside (before the first
    //!   paste, between pastes, after the last) is pasted over or replaced by
    //!   the old clipboard;
    //! - a paste offer that fails after the compositor took it leaves the
    //!   clipboard empty;
    //! - a password manager's secret is read, or put back;
    //! - a clipboard ibara never changed is replaced.
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;

    type Types = Vec<(String, Vec<u8>)>;

    enum Owner {
        Person(Types),
        Client { conn: usize, source: u32 },
    }

    enum Obj {
        Registry,
        Manager,
        Device,
        Source(Vec<String>),
        Offer,
        Other,
    }

    struct Conn {
        writer: Wire,
        objects: HashMap<u32, Obj>,
        next: u32,
        alive: bool,
        /// The next sync after a set_selection is not answered.
        stall: bool,
    }

    #[derive(Default)]
    struct Compositor {
        conns: Vec<Conn>,
        selection: Option<Owner>,
        stall_next_set: bool,
        /// Types programs read from a person's copy.
        received: Vec<String>,
    }

    impl Compositor {
        fn types(&self) -> Vec<String> {
            match &self.selection {
                None => Vec::new(),
                Some(Owner::Person(types)) => types.iter().map(|(m, _)| m.clone()).collect(),
                Some(Owner::Client { conn, source }) => match self.conns[*conn].objects.get(source) {
                    Some(Obj::Source(types)) => types.clone(),
                    _ => Vec::new(),
                },
            }
        }

        /// Tell every device, or only `only` (a new one), what the clipboard is.
        fn announce(&mut self, only: Option<(usize, u32)>) {
            let types = self.types();
            let some = self.selection.is_some();
            for (index, conn) in self.conns.iter_mut().enumerate().filter(|(_, c)| c.alive) {
                let devices: Vec<u32> = conn
                    .objects
                    .iter()
                    .filter(|(id, o)| matches!(o, Obj::Device) && only.is_none_or(|(c, d)| c == index && d == **id))
                    .map(|(id, _)| *id)
                    .collect();
                for device in devices {
                    if !some {
                        let _ = conn.writer.send(device, 1, &[Arg::Uint(0)], None);
                        continue;
                    }
                    conn.next += 1;
                    let offer = conn.next;
                    conn.objects.insert(offer, Obj::Offer);
                    let _ = conn.writer.send(device, 0, &[Arg::Uint(offer)], None);
                    for mime in &types {
                        let _ = conn.writer.send(offer, 0, &[Arg::Str(mime)], None);
                    }
                    let _ = conn.writer.send(device, 1, &[Arg::Uint(offer)], None);
                }
            }
        }

        fn set(&mut self, owner: Option<Owner>) {
            if let Some(Owner::Client { conn, source }) = &self.selection {
                let replaced = !matches!(&owner, Some(Owner::Client { conn: c, source: s }) if c == conn && s == source);
                if replaced && self.conns[*conn].alive {
                    let _ = self.conns[*conn].writer.send(*source, 1, &[], None);
                }
            }
            self.selection = owner;
            self.announce(None);
        }

        fn request(&mut self, conn: usize, event: Event, fds: &mut VecDeque<OwnedFd>) {
            let mut body = Body(&event.body);
            let object = self.conns[conn].objects.get(&event.object).map(|o| match o {
                Obj::Registry => 1,
                Obj::Manager => 2,
                Obj::Device => 3,
                Obj::Source(_) => 4,
                Obj::Offer => 5,
                Obj::Other => 0,
            });
            let stall = event.object != DISPLAY && object == Some(3) && event.opcode == 0 && std::mem::take(&mut self.stall_next_set);
            let c = &mut self.conns[conn];
            match (event.object, object, event.opcode) {
                (DISPLAY, _, 0) => {
                    let done = body.uint().unwrap();
                    if !std::mem::take(&mut c.stall) {
                        let _ = c.writer.send(done, 0, &[Arg::Uint(0)], None);
                    }
                }
                (DISPLAY, _, 1) => {
                    let registry = body.uint().unwrap();
                    c.objects.insert(registry, Obj::Registry);
                    let _ = c.writer.send(registry, 0, &[Arg::Uint(1), Arg::Str("ext_data_control_manager_v1"), Arg::Uint(1)], None);
                    let _ = c.writer.send(registry, 0, &[Arg::Uint(2), Arg::Str("wl_seat"), Arg::Uint(1)], None);
                }
                (_, Some(1), 0) => {
                    let name = body.uint().unwrap();
                    let _interface = body.string().unwrap();
                    let _version = body.uint().unwrap();
                    let id = body.uint().unwrap();
                    c.objects.insert(id, if name == 1 { Obj::Manager } else { Obj::Other });
                }
                (_, Some(2), 0) => {
                    c.objects.insert(body.uint().unwrap(), Obj::Source(Vec::new()));
                }
                (_, Some(2), 1) => {
                    let device = body.uint().unwrap();
                    c.objects.insert(device, Obj::Device);
                    self.announce(Some((conn, device)));
                }
                (source, Some(4), 0) => {
                    let mime = body.string().unwrap();
                    if let Some(Obj::Source(types)) = c.objects.get_mut(&source) {
                        types.push(mime);
                    }
                }
                (_, Some(3), 0) => {
                    let source = body.uint().unwrap();
                    c.stall = stall;
                    self.set((source != 0).then_some(Owner::Client { conn, source }));
                }
                (_, Some(5), 0) => {
                    let mime = body.string().unwrap();
                    let fd = fds.pop_front().unwrap();
                    match &self.selection {
                        Some(Owner::Person(types)) => {
                            self.received.push(mime.clone());
                            if let Some((_, data)) = types.iter().find(|(m, _)| *m == mime) {
                                let _ = File::from(fd).write_all(data);
                            }
                        }
                        Some(Owner::Client { conn, source }) => {
                            let _ = self.conns[*conn].writer.send(*source, 0, &[Arg::Str(&mime)], Some(fd.as_raw_fd()));
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }

    struct Fake {
        dir: PathBuf,
        socket: PathBuf,
        state: Arc<Mutex<Compositor>>,
    }

    impl Fake {
        fn start() -> Fake {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = PathBuf::from(format!("/tmp/ibs-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("wayland");
            let listener = UnixListener::bind(&socket).unwrap();
            let state = Arc::new(Mutex::new(Compositor::default()));
            let shared = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let writer = Wire { sock: stream.try_clone().unwrap(), next: 0, buf: Vec::new(), fds: VecDeque::new() };
                    let conn = {
                        let mut state = shared.lock().unwrap();
                        state.conns.push(Conn { writer, objects: HashMap::new(), next: 0xff00_0000, alive: true, stall: false });
                        state.conns.len() - 1
                    };
                    let shared = shared.clone();
                    std::thread::spawn(move || {
                        let mut reader = Wire { sock: stream, next: 0, buf: Vec::new(), fds: VecDeque::new() };
                        while let Ok(Some(event)) = reader.event(None) {
                            shared.lock().unwrap().request(conn, event, &mut reader.fds);
                        }
                        let mut state = shared.lock().unwrap();
                        state.conns[conn].alive = false;
                        if matches!(state.selection, Some(Owner::Client { conn: c, .. }) if c == conn) {
                            state.set(None);
                        }
                    });
                }
            });
            Fake { dir, socket, state }
        }

        /// The person copies `types`.
        fn copy(&self, types: &[(&str, &[u8])]) {
            let types = types.iter().map(|(m, d)| (m.to_string(), d.to_vec())).collect();
            self.state.lock().unwrap().set(Some(Owner::Person(types)));
        }

        /// What a program that pastes every type reads now.
        fn clipboard(&self) -> Option<Types> {
            let (types, owner) = {
                let state = self.state.lock().unwrap();
                let owner = match &state.selection {
                    None => return None,
                    Some(Owner::Person(types)) => return Some(types.clone()),
                    Some(Owner::Client { conn, source }) => (*conn, *source),
                };
                (state.types(), owner)
            };
            let mut read = Vec::new();
            for mime in types {
                let (out, into) = pipe().unwrap();
                self.state.lock().unwrap().conns[owner.0].writer.send(owner.1, 0, &[Arg::Str(&mime)], Some(into.as_raw_fd())).unwrap();
                drop(into);
                let mut data = Vec::new();
                File::from(out).read_to_end(&mut data).unwrap();
                read.push((mime, data));
            }
            Some(read)
        }

        fn text(&self) -> Option<String> {
            let types = self.clipboard()?;
            types.into_iter().find(|(m, _)| m == "text/plain").map(|(_, d)| String::from_utf8(d).unwrap())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const PICTURE: &[u8] = &[137, 80, 78, 71, 0, 255, 13, 10];

    fn persons_copy(fake: &Fake) -> Types {
        fake.copy(&[("image/png", PICTURE), ("text/html", b"<img src=\"a.png\">"), ("text/plain", b"a picture")]);
        fake.clipboard().unwrap()
    }

    #[test]
    fn the_persons_clipboard_comes_back_every_type_and_byte() {
        let fake = Fake::start();
        let before = persons_copy(&fake);
        let mut aside = set_aside(&fake.socket).unwrap();
        aside.offer_text("ë").unwrap();
        assert_eq!(fake.text().as_deref(), Some("ë"), "a paste reads the text");
        let offered = fake.clipboard().unwrap();
        assert!(offered.iter().any(|(m, d)| m == SECRET_HINT.0 && d == SECRET_HINT.1), "clipboard histories skip it");
        aside.offer_text("Å").unwrap();
        assert_eq!(fake.text().as_deref(), Some("Å"));
        assert_eq!(aside.put_back().unwrap(), PutBack::Restored);
        assert_eq!(fake.clipboard().unwrap(), before);
    }

    #[test]
    fn a_copy_made_after_the_clipboard_was_set_aside_is_kept() {
        // (pastes before the copy, pastes after it)
        for (before, after) in [(0, 1), (1, 1), (2, 0)] {
            let fake = Fake::start();
            persons_copy(&fake);
            let mut aside = set_aside(&fake.socket).unwrap();
            for piece in 0..before {
                aside.offer_text(&format!("piece {piece}")).unwrap();
            }
            fake.copy(&[("text/plain", b"the person's new copy")]);
            for _ in 0..after {
                let refused = aside.offer_text("ö").unwrap_err();
                assert_eq!(refused.details["reason"], serde_json::json!("clipboard_copied"), "{before}: {refused:?}");
            }
            assert_eq!(fake.text().as_deref(), Some("the person's new copy"), "{before}: nothing pasted over it");
            assert_eq!(aside.put_back().unwrap(), PutBack::Copied, "{before}");
            assert_eq!(fake.text().as_deref(), Some("the person's new copy"), "{before}: not replaced by the old clipboard");
        }
    }

    #[test]
    fn a_paste_offer_that_fails_does_not_leave_the_clipboard_empty() {
        let fake = Fake::start();
        let before = persons_copy(&fake);
        let mut aside = set_aside(&fake.socket).unwrap();
        aside.offer_text("ë").unwrap();
        // The compositor takes the next offer and then does not answer.
        fake.state.lock().unwrap().stall_next_set = true;
        let failed = aside.offer_text("Å").unwrap_err();
        assert!(failed.message.contains("could not be set"), "{failed:?}");
        assert_eq!(aside.put_back().unwrap(), PutBack::Restored);
        assert_eq!(fake.clipboard().unwrap(), before);
    }

    #[test]
    fn a_password_managers_secret_is_neither_read_nor_put_back() {
        let fake = Fake::start();
        fake.copy(&[("text/plain", b"correct horse"), (SECRET_HINT.0, SECRET_HINT.1)]);
        let mut aside = set_aside(&fake.socket).unwrap();
        assert_eq!(fake.state.lock().unwrap().received, [SECRET_HINT.0], "only the mark is read, not the password");
        aside.offer_text("ë").unwrap();
        assert_eq!(aside.put_back().unwrap(), PutBack::Emptied);
        assert_eq!(fake.clipboard(), None, "left empty, as the manager would leave it");

        // A secret ibara never replaced stays for its manager to clear.
        fake.copy(&[("text/plain", b"correct horse"), (SECRET_HINT.0, SECRET_HINT.1)]);
        let aside = set_aside(&fake.socket).unwrap();
        assert_eq!(aside.put_back().unwrap(), PutBack::Unchanged);
        assert_eq!(fake.text().as_deref(), Some("correct horse"));
    }
}
