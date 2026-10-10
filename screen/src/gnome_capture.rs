//! GNOME acquisition for the active sender; no remote-desktop/input API.
//! Qualified separately before selecting it in capture::run.
use anyhow::{Context,Result,ensure};
use futures_util::StreamExt;
use pipewire as pw;
use pw::{spa,properties::properties};
use std::{cell::RefCell,rc::Rc,collections::HashMap,time::{Duration,Instant}};
use zbus::zvariant::{OwnedObjectPath,OwnedValue};
#[cfg(feature = "qualification")]
use std::path::Path;

#[path="../../shared/gnome_target.rs"]
mod eligibility;
fn require_package()->Result<()> {
 let version=match option_env!("IBARA_PKGREL") {
  Some(rel) if !rel.is_empty()=>format!("{}-{rel}",env!("IBARA_CORE_VERSION")),
  _=>format!("{}-dev.{}",env!("IBARA_CORE_VERSION"),option_env!("IBARA_BUILD_ID").unwrap_or("local")),
 };
 eligibility::require_runtime_target(&version).map_err(anyhow::Error::msg)
}

struct ClosedWatch {
    closed:std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop:std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread:Option<std::thread::JoinHandle<()>>,
}
impl Drop for ClosedWatch {fn drop(&mut self) {
    self.stop.store(true,std::sync::atomic::Ordering::SeqCst);
    if let Some(thread)=self.thread.take() {let _=thread.join();}
}}
impl ClosedWatch {
    async fn open(connection:zbus::Connection,owner:String,path:OwnedObjectPath)->Result<Self> {
        use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
        let closed=Arc::new(AtomicBool::new(false));let stop=Arc::new(AtomicBool::new(false));
        let (ready_tx,ready_rx)=std::sync::mpsc::channel();
        let worker_closed=closed.clone();let worker_stop=stop.clone();
        let thread=std::thread::spawn(move || {
            let result=(||->Result<()> {
                let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(async {
                    let proxy=zbus::Proxy::new(&connection,owner.as_str(),path.as_str(),"org.gnome.Mutter.ScreenCast.Session").await?;
                    let mut signals=proxy.receive_signal("Closed").await?;
                    ready_tx.send(()).ok();
                    while !worker_stop.load(Ordering::SeqCst) {
                        tokio::select! {
                            _=signals.next()=>{worker_closed.store(true,Ordering::SeqCst);break;},
                            _=tokio::time::sleep(Duration::from_millis(50))=>{},
                        }
                    }
                    Ok(())
                })
            })();
            if result.is_err() {worker_closed.store(true,Ordering::SeqCst);}
        });
        let watch=Self {closed,stop,thread:Some(thread)};
        tokio::task::spawn_blocking(move ||ready_rx.recv_timeout(Duration::from_secs(2))).await??;
        Ok(watch)
    }
}

