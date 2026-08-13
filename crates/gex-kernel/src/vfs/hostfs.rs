//! Passthrough to the host filesystem.
//!
//! Only meaningful on the native runtime; the browser mounts OPFS or a
//! read-only image at the same points. A `root` prefix confines the guest to
//! a subtree, which is what `gexc run --sysroot` uses to present a rootfs.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use crate::fs::oflags;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A host directory presented as a guest filesystem.
pub struct HostFs {
    root: PathBuf,
    read_only: bool,
}

impl HostFs {
    pub fn new(root: impl Into<PathBuf>) -> HostFs {
        HostFs {
            root: root.into(),
            read_only: false,
        }
    }

    pub fn read_only(root: impl Into<PathBuf>) -> HostFs {
        HostFs {
            root: root.into(),
            read_only: true,
        }
    }

    /// Map a guest-relative path to a host path, refusing anything that would
    /// escape the root.
    ///
    /// The guest path has already been lexically normalised by the VFS, so a
    /// remaining `..` could only come from a symlink target and is rejected
    /// rather than followed out of the sandbox.
    fn host_path(&self, rel: &str) -> Result<PathBuf, Errno> {
        if rel.split('/').any(|p| p == "..") {
            return Err(Errno::EACCES);
        }
        Ok(if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        })
    }
}

fn file_type_of(md: &std::fs::Metadata) -> FileType {
    use std::os::unix::fs::FileTypeExt;
    let t = md.file_type();
    if md.is_dir() {
        FileType::Directory
    } else if md.is_symlink() {
        FileType::Symlink
    } else if md.is_file() {
        FileType::Regular
    } else if t.is_socket() {
        // A bound UNIX socket. Everything that waits for a server to come up
        // stats its path and asks for exactly this.
        FileType::Socket
    } else if t.is_fifo() {
        FileType::Fifo
    } else {
        FileType::CharDevice
    }
}

fn convert(md: &std::fs::Metadata) -> Metadata {
    Metadata {
        file_type: file_type_of(md),
        size: md.len(),
        mode: md.mode() & 0o7777,
        ino: md.ino(),
        mtime: md.mtime(),
        nlink: md.nlink() as u32,
    }
}

/// An open host file.
#[derive(Debug)]
struct HostHandle {
    file: Mutex<File>,
}

impl FileHandle for HostHandle {
    /// `ftruncate`. The trait default answers `EINVAL`; apt truncates a file to
    /// 0 before hashing a download and reported that as "Unable to truncate".
    fn truncate(&self, len: u64) -> Result<(), Errno> {
        self.file
            .lock()
            .unwrap()
            .set_len(len)
            .map_err(|e| Errno::from_io(&e))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| Errno::from_io(&e))?;
        f.read(buf)
            .map(|n| n as u64)
            .map_err(|e| Errno::from_io(&e))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| Errno::from_io(&e))?;
        f.write(buf)
            .map(|n| n as u64)
            .map_err(|e| Errno::from_io(&e))
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        let f = self.file.lock().unwrap();
        f.metadata()
            .map(|m| convert(&m))
            .map_err(|e| Errno::from_io(&e))
    }
}

/// An open host directory. Directories are listed eagerly, because the guest
/// expects a stable snapshot across a `getdents64` sequence.
#[derive(Debug)]
struct HostDir {
    entries: Vec<DirEntry>,
    md: Metadata,
}

impl FileHandle for HostDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(self.md.clone())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        Ok(self.entries.clone())
    }
}

impl FileSystem for HostFs {
    fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let host = self.host_path(path)?;
        let md = std::fs::metadata(&host);
        if let Ok(md) = &md {
            if md.is_dir() {
                let mut entries = vec![
                    DirEntry {
                        name: ".".into(),
                        file_type: FileType::Directory,
                        ino: md.ino(),
                    },
                    DirEntry {
                        name: "..".into(),
                        file_type: FileType::Directory,
                        ino: 1,
                    },
                ];
                for e in std::fs::read_dir(&host).map_err(|e| Errno::from_io(&e))? {
                    let e = e.map_err(|e| Errno::from_io(&e))?;
                    let emd = e.metadata().map_err(|e| Errno::from_io(&e))?;
                    entries.push(DirEntry {
                        name: e.file_name().to_string_lossy().into_owned(),
                        file_type: file_type_of(&emd),
                        ino: emd.ino(),
                    });
                }
                return Ok(Arc::new(HostDir {
                    entries,
                    md: convert(md),
                }));
            }
        }

