//! Live video of a display for the console's fleet cards (opt-in, preview).
//!
//! `wf-recorder` encodes one display with the hardware H.264 encoder
//! (VA-API) into MPEG-TS; the bytes collect in a ring buffer the console
//! reads with a cursor (`observe_video`). Each viewer has one encoder per
//! display and size, started under the access they have now: when that
//! access changes it starts again, so nobody reads bytes recorded before they
//! were allowed. It stops [`IDLE`] after the last read, and the controller
//! stops it when the screen locks or a person takes control; the next read
//! starts it again.
//!
//! A cursor is an absolute position: the encoder's generation in the high
//! bits, its byte offset in the low 32. A new stream always begins at an
//! encoder's first byte, so a decoder gets the stream headers and the first
//! keyframe. A reader without a cursor starts live: an encoder that began
//! more than [`FRESH`] ago (or no longer holds its first byte) starts again
//! for it, because a player plays a backlog at the pace it was recorded and
//! would stay that far behind.
//!
//! Only computers with a working hardware encoder stream: probed once with
//! `wf-recorder -c h264_vaapi` on each VA-API render node, and cached.

use crate::error::{IbaraError, Result, invalid};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// The largest picture a stream has.
pub const MAX_WIDTH: u32 = 640;
pub const MAX_HEIGHT: u32 = 360;
/// The smallest edge a stream has.
const MIN_EDGE: u32 = 16;
/// Bytes each encoder keeps for its readers.
pub const RING_BYTES: usize = 4 * 1024 * 1024;
/// Bytes one read returns at most.
pub const MAX_CHUNK: usize = 1024 * 1024;
/// An encoder nobody reads stops after this.
pub const IDLE: Duration = Duration::from_secs(5);
/// An encoder that ended is not started again before this.
pub const ENDED_HOLD: Duration = Duration::from_secs(5);
/// A reader without a cursor gets an encoder at most this old; an older one starts again.
pub const FRESH: Duration = Duration::from_secs(1);
/// How long the capability probe may take on one render node.
const PROBE_DEADLINE: Duration = Duration::from_secs(4);
const TS_PACKET: usize = 188;
/// A stream's offsets stay below this; the generation takes the bits above.
const STREAM_SPAN: u64 = 1 << 32;
/// Generations wrap here, so every cursor stays an exact JSON number.
const GENERATIONS: u64 = 1 << 20;

pub const UNSUPPORTED: &str = "This computer can't stream video efficiently.";
const NOT_INSTALLED: &str = "This computer can't stream video: wf-recorder isn't installed.";
const CHECKING: &str = "Checking whether this computer can stream video.";

pub fn unsupported(message: &str) -> IbaraError {
    IbaraError::new("VIDEO_UNSUPPORTED", message, false)
}

/// Whether this computer streams video, and in plain words why not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoCapability {
    pub capable: bool,
    pub reason: Option<String>,
}

/// One read of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Where the next read continues.
    pub cursor: u64,
    pub data: Vec<u8>,
    /// The reader's cursor could not be served: `data` begins a new stream.
    pub reset: bool,
    /// The encoder stopped and every byte has been read.
    pub ended: bool,
    pub width: u32,
    pub height: u32,
}

/// The size asked for, fit inside [`MAX_WIDTH`]×[`MAX_HEIGHT`] with its
/// aspect ratio, in even numbers (the encoder's chroma needs them).
pub fn clamp_size(width: i64, height: i64) -> Result<(u32, u32)> {
    if width < 1 || height < 1 || width > 1 << 16 || height > 1 << 16 {
        return Err(invalid("Expected a width and height in pixels.").with("field", "width/height"));
    }
    let (w, h) = super::capture::fit_inside(width as u32, height as u32, MAX_WIDTH, MAX_HEIGHT);
    Ok(((w & !1).max(MIN_EDGE), (h & !1).max(MIN_EDGE)))
}

/// The bytes one encoder wrote, the oldest dropped past `cap` in whole
/// MPEG-TS packets.
#[derive(Debug)]
pub struct Ring {
    base: u64,
    /// Offset of the first byte held.
    start: u64,
    bytes: VecDeque<u8>,
    cap: usize,
    ended_at: Option<Instant>,
}

