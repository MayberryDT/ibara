use crate::codec::Decoded;
use anyhow::{Result, ensure};
use std::{
    ffi::{CString, c_char, c_void},
    ptr::null_mut,
};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};
#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(native: *mut c_void) -> *mut c_void;
    fn eglInitialize(d: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(
        d: *mut c_void,
        a: *const i32,
        c: *mut *mut c_void,
        n: i32,
        count: *mut i32,
    ) -> u32;
    fn eglCreateContext(
        d: *mut c_void,
        c: *mut c_void,
        share: *mut c_void,
        a: *const i32,
    ) -> *mut c_void;
    fn eglCreateWindowSurface(
        d: *mut c_void,
        c: *mut c_void,
        w: *mut c_void,
        a: *const i32,
    ) -> *mut c_void;
    fn eglMakeCurrent(d: *mut c_void, draw: *mut c_void, read: *mut c_void, c: *mut c_void) -> u32;
    fn eglSwapInterval(d: *mut c_void, interval: i32) -> u32;
    fn eglSwapBuffers(d: *mut c_void, s: *mut c_void) -> u32;
    fn eglGetProcAddress(n: *const c_char) -> *const c_void;
    fn eglDestroyContext(d: *mut c_void, c: *mut c_void) -> u32;
    fn eglDestroySurface(d: *mut c_void, s: *mut c_void) -> u32;
    fn eglTerminate(d: *mut c_void) -> u32;
}
#[link(name = "wayland-egl")]
unsafe extern "C" {
    fn wl_egl_window_create(s: *mut c_void, w: i32, h: i32) -> *mut c_void;
    fn wl_egl_window_resize(w: *mut c_void, width: i32, height: i32, dx: i32, dy: i32);
    fn wl_egl_window_destroy(w: *mut c_void);
}
#[link(name = "GLESv2")]
unsafe extern "C" {
    fn glCreateShader(t: u32) -> u32;
    fn glShaderSource(s: u32, n: i32, text: *const *const c_char, len: *const i32);
    fn glCompileShader(s: u32);
    fn glGetShaderiv(s: u32, p: u32, out: *mut i32);
    fn glGetShaderInfoLog(s: u32, n: i32, len: *mut i32, out: *mut c_char);
    fn glCreateProgram() -> u32;
    fn glAttachShader(p: u32, s: u32);
    fn glLinkProgram(p: u32);
    fn glGetProgramiv(p: u32, n: u32, v: *mut i32);
    fn glUseProgram(p: u32);
    fn glDeleteShader(s: u32);
    fn glDeleteProgram(p: u32);
    fn glGenTextures(n: i32, t: *mut u32);
    fn glBindTexture(t: u32, id: u32);
    fn glTexParameteri(t: u32, p: u32, v: i32);
    fn glDeleteTextures(n: i32, t: *const u32);
    fn glTexImage2D(
        t: u32,
        level: i32,
        internal: i32,
        w: i32,
        h: i32,
        border: i32,
        format: u32,
        kind: u32,
        data: *const c_void,
    );
    fn glGetAttribLocation(p: u32, name: *const c_char) -> i32;
    fn glVertexAttribPointer(
        i: u32,
        size: i32,
        t: u32,
        normalized: u8,
        stride: i32,
        p: *const c_void,
    );
    fn glEnableVertexAttribArray(i: u32);
    fn glDrawArrays(t: u32, first: i32, n: i32);
    fn glViewport(x: i32, y: i32, w: i32, h: i32);
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(bits: u32);
    fn glFinish();
}
type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTexture = unsafe extern "C" fn(u32, *mut c_void);
pub struct Renderer {
    d: *mut c_void,
    c: *mut c_void,
    s: *mut c_void,
    w: *mut c_void,
    external: u32,
    software: u32,
    texture: u32,
    create: CreateImage,
    destroy: DestroyImage,
    image_texture: ImageTexture,
    width: u32,
    height: u32,
}
fn shader(kind: u32, source: &str) -> Result<u32> {
    unsafe {
        let text = CString::new(source)?;
        let s = glCreateShader(kind);
        glShaderSource(s, 1, &text.as_ptr(), std::ptr::null());
        glCompileShader(s);
        let mut status = 0;
        glGetShaderiv(s, 0x8B81, &mut status);
        if status == 0 {
            let mut buf = vec![0u8; 2048];
            glGetShaderInfoLog(s, 2048, null_mut(), buf.as_mut_ptr().cast());
            anyhow::bail!("shader: {}", String::from_utf8_lossy(&buf));
        }
        Ok(s)
    }
}
fn program(fragment: &str) -> Result<u32> {
    unsafe {
        let vertex = shader(
            0x8B31,
            "attribute vec2 pos; varying vec2 uv; void main(){gl_Position=vec4(pos,0.,1.);uv=vec2((pos.x+1.)*.5,(1.-pos.y)*.5);}",
        )?;
        let fragment = shader(0x8B30, fragment)?;
        let p = glCreateProgram();
        glAttachShader(p, vertex);
        glAttachShader(p, fragment);
        glLinkProgram(p);
        glDeleteShader(vertex);
        glDeleteShader(fragment);
        let mut status = 0;
        glGetProgramiv(p, 0x8B82, &mut status);
        ensure!(status != 0, "GLES program link failed");
        Ok(p)
    }
}
impl Renderer {
    pub fn new(conn: &Connection, surface: &WlSurface, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let d = eglGetDisplay(conn.backend().display_ptr().cast());
            ensure!(!d.is_null(), "EGL display unavailable");
            ensure!(
                eglInitialize(d, null_mut(), null_mut()) != 0,
                "EGL initialization failed"
            );
            ensure!(eglBindAPI(0x30A0) != 0, "GLES API unavailable");
            let attrs = [
                0x3033, 4, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038,
            ];
            let mut config = null_mut();
            let mut count = 0;
            ensure!(
                eglChooseConfig(d, attrs.as_ptr(), &mut config, 1, &mut count) != 0 && count > 0,
                "EGL config unavailable"
            );
            let c = eglCreateContext(d, config, null_mut(), [0x3098, 2, 0x3038].as_ptr());
            ensure!(!c.is_null(), "GLES context unavailable");
            let w = wl_egl_window_create(surface.id().as_ptr().cast(), width as i32, height as i32);
            ensure!(!w.is_null(), "EGL Wayland window failed");
            let s = eglCreateWindowSurface(d, config, w, [0x3038].as_ptr());
            ensure!(
                !s.is_null() && eglMakeCurrent(d, s, s, c) != 0,
                "EGL window surface failed"
            );
            ensure!(eglSwapInterval(d, 0) != 0, "EGL immediate swap unavailable");
            let create = eglGetProcAddress(c"eglCreateImageKHR".as_ptr());
            let destroy = eglGetProcAddress(c"eglDestroyImageKHR".as_ptr());
            let image_texture = eglGetProcAddress(c"glEGLImageTargetTexture2DOES".as_ptr());
            ensure!(
                !create.is_null() && !destroy.is_null() && !image_texture.is_null(),
                "EGL dmabuf image extensions unavailable"
            );
            let external = program(
                "#extension GL_OES_EGL_image_external : require\nprecision mediump float; varying vec2 uv; uniform samplerExternalOES tex; void main(){gl_FragColor=texture2D(tex,uv);}",
            )?;
            let software = program(
                "precision mediump float; varying vec2 uv; uniform sampler2D tex; void main(){gl_FragColor=texture2D(tex,uv);}",
            )?;
            let mut texture = 0;
            glGenTextures(1, &mut texture);
            Ok(Self {
                d,
                c,
                s,
                w,
                external,
                software,
                texture,
                create: std::mem::transmute::<*const c_void, CreateImage>(create),
                destroy: std::mem::transmute::<*const c_void, DestroyImage>(destroy),
                image_texture: std::mem::transmute::<*const c_void, ImageTexture>(image_texture),
                width,
                height,
            })
        }
    }
    pub fn resize(&mut self, w: u32, h: u32) {
        self.width = w;
        self.height = h;
        unsafe {
            wl_egl_window_resize(self.w, w as i32, h as i32, 0, 0);
        }
    }
    fn quad(&self, program: u32) {
        unsafe {
            glViewport(0, 0, self.width as i32, self.height as i32);
            glUseProgram(program);
            let pos = glGetAttribLocation(program, c"pos".as_ptr()) as u32;
            let points: [f32; 8] = [-1., -1., 1., -1., -1., 1., 1., 1.];
            glVertexAttribPointer(pos, 2, 0x1406, 0, 0, points.as_ptr().cast());
            glEnableVertexAttribArray(pos);
            glDrawArrays(5, 0, 4);
        }
    }
    pub fn clear(&self) -> Result<()> {
        unsafe {
            glViewport(0, 0, self.width as i32, self.height as i32);
            glClearColor(0.07, 0.07, 0.08, 1.);
            glClear(0x4000);
            ensure!(eglSwapBuffers(self.d, self.s) != 0, "EGL swap failed");
        }
        Ok(())
    }
    pub fn hardware(&self, f: &Decoded) -> Result<()> {
        let frame = f.dma()?;
        let d = &frame.descriptor;
        let mut attrs = vec![
            0x3057,
            f.width as i32,
            0x3056,
            f.height as i32,
            0x3271,
            if d.layers.len() == 1 {
                d.layers[0].drm_format as i32
            } else {
                u32::from_le_bytes(*b"NV12") as i32
            },
        ];
        let mut index = 0;
        for li in 0..d.layers.len() as usize {
            let layer = &d.layers[li];
            for p in 0..layer.num_planes as usize {
                ensure!(index < 3, "too many DRM planes");
                let o = &d.objects[layer.object_index[p] as usize];
                let base = 0x3272 + index * 3;
                attrs.extend([
                    base,
                    std::os::fd::AsRawFd::as_raw_fd(&o.fd),
                    base + 1,
                    layer.offset[p] as i32,
                    base + 2,
                    layer.pitch[p] as i32,
                ]);
                if o.drm_format_modifier != u64::MAX {
                    attrs.extend([
                        0x3443 + index * 2,
                        o.drm_format_modifier as u32 as i32,
                        0x3444 + index * 2,
                        (o.drm_format_modifier >> 32) as i32,
                    ]);
                }
                index += 1;
            }
        }
        attrs.push(0x3038);
        unsafe {
            let image = (self.create)(self.d, null_mut(), 0x3270, null_mut(), attrs.as_ptr());
            ensure!(!image.is_null(), "EGL dmabuf import failed");
            glBindTexture(0x8D65, self.texture);
            glTexParameteri(0x8D65, 0x2801, 0x2601);
            glTexParameteri(0x8D65, 0x2800, 0x2601);
            glTexParameteri(0x8D65, 0x2802, 0x812F);
            glTexParameteri(0x8D65, 0x2803, 0x812F);
            (self.image_texture)(0x8D65, image);
            self.quad(self.external);
            let swapped = eglSwapBuffers(self.d, self.s);
            glFinish();
            (self.destroy)(self.d, image);
            ensure!(swapped != 0, "EGL swap failed");
        }
        Ok(())
    }
    pub fn software(&self, rgba: &[u8], w: u32, h: u32) -> Result<()> {
        ensure!(
            rgba.len() == w as usize * h as usize * 4,
            "invalid software frame"
        );
        unsafe {
            glBindTexture(0x0DE1, self.texture);
            glTexParameteri(0x0DE1, 0x2801, 0x2601);
            glTexParameteri(0x0DE1, 0x2800, 0x2601);
            glTexImage2D(
                0x0DE1,
                0,
                0x1908,
                w as i32,
                h as i32,
                0,
                0x1908,
                0x1401,
                rgba.as_ptr().cast(),
            );
            self.quad(self.software);
            ensure!(eglSwapBuffers(self.d, self.s) != 0, "EGL swap failed");
        }
        Ok(())
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            glDeleteTextures(1, &self.texture);
            glDeleteProgram(self.external);
            glDeleteProgram(self.software);
            eglMakeCurrent(self.d, null_mut(), null_mut(), null_mut());
            eglDestroySurface(self.d, self.s);
            eglDestroyContext(self.d, self.c);
            wl_egl_window_destroy(self.w);
            eglTerminate(self.d);
        }
    }
}