struct Session {
    watch:ClosedWatch,
    connection:zbus::blocking::Connection,
    owner:String,
    path:OwnedObjectPath,
    epoch:String,
    generation:u64,
    monitor:serde_json::Value,
    node:u32,
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(proxy)=zbus::blocking::Proxy::new(&self.connection,self.owner.as_str(),self.path.as_str(),"org.gnome.Mutter.ScreenCast.Session") {
            let _:zbus::Result<()>=proxy.call("Stop",&());
        }
    }
}
impl Session {
    fn state(&self)->Result<serde_json::Value> {
        require_package()?;
        ensure!(!self.watch.closed.load(std::sync::atomic::Ordering::SeqCst),"Screencast session closed");
        let dbus=zbus::blocking::Proxy::new(&self.connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus")?;
        let owner:String=dbus.call("GetNameOwner",&("org.gnome.Shell",))?;
        ensure!(owner==self.owner,"Shell owner changed during capture");
        let helper=zbus::blocking::Proxy::new(&self.connection,self.owner.as_str(),"/org/ibara/Gnome","org.ibara.Gnome")?;
        let text:String=helper.call("GetState",&())?;
        ensure!(text.len()<=1024*1024,"GNOME capture state exceeds limit");
        let state:serde_json::Value=serde_json::from_str(&text)?;
        ensure!(state["capture_api"]==1 && state["locked"]==false && state["epoch"]==self.epoch && state["session_generation"]==self.generation,
            "GNOME capture session was revoked");
        ensure!(state["monitors"].as_array().is_some_and(|m|m.len()==1 && m[0]["scale"].as_f64()==Some(1.0) && m.iter().any(|m|
            ["id","name","x","y","width","height","scale"].iter().all(|key|m[*key]==self.monitor[*key]))),"Capture output changed");
        Ok(state)
    }
    fn open()->Result<Self> {
        require_package()?;
        // A private API pinned to the inspected Mutter version, with bounded
        // asynchronous signal startup; never wait forever for a missing node.
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let connection=zbus::connection::Builder::session()?.method_timeout(Duration::from_secs(2)).build().await?;
            let dbus=zbus::Proxy::new(&connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus").await?;
            let owner:String=dbus.call("GetNameOwner",&("org.gnome.Shell",)).await?;
            for name in ["org.ibara.Gnome","org.gnome.Mutter.ScreenCast","org.gnome.Mutter.DisplayConfig"] {
                let actual:String=dbus.call("GetNameOwner",&(name,)).await?;
                ensure!(actual==owner,"Capture service is not owned by GNOME Shell");
            }
            let helper=zbus::Proxy::new(&connection,owner.as_str(),"/org/ibara/Gnome","org.ibara.Gnome").await?;
            let text:String=helper.call("GetState",&()).await?;
            ensure!(text.len()<=1024*1024,"GNOME state exceeds limit");
            let state:serde_json::Value=serde_json::from_str(&text)?;
            ensure!(state["capture_api"]==1 && state["locked"]==false,"GNOME capture unavailable while locked or helper incompatible");
            let epoch=state["epoch"].as_str().context("No capture epoch")?.to_owned();
            let generation=state["session_generation"].as_u64().context("No capture generation")?;
            let monitors=state["monitors"].as_array().context("No capture outputs")?;
            ensure!(monitors.len()==1,"GNOME capture supports one output at scale1 only");
            let monitor=monitors[0].clone();
            let scale=monitor["scale"].as_f64().context("No output scale")?;
            ensure!(scale==1.0,"GNOME capture supports one output at scale1 only");
            let x=i32::try_from(monitor["x"].as_i64().context("No output X")?)?;
            let y=i32::try_from(monitor["y"].as_i64().context("No output Y")?)?;
            let width=monitor["width"].as_f64().context("No output width")?/scale;
            let height=monitor["height"].as_f64().context("No output height")?/scale;
            ensure!(width>=2.0 && height>=2.0 && width<=16384.0 && height<=16384.0 && width.fract()==0.0 && height.fract()==0.0,"Invalid logical output geometry");
            type Spec=(String,String,String,String);
            type Properties=HashMap<String,OwnedValue>;
            type Mode=(String,i32,i32,f64,f64,Vec<f64>,Properties);
            type Physical=(Spec,Vec<Mode>,Properties);
            type Logical=(i32,i32,f64,u32,bool,Vec<Spec>,Properties);
            let config=zbus::Proxy::new(&connection,owner.as_str(),"/org/gnome/Mutter/DisplayConfig","org.gnome.Mutter.DisplayConfig").await?;
            let (_,_,logical,_):(u32,Vec<Physical>,Vec<Logical>,Properties)=config.call("GetCurrentState",&()).await?;
            let matching=logical.iter().filter(|m|m.0==x && m.1==y && m.2==scale).collect::<Vec<_>>();
            ensure!(matching.len()==1 && !matching[0].5.is_empty(),"Output connector could not be proved uniquely");
            let connector=&matching[0].5[0].0;
            let cast=zbus::Proxy::new(&connection,owner.as_str(),"/org/gnome/Mutter/ScreenCast","org.gnome.Mutter.ScreenCast").await?;
            let version:i32=cast.get_property("Version").await?;
            ensure!(version>=2,"ScreenCast cursor mode unavailable");
            let empty:HashMap<&str,OwnedValue>=HashMap::new();
            let path:OwnedObjectPath=cast.call("CreateSession",&(empty,)).await?;
            let proxy=zbus::Proxy::new(&connection,owner.as_str(),path.as_str(),"org.gnome.Mutter.ScreenCast.Session").await?;
            let properties=HashMap::from([("cursor-mode",OwnedValue::from(1u32))]);
            let stream_path:OwnedObjectPath=proxy.call("RecordMonitor",&(connector,properties)).await?;
            let stream=zbus::Proxy::new(&connection,owner.as_str(),stream_path.as_str(),"org.gnome.Mutter.ScreenCast.Stream").await?;
            let parameters:HashMap<String,OwnedValue>=stream.get_property("Parameters").await?;
            let position:(i32,i32)=parameters.get("position").context("No stream position")?.try_clone()?.try_into()?;
            let output_name:String=parameters.get("output-name").context("No stream connector")?.try_clone()?.try_into()?;
            ensure!(position==(x,y) && output_name==*connector,"Stream output disagrees with requested monitor");
            let size:(i32,i32)=parameters.get("size").context("No stream logical size")?.try_clone()?.try_into()?;
            ensure!(size==(width as i32,height as i32),"Stream logical size {:?} disagrees with requested {}x{}",size,width,height);
            let mut nodes=stream.receive_signal("PipeWireStreamAdded").await?;
            let watch=ClosedWatch::open(connection.clone(),owner.clone(),path.clone()).await?;
            let _:()=proxy.call("Start",&()).await?;
            let signal=tokio::time::timeout(Duration::from_secs(3),nodes.next()).await?.context("Capture closed before node")?;
            let node=signal.body().deserialize::<u32>()?;
            ensure!(node!=0,"Invalid PipeWire node");
            Ok(Self {watch,connection:connection.clone().into(),owner:owner.clone(),path:path.clone(),epoch,generation,monitor,node})
        })
    }
}