/// What [`Ring::read`] served.
#[derive(Debug, PartialEq)]
pub struct Served {
    pub cursor: u64,
    pub data: Vec<u8>,
    pub reset: bool,
}

impl Ring {
    pub fn new(generation: u64, cap: usize) -> Ring {
        Ring { base: (generation % GENERATIONS) * STREAM_SPAN, start: 0, bytes: VecDeque::new(), cap, ended_at: None }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        if self.bytes.len() > self.cap {
            let drop = (self.bytes.len() - self.cap).div_ceil(TS_PACKET) * TS_PACKET;
            let drop = drop.min(self.bytes.len());
            self.bytes.drain(..drop);
            self.start += drop as u64;
        }
    }

    /// Offset just past the last byte written.
    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    /// The cursor just past the last byte written.
    pub fn end_cursor(&self) -> u64 {
        self.base + self.end()
    }

    fn full(&self) -> bool {
        self.end() >= STREAM_SPAN - self.cap as u64
    }

    /// At most `max` bytes from `cursor`. A reader without a cursor, or with
    /// one this ring cannot serve, starts over at the stream's first byte
    /// (`reset` when it had a cursor); `None` when that byte is gone.
    pub fn read(&self, cursor: Option<u64>, max: usize) -> Option<Served> {
        let held = self.base + self.start..=self.base + self.end();
        let (from, reset) = match cursor {
            Some(c) if held.contains(&c) => (c - self.base, false),
            _ if self.start > 0 => return None,
            other => (0, other.is_some()),
        };
        Some(self.serve(from, max, reset))
    }

    /// From the oldest byte held, as a new stream.
    fn read_oldest(&self, max: usize) -> Served {
        self.serve(self.start, max, true)
    }

    fn serve(&self, from: u64, max: usize, reset: bool) -> Served {
        let skip = (from - self.start) as usize;
        let n = (self.bytes.len() - skip).min(max);
        let mut data = Vec::with_capacity(n);
        let (a, b) = self.bytes.as_slices();
        let (end, first) = (skip + n, a.len());
        if skip < first {
            data.extend_from_slice(&a[skip..end.min(first)]);
        }
        if end > first {
            data.extend_from_slice(&b[skip.saturating_sub(first)..end - first]);
        }
        Served { cursor: self.base + from + n as u64, data, reset }
    }
}

/// What one person watches: their own encoder of a display at a size, under
/// the access they had when it started.
#[derive(Debug, Clone, Copy)]
pub struct Watch<'a> {
    /// The principal reading.
    pub viewer: &'a str,
    /// Changes whenever what the viewer may see changes (access revision,
    /// grant generation): an encoder started under other access restarts,
    /// so nobody reads bytes recorded before they were allowed.
    pub access: &'a str,
    pub display: &'a str,
    pub width: u32,
    pub height: u32,
}

/// Viewer, display and size: every viewer has encoders of their own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    viewer: String,
    display: String,
    width: u32,
    height: u32,
}

struct Encoder {
    ring: Arc<Mutex<Ring>>,
    stop: super::Cancel,
    started: Instant,
    last_read: Instant,
    /// [`Watch::access`] when it started.
    access: String,
}

impl Encoder {
    fn stop(&self) {
        self.stop.cancel();
    }
}

#[derive(Default)]
struct State {
    encoders: HashMap<Key, Encoder>,
    /// The idle sweep runs while any encoder does.
    sweeping: bool,
    generation: u64,
}

/// The probe's answer: the render node that encodes, or why none does.
type Probed = std::result::Result<PathBuf, String>;

struct Inner {
    program: PathBuf,
    env: Arc<[(OsString, OsString)]>,
    state: Mutex<State>,
    probed: tokio::sync::OnceCell<Probed>,
    probing: AtomicBool,
}

