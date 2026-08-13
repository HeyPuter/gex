//! The surfaceless EGL display and contexts gex hands to virglrenderer.
//!
//! virglrenderer's own DRM and `VIRGL_RENDERER_USE_SURFACELESS` paths both want
//! a device a sandbox may lack (no openable `/dev/dri/card0`), so gex supplies
//! the winsys instead. Nothing is drawn to a surface: virglrenderer renders
//! into textures it owns and pixels leave through a transfer, not a swap.

use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

/// Narrate what virglrenderer asks the winsys for.
fn tracing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_VIRGL_TRACE").is_some())
}

pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLSurface = *mut c_void;

#[allow(non_snake_case)]
extern "C" {
    fn eglGetProcAddress(name: *const u8) -> *mut c_void;
    fn eglInitialize(dpy: EGLDisplay, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(
        dpy: EGLDisplay,
        attrs: *const i32,
        configs: *mut EGLConfig,
        size: i32,
        num: *mut i32,
    ) -> u32;
    fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share: EGLContext,
        attrs: *const i32,
    ) -> EGLContext;
    fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> u32;
    fn eglMakeCurrent(dpy: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> u32;
    fn eglGetError() -> i32;
}

const EGL_NONE: i32 = 0x3038;
const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_BIT: i32 = 0x0008;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 0x0001;
const EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT: i32 = 0x0002;

/// A surfaceless EGL display and the contexts made on it.
pub struct Winsys {
    display: EGLDisplay,
    config: EGLConfig,
    /// The first context made. Later shared contexts share with it, because
    /// virglrenderer expects its sub-contexts to see each other's objects.
    root: EGLContext,
    /// Whether the display gave us desktop GL rather than GLES.
    pub desktop_gl: bool,
    /// The context virglrenderer last asked for. An EGL context is current per
    /// thread, and the renderer is pumped from whichever host thread the guest
    /// runs on, so it must be rebound there before use.
    last: EGLContext,
}

// The renderer is driven from one thread at a time; the winsys goes with it.
unsafe impl Send for Winsys {}

impl Winsys {
    /// Bring up surfaceless EGL, preferring desktop GL.
    ///
    /// Desktop first because virglrenderer's GL backend is the complete one;
    /// its GLES path only emulates what GLES lacks.
    pub fn new() -> Result<Winsys, String> {
        unsafe {
            let p = eglGetProcAddress(b"eglGetPlatformDisplayEXT\0".as_ptr());
            if p.is_null() {
                return Err("this EGL has no eglGetPlatformDisplayEXT".into());
            }
            let get_platform_display: unsafe extern "C" fn(
                u32,
                *mut c_void,
                *const isize,
            ) -> EGLDisplay = std::mem::transmute(p);

            let display =
                get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, ptr::null_mut(), ptr::null());
            if display.is_null() {
                return Err("no surfaceless EGL display".into());
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("eglInitialize failed: {:#x}", eglGetError()));
            }

            // Desktop unless `GEX_VIRGL_GLES` says otherwise. The override is
            // for recording: a stream is emitted against what the host offers,
            // so forcing GLES records one the browser's WebGL 2 could replay.
            let want_gles = std::env::var_os("GEX_VIRGL_GLES").is_some();
            let desktop_gl = !want_gles && eglBindAPI(EGL_OPENGL_API) != 0;
            if !desktop_gl && eglBindAPI(EGL_OPENGL_ES_API) == 0 {
                return Err("EGL offers neither OpenGL nor OpenGL ES".into());
            }

            let attrs = [
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                if desktop_gl { EGL_OPENGL_BIT } else { EGL_OPENGL_ES2_BIT },
                EGL_NONE,
            ];
            let mut config: EGLConfig = ptr::null_mut();
            let mut n = 0;
            if eglChooseConfig(display, attrs.as_ptr(), &mut config, 1, &mut n) == 0 || n == 0 {
                return Err("no usable EGL config".into());
            }

            Ok(Winsys {
                display,
                config,
                root: ptr::null_mut(),
                desktop_gl,
                last: ptr::null_mut(),
            })
        }
    }

    /// The display itself, which virglrenderer asks for so it can build its
    /// own state around the same one the contexts come from.
    pub fn display(&self) -> EGLDisplay {
        self.display
    }

    /// Make a context of the version virglrenderer asked for.
    pub fn create_context(&mut self, major: i32, minor: i32, shared: bool) -> EGLContext {
        unsafe {
            // Rebound every time: the API a context gets is whatever is bound
            // at creation, and virglrenderer may have bound the other one in
            // between.
            eglBindAPI(if self.desktop_gl {
                EGL_OPENGL_API
            } else {
                EGL_OPENGL_ES_API
            });
            let mut attrs = vec![
                EGL_CONTEXT_MAJOR_VERSION,
                major,
                EGL_CONTEXT_MINOR_VERSION,
                minor,
            ];
            if self.desktop_gl {
                // Core from 3.2 up, compatibility below: Mesa refuses a core
                // profile below 3.2.
                attrs.push(EGL_CONTEXT_OPENGL_PROFILE_MASK);
                attrs.push(if (major, minor) >= (3, 2) {
                    EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT
                } else {
                    EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT
                });
            }
            attrs.push(EGL_NONE);

            let share = if shared { self.root } else { ptr::null_mut() };
            let ctx = eglCreateContext(self.display, self.config, share, attrs.as_ptr());
            if tracing() {
                eprintln!(
                    "[virgl] create_context {major}.{minor} shared={shared} -> {ctx:?} (egl {:#x})",
                    eglGetError()
                );
            }
            if !ctx.is_null() && self.root.is_null() {
                self.root = ctx;
            }
            ctx
        }
    }

    pub fn destroy_context(&mut self, ctx: EGLContext) {
        unsafe {
            if ctx == self.root {
                self.root = ptr::null_mut();
            }
            eglDestroyContext(self.display, ctx);
        }
    }

    /// Re-bind the last context on the calling thread. Cheap when already
    /// current, and the only thing between a multi-process guest and a segfault
    /// inside the driver.
    pub fn rebind(&self) {
        if self.last.is_null() {
            return;
        }
        unsafe {
            eglBindAPI(if self.desktop_gl {
                EGL_OPENGL_API
            } else {
                EGL_OPENGL_ES_API
            });
            eglMakeCurrent(self.display, ptr::null_mut(), ptr::null_mut(), self.last);
        }
    }

    /// Let go of the context so another thread can take it.
    ///
    /// A context is current on exactly one thread; binding it on a second while
    /// the first holds it fails with `BAD_ACCESS`. So work brackets itself:
    /// take it, use it, put it back.
    pub fn release(&self) {
        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
    }

    pub fn make_current(&mut self, ctx: EGLContext) -> c_int {
        self.last = ctx;
        unsafe {
            let ok = eglMakeCurrent(self.display, ptr::null_mut(), ptr::null_mut(), ctx);
            if tracing() {
                eprintln!("[virgl] make_current {ctx:?} -> {ok} (egl {:#x})", eglGetError());
            }
            if ok != 0 {
                0
            } else {
                -1
            }
        }
    }
}
