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

pub(crate) fn convert(md: &std::fs::Metadata) -> Metadata {
    Metadata {
        file_type: file_type_of(md),
        size: md.len(),
        mode: md.mode() & 0o7777,
        ino: md.ino(),
        mtime: md.mtime(),
        nlink: md.nlink() as u32,
        rdev: 0,
    }
}

/// An open host file.
#[derive(Debug)]
struct HostHandle {
    file: Mutex<File>,
    /// Not a regular file (a FIFO, a socket, a device): no offset.
    stream: bool,
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
        if !self.stream {
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| Errno::from_io(&e))?;
        }
        f.read(buf)
            .map(|n| n as u64)
            .map_err(|e| Errno::from_io(&e))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let mut f = self.file.lock().unwrap();
        if !self.stream {
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| Errno::from_io(&e))?;
        }
        f.write(buf)
            .map(|n| n as u64)
            .map_err(|e| Errno::from_io(&e))
    }

    fn is_stream(&self) -> bool {
        self.stream
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
            // a directory opens for reading only
            let acc = flags & oflags::ACCMODE;
            if md.is_dir() && (acc == oflags::O_WRONLY || acc == oflags::O_RDWR) {
                return Err(Errno::EISDIR);
            }
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
        // Linux's O_CREAT, O_EXCL, O_TRUNC and O_NONBLOCK on the host.
        let mut custom = 0;
        if flags & oflags::O_CREAT != 0 {
            // The guest's mode but never without owner rw: gex does not enforce
            // permissions, but the host process is not root, and a file it
            // creates mode 0 (as dpkg does for `.dpkg-new`) it cannot reopen.
            opts.mode(mode | 0o600);
            if want_write {
                opts.create(true);
            } else {
                // std refuses to create without write access; Linux creates
                // with O_RDONLY (gnulib's open tests, `touch`-like opens)
                custom |= 0o100;
            }
        }
        if flags & oflags::O_EXCL != 0 {
            if want_write {
                opts.create_new(true);
            } else if flags & oflags::O_CREAT != 0 {
                custom |= 0o200;
            }
        }
        if flags & oflags::O_TRUNC != 0 {
            if want_write {
                opts.truncate(true);
            } else {
                custom |= 0o1000;
            }
        }
        if flags & oflags::O_APPEND != 0 {
            opts.append(true);
        }
        // O_NONBLOCK reaches the host: opening a host FIFO without a writer
        // must not block (grep -D skip opens one so, to fstat and skip it).
        // Linux's O_NONBLOCK on the host.
        if flags & oflags::O_NONBLOCK != 0 {
            custom |= 0o4000;
        }
        opts.custom_flags(custom);
        let file = opts.open(&host).map_err(|e| Errno::from_io(&e))?;
        // A FIFO, socket or device has no offset: read and write it as is.
        let stream = file.metadata().is_ok_and(|m| !m.is_file());
        Ok(Arc::new(HostHandle {
            file: Mutex::new(file),
            stream,
        }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let host = self.host_path(path)?;
        std::fs::metadata(&host)
            .map(|m| convert(&m))
            .map_err(|e| Errno::from_io(&e))
    }

    /// The host's answer: a guest that believes it owns everything (stat
    /// reports the guest's uid) would otherwise be told it may write
    /// root's directories, and pick them over its fallbacks (Xwayland's
    /// keymap directory, `/var/lib/xkb`, over `/tmp`).
    fn access(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let host = self.host_path(path)?;
        if mode & 2 != 0 && self.read_only {
            std::fs::metadata(&host).map_err(|e| Errno::from_io(&e))?;
            return Err(Errno::EROFS);
        }
        host_access(&host, mode)
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

    fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        // The guest's mode (its umask already applied), as Linux would make
        // it: wineserver refuses a server directory others can read. The
        // owner keeps rwx, as `chmod` does for files, so gex can always
        // manage what it made.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode((mode & 0o7777) | 0o700)
            .create(self.host_path(path)?)
            .map_err(|e| Errno::from_io(&e))
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

    fn set_mtime(&self, path: &str, secs: i64) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        let t = if secs >= 0 {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
        } else {
            std::time::UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
        };
        let f = std::fs::File::open(self.host_path(path)?).map_err(|e| Errno::from_io(&e))?;
        f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t))
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

/// `access(2)` against the host's permission bits, for this process's
/// effective ids and groups (from `/proc/self/status`; no unsafe here, so
/// not the host call itself). Root may read and write anything and execute
/// what has an execute bit. ACLs and read-only mounts are not seen.
#[cfg(unix)]
fn host_access(host: &std::path::Path, mode: u32) -> Result<(), Errno> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(host).map_err(|e| Errno::from_io(&e))?;
    let mode = mode & 7;
    if mode == 0 {
        return Ok(());
    }
    let ids = host_ids();
    let bits = md.mode();
    let ok = if ids.uid == 0 {
        mode & 1 == 0 || md.is_dir() || bits & 0o111 != 0
    } else {
        let have = if md.uid() == ids.uid {
            bits >> 6
        } else if md.gid() == ids.gid || ids.groups.contains(&md.gid()) {
            bits >> 3
        } else {
            bits
        } & 7;
        have & mode == mode
    };
    if ok {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

#[cfg(unix)]
struct HostIds {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

#[cfg(unix)]
fn host_ids() -> &'static HostIds {
    static IDS: std::sync::OnceLock<HostIds> = std::sync::OnceLock::new();
    IDS.get_or_init(|| {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        // "Uid:\treal\teffective\tsaved\tfs"; the effective one decides.
        let field = |name: &str| -> Vec<u32> {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|r| r.split_whitespace().filter_map(|v| v.parse().ok()).collect())
                .unwrap_or_default()
        };
        let uid = field("Uid:");
        let gid = field("Gid:");
        HostIds {
            // Unknown ids (no /proc) count as nobody's, the cautious answer.
            uid: uid.get(1).copied().unwrap_or(u32::MAX - 1),
            gid: gid.get(1).copied().unwrap_or(u32::MAX - 1),
            groups: field("Groups:"),
        }
    })
}

#[cfg(not(unix))]
fn host_access(host: &std::path::Path, _mode: u32) -> Result<(), Errno> {
    std::fs::metadata(host).map(|_| ()).map_err(|e| Errno::from_io(&e))
}
