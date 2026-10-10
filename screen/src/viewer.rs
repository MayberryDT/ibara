use crate::{codec, identity, render::Renderer};
use anyhow::{Context, Result, ensure};
use ibara_screen::wire::{self, VideoGate, VideoHeader};
use serde::Deserialize;
use serde_json::{Value, json};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
};
use std::{
    io::Read,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as inhibit_manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
#[derive(Clone, Deserialize)]
pub struct Bundle {
    pub host: String,
    pub server_cert_sha256: String,
    pub ticket: String,
    #[serde(default = "default_port", alias = "quic_port")]
    pub port: u16,
    #[serde(default, deserialize_with = "optional_string")]
    pub computer_id: String,
    #[serde(default, deserialize_with = "optional_string")]
    pub computer_name: String,
    #[serde(default, deserialize_with = "optional_string")]
    pub console_socket: String,
    #[serde(default, deserialize_with = "optional_string")]
    pub controller_epoch: String,
    #[serde(default, deserialize_with = "optional_string")]
    pub expected_owner: String,
    #[serde(default, deserialize_with = "optional_string")]
    #[serde(alias = "expected_ownership_revision")]
    pub expected_revision: String,
}
fn optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}
fn default_port() -> u16 {
    47910
}
#[derive(Clone)]
struct Encoded {
    header: Arc<VideoHeader>,
    bytes: Arc<Vec<u8>>,
}
struct Shared {
    pong: AtomicU64,
    width: AtomicU64,
    height: AtomicU64,
    stopped: AtomicBool,
    reset: AtomicBool,
    input: AtomicBool,
    holder: std::sync::Mutex<Option<String>>,
    attached: std::sync::Mutex<Option<Instant>>,
    handing_back: AtomicBool,
    frame_wake: std::os::fd::OwnedFd,
}
pub async fn run(dir: &Path) -> Result<()> {
    let id = identity::Identity::load(dir)?;
    eprintln!("ibara standby_ready");
    let data = tokio::task::spawn_blocking(|| {
        let mut b = vec![];
        std::io::stdin().take(65537).read_to_end(&mut b)?;
        Ok::<_, std::io::Error>(b)
    })
    .await??;
    ensure!(data.len() <= 65536, "bundle too large");
    let bundle: Bundle = serde_json::from_slice(&data)?;
    ensure!(bundle.ticket.len() == 64, "invalid ticket");
    let client = identity::client(&id, &bundle.server_cert_sha256)?;
    use std::os::fd::FromRawFd;
    let wake = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    ensure!(wake >= 0, "frame wakeup unavailable");
    let shared = Arc::new(Shared {
        pong: AtomicU64::new(crate::now_us()),
        width: AtomicU64::new(1920),
        height: AtomicU64::new(1080),
        stopped: AtomicBool::new(false),
        reset: AtomicBool::new(false),
        input: AtomicBool::new(false),
        holder: std::sync::Mutex::new(None),
        attached: std::sync::Mutex::new(Some(Instant::now())),
        handing_back: AtomicBool::new(false),
        frame_wake: unsafe { std::os::fd::OwnedFd::from_raw_fd(wake) },
    });
    let signal_state = shared.clone();
    let _signals = crate::AbortTask(tokio::spawn(async move {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=terminate.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
            signal_state.stopped.store(true, Ordering::Relaxed);
        }
    }));
    let (video_tx, video_rx) = mpsc::channel(2);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let h = shared.clone();
    let b = bundle.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = network(b, client, video_tx, events_rx, h.clone()).await {
            eprintln!("ibara-screen: {e:#}");
        }
        h.stopped.store(true, Ordering::Relaxed);
    });
    let h = shared.clone();
    tokio::task::spawn_blocking(move || window(bundle, video_rx, events_tx, h)).await??;
    shared.stopped.store(true, Ordering::Relaxed);
    handle.abort();
    Ok(())
}
async fn network(
    bundle: Bundle,
    config: quinn::ClientConfig,
    video: mpsc::Sender<Encoded>,
    mut events: mpsc::UnboundedReceiver<Value>,
    shared: Arc<Shared>,
) -> Result<()> {
    let addr = tokio::net::lookup_host((bundle.host.as_str(), bundle.port))
        .await?
        .next()
        .context("sender address unavailable")?;
    let bind = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let mut endpoint = quinn::Endpoint::client(bind.parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(config);
    let deadline = Instant::now() + Duration::from_secs(60);
    shared.attached.lock().unwrap().get_or_insert_with(Instant::now);
    while !shared.stopped.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            let replacement = bundle.clone();
            tokio::task::spawn_blocking(move || renew_viewer(&replacement)).await??;
            return Ok(());
        }
        let conn = match endpoint.connect(addr, "ibara-screen")?.await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ibara-screen reconnect: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let result=async{
 let (mut send,mut recv)=conn.open_bi().await?;wire::write_json(&mut send,&json!({"t":"ticket","ticket":bundle.ticket})).await?;let hello=wire::read_json(&mut recv).await?;ensure!(hello.get("t").and_then(Value::as_str)==Some("hello")&&hello.get("v").and_then(Value::as_u64)==Some(1),"invalid server hello");shared.width.store(hello.get("width").and_then(Value::as_u64).context("no frame width")?,Ordering::Relaxed);shared.height.store(hello.get("height").and_then(Value::as_u64).context("no frame height")?,Ordering::Relaxed);shared.pong.store(crate::now_us(),Ordering::Relaxed);shared.input.store(hello["input"].as_bool().unwrap_or(false),Ordering::Relaxed);shared.attached.lock().unwrap().get_or_insert_with(Instant::now);
 let(mut control,_reader)=crate::control_reader(recv);
 let mut next = 0u64;
 let mut input=conn.open_uni().await?;input.set_priority(20)?;let mut ping=tokio::time::interval(Duration::from_millis(500));let mut batch_timer=tokio::time::interval(Duration::from_millis(4));let mut pending=vec![json!({"release_all":true})];let mut latest_ack=None;
 let v=video.clone();let c=conn.clone();let health=shared.clone();let receiver=crate::AbortTask(tokio::spawn(async move{loop{let mut stream=c.accept_uni().await?;let result=async{let mut header=[0;24];stream.read_exact(&mut header).await?;let header=VideoHeader::decode(&header)?;let size=stream.read_u32().await? as usize;ensure!(size>0&&size<=wire::MAX_VIDEO,"video too large");let mut bytes=vec![0;size];stream.read_exact(&mut bytes).await?;ibara_screen::trace("receive_done",header.capture_us);if v.try_send(Encoded{header:Arc::new(header),bytes:Arc::new(bytes)}).is_err(){health.reset.store(true,Ordering::Relaxed);}let one=1u64;unsafe{libc::write(std::os::fd::AsRawFd::as_raw_fd(&health.frame_wake),(&one as *const u64).cast(),8);}Ok::<(),anyhow::Error>(())};if !matches!(tokio::time::timeout(Duration::from_millis(200),result).await,Ok(Ok(()))){health.reset.store(true,Ordering::Relaxed);}}#[allow(unreachable_code)]Ok::<(),anyhow::Error>(())}));
 loop {tokio::select!{
 _=conn.closed()=>break,
 _=ping.tick()=>wire::write_json(&mut send,&json!({"t":"ping"})).await?,
 _=batch_timer.tick(),if !pending.is_empty()=>{let n=pending.len();let batch=json!({"t":"input","from":next,"events":pending});next=next.checked_add(n as u64).context("input sequence overflow")?;pending=vec![];wire::write_json(&mut input,&batch).await?;},
 Some(event)=events.recv()=>{if event.get("t").and_then(Value::as_str)==Some("keyframe"){wire::write_json(&mut send,&event).await?;}else if pending.len()<256{pending.push(event);}else{pending.clear();pending.push(json!({"release_all":true}));}},
 m=control.recv()=>{let m=m.context("control closed")??;match m.get("t").and_then(Value::as_str){Some("pong")=>shared.pong.store(crate::now_us(),Ordering::Relaxed),Some("input_ack")=>{let upto=m.get("upto").and_then(Value::as_u64).context("invalid input ack")?;ensure!(upto<next&&latest_ack.is_none_or(|a|upto>=a),"invalid input ack sequence");latest_ack=Some(upto);},Some("input_gap")=>{let expected=m.get("expected").and_then(Value::as_u64).context("invalid input gap")?;ensure!(expected<=next,"invalid input gap");next=expected;pending.clear();pending.push(json!({"release_all":true}));},Some("geometry")=>{shared.width.store(m.get("width").and_then(Value::as_u64).context("invalid width")?,Ordering::Relaxed);shared.height.store(m.get("height").and_then(Value::as_u64).context("invalid height")?,Ordering::Relaxed);},Some("turn")=>{*shared.holder.lock().unwrap()=if m["yours"]==true{None}else{m["holder"].as_str().map(str::to_owned)};},Some("closing")=>{shared.stopped.store(true,Ordering::Relaxed);break;},_=>anyhow::bail!("unexpected sender message")}}
 }}drop(receiver);Ok::<(),anyhow::Error>(())}.await;
        if matches!(conn.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(ref c)) if c.error_code == 0u32.into()) {
            shared.stopped.store(true, Ordering::Relaxed);
        }
        if let Err(e) = result {
            eprintln!("ibara-screen reconnect: {e}");
        }
        conn.close(0u32.into(), b"reattach");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}
struct View {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    window: Window,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    manager: Option<inhibit_manager::ZwpKeyboardShortcutsInhibitManagerV1>,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
    seat: Option<wl_seat::WlSeat>,
    modifiers: Modifiers,
    grab: bool,
    inhibit_shortcuts: bool,
    focus: bool,
    events: mpsc::UnboundedSender<Value>,
    shared: Arc<Shared>,
    bundle: Bundle,
}
impl View {
    fn input(&self, event: Value) {
        if event.get("t").is_some() || self.shared.input.load(Ordering::Relaxed) {
            let _ = self.events.send(event);
        }
    }
    fn grab(&mut self, q: &QueueHandle<Self>) {
        if self.grab && self.focus && self.inhibit_shortcuts && self.inhibitor.is_none() {
            if let (Some(m), Some(seat)) = (&self.manager, &self.seat) {
                self.inhibitor = Some(m.inhibit_shortcuts(self.window.wl_surface(), seat, q, ()));
            }
        } else if !self.grab || !self.focus {
            if let Some(i) = self.inhibitor.take() {
                i.destroy();
            }
        }
    }
    fn key(&mut self, event: KeyEvent, down: bool, q: &QueueHandle<Self>) {
        if !self.focus {
            return;
        }
        if down && event.keysym == Keysym::Escape && self.modifiers.logo && self.modifiers.alt {
            self.input(json!({"release_all":true}));
            self.grab = !self.grab;
            if self.grab { self.inhibit_shortcuts = true; }
            self.grab(q);
            return;
        }
        if down
            && (event.keysym == Keysym::h || event.keysym == Keysym::H)
            && self.modifiers.ctrl
            && self.modifiers.alt
            && self.modifiers.shift
        {
            self.input(json!({"release_all":true}));
            if self.shared.handing_back.swap(true, Ordering::Relaxed) {
                return;
            }
            let b = self.bundle.clone();
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                match handback(&b) {
                    Ok(()) => shared.stopped.store(true, Ordering::Relaxed),
                    Err(e) => eprintln!("Could not Hand Back: {e:#}"),
                }
                shared.handing_back.store(false, Ordering::Relaxed);
            });
            return;
        }
        if self.grab {
            self.input(json!({"key":[event.raw_code,down]}));
        }
    }
}
fn fallback(b: &Bundle) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;
    ensure!(
        !b.console_socket.is_empty(),
        "No first frame within 3 seconds of attach"
    );
    let mut socket = UnixStream::connect(&b.console_socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(45)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = json!({"id":format!("screen-failed-{}",std::process::id()),"command":"operator-control","args":["--computer",b.computer_id,"--epoch",b.controller_epoch,"--op","screen_failed","--owner",b.expected_owner,"--revision",b.expected_revision]});
    writeln!(socket, "{request}")?;
    let mut line = String::new();
    std::io::BufReader::new(socket)
        .take(1024 * 1024)
        .read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    ensure!(
        response
            .pointer("/envelope/error")
            .is_none_or(Value::is_null),
        "The console refused screen fallback"
    );
    Ok(())
}
fn renew_viewer(b: &Bundle) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;
    ensure!(
        !b.console_socket.is_empty(),
        "Ticket expired. Open Join again."
    );
    let mut socket = UnixStream::connect(&b.console_socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(45)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let req = json!({"id":format!("screen-renew-{}",std::process::id()),"command":"open-viewer","args":["--computer",b.computer_id,"--epoch",b.controller_epoch]});
    writeln!(socket, "{req}")?;
    let mut line = String::new();
    std::io::BufReader::new(socket)
        .take(1024 * 1024)
        .read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    ensure!(
        response
            .pointer("/envelope/error")
            .is_none_or(Value::is_null)
            && response.pointer("/envelope/data/viewer_started") == Some(&Value::Bool(true)),
        "The console did not renew the viewer"
    );
    Ok(())
}
fn handback(b: &Bundle) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;
    ensure!(
        !b.console_socket.is_empty()
            && !b.expected_owner.is_empty()
            && !b.expected_revision.is_empty(),
        "Open Join again from the console."
    );
    let mut socket = UnixStream::connect(&b.console_socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(45)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let req = json!({"id":format!("screen-handback-{}",std::process::id()),"command":"operator-control","args":["--computer",b.computer_id,"--epoch",b.controller_epoch,"--op","viewer_handback","--owner",b.expected_owner,"--revision",b.expected_revision]});
    writeln!(socket, "{req}")?;
    let mut line = String::new();
    std::io::BufReader::new(socket)
        .take(1024 * 1024)
        .read_line(&mut line)?;
    let v: Value = serde_json::from_str(&line)?;
    ensure!(
        v.pointer("/envelope/error").is_none_or(Value::is_null),
        "Hand Back was refused"
    );
    let result = v
        .pointer("/envelope/data/result")
        .context("Hand Back returned no result")?;
    let owner = result["owner"].as_str().unwrap_or("");
    ensure!(
        !owner.is_empty()
            && !owner.starts_with("operator:")
            && result["ownership_revision"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
        "The computer did not confirm Hand Back"
    );
    Ok(())
}
fn window(
    bundle: Bundle,
    mut frames: mpsc::Receiver<Encoded>,
    events: mpsc::UnboundedSender<Value>,
    shared: Arc<Shared>,
) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<View>(&conn)?;
    let q = queue.handle();
    let compositor = CompositorState::bind(&globals, &q)?;
    let shell = XdgShell::bind(&globals, &q)?;
    let window = shell.create_window(
        compositor.create_surface(&q),
        WindowDecorations::RequestServer,
        &q,
    );
    window.set_title(format!("{} · Ibara", bundle.computer_name));
    window.set_app_id("io.zet.ibara.Screen");
    window.set_min_size(Some((320, 180)));
    window.commit();
    let manager = globals.bind(&q, 1..=1, ()).ok();
    let presentation: Option<wp_presentation::WpPresentation> = globals.bind(&q, 1..=1, ()).ok();
    let mut app = View {
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &q),
        outputs: OutputState::new(&globals, &q),
        window,
        width: 1280,
        height: 720,
        configured: false,
        exit: false,
        keyboard: None,
        pointer: None,
        manager,
        inhibitor: None,
        seat: None,
        modifiers: Modifiers::default(),
        grab: true,
        // GNOME asks for secure consent when inhibition is requested. Watch
        // starts with ordinary focused input; explicit capture toggling opts in.
        inhibit_shortcuts: !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
            .split(':').any(|desktop| desktop.eq_ignore_ascii_case("GNOME")),
        focus: false,
        events,
        shared: shared.clone(),
        bundle,
    };
    let mut renderer = None;
    let mut hardware = codec::Decoder::new().ok();
    let mut software = openh264::decoder::Decoder::new()?;
    let mut gate = VideoGate::default();
    let mut shown = false;
    let toggle = crate::keyboard_toggle::Toggle::new()?;
    while !app.exit && !shared.stopped.load(Ordering::Relaxed) {
        if !shown
            && shared
                .attached
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
        {
            fallback(&app.bundle)?;
            break;
        }
        if toggle.requested() {
            app.input(json!({"release_all":true}));
            app.grab = !app.grab;
            app.grab(&q);
        }
        if shared.reset.swap(false, Ordering::Relaxed) {
            app.input(json!({"t":"keyframe"}));
        }
        queue.dispatch_pending(&mut app)?;
        conn.flush()?;
        if app.configured && renderer.is_none() {
            let r = Renderer::new(&conn, app.window.wl_surface(), app.width, app.height)?;
            r.clear()?;
            renderer = Some(r);
        }
        if let Some(r) = renderer.as_mut() {
            r.resize(app.width, app.height);
            while let Ok(frame) = frames.try_recv() {
                    if !gate.accept(frame.header.sequence, frame.header.keyframe) {
                        app.input(json!({"t":"keyframe"}));
                    } else {
                        ibara_screen::trace("decode_submit", frame.header.capture_us);
                        let result = if let Some(d) = hardware.as_mut() {
                            d.decode(&frame.bytes).and_then(|decoded| {
                                if let Some(f) = decoded {
                                    if ibara_screen::trace_enabled() {
                                        if let Some(p) = &presentation {
                                            p.feedback(app.window.wl_surface(), &q, frame.header.capture_us);
                                        } else {
                                            app.window.wl_surface().frame(&q, frame.header.capture_us);
                                        }
                                    }
                                    ibara_screen::trace("decode_done", frame.header.capture_us);
                                    r.hardware(&f)?;
                                    Ok(true)
                                } else {
                                    Ok(false)
                                }
                            })
                        } else {
                            match software.decode(&frame.bytes) {
                                Ok(Some(f)) => {
                                    use openh264::formats::YUVSource;
                                    let (w, h) = f.dimensions();
                                    let mut rgba = vec![0; w * h * 4];
                                    f.write_rgba8(&mut rgba);
                                    r.software(&rgba, w as u32, h as u32).map(|_| true)
                                }
                                Ok(None) => Ok(false),
                                Err(e) => Err(e.into()),
                            }
                        };
                        match result {
                            Ok(true) => {
                                ibara_screen::trace("swap_done", frame.header.capture_us);
                                if !shown {
                                    eprintln!(
                                        "ibara first_frame capture_us={} decoded_us={}",
                                        frame.header.capture_us,
                                        crate::now_us()
                                    );
                                    shown = true;
                                }
                                eprintln!(
                                    "screen_frame sequence={} capture_us={} decoded_us={}",
                                    frame.header.sequence,
                                    frame.header.capture_us,
                                    crate::now_us()
                                );
                            }
                            Ok(false) => {}
                            Err(e) => {
                                eprintln!("decode error: {e:#}");
                                hardware = None;
                                app.input(json!({"t":"keyframe"}));
                            }
                        }
                    }
            }
        }
        let reconnect =
            crate::now_us().saturating_sub(shared.pong.load(Ordering::Relaxed)) > 2_000_000;
        app.window.set_title(if reconnect {
            "Reconnecting…".to_string()
        } else {
            if let Some(holder) = shared.holder.lock().unwrap().as_deref() {
                format!("{} · {holder} Has The Turn", app.bundle.computer_name)
            } else if !app.grab {
                format!("{} · Keys Here", app.bundle.computer_name)
            } else {
                format!("{} · Ibara", app.bundle.computer_name)
            }
        });
        if let Some(guard) = queue.prepare_read() {
            let mut fds = [
                libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&conn.backend().poll_fd()), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&shared.frame_wake), events: libc::POLLIN, revents: 0 },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 2, 4); }
            if fds[1].revents & libc::POLLIN != 0 {
                let mut count = 0u64;
                unsafe { libc::read(fds[1].fd, (&mut count as *mut u64).cast(), 8); }
            }
            if fds[0].revents & libc::POLLIN != 0 {
                guard.read()?;
            } else {
                drop(guard);
            }
        }
    }
    app.input(json!({"release_all":true}));
    Ok(())
}
impl CompositorHandler for View {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for View {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl WindowHandler for View {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        c: WindowConfigure,
        _: u32,
    ) {
        self.width = c.new_size.0.map(|n| n.get()).unwrap_or(self.width);
        self.height = c.new_size.1.map(|n| n.get()).unwrap_or(self.height);
        self.configured = true;
    }
}
impl SeatHandler for View {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        q: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        c: Capability,
    ) {
        self.seat = Some(seat.clone());
        if c == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(q, &seat, None).ok();
        }
        if c == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(q, &seat).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        c: Capability,
    ) {
        self.input(json!({"release_all":true}));
        if c == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if c == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        self.input(json!({"release_all":true}));
    }
}
impl KeyboardHandler for View {
    fn enter(
        &mut self,
        _: &Connection,
        q: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        s: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.focus = s == self.window.wl_surface();
        self.grab(q);
    }
    fn leave(
        &mut self,
        _: &Connection,
        q: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.focus = false;
        self.grab(q);
        self.input(json!({"release_all":true}));
    }
    fn press_key(
        &mut self,
        _: &Connection,
        q: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        e: KeyEvent,
    ) {
        self.key(e, true, q);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        q: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        e: KeyEvent,
    ) {
        self.key(e, false, q);
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        m: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = m;
    }
}
impl PointerHandler for View {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            if &e.surface != self.window.wl_surface() {
                continue;
            }
            let (x, y) = e.position;
            let w = self.shared.width.load(Ordering::Relaxed);
            let h = self.shared.height.load(Ordering::Relaxed);
            self.input(
                json!({"move":[x*w as f64/self.width as f64,y*h as f64/self.height as f64]}),
            );
            match e.kind {
                PointerEventKind::Press { button, .. } => {
                    self.input(json!({"button":[button,true]}))
                }
                PointerEventKind::Release { button, .. } => {
                    self.input(json!({"button":[button,false]}))
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let amount = |v: smithay_client_toolkit::seat::pointer::AxisScroll| {
                        if v.value120 != 0 {
                            v.value120
                        } else if v.discrete != 0 {
                            v.discrete * 120
                        } else {
                            (v.absolute * 12.) as i32
                        }
                    };
                    self.input(json!({"wheel":[-amount(horizontal),-amount(vertical)]}));
                }
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(View: ignore inhibit_manager::ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(View: ignore inhibitor::ZwpKeyboardShortcutsInhibitorV1);
delegate_compositor!(View);
delegate_output!(View);
delegate_registry!(View);
delegate_seat!(View);
delegate_keyboard!(View);
delegate_pointer!(View);
delegate_xdg_shell!(View);
delegate_xdg_window!(View);
impl ProvidesRegistryState for View {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<wp_presentation::WpPresentation, ()> for View {
    fn event(_: &mut Self, _: &wp_presentation::WpPresentation, event: wp_presentation::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            eprintln!("presentation_clock_id={clk_id}");
        }
    }
}
impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u64> for View {
    fn event(_: &mut Self, _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event, frame: &u64, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, .. } => {
                ibara_screen::trace_at("presented", *frame,
                    ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000 + tv_nsec as u64 / 1000);
            },
            wp_presentation_feedback::Event::Discarded => ibara_screen::trace("discarded", *frame),
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u64> for View {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, _: wl_callback::Event,
        frame: &u64, _: &Connection, _: &QueueHandle<Self>) {
        ibara_screen::trace("frame_callback", *frame);
    }
}