pub struct Pixels { pub width:u32,pub height:u32,pub rgba:Vec<u8> }
#[derive(Default)]
struct Intake { format:spa::param::video::VideoInfoRaw, frame:Option<Pixels>, error:Option<String> }

struct RawCapture {
    // Remove callbacks before releasing the stream and its owned core/context.
    _listener:pw::stream::StreamListener<Rc<RefCell<Intake>>>,
    stream:pw::stream::StreamRc,
    intake:Rc<RefCell<Intake>>,
    _core:pw::core::CoreRc,
    _context:pw::context::ContextRc,
    mainloop:pw::main_loop::MainLoopRc,
    session:Session,
}
impl RawCapture {
 fn open()->Result<Self> {
        require_package()?;
    let session=Session::open()?;
    pw::init();
    let mainloop=pw::main_loop::MainLoopRc::new(None)?;
    let context=pw::context::ContextRc::new(&mainloop,None)?;
    let core=context.connect_rc(None)?;
    let stream=pw::stream::StreamRc::new(core.clone(),"ibara-gnome-capture",properties! {
        *pw::keys::MEDIA_TYPE=>"Video", *pw::keys::MEDIA_CATEGORY=>"Capture", *pw::keys::MEDIA_ROLE=>"Screen",
    })?;
    let intake=Rc::new(RefCell::new(Intake::default()));
    let _listener=stream.add_local_listener_with_user_data(intake.clone())
        .state_changed(|_,data,_,state| {
            if let pw::stream::StreamState::Error(error)=state { data.borrow_mut().error=Some(error.to_string()); }
        })
        .param_changed(|_,data,id,param| {
            if id!=spa::param::ParamType::Format.as_raw() {return;}
            let mut data=data.borrow_mut();
            match param.and_then(|p|data.format.parse(p).ok()) {
                Some(_)=>{let size=data.format.size();
                    if data.format.format()!=spa::param::video::VideoFormat::BGRx || size.width<2 || size.height<2 || size.width>4096 || size.height>4096 {
                        data.error=Some("Unsupported PipeWire format or geometry".into());
                    }
                },
                None=>data.error=Some("Invalid PipeWire video format".into()),
            }
        })
        .process(|stream,data| {
            let result=(||->Result<()> {
                let Some(mut buffer)=stream.dequeue_buffer() else {return Ok(());};
                let mut intake=data.borrow_mut();
                let size=intake.format.size();
                ensure!(intake.format.format()==spa::param::video::VideoFormat::BGRx,"Unnegotiated capture format");
                let width=size.width;let height=size.height;
                ensure!((2..=4096).contains(&width) && (2..=4096).contains(&height),"Invalid capture dimensions");
                let datas=buffer.datas_mut();ensure!(datas.len()==1,"Unsupported capture planes");
                let data=&mut datas[0];
                let offset=data.chunk().offset() as usize;let length=data.chunk().size() as usize;
                let stride=data.chunk().stride();
                ensure!(stride>=width as i32*4,"Invalid capture stride");
                let required=(height as usize-1)*stride as usize+width as usize*4;
                let bytes=data.data().context("PipeWire buffer is not mapped")?;
                ensure!(offset<=bytes.len() && length<=bytes.len()-offset && required<=length,"Short capture buffer");
                let mut rgba=vec![0u8;width as usize*height as usize*4];
                for y in 0..height as usize {for x in 0..width as usize {
                    let src=offset+y*stride as usize+x*4;let dst=(y*width as usize+x)*4;
                    rgba[dst..dst+4].copy_from_slice(&[bytes[src+2],bytes[src+1],bytes[src],255]);
                }}
                intake.frame=Some(Pixels {width,height,rgba}); // owned copy before requeue
                Ok(())
            })();
            if let Err(error)=result {data.borrow_mut().error=Some(error.to_string());}
        }).register()?;
    let format=spa::pod::object!(spa::utils::SpaTypes::ObjectParamFormat,spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType,Id,spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype,Id,spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(spa::param::format::FormatProperties::VideoFormat,Id,spa::param::video::VideoFormat::BGRx));
    let serialized=spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()),&spa::pod::Value::Object(format))?.0.into_inner();
    let mut params=[spa::pod::Pod::from_bytes(&serialized).context("Invalid capture format pod")?];
    stream.connect(spa::utils::Direction::Input,Some(session.node),pw::stream::StreamFlags::AUTOCONNECT|pw::stream::StreamFlags::MAP_BUFFERS,&mut params)?;
    Ok(Self {session,mainloop,_context:context,_core:core,stream,_listener,intake})
 }
 fn next(&self)->Result<Option<Pixels>> {
    ensure!(!self.session.watch.closed.load(std::sync::atomic::Ordering::SeqCst),"Screencast session closed");
    self.mainloop.loop_().iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    if let Some(error)=self.intake.borrow_mut().error.take() {anyhow::bail!(error);}
    Ok(self.intake.borrow_mut().frame.take())
 }
 fn first(&self)->Result<Pixels> {
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        self.session.state()?;
        if let Some(frame)=self.next()? {
            ensure!(Some(frame.width as f64)==self.session.monitor["width"].as_f64() &&
                Some(frame.height as f64)==self.session.monitor["height"].as_f64(),"Captured pixels disagree with trusted output dimensions");
            return Ok(frame);
        }
        ensure!(Instant::now()<deadline,"PipeWire frame startup timed out");
    }
 }
}

