//! The guest's graphics device in a browser: `/dev/gex-gl`.
//!
//! Same protocol as the native device (the guest's `libGLESv2.so.2` is one
//! binary), executed here by Mesa (gex_wgpu over WebGPU). Page side is
//! `web/src/mesa-gl.js`. The import carries an address into guest memory rather than
//! a copy, so a frame costs one call out of wasm however many GL calls it holds.

use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::Arc;

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Run the batch whose header is at `ring` (an address in this module's
    /// memory). Returns zero, or negative when the page has no WebGL. The reply
    /// is written back into the buffer, not through the return value.
    fn gl_submit(ring: u32) -> i32;
    /// `gl_submit` without waiting for the batch to run: the page copies the
    /// ring and returns; the next submit waits for this one first.
    fn gl_submit_async(ring: u32) -> i32;
}

/// Set when the page said it has a canvas with WebGL on it. Off by default, so
/// a page without one has its guest see no graphics device (a clean `open`
/// failure) rather than one whose draw calls silently disappear.
static mut HAVE_GL: bool = false;

/// Say that the page has WebGL behind the import above.
///
/// # Safety
/// Called from JS before the guest runs, on the one thread a page has.
#[no_mangle]
pub extern "C" fn gex_set_gl(on: i32) {
    unsafe { HAVE_GL = on != 0 }
}

pub fn have_gl() -> bool {
    unsafe { HAVE_GL }
}

/// The same ioctl numbers the native device uses, for the same reason: one
/// guest library talks to both.
pub const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// `GEX_GL_SUBMIT`, not waited for (gex-kernel's NT OpenGL, for a frame's
/// last batch): one batch in flight at most.
pub const GEX_GL_SUBMIT_ASYNC: u64 = 0xc008_6704;
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;
/// `{ kind, id, width, height }` -- this process's window whose pixels the
/// host draws.
///
/// A client rendering through the host's GL attaches no buffer to its
/// `wl_surface`: there is nothing to attach, because its frames never come
/// back here. Without a buffer the compositor would never see the surface at
/// all, so the client says which surface it is and how big, and the
/// compositor finds the picture on the host. `kind` matches
/// `gex_gl::proto::surface`; an X drawable is not this device's business (the
/// window manager names those).
pub const GEX_GL_SURFACE: u64 = 0x4010_6703;

const MAX_BATCH: u32 = 32 * 1024 * 1024;

thread_local! {
    /// Nanoseconds this Worker has spent waiting on the page's GL host
    /// (`gl_submit`), for the NT host's per-thread time split.
    pub(crate) static GL_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The part of that spent waiting for the previous, unwaited batch
    /// (`GEX_GL_SUBMIT_ASYNC`) to finish: the page says, `gex_gl_note_wait_ns`.
    pub(crate) static GL_WAIT_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// From the page's GL bridge: how long a submit waited for the batch before
/// it (this Worker's thread).
#[no_mangle]
pub extern "C" fn gex_gl_note_wait_ns(ns: f64) {
    if ns > 0.0 {
        GL_WAIT_NS.with(|g| g.set(g.get() + ns as u64));
    }
}

#[derive(Debug)]
pub struct GlDevice;

impl GlDevice {
    pub fn new() -> Arc<GlDevice> {
        Arc::new(GlDevice)
    }
}

impl FileHandle for GlDevice {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 0,
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    fn ioctl(&self, m: &mut GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        // The request is an `int` in the C library, so every `_IOWR` number
        // arrives sign-extended.
        match request & 0xffff_ffff {
            GEX_GL_SUBMIT | GEX_GL_SUBMIT_ASYNC => {
                if !have_gl() {
                    return Some(Err(Errno::ENODEV));
                }
                // The address is checked here rather than in JS, because the
                // kernel is what knows how big the guest's memory is.
                if m.read(arg, 32).is_err() {
                    return Some(Err(Errno::EFAULT));
                }
                let t0 = crate::now_ns();
                let r = if request & 0xffff_ffff == GEX_GL_SUBMIT_ASYNC {
                    unsafe { gl_submit_async(arg as u32) }
                } else {
                    unsafe { gl_submit(arg as u32) }
                };
                GL_NS.with(|g| g.set(g.get() + crate::now_ns().saturating_sub(t0)));
                Some(if r < 0 { Err(Errno::EIO) } else { Ok(0) })
            }
            GEX_GL_LIMITS => Some(m.write_u32(arg, MAX_BATCH).map(|()| 0)),
            GEX_GL_SURFACE => {
                let Ok(kind) = m.read_u32(arg) else {
                    return Some(Err(Errno::EFAULT));
                };
                let Ok(id) = m.read_u32(arg + 4) else {
                    return Some(Err(Errno::EFAULT));
                };
                let Ok(w) = m.read_u32(arg + 8) else {
                    return Some(Err(Errno::EFAULT));
                };
                let Ok(h) = m.read_u32(arg + 12) else {
                    return Some(Err(Errno::EFAULT));
                };
                // Only a Wayland surface: an X window belongs to the X
                // server's process, not this one.
                if kind == 0 {
                    crate::wl::attach_external(id, w as i32, h as i32);
                }
                Some(Ok(0))
            }
            _ => None,
        }
    }
}
