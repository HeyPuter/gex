//! The guest's graphics device: `/dev/gex-gl` (native debug host).
//!
//! An `ioctl` node whose call reaches the host's OpenGL. The commands are
//! already in guest memory and the host can read it, so the call carries an
//! address rather than data -- one host call per frame however many GL calls
//! it holds.

use gex_gl::host::{read_header, Frame, Gl};
use gex_gl::proto::{error, Header, MAGIC};
use gex_gl::trace;
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// `_IOWR('g', 1, ...)`: hand over the batch whose header is at `arg`. A private
/// number, not DRM's, since this is not a DRM device.
pub const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// `_IOR('g', 2, ...)`: how big a batch the device will take.
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;

/// The largest batch the device will read in one go.
const MAX_BATCH: u32 = 32 * 1024 * 1024;

/// The device, and the host GL context behind it. The context is used on
/// whichever thread submits (a GL thread's channel round trip measured 23 of a
/// 24us GL call; `Gl` documents why this is sound), and made lazily on first
/// submit so a guest that never draws pays no EGL cost.
pub struct GlDevice {
    gl: Mutex<Option<Result<Gl, String>>>,
    size: (i32, i32),
    /// Where `GEX_GL_CAPTURE` said to record the command stream, and the file
    /// once one batch has arrived. Recording is a debugging aid: a guest whose
    /// trace cannot be written still runs.
    capture: Mutex<Capture>,
    /// Where presented frames go (`--wayland-out`), if anywhere.
    sink: Mutex<Option<FrameSink>>,
}

/// What a presented frame is handed to.
pub type FrameSink = Box<dyn Fn(Frame) + Send + Sync>;

/// The process's device, once `--gl` made one, so the host can route its
/// frames once the compositor exists.
pub static DEVICE: std::sync::OnceLock<Arc<GlDevice>> = std::sync::OnceLock::new();

/// The recording half of the device.
#[derive(Default)]
enum Capture {
    /// `GEX_GL_CAPTURE` was unset, or the file stopped being writable.
    #[default]
    Off,
    /// Asked for, not opened yet: the header carries the surface size, so it
    /// is written when the first batch proves a guest is really drawing.
    Requested(std::path::PathBuf),
    On(std::fs::File),
}

impl std::fmt::Debug for GlDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GlDevice")
    }
}

impl GlDevice {
    pub fn new(width: i32, height: i32) -> Arc<GlDevice> {
        let capture = match std::env::var_os("GEX_GL_CAPTURE") {
            Some(path) if !path.is_empty() => Capture::Requested(path.into()),
            _ => Capture::Off,
        };
        Arc::new(GlDevice {
            gl: Mutex::new(None),
            size: (width, height),
            capture: Mutex::new(capture),
            sink: Mutex::new(None),
        })
    }

    /// Append one batch to the recording, if there is one.
    ///
    /// Recorded before execution rather than after, so a batch that makes the
    /// host GL abort is in the file that gets replayed.
    fn record(&self, batch: &[u8], reply_capacity: u32) {
        let mut capture = self.capture.lock().unwrap();
        if let Capture::Requested(path) = &*capture {
            *capture = match std::fs::File::create(path).and_then(|mut file| {
                file.write_all(&trace::header(self.size.0 as u32, self.size.1 as u32))?;
                Ok(file)
            }) {
                Ok(file) => Capture::On(file),
                Err(e) => {
                    eprintln!("gex: cannot record GL to {}: {e}", path.display());
                    Capture::Off
                }
            };
        }
        if let Capture::On(file) = &mut *capture {
            if let Err(e) = file
                .write_all(&trace::record(batch, reply_capacity))
                .and_then(|()| file.flush())
            {
                eprintln!("gex: GL recording stopped: {e}");
                *capture = Capture::Off;
            }
        }
    }

    /// Hand every presented frame to `sink` (read back from the host's
    /// surface at each PRESENT).
    pub fn set_frame_sink(&self, sink: FrameSink) {
        *self.sink.lock().unwrap() = Some(sink);
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

        self.record(&batch, header.reply_capacity);

        let mut slot = self.gl.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Gl::new(self.size.0, self.size.1));
        }
        let sink = self.sink.lock().unwrap();
        let (reply, status, executed) = match slot.as_mut().unwrap() {
            Ok(gl) => {
                gl.keep_frames(sink.is_some());
                let r = gl.run(&batch);
                if let (Some(sink), Some(frame)) = (sink.as_ref(), gl.take_frame()) {
                    sink(frame);
                }
                r
            }
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
            rdev: 0,
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