#[cfg(feature = "qualification")]
pub fn proof(output:&Path)->Result<()> {
    let capture=RawCapture::open()?;
    let pixels=capture.first()?;
    let mut ppm=format!("P6\n{} {}\n255\n",pixels.width,pixels.height).into_bytes();
    for pixel in pixels.rgba.chunks_exact(4) {ppm.extend_from_slice(&pixel[..3]);}
    std::fs::write(output,ppm)?;
    let mut encoder=crate::software::Encoder::new(pixels.width,pixels.height,15)?;
    let encoded=encoder.encode_rgba(&pixels.rgba,pixels.width,pixels.height,true)?;
    ensure!(!encoded.is_empty() && encoded.iter().any(|frame|frame.0),"Capture encoder produced no keyframe");
    let bytes=encoded.into_iter().flat_map(|frame|frame.1).collect::<Vec<_>>();
    std::fs::write(output.with_extension("h264"),bytes)?;
    let session=&capture.session;
    let receipt=serde_json::json!({"layer":"Shell/PipeWire raw acquisition; not MCP acceptance","width":pixels.width,"height":pixels.height,
        "monitor":session.monitor,"epoch":session.epoch,"generation":session.generation,"format":"BGRx","node":session.node,"input_api_used":false,"encoded_width":encoder.width,"encoded_height":encoder.height,"encoder":"openh264"});
    std::fs::write(output.with_extension("json"),serde_json::to_vec_pretty(&receipt)?)?;
    println!("{receipt}");Ok(())
}

