use anyhow::{Context, Result, ensure};
use cros_codecs::{
    BlockingMode, Fourcc, FrameLayout, PlaneLayout, Resolution,
    decoder::{
        DecodedHandle, DecoderEvent,
        stateless::{DecodeError, StatelessVideoDecoder},
    },
    encoder::{FrameMetadata, RateControl, Tunings, VideoEncoder},
    video_frame::generic_dma_video_frame::GenericDmaVideoFrame,
};
use libva::{
    Display, DrmPrimeSurfaceDescriptor, DrmPrimeSurfaceDescriptorLayer,
    DrmPrimeSurfaceDescriptorObject,
};
use std::{
    fs::File,
    os::fd::{FromRawFd, OwnedFd},
    rc::Rc,
    sync::Arc,
};
/// Owns every exported fd; no decoded or captured pixels pass through CPU memory.
pub struct DmaFrame {
    pub y_inverted: bool,
    pub descriptor: DrmPrimeSurfaceDescriptor,
    pub width: u32,
    pub height: u32,
}
impl DmaFrame {
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            y_inverted: self.y_inverted,
            width: self.width,
            height: self.height,
            descriptor: DrmPrimeSurfaceDescriptor {
                fourcc: self.descriptor.fourcc,
                width: self.descriptor.width,
                height: self.descriptor.height,
                objects: self
                    .descriptor
                    .objects
                    .iter()
                    .map(|o| {
                        Ok(DrmPrimeSurfaceDescriptorObject {
                            fd: o.fd.try_clone()?,
                            size: o.size,
                            drm_format_modifier: o.drm_format_modifier,
                        })
                    })
                    .collect::<Result<_>>()?,
                layers: self
                    .descriptor
                    .layers
                    .iter()
                    .map(|l| DrmPrimeSurfaceDescriptorLayer {
                        drm_format: l.drm_format,
                        num_planes: l.num_planes,
                        object_index: l.object_index,
                        offset: l.offset,
                        pitch: l.pitch,
                    })
                    .collect(),
            },
        })
    }
    pub unsafe fn from_raw(d: libva::VADRMPRIMESurfaceDescriptor, w: u32, h: u32) -> Result<Self> {
        ensure!(
            d.num_objects <= 4 && d.num_layers <= 4,
            "invalid VA export descriptor"
        );
        let objects = d.objects[..d.num_objects as usize]
            .iter()
            .map(|o| DrmPrimeSurfaceDescriptorObject {
                fd: unsafe { OwnedFd::from_raw_fd(o.fd) },
                size: o.size,
                drm_format_modifier: o.drm_format_modifier,
            })
            .collect();
        let layers = d.layers[..d.num_layers as usize]
            .iter()
            .map(|l| DrmPrimeSurfaceDescriptorLayer {
                drm_format: l.drm_format,
                num_planes: l.num_planes,
                object_index: l.object_index.map(|n| n as u8),
                offset: l.offset,
                pitch: l.pitch,
            })
            .collect();
        Ok(Self {
            y_inverted: false,
            descriptor: DrmPrimeSurfaceDescriptor {
                fourcc: d.fourcc,
                width: d.width,
                height: d.height,
                objects,
                layers,
            },
            width: w,
            height: h,
        })
    }
    pub fn generic(&self) -> Result<(GenericDmaVideoFrame, FrameLayout)> {
        ensure!(!self.descriptor.objects.is_empty(), "empty dmabuf");
        let mut planes = vec![];
        for l in &self.descriptor.layers {
            ensure!(l.num_planes <= 4, "invalid plane count");
            for i in 0..l.num_planes as usize {
                planes.push(PlaneLayout {
                    buffer_index: l.object_index[i] as usize,
                    offset: l.offset[i] as usize,
                    stride: l.pitch[i] as usize,
                });
            }
        }
        let layout = FrameLayout {
            format: (
                Fourcc::from(b"NV12"),
                self.descriptor.objects[0].drm_format_modifier,
            ),
            size: Resolution {
                width: self.descriptor.width,
                height: self.descriptor.height,
            },
            planes,
        };
        let files = self
            .descriptor
            .objects
            .iter()
            .map(|o| o.fd.try_clone().map(File::from))
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok((
            GenericDmaVideoFrame::new(files, layout.clone()).map_err(anyhow::Error::msg)?,
            layout,
        ))
    }
}
fn allocate(d: &Rc<Display>, w: u32, h: u32, rgb: bool) -> Result<DmaFrame> {
    let surface = d
        .create_surfaces(
            if rgb {
                libva::VA_RT_FORMAT_RGB32
            } else {
                libva::VA_RT_FORMAT_YUV420
            },
            Some(if rgb {
                libva::VA_FOURCC_BGRA
            } else {
                libva::VA_FOURCC_NV12
            }),
            w,
            h,
            None,
            vec![()],
        )?
        .pop()
        .context("VA surface allocation")?;
    Ok(DmaFrame {
        y_inverted: false,
        descriptor: surface.export_prime()?,
        width: w,
        height: h,
    })
}
#[derive(Clone)]
pub struct Hardware {
    display: Rc<Display>,
    width: u32,
    height: u32,
}
impl Hardware {
    pub fn allocate(&self) -> Result<DmaFrame> {
        allocate(&self.display, self.width, self.height, true)
    }
}
type VaEncoder = cros_codecs::encoder::stateless::h264::StatelessEncoder<
    Rc<libva::Surface<()>>,
    cros_codecs::backend::vaapi::encoder::VaapiBackend<
        (),
        Rc<libva::Surface<()>>,
    >,
