//! Executing a guest's GL command stream on the host's GL.
//!
//! Every entry point is a thin wrapper over the real call: this crate is a
//! transport, not an implementation of OpenGL.
//!
//! # Names
//!
//! The guest chooses object names. For buffers and textures that is free (GL
//! binds a name it has never seen); for shaders and programs GL insists on
//! choosing, so [`Names`] maps the guest's number onto the host's.
//!
//! # Errors
//!
//! A failing command does not stop the batch -- GL has its own error latch that
//! `glGetError` reads, and stopping early would diverge from native behaviour.
//! Only a malformed *stream* stops it, since then the next command's start is
//! unknown.

use crate::proto::{error, op, Args, Commands, Header};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};

// The host's GL and EGL, declared by hand: the list is short and the ABI stable.
#[allow(non_snake_case)]
mod ffi {
    use super::*;

    pub type GLenum = c_uint;
    pub type GLboolean = u8;
    pub type GLbitfield = c_uint;
    pub type GLint = c_int;
    pub type GLuint = c_uint;
    pub type GLsizei = c_int;
    pub type GLfloat = f32;
    pub type EGLDisplay = *mut c_void;
    pub type EGLSurface = *mut c_void;
    pub type EGLContext = *mut c_void;
    pub type EGLConfig = *mut c_void;
    pub type EGLint = c_int;
    pub type EGLenum = c_uint;
    pub type EGLBoolean = c_uint;

    #[link(name = "EGL")]
    extern "C" {
        pub fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
        pub fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint)
            -> EGLBoolean;
        pub fn eglBindAPI(api: EGLenum) -> EGLBoolean;
        pub fn eglChooseConfig(
            dpy: EGLDisplay,
            attrs: *const EGLint,
            configs: *mut EGLConfig,
            size: EGLint,
            num: *mut EGLint,
        ) -> EGLBoolean;
        pub fn eglCreatePbufferSurface(
            dpy: EGLDisplay,
            config: EGLConfig,
            attrs: *const EGLint,
        ) -> EGLSurface;
        pub fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
        pub fn eglCreateContext(
            dpy: EGLDisplay,
            config: EGLConfig,
            share: EGLContext,
            attrs: *const EGLint,
        ) -> EGLContext;
        pub fn eglMakeCurrent(
            dpy: EGLDisplay,
            draw: EGLSurface,
            read: EGLSurface,
            ctx: EGLContext,
        ) -> EGLBoolean;
        pub fn eglGetError() -> EGLint;
        pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    }

    #[link(name = "GLESv2")]
    extern "C" {
        pub fn glGetString(name: GLenum) -> *const u8;
        pub fn glGetError() -> GLenum;
        pub fn glGetIntegerv(pname: GLenum, data: *mut GLint);
        pub fn glClearColor(r: GLfloat, g: GLfloat, b: GLfloat, a: GLfloat);
        pub fn glClearDepthf(d: GLfloat);
        pub fn glClear(mask: GLbitfield);
        pub fn glViewport(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
        pub fn glScissor(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
        pub fn glEnable(cap: GLenum);
        pub fn glDisable(cap: GLenum);
        pub fn glBlendFunc(src: GLenum, dst: GLenum);
        pub fn glDepthFunc(func: GLenum);
        pub fn glCullFace(mode: GLenum);
        pub fn glFrontFace(mode: GLenum);
        pub fn glPixelStorei(pname: GLenum, param: GLint);
        pub fn glFinish();

        pub fn glGenBuffers(n: GLsizei, buffers: *mut GLuint);
        pub fn glDeleteBuffers(n: GLsizei, buffers: *const GLuint);
        pub fn glBindBuffer(target: GLenum, buffer: GLuint);
        pub fn glBufferData(
            target: GLenum,
            size: isize,
            data: *const c_void,
            usage: GLenum,
        );
        pub fn glBufferSubData(target: GLenum, offset: isize, size: isize, data: *const c_void);

        pub fn glCreateShader(ty: GLenum) -> GLuint;
        pub fn glShaderSource(
            shader: GLuint,
            count: GLsizei,
            string: *const *const c_char,
            length: *const GLint,
        );
        pub fn glCompileShader(shader: GLuint);
        pub fn glDeleteShader(shader: GLuint);
        pub fn glCreateProgram() -> GLuint;
        pub fn glAttachShader(program: GLuint, shader: GLuint);
        pub fn glBindAttribLocation(program: GLuint, index: GLuint, name: *const c_char);
        pub fn glLinkProgram(program: GLuint);
        pub fn glUseProgram(program: GLuint);
        pub fn glDeleteProgram(program: GLuint);
        pub fn glGetShaderiv(shader: GLuint, pname: GLenum, params: *mut GLint);
        pub fn glGetProgramiv(program: GLuint, pname: GLenum, params: *mut GLint);
        pub fn glGetShaderInfoLog(
            shader: GLuint,
            max: GLsizei,
            len: *mut GLsizei,
            log: *mut c_char,
        );
        pub fn glGetProgramInfoLog(
            program: GLuint,
            max: GLsizei,
            len: *mut GLsizei,
            log: *mut c_char,
        );
        pub fn glGetUniformLocation(program: GLuint, name: *const c_char) -> GLint;
        pub fn glGetAttribLocation(program: GLuint, name: *const c_char) -> GLint;

        pub fn glUniform1i(location: GLint, v0: GLint);
        pub fn glUniform1f(location: GLint, v0: GLfloat);
        pub fn glUniform2f(location: GLint, v0: GLfloat, v1: GLfloat);
        pub fn glUniform3f(location: GLint, v0: GLfloat, v1: GLfloat, v2: GLfloat);
        pub fn glUniform4f(location: GLint, v0: GLfloat, v1: GLfloat, v2: GLfloat, v3: GLfloat);
        pub fn glUniformMatrix4fv(
            location: GLint,
            count: GLsizei,
            transpose: GLboolean,
            value: *const GLfloat,
        );

        pub fn glEnableVertexAttribArray(index: GLuint);
        pub fn glDisableVertexAttribArray(index: GLuint);
        pub fn glVertexAttribPointer(
            index: GLuint,
            size: GLint,
            ty: GLenum,
            normalized: GLboolean,
            stride: GLsizei,
            pointer: *const c_void,
        );

        pub fn glGenTextures(n: GLsizei, textures: *mut GLuint);
        pub fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
        pub fn glBindTexture(target: GLenum, texture: GLuint);
        pub fn glActiveTexture(unit: GLenum);
        pub fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
        #[allow(clippy::too_many_arguments)]
        pub fn glTexImage2D(
            target: GLenum,
            level: GLint,
            internal: GLint,
            width: GLsizei,
            height: GLsizei,
            border: GLint,
            format: GLenum,
            ty: GLenum,
            pixels: *const c_void,
        );
        #[allow(clippy::too_many_arguments)]
        pub fn glTexSubImage2D(
            target: GLenum,
            level: GLint,
            xoff: GLint,
            yoff: GLint,
            width: GLsizei,
            height: GLsizei,
            format: GLenum,
            ty: GLenum,
            pixels: *const c_void,
        );

        pub fn glGenFramebuffers(n: GLsizei, names: *mut GLuint);
        pub fn glDeleteFramebuffers(n: GLsizei, names: *const GLuint);
        pub fn glBindFramebuffer(target: GLenum, name: GLuint);
        pub fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
        pub fn glFramebufferTexture2D(
            target: GLenum,
            attachment: GLenum,
            textarget: GLenum,
            texture: GLuint,
            level: GLint,
        );
        pub fn glDeleteRenderbuffers(n: GLsizei, names: *const GLuint);
        pub fn glBindRenderbuffer(target: GLenum, name: GLuint);
        pub fn glRenderbufferStorage(target: GLenum, internal: GLenum, width: GLsizei, height: GLsizei);
        pub fn glFramebufferRenderbuffer(
            target: GLenum,
            attachment: GLenum,
            rb_target: GLenum,
            renderbuffer: GLuint,
        );

        pub fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
        pub fn glDrawElements(mode: GLenum, count: GLsizei, ty: GLenum, indices: *const c_void);
        #[allow(clippy::too_many_arguments)]
        pub fn glReadPixels(
            x: GLint,
            y: GLint,
            width: GLsizei,
            height: GLsizei,
            format: GLenum,
            ty: GLenum,
            pixels: *mut c_void,
        );
    }
}

use ffi::*;

const EGL_OPENGL_ES_API: EGLenum = 0x30A0;
const EGL_PLATFORM_SURFACELESS_MESA: EGLenum = 0x31DD;
const EGL_NONE: EGLint = 0x3038;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_PBUFFER_BIT: EGLint = 0x0001;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_DEPTH_SIZE: EGLint = 0x3025;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_OPENGL_API: EGLenum = 0x30A2;
const EGL_OPENGL_BIT: EGLint = 0x0008;
const EGL_STENCIL_SIZE: EGLint = 0x3026;
const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: EGLint = 0x30FD;
const EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT: EGLint = 0x2;

/// Desktop GL's fixed-function entry points, which libGLESv2 does not
/// export: looked up through EGL once a desktop context exists.
macro_rules! compat_table {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Compat {
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        impl Compat {
            /// `None` if any entry point is missing.
            unsafe fn load() -> Option<Compat> {
                Some(Compat {
                    $($name: {
                        let n = CString::new(stringify!($name)).unwrap();
                        let p = eglGetProcAddress(n.as_ptr());
                        if p.is_null() {
                            return None;
                        }
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p)
                    },)*
                })
            }
        }
    };
}