        let acc = flags & oflags::ACCMODE;
        let want_write = acc == oflags::O_WRONLY || acc == oflags::O_RDWR;
        if want_write && self.read_only {
            return Err(Errno::EROFS);
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(acc == oflags::O_RDONLY || acc == oflags::O_RDWR)
            .write(want_write);
        if flags & oflags::O_CREAT != 0 {
            // The guest's mode but never without owner rw: gex does not enforce
            // permissions, but the host process is not root, and a file it
            // creates mode 0 (as dpkg does for `.dpkg-new`) it cannot reopen.
            opts.create(true).mode(mode | 0o600);
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
        let file = opts.open(&host).map_err(|e| Errno::from_io(&e))?;
        Ok(Arc::new(HostHandle {
            file: Mutex::new(file),
        }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let host = self.host_path(path)?;
        std::fs::metadata(&host)
            .map(|m| convert(&m))
            .map_err(|e| Errno::from_io(&e))
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let host = self.host_path(path)?;
        // `read_link` on a non-symlink reports EINVAL, which the VFS uses to
        // decide it has reached the real file.
        let md = std::fs::symlink_metadata(&host).map_err(|e| Errno::from_io(&e))?;
        if !md.is_symlink() {
            return Err(Errno::EINVAL);
        }
        std::fs::read_link(&host)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| Errno::from_io(&e))
    }

    fn unlink(&self, path: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::fs::remove_file(self.host_path(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn mkdir(&self, path: &str, _mode: u32) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::fs::create_dir(self.host_path(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::fs::remove_dir(self.host_path(path)?).map_err(|e| Errno::from_io(&e))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::fs::rename(self.host_path(from)?, self.host_path(to)?).map_err(|e| Errno::from_io(&e))
    }

    /// `chmod`, same floor as creation: the host must keep owner rw on what it
    /// owns for the guest.
    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            self.host_path(path)?,
            std::fs::Permissions::from_mode((mode & 0o7777) | 0o600),
        )
        .map_err(|e| Errno::from_io(&e))
    }

    fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::fs::hard_link(self.host_path(from)?, self.host_path(to)?)
            .map_err(|e| Errno::from_io(&e))
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        std::os::unix::fs::symlink(Path::new(target), self.host_path(path)?)
            .map_err(|e| Errno::from_io(&e))
    }

    /// A real socket node, made by binding a `UnixListener` and dropping it,
    /// which leaves the node. `mknod` would mean a libc dependency for one call.
    fn mksocket(&self, path: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        let host = self.host_path(path)?;
        match std::os::unix::net::UnixListener::bind(&host) {
            Ok(l) => {
                drop(l);
                Ok(())
            }
            Err(e) => Err(Errno::from_io(&e)),
        }
    }
}

use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gex-hostfs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn reads_a_real_file() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), b"contents").unwrap();
        let fs = HostFs::new(&dir);
        let f = fs.open("a.txt", oflags::O_RDONLY, 0).unwrap();
        let mut buf = [0u8; 16];
        let n = f.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n as usize], b"contents");
    }

    #[test]
    fn escaping_the_root_is_refused() {
        let fs = HostFs::new(scratch());
        assert_eq!(fs.metadata("../../etc/passwd").err(), Some(Errno::EACCES));
    }

    #[test]
    fn read_only_mounts_refuse_writes() {
        let dir = scratch();
        std::fs::write(dir.join("ro.txt"), b"x").unwrap();
        let fs = HostFs::read_only(&dir);
        assert_eq!(
            fs.open("ro.txt", oflags::O_WRONLY, 0).err(),
            Some(Errno::EROFS)
        );
        assert!(fs.open("ro.txt", oflags::O_RDONLY, 0).is_ok());
    }
}
