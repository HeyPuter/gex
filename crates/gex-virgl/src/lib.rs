//! Unmodified Mesa in the guest, rendering on the host's GPU.
//!
//! Unlike the `gex-gl` path (a GLES marshaller against a driver gex ships),
//! here the guest runs **stock Mesa** with its virgl Gallium driver and Mesa's
//! own command stream crosses the boundary. Mesa's state tracker runs in the
//! guest, so desktop OpenGL -- immediate mode, display lists, all of it -- is
//! already lowered to shaders and buffers before a byte leaves; gex never
//! implements `glBegin`. The cost: Mesa runs as translated guest code, so this
//! is the compatible path, not the fast one (the alternative is llvmpipe on the
//! same translated CPU, slower still).
//!
//! # Shape
//!
//! - [`Virgl`] owns the host renderer; virglrenderer's state is process-global,
//!   so there is one.
//! - [`vtest::Session`] is one connected guest client, parsing its stream.
//! - [`egl`] is the winsys handed to virglrenderer.

pub mod egl;
pub mod ffi;
pub mod vtest;

#[allow(unused_imports)]
use std::os::raw::{c_int, c_void};

/// Let virglrenderer own the winsys. Unused: gex supplies its own (flags 0).
#[allow(dead_code)]
const VIRGL_RENDERER_USE_EGL: c_int = 1;
/// Bring up EGL without a device. Unused for the same reason.
#[allow(dead_code)]
const VIRGL_RENDERER_USE_SURFACELESS: c_int = 1 << 3;

/// The host renderer.
pub struct Virgl {
    /// The renderer's cookie, reached again in the callbacks. Leaked: the
    /// renderer holds the pointer for the life of the process, and there is one.
    winsys: &'static mut egl::Winsys,
    next_ctx: u32,
    /// Live resource handles, so they can be detached on the way out.
    live: Vec<u32>,
}

/// Whether this build was linked against libvirglrenderer.
pub const AVAILABLE: bool = cfg!(have_virgl);

#[cfg(have_virgl)]
mod callbacks {
    use super::*;

    /// The cookie is the winsys; it is the only thing ever passed as one.
    unsafe fn winsys<'a>(cookie: *mut c_void) -> &'a mut egl::Winsys {
        &mut *(cookie as *mut egl::Winsys)
    }

    pub unsafe extern "C" fn create_gl_context(
        cookie: *mut c_void,
        _scanout: c_int,
        param: *mut ffi::VirglGlCtxParam,
    ) -> ffi::VirglGlContext {
        let p = &*param;
        winsys(cookie).create_context(p.major_ver, p.minor_ver, p.shared) as ffi::VirglGlContext
    }

    pub unsafe extern "C" fn destroy_gl_context(cookie: *mut c_void, ctx: ffi::VirglGlContext) {
        winsys(cookie).destroy_context(ctx as egl::EGLContext);
    }

    pub unsafe extern "C" fn make_current(
        cookie: *mut c_void,
        _scanout: c_int,
        ctx: ffi::VirglGlContext,
    ) -> c_int {
        winsys(cookie).make_current(ctx as egl::EGLContext)
    }

    /// The display the contexts above are made on.
    pub unsafe extern "C" fn get_egl_display(cookie: *mut c_void) -> *mut c_void {
        winsys(cookie).display()
    }

    pub unsafe extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {
        // Submission is synchronous, so a fence has nothing left to signal.
        // Grows a body when the async DRM path lands.
    }

    /// Which DRM device to render on. -1 means "find one, else be surfaceless",
    /// which is what a sandbox with no render node needs.
    pub unsafe extern "C" fn get_drm_fd(_cookie: *mut c_void) -> c_int {
        -1
    }
}

