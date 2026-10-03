//! RGB dmabuf import and GPU color conversion through libva's public raw bindings.
use crate::codec::DmaFrame;
use anyhow::{Context, Result, ensure};
use libva::*;
use std::{os::fd::AsRawFd, ptr::null_mut, rc::Rc};
fn check(s: VAStatus) -> Result<()> {
    ensure!(
        s == VA_STATUS_SUCCESS as i32,
        "VA video processing status {s}"
    );
    Ok(())
}
pub struct Vpp {
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    owner: Rc<Display>,
    width: u32,
    height: u32,
    rgb: Vec<((u64, u64), SurfaceGuard)>,
    output: Option<Rc<Surface<()>>>,
}
impl Vpp {
    pub fn new(w: u32, h: u32) -> Result<Self> {
        Self::new_on(Display::open().context("VA display unavailable")?, w, h)
    }
    pub fn new_on(owner: Rc<Display>, w: u32, h: u32) -> Result<Self> {
        unsafe {
            let display = owner.handle();
            let mut this = Self {
                display,
                config: VA_INVALID_ID,
                context: VA_INVALID_ID,
                owner,
                width: w,
                height: h,
                rgb: Vec::new(),
                output: None,
            };
            check(vaCreateConfig(
                display,
                VAProfile::VAProfileNone,
                VAEntrypoint::VAEntrypointVideoProc,
                null_mut(),
                0,
                &mut this.config,
            ))?;
            check(vaCreateContext(
                display,
                this.config,
                w as i32,
                h as i32,
                VA_PROGRESSIVE as i32,
                null_mut(),
                0,
                &mut this.context,
            ))?;
            Ok(this)
        }
    }
    fn process(&mut self, src: &DmaFrame, sync: bool) -> Result<VASurfaceID> {
        unsafe {
            ensure!(
                src.descriptor.layers.len() == 1,
                "RGB input must have one layer"
            );
            let format = src.descriptor.layers[0].drm_format;
            ensure!(
                format == u32::from_le_bytes(*b"AR24") || format == u32::from_le_bytes(*b"XR24"),
                "unsupported captured RGB format"
            );
            let mut desc = VADRMPRIMESurfaceDescriptor::default();
            desc.fourcc = if format == u32::from_le_bytes(*b"AR24") {
                VA_FOURCC_BGRA
            } else {
                VA_FOURCC_BGRX
            };
            desc.width = src.width;
            desc.height = src.height;
            desc.num_objects = src.descriptor.objects.len() as u32;
            desc.num_layers = 1;
            for (i, o) in src.descriptor.objects.iter().enumerate() {
                ensure!(i < 4, "too many objects");
                desc.objects[i] = VADRMPRIMESurfaceDescriptorObject {
                    fd: o.fd.as_raw_fd(),
                    size: o.size,
                    drm_format_modifier: o.drm_format_modifier,
                };
            }
            let l = &src.descriptor.layers[0];
            desc.layers[0].drm_format = l.drm_format;
            desc.layers[0].num_planes = l.num_planes;
            desc.layers[0].pitch = l.pitch;
            desc.layers[0].offset = l.offset;
            desc.layers[0].object_index = l.object_index.map(u32::from);
            let mut attrs = [
                VASurfaceAttrib::new_memory_type(MemoryType::DrmPrime2),
                VASurfaceAttrib::new_buffer_descriptor(&mut desc),
            ];
            // The capture pool has two persistent dmabufs. Keep their VA
            // imports and the synchronized conversion surface across frames.
            let mut stat: libc::stat = std::mem::zeroed();
            ensure!(libc::fstat(src.descriptor.objects[0].fd.as_raw_fd(), &mut stat) == 0,
                "capture dmabuf identity unavailable");
            let key = (stat.st_dev as u64, stat.st_ino as u64);
            let rgb = if let Some((_, surface)) = self.rgb.iter().find(|(k, _)| *k == key) {
                surface.id
            } else {
                let mut surface = SurfaceGuard { display: self.display, id: VA_INVALID_SURFACE };
                check(vaCreateSurfaces(self.display, VA_RT_FORMAT_RGB32,
                    src.width, src.height, &mut surface.id, 1, attrs.as_mut_ptr(), 2))?;
                let id = surface.id;
                if self.rgb.len() == 2 { self.rgb.clear(); }
                self.rgb.push((key, surface));
                id
            };
            if self.output.is_none() {
                let output = self.owner.create_surfaces(VA_RT_FORMAT_YUV420,
                    Some(VA_FOURCC_NV12), self.width, self.height, None, vec![()])?
                    .pop().context("VPP output unavailable")?;
                self.output = Some(Rc::new(output));
            }
            let nv12 = self.output.as_ref().unwrap().id();
            unsafe extern "C" {
                fn screen_vpp_parameters(
                    display: VADisplay,
                    context: VAContextID,
                    rgb: VASurfaceID,
                    mirror_vertical: u32,
                    buffer: *mut VABufferID,
                ) -> VAStatus;
            }
            let mut buffer = BufferGuard {
                display: self.display,
                id: VA_INVALID_ID,
            };
            check(screen_vpp_parameters(
                self.display,
                self.context,
                rgb,
                u32::from(src.y_inverted),
                &mut buffer.id,
            ))?;
            check(vaBeginPicture(self.display, self.context, nv12))?;
            check(vaRenderPicture(
                self.display,
                self.context,
                &mut buffer.id,
                1,
            ))?;
            check(vaEndPicture(self.display, self.context))?;
            if sync { check(vaSyncSurface(self.display, nv12))?; }
            Ok(nv12)
        }
    }
    pub fn read_yuv(&mut self, src: &DmaFrame) -> Result<Vec<u8>> {
        let surface = self.process(src, true)?;
        let mut pixels = vec![0; (self.width * self.height * 3 / 2) as usize];
        unsafe extern "C" {
            fn screen_vpp_read_yuv(display: VADisplay, surface: VASurfaceID,
                width: u32, height: u32, pixels: *mut u8) -> VAStatus;
        }
        unsafe { check(screen_vpp_read_yuv(self.display, surface,
            self.width, self.height, pixels.as_mut_ptr()))?; }
        Ok(pixels)
    }
    pub fn convert(&mut self, src: &DmaFrame) -> Result<Rc<Surface<()>>> {
        // A native surface on the encoder's display carries the GPU dependency.
        // Only readback/diagnostic completion receipts need a CPU sync here.
        self.process(src, ibara_screen::trace_enabled())?;
        Ok(self.output.as_ref().unwrap().clone())
    }

}
struct SurfaceGuard {
    display: VADisplay,
    id: VASurfaceID,
}
impl Drop for SurfaceGuard {
    fn drop(&mut self) {
        if self.id != VA_INVALID_SURFACE {
            unsafe {
                vaDestroySurfaces(self.display, &mut self.id, 1);
            }
        }
    }
}
struct BufferGuard {
    display: VADisplay,
    id: VABufferID,
}
impl Drop for BufferGuard {
    fn drop(&mut self) {
        if self.id != VA_INVALID_ID {
            unsafe {
                vaDestroyBuffer(self.display, self.id);
            }
        }
    }
}
impl Drop for Vpp {
    fn drop(&mut self) {
        self.rgb.clear();
        self.output.take();
        unsafe {
            if self.context != VA_INVALID_ID {
                vaDestroyContext(self.display, self.context);
            }
            if self.config != VA_INVALID_ID {
                vaDestroyConfig(self.display, self.config);
            }

        }
    }
}