>;
pub struct HardwareEncoder {
    pub hw: Hardware,
    vpp: crate::vpp::Vpp,
    encoder: VaEncoder,
    tunings: Tunings,
}
impl HardwareEncoder {
    pub fn new(w: u32, h: u32, fps: u32, node: &str) -> Result<Self> {
        let display = Display::open_drm_display(node).ok().context("VA display unavailable")?;
        let size = Resolution {
            width: w,
            height: h,
        };
        let tunings = Tunings {
            rate_control: RateControl::ConstantBitrate(10_000_000),
            framerate: fps,
            ..Default::default()
        };
        let cfg = cros_codecs::encoder::h264::EncoderConfig {
            resolution: size,
            profile: cros_codecs::codec::h264::parser::Profile::Main,
            initial_tunings: tunings.clone(),
            ..Default::default()
        };
        let backend = cros_codecs::backend::vaapi::encoder::VaapiBackend::new(
            display.clone(), libva::VAProfile::VAProfileH264Main,
            Fourcc::from(b"NV12"), size, libva::VA_RC_CBR, false,
        ).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let encoder = VaEncoder::new_h264(backend, cfg, BlockingMode::Blocking)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Self {
            hw: Hardware {
                display: display.clone(),
                width: w,
                height: h,
            },
            vpp: crate::vpp::Vpp::new_on(display, w, h)?,
            encoder,
            tunings,
        })
    }
    pub fn bitrate(&mut self, b: u64) -> Result<()> {
        let rate = RateControl::ConstantBitrate(b);
        if self.tunings.rate_control != rate {
            self.tunings.rate_control = rate;
            self.encoder
                .tune(self.tunings.clone())
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        Ok(())
    }
    pub fn encode(
        &mut self,
        src: &DmaFrame,
        capture_us: u64,
        keyframe: bool,
    ) -> Result<Vec<(bool, Vec<u8>)>> {
        let converted = self.vpp.convert(src)?;
        ibara_screen::trace("vpp_done", capture_us);
        let layout = FrameLayout {
            format: (Fourcc::from(b"NV12"), 0),
            size: Resolution { width: self.hw.width, height: self.hw.height },
            planes: vec![], // native VA surfaces need no external plane descriptors
        };
        self.encoder
            .encode(
                FrameMetadata {
                    timestamp: capture_us,
                    layout,
                    force_keyframe: keyframe,
                },
                converted,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut result = vec![];
        while let Some(out) = self.encoder.poll().map_err(|e| anyhow::anyhow!("{e:?}"))? {
            let key = cros_codecs::bitstream_utils::NalIterator::<
                cros_codecs::codec::h264::parser::Nalu,
            >::new(&out.bitstream)
            .any(|n| n.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 31 == 5));
            result.push((key, out.bitstream));
        }
        ibara_screen::trace("encode_done", capture_us);
        Ok(result)
    }
}
type VaDecoder = cros_codecs::decoder::stateless::StatelessDecoder<
    cros_codecs::decoder::stateless::h264::H264,
    cros_codecs::backend::vaapi::decoder::VaapiBackend<GenericDmaVideoFrame>,
