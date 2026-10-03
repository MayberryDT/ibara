//! Real VA VPP/encode/decode geometry proof; no generated picture is retained.
#[path = "../src/codec.rs"]
mod codec;
#[path = "../src/nvenc.rs"]
mod nvenc;
#[path = "../src/nvenc_egl.rs"]
mod nvenc_egl;
#[path = "../src/software.rs"]
mod software;
#[path = "../src/vpp.rs"]
mod vpp;
use anyhow::{Result, ensure};
fn main() -> Result<()> {
    let mut encoder = codec::Encoder::new(2560, 1440, 30, "/dev/dri/renderD128")?;
    ensure!(
        encoder.name == "h264_vaapi",
        "hardware encode required for VPP proof"
    );
    let source = encoder.hw.allocate()?;
    let mut decoder = codec::Decoder::new()?;
    let mut decoded = None;
    let mut access_units = 0;
    for index in 0..8 {
        let units = encoder.encode(&source, ibara_screen::now_us(), index == 0)?;
        access_units += units.len();
        for (_, unit) in units {
            let nals: Vec<_> = cros_codecs::bitstream_utils::NalIterator::<
                cros_codecs::codec::h264::parser::Nalu,
            >::new(&unit)
            .map(|n| {
                n.iter()
                    .position(|b| *b == 1)
                    .and_then(|i| n.get(i + 1))
                    .map(|b| b & 31)
            })
            .collect();
            eprintln!("unit bytes={} nal_types={nals:?}", unit.len());
            decoded = decoder.decode(&unit)?.or(decoded);
        }
    }
    eprintln!("encoded access units: {access_units}");
    let decoded = decoded.ok_or_else(|| anyhow::anyhow!("no decoded output"))?;
    ensure!(
        (decoded.width, decoded.height) == (1920, 1080),
        "incorrect scaled dimensions"
    );
    println!(
        "{}",
        serde_json::json!({"source":[2560,1440],"encoded":[encoder.width,encoder.height],"decoded":[decoded.width,decoded.height],"encoder":encoder.name})
    );
    Ok(())
}
