//! Capped OpenH264 fallback, using linear dmabufs when VA encode is unavailable.
use crate::codec::DmaFrame;
use anyhow::{Context, Result, ensure};
use libva::{
    DrmPrimeSurfaceDescriptor, DrmPrimeSurfaceDescriptorLayer, DrmPrimeSurfaceDescriptorObject,
};
use std::{fs::File, os::fd::AsRawFd};
pub struct Allocator {
    device: gbm::Device<File>,
    width: u32,
    height: u32,
    nvidia: bool,
}
impl Allocator {
    pub fn new_on(w: u32, h: u32, node: &str) -> Result<Self> {
        Ok(Self {
            device: gbm::Device::new(
                File::options()
                    .read(true)
                    .write(true)
                    .open(node)?,
            )?,
            width: w,
            height: h,
            nvidia: std::fs::read_to_string(format!("/sys/class/drm/{}/device/vendor", node.rsplit('/').next().unwrap_or(""))).unwrap_or_default().trim() == "0x10de",
        })
    }
    pub fn allocate(&self) -> Result<DmaFrame> {
        let bo = if self.nvidia { self.device.create_buffer_object::<()>(self.width, self.height, gbm::Format::Argb8888, gbm::BufferObjectFlags::RENDERING)? } else { self
            .device
            .create_buffer_object_with_modifiers2::<()>(
                self.width,
                self.height,
                gbm::Format::Argb8888,
                std::iter::once(gbm::Modifier::Linear),
                gbm::BufferObjectFlags::RENDERING,
            )
            .context("allocate explicit linear capture buffer")? };
        let stride = bo.stride().context("capture stride")?;
        let offset = bo.offset(0).context("capture offset")?;
        let modifier: u64 = bo.modifier()?.into();
        ensure!(self.nvidia || modifier == 0, "software capture requires a linear dmabuf");
        let fd = bo.fd().context("capture dmabuf export")?;
        Ok(DmaFrame {
            y_inverted: false,
            width: self.width,
            height: self.height,
            descriptor: DrmPrimeSurfaceDescriptor {
                fourcc: u32::from_le_bytes(*b"AR24"),
                width: self.width,
                height: self.height,
                objects: vec![DrmPrimeSurfaceDescriptorObject {
                    fd,
                    size: stride * self.height,
                    drm_format_modifier: modifier,
                }],
                layers: vec![DrmPrimeSurfaceDescriptorLayer {
                    drm_format: u32::from_le_bytes(*b"AR24"),
                    num_planes: 1,
                    object_index: [0; 4],
                    offset: [offset, 0, 0, 0],
                    pitch: [stride, 0, 0, 0],
                }],
            },
        })
    }
}
pub struct Encoder {
    encoder: openh264::encoder::Encoder,
    vpp: Option<crate::vpp::Vpp>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    bitrate: u64,
    reset: bool,
}
fn create(b: u64, fps: u32) -> Result<openh264::encoder::Encoder> {
    use openh264::encoder::*;
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(b as u32))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate);
    Ok(openh264::encoder::Encoder::with_api_config(
        openh264::OpenH264API::from_source(),
        config,
    )?)
}
impl Encoder {
    pub fn new(w: u32, h: u32, fps: u32) -> Result<Self> {
        let scale = (1280f64 / w as f64).min(720f64 / h as f64).min(1.0);
        let width = ((w as f64 * scale) as u32) & !1;
        let height = ((h as f64 * scale) as u32) & !1;
        ensure!(width >= 2 && height >= 2, "invalid software geometry");
        let fps = fps.min(15);
        let bitrate = 3_000_000;
        Ok(Self {
            encoder: create(bitrate, fps)?,
            vpp: crate::vpp::Vpp::new(width, height).ok(),
            width,
            height,
            fps,
            bitrate,
            reset: false,
        })
    }
    pub fn bitrate(&mut self, b: u64) -> Result<()> {
        let b = b.min(3_000_000);
        if b != self.bitrate {
            self.encoder = create(b, self.fps)?;
            self.bitrate = b;
            self.reset = true;
        }
        Ok(())
    }
    pub fn encode(&mut self, src: &DmaFrame, keyframe: bool) -> Result<Vec<(bool, Vec<u8>)>> {
        if let Some(vpp) = &mut self.vpp {
            let pixels = vpp.read_yuv(src)?;
            let yuv = openh264::formats::YUVBuffer::from_vec(pixels, self.width as usize, self.height as usize);
            return self.encode_yuv(&yuv, keyframe);
        }
        ensure!(
            src.descriptor.layers.len() == 1
                && src.descriptor.layers[0].num_planes == 1
                && src.descriptor.objects.len() == 1,
            "software capture requires packed RGB"
        );
        let l = &src.descriptor.layers[0];
        let o = &src.descriptor.objects[0];
        ensure!(
            o.drm_format_modifier == 0,
            "software capture requires linear pixels"
        );
        let len = o.size as usize;
        ensure!(
            len >= l.offset[0] as usize + l.pitch[0] as usize * src.height as usize,
            "short capture buffer"
        );
        let mapping = Mapping::new(o.fd.as_raw_fd(), len)?;
        let input = unsafe { std::slice::from_raw_parts(mapping.ptr.cast::<u8>(), len) };
        let mut rgba = vec![0u8; self.width as usize * self.height as usize * 4];
        for y in 0..self.height as usize {
            let sy = y * src.height as usize / self.height as usize;
            let sy = if src.y_inverted {
                src.height as usize - 1 - sy
            } else {
                sy
            };
            for x in 0..self.width as usize {
                let sx = x * src.width as usize / self.width as usize;
                let i = l.offset[0] as usize + sy * l.pitch[0] as usize + sx * 4;
                let j = (y * self.width as usize + x) * 4;
                rgba[j..j + 4].copy_from_slice(&[input[i + 2], input[i + 1], input[i], 255]);
            }
        }
        drop(mapping);
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(
            openh264::formats::RgbaSliceU8::new(&rgba, (self.width as usize, self.height as usize)),
        );
        self.encode_yuv(&yuv, keyframe)
    }
    pub fn encode_rgba(&mut self, pixels:&[u8], width:u32, height:u32, keyframe:bool)->Result<Vec<(bool,Vec<u8>)>> {
        ensure!(width>=2 && height>=2 && width<=4096 && height<=4096 &&
            pixels.len()==width as usize*height as usize*4,"Invalid CPU capture pixels");
        let mut rgba=vec![0u8;self.width as usize*self.height as usize*4];
        for y in 0..self.height as usize {for x in 0..self.width as usize {
            let src=((y*height as usize/self.height as usize)*width as usize+x*width as usize/self.width as usize)*4;
            let dst=(y*self.width as usize+x)*4;
            rgba[dst..dst+4].copy_from_slice(&pixels[src..src+4]);
        }}
        let yuv=openh264::formats::YUVBuffer::from_rgb_source(
            openh264::formats::RgbaSliceU8::new(&rgba,(self.width as usize,self.height as usize)));
        self.encode_yuv(&yuv,keyframe)
    }
    fn encode_yuv(&mut self, yuv: &openh264::formats::YUVBuffer, keyframe: bool) -> Result<Vec<(bool, Vec<u8>)>> {
        if keyframe || self.reset {
            self.encoder.force_intra_frame();
            self.reset = false;
        }
        let out = self.encoder.encode(yuv)?.to_vec();
        if out.is_empty() {
            return Ok(vec![]);
        }
        let key = out
            .windows(4)
            .any(|n| n[..3] == [0, 0, 1] && n[3] & 31 == 5);
        Ok(vec![(key, out)])
    }
}
pub(crate) struct Mapping {
    pub(crate) ptr: *mut libc::c_void,
    len: usize,
    fd: i32,
}
impl Mapping {
    pub(crate) fn new(fd: i32, len: usize) -> Result<Self> {
        unsafe {
            let start: u64 = 1;
            ensure!(
                libc::ioctl(fd, 0x40086200u64, &start) >= 0,
                "DMA read synchronization failed"
            );
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                let end: u64 = 5;
                libc::ioctl(fd, 0x40086200u64, &end);
                return Err(std::io::Error::last_os_error()).context("map captured dmabuf");
            }
            Ok(Self { ptr, len, fd })
        }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
            let end: u64 = 5;
            libc::ioctl(self.fd, 0x40086200u64, &end);
        }
    }
}
