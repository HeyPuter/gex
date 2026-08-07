//! Unmodified Mesa in the guest, rendering on the host's GPU.
//!
//! The other graphics path in gex -- `gex-gl` and the guest's `gexgl` driver
//! -- marshals GLES calls one for one. It is fast, and it only works for
//! programs that speak GLES, against a driver gex ships. This one inverts
//! that: the guest runs **stock Mesa** with its virgl Gallium driver, and what
//! crosses the boundary is Mesa's own command stream.
//!
//! That difference is the whole point. Mesa's state tracker runs inside the
//! guest, so a program using desktop OpenGL -- immediate mode, display lists,
//! fixed-function lighting, all of it -- has already been lowered to shaders
//! and vertex buffers before a single byte leaves. gex never implements
//! `glBegin`. glxgears works because Mesa makes it work, and glxgears is a
//! binary nobody here compiled.
//!
//! The cost is honest and should not be hidden: Mesa itself now runs as
//! translated guest code, which is far more work per frame than a marshaller
//! that memcpys into a ring. This path is the compatible one, not the fast
//! one. It is worth having because the alternative for those programs is
//! llvmpipe rasterising on the same translated processor, which is slower
//! still by a wide margin.
//!
//! # Shape
//!
//! - [`Virgl`] owns the host renderer. virglrenderer keeps process-global
//!   state, so there is one of these.
//! - [`vtest::Session`] is one connected guest client, parsing its stream.
//! - [`egl`] is the winsys handed to virglrenderer, because asking it to open
//!   a DRM node fails on a machine that has none.

pub mod egl;
pub mod ffi;
pub mod vtest;

#[allow(unused_imports)]
use std::os::raw::{c_int, c_void};

/// Let virglrenderer own the winsys. See the note in `Virgl::new`.
#[allow(dead_code)]
const VIRGL_RENDERER_USE_EGL: c_int = 1;
/// ...and bring it up without a device, which is the only option here: this
/// machine cannot even open `/dev/dri/card0`. Without this, `USE_EGL` insists
/// on a render node and initialisation fails outright.
#[allow(dead_code)]
const VIRGL_RENDERER_USE_SURFACELESS: c_int = 1 << 3;

/// The host renderer.
pub struct Virgl {
    /// Handed to virglrenderer as its cookie and reached again in the
    /// callbacks. Leaked deliberately: virglrenderer holds the pointer for as
    /// long as the process renders, and there is exactly one.
    winsys: &'static mut egl::Winsys,
    next_ctx: u32,
    /// Resources the guest has made, so they can be detached on the way out.
    /// virglrenderer numbers them; this only tracks what is live.
    live: Vec<u32>,
}

/// Whether this build can render at all.
///
/// virglrenderer is a system library, and a machine without it still has to
/// build and run everything else gex does.
pub const AVAILABLE: bool = cfg!(have_virgl);

#[cfg(have_virgl)]
mod callbacks {
    use super::*;

    /// The cookie is the winsys. virglrenderer passes back whatever it was
    /// given, and it is only ever given this.
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
        // Every command this server runs has finished before its reply is
        // written, so a fence has nothing left to signal. When the DRM path
        // lands and submission becomes asynchronous, this grows a body.
    }

    /// Which DRM device to render on.
    ///
    /// Minus one means "find one yourself, and if there is none, be
    /// surfaceless". That is the honest answer here: gex does not own a
    /// device, and a sandbox frequently has no render node at all -- this
    /// machine's `/dev/dri/card0` is not even openable. virglrenderer copes,
    /// falling back to surfaceless EGL and whatever Mesa can do in software.
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

        // All five callbacks, and version 4. virglrenderer takes the caller's
        // EGL display only when the structure claims version 4 *and* every
        // one of `get_egl_display`, `create_gl_context`, `destroy_gl_context`
        // and `make_current` is set. Miss any of them and the winsys block is
        // skipped silently -- `virgl_renderer_init` returns success having
        // initialised no winsys, and the first context creation segfaults.
        //
        // Flags are zero on purpose. `USE_EGL` would send it off to open a
        // DRM node instead, and `USE_SURFACELESS` alongside it does not
        // rescue a machine with no render node: both were tried, and both
        // failed before this worked.
        // Leaked, like the winsys, and for a sharper reason: virglrenderer
        // stores the *pointer* to this structure and dereferences it on every
        // later call -- `state.cbs->create_gl_context` and the rest. A local
        // stays valid for the whole of `virgl_renderer_init`, which is why
        // leaving it on the stack looked like it worked: the renderer came up,
        // reported GL 4.5, filled its capability set. Then this function
        // returned, the frame went away, and the first
        // `virgl_renderer_context_create` read a dead stack.
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

    /// Make the renderer's GL context current on the calling thread.
    ///
    /// Call before handing it any work. gex has no thread of its own for the
    /// renderer -- the work is done by whichever guest process is running --
    /// and EGL binds a context per thread.
    pub fn bind_here(&self) {
        self.winsys.rebind();
    }

    /// Give the context back, so another thread may bind it.
    pub fn unbind(&self) {
        self.winsys.release();
    }

    /// A line for the log. virglrenderer prints its own GL version on the way
    /// up, which is the part that answers "is this actually accelerated".
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

    // Without the library none of the above can be called, but the callers
    // are compiled either way; these keep them compiling.
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
