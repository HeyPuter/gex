//! File descriptors and the M0 filesystem backend.
//!
//! PLAN §8 calls for a mount table over pluggable backends. M0 ships the two
//! that the integration ladder's first rungs need: the standard streams, and
//! `hostfs` passthrough so `cat` and friends can read real files. The
//! `FileBackend` split is what the tmpfs/OPFS/httpfs backends will slot into.

use crate::errno::{Errno, SysResult};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// Open-file flags the guest passes to `openat`.
pub mod oflags {
    pub const O_RDONLY: u64 = 0o0;
    pub const O_WRONLY: u64 = 0o1;
    pub const O_RDWR: u64 = 0o2;
    pub const O_CREAT: u64 = 0o100;
    pub const O_EXCL: u64 = 0o200;
    pub const O_TRUNC: u64 = 0o1000;
    pub const O_APPEND: u64 = 0o2000;
    pub const O_NONBLOCK: u64 = 0o4000;
    pub const O_DIRECTORY: u64 = 0o200000;
    pub const O_CLOEXEC: u64 = 0o2000000;
    pub const ACCMODE: u64 = 0o3;
}

/// `st_mode` type bits.
pub mod mode {
    pub const S_IFMT: u32 = 0o170000;
    pub const S_IFIFO: u32 = 0o010000;
    pub const S_IFCHR: u32 = 0o020000;
    pub const S_IFDIR: u32 = 0o040000;
    pub const S_IFREG: u32 = 0o100000;
}

/// Metadata in the shape `fstat` reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i32,
    pub blocks: i64,
    pub mtime: i64,
}

impl Stat {
    /// Serialise into the aarch64 kernel `struct stat` (128 bytes).
    pub fn encode(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&self.dev.to_le_bytes());
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.mode.to_le_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
        b[24..28].copy_from_slice(&self.uid.to_le_bytes());
        b[28..32].copy_from_slice(&self.gid.to_le_bytes());
        b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..60].copy_from_slice(&self.blksize.to_le_bytes());
        b[64..72].copy_from_slice(&self.blocks.to_le_bytes());
        for off in [72usize, 88, 104] {
            b[off..off + 8].copy_from_slice(&self.mtime.to_le_bytes());
        }
        b
    }
}

/// What sits behind a file descriptor.
#[derive(Debug)]
pub enum FileBackend {
    /// The host's standard input.
    Stdin,
    /// The host's standard output or error.
    StdWrite { is_err: bool },
    /// A real host file (`hostfs`).
    Host { file: File, path: String },
    /// `/dev/null`.
    Null,
    /// Reads produce pseudorandom bytes; writes are discarded.
    Random,
}

/// One entry in the descriptor table.
#[derive(Debug)]
pub struct OpenFile {
    pub backend: FileBackend,
    pub flags: u64,
    pub readable: bool,
    pub writable: bool,
}

