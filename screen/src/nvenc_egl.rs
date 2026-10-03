//! CUDA/EGL interop for NVIDIA-owned capture buffers. No CUDA link dependency.
use crate::codec::DmaFrame;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{c_char, c_void},
    os::fd::AsRawFd,
    ptr::null_mut,
    sync::Arc,
};
#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
    fn eglInitialize(display: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglTerminate(display: *mut c_void) -> u32;
}
// CUDA's CUeglFrame ABI: three array/pitch pointers followed by nine 32-bit fields.
#[repr(C)]
#[derive(Default)]
struct EglFrame {
    planes: [*mut c_void; 3],
    width: u32,
    height: u32,
    depth: u32,
    pitch: u32,
    plane_count: u32,
    channels: u32,
    frame_type: u32,
    color: u32,
    format: u32,
}
type Register = unsafe extern "C" fn(*mut *mut c_void, *mut c_void, u32) -> i32;
type Mapped = unsafe extern "C" fn(*mut EglFrame, *mut c_void, u32, u32) -> i32;
type Unregister = unsafe extern "C" fn(*mut c_void) -> i32;
type Create =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type Destroy = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
pub struct Display {
    display: *mut c_void,
    create: Create,
    destroy: Destroy,
    register: Register,
    mapped: Mapped,
    unregister: Unregister,
    _cuda: libloading::Library,
}
impl Display {
    pub fn new(node: &str) -> Result<Arc<Self>> {
        unsafe {
            type Query = unsafe extern "C" fn(i32, *mut *mut c_void, *mut i32) -> u32;
            type StringQuery = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
            type Platform = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> *mut c_void;
            macro_rules! egl {
                ($name:literal, $t:ty) => {{
                    let p = eglGetProcAddress(concat!($name, "\0").as_ptr().cast());
                    ensure!(!p.is_null(), concat!($name, " unavailable"));
                    std::mem::transmute::<*const c_void, $t>(p)
                }};
            }
            let query = egl!("eglQueryDevicesEXT", Query);
            let string = egl!("eglQueryDeviceStringEXT", StringQuery);
            let platform = egl!("eglGetPlatformDisplayEXT", Platform);
            let create = egl!("eglCreateImageKHR", Create);
            let destroy = egl!("eglDestroyImageKHR", Destroy);
            let mut devices = [null_mut(); 16];
            let mut count = 0;
            ensure!(
                query(16, devices.as_mut_ptr(), &mut count) != 0,
                "EGL device enumeration failed"
            );
            let device = devices[..count.min(16) as usize]
                .iter()
                .copied()
                .find(|d| {
                    let name = string(*d, 0x3377); // EGL_DRM_RENDER_NODE_FILE_EXT
                    !name.is_null() && std::ffi::CStr::from_ptr(name).to_bytes() == node.as_bytes()
                })
                .context("capture GPU has no EGL device")?;
            let cuda = libloading::Library::new("libcuda.so.1")?;
            let register = *cuda.get::<Register>(b"cuGraphicsEGLRegisterImage\0")?;
            let mapped = *cuda.get::<Mapped>(b"cuGraphicsResourceGetMappedEglFrame\0")?;
            let unregister = *cuda.get::<Unregister>(b"cuGraphicsUnregisterResource\0")?;
            let display = platform(0x313F, device, [0x3038].as_ptr());
            ensure!(
                !display.is_null() && eglInitialize(display, null_mut(), null_mut()) != 0,
                "NVIDIA EGL initialization failed"
            );
            Ok(Arc::new(Self {
                display,
                create,
                destroy,
                register,
                mapped,
                unregister,
                _cuda: cuda,
            }))
        }
    }
    pub fn readback(&self, image: &Image, width: u32, height: u32) -> Result<Vec<u8>> {
        use cudarc::driver::sys::{CUDA_MEMCPY2D, CUmemorytype};
        let mut pixels = vec![0; width as usize * height as usize * 4];
        unsafe {
            type Copy2d = unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> i32;
            let copy = self._cuda.get::<Copy2d>(b"cuMemcpy2D_v2\0")?;
            let args = CUDA_MEMCPY2D {
                srcXInBytes: 0,
                srcY: 0,
                srcMemoryType: CUmemorytype::CU_MEMORYTYPE_ARRAY,
                srcHost: std::ptr::null(),
                srcDevice: 0,
                srcArray: image.array.cast(),
                srcPitch: 0,
                dstXInBytes: 0,
                dstY: 0,
                dstMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
                dstHost: pixels.as_mut_ptr().cast(),
                dstDevice: 0,
                dstArray: std::ptr::null_mut(),
                dstPitch: width as usize * 4,
                WidthInBytes: width as usize * 4,
                Height: height as usize,
            };
            let code = copy(&args);
            ensure!(code == 0, "CUDA capture readback failed: {code}");
        }
        Ok(pixels)
    }
    pub fn import(self: &Arc<Self>, src: &DmaFrame) -> Result<Image> {
        ensure!(
            src.descriptor.layers.len() == 1 && src.descriptor.objects.len() == 1,
            "CUDA/EGL requires one RGB plane"
        );
        let l = &src.descriptor.layers[0];
        let o = &src.descriptor.objects[0];
        ensure!(
            l.num_planes == 1 && l.object_index[0] == 0,
            "CUDA/EGL requires one RGB plane"
        );
        let modifier = o.drm_format_modifier;
        let attrs = [
            0x3057,
            src.width as i32,
            0x3056,
            src.height as i32,
            0x3271,
            l.drm_format as i32,
            0x3272,
            o.fd.as_raw_fd(),
            0x3273,
            l.offset[0] as i32,
            0x3274,
            l.pitch[0] as i32,
            0x3443,
            modifier as i32,
            0x3444,
            (modifier >> 32) as i32,
            0x3038,
        ];
        let source = src.try_clone()?;
        unsafe {
            let image = (self.create)(self.display, null_mut(), 0x3270, null_mut(), attrs.as_ptr());
            ensure!(!image.is_null(), "EGL capture dmabuf import failed");
            let mut owner = Image {
                display: self.clone(),
                image,
                resource: null_mut(),
                array: null_mut(),
                _source: source,
            };
            let code = (self.register)(&mut owner.resource, image, 1); // READ_ONLY
            ensure!(code == 0, "cuGraphicsEGLRegisterImage failed: {code}");
            let mut frame = EglFrame::default();
            let code = (self.mapped)(&mut frame, owner.resource, 0, 0);
            ensure!(
                code == 0,
                "cuGraphicsResourceGetMappedEglFrame failed: {code}"
            );
            ensure!(
                frame.frame_type == 0
                    && frame.plane_count == 1
                    && frame.width == src.width
                    && frame.height == src.height
                    && frame.channels == 4,
                "CUDA/EGL returned unsupported frame layout"
            );
            owner.array = frame.planes[0];
            ensure!(!owner.array.is_null(), "empty CUDA capture array");
            Ok(owner)
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        unsafe {
            eglTerminate(self.display);
        }
    }
}
pub struct Image {
    display: Arc<Display>,
    image: *mut c_void,
    resource: *mut c_void,
    pub array: *mut c_void,
    _source: DmaFrame,
}
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            if !self.resource.is_null() {
                (self.display.unregister)(self.resource);
            }
            (self.display.destroy)(self.display.display, self.image);
        }
    }
}
