//! The virtual filesystem: a mount table over pluggable backends.
//!
//! The [`FileSystem`] trait is the seam every backend fits into; paths are
//! resolved against the mount table by longest prefix, so a guest sees one
//! namespace regardless of which backend answers.

use crate::errno::{Errno, SysResult};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod devfs;
// Passthrough to the host's own filesystem; the browser gets `opfs`/`httpfs`
// instead.
#[cfg(unix)]
pub mod hostfs;
pub mod procfs;
/// The filesystem when it is not in this process's memory.
pub mod remote;
pub mod tmpfs;

pub use devfs::DevFs;
#[cfg(unix)]
pub use hostfs::HostFs;
pub use procfs::{ProcFs, ProcSource};
pub use remote::RemoteFs;
pub use tmpfs::TmpFs;

/// What kind of thing a directory entry names.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    Fifo,
    Socket,
}

impl FileType {
    /// The `st_mode` type bits.
    pub fn mode_bits(self) -> u32 {
        match self {
            FileType::Regular => 0o100000,
            FileType::Directory => 0o040000,
            FileType::Symlink => 0o120000,
            FileType::CharDevice => 0o020000,
            FileType::Fifo => 0o010000,
            FileType::Socket => 0o140000,
        }
    }
    /// The `d_type` byte in a `getdents64` record.
    pub fn dirent_type(self) -> u8 {
        match self {
            FileType::Fifo => 1,
            FileType::CharDevice => 2,
            FileType::Directory => 4,
            FileType::Regular => 8,
            FileType::Symlink => 10,
            FileType::Socket => 12,
        }
    }
}

/// Metadata about a path, independent of any open file.
#[derive(Clone, Debug)]
pub struct Metadata {
    pub file_type: FileType,
    pub size: u64,
    pub mode: u32,
    pub ino: u64,
    pub mtime: i64,
    /// Hard-link count, reported as `st_nlink`. A file with one name is 1; a
    /// directory is `2 + immediate subdirectories`.
    pub nlink: u32,
    /// `st_rdev`: the device a character or block device node names, in
    /// Linux's `makedev` encoding. Zero where the node does not say; `stat`
    /// then reports a character device as a pseudo-terminal (136:0).
    pub rdev: u64,
}

impl Metadata {
    pub fn regular(size: u64) -> Metadata {
        Metadata {
            file_type: FileType::Regular,
            size,
            mode: 0o644,
            ino: 1,
            mtime: 0,
            nlink: 1,
            rdev: 0,
        }
    }
    pub fn dir() -> Metadata {
        Metadata {
            file_type: FileType::Directory,
            size: 4096,
            mode: 0o755,
            ino: 1,
            mtime: 0,
            nlink: 2,
            rdev: 0,
        }
    }
}

/// One directory entry.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub file_type: FileType,
    pub ino: u64,
}