/// Every stream of this computer. Cheap to clone.
#[derive(Clone)]
pub struct Video(Arc<Inner>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `wf-recorder` streaming `display` at `width`×`height` to stdout. The
/// probe adds `-D` so a still screen still gives frames.
fn encoder_args(node: &Path, display: &str, width: u32, height: u32, fps: u32, every_frame: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-y".into()];
    if every_frame {
        args.push("-D".into());
    }
    for arg in ["-m", "mpegts", "-c", "h264_vaapi", "-d"] {
        args.push(arg.into());
    }
    args.push(node.into());
    args.push("-o".into());
    args.push(display.into());
    args.push("-F".into());
    args.push(format!("scale_vaapi=w={width}:h={height}:format=nv12").into());
    // A keyframe about every second, so a player that starts or recovers mid-stream shows the screen soon.
    for arg in ["-r", &fps.to_string(), "-p", "qp=30", "-p", "g=15", "-f", "pipe:1"] {
        args.push(arg.into());
    }
    args
}

/// VA-API render nodes, in order.
fn render_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|dir| dir.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD"))).collect())
        .unwrap_or_default();
    nodes.sort();
    nodes
}

impl Video {
    pub fn new(program: PathBuf, env: Arc<[(OsString, OsString)]>) -> Video {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Video(Arc::new(Inner {
            program,
            env,
            state: Mutex::new(State { generation: seed % GENERATIONS, ..State::default() }),
            probed: tokio::sync::OnceCell::new(),
            probing: AtomicBool::new(false),
        }))
    }

    /// A render node proven to encode H.264, after the background probe.
    pub fn encoder_node(&self) -> Option<PathBuf> {
        self.0
            .probed
            .get()
            .and_then(|result| result.as_ref().ok())
            .cloned()
    }

    /// The cached probe, if it has finished.
    pub fn capability(&self) -> Option<VideoCapability> {
        self.0.probed.get().map(|probed| match probed {
            Ok(_) => VideoCapability { capable: true, reason: None },
            Err(reason) => VideoCapability { capable: false, reason: Some(reason.clone()) },
        })
    }

    /// Said while the probe has not finished.
    pub fn checking() -> VideoCapability {
        VideoCapability { capable: false, reason: Some(CHECKING.into()) }
    }

    /// Claim the one background probe; `false` while another runs.
    pub fn begin_probe(&self) -> bool {
        !self.0.probing.swap(true, Ordering::SeqCst)
    }

    pub fn end_probe(&self) {
        self.0.probing.store(false, Ordering::SeqCst);
    }

    /// Probe once with `display` (unlocked), then keep the answer.
    pub async fn probe(&self, display: &str) -> Probed {
        self.0.probed.get_or_init(|| self.probe_now(display)).await.clone()
    }

    async fn probe_now(&self, display: &str) -> Probed {
        if !super::on_path(&self.0.program) {
            return Err(NOT_INSTALLED.into());
        }
        for node in render_nodes() {
            if self.encodes(&node, display).await {
                return Ok(node);
            }
        }
        Err(UNSUPPORTED.into())
    }

