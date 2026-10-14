//! The guest's WebGPU device in a browser: `/dev/gex-gpu`
//! (guest/gpu/transport-dev.h).
//!
//! Same ioctls as the native device (shims/native/src/gpu.rs); here the wire
//! server is on the page (`gpu-bridge.mjs`, the server built from
//! guest/gpu/wire-server.c on the browser's WebGPU). The import carries the
//! address of the guest's `struct gex_gpu_submit`, which JS reads, message
//! and reply buffer included, straight out of this memory.

use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::Arc;

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// A new connection to the page's WebGPU: its number, or negative.
    fn gpu_open() -> i32;
    /// Run `struct gex_gpu_submit` at `arg` for `conn` (`fetch`: hand over
    /// the held reply instead). Zero, or a negative errno.
    fn gpu_submit(conn: i32, arg: u32, fetch: i32) -> i32;
    fn gpu_close(conn: i32);
}

/// Set when the page said it serves WebGPU (`gex_set_gpu`).
static mut HAVE_GPU: bool = false;

#[no_mangle]
pub extern "C" fn gex_set_gpu(on: i32) {
    unsafe { HAVE_GPU = on != 0 }
}

pub fn have_gpu() -> bool {
    unsafe { HAVE_GPU }
}

pub const GEX_GPU_SUBMIT: u64 = 0xc030_7701;
pub const GEX_GPU_FETCH: u64 = 0xc030_7702;

/// `struct gex_gpu_submit`'s size.
const SUBMIT_BYTES: usize = 48;

thread_local! {
    /// A submit the ioctl took, left for after the kernel's lock
    /// (`gex_syscall`): a message can wait on the GPU (a fence, a map) for
    /// as long as the guest asked, and every other thread's syscall would
    /// wait with it. (connection, argument, fetch)
    static PENDING: std::cell::Cell<Option<(i32, u32, bool)>> = const { std::cell::Cell::new(None) };
}

/// The submit the ioctl left, now the kernel's lock is released; a failure
/// becomes the call's result in `ctx`'s x0.
pub(crate) fn submit_pending(ctx_addr: u64) {
    let Some((conn, arg, fetch)) = PENDING.with(|p| p.take()) else { return };
    let r = unsafe { gpu_submit(conn, arg, fetch as i32) };
    if r < 0 {
        let mut m = GuestMem::new(crate::guest_bytes());
        let _ = m.write_u64(ctx_addr + gex_abi::ctx::x(0) as u64, r as i64 as u64);
    }
}

/// One open of the device: a connection on the page.
#[derive(Debug)]
pub struct GpuConnection {
    conn: i32,
}

impl GpuConnection {
    pub fn new() -> Arc<dyn FileHandle> {
        Arc::new(GpuConnection { conn: unsafe { gpu_open() } })
    }
}

impl Drop for GpuConnection {
    fn drop(&mut self) {
        if self.conn >= 0 {
            unsafe { gpu_close(self.conn) }
        }
    }
}

impl FileHandle for GpuConnection {
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
        let fetch = match request & 0xffff_ffff {
            GEX_GPU_SUBMIT => false,
            GEX_GPU_FETCH => true,
            _ => return None,
        };
        if self.conn < 0 {
            return Some(Err(Errno::ENODEV));
        }
        // The kernel knows how big the guest's memory is; JS does not check.
        if m.read(arg, SUBMIT_BYTES).is_err() {
            return Some(Err(Errno::EFAULT));
        }
        if let Some((conn, arg, fetch)) = PENDING.with(|p| p.take()) {
            unsafe { gpu_submit(conn, arg, fetch as i32) };
        }
        if crate::gl::DEFER.with(|d| d.get()) {
            PENDING.with(|p| p.set(Some((self.conn, arg as u32, fetch))));
            return Some(Ok(0));
        }
        let r = unsafe { gpu_submit(self.conn, arg as u32, fetch as i32) };
        Some(if r < 0 { Err(Errno(-r as _)) } else { Ok(0) })
    }
}