/// An open file's backing object.
///
/// Handles are reference-counted so `dup` and `fork` can share one open-file
/// description, which is what makes a shared file offset behave correctly.
pub trait FileHandle: Send + Sync + std::fmt::Debug {
    /// Read at `offset`, returning the bytes read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult;
    /// Write at `offset`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let _ = (offset, buf);
        Err(Errno::EBADF)
    }
    fn metadata(&self) -> Result<Metadata, Errno>;
    /// The id this handle has in the global kernel, when it has one. `fork`
    /// hands the child an id rather than a path, because a file the parent
    /// created and unlinked (as dpkg-deb does) has no name left to reopen.
    fn remote_id(&self) -> Option<i32> {
        None
    }
    /// List a directory. Only directories implement this.
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        Err(Errno::ENOTDIR)
    }
    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let _ = len;
        Err(Errno::EINVAL)
    }
    /// True when reads and writes ignore the offset, as for a terminal or
    /// pipe. Such files also report `ESPIPE` from `lseek`.
    fn is_stream(&self) -> bool {
        false
    }
    /// True when the file should behave like a terminal for `ioctl`.
    fn is_tty(&self) -> bool {
        false
    }
    /// Whether a read would return without waiting. Only streams that can be
    /// empty answer no; a file never waits. Asked *before* the read so an
    /// `O_NONBLOCK` descriptor still sees its own `EAGAIN`.
    fn poll_readable(&self) -> bool {
        true
    }

    /// How many bytes a read would return now, for `FIONREAD`. `None` means
    /// "work it out from size and offset", right for a file but no answer for
    /// a socket -- glibc's resolver sizes its datagram buffer by this.
    fn available(&self) -> Option<u64> {
        None
    }

    /// Whether a write would make progress without waiting. Only a stream
    /// that can fill up answers no.
    fn poll_writable(&self) -> bool {
        true
    }

    /// Whether the other end has hung up: a pipe with no writers left, a
    /// stream socket shut down in both directions (its peer closed).
    /// `poll` and `epoll` report `POLLHUP` for it whether or not it was
    /// asked for, as Linux does; a server learns that a client died this way.
    fn poll_hup(&self) -> bool {
        false
    }

    /// A short description of this handle's state, for a diagnostic: the
    /// buffer behind a descriptor -- empty, full, which way it points -- that
    /// the fd number and path cannot show.
    fn debug_state(&self) -> String {
        String::new()
    }

    /// An `ioctl` this handle answers itself. `None` means "not mine" and the
    /// kernel's own handling runs. Guest memory is passed because an ioctl arg
    /// is usually a pointer to a struct the device reads or writes -- and a
    /// host device reaches the caller's buffers through it uncopied.
    /// A session has claimed this file as its controlling terminal.
    ///
    /// `TIOCSCTTY` on a *local* tty is handled at the syscall layer, which has
    /// the process table and can record the session there. A pty in the global
    /// kernel cannot be reached that way -- its state is in another module --
    /// so the syscall layer hands the session down here instead. Nothing else
    /// needs it, hence the default.
    fn claim_session(&self, _sid: i32) {}

    /// Whether another *process* is what ends a wait on this handle, rather
    /// than the host.
    ///
    /// A host with one thread per process has to know the difference. A guest
    /// waiting for a pipe, a unix socket or the far end of a pseudoterminal is
    /// waiting for a program that runs elsewhere and will say so when it gets
    /// there; a guest waiting for a keystroke, or for bytes off a socket the
    /// host owns, is waiting for the host to do something -- and a host that is
    /// inside this guest's syscall is not doing it. Answer `true` and the
    /// browser shim sleeps on the shared wake word; answer `false` and it hands
    /// the page its event loop back.
    ///
    /// Pipes and sockets are recognised by their file type. A pseudoterminal is
    /// not: it looks exactly like the terminal a person types into, and the two
    /// want opposite answers, so it says which it is here.
    fn peer_wakes(&self) -> bool {
        false
    }

    fn ioctl(&self, m: &mut crate::GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        let _ = (m, request, arg);
        None
    }

    /// How many bytes a write can take now, when bounded. `None` means it is
    /// not (file, terminal, socket). A pipe is bounded, and a blocking write
    /// larger than its free space is carried to completion by the syscall layer.
    fn write_room(&self) -> Option<u64> {
        None
    }

    /// Arm or disarm a `timerfd`, returning the previous `(remaining, interval)`
    /// in nanoseconds. `None` means this handle is not a timerfd, which the
    /// `timerfd_settime` syscall reports as `EINVAL`.
    fn timerfd_set(&self, value_ns: u64, interval_ns: u64, abstime: bool) -> Option<(u64, u64)> {
        let _ = (value_ns, interval_ns, abstime);
        None
    }

    /// The current `(remaining, interval)` in nanoseconds of a `timerfd`, or
    /// `None` when this handle is not one.
    fn timerfd_get(&self) -> Option<(u64, u64)> {
        None
    }
}

/// A descriptor that names a file without opening it: `O_PATH`. Answers
/// `fstat`, carries its path for `*at` calls, refuses the rest. Legal on
/// things an ordinary open is not, e.g. a dangling symlink.
#[derive(Debug)]
pub struct PathHandle(pub Metadata);

impl FileHandle for PathHandle {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EBADF)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EBADF)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(self.0.clone())
    }
}

/// A mounted filesystem.
pub trait FileSystem: Send + Sync {
    /// Open a path relative to this mount's root (no leading slash).
    fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno>;
    fn metadata(&self, path: &str) -> Result<Metadata, Errno>;
    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let _ = path;
        Err(Errno::EINVAL)
    }
    fn unlink(&self, path: &str) -> Result<(), Errno> {
        let _ = path;
        Err(Errno::EROFS)
    }
    fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let _ = (path, mode);
        Err(Errno::EROFS)
    }
    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let _ = path;
        Err(Errno::EROFS)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let _ = (from, to);
        Err(Errno::EROFS)
    }
    /// Change a file's mode. Filesystems that do not model permissions
    /// accept it and do nothing, which is what the guest expects to see.
    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let _ = (path, mode);
        Ok(())
    }

    /// A second name for the same file. `EPERM` is what Linux says for a
    /// filesystem that has no hard links, which is the honest default.
    fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        let _ = (from, to);
        Err(Errno::EPERM)
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let _ = (target, path);
        Err(Errno::EROFS)
    }

    /// Place a node that `stat` reports as a socket. A bound UNIX socket must
    /// exist in the filesystem too, not just the socket namespace, or
    /// `test -e /tmp/.X11-unix/X0` says no while the X server is listening.
    fn mksocket(&self, path: &str) -> Result<(), Errno> {
        let _ = path;
        Err(Errno::EROFS)
    }
}