pub fn run(tx:tokio::sync::watch::Sender<Option<crate::capture::Frame>>,
    health:std::sync::Arc<crate::capture::Health>,
    ready:std::sync::mpsc::Sender<Result<(u32,u32,u32,String,String)>>,
    start:std::sync::mpsc::Receiver<()>)->Result<()> {
    use std::sync::atomic::Ordering;
    let capture=RawCapture::open()?;
    let initial=capture.first()?;
    let source=(initial.width,initial.height);
    let mut encoder=crate::software::Encoder::new(source.0,source.1,15)?;
    capture.stream.set_active(false)?;
    capture.intake.borrow_mut().frame=None;
    ready.send(Ok((encoder.width,encoder.height,encoder.fps,"gnome-pipewire-bgrx".into(),"openh264".into()))).ok();
    start.recv().context("Sender stopped during GNOME capture startup")?;
    let mut active=false;let mut sequence=0;
    let mut checked=Instant::now()-Duration::from_secs(1);
    let mut encoded=Instant::now()-Duration::from_secs(1);
    while !health.stop.load(Ordering::Relaxed) {
        if checked.elapsed()>=Duration::from_millis(250) {capture.session.state()?;checked=Instant::now();}
        let wanted=health.viewers.load(Ordering::Relaxed)>0;
        if active!=wanted {
            capture.stream.set_active(wanted)?;active=wanted;
            capture.intake.borrow_mut().frame=None;
            if active {health.force_idr.store(true,Ordering::Relaxed);}
            else {tx.send_replace(None);} // Do not offer a cached pre-idle frame to a resumed viewer.
        }
        health.idle.store(!active,Ordering::Relaxed);
        let Some(frame)=capture.next()? else {continue;};
        ensure!((frame.width,frame.height)==source,"GNOME capture dimensions changed; restart with fresh geometry");
        if !active || encoded.elapsed()<Duration::from_secs_f64(1.0/encoder.fps as f64) {continue;}
        capture.session.state()?; // Revalidate immediately before publishing an encoded frame.
        encoder.bitrate(health.bitrate.load(Ordering::Relaxed))?;
        let key=health.force_idr.swap(false,Ordering::Relaxed);
        let now=crate::now_us();
        for (keyframe,bytes) in encoder.encode_rgba(&frame.rgba,frame.width,frame.height,key)? {
            sequence+=1;
            tx.send_replace(Some(crate::capture::Frame {width:encoder.width,height:encoder.height,fps:encoder.fps,
                sequence,capture_us:now,keyframe,bytes:std::sync::Arc::new(bytes)}));
        }
        health.last_capture.store(now,Ordering::Relaxed);encoded=Instant::now();
    }
    Ok(())
}

