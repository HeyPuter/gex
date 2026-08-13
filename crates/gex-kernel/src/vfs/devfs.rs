//! The synthetic `/dev`.
//!
//! Only the character devices a userspace program actually opens: the null
//! and zero sinks, the random sources, the standard-stream symlinks and
//! `/dev/tty`. Everything else is absent rather than faked, so a program that
//! needs a real device fails where it would on a container.
//!
//! The host may add its own. That is how the guest gets a graphics device: a
//! node here whose `ioctl` reaches code outside the guest entirely.

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
            nlink: 1,
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
pub struct DevFs {
    /// Nodes the host added, by name without a leading slash.
    extra: Vec<(String, Arc<dyn FileHandle>)>,
}

impl DevFs {
    pub fn new() -> DevFs {
        DevFs { extra: Vec::new() }
    }

    /// Add a device the host provides. The handle is shared, not cloned per
    /// open, so a device keeps state (e.g. a graphics context) across opens.
    pub fn add(&mut self, name: &str, handle: Arc<dyn FileHandle>) {
        self.extra.retain(|(n, _)| n != name);
        self.extra.push((name.to_string(), handle));
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
        if let Some((_, handle)) = self.extra.iter().find(|(n, _)| n == path) {
            return Ok(Arc::clone(handle));
        }
        if path.is_empty() {
            return Ok(Arc::new(DevDir {
                extra: self.extra.iter().map(|(n, _)| n.clone()).collect(),
            }));
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
                nlink: 1,
            });
        }
        if matches!(path, "stdin" | "stdout" | "stderr" | "fd") {
            return Ok(Metadata {
                file_type: FileType::Symlink,
                size: 0,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
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
struct DevDir {
    /// Names the host added, so `ls /dev` shows them.
    extra: Vec<String>,
}

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
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(self.extra.iter().cloned())
        {
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
        let f = DevFs::new().open("null", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(f.write_at(0, b"discarded").unwrap(), 9);
    }

    #[test]
    fn zero_reads_zeros() {
        let f = DevFs::new().open("zero", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0u8; 8]);
    }

    #[test]
    fn full_fails_writes_with_enospc() {
        let f = DevFs::new().open("full", 0, 0).unwrap();
        assert_eq!(f.write_at(0, b"x").err(), Some(Errno::ENOSPC));
    }

    #[test]
    fn standard_stream_names_are_symlinks_into_proc() {
        assert_eq!(DevFs::new().read_link("stdout").unwrap(), "/proc/self/fd/1");
        assert_eq!(
            DevFs::new().metadata("stdout").unwrap().file_type,
            FileType::Symlink
        );
    }

    #[test]
    fn unknown_devices_are_absent_rather_than_faked() {
        assert_eq!(DevFs::new().open("sda1", 0, 0).err(), Some(Errno::ENOENT));
    }

    /// A device the host adds is opened like any other, and the same handle
    /// comes back every time -- which is what lets one keep state across the
    /// several times a program opens it.
    #[test]
    fn a_host_device_is_openable_and_shared() {
        #[derive(Debug)]
        struct Counter(std::sync::atomic::AtomicU32);
        impl FileHandle for Counter {
            fn read_at(&self, _o: u64, _b: &mut [u8]) -> SysResult {
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
            fn ioctl(
                &self,
                _m: &mut crate::GuestMem<'_>,
                request: u64,
                _arg: u64,
            ) -> Option<SysResult> {
                if request == 42 {
                    let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(Ok(n as u64))
                } else {
                    None
                }
            }
        }
        let device = Arc::new(Counter(std::sync::atomic::AtomicU32::new(0)));
        let mut fs = DevFs::new();
        fs.add("gex-thing", device);
        let a = fs.open("gex-thing", 0, 0).unwrap();
        let b = fs.open("gex-thing", 0, 0).unwrap();
        let mut bytes = [0u8; 16];
        let mut mem = crate::GuestMem::new(&mut bytes);
        assert_eq!(a.ioctl(&mut mem, 42, 0), Some(Ok(0)));
        assert_eq!(b.ioctl(&mut mem, 42, 0), Some(Ok(1)), "the same device");
        assert_eq!(a.ioctl(&mut mem, 7, 0), None, "not its ioctl");
        // And it is listed, so `ls /dev` shows it.
        let names: Vec<String> = fs
            .open("", 0, 0)
            .unwrap()
            .read_dir()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == "gex-thing"));
    }
}
