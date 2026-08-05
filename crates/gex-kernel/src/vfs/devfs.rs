//! The synthetic `/dev`.
//!
//! Only the character devices a userspace program actually opens: the null
//! and zero sinks, the random sources, the standard-stream symlinks and
//! `/dev/tty`. Everything else is absent rather than faked, so a program that
//! needs a real device fails where it would on a container.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::Arc;

/// Which device a handle refers to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Device {
    Null,
    Zero,
    Full,
    Random,
    Tty,
}

#[derive(Debug)]
struct DevHandle {
    device: Device,
}

impl FileHandle for DevHandle {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        match self.device {
            // Reading /dev/null is immediate end of file.
            Device::Null => Ok(0),
            Device::Zero | Device::Full => {
                buf.fill(0);
                Ok(buf.len() as u64)
            }
            Device::Random => {
                fill_random(buf);
                Ok(buf.len() as u64)
            }
            // A terminal read has to come from the host's stdin, which the
            // kernel wires up separately; through the VFS it looks empty.
            Device::Tty => Ok(0),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        match self.device {
            // /dev/full exists precisely so that writes fail this way.
            Device::Full => Err(Errno::ENOSPC),
            _ => Ok(buf.len() as u64),
        }
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 1,
            mtime: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn is_tty(&self) -> bool {
        self.device == Device::Tty
    }
}

/// The `/dev` filesystem.
pub struct DevFs;

impl DevFs {
    pub fn new() -> DevFs {
        DevFs
    }

    fn device_of(path: &str) -> Option<Device> {
        Some(match path {
            "null" => Device::Null,
            "zero" => Device::Zero,
            "full" => Device::Full,
            "random" | "urandom" => Device::Random,
            "tty" | "console" => Device::Tty,
            _ => return None,
        })
    }
}

impl Default for DevFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for DevFs {
    fn open(&self, path: &str, _flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        if let Some(device) = Self::device_of(path) {
            return Ok(Arc::new(DevHandle { device }));
        }
        if path.is_empty() {
            return Ok(Arc::new(DevDir));
        }
        Err(Errno::ENOENT)
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        if path.is_empty() {
            return Ok(Metadata::dir());
        }
        if Self::device_of(path).is_some() {
            return Ok(Metadata {
                file_type: FileType::CharDevice,
                size: 0,
                mode: 0o666,
                ino: 1,
                mtime: 0,
            });
        }
        if matches!(path, "stdin" | "stdout" | "stderr" | "fd") {
            return Ok(Metadata {
                file_type: FileType::Symlink,
                size: 0,
                mode: 0o777,
                ino: 1,
                mtime: 0,
            });
        }
        Err(Errno::ENOENT)
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        Ok(match path {
            "stdin" => "/proc/self/fd/0".to_string(),
            "stdout" => "/proc/self/fd/1".to_string(),
            "stderr" => "/proc/self/fd/2".to_string(),
            "fd" => "/proc/self/fd".to_string(),
            _ => return Err(Errno::EINVAL),
        })
    }
}

#[derive(Debug)]
struct DevDir;

impl FileHandle for DevDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::dir())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let mut out = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
            DirEntry {
                name: "..".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
        ];
        for name in [
            "null", "zero", "full", "random", "urandom", "tty", "console",
        ] {
            out.push(DirEntry {
                name: name.into(),
                file_type: FileType::CharDevice,
                ino: 1,
            });
        }
        for name in ["stdin", "stdout", "stderr", "fd"] {
            out.push(DirEntry {
                name: name.into(),
                file_type: FileType::Symlink,
                ino: 1,
            });
        }
        Ok(out)
    }
}

/// Pseudorandom bytes.
///
/// Deterministic by default so record/replay stays possible (PLAN §11); a
/// host that wants real entropy replaces the `/dev/random` backend.
fn fill_random(buf: &mut [u8]) {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9e37_79b9_7f4a_7c15) };
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
    fn null_reads_empty_and_swallows_writes() {
        let f = DevFs.open("null", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(f.write_at(0, b"discarded").unwrap(), 9);
    }

    #[test]
    fn zero_reads_zeros() {
        let f = DevFs.open("zero", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0u8; 8]);
    }

    #[test]
    fn full_fails_writes_with_enospc() {
        let f = DevFs.open("full", 0, 0).unwrap();
        assert_eq!(f.write_at(0, b"x").err(), Some(Errno::ENOSPC));
    }

    #[test]
    fn standard_stream_names_are_symlinks_into_proc() {
        assert_eq!(DevFs.read_link("stdout").unwrap(), "/proc/self/fd/1");
        assert_eq!(
            DevFs.metadata("stdout").unwrap().file_type,
            FileType::Symlink
        );
    }

    #[test]
    fn unknown_devices_are_absent_rather_than_faked() {
        assert_eq!(DevFs.open("sda1", 0, 0).err(), Some(Errno::ENOENT));
    }
}