>;
pub struct Decoder {
    decoder: VaDecoder,
    display: Rc<Display>,
    size: Resolution,
    visible: Resolution,
}
pub struct Decoded {
    pub frame: Arc<GenericDmaVideoFrame>,
    pub width: u32,
    pub height: u32,
}
impl Decoded {
    pub fn dma(&self) -> Result<DmaFrame> {
        use libva::ExternalBufferDescriptor;
        let mut f = (*self.frame).clone();
        let d = f.va_surface_attribute();
        let mut raw = d;
        for o in &mut raw.objects[..raw.num_objects as usize] {
            let fd = unsafe { libc::fcntl(o.fd, libc::F_DUPFD_CLOEXEC, 0) };
            ensure!(fd >= 0, "dmabuf fd clone failed");
            o.fd = fd;
        }
        unsafe { DmaFrame::from_raw(raw, self.width, self.height) }
    }
}
impl Decoder {
    pub fn new() -> Result<Self> {
        let display = Display::open().context("VA display unavailable")?;
        let decoder = VaDecoder::new_vaapi(display.clone(), BlockingMode::Blocking)?;
        Ok(Self {
            decoder,
            display,
            size: Resolution {
                width: 1920,
                height: 1088,
            },
            visible: Resolution {
                width: 1920,
                height: 1080,
            },
        })
    }
    fn events(&mut self, out: &mut Option<Decoded>) -> Result<()> {
        while let Some(e) = self.decoder.next_event() {
            match e {
                DecoderEvent::FormatChanged => {
                    let info = self.decoder.stream_info().context("no stream info")?;
                    self.size = info.coded_resolution;
                    self.visible = info.display_resolution;
                }
                DecoderEvent::FrameReady(h) => {
                    h.sync()?;
                    *out = Some(Decoded {
                        frame: h.video_frame(),
                        width: h.display_resolution().width,
                        height: h.display_resolution().height,
                    });
                }
            }
        }
        Ok(())
    }
    pub fn decode(&mut self, bytes: &[u8]) -> Result<Option<Decoded>> {
        let mut out = None;
        for nal in
            cros_codecs::bitstream_utils::NalIterator::<cros_codecs::codec::h264::parser::Nalu>::new(
                bytes,
            )
        {
            let mut offset = 0;
            let mut attempts = 0;
            while offset < nal.len() {
                attempts += 1;
                ensure!(attempts < 16, "decoder made no progress");
                let display = self.display.clone();
                let size = self.size;
                let mut failure = None;
                let result = self
                    .decoder
                    .decode(0, &nal[offset..], &mut || match allocate(
                        &display,
                        size.width,
                        size.height,
                        false,
                    )
                    .and_then(|f| f.generic().map(|p| p.0))
                    {
                        Ok(f) => Some(f),
                        Err(e) => {
                            failure = Some(e);
                            None
                        }
                    });
                if let Some(e) = failure {
                    return Err(e);
                }
                match result {
                    Ok(n) => {
                        ensure!(n > 0, "decoder consumed no bytes");
                        offset += n;
                    }
                    Err(DecodeError::CheckEvents) => {}
                    Err(e) => return Err(e.into()),
                }
                self.events(&mut out)?;
            }
        }
        self.decoder.end_access_unit()?;
        self.events(&mut out)?;
        Ok(out)
    }
}

