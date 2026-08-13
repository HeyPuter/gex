//! The guest's graphics device: `/dev/gex-gl`.
//!
//! A node in the guest's `/dev` whose `ioctl` reaches the host's OpenGL. The
//! guest's GL library fills a buffer with commands and then makes exactly one
//! call -- this one -- to hand the whole batch over.
//!
//! # Why an ioctl and not a write
//!
//! Because there is nothing to write. The commands are already in guest
//! memory, and the host can read guest memory, so the call carries an address
//! rather than any data. A `write` would mean the kernel copying a buffer out
//! of the guest and the host copying it back in, for a batch that may be
//! hundreds of kilobytes on a frame that uploads a texture.
//!
//! That is the whole performance argument for this design, and it is worth
//! being precise about what it buys: the *only* cost per frame is one host
//! call, whatever the number of GL calls in it. Everything else is the guest
//! appending bytes to memory it already has.

use gex_gl::host::{read_header, Gl};
use gex_gl::proto::{error, Header, MAGIC};
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::sync::{Arc, Mutex};

/// `_IOWR('g', 1, ...)`: hand over the batch whose header is at `arg`.
///
/// A private number rather than one of DRM's, because this is not a DRM
/// device and pretending otherwise would mislead anything that went looking.
pub const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// `_IOR('g', 2, ...)`: how big a batch the device will take, so the guest
/// can size its buffer without guessing.
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;

/// The largest batch the device will read in one go.
const MAX_BATCH: u32 = 16 * 1024 * 1024;

/// The device, and the host GL context behind it.
///
/// The context is used on whichever thread submits. That is not the obvious
/// arrangement -- the obvious one is a GL thread everything posts to -- and it
/// was measured: a channel round trip between two threads was 23 of the 24
/// microseconds a synchronous GL call took, against 1 microsecond for the
/// syscall itself. `Gl` documents why moving it is sound.
///
/// The context is made when the guest first submits rather than at startup,
/// so a guest that never draws does not pay for bringing up EGL, and a host
/// with no GL at all still runs everything else.
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

    /// Run one batch, reading it out of guest memory and writing the reply
    /// back the same way.
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
            // No GL on this host is reported in the header rather than as a
            // failed ioctl, so a guest can fall back the way it would if a
            // driver refused to load.
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

/// Update the header after a batch: the guest's commands are consumed, and
/// however much reply there is is now there.
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
        // `ioctl`'s request is an `int` in the C library, so a number with the
        // top bit set -- which every `_IOWR` has -- arrives sign-extended.
        // Comparing the full 64 bits would never match.
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