/// Exercise the active acquisition/encoder worker without network pairing.
#[cfg(feature = "qualification")]
pub async fn stream_proof(output:&Path)->Result<()> {
    let root=output.parent().context("No fixture root")?.to_owned();
    tokio::task::spawn_blocking(move ||focus_fixture(&root)).await??;
    use std::sync::{Arc,atomic::Ordering};
    let health=Arc::new(crate::capture::Health::default());
    let (tx,mut rx)=tokio::sync::watch::channel(None);
    let (ready_tx,ready_rx)=std::sync::mpsc::channel();
    let (start_tx,start_rx)=std::sync::mpsc::channel();
    let worker_health=health.clone();
    let thread=std::thread::spawn(move ||crate::capture::run(tx,worker_health,ready_tx,start_rx));
    struct Worker {health:Arc<crate::capture::Health>,thread:Option<std::thread::JoinHandle<Result<()>>>}
    impl Drop for Worker {fn drop(&mut self) {
        self.health.stop.store(true,Ordering::Relaxed);
        if let Some(thread)=self.thread.take() {let _=thread.join();}
    }}
    let mut worker=Worker {health:health.clone(),thread:Some(thread)};
    let ready=tokio::task::spawn_blocking(move ||ready_rx.recv_timeout(Duration::from_secs(8))).await???;
    health.viewers.store(1,Ordering::Relaxed);start_tx.send(())?;
    let mut bytes=Vec::new();let mut sequence=0;let mut dimensions=(0,0);
    for _ in 0..if std::env::var_os("IBARA_STATIC_CAPTURE").is_some() {1} else {3} {
        tokio::time::timeout(Duration::from_secs(5),rx.changed()).await.context("Active encoded frame timed out")??;
        let frame=rx.borrow_and_update().clone().context("No encoded capture frame")?;
        ensure!(frame.sequence>sequence,"Stream sequence did not advance");
        sequence=frame.sequence;dimensions=(frame.width,frame.height);bytes.extend_from_slice(&frame.bytes);
    }
    health.viewers.store(0,Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(2),async {
        while !health.idle.load(Ordering::Relaxed) {tokio::time::sleep(Duration::from_millis(25)).await;}
    }).await?;
    let idle_capture=health.last_capture.load(Ordering::Relaxed);
    let idle_sequence=rx.borrow_and_update().as_ref().context("No idle frame")?.sequence;
    tokio::time::sleep(Duration::from_secs(1)).await;
    ensure!(health.last_capture.load(Ordering::Relaxed)==idle_capture && rx.borrow().as_ref().context("No held frame")?.sequence==idle_sequence,
        "GNOME capture continued encoding without viewers");
    health.viewers.store(1,Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(5),rx.changed()).await??;
    let resumed=rx.borrow_and_update().clone().context("No resumed frame")?;
    ensure!(resumed.sequence>idle_sequence && resumed.keyframe,"Resumed stream has no fresh keyframe");
    bytes.extend_from_slice(&resumed.bytes);
    health.stop.store(true,Ordering::Relaxed);
    let joined=worker.thread.take().context("No capture worker")?;
    tokio::task::spawn_blocking(move ||joined.join()).await?.map_err(|_|anyhow::anyhow!("Capture worker panicked"))??;
    std::fs::write(output,bytes)?;
    let receipt=serde_json::json!({"passed":true,"layer":"active capture/encoder worker; not transport/MCP acceptance",
        "capture":ready.3,"encoder":ready.4,"encoded_width":dimensions.0,"encoded_height":dimensions.1,
        "last_sequence":resumed.sequence,"idle_encoding_stopped":true,"resume_keyframe":true,"worker_stopped":true});
    std::fs::write(output.with_extension("json"),serde_json::to_vec_pretty(&receipt)?)?;
    println!("{receipt}");Ok(())
}