/// The mount table.
#[derive(Clone)]
pub struct Vfs {
    /// Mount point (absolute, no trailing slash except for "/") to backend.
    mounts: BTreeMap<String, Arc<dyn FileSystem>>,
    /// Current working directory, always absolute and normalised.
    pub cwd: String,
}

impl Vfs {
    pub fn new() -> Vfs {
        Vfs {
            mounts: BTreeMap::new(),
            cwd: "/".to_string(),
        }
    }

    pub fn mount(&mut self, at: &str, fs: Arc<dyn FileSystem>) {
        let at = normalize(at, "/");
        self.mounts.insert(at, fs);
    }

    /// Whether something is mounted at exactly this path.
    pub fn is_mount_point(&self, path: &str) -> bool {
        self.mounts.contains_key(&normalize(path, &self.cwd))
    }

    /// Remove the mount at exactly this path. The root mount stays: a guest
    /// that unmounts `/` has nothing left to run from.
    pub fn unmount(&mut self, path: &str) -> bool {
        let at = normalize(path, &self.cwd);
        at != "/" && self.mounts.remove(&at).is_some()
    }

    /// Find the backend responsible for `path`, and the path relative to it.
    ///
    /// Longest prefix wins, so a `/proc` mount shadows the root mount for
    /// everything beneath it.
    fn resolve<'a>(&'a self, path: &str) -> Result<(&'a Arc<dyn FileSystem>, String), Errno> {
        let abs = normalize(path, &self.cwd);
        let mut best: Option<(&String, &Arc<dyn FileSystem>)> = None;
        for (at, fs) in &self.mounts {
            let matches = if at == "/" {
                true
            } else {
                abs == *at || abs.starts_with(&format!("{at}/"))
            };
            if matches && best.map(|(b, _)| at.len() > b.len()).unwrap_or(true) {
                best = Some((at, fs));
            }
        }
        let (at, fs) = best.ok_or(Errno::ENOENT)?;
        let rel = if at == "/" {
            abs.trim_start_matches('/').to_string()
        } else {
            abs[at.len()..].trim_start_matches('/').to_string()
        };
        Ok((fs, rel))
    }

    /// Open a path, following symlinks up to a bounded depth.
    pub fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let mut current = normalize(path, &self.cwd);
        for _ in 0..40 {
            let (fs, rel) = self.resolve(&current)?;
            // O_NOFOLLOW and O_PATH are not modelled; a symlink is always
            // followed, which is what every caller here wants.
            match fs.read_link(&rel) {
                Ok(target) => {
                    current = if target.starts_with('/') {
                        normalize(&target, "/")
                    } else {
                        let parent = parent_of(&current);
                        normalize(&target, &parent)
                    };
                }
                Err(_) => return fs.open(&rel, flags, mode),
            }
        }
        Err(Errno::ELOOP)
    }

    pub fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let mut current = normalize(path, &self.cwd);
        for _ in 0..40 {
            let (fs, rel) = self.resolve(&current)?;
            match fs.read_link(&rel) {
                Ok(target) => {
                    current = if target.starts_with('/') {
                        normalize(&target, "/")
                    } else {
                        normalize(&target, &parent_of(&current))
                    };
                }
                Err(_) => return fs.metadata(&rel),
            }
        }
        Err(Errno::ELOOP)
    }

    /// Metadata without following a final symlink, for `lstat`.
    pub fn metadata_nofollow(&self, path: &str) -> Result<Metadata, Errno> {
        let (fs, rel) = self.resolve(path)?;
        if let Ok(target) = fs.read_link(&rel) {
            return Ok(Metadata {
                file_type: FileType::Symlink,
                size: target.len() as u64,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
            });
        }
        fs.metadata(&rel)
    }

    pub fn read_link(&self, path: &str) -> Result<String, Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.read_link(&rel)
    }

    pub fn unlink(&self, path: &str) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.unlink(&rel)
    }

    pub fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.mkdir(&rel, mode)
    }

    pub fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.rmdir(&rel)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let (fs, rel_from) = self.resolve(from)?;
        let (fs_to, rel_to) = self.resolve(to)?;
        // Crossing a mount boundary needs a copy, which Linux also refuses.
        if !Arc::ptr_eq(fs, fs_to) {
            return Err(Errno::EXDEV);
        }
        fs.rename(&rel_from, &rel_to)
    }

    pub fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.chmod(&rel, mode)
    }

    /// `link`. Both names must be on one filesystem, as they must on Linux:
    /// a hard link is a second directory entry for the same inode, and an
    /// inode does not span mounts.
    pub fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        let (fs, rel_from) = self.resolve(from)?;
        let (fs_to, rel_to) = self.resolve(to)?;
        if !Arc::ptr_eq(fs, fs_to) {
            return Err(Errno::EXDEV);
        }
        fs.link(&rel_from, &rel_to)
    }

    /// Create a filesystem node for a bound UNIX socket.
    pub fn mksocket(&self, path: &str) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.mksocket(&rel)
    }

    pub fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.symlink(target, &rel)
    }

    /// Resolve a path to its absolute, normalised form.
    pub fn absolute(&self, path: &str) -> String {
        normalize(path, &self.cwd)
    }

    pub fn set_cwd(&mut self, path: &str) -> Result<(), Errno> {
        let abs = normalize(path, &self.cwd);
        let md = self.metadata(&abs)?;
        if md.file_type != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        self.cwd = abs;
        Ok(())
    }
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

