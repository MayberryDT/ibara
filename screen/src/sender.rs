use crate::{
    capture::{self, Frame, Health},
    identity,
    input::Input,
};
use anyhow::{Context, Result, ensure};
use ibara_screen::{
    admission::{Admission, Decision},
    turn::{Action, TurnGate},
    wire::{self, InputSequence, VideoHeader},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};
struct Viewer {
    cert: String,
    connection: quinn::Connection,
    control: mpsc::Sender<Value>,
}
struct State {
    admission: Admission,
    turn: TurnGate,
    input: Input,
    viewers: HashMap<usize, Viewer>,
    last_report: Instant,
    dirty: bool,
    width: u32,
    height: u32,
    fps: u32,
    encoder: String,
}
fn emit(v: Value) {
    println!("{}", v);
}
impl State {
    fn settle(&mut self) -> bool {
        self.turn.clear();
        self.input.settle().is_ok() && self.input.held() == 0
    }
    fn turn_notice(&self) {
        for v in self.viewers.values() {
            let _=v.control.try_send(json!({"t":"turn","yours":self.turn.is_operator(&v.cert),"holder":self.turn.holder}));
        }
    }
    fn close_viewers(&mut self, reason: &str) {
        for v in self.viewers.values() {
            v.connection.close(0u32.into(), reason.as_bytes());
        }
        self.viewers.clear();
    }
    fn viewer_event(&self, cert: &str, attached: bool) {
        let operator_viewers = self.viewers.values().filter(|v| v.cert == cert).count();
        emit(json!({"v":1,"t":"viewer","attached":attached,"count":self.viewers.len(),
            "operator_cert_sha256":cert,"operator_viewers":operator_viewers}));
    }
    fn report_input(&mut self) {
        self.dirty = true;
        if self.last_report.elapsed() >= Duration::from_millis(250) {
            emit(json!({"v":1,"t":"person_input","held":self.input.held()}));
            self.last_report = Instant::now();
            self.dirty = false;
        }
    }
}
pub async fn run(dir: &Path) -> Result<()> {
    unsafe {
        let parent = libc::getppid();
        ensure!(
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == 0,
            "parent death signal failed"
        );
        ensure!(libc::getppid() == parent, "parent exited during startup");
    }
    let id = identity::Identity::load(dir)?;
    let server = identity::server(&id)?;
    let output = std::process::Command::new("tailscale").arg("ip").output()?;
    ensure!(output.status.success(), "Tailscale addresses unavailable");
    let mut endpoints = vec![];
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        let ip: IpAddr = line.parse()?;
        let tailnet = match ip {
            IpAddr::V4(a) => a.octets()[0] == 100 && (64..=127).contains(&a.octets()[1]),
            IpAddr::V6(a) => a.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
        };
        ensure!(tailnet, "refusing non-Tailscale bind address");
        endpoints.push(quinn::Endpoint::server(
            server.clone(),
            SocketAddr::new(ip, 47910),
        )?);
    }
    ensure!(!endpoints.is_empty(), "no tailnet address");
    let health = Arc::new(Health::default());
    let (frames, rx) = watch::channel(None);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let h = health.clone();
    let capture_thread = std::thread::spawn(move || {
        if let Err(e) = capture::run(frames, h.clone(), ready_tx.clone(), start_rx) {
            let _ = ready_tx.send(Err(anyhow::anyhow!(e.to_string())));
            eprintln!("capture failed: {e:#}");
            emit(json!({"v":1,"t":"failed","reason":e.to_string()}));
            h.stop.store(true, Ordering::Relaxed);
        }
    });
    let (w, h, fps, capture_name, encoder_name) =
        tokio::task::spawn_blocking(move || ready_rx.recv()).await???;
    let state = Arc::new(Mutex::new(State {
        admission: Admission::default(),
        turn: TurnGate::default(),
        input: Input::new(w, h)?,
        viewers: HashMap::new(),
        last_report: Instant::now() - Duration::from_secs(1),
        dirty: false,
        width: w,
        height: h,
        fps,
        encoder: encoder_name.clone(),
    }));
    emit(
        json!({"v":1,"t":"ready","port":47910,"cert_sha256":identity::hash(&id.cert),"encoder":encoder_name,"capture":capture_name,"command_results":true}),
    );
    start_tx
        .send(())
        .context("capture worker stopped during startup")?;
    for ep in &endpoints {
        let ep = ep.clone();
        let s = state.clone();
        let h = health.clone();
        let rx = rx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let s = s.clone();
                let h = h.clone();
                let rx = rx.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        if let Err(e) = session(conn.clone(), s, h, rx).await {
                            eprintln!("viewer session: {e:#}");
                            conn.close(0x401u32.into(), b"session ended");
                        }
                    }
                });
            }
        });
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {line=lines.next_line()=>{
        let Some(line)=line? else{break};let request:Value=match serde_json::from_str(&line){Ok(v)=>v,Err(_)=>{emit(json!({"v":1,"t":"error","reason":"Malformed control JSON"}));continue;}};
        if request.get("v").and_then(Value::as_u64)!=Some(1){emit(json!({"v":1,"t":"error","reason":"Unsupported control version"}));continue;}
        let mut s=state.lock().unwrap();let op=request.get("t").or_else(||request.get("op")).and_then(Value::as_str).unwrap_or("");let result:Result<()>=match op {
        "open"=>request.get("generation").and_then(Value::as_u64).context("missing generation").and_then(|g|{s.admission.open(g)?;s.close_viewers("generation changed");ensure!(s.settle(),"input did not settle");Ok(())}),
        "ticket"=>{let fields=(request.get("ticket").and_then(Value::as_str),request.get("client_cert_sha256").and_then(Value::as_str),request.get("generation").and_then(Value::as_u64),request.get("expires_ms").and_then(Value::as_u64),request.get("input").and_then(Value::as_bool));match fields{(Some(t),Some(c),Some(g),Some(e),Some(i))=>s.admission.ticket(t,c,g,e,i,crate::now_ms()),_=>Err(anyhow::anyhow!("malformed ticket"))}},
        "turn"=>{let person=request.get("person").and_then(Value::as_bool).unwrap_or(false);if !person {let ok=s.settle();emit(json!({"v":1,"t":"settled","ok":ok}));}else{let held=if let Some(cert)=request.get("operator_cert_sha256").and_then(Value::as_str){s.turn.answer(cert,request.get("accepted").and_then(Value::as_bool).unwrap_or(false),person,request.get("holder").and_then(Value::as_str))}else{s.turn.grant(true,request.get("holder").and_then(Value::as_str))};for event in held{let _=s.input.deliver(&event);}s.report_input();}s.turn_notice();Ok(())},
        "settle"=>{let ok=s.settle();s.turn_notice();emit(json!({"v":1,"t":"settled","ok":ok}));Ok(())},
        "revoke"=>{s.admission.revoke()?;s.close_viewers("revoked");let ok=s.settle();emit(json!({"v":1,"t":"settled","ok":ok}));Ok(())},
        "status"=>{let captured=health.last_capture.load(Ordering::Relaxed);emit(json!({"v":1,"t":"status","generation":s.admission.generation,"viewers":s.viewers.len(),"encoder":s.encoder,"last_frame_age_ms":if captured==0{Value::Null}else{json!(crate::now_us().saturating_sub(captured)/1000)},"target_idle":health.idle.load(Ordering::Relaxed),"held":s.input.held()}));Ok(())},
        "stop"=>break,_=>Err(anyhow::anyhow!("unknown control command"))};
        if let Some(id)=request.get("id").and_then(Value::as_u64) {
            emit(json!({"v":1,"t":"command_result","id":id,"ok":result.is_ok(),"reason":result.as_ref().err().map(ToString::to_string)}));
        }
        if let Err(e)=result{emit(json!({"v":1,"t":"error","op":op,"reason":e.to_string()}));}
        },_=timer.tick()=>{let mut s=state.lock().unwrap();if s.dirty {s.report_input();}health.viewers.store(s.viewers.len()as u64,Ordering::Relaxed);if health.stop.load(Ordering::Relaxed){break;}},_=tokio::signal::ctrl_c()=>break,_=terminate.recv()=>break}
    }
    let mut s = state.lock().unwrap();
    s.admission.revoke()?;
    s.close_viewers("sender stopped");
    let ok = s.settle();
    emit(json!({"v":1,"t":"settled","ok":ok}));
    health.stop.store(true, Ordering::Relaxed);
    drop(s);
    for ep in endpoints {
        ep.close(0u32.into(), b"stopped");
    }
    capture_thread
        .join()
        .map_err(|_| anyhow::anyhow!("capture worker panicked during shutdown"))?;
    Ok(())
}
async fn session(
    conn: quinn::Connection,
    state: Arc<Mutex<State>>,
    health: Arc<Health>,
    frames: watch::Receiver<Option<Frame>>,
) -> Result<()> {
    let result = attach(&conn, state.clone(), health.clone(), frames).await;
    let mut s = state.lock().unwrap();
    let id = conn.stable_id();
    if let Some(viewer) = s.viewers.remove(&id) {
        if s.turn.owns_input(&viewer.cert) {
            let _ = s.input.settle();
            if !s.viewers.values().any(|v| v.cert == viewer.cert) {
                s.turn.disconnect(&viewer.cert);
            }
            s.report_input();
            s.turn_notice();
        }
        s.viewer_event(&viewer.cert, false);
    }
    health
        .viewers
        .store(s.viewers.len() as u64, Ordering::Relaxed);
    result
}
async fn attach(
    conn: &quinn::Connection,
    state: Arc<Mutex<State>>,
    health: Arc<Health>,
    mut frames: watch::Receiver<Option<Frame>>,
) -> Result<()> {
    let certificates = conn
        .peer_identity()
        .context("no peer identity")?
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("invalid client certificate"))?;
    let cert = identity::hash(certificates.first().context("empty client certificate")?);
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(3), conn.accept_bi()).await??;
    let ticket = tokio::time::timeout(Duration::from_secs(3), wire::read_json(&mut recv)).await??;
    if ticket.get("t").and_then(Value::as_str) != Some("ticket") {
        conn.close(0x401u32.into(), b"ticket required");
        anyhow::bail!("ticket required");
    }
    let (notice_tx, mut notices) = mpsc::channel(32);
    let (input, generation, w, h, fps) = {
        let mut s = state.lock().unwrap();
        ensure!(s.viewers.len() < 4, "viewer limit");
        match s.admission.admit(
            ticket.get("ticket").and_then(Value::as_str).unwrap_or(""),
            &cert,
            crate::now_ms(),
        ) {
            Decision::Refused => {
                conn.close(0x401u32.into(), b"ticket refused");
                anyhow::bail!("ticket refused");
            }
            Decision::Accepted { input, generation } => {
                s.viewers.insert(
                    conn.stable_id(),
                    Viewer {
                        cert: cert.clone(),
                        connection: conn.clone(),
                        control: notice_tx.clone(),
                    },
                );
                s.viewer_event(&cert, true);
                (input, generation, s.width, s.height, s.fps)
            }
        }
    };
    health.viewers.fetch_add(1, Ordering::Relaxed);
    health.force_idr.store(true, Ordering::Relaxed);
    wire::write_json(
        &mut send,
        &json!({"t":"hello","v":1,"codec":"h264","width":w,"height":h,"fps":fps,"input":input}),
    )
    .await?;
    state.lock().unwrap().turn_notice();
    let input_conn = conn.clone();
    let s = state.clone();
    let input_cert = cert.clone();
    let ctl = notice_tx.clone();
    let mut input_task = crate::AbortTask(tokio::spawn(async move {
        let mut input_stream = input_conn.accept_uni().await?;
        let mut seq = InputSequence::default();
        loop {
            let message = wire::read_json(&mut input_stream).await?;
            ensure!(
                message.get("t").and_then(Value::as_str) == Some("input"),
                "expected input"
            );
            let from = message
                .get("from")
                .and_then(Value::as_u64)
                .context("missing input sequence")?;
            let events = message
                .get("events")
                .and_then(Value::as_array)
                .context("missing input events")?;
            let upto = match seq.consume(from, events.len()) {
                Ok(u) => u,
                Err(_) => {
                    ctl.send(json!({"t":"input_gap","expected":seq.expected().unwrap_or(from)}))
                        .await?;
                    continue;
                }
            };
            {
                let mut s = s.lock().unwrap();
                if s.admission.open && s.admission.generation == generation {
                    for event in events {
                        match s.turn.event(&input_cert, input, event.clone()) {
                            Action::Request => emit(
                                json!({"v":1,"t":"turn_request","operator_cert_sha256":input_cert}),
                            ),
                            Action::Deliver(e) => {
                                let _ = s.input.deliver(&e);
                                s.report_input();
                            }
                            Action::Refused => {
                                let _ = ctl.try_send(json!({"t":"turn","yours":false,"holder":s.turn.holder}));
                            }
                            Action::Ignore => {}
                        }
                    }
                }
            }
            ctl.send(json!({"t":"input_ack","upto":upto})).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    }));
    let (mut control, _reader) = crate::control_reader(recv);
    let video_conn = conn.clone();
    let video_health = health.clone();
    let video_state = state.clone();
    let mut video_task = crate::AbortTask(tokio::spawn(async move {
        let conn = video_conn;
        let health = video_health;
        let mut first = true;
        let mut last_sent = None;
        let mut key_only = true;
        let mut last_reset = Instant::now();
        let mut resets = 0u32;
        let mut in_flight = tokio::task::JoinSet::new();
        loop {
            if first {
                first = false;
            } else {
                frames.changed().await?;
            }
            let current = { frames.borrow_and_update().clone() };
            if let Some(frame) = current {
                {
                    let mut s = video_state.lock().unwrap();
                    if (s.width, s.height, s.fps) != (frame.width, frame.height, frame.fps) {
                        ensure!(s.input.settle().is_ok(), "geometry input did not settle");
                        s.input = Input::new(frame.width, frame.height)?;
                        emit(json!({"v":1,"t":"person_input","held":0}));
                        s.turn_notice();
                        s.width = frame.width;
                        s.height = frame.height;
                        s.fps = frame.fps;
                        for v in s.viewers.values() {
                            let _=v.control.try_send(json!({"t":"geometry","width":frame.width,"height":frame.height,"fps":frame.fps}));
                        }
                        key_only = true;
                    }
                }
                if last_sent == Some(frame.sequence) {
                    continue;
                }
                if key_only && !frame.keyframe {
                    health.force_idr.store(true, Ordering::Relaxed);
                    continue;
                }
                key_only = false;
                while let Some(result) = in_flight.try_join_next() {
                    if !matches!(result, Ok(true)) {
                        key_only = true;
                        health.force_idr.store(true, Ordering::Relaxed);
                        resets += 1;
                        if resets >= 2 {
                            health.bitrate.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                                Some((b * 3 / 4).max(1_000_000))
                            }).ok();
                        }
                        last_reset = Instant::now();
                    }
                }
                if key_only && !frame.keyframe {
                    continue;
                }
                key_only = false;
                if in_flight.len() >= 4 {
                    health.force_idr.store(true, Ordering::Relaxed);
                    key_only = true;
                    continue;
                }
                let started = tokio::time::Instant::now();
                let mut stream = conn.open_uni().await?;
                stream.set_priority(-10)?;
                let write = async {
                    ibara_screen::trace("send_start", frame.capture_us);
                    stream
                        .write_all(
                            &VideoHeader {
                                keyframe: frame.keyframe,
                                sequence: frame.sequence,
                                capture_us: frame.capture_us,
                            }
                            .encode(),
                        )
                        .await?;
                    stream.write_u32(frame.bytes.len() as u32).await?;
                    stream.write_all(&frame.bytes).await?;
                    stream.finish()?;
                    Ok::<(), anyhow::Error>(())
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_millis(100), write).await,
                    Ok(Ok(()))
                ) {
                    let _ = stream.reset(1u32.into());
                    health.force_idr.store(true, Ordering::Relaxed);
                    key_only = true;
                    resets += 1;
                    if resets >= 2 {
                        health
                            .bitrate
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                                Some((b * 3 / 4).max(1_000_000))
                            })
                            .ok();
                    }
                    last_reset = Instant::now();
                } else if last_reset.elapsed() > Duration::from_secs(10) {
                    resets = 0;
                    health
                        .bitrate
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            Some((b * 4 / 3).min(10_000_000))
                        })
                        .ok();
                    last_reset = Instant::now();
                }
                let h = health.clone();
                in_flight.spawn(async move {
                    let delivered = matches!(tokio::time::timeout_at(started + Duration::from_millis(100), stream.stopped()).await, Ok(Ok(None)));
                    if !delivered {
                        let _ = stream.reset(1u32.into());
                        h.force_idr.store(true, Ordering::Relaxed);
                    }
                    delivered
                });
                last_sent = Some(frame.sequence);
            }
        }
        Ok::<(), anyhow::Error>(())
    }));
    let mut ping_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        tokio::select! {
        _=tokio::time::sleep_until(ping_deadline)=>{eprintln!("viewer connection closed: ping timeout");conn.close(0u32.into(), b"ping timeout");break;},
        m=control.recv()=>{let m=m.context("control closed")??;match m.get("t").and_then(Value::as_str){Some("ping")=>{ping_deadline=tokio::time::Instant::now()+Duration::from_secs(1);wire::write_json(&mut send,&json!({"t":"pong"})).await?;},Some("keyframe")=>health.force_idr.store(true,Ordering::Relaxed),_=>anyhow::bail!("unexpected control message")}},
        Some(m)=notices.recv()=>wire::write_json(&mut send,&m).await?,
        result=&mut video_task.0=>{result??;break;},
        result=&mut input_task.0=>{result??;break;},
        reason=conn.closed()=>{eprintln!("viewer connection closed: {reason}");break;}}
    }
    drop(input_task);
    drop(video_task);
    Ok(())
}
