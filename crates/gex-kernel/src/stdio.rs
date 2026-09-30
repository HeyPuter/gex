//! The standard streams.
//!
//! On the native runtime these pass through to the host's stdio. In the
//! browser the same descriptors are backed by the terminal, which is why they
//! are a [`FileHandle`] like anything else rather than a special case in the
//! syscall layer.

use crate::errno::{Errno, SysResult};
use crate::vfs::{FileHandle, FileType, Metadata};
use std::io::{Read, Write};

/// Which standard stream a handle refers to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    In,
    Out,
    Err,
}

/// A standard stream, optionally reported as a terminal.
#[derive(Debug)]
pub struct StdStream {
    pub stream: Stream,
    /// Whether `isatty` should say yes, which decides libc's buffering.
    pub tty: bool,
}

impl StdStream {
    pub fn new(stream: Stream, tty: bool) -> StdStream {
        StdStream { stream, tty }
    }
}

impl FileHandle for StdStream {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if self.stream != Stream::In {
            return Err(Errno::EBADF);
        }
        std::io::stdin()
            .read(buf)
            .map(|n| n as u64)
            .map_err(|e| Errno::from_io(&e))
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        let n = match self.stream {
            Stream::In => return Err(Errno::EBADF),
            Stream::Out => {
                let mut o = std::io::stdout();
                let n = o.write(buf).map_err(|e| Errno::from_io(&e))?;
                // Flushing on every write keeps interleaving with stderr
                // faithful, which the differential tests compare byte for
                // byte against QEMU.
                let _ = o.flush();
                n
            }
            Stream::Err => {
                let mut e = std::io::stderr();
                let n = e.write(buf).map_err(|e| Errno::from_io(&e))?;
                let _ = e.flush();
                n
            }
        };
        Ok(n as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o620,
            ino: 1,
            mtime: 0,
            nlink: 1,
            rdev: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn is_tty(&self) -> bool {
        self.tty
    }
}
