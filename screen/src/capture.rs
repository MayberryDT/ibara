use crate::codec::DmaFrame;
use crate::codec::Encoder;
use anyhow::{Context, Result, ensure};
use libva::DrmPrimeSurfaceDescriptor;
use std::{
    os::fd::BorrowedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_buffer, wl_output, wl_registry},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
        ext_image_copy_capture_session_v1,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};
enum Request {
    Wlr(zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1),
    Ext(ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1),
}
impl Request {
    fn copy(&self, b: &wl_buffer::WlBuffer, damage: bool, w: u32, h: u32) {
        match self {
            Self::Wlr(f) => {
                if damage {
                    f.copy_with_damage(b)
                } else {
                    f.copy(b)
                }
            }
            Self::Ext(f) => {
                f.attach_buffer(b);
                f.damage_buffer(0, 0, w as i32, h as i32);
                f.capture();
            }
        }
    }
    fn destroy(&self) {
        match self {
            Self::Wlr(f) => f.destroy(),
            Self::Ext(f) => f.destroy(),
        }
    }
}
fn request(s: &Capture, q: &QueueHandle<Capture>) -> Result<Request> {
    if let Some(session) = &s.session {
        Ok(Request::Ext(session.create_frame(q, ())))
    } else {
        Ok(Request::Wlr(
            s.manager
                .as_ref()
                .context("no capture protocol")?
                .capture_output(0, s.output.as_ref().context("no output")?, q, ()),
        ))
    }
}

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sequence: u64,
    pub capture_us: u64,
    pub keyframe: bool,
    pub bytes: Arc<Vec<u8>>,
}
pub struct Health {
    pub last_capture: AtomicU64,
    pub idle: AtomicBool,
    pub force_idr: AtomicBool,
    pub stop: AtomicBool,
    pub bitrate: AtomicU64,
    pub viewers: AtomicU64,
}
impl Default for Health {
    fn default() -> Self {
        Self {
            last_capture: AtomicU64::new(0),
            idle: AtomicBool::new(false),
            force_idr: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            bitrate: AtomicU64::new(10_000_000),
            viewers: AtomicU64::new(0),
        }
    }
}
#[derive(Default)]
struct Capture {
    ext: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    sources:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    ext_size: Option<(u32, u32)>,
    ext_format: Option<u32>,
    render_node: Option<String>,
    y_inverted: bool,
    output: Option<wl_output::WlOutput>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    dma: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    offer: Option<(u32, u32, u32)>,
    done: bool,
    ready: bool,
    ready_us: u64,
    presentation_us: u64,
    failed: bool,
    retryable: bool,
    refresh: u32,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Capture {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "wl_output" if s.output.is_none() => {
                    s.output = Some(r.bind(name, version.min(3), q, ()))
                }
                "zwlr_screencopy_manager_v1" => {
                    s.manager = Some(r.bind(name, version.min(3), q, ()))
                }
                "ext_image_copy_capture_manager_v1" => s.ext = Some(r.bind(name, 1, q, ())),
                "ext_output_image_capture_source_manager_v1" => {
                    s.sources = Some(r.bind(name, 1, q, ()))
                }
                "zwp_linux_dmabuf_v1" => s.dma = Some(r.bind(name, version.min(3), q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Capture {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { refresh, .. } = e {
            s.refresh = (refresh.max(1000) / 1000) as u32;
        }
    }
}
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for Capture {
    fn event(
        s: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        e: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => s.offer = Some((format, width, height)),
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: wayland_client::WEnum::Value(flags),
            } => s.y_inverted = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            zwlr_screencopy_frame_v1::Event::BufferDone => s.done = true,
            zwlr_screencopy_frame_v1::Event::Ready { tv_sec_hi, tv_sec_lo, tv_nsec } => {
                s.ready_us = crate::now_us();
                ibara_screen::trace("compositor_ready", s.ready_us);
                ibara_screen::trace_at("compositor_frame", s.ready_us,
                    ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000 + tv_nsec as u64 / 1000);
                s.ready = true;
            },
            zwlr_screencopy_frame_v1::Event::Failed => {
                s.failed = true;
                s.retryable = true;
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Capture: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Capture: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(Capture: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
wayland_client::delegate_noop!(Capture: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
wayland_client::delegate_noop!(Capture: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(Capture: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(Capture: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for Capture {
    fn event(
        s: &mut Self,
        _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        e: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        if s.done && !matches!(&e, Event::Done | Event::Stopped) {
            s.done = false;
            s.ext_size = None;
            s.ext_format = None;
            s.offer = None;
        }
        match e {
            Event::DmabufDevice { device } => {
                if device.len() == std::mem::size_of::<libc::dev_t>() {
                    let dev = libc::dev_t::from_ne_bytes(device.try_into().unwrap());
                    s.render_node = std::fs::read_dir("/dev/dri").ok().and_then(|nodes| {
                        use std::os::unix::fs::MetadataExt;
                        nodes.flatten().find_map(|n| (n.file_name().to_string_lossy().starts_with("renderD") && n.metadata().ok()?.rdev() == dev).then(|| n.path().to_string_lossy().into_owned()))
                    });
                }
            }
            Event::BufferSize { width, height } => s.ext_size = Some((width, height)),
            Event::DmabufFormat { format, .. } => {
                if format == u32::from_le_bytes(*b"AR24")
                    || (format == u32::from_le_bytes(*b"XR24") && s.ext_format.is_none())
                {
                    s.ext_format = Some(format);
                }
            }
            Event::Done => {
                if let Some((w, h)) = s.ext_size {
                    if let Some(format) = s.ext_format {
                        s.offer = Some((format, w, h));
                    }
                }
                s.done = true;
            }
            Event::Stopped => {
                s.failed = true;
                s.retryable = false;
            }
            _ => {}
        }
    }
}
impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for Capture {
    fn event(
        s: &mut Self,
        _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        e: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                if transform != wayland_client::WEnum::Value(wl_output::Transform::Normal) {
                    s.failed = true;
                }
            }
            ext_image_copy_capture_frame_v1::Event::PresentationTime { tv_sec_hi, tv_sec_lo, tv_nsec } => {
                s.presentation_us = ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000 + tv_nsec as u64 / 1000;
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                s.ready_us = crate::now_us();
                ibara_screen::trace("compositor_ready", s.ready_us);
                ibara_screen::trace_at("compositor_frame", s.ready_us, s.presentation_us);
                s.ready = true;
            },
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                s.failed = true;
                s.retryable = reason
                    != wayland_client::WEnum::Value(
                        ext_image_copy_capture_frame_v1::FailureReason::Stopped,
                    );
            }
            _ => {}
        }
    }
}
fn wlbuffer(
    d: &DrmPrimeSurfaceDescriptor,
    w: u32,
    h: u32,
    s: &Capture,
    q: &QueueHandle<Capture>,
) -> Result<wl_buffer::WlBuffer> {
    use std::os::fd::AsFd;
    ensure!(d.layers.len() == 1, "capture needs one DRM layer");
    let p = s
        .dma
        .as_ref()
        .context("linux-dmabuf unavailable")?
        .create_params(q, ());
    let l = &d.layers[0];
    for i in 0..l.num_planes as usize {
        let o = &d.objects[l.object_index[i] as usize];
        p.add(
            o.fd.as_fd(),
            i as u32,
            l.offset[i],
            l.pitch[i],
            (o.drm_format_modifier >> 32) as u32,
            o.drm_format_modifier as u32,
        );
    }
    let b = p.create_immed(
        w as i32,
        h as i32,
        l.drm_format,
        zwp_linux_buffer_params_v1::Flags::empty(),
        q,
        (),
    );
    p.destroy();
    Ok(b)
}
pub fn run(
    tx: tokio::sync::watch::Sender<Option<Frame>>,
    health: Arc<Health>,
    ready: std::sync::mpsc::Sender<Result<(u32, u32, u32, String, String)>>,
    start: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<Capture>();
    let q = queue.handle();
    conn.display().get_registry(&q, ());
    let mut s = Capture::default();
    queue.roundtrip(&mut s)?;
    queue.roundtrip(&mut s)?;
    let output = s.output.clone().context("no output")?;
    if let (Some(ext), Some(sources)) = (&s.ext, &s.sources) {
        let source = sources.create_source(&output, &q, ());
        s.session = Some(ext.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            &q,
            (),
        ));
        source.destroy();
        while !s.done && !s.failed {
            queue.blocking_dispatch(&mut s)?;
        }
        if s.offer.is_none() {
            s.session.take().unwrap().destroy();
            s.done = false;
            s.failed = false;
        }
    }
    let mut capture = request(&s, &q)?;
    if s.session.is_none() {
        while !s.done && !s.failed {
            queue.blocking_dispatch(&mut s)?;
        }
    }
    let (_, mut w, mut h) = s.offer.context("no captured dmabuf")?;

    let node = s.render_node.clone().unwrap_or_else(|| {
        std::fs::read_dir("/sys/class/drm").ok().and_then(|nodes| nodes.flatten().find_map(|n| {
            let name = n.file_name().to_string_lossy().into_owned();
            (name.starts_with("renderD") && std::fs::read_to_string(n.path().join("device/boot_vga")).unwrap_or_default().trim() == "1").then(|| format!("/dev/dri/{name}"))
        })).unwrap_or_else(|| "/dev/dri/renderD128".into())
    });
    eprintln!("capture render node: {node}");
    let fps = s.refresh.clamp(1, 30);
    let mut encoder = Encoder::new(w, h, fps, &node)?;
    let mut fps = encoder.fps;
    ready
        .send(Ok((
            encoder.width,
            encoder.height,
            fps,
            if s.session.is_some() {
                "ext-image-copy-capture-dmabuf"
            } else {
                "wlr-screencopy-dmabuf"
            }
            .to_string(),
            encoder.name.to_string(),
        )))
        .ok();
    start.recv().context("sender stopped during startup")?;
    let mut seq = 0u64;
    let mut first = true;
    let mut cached: Option<DmaFrame> = None;
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut pool: Vec<(DmaFrame, wl_buffer::WlBuffer)> = Vec::new();
    let mut pool_index = 0usize;
    let mut retries = 0;
    let mut armed = false;
    loop {
        if health.stop.load(Ordering::Relaxed) {
            break;
        }
        if pool.is_empty() {
            for _ in 0..2 {
                let mut src = encoder.hw.allocate()?;
                let format = s.offer.unwrap().0;
                if format == u32::from_le_bytes(*b"XR24") {
                    src.descriptor.fourcc = libva::VA_FOURCC_BGRX;
                    src.descriptor.layers[0].drm_format = format;
                }
                ensure!(
                    src.descriptor.layers[0].drm_format == format,
                    "VA surface and compositor formats differ"
                );
                let buffer = wlbuffer(&src.descriptor, w, h, &s, &q)?;
                pool.push((src, buffer));
            }
        }
        let (src, buffer) = &pool[pool_index];
        if !armed {
            s.ready = false;
            s.failed = false;
            s.retryable = false;
            health.idle.store(!first, Ordering::Relaxed);
            capture.copy(
                &buffer,
                !first && !health.force_idr.load(Ordering::Relaxed),
                w,
                h,
            );
        }
        armed = false;
        health.idle.store(!first, Ordering::Relaxed);
        while !s.ready && !s.failed {
            queue.dispatch_pending(&mut s)?;
            if s.ready || s.failed { break; }
            conn.flush()?;
            if health.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if health.force_idr.load(Ordering::Relaxed)
                && health.viewers.load(Ordering::Relaxed) > 0
            {
                if let Some(saved) = cached.as_ref() {
                    health.force_idr.store(false, Ordering::Relaxed);
                    let now = crate::now_us();
                    encoder.bitrate(health.bitrate.load(Ordering::Relaxed))?;
                    for (keyframe, bytes) in encoder.encode(saved, now, true)? {
                        seq = seq.checked_add(1).context("frame sequence overflow")?;
                        tx.send_replace(Some(Frame {
                            width: encoder.width,
                            height: encoder.height,
                            fps,
                            sequence: seq,
                            capture_us: health.last_capture.load(Ordering::Relaxed),
                            keyframe,
                            bytes: Arc::new(bytes),
                        }));
                    }
                }
            }
            if let Some(guard) = queue.prepare_read() {
                use std::os::fd::AsRawFd;
                let mut fd = libc::pollfd {
                    fd: conn.backend().poll_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe {
                    libc::poll(&mut fd, 1, 30);
                }
                if fd.revents & libc::POLLIN != 0 {
                    guard.read()?;
                } else {
                    drop(guard);
                }
            }
        }
        if s.failed {
            ensure!(s.retryable && retries < 3, "screencopy failed");
            retries += 1;
            capture.destroy();
            if s.session.is_none() {
                s.done = false;
                s.offer = None;
                capture = request(&s, &q)?;
            }
            while !s.done {
                queue.blocking_dispatch(&mut s)?;
            }
            let (_, nw, nh) = s.offer.context("missing refreshed capture constraints")?;
            if (w, h) != (nw, nh) {
                w = nw;
                h = nh;
                encoder = Encoder::new(w, h, s.refresh.clamp(1, 30), s.render_node.as_deref().unwrap_or(&node))?;
                fps = encoder.fps;
            }
            cached = None;
            for (_, buffer) in pool.drain(..) {
                buffer.destroy();
            }
            pool_index = 0;
            armed = false;
            first = true;
            if s.session.is_some() {
                capture = request(&s, &q)?;
            }
            health.force_idr.store(true, Ordering::Relaxed);
            continue;
        }
        ensure!(s.ready, "screencopy failed");
        retries = 0;
        health.idle.store(false, Ordering::Relaxed);
        let now = s.ready_us;
        health.last_capture.store(now, Ordering::Relaxed);
        ibara_screen::trace("capture_done", now);
        capture.destroy();
        // Arm the other buffer before the GPU reads this one. A 30 Hz
        // compositor otherwise spends a whole extra refresh waiting for us.
        let inverted = s.y_inverted;
        if s.session.is_none() {
            s.done = false;
            s.offer = None;
        }
        capture = request(&s, &q)?;
        if s.session.is_none() {
            while !s.done && !s.failed {
                queue.blocking_dispatch(&mut s)?;
            }
        }
        let next_size = s.offer.map(|(_, w, h)| (w, h));
        if next_size == Some((w, h)) && !s.failed {
            s.ready = false;
            s.retryable = false;
            capture.copy(&pool[pool_index ^ 1].1, true, w, h);
            conn.flush()?;
            armed = true;
        }
        if health.viewers.load(Ordering::Relaxed) > 0 || first {
            let interval = Duration::from_micros(1_000_000 / fps as u64);
            if let Some(wait) = interval.checked_sub(last.elapsed()) {
                std::thread::sleep(wait);
            }
            last += interval;
            if last.elapsed() > interval { last = Instant::now(); }
            ibara_screen::trace("encode_start", now);
            let idr = first || health.force_idr.swap(false, Ordering::Relaxed);
            let mut captured = src.try_clone()?;
            captured.y_inverted = inverted;
            cached = Some(captured);
            encoder.bitrate(health.bitrate.load(Ordering::Relaxed))?;
            let units = encoder.encode(cached.as_ref().unwrap(), now, idr)?;
            for (keyframe, bytes) in units {
                seq = seq.checked_add(1).context("frame sequence overflow")?;
                tx.send_replace(Some(Frame {
                    width: encoder.width,
                    height: encoder.height,
                    fps,
                    sequence: seq,
                    capture_us: now,
                    keyframe,
                    bytes: Arc::new(bytes),
                }));
            }
        }
        first = false;
        pool_index ^= 1;
        let (_, nw, nh) = s.offer.context("missing dmabuf offer")?;
        if nw != w || nh != h {
            w = nw;
            h = nh;
            encoder = Encoder::new(w, h, s.refresh.clamp(1, 30), s.render_node.as_deref().unwrap_or(&node))?;
            fps = encoder.fps;
            cached = None;
            for (_, buffer) in pool.drain(..) {
                buffer.destroy();
            }
            pool_index = 0;
            armed = false;
            first = true;
            health.force_idr.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}