pub enum Allocator {
    Va(Hardware),
    Software(crate::software::Allocator),
}
impl Allocator {
    pub fn allocate(&self) -> Result<DmaFrame> {
        match self {
            Self::Va(a) => a.allocate(),
            Self::Software(a) => a.allocate(),
        }
    }
}
enum Backend {
    Va(HardwareEncoder),
    Nv(crate::nvenc::Encoder),
    Software(crate::software::Encoder),
}
pub struct Encoder {
    pub hw: Allocator,
    backend: Backend,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub name: &'static str,
}
impl Encoder {
    pub fn new(w: u32, h: u32, fps: u32, node: &str) -> Result<Self> {
        ensure!(
            w >= 2 && h >= 2 && w <= 16384 && h <= 16384,
            "invalid source geometry"
        );
        let scale = (1920f64 / w as f64).min(1080f64 / h as f64).min(1.0);
        let output_w = ((w as f64 * scale) as u32) & !1;
        let output_h = ((h as f64 * scale) as u32) & !1;
        let vendor =
            std::fs::read_to_string(format!("/sys/class/drm/{}/device/vendor", node.rsplit('/').next().unwrap_or(""))).unwrap_or_default();
        // The current upstream VA backend lacks the packed slice headers required
        // by radeonsi. Its real output has reserved NAL type 0, so use OpenH264.
        let hardware_usable = !matches!(vendor.trim(), "0x1002" | "0x10de");
        if std::env::var_os("IBARA_SCREEN_SOFTWARE").is_none() && hardware_usable && std::env::var("IBARA_SCREEN_ENCODER").as_deref() != Ok("nvenc") {
            match HardwareEncoder::new(output_w, output_h, fps, node) {
                Ok(encoder) => {
                    return Ok(Self {
                        hw: Allocator::Va(Hardware {
                            display: encoder.hw.display.clone(),
                            width: w,
                            height: h,
                        }),
                        backend: Backend::Va(encoder),
                        width: output_w,
                        height: output_h,
                        fps,
                        name: "h264_vaapi",
                    });
                }
                Err(e) => eprintln!("VA encode unavailable, trying OpenH264: {e:#}"),
            }
        }
        if std::env::var_os("IBARA_SCREEN_SOFTWARE").is_none() {
            match crate::nvenc::Encoder::new(output_w, output_h, fps, node) {
                Ok(encoder) => return Ok(Self {
                    hw: Allocator::Software(crate::software::Allocator::new_on(w, h, node)?),
                    backend: Backend::Nv(encoder), width: output_w, height: output_h,
                    fps, name: "nvenc",
                }),
                Err(e) => eprintln!("NVENC unavailable, trying OpenH264: {e:#}"),
            }
        }
        let encoder = crate::software::Encoder::new(w, h, fps)?;
        let (width, height, fps) = (encoder.width, encoder.height, encoder.fps);
        Ok(Self {
            hw: Allocator::Software(crate::software::Allocator::new_on(w, h, node)?),
            backend: Backend::Software(encoder),
            width,
            height,
            fps,
            name: "openh264",
        })
    }
    pub fn bitrate(&mut self, b: u64) -> Result<()> {
        match &mut self.backend {
            Backend::Va(e) => e.bitrate(b),
            Backend::Nv(e) => e.bitrate(b),
            Backend::Software(e) => e.bitrate(b),
        }
    }
    pub fn encode(&mut self, src: &DmaFrame, us: u64, key: bool) -> Result<Vec<(bool, Vec<u8>)>> {
        match &mut self.backend {
            Backend::Va(e) => e.encode(src, us, key),
            Backend::Nv(e) => e.encode(src, us, key),
            Backend::Software(e) => e.encode(src, key),
        }
    }
}
