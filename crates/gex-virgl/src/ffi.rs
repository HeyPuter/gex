//! The parts of `virglrenderer.h` gex uses.
//!
//! Hand-written rather than bindgen-generated: about twenty functions and three
//! structs of stable, carefully-versioned ABI.
//!
//! The callbacks struct is declared through the version-4 fields (`get_egl_display`
//! and friends), which the library exports only when built with
//! `VIRGL_RENDERER_UNSTABLE_APIS` -- see [`VIRGL_RENDERER_CALLBACKS_VERSION`].
//! Declaring the struct longer than the library expects is harmless; shorter is
//! not.

#[allow(unused_imports)]
use std::os::raw::{c_char, c_int, c_uint, c_void};

pub type VirglGlContext = *mut c_void;

#[repr(C)]
pub struct VirglGlCtxParam {
    pub version: c_int,
    pub shared: bool,
    pub major_ver: c_int,
    pub minor_ver: c_int,
}

#[repr(C)]
pub struct VirglRendererCallbacks {
    pub version: c_int,
    pub write_fence: Option<unsafe extern "C" fn(cookie: *mut c_void, fence: u32)>,
    pub create_gl_context: Option<
        unsafe extern "C" fn(
            cookie: *mut c_void,
            scanout_idx: c_int,
            param: *mut VirglGlCtxParam,
        ) -> VirglGlContext,
    >,
    pub destroy_gl_context: Option<unsafe extern "C" fn(cookie: *mut c_void, ctx: VirglGlContext)>,
    pub make_current: Option<
        unsafe extern "C" fn(cookie: *mut c_void, scanout_idx: c_int, ctx: VirglGlContext) -> c_int,
    >,
    pub get_drm_fd: Option<unsafe extern "C" fn(cookie: *mut c_void) -> c_int>,
    // Version 3 and 4 fields, present only when built with
    // `VIRGL_RENDERER_UNSTABLE_APIS` (as Debian does).
    pub write_context_fence:
        Option<unsafe extern "C" fn(cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64)>,
    pub get_server_fd: Option<unsafe extern "C" fn(cookie: *mut c_void, version: u32) -> c_int>,
    pub get_egl_display: Option<unsafe extern "C" fn(cookie: *mut c_void) -> *mut c_void>,
}

/// Must be 4: virglrenderer takes the caller's EGL display (the external-winsys
/// path) only when the struct says version 4 *and* `get_egl_display` is set.
/// Any lower and the winsys block is silently skipped -- `virgl_renderer_init`
/// still succeeds, then the first `virgl_renderer_context_create` segfaults.
pub const VIRGL_RENDERER_CALLBACKS_VERSION: c_int = 4;

#[repr(C)]
#[derive(Default)]
pub struct VirglResourceCreateArgs {
    pub handle: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// The same layout as `struct pipe_box`, which is what a transfer's region is.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct VirglBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[repr(C)]
pub struct IoVec {
    pub iov_base: *mut c_void,
    pub iov_len: usize,
}

#[cfg(have_virgl)]
extern "C" {
    pub fn virgl_renderer_init(
        cookie: *mut c_void,
        flags: c_int,
        cb: *mut VirglRendererCallbacks,
    ) -> c_int;
    pub fn virgl_renderer_cleanup(cookie: *mut c_void);
    pub fn virgl_renderer_poll();
    pub fn virgl_renderer_get_poll_fd() -> c_int;

    pub fn virgl_renderer_context_create(
        handle: u32,
        nlen: u32,
        name: *const c_char,
    ) -> c_int;
    pub fn virgl_renderer_context_destroy(handle: u32);

    pub fn virgl_renderer_resource_create(
        args: *mut VirglResourceCreateArgs,
        iov: *mut IoVec,
        num_iovs: u32,
    ) -> c_int;
    pub fn virgl_renderer_resource_unref(res_handle: u32);
    pub fn virgl_renderer_resource_attach_iov(
        res_handle: c_int,
        iov: *mut IoVec,
        num_iovs: c_int,
    ) -> c_int;
    pub fn virgl_renderer_resource_detach_iov(
        res_handle: c_int,
        iov: *mut *mut IoVec,
        num_iovs: *mut c_int,
    );
    pub fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
    pub fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);

    pub fn virgl_renderer_submit_cmd(
        buffer: *mut c_void,
        ctx_id: c_int,
        ndw: c_int,
    ) -> c_int;

    pub fn virgl_renderer_transfer_read_iov(
        handle: u32,
        ctx_id: u32,
        level: u32,
        stride: u32,
        layer_stride: u32,
        box_: *mut VirglBox,
        offset: u64,
        iov: *mut IoVec,
        iovec_cnt: c_int,
    ) -> c_int;
    pub fn virgl_renderer_transfer_write_iov(
        handle: u32,
        ctx_id: u32,
        level: c_int,
        stride: u32,
        layer_stride: u32,
        box_: *mut VirglBox,
        offset: u64,
        iovec: *mut IoVec,
        iovec_cnt: c_uint,
    ) -> c_int;

    pub fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
    pub fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);

    pub fn virgl_renderer_create_fence(client_fence_id: c_int, ctx_id: u32) -> c_int;
    pub fn virgl_renderer_force_ctx_0();
}
