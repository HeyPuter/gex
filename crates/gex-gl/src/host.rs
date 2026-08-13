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

/// Guest object names to the host's, for the kinds GL insists on naming.
#[derive(Default)]
struct Names {
    shaders: HashMap<u32, GLuint>,
    programs: HashMap<u32, GLuint>,
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
    width: i32,
    height: i32,
    /// Which thread the context is current on, so it is only moved when needed.
    current_on: Option<std::thread::ThreadId>,
    /// Read once at startup; the answer never changes.
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
                current_on: Some(std::thread::current().id()),
                width,
                height,
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
                    let (Some(target), Some(usage), Some(data)) = (a.u32(), a.u32(), a.blob())
                    else {
                        return true;
                    };
                    glBufferData(
                        target,
                        data.len() as isize,
                        if data.is_empty() {
                            std::ptr::null()
                        } else {
                            data.as_ptr() as *const c_void
                        },
                        usage,
                    );
                }
                op::BUFFER_SUB_DATA => {
                    let (Some(target), Some(offset), Some(data)) = (a.u32(), a.u32(), a.blob())
                    else {
                        return true;
                    };
                    glBufferSubData(
                        target,
                        offset as isize,
                        data.len() as isize,
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
                        if data.is_empty() {
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
                        data.as_ptr() as *const c_void,
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
                    let (Some(x), Some(y), Some(w), Some(h), Some(format), Some(ty)) =
                        (a.i32(), a.i32(), a.i32(), a.i32(), a.u32(), a.u32())
                    else {
                        return true;
                    };
                    // Four bytes a pixel: RGBA/UNSIGNED_BYTE is the only combo
                    // GLES2 guarantees, and this is large enough for it.
                    let n = (w.max(0) as usize) * (h.max(0) as usize) * 4;
                    let mut px = vec![0u8; n];
                    glReadPixels(x, y, w, h, format, ty, px.as_mut_ptr() as *mut c_void);
                    reply.blob(&px);
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
        reserved: w(7),
    })
}

impl Header {
    pub const SIZE: usize = 32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{op, Batch};

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
        b.cmd(op::CONTEXT_CREATE, &[]);
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
            &[GL_ARRAY_BUFFER, GL_STATIC_DRAW, bytes.len() as u32],
            &bytes,
        );
        b.cmd(op::ENABLE_VERTEX_ATTRIB_ARRAY, &[0]);
        b.cmd(op::VERTEX_ATTRIB_POINTER, &[0, 2, GL_FLOAT, 0, 0, 0]);
        b.cmd(op::DRAW_ARRAYS, &[GL_TRIANGLES, 0, 3]);
        b.cmd(op::FINISH, &[]);
        b.cmd(op::READ_PIXELS, &[0, 0, 64, 64, GL_RGBA, GL_UNSIGNED_BYTE]);
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
}