impl Virgl {
    /// Bring up the host renderer, or say why not.
    #[cfg(have_virgl)]
    pub fn new() -> Result<Virgl, String> {
        let winsys = Box::leak(Box::new(egl::Winsys::new()?));
        let cookie = winsys as *mut egl::Winsys as *mut c_void;

        // Version 4 with every winsys callback set (`get_egl_display`,
        // `create_gl_context`, `destroy_gl_context`, `make_current`): miss any
        // and the winsys block is skipped silently, `virgl_renderer_init`
        // succeeds, and the first context creation segfaults. Flags 0: gex
        // supplies its own winsys rather than opening a DRM node.
        //
        // Leaked, because virglrenderer stores the *pointer* to this struct and
        // dereferences it on every later call. A stack local outlives only
        // `virgl_renderer_init`, then the first context_create reads dead stack.
        let cbs: &'static mut ffi::VirglRendererCallbacks = Box::leak(Box::new(
            ffi::VirglRendererCallbacks {
            version: ffi::VIRGL_RENDERER_CALLBACKS_VERSION,
            write_fence: Some(callbacks::write_fence),
            create_gl_context: Some(callbacks::create_gl_context),
            destroy_gl_context: Some(callbacks::destroy_gl_context),
            make_current: Some(callbacks::make_current),
            get_drm_fd: Some(callbacks::get_drm_fd),
            write_context_fence: None,
            get_server_fd: None,
            get_egl_display: Some(callbacks::get_egl_display),
            },
        ));
        let rc = unsafe { ffi::virgl_renderer_init(cookie, 0, cbs) };
        if rc != 0 {
            return Err(format!(
                "virgl_renderer_init failed: {rc} (a library built without \
                 VIRGL_RENDERER_UNSTABLE_APIS rejects callbacks version 4)"
            ));
        }
        Ok(Virgl {
            winsys,
            next_ctx: 1,
            live: Vec::new(),
        })
    }

    #[cfg(not(have_virgl))]
    pub fn new() -> Result<Virgl, String> {
        Err("this build has no libvirglrenderer".into())
    }

    /// Make the renderer's GL context current on the calling thread, before
    /// handing it any work: the renderer runs on whichever guest thread is
    /// active, and EGL binds a context per thread.
    pub fn bind_here(&self) {
        self.winsys.rebind();
    }

    /// Give the context back, so another thread may bind it.
    pub fn unbind(&self) {
        self.winsys.release();
    }

    /// A line for the log.
    pub fn describe(&self) -> String {
        format!(
            "virglrenderer on {}",
            if self.winsys.desktop_gl {
                "desktop OpenGL"
            } else {
                "OpenGL ES"
            }
        )
    }

    #[cfg(have_virgl)]
    pub fn context_create(&mut self, name: &str) -> Result<u32, String> {
        let id = self.next_ctx;
        self.next_ctx += 1;
        let rc = unsafe {
            ffi::virgl_renderer_context_create(id, name.len() as u32, name.as_ptr() as *const i8)
        };
        if rc != 0 {
            return Err(format!("virgl_renderer_context_create: {rc}"));
        }
        Ok(id)
    }

    #[cfg(have_virgl)]
    pub fn caps(&mut self, set: u32) -> Vec<u8> {
        let (mut max_ver, mut max_size) = (0u32, 0u32);
        unsafe { ffi::virgl_renderer_get_cap_set(set, &mut max_ver, &mut max_size) };
        if max_size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; max_size as usize];
        unsafe { ffi::virgl_renderer_fill_caps(set, 1, buf.as_mut_ptr() as *mut c_void) };
        buf
    }

    #[cfg(have_virgl)]
    pub fn resource_create(&mut self, ctx: u32, body: &[u8]) -> Result<(), String> {
        let g = |i: usize| u32::from_le_bytes(body[i * 4..i * 4 + 4].try_into().unwrap());
        let mut args = ffi::VirglResourceCreateArgs {
            handle: g(0),
            target: g(1),
            format: g(2),
            bind: g(3),
            width: g(4),
            height: g(5),
            depth: g(6),
            array_size: g(7),
            last_level: g(8),
            nr_samples: g(9),
            flags: 0,
        };
        let handle = args.handle;
        let rc =
            unsafe { ffi::virgl_renderer_resource_create(&mut args, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return Err(format!("virgl_renderer_resource_create: {rc}"));
        }
        unsafe { ffi::virgl_renderer_ctx_attach_resource(ctx as c_int, handle as c_int) };
        self.live.push(handle);
        Ok(())
    }

    #[cfg(have_virgl)]
    pub fn resource_unref(&mut self, ctx: u32, handle: u32) {
        unsafe {
            ffi::virgl_renderer_ctx_detach_resource(ctx as c_int, handle as c_int);
            ffi::virgl_renderer_resource_unref(handle);
        }
        self.live.retain(|&h| h != handle);
    }

    #[cfg(have_virgl)]
    pub fn submit_cmd(&mut self, ctx: u32, body: &[u8]) -> Result<(), String> {
        let mut buf = body.to_vec();
        let rc = unsafe {
            ffi::virgl_renderer_submit_cmd(
                buf.as_mut_ptr() as *mut c_void,
                ctx as c_int,
                (buf.len() / 4) as c_int,
            )
        };
        if rc != 0 {
            return Err(format!("virgl_renderer_submit_cmd: {rc}"));
        }
        Ok(())
    }

    #[cfg(have_virgl)]
    pub fn transfer_get(
        &mut self,
        ctx: u32,
        t: &vtest::Transfer,
        size: usize,
    ) -> Result<Vec<u8>, String> {
        let mut data = vec![0u8; size];
        let mut b = t.b;
        let mut iov = ffi::IoVec {
            iov_base: data.as_mut_ptr() as *mut c_void,
            iov_len: data.len(),
        };
        let rc = unsafe {
            ffi::virgl_renderer_transfer_read_iov(
                t.handle,
                ctx,
                t.level,
                t.stride,
                t.layer_stride,
                &mut b,
                t.offset,
                if size > 0 { &mut iov } else { std::ptr::null_mut() },
                if size > 0 { 1 } else { 0 },
            )
        };
        if rc != 0 {
            return Err(format!("virgl_renderer_transfer_read_iov: {rc}"));
        }
        Ok(data)
    }

    #[cfg(have_virgl)]
    pub fn transfer_put(
        &mut self,
        ctx: u32,
        t: &vtest::Transfer,
        data: &[u8],
    ) -> Result<(), String> {
        let mut buf = data.to_vec();
        let mut b = t.b;
        let mut iov = ffi::IoVec {
            iov_base: buf.as_mut_ptr() as *mut c_void,
            iov_len: buf.len(),
        };
        let rc = unsafe {
            ffi::virgl_renderer_transfer_write_iov(
                t.handle,
                ctx,
                t.level as c_int,
                t.stride,
                t.layer_stride,
                &mut b,
                t.offset,
                &mut iov,
                1,
            )
        };
        if rc != 0 {
            return Err(format!("virgl_renderer_transfer_write_iov: {rc}"));
        }
        Ok(())
    }

    #[cfg(have_virgl)]
    pub fn poll(&mut self) {
        unsafe { ffi::virgl_renderer_poll() };
    }

    // Stubs so callers compile without the library.
    #[cfg(not(have_virgl))]
    pub fn context_create(&mut self, _name: &str) -> Result<u32, String> {
        Err("no libvirglrenderer".into())
    }
    #[cfg(not(have_virgl))]
    pub fn caps(&mut self, _set: u32) -> Vec<u8> {
        Vec::new()
    }
    #[cfg(not(have_virgl))]
    pub fn resource_create(&mut self, _ctx: u32, _body: &[u8]) -> Result<(), String> {
        Err("no libvirglrenderer".into())
    }
    #[cfg(not(have_virgl))]
    pub fn resource_unref(&mut self, _ctx: u32, _handle: u32) {}
    #[cfg(not(have_virgl))]
    pub fn submit_cmd(&mut self, _ctx: u32, _body: &[u8]) -> Result<(), String> {
        Err("no libvirglrenderer".into())
    }
    #[cfg(not(have_virgl))]
    pub fn transfer_get(
        &mut self,
        _ctx: u32,
        _t: &vtest::Transfer,
        _size: usize,
    ) -> Result<Vec<u8>, String> {
        Err("no libvirglrenderer".into())
    }
    #[cfg(not(have_virgl))]
    pub fn transfer_put(
        &mut self,
        _ctx: u32,
        _t: &vtest::Transfer,
        _data: &[u8],
    ) -> Result<(), String> {
        Err("no libvirglrenderer".into())
    }
    #[cfg(not(have_virgl))]
    pub fn poll(&mut self) {}
}