impl OpenFile {
    pub fn read(&mut self, buf: &mut [u8]) -> SysResult {
        if !self.readable {
            return Err(Errno::EBADF);
        }
        match &mut self.backend {
            FileBackend::Stdin => std::io::stdin()
                .read(buf)
                .map(|n| n as u64)
                .map_err(|e| Errno::from_io(&e)),
            FileBackend::StdWrite { .. } => Err(Errno::EBADF),
            FileBackend::Host { file, .. } => file
                .read(buf)
                .map(|n| n as u64)
                .map_err(|e| Errno::from_io(&e)),
            FileBackend::Null => Ok(0),
            FileBackend::Random => {
                fill_pseudorandom(buf);
                Ok(buf.len() as u64)
            }
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> SysResult {
        if !self.writable {
            return Err(Errno::EBADF);
        }
        match &mut self.backend {
            FileBackend::Stdin => Err(Errno::EBADF),
            FileBackend::StdWrite { is_err } => {
                let n = if *is_err {
                    let mut e = std::io::stderr();
                    let n = e.write(buf).map_err(|e| Errno::from_io(&e))?;
                    let _ = e.flush();
                    n
                } else {
                    let mut o = std::io::stdout();
                    let n = o.write(buf).map_err(|e| Errno::from_io(&e))?;
                    let _ = o.flush();
                    n
                };
                Ok(n as u64)
            }
            FileBackend::Host { file, .. } => file
                .write(buf)
                .map(|n| n as u64)
                .map_err(|e| Errno::from_io(&e)),
            FileBackend::Null | FileBackend::Random => Ok(buf.len() as u64),
        }
    }

    pub fn seek(&mut self, offset: i64, whence: u32) -> SysResult {
        let from = match whence {
            0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(Errno::EINVAL),
        };
        match &mut self.backend {
            FileBackend::Host { file, .. } => {
                file.seek(from).map_err(|e| Errno::from_io(&e))
            }
            _ => Err(Errno::ESPIPE),
        }
    }

    pub fn stat(&self) -> Result<Stat, Errno> {
        match &self.backend {
            FileBackend::Host { file, .. } => {
                let md = file.metadata().map_err(|e| Errno::from_io(&e))?;
                Ok(Stat {
                    ino: 1,
                    mode: if md.is_dir() {
                        mode::S_IFDIR | 0o755
                    } else {
                        mode::S_IFREG | 0o644
                    },
                    nlink: 1,
                    size: md.len() as i64,
                    blksize: 4096,
                    blocks: (md.len() as i64 + 511) / 512,
                    ..Default::default()
                })
            }
            // The standard streams look like character devices, which is
            // what makes libc choose line buffering for a terminal.
            _ => Ok(Stat {
                ino: 1,
                mode: mode::S_IFCHR | 0o620,
                nlink: 1,
                rdev: 0x8800,
                blksize: 1024,
                ..Default::default()
            }),
        }
    }

    /// True when the descriptor should behave like a terminal for `ioctl`.
    pub fn is_tty(&self) -> bool {
        matches!(
            self.backend,
            FileBackend::Stdin | FileBackend::StdWrite { .. }
        )
    }
}

/// The per-process descriptor table.
pub struct FdTable {
    entries: Vec<Option<OpenFile>>,
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTable {
    /// A table with the three standard streams already open.
    pub fn new() -> FdTable {
        FdTable {
            entries: vec![
                Some(OpenFile {
                    backend: FileBackend::Stdin,
                    flags: 0,
                    readable: true,
                    writable: false,
                }),
                Some(OpenFile {
                    backend: FileBackend::StdWrite { is_err: false },
                    flags: 0,
                    readable: false,
                    writable: true,
                }),
                Some(OpenFile {
                    backend: FileBackend::StdWrite { is_err: true },
                    flags: 0,
                    readable: false,
                    writable: true,
                }),
            ],
        }
    }

    pub fn get(&mut self, fd: i32) -> Result<&mut OpenFile, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        self.entries
            .get_mut(fd as usize)
            .and_then(|e| e.as_mut())
            .ok_or(Errno::EBADF)
    }

    /// Install a file at the lowest free descriptor.
    pub fn insert(&mut self, f: OpenFile) -> SysResult {
        if let Some(i) = self.entries.iter().position(|e| e.is_none()) {
            self.entries[i] = Some(f);
            return Ok(i as u64);
        }
        // A hard cap keeps a runaway guest from exhausting host memory.
        if self.entries.len() >= 4096 {
            return Err(Errno::EMFILE);
        }
        self.entries.push(Some(f));
        Ok(self.entries.len() as u64 - 1)
    }

    pub fn close(&mut self, fd: i32) -> Result<(), Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        match self.entries.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(Errno::EBADF),
        }
    }