#[cfg(feature = "qualification")]
pub fn revocation_proof(output:&Path)->Result<()> {
    let capture=RawCapture::open()?;
    let _=capture.first()?;
    let proxy=zbus::blocking::Proxy::new(&capture.session.connection,capture.session.owner.as_str(),capture.session.path.as_str(),"org.gnome.Mutter.ScreenCast.Session")?;
    let _:()=proxy.call("Stop",&())?;
    let deadline=Instant::now()+Duration::from_secs(2);
    while !capture.session.watch.closed.load(std::sync::atomic::Ordering::SeqCst) {
        ensure!(Instant::now()<deadline,"Session Closed was not witnessed");
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(capture.next().is_err(),"Closed session returned a frame");
    let receipt=serde_json::json!({"passed":true,"layer":"PipeWire/session mechanism; not MCP acceptance",
        "session_closed_witness":true,"frame_after_revocation_refused":true,"input_api_used":false});
    std::fs::write(output,serde_json::to_vec_pretty(&receipt)?)?;println!("{receipt}");Ok(())
}

#[cfg(feature = "qualification")]
fn focus_fixture(root:&Path)->Result<()> {
    let process:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("agent-process.json"))?)?;
    let pid=process["pid"].as_u64().context("No fixture process")?;
    let connection=zbus::blocking::connection::Builder::session()?.method_timeout(Duration::from_secs(2)).build()?;
    let dbus=zbus::blocking::Proxy::new(&connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus")?;
    let owner:String=dbus.call("GetNameOwner",&("org.gnome.Shell",))?;
    let helper_owner:String=dbus.call("GetNameOwner",&("org.ibara.Gnome",))?;
    ensure!(owner==helper_owner,"Fixture focus helper is not Shell-owned");
    let helper=zbus::blocking::Proxy::new(&connection,owner.as_str(),"/org/ibara/Gnome","org.ibara.Gnome")?;
    let text:String=helper.call("GetState",&())?;
    let state:serde_json::Value=serde_json::from_str(&text)?;
    let candidates=state["windows"].as_array().context("No fixture windows")?.iter()
        .filter(|w|w["pid"].as_u64()==Some(pid) && w["title"].as_str().is_some_and(|t|t.starts_with("Ibara qualification agent"))).collect::<Vec<_>>();
    ensure!(candidates.len()==1,"Capture fixture window not proved uniquely");
    let window=candidates[0];
    let identity=serde_json::json!({"epoch":state["epoch"],"address":window["address"],"pid":pid,"class":window["class"]}).to_string();
    let _:bool=helper.call("Focus",&(identity,))?;
    let deadline=Instant::now()+Duration::from_secs(2);
    loop {
        let text:String=helper.call("GetState",&())?;let observed:serde_json::Value=serde_json::from_str(&text)?;
        if observed["epoch"]==state["epoch"] && observed["focused"]==window["address"] {break;}
        ensure!(Instant::now()<deadline,"Capture fixture focus did not settle");
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[cfg(feature = "qualification")]
pub fn boundary_proof(root:&Path)->Result<()> {
    let capture=RawCapture::open()?;
    let _=capture.first()?;
    std::fs::write(root.join("capture-ready"),b"ready")?;
    let deadline=Instant::now()+Duration::from_secs(12);
    let failure=loop {
        match capture.session.state() {Err(error)=>break error.to_string(),Ok(_)=>{}}
        let _=capture.next();
        ensure!(Instant::now()<deadline,"Capture session boundary was not witnessed");
        std::thread::sleep(Duration::from_millis(20));
    };
    std::fs::write(root.join("capture-blocked"),failure.as_bytes())?;
    let deadline=Instant::now()+Duration::from_secs(12);
    while !root.join("capture-continue").exists() {
        ensure!(Instant::now()<deadline,"Capture boundary recovery missing");
        std::thread::sleep(Duration::from_millis(25));
    }
    ensure!(capture.session.state().is_err(),"Old capture identity recovered after boundary");
    drop(capture); // Match production failure cleanup before reopening.
    let deadline=Instant::now()+Duration::from_secs(5);
    let fresh=loop {
        match RawCapture::open() {
            Ok(fresh)=>break fresh,
            Err(error)=>{
                ensure!(Instant::now()<deadline,"Fresh capture did not recover: {error}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let frame=fresh.first().context("Fresh capture frame after boundary")?;
    let receipt=serde_json::json!({"passed":true,"layer":"capture boundary mechanism; not MCP acceptance",
        "failure":failure,"stale_capture_refused":true,"fresh_capture_recovered":true,"width":frame.width,"height":frame.height,"input_api_used":false});
    std::fs::write(root.join("capture-boundary.json"),serde_json::to_vec_pretty(&receipt)?)?;println!("{receipt}");Ok(())
}