/// Make a path absolute and remove `.`, `..` and redundant separators.
///
/// Purely lexical, like `realpath -m`: symlinks are resolved separately, by
/// the caller, so that a loop can be bounded.
pub fn normalize(path: &str, cwd: &str) -> String {
    let combined = if path.starts_with('/') {
        path.to_string()
    } else if cwd == "/" {
        format!("/{path}")
    } else {
        format!("{cwd}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in combined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// The directory containing `path`.
pub fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// The final component of a path.
pub fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Serialise directory entries into the `getdents64` record format.
///
/// Returns the bytes written and how many entries were consumed, so the
/// caller can resume from the right place on the next call.
pub fn encode_dirents(entries: &[DirEntry], start: usize, buf_len: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut consumed = 0;
    for (i, e) in entries.iter().enumerate().skip(start) {
        let name = e.name.as_bytes();
        // struct linux_dirent64 { u64 ino; i64 off; u16 reclen; u8 type; char name[]; }
        let reclen = (19 + name.len() + 1).div_ceil(8) * 8;
        if out.len() + reclen > buf_len {
            break;
        }
        let mut rec = vec![0u8; reclen];
        rec[0..8].copy_from_slice(&e.ino.to_le_bytes());
        rec[8..16].copy_from_slice(&((i + 1) as i64).to_le_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = e.file_type.dirent_type();
        rec[19..19 + name.len()].copy_from_slice(name);
        out.extend_from_slice(&rec);
        consumed = i + 1 - start;
    }
    (out, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_dots_and_roots() {
        assert_eq!(normalize("/a/b/../c", "/"), "/a/c");
        assert_eq!(normalize("b/c", "/a"), "/a/b/c");
        assert_eq!(normalize("..", "/a/b"), "/a");
        assert_eq!(normalize("../../..", "/a"), "/");
        assert_eq!(normalize(".", "/a/b"), "/a/b");
        assert_eq!(normalize("//a///b//", "/"), "/a/b");
        assert_eq!(normalize("/", "/x"), "/");
    }

    #[test]
    fn parent_and_basename() {
        assert_eq!(parent_of("/a/b/c"), "/a/b");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(basename("/a/b/c"), "c");
        assert_eq!(basename("c"), "c");
    }

    #[test]
    fn longest_mount_prefix_wins() {
        let mut vfs = Vfs::new();
        vfs.mount("/", Arc::new(TmpFs::new()));
        vfs.mount("/proc", Arc::new(TmpFs::new()));
        let (_, rel) = vfs.resolve("/proc/self/maps").unwrap();
        assert_eq!(rel, "self/maps");
        let (_, rel) = vfs.resolve("/etc/passwd").unwrap();
        assert_eq!(rel, "etc/passwd");
        // A path that merely starts with the same letters is not inside it.
        let (_, rel) = vfs.resolve("/procession").unwrap();
        assert_eq!(rel, "procession");
    }

    #[test]
    fn dirents_encode_and_stop_at_the_buffer() {
        let entries = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
            DirEntry {
                name: "file".into(),
                file_type: FileType::Regular,
                ino: 2,
            },
        ];
        let (buf, n) = encode_dirents(&entries, 0, 4096);
        assert_eq!(n, 2);
        // Each record is 8-byte aligned and self-describing.
        let reclen = u16::from_le_bytes(buf[16..18].try_into().unwrap()) as usize;
        assert_eq!(reclen % 8, 0);
        assert_eq!(buf[18], FileType::Directory.dirent_type());
        assert_eq!(&buf[19..20], b".");
        // A buffer that fits nothing consumes nothing.
        let (buf, n) = encode_dirents(&entries, 0, 8);
        assert!(buf.is_empty());
        assert_eq!(n, 0);
    }
}