    /// Duplicate `fd` into the lowest free descriptor at or above `min`.
    pub fn dup(&mut self, fd: i32, min: i32) -> SysResult {
        let path = match &self.get(fd)?.backend {
            FileBackend::Host { path, .. } => Some(path.clone()),
            _ => None,
        };
        let src = self.get(fd)?;
        let flags = src.flags;
        let (readable, writable) = (src.readable, src.writable);
        let backend = match &src.backend {
            FileBackend::Stdin => FileBackend::Stdin,
            FileBackend::StdWrite { is_err } => FileBackend::StdWrite { is_err: *is_err },
            FileBackend::Null => FileBackend::Null,
            FileBackend::Random => FileBackend::Random,
            FileBackend::Host { file, .. } => FileBackend::Host {
                file: file.try_clone().map_err(|e| Errno::from_io(&e))?,
                path: path.unwrap_or_default(),
            },
        };
        let new = OpenFile {
            backend,
            flags,
            readable,
            writable,
        };
        if min <= 0 {
            return self.insert(new);
        }
        while self.entries.len() <= min as usize {
            self.entries.push(None);
        }
        let i = self.entries[min as usize..]
            .iter()
            .position(|e| e.is_none())
            .map(|p| p + min as usize);
        match i {
            Some(i) => {
                self.entries[i] = Some(new);
                Ok(i as u64)
            }
            None => {
                self.entries.push(Some(new));
                Ok(self.entries.len() as u64 - 1)
            }
        }
    }
}

/// Open a host path with guest `openat` flags.
pub fn open_host(path: &str, flags: u64) -> Result<OpenFile, Errno> {
    let acc = flags & oflags::ACCMODE;
    let readable = acc == oflags::O_RDONLY || acc == oflags::O_RDWR;
    let writable = acc == oflags::O_WRONLY || acc == oflags::O_RDWR;

    // A few paths are synthesised rather than passed through, because the
    // host's versions are either wrong for the guest or unsafe to expose.
    match path {
        "/dev/null" => {
            return Ok(OpenFile {
                backend: FileBackend::Null,
                flags,
                readable: true,
                writable: true,
            })
        }
        "/dev/urandom" | "/dev/random" => {
            return Ok(OpenFile {
                backend: FileBackend::Random,
                flags,
                readable: true,
                writable: true,
            })
        }
        _ => {}
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.read(readable).write(writable);
    if flags & oflags::O_CREAT != 0 {
        opts.create(true);
    }
    if flags & oflags::O_EXCL != 0 {
        opts.create_new(true);
    }
    if flags & oflags::O_TRUNC != 0 {
        opts.truncate(true);
    }
    if flags & oflags::O_APPEND != 0 {
        opts.append(true);
    }
    let file = opts.open(path).map_err(|e| Errno::from_io(&e))?;
    Ok(OpenFile {
        backend: FileBackend::Host {
            file,
            path: path.to_string(),
        },
        flags,
        readable,
        writable,
    })
}

/// A small xorshift generator: enough for `getrandom` and `/dev/urandom` in a
/// deterministic-by-default runtime, and replaced by a real CSPRNG when the
/// host provides one.
fn fill_pseudorandom(buf: &mut [u8]) {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x2545_F491_4F6C_DD1D) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        for chunk in buf.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let bytes = x.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        s.set(x);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_layout_matches_the_kernel_struct() {
        let s = Stat {
            ino: 0x1122,
            mode: mode::S_IFREG | 0o644,
            size: 4096,
            blksize: 512,
            ..Default::default()
        };
        let b = s.encode();
        assert_eq!(b.len(), 128);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 0x1122);
        assert_eq!(
            u32::from_le_bytes(b[16..20].try_into().unwrap()),
            mode::S_IFREG | 0o644
        );
        assert_eq!(i64::from_le_bytes(b[48..56].try_into().unwrap()), 4096);
        assert_eq!(i32::from_le_bytes(b[56..60].try_into().unwrap()), 512);
    }

    #[test]
    fn standard_streams_are_preinstalled() {
        let mut t = FdTable::new();
        assert!(t.get(0).is_ok());
        assert!(t.get(1).is_ok());
        assert!(t.get(2).is_ok());
        assert_eq!(t.get(3).unwrap_err(), Errno::EBADF);
    }

    #[test]
    fn closing_frees_the_lowest_descriptor() {
        let mut t = FdTable::new();
        let fd = t
            .insert(OpenFile {
                backend: FileBackend::Null,
                flags: 0,
                readable: true,
                writable: true,
            })
            .ok();
        assert_eq!(fd, Some(3));
        t.close(3).unwrap();
        assert_eq!(t.close(3).unwrap_err(), Errno::EBADF);
        let fd = t
            .insert(OpenFile {
                backend: FileBackend::Null,
                flags: 0,
                readable: true,
                writable: true,
            })
            .ok();
        assert_eq!(fd, Some(3));
    }
}