    /// Whether `node` gives an MPEG-TS stream of `display` within the deadline.
    async fn encodes(&self, node: &Path, display: &str) -> bool {
        let spawned = tokio::process::Command::new(&self.0.program)
            .args(encoder_args(node, display, 64, 36, 5, true))
            .envs(self.0.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else { return false };
        let Some(mut out) = child.stdout.take() else { return false };
        let mut got = Vec::new();
        let read = async {
            let mut buf = [0u8; 4096];
            while got.len() < 2 * TS_PACKET {
                match out.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
        };
        let _ = tokio::time::timeout(PROBE_DEADLINE, read).await;
        let _ = child.start_kill();
        let _ = child.wait().await;
        got.len() >= 2 * TS_PACKET && got[0] == 0x47 && got[TS_PACKET] == 0x47
    }

    /// Read what `watch` names from `cursor`, starting its encoder if needed.
    /// `VIDEO_UNSUPPORTED` without a working encoder.
    pub async fn observe(&self, watch: &Watch<'_>, cursor: Option<u64>) -> Result<Chunk> {
        let node = self.probe(watch.display).await.map_err(|reason| unsupported(&reason))?;
        self.read(&node, watch, cursor)
    }

    fn read(&self, node: &Path, watch: &Watch<'_>, cursor: Option<u64>) -> Result<Chunk> {
        let key = Key { viewer: watch.viewer.to_string(), display: watch.display.to_string(), width: watch.width, height: watch.height };
        let (width, height) = (watch.width, watch.height);
        let now = Instant::now();
        let mut state = lock(&self.0.state);
        let stale = |e: &Encoder| e.access != watch.access || lock(&e.ring).ended_at.is_some_and(|at| now.duration_since(at) >= ENDED_HOLD);
        // A new reader starts live, not with what an older encoder recorded before
        // (one that ended stays held until it may start again).
        let old = |e: &Encoder| cursor.is_none() && lock(&e.ring).ended_at.is_none() && now.duration_since(e.started) >= FRESH;
        if state.encoders.get(&key).is_none_or(|e| stale(e) || old(e)) {
            self.start(&mut state, &key, watch.access, node)?;
        }
        for _ in 0..2 {
            let encoder = state.encoders.get_mut(&key).expect("started");
            encoder.last_read = now;
            let ring = lock(&encoder.ring);
            let ended = ring.ended_at.is_some();
            if ended && cursor.is_none() {
                // Held after it ended: nothing new until it may start again.
                return Ok(Chunk { cursor: ring.end_cursor(), data: Vec::new(), reset: false, ended, width, height });
            }
            let served = match ring.read(cursor, MAX_CHUNK) {
                Some(served) => served,
                None if ended => ring.read_oldest(MAX_CHUNK),
                None => {
                    drop(ring);
                    self.start(&mut state, &key, watch.access, node)?;
                    continue;
                }
            };
            let ended = ended && served.cursor == ring.end_cursor();
            return Ok(Chunk { cursor: served.cursor, data: served.data, reset: served.reset, ended, width, height });
        }
        Err(IbaraError::new("INTERNAL_ERROR", "Video stream restarted without its first bytes.", true))
    }

    /// Start (or restart) the encoder for `key`.
    fn start(&self, state: &mut State, key: &Key, access: &str, node: &Path) -> Result<()> {
        if let Some(old) = state.encoders.remove(key) {
            old.stop();
        }
        state.generation = (state.generation + 1) % GENERATIONS;
        let mut child = tokio::process::Command::new(&self.0.program)
            .args(encoder_args(node, &key.display, key.width, key.height, 15, false))
            .envs(self.0.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| unsupported(UNSUPPORTED))?;
        let mut out = child.stdout.take().ok_or_else(|| unsupported(UNSUPPORTED))?;
        let ring = Arc::new(Mutex::new(Ring::new(state.generation, RING_BYTES)));
        let stop = super::Cancel::new();
        let (writing, stopping) = (ring.clone(), stop.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                tokio::select! {
                    read = out.read(&mut buf) => match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut ring = lock(&writing);
                            ring.push(&buf[..n]);
                            if ring.full() {
                                break;
                            }
                        }
                    },
                    _ = stopping.cancelled() => break,
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            lock(&writing).ended_at = Some(Instant::now());
        });
        let now = Instant::now();
        state.encoders.insert(key.clone(), Encoder { ring, stop, started: now, last_read: now, access: access.to_string() });
        if !state.sweeping {
            state.sweeping = true;
            tokio::spawn(self.clone().sweep());
        }
        Ok(())
    }

