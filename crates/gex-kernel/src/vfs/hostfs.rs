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
    /// `root` as the host resolves it, once asked (`HostDir::moved`).
    canon: std::sync::OnceLock<Option<PathBuf>>,
    /// The rename lock of the renames made through this mount. The host's
    /// own, and another process's, are not seen by it: a lookup racing
    /// one of those can still miss.
    gate: Arc<super::RenameGate>,
    /// `root` held open, once a walk asks ([`host_walk`]).
    root_dir: std::sync::OnceLock<Option<File>>,
}

impl HostFs {
    pub fn new(root: impl Into<PathBuf>) -> HostFs {
        HostFs {
            root: root.into(),
            read_only: false,
            canon: Default::default(),
            gate: Default::default(),
            root_dir: Default::default(),
        }
    }

    pub fn read_only(root: impl Into<PathBuf>) -> HostFs {
        HostFs {
            root: root.into(),
            read_only: true,
            canon: Default::default(),
            gate: Default::default(),
            root_dir: Default::default(),
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

/// A host directory's entries, `.` and `..` first; `md` is its own.
fn list_dir(host: &Path, md: &Metadata) -> Result<Vec<DirEntry>, Errno> {
    let mut entries = vec![
        DirEntry {
            name: ".".into(),
            file_type: FileType::Directory,
            ino: md.ino,
        },
        DirEntry {
            name: "..".into(),
            file_type: FileType::Directory,
            ino: 1,
        },
    ];
    // Each entry's type and inode as the directory read gives them
    // (d_type, d_ino), as Linux's getdents does: no call per entry. A
    // filesystem that gives no type is asked, by lstat.
    for e in std::fs::read_dir(&host).map_err(|e| Errno::from_io(&e))? {
        use std::os::unix::fs::DirEntryExt;
        let e = e.map_err(|e| Errno::from_io(&e))?;
        let ft = e.file_type().map_err(|e| Errno::from_io(&e))?;
        entries.push(DirEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            file_type: entry_type(ft),
            ino: e.ino(),
        });
    }
    Ok(entries)
}

/// [`file_type_of`] for a directory entry's type.
fn entry_type(t: std::fs::FileType) -> FileType {
    use std::os::unix::fs::FileTypeExt;
    if t.is_dir() {
        FileType::Directory
    } else if t.is_symlink() {
        FileType::Symlink
    } else if t.is_file() {
        FileType::Regular
    } else if t.is_socket() {
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
        times: super::Times {
            mtime_ns: md.mtime_nsec() as u32,
            atime: md.atime(),
            atime_ns: md.atime_nsec() as u32,
            ctime: md.ctime(),
            ctime_ns: md.ctime_nsec() as u32,
        },
    }
}

/// An open host file.
#[derive(Debug)]
struct HostHandle {
    file: Mutex<File>,
    /// Not a regular file (a FIFO, a socket, a device): no offset.
    stream: bool,
}

/// `(seconds, nanoseconds)` since the epoch as a host time.
fn host_time((secs, ns): (i64, u32)) -> std::time::SystemTime {
    let d = std::time::Duration::new(secs.unsigned_abs(), 0);
    let t = if secs >= 0 { std::time::UNIX_EPOCH + d } else { std::time::UNIX_EPOCH - d };
    t + std::time::Duration::from_nanos(ns as u64)
}

fn file_times(atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> std::fs::FileTimes {
    let mut t = std::fs::FileTimes::new();
    if let Some(a) = atime {
        t = t.set_accessed(host_time(a));
    }
    if let Some(m) = mtime {
        t = t.set_modified(host_time(m));
    }
    t
}

impl FileHandle for HostHandle {
    fn xattr(&self, op: super::XattrOp<'_>) -> Option<Result<usize, Errno>> {
        use std::os::unix::io::AsRawFd;
        let f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        Some(host_xattr::call(host_xattr::On::Fd(f.as_raw_fd()), op))
    }

    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.file.lock().unwrap().set_times(file_times(atime, mtime)).map_err(|e| Errno::from_io(&e))
    }

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
    /// Where it was opened, on the host: what is listed when the directory
    /// is not held open.
    host: PathBuf,
    md: Metadata,
    /// The directory held open, the path it was opened at (relative to the
    /// mount) and the mount's root as the host resolves it: where the
    /// directory is now is what the host says of the open descriptor
    /// ([`FileHandle::moved`]).
    held: Option<(File, String, PathBuf)>,
    gate: Arc<super::RenameGate>,
}

impl FileHandle for HostDir {
    fn xattr(&self, op: super::XattrOp<'_>) -> Option<Result<usize, Errno>> {
        use std::os::unix::io::AsRawFd;
        let (f, _, _) = self.held.as_ref()?;
        Some(host_xattr::call(host_xattr::On::Fd(f.as_raw_fd()), op))
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(self.md.clone())
    }
    /// Listed when asked, not at the open, as Linux reads a directory at
    /// `getdents`: an open for `fstatfs` or an ioctl lists nothing. The
    /// directory held open is listed, wherever it is now.
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        use std::os::fd::AsRawFd;
        let at = match &self.held {
            Some((f, _, _)) if cfg!(target_os = "linux") => PathBuf::from(format!("/proc/self/fd/{}", f.as_raw_fd())),
            _ => self.host.clone(),
        };
        list_dir(&at, &self.md)
    }
    #[cfg(target_os = "linux")]
    fn moved(&self) -> Option<Result<super::Moved, Errno>> {
        use std::os::fd::AsRawFd;
        let (file, was, root) = self.held.as_ref()?;
        let now = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
        let now = now.to_str()?;
        if now.ends_with(" (deleted)") {
            return Some(Err(Errno::ENOENT));
        }
        // Moved out of the mount: the path it was opened at is all there is.
        let rel = Path::new(now).strip_prefix(root).ok()?;
        // Any directory renamed here counts as this one moving: the host
        // keeps no count per directory.
        Some(Ok(super::Moved { was: was.clone(), now: rel.to_str()?.to_string(), epoch: self.gate.epoch() }))
    }

    // No `rename_epoch`: the host renames directories too, so where the
    // directory is is asked each time it matters.
    fn own_rename_epoch(&self) -> Option<u64> {
        Some(self.gate.epoch())
    }

    fn hold_renames(&self, owner: i32, on: bool) -> bool {
        self.gate.hold(owner, on);
        true
    }
}

impl FileSystem for HostFs {
    /// [`super::walk_links`], which asks the host about each directory on
    /// the path (a call per component), but with the host's word on a
    /// whole stretch of the path at once ([`host_walk`]). No link on the
    /// path, the common case and every miss: one call. A link: the first
    /// one found by halving (a call per halving), followed, and the rest of
    /// the path after it the same way. A mount on the path, or a host that
    /// cannot say: `walk_links`.
    fn walk(&self, abs: &str, follow_final: bool, mounts: &str) -> Result<super::Walked, Errno> {
        use std::os::fd::AsRawFd;
        let long = |abs: String| super::walk_links(abs, follow_final, mounts, &|rel| self.read_link(rel));
        let mut abs = if super::is_normal(abs) { abs.to_string() } else { super::normalize(abs, "/") };
        let Some(no_links) = host_walk::hook() else { return long(abs) };
        let Some(root) = self.root_dir.get_or_init(|| File::open(&self.root).ok()) else { return long(abs) };
        let root = root.as_raw_fd();
        for _ in 0..40 {
            // what the walk looks at: the directories, and with
            // `follow_final` the last component too
            let end = if follow_final { abs.len() } else { abs.rfind('/').unwrap_or(0) };
            let looked = &abs[..end];
            // a mount on it resolves its own names
            if !looked.is_empty() && mounts.split('\0').any(|m| !m.is_empty() && super::within(looked, m)) {
                return long(abs);
            }
            let rel = looked.trim_start_matches('/');
            if rel.split('/').any(|c| c == "..") {
                return long(abs);
            }
            if no_links(root, rel) {
                return Ok(super::Walked { path: abs, at_mount: false });
            }
            // The first component whose prefix has a link: every prefix
            // before `lo` components has none, the one of `hi` has.
            let ends: Vec<usize> = rel.match_indices('/').map(|(i, _)| i).chain([rel.len()]).collect();
            let (mut lo, mut hi) = (0, ends.len());
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if no_links(root, &rel[..ends[mid - 1]]) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let link = &rel[..ends[hi - 1]];
            let Ok(target) = self.read_link(link) else {
                // not a link after all (a host error): the long way
                return long(abs);
            };
            let prefix = format!("/{link}");
            let base = if target.starts_with('/') {
                super::normalize(&target, "/")
            } else {
                super::normalize(&target, &super::parent_of(&prefix))
            };
            abs = super::normalize(abs[prefix.len()..].trim_start_matches('/'), &base);
        }
        Err(Errno::ELOOP)
    }

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
                // one the host will not open (no read permission) fails here,
                // as on Linux, though it is listed only later
                let f = File::open(&host).map_err(|e| Errno::from_io(&e))?;
                let held = self.canon.get_or_init(|| self.root.canonicalize().ok()).clone().map(|root| (f, path.to_string(), root));
                return Ok(Arc::new(HostDir {
                    host,
                    md: convert(md),
                    held,
                    gate: Arc::clone(&self.gate),
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
        std::fs::remove_dir(self.host_path(path)?).map_err(|e| Errno::from_io(&e))?;
        self.gate.bump();
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        let (from, to) = (self.host_path(from)?, self.host_path(to)?);
        let dir = std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_dir());
        self.gate.rename(super::own_holds(), dir, || std::fs::rename(&from, &to).map_err(|e| Errno::from_io(&e)))
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

    fn xattr(&self, path: &str, op: super::XattrOp<'_>) -> Result<usize, Errno> {
        if self.read_only && matches!(op, super::XattrOp::Set { .. } | super::XattrOp::Remove { .. }) {
            return Err(Errno::EROFS);
        }
        host_xattr::call(host_xattr::On::Path(&self.host_path(path)?), op)
    }

    fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        if self.read_only {
            return Err(Errno::EROFS);
        }
        let host = self.host_path(path)?;
        // A symlink's own times (`utimensat` with AT_SYMLINK_NOFOLLOW: dpkg
        // stamps every link it unpacks, before its target exists): std opens
        // only what a link names, so this leaves the link's times as they are.
        if std::fs::symlink_metadata(&host).is_ok_and(|m| m.file_type().is_symlink()) {
            return Ok(());
        }
        let f = std::fs::File::open(host).map_err(|e| Errno::from_io(&e))?;
        f.set_times(file_times(atime, mtime)).map_err(|e| Errno::from_io(&e))
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

/// Extended attributes on the host's files, `user.` only either way: a
/// guest neither sets nor sees the host's other namespaces. The calls
/// themselves are the host's ([`set_host_xattr`]: this crate has no unsafe
/// code); without one, `EOPNOTSUPP`.
/// The host's word on a whole path at once, for [`HostFs`]'s walk. The call
/// is the host's (the native shim's `openat2` with `RESOLVE_NO_SYMLINKS`;
/// this crate has no unsafe), so it is a hook.
pub mod host_walk {
    /// Whether the host meets no symbolic link walking `rel` (relative,
    /// normalised, no `..`) from the directory open as `root`, up to the
    /// end of `rel` or to its first missing component, whichever comes
    /// first. `false` when it meets one, or cannot say.
    pub type NoLinks = fn(root: i32, rel: &str) -> bool;

    static HOOK: std::sync::OnceLock<NoLinks> = std::sync::OnceLock::new();

    /// Install the host's call (the native shim's). The first call wins.
    pub fn set_host_no_links(f: NoLinks) {
        let _ = HOOK.set(f);
    }

    pub(super) fn hook() -> Option<NoLinks> {
        HOOK.get().copied()
    }
}

pub mod host_xattr {
    use super::super::{xattr_name, XattrOp};
    use crate::errno::Errno;

    /// The node: a host path (its final link not followed: the guest's walk
    /// already followed it, or was asked not to) or an open descriptor.
    pub enum On<'a> {
        Path(&'a std::path::Path),
        Fd(i32),
    }

    /// The host's `l*xattr`/`f*xattr` calls, one [`XattrOp`] on `on`: what
    /// they return, or the host's errno.
    pub type Calls = fn(on: &On<'_>, op: XattrOp<'_>) -> Result<usize, Errno>;

    static CALLS: std::sync::OnceLock<Calls> = std::sync::OnceLock::new();

    /// Install the host's calls (the native shim's). The first call wins.
    pub fn set_host_xattr(calls: Calls) {
        let _ = CALLS.set(calls);
    }

    pub fn call(on: On<'_>, op: XattrOp<'_>) -> Result<usize, Errno> {
        let calls = CALLS.get().ok_or(Errno::EOPNOTSUPP)?;
        let not_user = |e: Errno| if e == Errno::EOPNOTSUPP { Errno::ENODATA } else { e };
        match op {
            XattrOp::Get { name, buf } => {
                xattr_name(name).map_err(not_user)?;
                calls(&on, XattrOp::Get { name, buf })
            }
            XattrOp::Set { name, value, flags } => {
                xattr_name(name)?;
                calls(&on, XattrOp::Set { name, value, flags })
            }
            XattrOp::Remove { name } => {
                xattr_name(name).map_err(not_user)?;
                calls(&on, XattrOp::Remove { name })
            }
            XattrOp::List { buf } => {
                // The host's whole list, then only its `user.` names.
                let size = calls(&on, XattrOp::List { buf: &mut [] })?;
                let mut all = vec![0u8; size];
                let got = calls(&on, XattrOp::List { buf: &mut all })?;
                all.truncate(got);
                let mut mine = Vec::new();
                for name in all.split(|&c| c == 0).filter(|n| n.starts_with(b"user.")) {
                    mine.extend_from_slice(name);
                    mine.push(0);
                }
                if buf.is_empty() {
                    return Ok(mine.len());
                }
                if buf.len() < mine.len() {
                    return Err(Errno::ERANGE);
                }
                buf[..mine.len()].copy_from_slice(&mine);
                Ok(mine.len())
            }
        }
    }
}

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

    /// The host's call as the native shim makes it, in safe code: a
    /// component at a time (what `openat2(RESOLVE_NO_SYMLINKS)` answers).
    fn emulated_no_links(root: i32, rel: &str) -> bool {
        let mut at = PathBuf::from(format!("/proc/self/fd/{root}"));
        for c in rel.split('/').filter(|c| !c.is_empty()) {
            at.push(c);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.is_symlink() => return false,
                Ok(_) => {}
                Err(e) => return matches!(e.raw_os_error(), Some(2) | Some(20)),
            }
        }
        true
    }

    #[test]
    fn the_hosts_walk_follows_what_walk_links_follows() {
        use crate::vfs::FileSystem;
        let dir = scratch().join("walk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("a/b/c/f"), b"").unwrap();
        let link = |t: &str, at: &str| std::os::unix::fs::symlink(t, dir.join(at)).unwrap();
        link("a/b", "l");
        link("/a", "abs");
        link("../..", "a/b/up");
        link("nothere", "dangle");
        link("l", "l2");
        link("loop2", "loop1");
        link("loop1", "loop2");
        let fs = HostFs::new(&dir);
        host_walk::set_host_no_links(emulated_no_links);
        let paths = [
            "/a/b/c/f", "/l/c/f", "/l2/c/f", "/abs/b/c", "/a/b/up/l/c", "/a/b/up/a/b/up/abs/b", "/dangle",
            "/dangle/x", "/l", "/l2", "/missing/l/c", "/a/b/c/f/g", "/loop1/x", "/loop1", "/", "/a/./b/../b/c",
        ];
        for mounts in ["", "\0/a/b", "\0/proc\0/a"] {
            for p in paths {
                for follow in [false, true] {
                    let long = crate::vfs::walk_links(p.to_string(), follow, mounts, &|rel| fs.read_link(rel));
                    let short = fs.walk(p, follow, mounts);
                    let show = |w: &Result<crate::vfs::Walked, Errno>| w.as_ref().map(|w| (w.path.clone(), w.at_mount)).map_err(|e| *e);
                    assert_eq!(show(&short), show(&long), "{p} follow {follow} mounts {mounts:?}");
                }
            }
        }
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
