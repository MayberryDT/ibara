//! E2E peer for a real sender: decode on AMD/Intel, loss/soak, and disconnect
//! while a key is held. Emits repeatable JSON timings; never logs bundles.
#[path = "../src/codec.rs"]
mod codec;
#[path = "../src/nvenc.rs"]
mod nvenc;
#[path = "../src/nvenc_egl.rs"]
mod nvenc_egl;
#[path = "../src/identity.rs"]
mod identity;
#[path = "../src/software.rs"]
mod software;
#[path = "../src/vpp.rs"]
mod vpp;
use anyhow::{Context, Result, ensure};
use ibara_screen::wire::{self, VideoGate, VideoHeader};
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
fn read_image<'a, D: libva::SurfaceMemoryDescriptor>(
    surface: &'a libva::Surface<D>,
    display: &std::rc::Rc<libva::Display>,
    visible: (u32, u32),
    coded: (u32, u32),
) -> Result<libva::Image<'a>> {
    surface.sync().context("sample surface synchronization")?;
    if let Ok(image) = libva::Image::derive_from(surface, visible) {
        return Ok(image);
    }
    let format = display
        .query_image_formats()?
        .into_iter()
        .find(|f| f.fourcc == libva::VA_FOURCC_NV12)
        .context("NV12 readback format")?;
    libva::Image::create_from(surface, format, coded, visible)
        .context("copy decoded image for proof")
}
fn sample_notify(frame: &codec::Decoded, display: &std::rc::Rc<libva::Display>) -> Result<Vec<u8>> {
    use cros_codecs::video_frame::VideoFrame;
    ensure!(
        frame.width == 1920 && frame.height == 1080,
        "study crop requires 1080p"
    );
    let surface = frame
        .frame
        .to_native_handle(display)
        .map_err(anyhow::Error::msg)?;
    let coded = frame.frame.resolution();
    let image = read_image(
        &surface,
        display,
        (frame.width, frame.height),
        (coded.width, coded.height),
    )?;
    ensure!(
        image.image().format.fourcc == libva::VA_FOURCC_NV12,
        "study sample requires NV12"
    );
    let bytes = image.as_ref();
    let pitch = image.image().pitches[0] as usize;
    let offset = image.image().offsets[0] as usize;
    let mut blocks = Vec::with_capacity(96 * 32);
    for y in 0..32 {
        for x in 0..96 {
            let mut total = 0u32;
            for dy in 0..5 {
                for dx in 0..5 {
                    let i = offset + (26 + y * 5 + dy) * pitch + 1440 + x * 5 + dx;
                    total += *bytes.get(i).context("short VA image")? as u32;
                }
            }
            // Match the study's full-range gray threshold after limited-range NV12 luma.
            blocks.push(((total / 25).saturating_sub(16) * 255 / 219).min(255) as u8);
        }
    }
    Ok(blocks)
}
fn sample_colors(
    frame: &codec::Decoded,
    display: &std::rc::Rc<libva::Display>,
) -> Result<[[u8; 3]; 2]> {
    use cros_codecs::video_frame::VideoFrame;
    let surface = frame
        .frame
        .to_native_handle(display)
        .map_err(anyhow::Error::msg)?;
    let coded = frame.frame.resolution();
    let image = read_image(
        &surface,
        display,
        (frame.width, frame.height),
        (coded.width, coded.height),
    )?;
    ensure!(
        image.image().format.fourcc == libva::VA_FOURCC_NV12,
        "sample format"
    );
    let meta = image.image();
    let bytes = image.as_ref();
    let mut result = [[0; 3]; 2];
    for (index, y) in [frame.height / 4, frame.height * 3 / 4]
        .into_iter()
        .enumerate()
    {
        let x = (frame.width / 2) & !1;
        let luma = meta.offsets[0] as usize + y as usize * meta.pitches[0] as usize + x as usize;
        let uv =
            meta.offsets[1] as usize + (y / 2) as usize * meta.pitches[1] as usize + x as usize;
        result[index] = [
            *bytes.get(luma).context("luma")?,
            *bytes.get(uv).context("u")?,
            *bytes.get(uv + 1).context("v")?,
        ];
    }
    Ok(result)
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let id = identity::Identity::load(
        &std::path::PathBuf::from(std::env::var("HOME")?).join(".config/Ibara/screen-proof"),
    )?;
    if args.iter().any(|a| a == "--identity") {
        println!("{}", identity::hash(&id.cert));
        return Ok(());
    }
    let mut data = vec![];
    std::io::stdin().take(65537).read_to_end(&mut data)?;
    ensure!(data.len() <= 65536, "bundle size");
    let b: Value = serde_json::from_slice(&data)?;
    let start = Instant::now();
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(identity::client(
        &id,
        b["server_cert_sha256"].as_str().context("pin")?,
    )?);
    let host = b["host"].as_str().context("host")?;
    let addr = tokio::net::lookup_host((host, 47910))
        .await?
        .next()
        .context("address")?;
    let conn = endpoint.connect(addr, "ibara-screen")?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    wire::write_json(&mut send, &json!({"t":"ticket","ticket":b["ticket"]})).await?;
    let hello = wire::read_json(&mut recv).await?;
    ensure!(hello["t"] == "hello", "hello refused");
    println!(
        "{}",
        json!({"event":"hello","pid":std::process::id(),"elapsed_us":start.elapsed().as_micros()})
    );
    let mut input = conn.open_uni().await?;
    if args.iter().any(|a| a == "--hold-key") {
        wire::write_json(
            &mut input,
            &json!({"t":"input","from":0,"events":[{"key":[30,true]}]}),
        )
        .await?;
        println!("{}", json!({"event":"key_down_sent","code":30}));
    }
    if args.iter().any(|a| a == "--release-pending") {
        tokio::time::sleep(Duration::from_millis(150)).await;
        wire::write_json(
            &mut input,
            &json!({"t":"input","from":1,"events":[{"release_all":true}]}),
        )
        .await?;
        println!("{}", json!({"event":"release_all_sent"}));
    }
    let (tx, mut ctl) = tokio::sync::mpsc::channel(32);
    let reader = tokio::spawn(async move {
        loop {
            let v = wire::read_json(&mut recv).await;
            if tx.send(v).await.is_err() {
                break;
            }
        }
    });
    let (vtx, mut video) = tokio::sync::mpsc::channel(2);
    let c = conn.clone();
    let video_reader = tokio::spawn(async move {
        loop {
            let mut stream = c.accept_uni().await?;
            let result = async {
                let mut head = [0; 24];
                stream.read_exact(&mut head).await?;
                let header = VideoHeader::decode(&head)?;
                let size = stream.read_u32().await? as usize;
                ensure!(size > 0 && size <= wire::MAX_VIDEO, "frame size");
                let mut bytes = vec![0; size];
                stream.read_exact(&mut bytes).await?;
                Ok::<_, anyhow::Error>((header, bytes))
            }
            .await;
            if let Ok(frame) = result {
                vtx.send(frame).await?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let mut decoder = codec::Decoder::new()?;
    let color_check = args.iter().any(|a| a == "--color-check");
    let pixel_check = args.iter().any(|a| a == "--pixels");
    let mut colors_verified = false;
    let pixel_display = if pixel_check || color_check {
        Some(libva::Display::open().context("sample VA display")?)
    } else {
        None
    };
    let mut previous: Option<Vec<u8>> = None;
    let mut gate = VideoGate::default();
    let mut ping = tokio::time::interval(Duration::from_millis(500));
    let mut frames = 0u64;
    let mut pongs = 0u64;
    let mut last_pong = Instant::now();
    let mut max_pong_gap = 0u128;
    let duration = args
        .windows(2)
        .find(|a| a[0] == "--seconds")
        .map(|a| a[1].parse::<u64>())
        .transpose()?
        .unwrap_or(10);
    let deadline = tokio::time::sleep(Duration::from_secs(duration));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
                      _=&mut deadline=>break,
                      _=ping.tick()=>wire::write_json(&mut send,&json!({"t":"ping"})).await?,
                      m=ctl.recv()=>{let m=m.context("control closed")??;if m["t"]=="pong"{pongs+=1;max_pong_gap=max_pong_gap.max(last_pong.elapsed().as_millis());last_pong=Instant::now();if pongs%20==0{println!("{}",json!({"event":"heartbeat","elapsed_us":start.elapsed().as_micros(),"frames":frames,"pongs":pongs,"max_pong_gap_ms":max_pong_gap}));}}else{println!("{}",json!({"event":"control","message":m}));}},
                      frame=video.recv()=>{let(h,bytes)=frame.context("video closed")?;if !gate.accept(h.sequence,h.keyframe){wire::write_json(&mut send,&json!({"t":"keyframe"})).await?;continue;}
                      match decoder.decode(&bytes){Ok(Some(frame))=>{let decoded_us=ibara_screen::now_us();frames+=1;
               if color_check{let samples=sample_colors(&frame,pixel_display.as_ref().unwrap())?;colors_verified=samples[0][2]>200&&samples[0][1]<140&&samples[1][1]>200&&samples[1][2]<140;println!("{}",json!({"event":"color_sample","samples":samples,"correct_orientation":colors_verified,"width":frame.width,"height":frame.height}));}
        if pixel_check{let display=pixel_display.as_ref().unwrap();let pixels=sample_notify(&frame,display)?;if let Some(old)=&previous{let changed=old.iter().zip(&pixels).filter(|(a,b)|a.abs_diff(**b)>20).count();if changed>=2{println!("{}",json!({"event":"pixel_change","decoded_us":decoded_us,"blocks":changed,"sequence":h.sequence}));}}previous=Some(pixels);}
               println!("{}",json!({"event":"decoded","sequence":h.sequence,"keyframe":h.keyframe,"capture_us":h.capture_us,"decoded_us":decoded_us,"elapsed_us":start.elapsed().as_micros()}));},Ok(None)=>{},Err(e)=>{println!("{}",json!({"event":"decode_error","reason":e.to_string()}));decoder=codec::Decoder::new()?;wire::write_json(&mut send,&json!({"t":"keyframe"})).await?;}}
                      },_=conn.closed()=>break}
    }
    let close_reason = conn.close_reason().map(|reason| reason.to_string());
    reader.abort();
    video_reader.abort();
    conn.close(0u32.into(), b"proof complete");
    println!(
        "{}",
        json!({"event":"complete","frames":frames,"pongs":pongs,"max_pong_gap_ms":max_pong_gap,"elapsed_us":start.elapsed().as_micros(),"close_reason":close_reason})
    );
    ensure!(frames > 0, "no decoded frames");
    ensure!(start.elapsed() >= Duration::from_secs(duration), "peer ended before its deadline: {close_reason:?}");
    ensure!(
        !color_check || colors_verified,
        "decoded colors or orientation are wrong"
    );
    Ok(())
}
