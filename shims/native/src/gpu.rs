//! The guest's WebGPU device: `/dev/gex-gpu` (guest/gpu/transport-dev.h).
//!
//! Each open is a connection: a wire server of its own (`gex_webgpu`), whose
//! objects are the ids its client chose. A submit names a message in guest
//! memory and a buffer for the reply; a reply that does not fit is held for
//! `GEX_GPU_FETCH` once the guest has a bigger buffer.

use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::{Arc, Mutex};

/// `_IOWR('w', 1, struct gex_gpu_submit)`: run a message.
pub const GEX_GPU_SUBMIT: u64 = 0xc030_7701;
/// `_IOWR('w', 2, struct gex_gpu_submit)`: take the held reply.
pub const GEX_GPU_FETCH: u64 = 0xc030_7702;

const STATUS_OK: u32 = 0;
const STATUS_HELD: u32 = 1;
const STATUS_MALFORMED: u32 = 2;

/// The wire version this device speaks (guest/gpu/wire.h).
const WIRE_VERSION: u32 = 1;

/// The largest message the device reads.
const MAX_MESSAGE: u64 = 256 * 1024 * 1024;

/// One open of the device.
pub struct GpuConnection {
    /// Made at the first message, so an open that never submits costs no
    /// Dawn instance.
    server: Mutex<Option<Option<gex_webgpu::Server>>>,
    /// A reply too big for the guest's buffer, and whether its message parsed.
    held: Mutex<Option<(Vec<u8>, bool)>>,
}

impl std::fmt::Debug for GpuConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GpuConnection")
    }
}

/// `struct gex_gpu_submit`'s fields, by offset.
mod field {
    pub const DATA: u64 = 0;
    pub const SIZE: u64 = 8;
    pub const REPLY: u64 = 16;
    pub const REPLY_CAPACITY: u64 = 24;
    pub const REPLY_SIZE: u64 = 32;
    pub const STATUS: u64 = 40;
    pub const VERSION: u64 = 44;
}

impl GpuConnection {
    pub fn new() -> Arc<GpuConnection> {
        Arc::new(GpuConnection { server: Mutex::new(None), held: Mutex::new(None) })
    }

    /// Hands the guest a reply: in its buffer if it fits, held otherwise.
    fn answer(&self, m: &mut GuestMem<'_>, arg: u64, reply: Vec<u8>, ok: bool) -> SysResult {
        let buffer = m.read_u64(arg + field::REPLY)?;
        let capacity = m.read_u64(arg + field::REPLY_CAPACITY)?;
        m.write_u64(arg + field::REPLY_SIZE, reply.len() as u64)?;
        if reply.len() as u64 > capacity {
            *self.held.lock().unwrap() = Some((reply, ok));
            m.write_u32(arg + field::STATUS, STATUS_HELD)?;
            return Ok(0);
        }
        m.write(buffer, &reply)?;
        m.write_u32(arg + field::STATUS, if ok { STATUS_OK } else { STATUS_MALFORMED })?;
        Ok(0)
    }

    fn submit(&self, m: &mut GuestMem<'_>, arg: u64) -> SysResult {
        if m.read_u32(arg + field::VERSION)? != WIRE_VERSION {
            return Err(Errno::EINVAL);
        }
        let data = m.read_u64(arg + field::DATA)?;
        let size = m.read_u64(arg + field::SIZE)?;
        if size > MAX_MESSAGE {
            return Err(Errno::E2BIG);
        }
        let message = m.read(data, size as usize)?.to_vec();
        let (reply, ok) = {
            let mut server = self.server.lock().unwrap();
            let server = server.get_or_insert_with(|| {
                let s = gex_webgpu::Server::new();
                if s.is_none() {
                    eprintln!("gex: /dev/gex-gpu: no WebGPU server in this build");
                }
                s
            });
            match server {
                Some(s) => s.handle(message),
                None => return Err(Errno::ENODEV),
            }
        };
        self.held.lock().unwrap().take();
        self.answer(m, arg, reply, ok)
    }

    fn fetch(&self, m: &mut GuestMem<'_>, arg: u64) -> SysResult {
        let Some((reply, ok)) = self.held.lock().unwrap().take() else {
            return Err(Errno::EINVAL);
        };
        self.answer(m, arg, reply, ok)
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
        // `ioctl`'s request is a C `int`, so every `_IOWR` arrives sign-extended.
        match request & 0xffff_ffff {
            GEX_GPU_SUBMIT => Some(self.submit(m, arg)),
            GEX_GPU_FETCH => Some(self.fetch(m, arg)),
            _ => None,
        }
    }
}