    /// Stop encoders nobody read for [`IDLE`]; ends with the last one.
    async fn sweep(self) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut state = lock(&self.0.state);
            state.encoders.retain(|_, e| {
                let keep = e.last_read.elapsed() < IDLE;
                if !keep {
                    e.stop();
                }
                keep
            });
            if state.encoders.is_empty() {
                state.sweeping = false;
                return;
            }
        }
    }

    /// Stop every encoder now (lock, Take Control). The next read restarts one.
    pub fn stop_all(&self) {
        for (_, encoder) in lock(&self.0.state).encoders.drain() {
            encoder.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(n: usize, fill: u8) -> Vec<u8> {
        (0..n).flat_map(|_| {
            let mut p = vec![fill; TS_PACKET];
            p[0] = 0x47;
            p
        }).collect()
    }

    #[test]
    fn sizes_fit_640_by_360_in_even_numbers_keeping_the_aspect() {
        assert_eq!(clamp_size(1920, 1080).unwrap(), (640, 360));
        assert_eq!(clamp_size(480, 270).unwrap(), (480, 270));
        assert_eq!(clamp_size(1280, 720).unwrap(), (640, 360));
        assert_eq!(clamp_size(641, 361).unwrap(), (638, 360), "fit inside, then even");
        assert_eq!(clamp_size(333, 187).unwrap(), (332, 186), "odd sizes round down to even");
        assert_eq!(clamp_size(1080, 1920).unwrap(), (202, 360), "portrait keeps its shape");
        assert_eq!(clamp_size(1, 1).unwrap(), (MIN_EDGE, MIN_EDGE));
        for (w, h) in [(0, 360), (640, -2), (1 << 20, 360)] {
            assert_eq!(clamp_size(w, h).unwrap_err().code, "INVALID_ARGUMENT", "{w}x{h}");
        }
    }

    #[test]
    fn a_cursor_continues_where_the_last_read_ended() {
        let mut ring = Ring::new(7, 10 * TS_PACKET);
        let first = ring.read(None, MAX_CHUNK).unwrap();
        assert_eq!((first.data.len(), first.reset), (0, false), "a new encoder has nothing yet");
        ring.push(&packets(3, 1));
        let a = ring.read(Some(first.cursor), 2 * TS_PACKET).unwrap();
        assert_eq!((a.data.len(), a.reset), (2 * TS_PACKET, false), "reads stop at the chunk size");
        let b = ring.read(Some(a.cursor), MAX_CHUNK).unwrap();
        assert_eq!((b.data.len(), b.reset), (TS_PACKET, false));
        let c = ring.read(Some(b.cursor), MAX_CHUNK).unwrap();
        assert_eq!((c.data.len(), c.cursor), (0, b.cursor), "caught up: no bytes, same cursor");
        let whole = [a.data, b.data].concat();
        assert_eq!(whole, packets(3, 1), "the reads join into what was written");
    }

    #[test]
    fn an_unknown_cursor_restarts_at_the_first_byte_or_asks_for_a_new_encoder() {
        let mut ring = Ring::new(7, 4 * TS_PACKET);
        ring.push(&packets(2, 1));
        // Another encoder's cursor, or one past what was written.
        let other = Ring::new(8, 4 * TS_PACKET).end_cursor();
        for stale in [other, ring.end_cursor() + 1] {
            let served = ring.read(Some(stale), MAX_CHUNK).unwrap();
            assert!(served.reset, "a cursor this encoder never gave means a new stream");
            assert_eq!(served.data, packets(2, 1), "the new stream starts at the first byte");
        }
        // Writing past the cap drops the oldest whole packets.
        ring.push(&packets(3, 2));
        assert_eq!(ring.bytes.len(), 4 * TS_PACKET);
        assert_eq!(ring.bytes.front(), Some(&0x47), "dropped in whole packets");
        assert_eq!(ring.read(None, MAX_CHUNK), None, "the first byte is gone: the encoder must restart");
        assert_eq!(ring.read(Some(ring.base), MAX_CHUNK), None, "a cursor that fell out of the ring too");
        let kept = ring.read(Some(ring.base + ring.start), MAX_CHUNK).unwrap();
        assert_eq!((kept.data.len(), kept.reset), (4 * TS_PACKET, false), "the oldest byte held is still served");
        let oldest = ring.read_oldest(MAX_CHUNK);
        assert!(oldest.reset && oldest.data[0] == 0x47);
    }

    #[test]
    fn reads_across_the_ring_seam_are_whole() {
        let mut ring = Ring::new(1, 3 * TS_PACKET);
        let mut written = Vec::new();
        let mut read = Vec::new();
        let mut cursor = ring.read(None, MAX_CHUNK).unwrap().cursor;
        for round in 0..20u8 {
            let bytes = packets(2, round);
            ring.push(&bytes);
            written.extend_from_slice(&bytes);
            let served = ring.read(Some(cursor), 100).unwrap();
            let rest = ring.read(Some(served.cursor), MAX_CHUNK).unwrap();
            assert!(!served.reset && !rest.reset, "round {round}");
            read.extend_from_slice(&served.data);
            read.extend_from_slice(&rest.data);
            cursor = rest.cursor;
        }
        assert_eq!(read, written);
    }

    #[test]
    fn cursors_of_different_generations_never_overlap_and_stay_exact_in_json() {
        let last = Ring::new(GENERATIONS - 1, RING_BYTES);
        assert!(last.base + STREAM_SPAN <= 1 << 53);
        assert_eq!(Ring::new(GENERATIONS, RING_BYTES).base, 0, "generations wrap");
        assert_ne!(Ring::new(3, RING_BYTES).base, Ring::new(4, RING_BYTES).base);
    }

    /// An encoder that writes `screen-of-<its pid>;` and then waits.
    fn fake_encoder(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("ibara-video-test-{name}-{}", std::process::id()));
        std::fs::write(&path, "#!/bin/sh\nprintf 'screen-of-%s;' \"$$\"\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Reads from `cursor` until the encoder's line has arrived: the bytes,
    /// whether the first read reset, and the next cursor.
    async fn read_line(video: &Video, watch: &Watch<'_>, mut cursor: Option<u64>) -> (Vec<u8>, bool, u64) {
        let mut got = Vec::new();
        let mut reset = None;
        for _ in 0..300 {
            let chunk = video.read(Path::new("/dev/null"), watch, cursor).unwrap();
            reset.get_or_insert(chunk.reset);
            got.extend_from_slice(&chunk.data);
            cursor = Some(chunk.cursor);
            if got.ends_with(b";") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (got, reset.unwrap_or(false), cursor.unwrap_or(0))
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Failure cases: a person approved after another's encoder recorded
    /// gets its earlier bytes, from the start or with that encoder's cursor;
    /// or a viewer whose access changed keeps reading what was recorded
    /// before.
    #[tokio::test]
    async fn nobody_reads_bytes_recorded_before_they_were_allowed() {
        let program = fake_encoder("access");
        let video = Video::new(program.clone(), Arc::from(Vec::new()));
        let watch = |viewer, access| Watch { viewer, access, display: "HDMI-A-1", width: 640, height: 360 };
        let alice = watch("alice", "revision 1, generation 3");
        let (earlier, _, alice_cursor) = read_line(&video, &alice, None).await;
        assert!(earlier.starts_with(b"screen-of-") && earlier.ends_with(b";"), "{}", String::from_utf8_lossy(&earlier));

        let bob = watch("bob", "revision 2, generation 3");
        let (seen, reset, _) = read_line(&video, &bob, None).await;
        assert!(!seen.is_empty() && !reset, "bob gets a stream of his own");
        assert!(!contains(&seen, &earlier), "bob, approved later, never sees alice's earlier bytes");
        let (crafted, reset, _) = read_line(&video, &bob, Some(alice_cursor - earlier.len() as u64)).await;
        assert!(reset && !contains(&crafted, &earlier), "alice's cursor means nothing to bob's encoder");

        let changed = watch("alice", "revision 3, generation 3");
        let (after, reset, _) = read_line(&video, &changed, Some(alice_cursor)).await;
        assert!(reset && !after.is_empty() && !contains(&after, &earlier), "a change of access restarts alice's encoder");
        video.stop_all();
        let _ = std::fs::remove_file(program);
    }

    /// Failure case: a reader without a cursor is handed what an older
    /// encoder recorded, which its player then plays that far behind forever.
    #[tokio::test]
    async fn a_reader_without_a_cursor_starts_live() {
        let program = fake_encoder("live");
        let video = Video::new(program.clone(), Arc::from(Vec::new()));
        let alice = Watch { viewer: "alice", access: "revision 1", display: "HDMI-A-1", width: 640, height: 360 };
        let (first, _, cursor) = read_line(&video, &alice, None).await;
        let (again, reset, _) = read_line(&video, &alice, None).await;
        assert!(!reset && again == first, "a young encoder serves a new reader from its first byte");
        tokio::time::sleep(FRESH).await;
        let (kept, reset, _) = read_line(&video, &alice, Some(cursor - first.len() as u64)).await;
        assert!(!reset && kept == first, "a reader with a cursor keeps its stream");
        let (live, reset, _) = read_line(&video, &alice, None).await;
        assert!(!reset && live.starts_with(b"screen-of-") && live != first, "an older encoder starts again for a new reader");
        video.stop_all();
        let _ = std::fs::remove_file(program);
    }
}