compat_table! {
    glMatrixMode: fn(GLenum);
    glLoadIdentity: fn();
    glPushMatrix: fn();
    glPopMatrix: fn();
    glTranslatef: fn(GLfloat, GLfloat, GLfloat);
    glRotatef: fn(GLfloat, GLfloat, GLfloat, GLfloat);
    glFrustum: fn(f64, f64, f64, f64, f64, f64);
    glBegin: fn(GLenum);
    glEnd: fn();
    glVertex3f: fn(GLfloat, GLfloat, GLfloat);
    glNormal3f: fn(GLfloat, GLfloat, GLfloat);
    glShadeModel: fn(GLenum);
    glLightfv: fn(GLenum, GLenum, *const GLfloat);
    glMaterialfv: fn(GLenum, GLenum, *const GLfloat);
    glGenLists: fn(GLsizei) -> GLuint;
    glNewList: fn(GLuint, GLenum);
    glEndList: fn();
    glCallList: fn(GLuint);
    glDeleteLists: fn(GLuint, GLsizei);
    glEnableClientState: fn(GLenum);
    glDisableClientState: fn(GLenum);
    glVertexPointer: fn(GLint, GLenum, GLsizei, *const c_void);
    glColorPointer: fn(GLint, GLenum, GLsizei, *const c_void);
    glTexCoordPointer: fn(GLint, GLenum, GLsizei, *const c_void);
    glClientActiveTexture: fn(GLenum);
    glOrtho: fn(f64, f64, f64, f64, f64, f64);
    glScalef: fn(GLfloat, GLfloat, GLfloat);
    glLoadMatrixf: fn(*const GLfloat);
    glMultMatrixf: fn(*const GLfloat);
    glColor4f: fn(GLfloat, GLfloat, GLfloat, GLfloat);
    glTexEnvi: fn(GLenum, GLenum, GLint);
    glAlphaFunc: fn(GLenum, GLfloat);
}

/// Guest object names to the host's, for the kinds GL insists on naming.
#[derive(Default)]
struct Names {
    shaders: HashMap<u32, GLuint>,
    programs: HashMap<u32, GLuint>,
}

#[derive(Copy, Clone)]
struct PixelStore {
    alignment: usize,
    row_length: usize,
    image_height: usize,
    skip_pixels: usize,
    skip_rows: usize,
    skip_images: usize,
}

impl Default for PixelStore {
    fn default() -> Self {
        Self { alignment: 4, row_length: 0, image_height: 0,
               skip_pixels: 0, skip_rows: 0, skip_images: 0 }
    }
}

fn format_components(format: u32) -> Option<usize> {
    match format {
        0x1900..=0x1906 | 0x1909 | 0x8d94..=0x8d96 | 0x84f9 => Some(1),
        0x190a | 0x8227 | 0x8228 => Some(2),
        0x1907 | 0x80e0 | 0x8d98 | 0x8d9a => Some(3),
        0x1908 | 0x80e1 | 0x8d99 | 0x8d9b => Some(4),
        _ => None,
    }
}

fn pixel_group_bytes(format: u32, ty: u32) -> Option<usize> {
    match ty {
        0x8032 | 0x8362 => Some(1),
        0x8033 | 0x8034 | 0x8363..=0x8366 => Some(2),
        0x8035 | 0x8036 | 0x8367 | 0x8368 | 0x84fa | 0x8c3b | 0x8c3e => Some(4),
        0x8dad => Some(8),
        0x1a00 => None, // GL_BITMAP requires bit-level offsets.
        _ => {
            let scalar = match ty {
                0x1400 | 0x1401 => 1,
                0x1402 | 0x1403 | 0x140b => 2,
                0x1404..=0x1406 | 0x140c => 4,
                0x140a => 8,
                _ => return None,
            };
            format_components(format)?.checked_mul(scalar)
        }
    }
}

fn pixel_layout(format: u32, ty: u32, width: i32, height: i32,
                depth: Option<i32>, store: PixelStore) -> Option<(usize, usize)> {
    if width < 0 || height < 0 || depth.is_some_and(|d| d < 0) { return None; }
    if width == 0 || height == 0 || depth == Some(0) { return Some((0, 0)); }
    let group = pixel_group_bytes(format, ty)?;
    let row_pixels = if store.row_length != 0 { store.row_length } else { width as usize };
    let row_bytes = row_pixels.checked_mul(group)?;
    let row_stride = row_bytes.checked_add(store.alignment - 1)?
        & !(store.alignment - 1);
    let image_rows = if depth.is_some() && store.image_height != 0 {
        store.image_height
    } else { height as usize };
    let image_stride = row_stride.checked_mul(image_rows)?;
    let offset = (if depth.is_some() { store.skip_images } else { 0 })
        .checked_mul(image_stride)?
        .checked_add(store.skip_rows.checked_mul(row_stride)?)?
        .checked_add(store.skip_pixels.checked_mul(group)?)?;
    let end = offset
        .checked_add((depth.unwrap_or(1) as usize - 1).checked_mul(image_stride)?)?
        .checked_add((height as usize - 1).checked_mul(row_stride)?)?
        .checked_add((width as usize).checked_mul(group)?)?;
    Some((offset, end))
}

fn set_pixel_store(pack: &mut PixelStore, unpack: &mut PixelStore,
                   pname: u32, param: i32) {
    if param < 0 { return; }
    let alignment = pname == 0x0d05 || pname == 0x0cf5;
    if alignment && !matches!(param, 1 | 2 | 4 | 8) { return; }
    let (state, field): (&mut PixelStore, fn(&mut PixelStore) -> &mut usize) = match pname {
        0x0d05 => (pack, |s| &mut s.alignment),
        0x0d02 => (pack, |s| &mut s.row_length),
        0x806c => (pack, |s| &mut s.image_height),
        0x0d04 => (pack, |s| &mut s.skip_pixels),
        0x0d03 => (pack, |s| &mut s.skip_rows),
        0x806b => (pack, |s| &mut s.skip_images),
        0x0cf5 => (unpack, |s| &mut s.alignment),
        0x0cf2 => (unpack, |s| &mut s.row_length),
        0x806e => (unpack, |s| &mut s.image_height),
        0x0cf4 => (unpack, |s| &mut s.skip_pixels),
        0x0cf3 => (unpack, |s| &mut s.skip_rows),
        0x806d => (unpack, |s| &mut s.skip_images),
        _ => return,
    };
    *field(state) = param as usize;
}

/// # Threads
///
/// The fields are EGL handles (bare pointers). What belongs to a thread is the
/// *current* context, and EGL allows a context to be made current on another
/// thread once it is not current anywhere. [`Gl::run`] makes it current before
/// every use, so moving one of these between threads is sound. The alternative,
/// a dedicated GL thread posted to over a channel, cost 23 of the 24 µs a
/// synchronous GL call took -- paid per `glGetError` in a draw loop.
unsafe impl Send for Gl {}
/// A GL context and the state needed to run commands against it.
pub struct Gl {
    display: EGLDisplay,
    surface: EGLSurface,
    config: EGLConfig,
    context: EGLContext,
    names: Names,
    pack_store: PixelStore,
    unpack_store: PixelStore,
    width: i32,
    height: i32,
    /// Which thread the context is current on, so it is only moved when needed.
    current_on: Option<std::thread::ThreadId>,
    /// Fixed function, once the context is a desktop one.
    compat: Option<Compat>,
    /// Read when the context is made; the answer never changes after.
    pub vendor: String,
    pub renderer: String,
    pub version: String,
}

/// What the host wants to say back, appended to the reply area in order.
#[derive(Default)]
pub struct Reply {
    bytes: Vec<u8>,
}

