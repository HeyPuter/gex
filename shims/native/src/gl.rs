//! The guest's graphics device: `/dev/gex-gl` (native debug host).
//!
//! An `ioctl` node whose call reaches the host's OpenGL. The commands are
//! already in guest memory and the host can read it, so the call carries an
//! address rather than data -- one host call per frame however many GL calls
//! it holds.

use gex_gl::host::{read_header, Gl};
use gex_gl::proto::{error, Header, MAGIC};
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::{Arc, Mutex};

/// `_IOWR('g', 1, ...)`: hand over the batch whose header is at `arg`. A private
/// number, not DRM's, since this is not a DRM device.
pub const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// `_IOR('g', 2, ...)`: how big a batch the device will take.
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;

/// The largest batch the device will read in one go.
const MAX_BATCH: u32 = 16 * 1024 * 1024;

/// The device, and the host GL context behind it. The context is used on
/// whichever thread submits (a GL thread's channel round trip measured 23 of a
/// 24us GL call; `Gl` documents why this is sound), and made lazily on first
/// submit so a guest that never draws pays no EGL cost.
pub struct GlDevice {
    gl: Mutex<Option<Result<Gl, String>>>,
    size: (i32, i32),
}

impl std::fmt::Debug for GlDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GlDevice")
    }
}

impl GlDevice {
    pub fn new(width: i32, height: i32) -> Arc<GlDevice> {
        Arc::new(GlDevice {
            gl: Mutex::new(None),
            size: (width, height),
        })
    }

    /// What the host's GL turned out to be, once a guest has asked for it.
    pub fn describe(&self) -> Option<String> {
        match &*self.gl.lock().unwrap() {
            Some(Ok(gl)) => Some(format!("{} / {}", gl.renderer, gl.version)),
            Some(Err(e)) => Some(format!("unavailable: {e}")),
            None => None,
        }
    }

    /// Run one batch, reading it out of guest memory and writing the reply back.
    fn submit(&self, m: &mut GuestMem<'_>, ring: u64) -> SysResult {
        let head = m.read(ring, Header::SIZE)?.to_vec();
        let Some(header) = read_header(&head) else {
            return Err(Errno::EINVAL);
        };
        if header.magic != MAGIC {
            return Err(Errno::EINVAL);
        }
        if header.capacity > MAX_BATCH || header.tail > header.capacity {
            return Err(Errno::EINVAL);
        }

        let batch = m
            .read(ring + Header::SIZE as u64, header.tail as usize)?
            .to_vec();

        let mut slot = self.gl.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Gl::new(self.size.0, self.size.1));
        }
        let (reply, status, executed) = match slot.as_mut().unwrap() {
            Ok(gl) => gl.run(&batch),
            // No GL is reported in the header, not as a failed ioctl, so the
            // guest falls back as it would for a driver that refused to load.
            Err(_) => {
                write_status(m, ring, error::NO_CONTEXT, 0, 0)?;
                return Ok(0);
            }
        };
        let bytes = reply.bytes();
        let room = header.reply_capacity as usize;
        let n = bytes.len().min(room);
        if n > 0 {
            let at = ring + Header::SIZE as u64 + header.capacity as u64;
            m.write(at, &bytes[..n])?;
        }
        write_status(m, ring, status, executed, n as u32)?;
        Ok(0)
    }
}

/// Update the header after a batch: commands consumed, reply length recorded.
fn write_status(
    m: &mut GuestMem<'_>,
    ring: u64,
    status: u32,
    failed_at: u32,
    reply_len: u32,
) -> Result<(), Errno> {
    m.write_u32(ring + 8, 0)?; // tail: consumed
    m.write_u32(ring + 16, reply_len)?;
    m.write_u32(ring + 20, status)?;
    m.write_u32(ring + 24, failed_at)?;
    Ok(())
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
        })
    }

    fn ioctl(&self, m: &mut GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        // `ioctl`'s request is a C `int`, so every `_IOWR` arrives sign-extended.
        match request & 0xffff_ffff {
            GEX_GL_SUBMIT => Some(self.submit(m, arg)),
            GEX_GL_LIMITS => Some(
                m.write_u32(arg, MAX_BATCH)
                    .map(|()| 0)
                    .map_err(|e| e),
            ),
            _ => None,
        }
    }
}
