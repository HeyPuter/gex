//! The guest's graphics device in a browser: `/dev/gex-gl`.
//!
//! The same device the native host provides, and deliberately the same
//! protocol -- the guest's `libGLESv2.so.2` is one binary and does not know
//! which side of it is listening. What differs is only who executes the
//! commands: natively that is the host's OpenGL through `gex_gl::host`, and
//! here it is the page's WebGL 2, through a single import.
//!
//! WebGL 2 is OpenGL ES 3.0, so this is not much of a translation. The page's
//! side of it is `web/src/gl.js`.
//!
//! # Why the whole batch crosses in one call
//!
//! The commands are already in the guest's linear memory, and JavaScript can
//! see that memory as a typed array. So the import carries an address, and
//! the page reads the batch in place. Nothing is copied in either direction,
//! and a frame of drawing costs one call out of wasm however many GL calls it
//! contained -- which is the point of the design and matters more here than
//! natively, where a call out of wasm is dearer.

use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::Arc;

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Run the batch whose header is at `ring`, an address in this module's
    /// memory. Returns zero, or negative when the page has no WebGL at all.
    ///
    /// The reply is written back into the buffer, which is where the guest
    /// looks for it, so nothing comes back through the return value.
    fn gl_submit(ring: u32) -> i32;
}

/// Set when the page said it has a canvas with WebGL on it.
///
/// Off by default, because a page that has not been given one should have its
/// guest see a machine with no graphics device -- which fails cleanly at
/// `open` -- rather than one whose every draw call disappears.
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
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;

const MAX_BATCH: u32 = 16 * 1024 * 1024;

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
        })
    }

    fn ioctl(&self, m: &mut GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        // The request is an `int` in the C library, so every `_IOWR` number
        // arrives sign-extended.
        match request & 0xffff_ffff {
            GEX_GL_SUBMIT => {
                if !have_gl() {
                    return Some(Err(Errno::ENODEV));
                }
                // The address is checked here rather than in JS, because the
                // kernel is what knows how big the guest's memory is.
                if m.read(arg, 32).is_err() {
                    return Some(Err(Errno::EFAULT));
                }
                let r = unsafe { gl_submit(arg as u32) };
                Some(if r < 0 { Err(Errno::EIO) } else { Ok(0) })
            }
            GEX_GL_LIMITS => Some(m.write_u32(arg, MAX_BATCH).map(|()| 0)),
            _ => None,
        }
    }
}