impl Reply {
    fn u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    fn blob(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.bytes.extend_from_slice(b);
        let pad = b.len().div_ceil(4) * 4 - b.len();
        self.bytes.extend(std::iter::repeat_n(0u8, pad));
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

unsafe fn gl_string(name: GLenum) -> String {
    let p = glGetString(name);
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned()
}

impl Gl {
    /// Bring up an offscreen context on the host.
    ///
    /// Surfaceless on purpose: the guest renders to a texture or reads back, and
    /// the host decides separately whether any of it reaches a screen.
    pub fn new(width: i32, height: i32) -> Result<Gl, String> {
        unsafe {
            let get_platform_display: Option<
                unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay,
            > = {
                let name = CString::new("eglGetPlatformDisplayEXT").unwrap();
                let p = eglGetProcAddress(name.as_ptr());
                if p.is_null() {
                    None
                } else {
                    Some(std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay,
                    >(p))
                }
            };
            let display = match get_platform_display {
                Some(f) => f(
                    EGL_PLATFORM_SURFACELESS_MESA,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ),
                None => return Err("no eglGetPlatformDisplayEXT".into()),
            };
            if display.is_null() {
                return Err("no EGL display".into());
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("eglInitialize failed: {:#x}", eglGetError()));
            }
            eglBindAPI(EGL_OPENGL_ES_API);
            let attrs = [
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES2_BIT,
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_DEPTH_SIZE,
                16,
                EGL_NONE,
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut num = 0;
            if eglChooseConfig(display, attrs.as_ptr(), &mut config, 1, &mut num) == 0 || num < 1 {
                return Err("no EGL config".into());
            }
            let pb = [EGL_WIDTH, width, EGL_HEIGHT, height, EGL_NONE];
            let surface = eglCreatePbufferSurface(display, config, pb.as_ptr());
            if surface.is_null() {
                return Err(format!("no pbuffer: {:#x}", eglGetError()));
            }
            let ctx_attrs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
            let context = eglCreateContext(
                display,
                config,
                std::ptr::null_mut(),
                ctx_attrs.as_ptr(),
            );
            if context.is_null() {
                return Err(format!("no context: {:#x}", eglGetError()));
            }
            if eglMakeCurrent(display, surface, surface, context) == 0 {
                return Err(format!("makeCurrent failed: {:#x}", eglGetError()));
            }
            Ok(Gl {
                display,
                surface,
                config,
                context,
                names: Names::default(),
                pack_store: PixelStore::default(),
                unpack_store: PixelStore::default(),
                current_on: Some(std::thread::current().id()),
                width,
                height,
                compat: None,
                vendor: gl_string(0x1F00),
                renderer: gl_string(0x1F01),
                version: gl_string(0x1F02),
            })
        }
    }

    /// Give the context a differently sized surface. EGL cannot resize a
    /// pbuffer, so this makes another and moves the context onto it; only the
    /// default framebuffer changes.
    pub fn resize(&mut self, width: i32, height: i32) -> Result<(), String> {
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            let pb = [EGL_WIDTH, width, EGL_HEIGHT, height, EGL_NONE];
            let surface = eglCreatePbufferSurface(self.display, self.config, pb.as_ptr());
            if surface.is_null() {
                return Err(format!("no pbuffer: {:#x}", eglGetError()));
            }
            if eglMakeCurrent(self.display, surface, surface, self.context) == 0 {
                return Err(format!("makeCurrent failed: {:#x}", eglGetError()));
            }
            eglDestroySurface(self.display, self.surface);
            self.surface = surface;
            self.width = width;
            self.height = height;
        }
        Ok(())
    }

    /// Replace the startup GLES context with a desktop OpenGL one of the
    /// compatibility profile, for a guest that asked for desktop GL (a WGL or
    /// GLX program). The surface keeps its size; names start over, as they
    /// would for a new context.
    ///
    /// `major` 0 asks for the highest version the host offers; a
    /// compatibility context of an older version is a valid answer to any
    /// request for 2.x or 3.0, so those are not passed on.
    fn become_desktop(&mut self, major: u32, minor: u32) -> Result<(), String> {
        unsafe {
            if eglBindAPI(EGL_OPENGL_API) == 0 {
                return Err(format!("no desktop OpenGL in EGL: {:#x}", eglGetError()));
            }
            let attrs = [
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_BIT,
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_DEPTH_SIZE,
                24,
                EGL_STENCIL_SIZE,
                8,
                EGL_NONE,
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut num = 0;
            if eglChooseConfig(self.display, attrs.as_ptr(), &mut config, 1, &mut num) == 0 || num < 1 {
                eglBindAPI(EGL_OPENGL_ES_API);
                return Err("no desktop OpenGL EGL config".into());
            }
            let mut ctx_attrs = vec![
                EGL_CONTEXT_OPENGL_PROFILE_MASK,
                EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT,
            ];
            if major > 3 || (major == 3 && minor > 0) {
                ctx_attrs.extend_from_slice(&[EGL_CONTEXT_MAJOR_VERSION, major as EGLint]);
                ctx_attrs.extend_from_slice(&[EGL_CONTEXT_MINOR_VERSION, minor as EGLint]);
            }
            ctx_attrs.push(EGL_NONE);
            let context = eglCreateContext(self.display, config, std::ptr::null_mut(), ctx_attrs.as_ptr());
            if context.is_null() {
                eglBindAPI(EGL_OPENGL_ES_API);
                return Err(format!("no desktop OpenGL context: {:#x}", eglGetError()));
            }
            let pb = [EGL_WIDTH, self.width, EGL_HEIGHT, self.height, EGL_NONE];
            let surface = eglCreatePbufferSurface(self.display, config, pb.as_ptr());
            if surface.is_null() {
                eglDestroyContext(self.display, context);
                eglBindAPI(EGL_OPENGL_ES_API);
                return Err(format!("no desktop pbuffer: {:#x}", eglGetError()));
            }
            if eglMakeCurrent(self.display, surface, surface, context) == 0 {
                return Err(format!("makeCurrent failed: {:#x}", eglGetError()));
            }
            eglDestroySurface(self.display, self.surface);
            eglDestroyContext(self.display, self.context);
            self.surface = surface;
            self.context = context;
            self.config = config;
            self.names = Names::default();
            self.pack_store = PixelStore::default();
            self.unpack_store = PixelStore::default();
            self.current_on = Some(std::thread::current().id());
            self.compat = Compat::load();
            self.vendor = gl_string(0x1F00);
            self.renderer = gl_string(0x1F01);
            self.version = gl_string(0x1F02);
        }
        Ok(())
    }

    /// Whether the context is a desktop one (fixed function available).
    pub fn is_desktop(&self) -> bool {
        self.compat.is_some()
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Make this context current on the calling thread, if it is not already.
    ///
    /// A no-op after the first call for a single-threaded guest; one with
    /// several pays an `eglMakeCurrent` when the drawing thread changes.
    pub fn make_current(&mut self) -> Result<(), String> {
        let me = std::thread::current().id();
        if self.current_on == Some(me) {
            return Ok(());
        }
        unsafe {
            // Fails if the context is still current on another thread; EGL
            // reports that rather than doing something surprising.
            if eglMakeCurrent(self.display, self.surface, self.surface, self.context) == 0 {
                return Err(format!("makeCurrent failed: {:#x}", eglGetError()));
            }
        }
        self.current_on = Some(me);
        Ok(())
    }

    fn shader(&self, guest: u32) -> Option<GLuint> {
        self.names.shaders.get(&guest).copied()
    }

    fn program(&self, guest: u32) -> Option<GLuint> {
        self.names.programs.get(&guest).copied()
    }

    /// Run one batch. Returns the reply and, if the stream was malformed,
    /// where and why.
    pub fn run(&mut self, batch: &[u8]) -> (Reply, u32, u32) {
        let mut reply = Reply::default();
        if self.make_current().is_err() {
            return (reply, error::NO_CONTEXT, 0);
        }
        let mut commands = Commands::new(batch);
        let mut index = 0u32;
        loop {
            match commands.next() {
                None => return (reply, error::NONE, index),
                Some(Err(())) => return (reply, error::TRUNCATED, index),
                Some(Ok((opcode, body))) => {
                    if !self.one(opcode, body, &mut reply) {
                        return (reply, error::BAD_OPCODE, index);
                    }
                    index += 1;
                }
            }
        }
    }

    /// Execute one command. `false` means the opcode is unknown, which is a
    /// stream the host cannot keep reading.
    fn one(&mut self, opcode: u32, body: &[u8], reply: &mut Reply) -> bool {
        let mut a = Args::new(body);
        unsafe {
            match opcode {
                op::CONTEXT_CREATE => {
                    // This host brings up a GLES context at startup; a guest
                    // that asks for desktop GL gets a desktop compatibility
                    // context in its place.
                    let (Some(api), Some(major), Some(minor)) =
                        (a.u32(), a.u32(), a.u32())
                    else {
                        return true;
                    };
                    if api == crate::proto::client_api::DESKTOP && self.compat.is_none() {
                        if let Err(e) = self.become_desktop(major, minor) {
                            eprintln!("gex-gl: {e}; staying on OpenGL ES");
                        }
                    }
                    reply.blob(self.vendor.as_bytes());
                    reply.blob(self.renderer.as_bytes());
                    reply.blob(self.version.as_bytes());
                }
                op::SURFACE_RESIZE => {
                    let (Some(w), Some(h)) = (a.i32(), a.i32()) else {
                        return true;
                    };
                    let _ = self.resize(w, h);
                }
                op::FINISH => {
                    glFinish();
                    reply.u32(0);
                }

                op::CLEAR_COLOR => {
                    let (Some(r), Some(g), Some(b), Some(al)) =
                        (a.f32(), a.f32(), a.f32(), a.f32())
                    else {
                        return true;
                    };
                    glClearColor(r, g, b, al);
                }
                op::CLEAR_DEPTH => {
                    if let Some(d) = a.f32() {
                        glClearDepthf(d);
                    }
                }
                op::CLEAR => {
                    if let Some(m) = a.u32() {
                        glClear(m);
                    }
                }
                op::VIEWPORT => {
                    let (Some(x), Some(y), Some(w), Some(h)) =
                        (a.i32(), a.i32(), a.i32(), a.i32())
                    else {
                        return true;
                    };
                    glViewport(x, y, w, h);
                }
                op::SCISSOR => {
                    let (Some(x), Some(y), Some(w), Some(h)) =
                        (a.i32(), a.i32(), a.i32(), a.i32())
                    else {
                        return true;
                    };
                    glScissor(x, y, w, h);
                }
                op::ENABLE => {
                    if let Some(c) = a.u32() {
                        glEnable(c);
                    }
                }
                op::DISABLE => {
                    if let Some(c) = a.u32() {
                        glDisable(c);
                    }
                }
                op::BLEND_FUNC => {
                    if let (Some(s), Some(d)) = (a.u32(), a.u32()) {
                        glBlendFunc(s, d);
                    }
                }
                op::DEPTH_FUNC => {
                    if let Some(f) = a.u32() {
                        glDepthFunc(f);
                    }
                }
                op::CULL_FACE => {
                    if let Some(m) = a.u32() {
                        glCullFace(m);
                    }
                }
                op::FRONT_FACE => {
                    if let Some(m) = a.u32() {
                        glFrontFace(m);
                    }
                }
                op::PIXEL_STOREI => {
                    if let (Some(p), Some(v)) = (a.u32(), a.i32()) {
                        glPixelStorei(p, v);
                        set_pixel_store(&mut self.pack_store, &mut self.unpack_store, p, v);
                    }
                }

                op::GEN_BUFFERS | op::GEN_TEXTURES => {
                    // Nothing to do: the guest picked the names, and GL accepts
                    // an unseen one at bind time.
                }
                op::DELETE_BUFFERS => {
                    let n = a.u32().unwrap_or(0);
                    let names: Vec<GLuint> = (0..n).filter_map(|_| a.u32()).collect();
                    glDeleteBuffers(names.len() as GLsizei, names.as_ptr());
                }
                op::DELETE_TEXTURES => {
                    let n = a.u32().unwrap_or(0);
                    let names: Vec<GLuint> = (0..n).filter_map(|_| a.u32()).collect();
                    glDeleteTextures(names.len() as GLsizei, names.as_ptr());
                }
                op::BIND_BUFFER => {
                    if let (Some(t), Some(n)) = (a.u32(), a.u32()) {
                        glBindBuffer(t, n);
                    }
                }
                op::BUFFER_DATA => {
                    let (Some(target), Some(usage), Some(size), Some(has_data), Some(data)) =
                        (a.u32(), a.u32(), a.u64(), a.u32(), a.blob())
                    else {
                        return true;
                    };
                    if size > isize::MAX as u64
                        || (has_data != 0 && data.len() as u64 != size)
                        || (has_data == 0 && !data.is_empty())
                    {
                        return true;
                    }
                    glBufferData(
                        target,
                        size as isize,
                        if has_data != 0 {
                            data.as_ptr() as *const c_void
                        } else {
                            std::ptr::null()
                        },
                        usage,
                    );
                }
                op::BUFFER_SUB_DATA => {
                    let (Some(target), Some(offset), Some(size), Some(data)) =
                        (a.u32(), a.u64(), a.u64(), a.blob())
                    else {
                        return true;
                    };
                    if offset > isize::MAX as u64 || size > isize::MAX as u64 || data.len() as u64 != size {
                        return true;
                    }
                    glBufferSubData(
                        target,
                        offset as isize,
                        size as isize,
                        data.as_ptr() as *const c_void,
                    );
                }

                op::CREATE_SHADER => {
                    let (Some(name), Some(ty)) = (a.u32(), a.u32()) else {
                        return true;
                    };
                    let real = glCreateShader(ty);
                    self.names.shaders.insert(name, real);
                }
                op::SHADER_SOURCE => {
                    let (Some(name), Some(src)) = (a.u32(), a.blob()) else {
                        return true;
                    };
                    let Some(real) = self.shader(name) else {
                        return true;
                    };
                    let ptr = src.as_ptr() as *const c_char;
                    let len = src.len() as GLint;
                    glShaderSource(real, 1, &ptr, &len);
                }
                op::COMPILE_SHADER => {
                    if let Some(real) = a.u32().and_then(|n| self.shader(n)) {
                        glCompileShader(real);
                    }
                }
                op::DELETE_SHADER => {
                    if let Some(n) = a.u32() {
                        if let Some(real) = self.names.shaders.remove(&n) {
                            glDeleteShader(real);
                        }
                    }
                }
                op::CREATE_PROGRAM => {
                    if let Some(name) = a.u32() {
                        let real = glCreateProgram();
                        self.names.programs.insert(name, real);
                    }
                }
                op::ATTACH_SHADER => {
                    let (Some(p), Some(s)) = (a.u32(), a.u32()) else {
                        return true;
                    };
                    if let (Some(p), Some(s)) = (self.program(p), self.shader(s)) {
                        glAttachShader(p, s);
                    }
                }
                op::BIND_ATTRIB_LOCATION => {
                    let (Some(p), Some(index), Some(name)) = (a.u32(), a.u32(), a.blob()) else {
                        return true;
                    };
                    if let (Some(p), Ok(name)) = (self.program(p), CString::new(name)) {
                        glBindAttribLocation(p, index, name.as_ptr());
                    }
                }
                op::LINK_PROGRAM => {
                    if let Some(p) = a.u32().and_then(|n| self.program(n)) {
                        glLinkProgram(p);
                    }
                }
                op::USE_PROGRAM => {
                    let Some(n) = a.u32() else { return true };
                    // Zero is "no program", and is not in the map.
                    let real = if n == 0 { Some(0) } else { self.program(n) };
                    if let Some(p) = real {
                        glUseProgram(p);
                    }
                }
                op::DELETE_PROGRAM => {
                    if let Some(n) = a.u32() {
                        if let Some(real) = self.names.programs.remove(&n) {
                            glDeleteProgram(real);
                        }
                    }
                }
                op::GET_UNIFORM_LOCATION | op::GET_ATTRIB_LOCATION => {
                    let (Some(p), Some(name)) = (a.u32(), a.blob()) else {
                        return true;
                    };
                    let loc = match (self.program(p), CString::new(name)) {
                        (Some(p), Ok(name)) => {
                            if opcode == op::GET_UNIFORM_LOCATION {
                                glGetUniformLocation(p, name.as_ptr())
                            } else {
                                glGetAttribLocation(p, name.as_ptr())
                            }
                        }
                        _ => -1,
                    };
                    reply.u32(loc as u32);
                }
                op::GET_SHADER_IV | op::GET_PROGRAM_IV => {
                    let (Some(n), Some(pname)) = (a.u32(), a.u32()) else {
                        return true;
                    };
                    let mut v: GLint = 0;
                    if opcode == op::GET_SHADER_IV {
                        if let Some(real) = self.shader(n) {
                            glGetShaderiv(real, pname, &mut v);
                        }
                    } else if let Some(real) = self.program(n) {
                        glGetProgramiv(real, pname, &mut v);
                    }
                    reply.u32(v as u32);
                }
                op::GET_SHADER_INFO_LOG | op::GET_PROGRAM_INFO_LOG => {
                    let Some(n) = a.u32() else { return true };
                    let mut buf = vec![0u8; 4096];
                    let mut len: GLsizei = 0;
                    if opcode == op::GET_SHADER_INFO_LOG {
                        if let Some(real) = self.shader(n) {
                            glGetShaderInfoLog(
                                real,
                                buf.len() as GLsizei,
                                &mut len,
                                buf.as_mut_ptr() as *mut c_char,
                            );
                        }
                    } else if let Some(real) = self.program(n) {
                        glGetProgramInfoLog(
                            real,
                            buf.len() as GLsizei,
                            &mut len,
                            buf.as_mut_ptr() as *mut c_char,
                        );
                    }
                    reply.blob(&buf[..len.max(0) as usize]);
                }

                op::UNIFORM_1I => {
                    if let (Some(l), Some(v)) = (a.i32(), a.i32()) {
                        glUniform1i(l, v);
                    }
                }
                op::UNIFORM_1F => {
                    if let (Some(l), Some(v)) = (a.i32(), a.f32()) {
                        glUniform1f(l, v);
                    }
                }
                op::UNIFORM_2F => {
                    if let (Some(l), Some(x), Some(y)) = (a.i32(), a.f32(), a.f32()) {
                        glUniform2f(l, x, y);
                    }
                }
                op::UNIFORM_3F => {
                    if let (Some(l), Some(x), Some(y), Some(z)) =
                        (a.i32(), a.f32(), a.f32(), a.f32())
                    {
                        glUniform3f(l, x, y, z);
                    }
                }
                op::UNIFORM_4F => {
                    if let (Some(l), Some(x), Some(y), Some(z), Some(w)) =
                        (a.i32(), a.f32(), a.f32(), a.f32(), a.f32())
                    {
                        glUniform4f(l, x, y, z, w);
                    }
                }
                op::UNIFORM_MATRIX4FV => {
                    let (Some(l), Some(count), Some(transpose), Some(data)) =
                        (a.i32(), a.u32(), a.u32(), a.blob())
                    else {
                        return true;
                    };
                    if data.len() >= count as usize * 64 {
                        glUniformMatrix4fv(
                            l,
                            count as GLsizei,
                            transpose as GLboolean,
                            data.as_ptr() as *const GLfloat,
                        );
                    }
                }

                op::ENABLE_VERTEX_ATTRIB_ARRAY => {
                    if let Some(i) = a.u32() {
                        glEnableVertexAttribArray(i);
                    }
                }
                op::DISABLE_VERTEX_ATTRIB_ARRAY => {
                    if let Some(i) = a.u32() {
                        glDisableVertexAttribArray(i);
                    }
                }
                op::VERTEX_ATTRIB_POINTER => {
                    let (Some(i), Some(size), Some(ty), Some(norm), Some(stride), Some(offset)) =
                        (a.u32(), a.i32(), a.u32(), a.u32(), a.i32(), a.u32())
                    else {
                        return true;
                    };
                    glVertexAttribPointer(
                        i,
                        size,
                        ty,
                        norm as GLboolean,
                        stride,
                        offset as usize as *const c_void,
                    );
                }

                op::BIND_TEXTURE => {
                    if let (Some(t), Some(n)) = (a.u32(), a.u32()) {
                        glBindTexture(t, n);
                    }
                }
                op::ACTIVE_TEXTURE => {
                    if let Some(u) = a.u32() {
                        glActiveTexture(u);
                    }
                }
                op::TEX_PARAMETER_I => {
                    if let (Some(t), Some(p), Some(v)) = (a.u32(), a.u32(), a.i32()) {
                        glTexParameteri(t, p, v);
                    }
                }
                op::TEX_IMAGE_2D => {
                    let (
                        Some(target),
                        Some(level),
                        Some(internal),
                        Some(w),
                        Some(h),
                        Some(border),
                        Some(format),
                        Some(ty),
                        Some(pbo),
                        Some(pointer),
                        Some(data),
                    ) = (
                        a.u32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.u32(),
                        a.u32(),
                        a.u32(),
                        a.u64(),
                        a.blob(),
                    )
                    else {
                        return true;
                    };
                    glTexImage2D(
                        target,
                        level,
                        internal,
                        w,
                        h,
                        border,
                        format,
                        ty,
                        if pbo != 0 {
                            pointer as usize as *const c_void
                        } else if pointer == 0 {
                            std::ptr::null()
                        } else {
                            data.as_ptr() as *const c_void
                        },
                    );
                }
                op::TEX_SUB_IMAGE_2D => {
                    let (
                        Some(target),
                        Some(level),
                        Some(x),
                        Some(y),
                        Some(w),
                        Some(h),
                        Some(format),
                        Some(ty),
                        Some(pbo),
                        Some(pointer),
                        Some(data),
                    ) = (
                        a.u32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.i32(),
                        a.u32(),
                        a.u32(),
                        a.u32(),
                        a.u64(),
                        a.blob(),
                    )
                    else {
                        return true;
                    };
                    glTexSubImage2D(
                        target,
                        level,
                        x,
                        y,
                        w,
                        h,
                        format,
                        ty,
                        if pbo != 0 { pointer as usize as *const c_void }
                        else { data.as_ptr() as *const c_void },
                    );
                }

                op::DRAW_ARRAYS => {
                    let (Some(mode), Some(first), Some(count)) = (a.u32(), a.i32(), a.i32())
                    else {
                        return true;
                    };
                    glDrawArrays(mode, first, count);
                }
                op::DRAW_ELEMENTS => {
                    let (Some(mode), Some(count), Some(ty), Some(offset)) =
                        (a.u32(), a.i32(), a.u32(), a.u32())
                    else {
                        return true;
                    };
                    glDrawElements(mode, count, ty, offset as usize as *const c_void);
                }

                op::READ_PIXELS => {
                    let (Some(x), Some(y), Some(w), Some(h), Some(format), Some(ty),
                         Some(pbo), Some(pointer)) =
                        (a.i32(), a.i32(), a.i32(), a.i32(), a.u32(), a.u32(),
                         a.u32(), a.u64())
                    else {
                        return true;
                    };
                    if pbo != 0 {
                        glReadPixels(x, y, w, h, format, ty,
                                     pointer as usize as *mut c_void);
                    } else if let Some((offset, end)) =
                        pixel_layout(format, ty, w, h, None, self.pack_store)
                    {
                        let mut px = vec![0u8; end];
                        glReadPixels(x, y, w, h, format, ty,
                                     px.as_mut_ptr() as *mut c_void);
                        reply.u32(offset as u32);
                        reply.blob(&px[offset..]);
                    } else {
                        glReadPixels(x, y, w, h, format, ty, std::ptr::null_mut());
                        reply.u32(0);
                        reply.blob(&[]);
                    }
                }
                op::GET_STRING => {
                    let Some(name) = a.u32() else { return true };
                    let s = gl_string(name);
                    reply.blob(s.as_bytes());
                }
                op::GET_INTEGERV => {
                    let Some(pname) = a.u32() else { return true };
                    let mut v = [0 as GLint; 4];
                    glGetIntegerv(pname, v.as_mut_ptr());
                    for x in v {
                        reply.u32(x as u32);
                    }
                }
                op::GET_ERROR => {
                    reply.u32(glGetError());
                }

                // Framebuffers. As for buffers and textures the guest picks
                // the names, which binding accepts.
                op::GEN_FRAMEBUFFERS | op::GEN_RENDERBUFFERS => {}
                op::DELETE_FRAMEBUFFERS | op::DELETE_RENDERBUFFERS => {
                    let n = a.u32().unwrap_or(0);
                    let names: Vec<GLuint> = (0..n).filter_map(|_| a.u32()).collect();
                    if opcode == op::DELETE_FRAMEBUFFERS {
                        glDeleteFramebuffers(names.len() as GLsizei, names.as_ptr());
                    } else {
                        glDeleteRenderbuffers(names.len() as GLsizei, names.as_ptr());
                    }
                }
                op::BIND_FRAMEBUFFER => {
                    if let (Some(t), Some(n)) = (a.u32(), a.u32()) {
                        glBindFramebuffer(t, n);
                    }
                }
                op::BIND_RENDERBUFFER => {
                    if let (Some(t), Some(n)) = (a.u32(), a.u32()) {
                        glBindRenderbuffer(t, n);
                    }
                }
                op::CHECK_FRAMEBUFFER_STATUS => {
                    let Some(t) = a.u32() else { return true };
                    reply.u32(glCheckFramebufferStatus(t));
                }
                op::FRAMEBUFFER_TEXTURE_2D => {
                    let (Some(t), Some(att), Some(tt), Some(tex), Some(level)) =
                        (a.u32(), a.u32(), a.u32(), a.u32(), a.i32())
                    else {
                        return true;
                    };
                    glFramebufferTexture2D(t, att, tt, tex, level);
                }
                op::FRAMEBUFFER_RENDERBUFFER => {
                    let (Some(t), Some(att), Some(rt), Some(rb)) = (a.u32(), a.u32(), a.u32(), a.u32()) else {
                        return true;
                    };
                    glFramebufferRenderbuffer(t, att, rt, rb);
                }
                op::RENDERBUFFER_STORAGE => {
                    let (Some(t), Some(f), Some(w), Some(h)) = (a.u32(), a.u32(), a.i32(), a.i32()) else {
                        return true;
                    };
                    glRenderbufferStorage(t, f, w, h);
                }

                // Fixed function: only in a desktop context.
                op::MATRIX_MODE..=op::ALPHA_FUNC => {
                    let Some(f) = &self.compat else { return false };
                    match opcode {
                        op::MATRIX_MODE => {
                            let Some(m) = a.u32() else { return true };
                            (f.glMatrixMode)(m);
                        }
                        op::LOAD_IDENTITY => (f.glLoadIdentity)(),
                        op::PUSH_MATRIX => (f.glPushMatrix)(),
                        op::POP_MATRIX => (f.glPopMatrix)(),
                        op::TRANSLATEF => {
                            let (Some(x), Some(y), Some(z)) = (a.f32(), a.f32(), a.f32()) else { return true };
                            (f.glTranslatef)(x, y, z);
                        }
                        op::ROTATEF => {
                            let (Some(g), Some(x), Some(y), Some(z)) = (a.f32(), a.f32(), a.f32(), a.f32()) else {
                                return true;
                            };
                            (f.glRotatef)(g, x, y, z);
                        }
                        op::FRUSTUM => {
                            let v: Vec<f64> = (0..6).filter_map(|_| a.f64()).collect();
                            if v.len() != 6 {
                                return true;
                            }
                            (f.glFrustum)(v[0], v[1], v[2], v[3], v[4], v[5]);
                        }
                        op::BEGIN => {
                            let Some(m) = a.u32() else { return true };
                            (f.glBegin)(m);
                        }
                        op::END => (f.glEnd)(),
                        op::VERTEX3F | op::NORMAL3F => {
                            let (Some(x), Some(y), Some(z)) = (a.f32(), a.f32(), a.f32()) else { return true };
                            if opcode == op::VERTEX3F {
                                (f.glVertex3f)(x, y, z);
                            } else {
                                (f.glNormal3f)(x, y, z);
                            }
                        }
                        op::SHADE_MODEL => {
                            let Some(m) = a.u32() else { return true };
                            (f.glShadeModel)(m);
                        }
                        op::LIGHTFV | op::MATERIALFV => {
                            let (Some(target), Some(pname), Some(count)) = (a.u32(), a.u32(), a.u32()) else {
                                return true;
                            };
                            if count > 4 {
                                return true;
                            }
                            let mut v = [0f32; 4];
                            for x in v.iter_mut().take(count as usize) {
                                *x = a.f32().unwrap_or(0.0);
                            }
                            if opcode == op::LIGHTFV {
                                (f.glLightfv)(target, pname, v.as_ptr());
                            } else {
                                (f.glMaterialfv)(target, pname, v.as_ptr());
                            }
                        }
                        op::GEN_LISTS => {
                            let Some(n) = a.i32() else { return true };
                            reply.u32((f.glGenLists)(n));
                        }
                        op::NEW_LIST => {
                            let (Some(l), Some(m)) = (a.u32(), a.u32()) else { return true };
                            (f.glNewList)(l, m);
                        }
                        op::END_LIST => (f.glEndList)(),
                        op::CALL_LIST => {
                            let Some(l) = a.u32() else { return true };
                            (f.glCallList)(l);
                        }
                        op::DELETE_LISTS => {
                            let (Some(l), Some(n)) = (a.u32(), a.i32()) else { return true };
                            (f.glDeleteLists)(l, n);
                        }
                        op::ENABLE_CLIENT_STATE | op::DISABLE_CLIENT_STATE => {
                            let Some(cap) = a.u32() else { return true };
                            if opcode == op::ENABLE_CLIENT_STATE {
                                (f.glEnableClientState)(cap);
                            } else {
                                (f.glDisableClientState)(cap);
                            }
                        }
                        op::VERTEX_POINTER | op::COLOR_POINTER | op::TEX_COORD_POINTER => {
                            let (Some(size), Some(ty), Some(stride), Some(offset)) = (a.i32(), a.u32(), a.i32(), a.u64())
                            else {
                                return true;
                            };
                            let p = offset as usize as *const c_void;
                            match opcode {
                                op::VERTEX_POINTER => (f.glVertexPointer)(size, ty, stride, p),
                                op::COLOR_POINTER => (f.glColorPointer)(size, ty, stride, p),
                                _ => (f.glTexCoordPointer)(size, ty, stride, p),
                            }
                        }
                        op::CLIENT_ACTIVE_TEXTURE => {
                            let Some(u) = a.u32() else { return true };
                            (f.glClientActiveTexture)(u);
                        }
                        op::ORTHO => {
                            let v: Vec<f64> = (0..6).filter_map(|_| a.f64()).collect();
                            if v.len() != 6 {
                                return true;
                            }
                            (f.glOrtho)(v[0], v[1], v[2], v[3], v[4], v[5]);
                        }
                        op::SCALEF => {
                            let (Some(x), Some(y), Some(z)) = (a.f32(), a.f32(), a.f32()) else { return true };
                            (f.glScalef)(x, y, z);
                        }
                        op::LOAD_MATRIXF | op::MULT_MATRIXF => {
                            let m: Vec<f32> = (0..16).filter_map(|_| a.f32()).collect();
                            if m.len() != 16 {
                                return true;
                            }
                            if opcode == op::LOAD_MATRIXF {
                                (f.glLoadMatrixf)(m.as_ptr());
                            } else {
                                (f.glMultMatrixf)(m.as_ptr());
                            }
                        }
                        op::COLOR4F => {
                            let (Some(r), Some(g), Some(b2), Some(al)) = (a.f32(), a.f32(), a.f32(), a.f32()) else {
                                return true;
                            };
                            (f.glColor4f)(r, g, b2, al);
                        }
                        op::TEX_ENVI => {
                            let (Some(t), Some(pn), Some(v)) = (a.u32(), a.u32(), a.i32()) else { return true };
                            (f.glTexEnvi)(t, pn, v);
                        }
                        op::ALPHA_FUNC => {
                            let (Some(func), Some(r)) = (a.u32(), a.f32()) else { return true };
                            (f.glAlphaFunc)(func, r);
                        }
                        _ => return false,
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

/// Read a [`Header`] out of the front of a shared buffer.
pub fn read_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < Header::SIZE {
        return None;
    }
    let w = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    Some(Header {
        magic: w(0),
        capacity: w(1),
        tail: w(2),
        reply_capacity: w(3),
        reply_len: w(4),
        status: w(5),
        failed_at: w(6),
        protocol_version: w(7),
    })
}

impl Header {
    pub const SIZE: usize = 32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{client_api, op, Batch};

    // The handful of GL constants a test needs, spelled out rather than
    // pulled in: a guest sends these numbers, so a test should send numbers.
    const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
    const GL_TRIANGLES: u32 = 0x0004;
    const GL_ARRAY_BUFFER: u32 = 0x8892;
    const GL_STATIC_DRAW: u32 = 0x88E4;
    const GL_FLOAT: u32 = 0x1406;
    const GL_VERTEX_SHADER: u32 = 0x8B31;
    const GL_FRAGMENT_SHADER: u32 = 0x8B30;
    const GL_COMPILE_STATUS: u32 = 0x8B81;
    const GL_LINK_STATUS: u32 = 0x8B82;
    const GL_RGBA: u32 = 0x1908;
    const GL_UNSIGNED_BYTE: u32 = 0x1401;

    fn read_blob(reply: &[u8], at: &mut usize) -> Vec<u8> {
        let len = u32::from_le_bytes(reply[*at..*at + 4].try_into().unwrap()) as usize;
        *at += 4;
        let out = reply[*at..*at + len].to_vec();
        *at += len.div_ceil(4) * 4;
        out
    }

    fn read_u32(reply: &[u8], at: &mut usize) -> u32 {
        let v = u32::from_le_bytes(reply[*at..*at + 4].try_into().unwrap());
        *at += 4;
        v
    }

    /// The whole path: bring up a context, compile the application's own
    /// GLSL, upload vertices, draw, read the pixels back.
    ///
    /// One test rather than several because an EGL context belongs to the
    /// thread that made it, and `cargo test` runs threads in parallel.
    #[test]
    fn a_command_stream_draws_a_triangle_and_reads_it_back() {
        let mut gl = match Gl::new(64, 64) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no host GL ({e})");
                return;
            }
        };
        eprintln!("host GL: {} / {}", gl.renderer, gl.version);

        // Vertex data covering the left half of the viewport, so a readback
        // has an obvious inside and outside.
        let verts: [f32; 6] = [-1.0, -1.0, 0.0, -1.0, -1.0, 1.0];
        let mut bytes = Vec::new();
        for v in verts {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        let vs = b"attribute vec2 pos;\nvoid main(){gl_Position=vec4(pos,0.0,1.0);}\n";
        let fs = b"precision mediump float;\nvoid main(){gl_FragColor=vec4(0.0,1.0,0.0,1.0);}\n";

        let mut b = Batch::new();
        b.cmd(op::CONTEXT_CREATE, &[client_api::ES, 0, 0]);
        b.cmd(op::VIEWPORT, &[0, 0, 64, 64]);
        b.cmd(
            op::CLEAR_COLOR,
            &[
                0.0f32.to_bits(),
                0.0f32.to_bits(),
                1.0f32.to_bits(),
                1.0f32.to_bits(),
            ],
        );
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        // Shaders, with the guest choosing the names.
        b.cmd(op::CREATE_SHADER, &[1, GL_VERTEX_SHADER]);
        b.raw(op::SHADER_SOURCE, &[1, vs.len() as u32], vs);
        b.cmd(op::COMPILE_SHADER, &[1]);
        b.cmd(op::GET_SHADER_IV, &[1, GL_COMPILE_STATUS]);
        b.cmd(op::CREATE_SHADER, &[2, GL_FRAGMENT_SHADER]);
        b.raw(op::SHADER_SOURCE, &[2, fs.len() as u32], fs);
        b.cmd(op::COMPILE_SHADER, &[2]);
        b.cmd(op::GET_SHADER_IV, &[2, GL_COMPILE_STATUS]);
        b.cmd(op::CREATE_PROGRAM, &[3]);
        b.cmd(op::ATTACH_SHADER, &[3, 1]);
        b.cmd(op::ATTACH_SHADER, &[3, 2]);
        b.cmd(op::LINK_PROGRAM, &[3]);
        b.cmd(op::GET_PROGRAM_IV, &[3, GL_LINK_STATUS]);
        b.cmd(op::USE_PROGRAM, &[3]);
        b.raw(op::GET_ATTRIB_LOCATION, &[3, 3], b"pos");
        // A buffer, with the guest choosing the name.
        b.cmd(op::GEN_BUFFERS, &[1, 7]);
        b.cmd(op::BIND_BUFFER, &[GL_ARRAY_BUFFER, 7]);
        b.raw(
            op::BUFFER_DATA,
            &[GL_ARRAY_BUFFER, GL_STATIC_DRAW, bytes.len() as u32, 0, 1,
              bytes.len() as u32],
            &bytes,
        );
        b.cmd(op::ENABLE_VERTEX_ATTRIB_ARRAY, &[0]);
        b.cmd(op::VERTEX_ATTRIB_POINTER, &[0, 2, GL_FLOAT, 0, 0, 0]);
        b.cmd(op::DRAW_ARRAYS, &[GL_TRIANGLES, 0, 3]);
        b.cmd(op::FINISH, &[]);
        b.cmd(op::READ_PIXELS,
              &[0, 0, 64, 64, GL_RGBA, GL_UNSIGNED_BYTE, 0, 1, 0]);
        b.cmd(op::GET_ERROR, &[]);

        let (reply, status, ran) = gl.run(b.bytes());
        assert_eq!(status, error::NONE, "the stream was understood");
        assert!(ran > 20, "every command ran, got {ran}");

        let r = reply.bytes();
        let mut at = 0;
        let vendor = read_blob(r, &mut at);
        let renderer = read_blob(r, &mut at);
        let version = read_blob(r, &mut at);
        assert!(!vendor.is_empty() && !renderer.is_empty() && !version.is_empty());
        let vs_ok = read_u32(r, &mut at);
        let fs_ok = read_u32(r, &mut at);
        let link_ok = read_u32(r, &mut at);
        assert_eq!(vs_ok, 1, "the vertex shader compiled");
        assert_eq!(fs_ok, 1, "the fragment shader compiled");
        assert_eq!(link_ok, 1, "the program linked");
        let attrib = read_u32(r, &mut at) as i32;
        assert_eq!(attrib, 0, "pos was bound to location 0");
        let _finish = read_u32(r, &mut at);
        let pixel_offset = read_u32(r, &mut at);
        assert_eq!(pixel_offset, 0);
        let pixels = read_blob(r, &mut at);
        let err = read_u32(r, &mut at);
        assert_eq!(err, 0, "GL reported no error");

        assert_eq!(pixels.len(), 64 * 64 * 4);
        let at_px = |x: usize, y: usize| {
            let o = (y * 64 + x) * 4;
            (pixels[o], pixels[o + 1], pixels[o + 2], pixels[o + 3])
        };
        // The triangle covers the bottom-left; readback is bottom-up.
        assert_eq!(at_px(2, 2), (0, 255, 0, 255), "inside the triangle");
        assert_eq!(at_px(60, 60), (0, 0, 255, 255), "outside it is the clear");
    }

    #[test]
    fn a_shader_that_does_not_compile_says_why() {
        let mut gl = match Gl::new(16, 16) {
            Ok(g) => g,
            Err(_) => return,
        };
        let bad = b"void main(){ this is not GLSL }\n";
        let mut b = Batch::new();
        b.cmd(op::CREATE_SHADER, &[1, GL_FRAGMENT_SHADER]);
        b.raw(op::SHADER_SOURCE, &[1, bad.len() as u32], bad);
        b.cmd(op::COMPILE_SHADER, &[1]);
        b.cmd(op::GET_SHADER_IV, &[1, GL_COMPILE_STATUS]);
        b.cmd(op::GET_SHADER_INFO_LOG, &[1]);
        let (reply, status, _) = gl.run(b.bytes());
        assert_eq!(status, error::NONE);
        let r = reply.bytes();
        let mut at = 0;
        assert_eq!(read_u32(r, &mut at), 0, "it did not compile");
        let log = read_blob(r, &mut at);
        assert!(!log.is_empty(), "and the driver said why");
    }

    #[test]
    fn an_unknown_opcode_stops_the_batch_rather_than_being_skipped() {
        let mut gl = match Gl::new(16, 16) {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut b = Batch::new();
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        b.cmd(9999, &[]);
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        let (_, status, ran) = gl.run(b.bytes());
        assert_eq!(status, error::BAD_OPCODE);
        assert_eq!(ran, 1, "it stopped where it stopped understanding");
    }

    /// Read a 16x16 RGBA readback's pixel (bottom-up rows).
    fn pixel16(pixels: &[u8], x: usize, y: usize) -> (u8, u8, u8, u8) {
        let o = (y * 16 + x) * 4;
        (pixels[o], pixels[o + 1], pixels[o + 2], pixels[o + 3])
    }

    fn read_back(b: &mut Batch) {
        b.cmd(op::READ_PIXELS, &[0, 0, 16, 16, GL_RGBA, GL_UNSIGNED_BYTE, 0, 0, 0]);
    }

    /// A guest asking for desktop GL gets a compatibility context: immediate
    /// mode, the matrix stack and display lists all run.
    #[test]
    fn a_desktop_context_runs_fixed_function() {
        let mut gl = match Gl::new(16, 16) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no host GL ({e})");
                return;
            }
        };
        const GL_PROJECTION: u32 = 0x1701;
        const GL_MODELVIEW: u32 = 0x1700;
        const GL_COMPILE: u32 = 0x1300;
        let mut b = Batch::new();
        b.cmd(op::CONTEXT_CREATE, &[client_api::DESKTOP, 2, 1]);
        let (reply, status, _) = gl.run(b.bytes());
        assert_eq!(status, error::NONE);
        if !gl.is_desktop() {
            eprintln!("skipping: the host has no desktop OpenGL");
            return;
        }
        let r = reply.bytes();
        let mut at = 0;
        let _vendor = read_blob(r, &mut at);
        let _renderer = read_blob(r, &mut at);
        let version = String::from_utf8(read_blob(r, &mut at)).unwrap();
        assert!(!version.starts_with("OpenGL ES"), "a desktop version: {version}");

        let mut b = Batch::new();
        b.cmd(op::MATRIX_MODE, &[GL_PROJECTION]);
        b.cmd(op::LOAD_IDENTITY, &[]);
        b.cmd(op::MATRIX_MODE, &[GL_MODELVIEW]);
        b.cmd(op::LOAD_IDENTITY, &[]);
        b.cmd(op::CLEAR_COLOR, &[0, 0, 0, 1.0f32.to_bits()]);
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        // The left half, compiled into a list and called; the current color
        // is white.
        b.cmd(op::GEN_LISTS, &[1]);
        b.cmd(op::NEW_LIST, &[1, GL_COMPILE]);
        b.cmd(op::BEGIN, &[GL_TRIANGLES]);
        for (x, y) in [(-1.0f32, -1.0f32), (0.0, -1.0), (0.0, 1.0), (-1.0, -1.0), (0.0, 1.0), (-1.0, 1.0)] {
            b.cmd(op::VERTEX3F, &[x.to_bits(), y.to_bits(), 0]);
        }
        b.cmd(op::END, &[]);
        b.cmd(op::END_LIST, &[]);
        b.cmd(op::CALL_LIST, &[1]);
        read_back(&mut b);
        b.cmd(op::GET_ERROR, &[]);
        let (reply, status, _) = gl.run(b.bytes());
        assert_eq!(status, error::NONE, "every command was understood");
        let r = reply.bytes();
        let mut at = 0;
        let list = read_u32(r, &mut at);
        assert_ne!(list, 0, "a display list was made");
        let _offset = read_u32(r, &mut at);
        let px = read_blob(r, &mut at);
        assert_eq!(read_u32(r, &mut at), 0, "GL reported no error");
        assert_eq!(pixel16(&px, 3, 8), (255, 255, 255, 255), "inside the quad");
        assert_eq!(pixel16(&px, 12, 8), (0, 0, 0, 255), "outside it");
    }

    /// Rendering into a texture through a framebuffer object, with names the
    /// guest chose.
    #[test]
    fn a_framebuffer_object_renders_to_a_texture() {
        let mut gl = match Gl::new(16, 16) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no host GL ({e})");
                return;
            }
        };
        const GL_TEXTURE_2D: u32 = 0x0DE1;
        const GL_FRAMEBUFFER: u32 = 0x8D40;
        const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
        const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
        let mut b = Batch::new();
        b.cmd(op::GEN_TEXTURES, &[1, 5]);
        b.cmd(op::BIND_TEXTURE, &[GL_TEXTURE_2D, 5]);
        b.raw(op::TEX_IMAGE_2D, &[GL_TEXTURE_2D, 0, GL_RGBA, 16, 16, 0, GL_RGBA, GL_UNSIGNED_BYTE, 0, 0, 0, 0], &[]);
        b.cmd(op::GEN_FRAMEBUFFERS, &[1, 6]);
        b.cmd(op::BIND_FRAMEBUFFER, &[GL_FRAMEBUFFER, 6]);
        b.cmd(op::FRAMEBUFFER_TEXTURE_2D, &[GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 5, 0]);
        b.cmd(op::CHECK_FRAMEBUFFER_STATUS, &[GL_FRAMEBUFFER]);
        b.cmd(op::VIEWPORT, &[0, 0, 16, 16]);
        b.cmd(op::CLEAR_COLOR, &[1.0f32.to_bits(), 0, 0, 1.0f32.to_bits()]);
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        read_back(&mut b);
        b.cmd(op::BIND_FRAMEBUFFER, &[GL_FRAMEBUFFER, 0]);
        b.cmd(op::DELETE_FRAMEBUFFERS, &[1, 6]);
        b.cmd(op::GET_ERROR, &[]);
        let (reply, status, _) = gl.run(b.bytes());
        assert_eq!(status, error::NONE);
        let r = reply.bytes();
        let mut at = 0;
        assert_eq!(read_u32(r, &mut at), GL_FRAMEBUFFER_COMPLETE);
        let _offset = read_u32(r, &mut at);
        let px = read_blob(r, &mut at);
        assert_eq!(read_u32(r, &mut at), 0, "GL reported no error");
        assert_eq!(pixel16(&px, 7, 7), (255, 0, 0, 255), "the clear went to the texture");
    }

    /// Protocol 11: client arrays from a buffer, an orthographic projection
    /// and scaling, a loaded matrix, the current color and the alpha test.
    #[test]
    fn protocol_11_client_arrays_and_fixed_function_matrices() {
        let mut gl = match Gl::new(16, 16) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no host GL ({e})");
                return;
            }
        };
        const GL_PROJECTION: u32 = 0x1701;
        const GL_MODELVIEW: u32 = 0x1700;
        const GL_VERTEX_ARRAY: u32 = 0x8074;
        const GL_COLOR_ARRAY: u32 = 0x8076;
        const GL_TEXTURE_COORD_ARRAY: u32 = 0x8078;
        const GL_TEXTURE0: u32 = 0x84C0;
        const GL_TEXTURE_ENV: u32 = 0x2300;
        const GL_TEXTURE_ENV_MODE: u32 = 0x2200;
        const GL_MODULATE: u32 = 0x2100;
        const GL_ALPHA_TEST: u32 = 0x0BC0;
        const GL_GREATER: u32 = 0x0204;
        const GL_QUADS: u32 = 0x0007;
        let mut b = Batch::new();
        b.cmd(op::CONTEXT_CREATE, &[client_api::DESKTOP, 2, 1]);
        let _ = gl.run(b.bytes());
        if !gl.is_desktop() {
            eprintln!("skipping: the host has no desktop OpenGL");
            return;
        }
        let f = |v: f32| v.to_bits();
        let d = |v: f64| Batch::words64(v.to_bits());
        let mut ortho = Vec::new();
        for v in [0.0, 16.0, 0.0, 16.0, -1.0, 1.0] {
            ortho.extend_from_slice(&d(v));
        }
        // A quad over x 0..4 (scaled by 2 to 0..8), all rows, in red.
        let mut bytes = Vec::new();
        for (x, y) in [(0.0f32, 0.0f32), (4.0, 0.0), (4.0, 16.0), (0.0, 16.0)] {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
        }
        for _ in 0..4 {
            bytes.extend_from_slice(&[255, 0, 0, 255]);
        }
        let mut b = Batch::new();
        b.cmd(op::MATRIX_MODE, &[GL_PROJECTION]);
        b.cmd(op::LOAD_IDENTITY, &[]);
        b.cmd(op::ORTHO, &ortho);
        b.cmd(op::MATRIX_MODE, &[GL_MODELVIEW]);
        b.cmd(op::LOAD_IDENTITY, &[]);
        b.cmd(op::SCALEF, &[f(2.0), f(1.0), f(1.0)]);
        b.cmd(op::CLEAR_COLOR, &[0, 0, 0, f(1.0)]);
        b.cmd(op::CLEAR, &[GL_COLOR_BUFFER_BIT]);
        b.cmd(op::GEN_BUFFERS, &[1, 9]);
        b.cmd(op::BIND_BUFFER, &[GL_ARRAY_BUFFER, 9]);
        b.raw(op::BUFFER_DATA, &[GL_ARRAY_BUFFER, GL_STATIC_DRAW, bytes.len() as u32, 0, 1, bytes.len() as u32], &bytes);
        b.cmd(op::ENABLE_CLIENT_STATE, &[GL_VERTEX_ARRAY]);
        b.cmd(op::ENABLE_CLIENT_STATE, &[GL_COLOR_ARRAY]);
        b.cmd(op::VERTEX_POINTER, &[2, GL_FLOAT, 8, 0, 0]);
        b.cmd(op::COLOR_POINTER, &[4, GL_UNSIGNED_BYTE, 4, 32, 0]);
        b.cmd(op::CLIENT_ACTIVE_TEXTURE, &[GL_TEXTURE0]);
        b.cmd(op::TEX_COORD_POINTER, &[2, GL_FLOAT, 8, 0, 0]);
        b.cmd(op::DISABLE_CLIENT_STATE, &[GL_TEXTURE_COORD_ARRAY]);
        b.cmd(op::DRAW_ARRAYS, &[GL_QUADS, 0, 4]);
        b.cmd(op::DISABLE_CLIENT_STATE, &[GL_COLOR_ARRAY]);
        b.cmd(op::DISABLE_CLIENT_STATE, &[GL_VERTEX_ARRAY]);
        // Immediate mode in the current color, placed by a loaded matrix:
        // the identity translated by +12 in x, then multiplied by identity.
        let mut m = [0f32; 16];
        m[0] = 1.0;
        m[5] = 1.0;
        m[10] = 1.0;
        m[15] = 1.0;
        let ident: Vec<u32> = m.iter().map(|v| v.to_bits()).collect();
        m[12] = 12.0;
        let moved: Vec<u32> = m.iter().map(|v| v.to_bits()).collect();
        b.cmd(op::LOAD_MATRIXF, &moved);
        b.cmd(op::MULT_MATRIXF, &ident);
        b.cmd(op::TEX_ENVI, &[GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, GL_MODULATE]);
        b.cmd(op::COLOR4F, &[0, f(1.0), 0, f(1.0)]);
        b.cmd(op::BEGIN, &[GL_QUADS]);
        for (x, y) in [(0.0f32, 0.0f32), (4.0, 0.0), (4.0, 16.0), (0.0, 16.0)] {
            b.cmd(op::VERTEX3F, &[f(x), f(y), 0]);
        }
        b.cmd(op::END, &[]);
        // The alpha test drops a translucent quad drawn over everything.
        b.cmd(op::LOAD_IDENTITY, &[]);
        b.cmd(op::ENABLE, &[GL_ALPHA_TEST]);
        b.cmd(op::ALPHA_FUNC, &[GL_GREATER, f(0.5)]);
        b.cmd(op::COLOR4F, &[f(1.0), f(1.0), f(1.0), f(0.25)]);
        b.cmd(op::BEGIN, &[GL_QUADS]);
        for (x, y) in [(0.0f32, 0.0f32), (16.0, 0.0), (16.0, 16.0), (0.0, 16.0)] {
            b.cmd(op::VERTEX3F, &[f(x), f(y), 0]);
        }
        b.cmd(op::END, &[]);
        read_back(&mut b);
        b.cmd(op::GET_ERROR, &[]);
        let (reply, status, _) = gl.run(b.bytes());
        assert_eq!(status, error::NONE, "every command was understood");
        let r = reply.bytes();
        let mut at = 0;
        let _offset = read_u32(r, &mut at);
        let px = read_blob(r, &mut at);
        assert_eq!(read_u32(r, &mut at), 0, "GL reported no error");
        assert_eq!(pixel16(&px, 3, 8), (255, 0, 0, 255), "the client-array quad, scaled");
        assert_eq!(pixel16(&px, 9, 8), (0, 0, 0, 255), "nothing between");
        assert_eq!(pixel16(&px, 14, 8), (0, 255, 0, 255), "the immediate quad, moved by the loaded matrix");
    }
}
