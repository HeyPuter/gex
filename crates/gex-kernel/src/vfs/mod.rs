//! The virtual filesystem: a mount table over pluggable backends.
//!
//! The [`FileSystem`] trait is the seam every backend fits into; paths are
//! resolved against the mount table by longest prefix, so a guest sees one
//! namespace regardless of which backend answers.

use gex_abi::GuestEndian;
use crate::errno::{Errno, SysResult};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod devfs;
// Passthrough to the host's own filesystem; the browser gets `opfs`/`httpfs`
// instead.
#[cfg(unix)]
pub mod hostfs;
pub mod oss;
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

/// An inode number for an object no file system holds (a pipe, a socket,
/// the host's stdio): unique among live objects, from the object's address,
/// in a range no file system's inodes reach. Programs compare (dev, ino) to
/// tell files apart -- GNU grep takes an output that matches /dev/null's for
/// /dev/null and prints nothing -- so these must not all be 1.
pub fn anon_ino(p: *const u8) -> u64 {
    (1 << 62) | (p as usize as u64 >> 3)
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
    /// The other times and the nanoseconds, where the filesystem keeps
    /// them (the host's). All zero: every time is `mtime`, whole seconds.
    pub times: Times,
}

/// `st_atim`, `st_ctim` and the nanoseconds of `st_mtim`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Times {
    pub mtime_ns: u32,
    pub atime: i64,
    pub atime_ns: u32,
    pub ctime: i64,
    pub ctime_ns: u32,
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
            times: Default::default(),
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
            times: Default::default(),
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
    /// Where a directory opened here is now, for a descriptor that relative
    /// paths resolve through (`openat(dirfd, ...)`, `fchdir`), or `ENOENT`
    /// once it is removed. Linux resolves such a descriptor by the
    /// directory, not by its name, so it follows a rename of the directory
    /// or of any directory above it. `None` for a handle that does not
    /// follow its node: its recorded path stands.
    fn moved(&self) -> Option<Result<Moved, Errno>> {
        None
    }
    /// How many directories this handle's filesystem has renamed or removed, read
    /// without a lock: while it stays the same, a directory's path found
    /// by [`FileHandle::moved`] still stands (a working directory keeps its
    /// path this way, `FsCwd`). `None` for a filesystem that does not count.
    fn rename_epoch(&self) -> Option<u64> {
        None
    }
    /// Hold off the renames of directories on this handle's filesystem
    /// (`on`), or let them go again, for process `owner`: its rename lock,
    /// taken shared. What a lookup through a directory descriptor that kept
    /// racing renames falls back to, as Linux's walk falls back to
    /// `rename_lock` (`PathIo::call`). Whether the filesystem has one.
    fn hold_renames(&self, owner: i32, on: bool) -> bool {
        let _ = (owner, on);
        false
    }
    /// List a directory. Only directories implement this.
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        Err(Errno::ENOTDIR)
    }
    /// Set the modification time of what this handle is open on. A handle
    /// onto something that keeps no times accepts it and does nothing.
    fn set_mtime(&self, _secs: i64) -> Result<(), Errno> {
        Ok(())
    }
    /// `futimens`: [`FileSystem::set_times`] on an open file.
    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        let _ = atime;
        match mtime {
            Some((secs, _)) => self.set_mtime(secs),
            None => Ok(()),
        }
    }
    /// [`FileHandle::set_times`] with the change time given (`now`, whole
    /// seconds): for a module with no clock of its own -- the global kernel,
    /// told the time by the process that asked.
    fn set_times_now(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) -> Result<(), Errno> {
        let _ = now;
        self.set_times(atime, mtime)
    }
    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let _ = len;
        Err(Errno::EINVAL)
    }
    /// `fchmod` on what this handle is open on, when the handle can do it
    /// itself; `None` sends the caller to [`FileSystem::chmod`] by name.
    fn chmod(&self, mode: u32) -> Option<Result<(), Errno>> {
        let _ = mode;
        None
    }
    /// True when reads and writes ignore the offset, as for a terminal or
    /// pipe. Such files also report `ESPIPE` from `lseek`.
    fn is_stream(&self) -> bool {
        false
    }
    /// `lseek` on a handle that keeps its own offset (a standard stream
    /// redirected from a host file: the offset is the host descriptor's).
    /// `None`: the open-file description's offset, or `ESPIPE` for a stream.
    fn seek(&self, _offset: i64, _whence: u32) -> Option<SysResult> {
        None
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

    /// The wait queue a change in this handle's readiness is announced on
    /// ([`crate::wait`]): a thread waiting for it registers its wait slot
    /// there. 0 for a handle whose readiness no event changes (a file is
    /// always ready; a timer becomes ready by the clock, [`Self::ready_in_ns`]).
    fn wait_queue(&self) -> u32 {
        0
    }

    /// How long until this handle becomes ready by the clock alone (a
    /// timerfd's next expiry, a sound device draining its buffer), for a
    /// waiter to bound its park by. `None` when only an event makes it ready.
    fn ready_in_ns(&self) -> Option<u64> {
        None
    }

    /// Whether the peer has shut its sending side, so reading ends at EOF
    /// once the buffer is empty: `POLLRDHUP` / `EPOLLRDHUP`, reported only
    /// when asked for.
    fn poll_rdhup(&self) -> bool {
        false
    }

    /// This handle as a unix socket, when it is one: the syscalls that need
    /// the concrete type (descriptor passing, `listen`, `accept`) recover it
    /// from the handle, whichever process made it.
    fn as_unix_socket(self: Arc<Self>) -> Option<Arc<crate::unix::UnixSocket>> {
        None
    }

    /// This handle as a unix socket in the global kernel, when it is one.
    fn as_remote_unix(self: Arc<Self>) -> Option<Arc<crate::unix::RemoteUnix>> {
        None
    }

    /// This handle as an internet socket, when it is one.
    fn as_inet_socket(self: Arc<Self>) -> Option<Arc<crate::net::Socket>> {
        None
    }

    /// This handle as an epoll set, when it is one: `epoll_ctl` and
    /// `epoll_pwait` recover the set from the descriptor's handle, with or
    /// without the kernel.
    fn as_epoll(self: Arc<Self>) -> Option<Arc<crate::unix::EpollSet>> {
        None
    }

    /// This handle as an anonymous shared file (`memfd_create`), when it is
    /// one: its seals and `fallocate`.
    fn as_memfd(self: Arc<Self>) -> Option<Arc<crate::unix::MemFd>> {
        None
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

    /// Whether this is a socket in the global kernel (`unix::RemoteUnix`):
    /// what a host asks of a parked thread's handle without the kernel.
    fn is_global_socket(&self) -> bool {
        false
    }

    /// Whether a read on this handle, made now, can end nobody's wait. Asked
    /// *before* the read, of a stream only (a read of anything else never
    /// ends a wait). A read ends a writer's wait by making room, and a writer
    /// parks only when there is none, so a stream that knows it has room
    /// says so here and the read is not followed by a wake of every parked
    /// Worker. `false`, "it might", is always safe.
    fn read_frees_nothing(&self) -> bool {
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

static OWNER_ALIVE: std::sync::OnceLock<fn(i32) -> bool> = std::sync::OnceLock::new();

/// How a filesystem shared by several processes tells whether the process
/// holding its directory renames off is still there
/// ([`FileHandle::hold_renames`]): one that ended without letting go holds
/// nothing. The first call wins; without one every holder is.
pub fn set_owner_alive(alive: fn(i32) -> bool) {
    let _ = OWNER_ALIVE.set(alive);
}

thread_local! {
    /// The holds this thread has taken: a rename it makes itself does not
    /// wait for them. Counted by the caller of `hold_renames` (`PathIo`),
    /// on its own thread: the global kernel's thread-locals are not one per
    /// thread of the process calling it, so a remote rename carries the
    /// count with it.
    static OWN_HOLDS: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// The holds the calling thread has taken ([`FileHandle::hold_renames`]).
pub fn own_holds() -> u32 {
    OWN_HOLDS.with(|h| h.get())
}

/// The calling thread took a hold (`on`) or let one go.
pub fn note_own_hold(on: bool) {
    OWN_HOLDS.with(|h| h.set(if on { h.get() + 1 } else { h.get().saturating_sub(1) }));
}

/// A filesystem's rename lock (Linux's `rename_lock`, per filesystem): the
/// count of its directories renamed or removed, read without a lock
/// ([`FileHandle::rename_epoch`]), and the lookups holding directory renames
/// off, by process ([`FileHandle::hold_renames`]). A directory rename runs
/// in [`RenameGate::rename`], which waits while another thread holds, and a
/// hold waits for a rename under way.
#[derive(Debug, Default)]
pub struct RenameGate {
    renamed: std::sync::atomic::AtomicU64,
    holds: std::sync::Mutex<std::collections::BTreeMap<i32, u32>>,
}

impl RenameGate {
    pub fn epoch(&self) -> u64 {
        self.renamed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A directory renamed or removed.
    pub fn bump(&self) {
        self.renamed.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<i32, u32>> {
        self.holds.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// [`FileHandle::hold_renames`].
    pub fn hold(&self, owner: i32, on: bool) {
        let mut holds = self.lock();
        if on {
            *holds.entry(owner).or_insert(0) += 1;
        } else if let Some(n) = holds.get_mut(&owner) {
            *n -= 1;
            if *n == 0 {
                holds.remove(&owner);
            }
        }
    }

    /// Run `f`, a directory rename (`dir`) or another, once no thread but
    /// the caller, which holds `own`, holds directory renames off: a
    /// lookup's few calls, made after it raced renames many times over. A
    /// rename that moved a directory counts ([`RenameGate::epoch`]).
    pub fn rename<T>(&self, own: u32, dir: bool, f: impl FnOnce() -> Result<T, Errno>) -> Result<T, Errno> {
        if !dir {
            return f();
        }
        let holds = loop {
            let mut holds = self.lock();
            if !Self::held_by_others(&mut holds, own) {
                break holds;
            }
            drop(holds);
            std::thread::yield_now();
            core::hint::spin_loop();
        };
        let r = f();
        if r.is_ok() {
            self.bump();
        }
        drop(holds);
        r
    }

    /// Whether a thread other than this one holds, a holder that has ended
    /// forgotten.
    fn held_by_others(holds: &mut std::collections::BTreeMap<i32, u32>, own: u32) -> bool {
        if holds.is_empty() {
            return false;
        }
        if let Some(alive) = OWNER_ALIVE.get() {
            holds.retain(|pid, _| alive(*pid));
        }
        let all: u32 = holds.values().sum();
        all > own
    }
}

/// Where an open directory is ([`FileHandle::moved`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// The path it was opened at and the path it has now, both relative to
    /// its filesystem.
    pub was: String,
    pub now: String,
    /// How many times it has moved, renamed itself or with a directory
    /// above it: a lookup through the path that missed while this changed
    /// raced a rename, and is made again (`PathIo::call`).
    pub epoch: u64,
}

impl Moved {
    /// The absolute path the directory has now, given the absolute path it
    /// was opened at: the mount's part of that path does not change. `None`
    /// when the directory was opened through a symlink among the
    /// directories of its path (the path does not end in `was`).
    pub fn path_from(&self, opened: &str) -> Option<String> {
        if self.was == self.now {
            return Some(opened.to_string());
        }
        let mount = if self.was.is_empty() {
            opened
        } else {
            opened.strip_suffix(self.was.as_str())?.strip_suffix('/')?
        };
        Some(match (mount, self.now.as_str()) {
            ("", "") => "/".to_string(),
            (m, "") => m.to_string(),
            (m, now) => format!("{m}/{now}"),
        })
    }
}

/// A mounted filesystem.
pub trait FileSystem: Send + Sync {
    /// Open a path relative to this mount's root (no leading slash).
    fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno>;
    fn metadata(&self, path: &str) -> Result<Metadata, Errno>;
    /// [`FileSystem::metadata`] of a symlink itself rather than what it
    /// names (`lstat`). The default asks [`FileSystem::read_link`] first; a
    /// filesystem whose `metadata` never follows a link answers in one.
    fn metadata_nofollow(&self, path: &str) -> Result<Metadata, Errno> {
        if let Ok(target) = self.read_link(path) {
            return Ok(Metadata {
                file_type: FileType::Symlink,
                size: target.len() as u64,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            });
        }
        self.metadata(path)
    }
    /// `access(2)`: whether the caller may use `path` as `mode` asks
    /// (`R_OK` 4, `W_OK` 2, `X_OK` 1; 0 is existence). Filesystems that do
    /// not model permissions say yes to anything that exists.
    fn access(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let _ = mode;
        self.metadata(path).map(|_| ())
    }
    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let _ = path;
        Err(Errno::EINVAL)
    }
    /// Absolute path `abs` with the symlinks among its directories followed
    /// (and its last component's, with `follow_final`), as far as this
    /// filesystem mounted at `/` can say: see [`walk_links`]. A filesystem
    /// whose `read_link` is a round trip (the browser's [`remote::RemoteFs`])
    /// does the whole walk in one.
    fn walk(&self, abs: &str, follow_final: bool, mounts: &str) -> Result<Walked, Errno> {
        walk_links(abs.to_string(), follow_final, mounts, &|rel| self.read_link(rel))
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
    /// `rename`, refusing (`EEXIST`) a destination that exists when
    /// `noreplace` (`RENAME_NOREPLACE`). A filesystem that can make the
    /// check and the move one step does; this default checks first.
    fn rename2(&self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        if noreplace && self.metadata_nofollow(to).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.rename(from, to)
    }
    /// Change a file's mode. Filesystems that do not model permissions
    /// accept it and do nothing, which is what the guest expects to see.
    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let _ = (path, mode);
        Ok(())
    }
    /// Set a file's modification time (seconds since the epoch), for
    /// `utimensat`. Filesystems that keep no times accept it and do nothing.
    fn set_mtime(&self, path: &str, secs: i64) -> Result<(), Errno> {
        let _ = (path, secs);
        Ok(())
    }
    /// `utimensat` with both times and nanoseconds (`None`: leave it). A
    /// filesystem that keeps only whole-second modification times takes
    /// what it can.
    fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        let _ = atime;
        match mtime {
            Some((secs, _)) => self.set_mtime(path, secs),
            None => Ok(()),
        }
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

/// Whether open `flags` ask to write, which a directory refuses (`EISDIR`).
pub(crate) fn writes(flags: u64) -> bool {
    let acc = flags & crate::fs::oflags::ACCMODE;
    acc == crate::fs::oflags::O_WRONLY || acc == crate::fs::oflags::O_RDWR
}

/// Whether absolute `abs` is `at` or below it.
fn within(abs: &str, at: &str) -> bool {
    abs.starts_with(at) && matches!(abs.as_bytes().get(at.len()), None | Some(b'/'))
}

/// A process's mount table and working directory, and its published
/// [`Namespace`]: what a caller without the kernel lock reads
/// ([`Vfs::view`], docs/kernel-locking.md §3.4). Every lookup is the
/// namespace's (`Deref`); the three changes -- a mount, an unmount, a
/// `chdir` -- are this type's, and publish.
pub struct Vfs {
    ns: Namespace,
    view: NsCell,
    /// The process's working directory while a thread with an fs_struct
    /// of its own has its directory in `ns` for its call ([`Vfs::enter_cwd`]):
    /// what a publication shows.
    held: Option<String>,
}

/// A [`Vfs`]'s namespace as other threads read it: replaced whole on every
/// change, so a reader holds one consistent table and directory for as long
/// as it likes (an `Arc`), and a lookup through it runs outside every lock.
pub struct VfsView {
    cur: gex_atomic::rcu::Published<Arc<Namespace>>,
}

impl VfsView {
    fn new(ns: &Namespace) -> VfsView {
        VfsView { cur: gex_atomic::rcu::Published::new(Arc::new(ns.clone()), crate::file::FD_READER_SLOTS) }
    }

    /// The namespace as it stands.
    pub fn get(&self) -> Arc<Namespace> {
        self.cur.read(crate::sync::me() as usize, Arc::clone)
    }
}

/// A view a copy of the table does not share: a forked child's mounts and
/// directory are its own.
struct NsCell(Arc<VfsView>);

impl Clone for Vfs {
    fn clone(&self) -> Vfs {
        // A fork's copy: the calling thread's directory is the child's.
        Vfs { ns: self.ns.clone(), view: NsCell(Arc::new(VfsView::new(&self.ns))), held: None }
    }
}

impl std::ops::Deref for Vfs {
    type Target = Namespace;
    fn deref(&self) -> &Namespace {
        &self.ns
    }
}

impl Vfs {
    pub fn new() -> Vfs {
        let ns = Namespace::new();
        let view = NsCell(Arc::new(VfsView::new(&ns)));
        Vfs { ns, view, held: None }
    }

    /// The published namespace, the same view for this table's whole life.
    pub fn view(&self) -> &Arc<VfsView> {
        &self.view.0
    }

    /// Every build, the single-threaded page's too, which reads its
    /// namespace the same way: a mount or a `chdir` is rare.
    fn publish(&self) {
        let mut ns = self.ns.clone();
        if let Some(cwd) = &self.held {
            ns.cwd.clone_from(cwd);
        }
        let ns = Arc::new(ns);
        self.view.0.cur.write(|_| (Some(ns), ()));
    }

    pub fn mount(&mut self, at: &str, fs: Arc<dyn FileSystem>) {
        self.ns.mount(at, fs);
        self.publish();
    }

    /// Remove the mount at exactly this path ([`Namespace::unmount`]).
    pub fn unmount(&mut self, path: &str) -> bool {
        let gone = self.ns.unmount(path);
        if gone {
            self.publish();
        }
        gone
    }

    pub fn set_cwd(&mut self, path: &str) -> Result<(), Errno> {
        self.ns.set_cwd(path)?;
        self.publish();
        Ok(())
    }
    /// Whether a thread's own directory is in for its call
    /// ([`Vfs::enter_cwd`]).
    pub fn in_thread_cwd(&self) -> bool {
        self.held.is_some()
    }
    /// Make `cwd` the process's directory as it is, checked when it was
    /// set: an `execve` from a thread with an fs_struct of its own keeps
    /// that thread's directory.
    pub fn put_cwd(&mut self, cwd: String) {
        self.ns.cwd = cwd;
        self.publish();
    }
    /// Resolve against `cwd`, a thread's own fs_struct's directory, until
    /// [`Vfs::leave_cwd`]: the process's stays what is published, a `chdir`
    /// in between changing only the thread's.
    pub fn enter_cwd(&mut self, cwd: String) {
        self.held = Some(std::mem::replace(&mut self.ns.cwd, cwd));
    }

    /// Back to the process's directory; the thread's, as the call left it.
    pub fn leave_cwd(&mut self) -> String {
        match self.held.take() {
            Some(p) => std::mem::replace(&mut self.ns.cwd, p),
            None => self.ns.cwd.clone(),
        }
    }
}

/// The mount table and working directory: every path lookup.
#[derive(Clone)]
pub struct Namespace {
    /// Mount point (absolute, no trailing slash except for "/") to backend.
    mounts: BTreeMap<String, Arc<dyn FileSystem>>,
    /// The mount points other than `/`, NUL-separated: what a walk of the
    /// root filesystem stops at. Kept with `mounts`, not built per lookup.
    others: String,
    /// Current working directory, always absolute and normalised.
    pub cwd: String,
}

impl Namespace {
    fn new() -> Namespace {
        Namespace {
            mounts: BTreeMap::new(),
            others: String::new(),
            cwd: "/".to_string(),
        }
    }

    fn mount(&mut self, at: &str, fs: Arc<dyn FileSystem>) {
        let at = normalize(at, "/");
        self.mounts.insert(at, fs);
        self.list_others();
    }

    fn list_others(&mut self) {
        let others: Vec<&str> = self.mounts.keys().map(String::as_str).filter(|m| *m != "/").collect();
        self.others = others.join("\0");
    }

    /// Whether something is mounted at exactly this path.
    pub fn is_mount_point(&self, path: &str) -> bool {
        self.mounts.contains_key(&normalize(path, &self.cwd))
    }

    /// Remove the mount at exactly this path. The root mount stays: a guest
    /// that unmounts `/` has nothing left to run from.
    fn unmount(&mut self, path: &str) -> bool {
        let at = normalize(path, &self.cwd);
        let gone = at != "/" && self.mounts.remove(&at).is_some();
        if gone {
            self.list_others();
        }
        gone
    }

    /// Find the backend responsible for `path`, and the path relative to it.
    ///
    /// Longest prefix wins, so a `/proc` mount shadows the root mount for
    /// everything beneath it.
    fn resolve<'a>(&'a self, path: &str) -> Result<(&'a Arc<dyn FileSystem>, String), Errno> {
        let abs = normalize(path, &self.cwd);
        let abs = match self.walk(&abs, false)? {
            Some(w) if !w.at_mount => w.path,
            _ => self.through_dir_links(abs)?,
        };
        self.mount_of(abs)
    }

    /// Absolute `abs` with every root-filesystem symlink on it followed,
    /// the last component's too: the name Linux's `/proc/self/fd/N` shows
    /// for a file opened by `abs`. `abs` itself when the walk fails or
    /// reaches another mount (which names its own links).
    pub fn canonical(&self, abs: &str) -> String {
        match self.walk(abs, true) {
            Ok(Some(w)) if !w.at_mount => w.path,
            _ => abs.to_string(),
        }
    }

    /// The root filesystem's [`FileSystem::walk`] of absolute `abs`, or
    /// `None` with nothing mounted at `/`.
    fn walk(&self, abs: &str, follow_final: bool) -> Result<Option<Walked>, Errno> {
        let Some(root) = self.mounts.get("/") else {
            return Ok(None);
        };
        root.walk(abs, follow_final, &self.others).map(Some)
    }

    /// `abs` fully resolved in the root filesystem, final link and all, and
    /// that filesystem with the path relative to it; `None` when the walk
    /// reached another mount, which the caller resolves the long way.
    fn resolve_final(&self, abs: &str) -> Result<Option<(&Arc<dyn FileSystem>, String)>, Errno> {
        self.resolve_walked(abs, true)
    }

    /// [`Vfs::resolve_final`], following the final link only with
    /// `follow_final`.
    fn resolve_walked(&self, abs: &str, follow_final: bool) -> Result<Option<(&Arc<dyn FileSystem>, String)>, Errno> {
        match self.walk(abs, follow_final)? {
            Some(w) if !w.at_mount => self.mount_of(w.path).map(Some),
            _ => Ok(None),
        }
    }

    /// `abs` with every symlink among its directories followed (not the
    /// final component: each caller decides about that). A filesystem here
    /// is a map of whole paths, so `/bin/sh` under a merged /usr (`/bin ->
    /// usr/bin`) names nothing until the `/bin` link is followed. Only the
    /// root filesystem's links: /proc and /dev resolve their own names.
    fn through_dir_links(&self, mut abs: String) -> Result<String, Errno> {
        'again: for _ in 0..40 {
            let comps: Vec<&str> = abs.split('/').filter(|c| !c.is_empty()).collect();
            let mut prefix = String::new();
            for (i, c) in comps.iter().enumerate().take(comps.len().saturating_sub(1)) {
                prefix.push('/');
                prefix.push_str(c);
                let (at, fs) = self.mount_at(&prefix)?;
                if at != "/" {
                    break;
                }
                if let Ok(target) = fs.read_link(prefix.trim_start_matches('/')) {
                    let base = if target.starts_with('/') {
                        normalize(&target, "/")
                    } else {
                        normalize(&target, &parent_of(&prefix))
                    };
                    abs = normalize(&comps[i + 1..].join("/"), &base);
                    continue 'again;
                }
            }
            return Ok(abs);
        }
        Err(Errno::ELOOP)
    }

    /// The mount holding absolute path `abs`, and where it is mounted.
    fn mount_at<'a>(&'a self, abs: &str) -> Result<(&'a str, &'a Arc<dyn FileSystem>), Errno> {
        let mut best: Option<(&String, &Arc<dyn FileSystem>)> = None;
        for (at, fs) in &self.mounts {
            let matches = if at == "/" {
                true
            } else {
                within(abs, at)
            };
            if matches && best.map(|(b, _)| at.len() > b.len()).unwrap_or(true) {
                best = Some((at, fs));
            }
        }
        best.map(|(at, fs)| (at.as_str(), fs)).ok_or(Errno::ENOENT)
    }

    fn mount_of<'a>(&'a self, abs: String) -> Result<(&'a Arc<dyn FileSystem>, String), Errno> {
        let mut best: Option<(&String, &Arc<dyn FileSystem>)> = None;
        for (at, fs) in &self.mounts {
            let matches = if at == "/" {
                true
            } else {
                within(&abs, at)
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
        if let Some((fs, rel)) = self.resolve_final(&current)? {
            return fs.open(&rel, flags, mode);
        }
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
        if let Some((fs, rel)) = self.resolve_final(&current)? {
            return fs.metadata(&rel);
        }
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

    /// `access(2)` on `path`, following symlinks as [`Vfs::metadata`] does.
    pub fn access(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let mut current = normalize(path, &self.cwd);
        if let Some((fs, rel)) = self.resolve_final(&current)? {
            return fs.access(&rel, mode);
        }
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
                Err(_) => return fs.access(&rel, mode),
            }
        }
        Err(Errno::ELOOP)
    }

    /// Metadata without following a final symlink, for `lstat`.
    pub fn metadata_nofollow(&self, path: &str) -> Result<Metadata, Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.metadata_nofollow(&rel)
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
        self.rename2(from, to, false)
    }

    /// `rename`, or with `noreplace` `RENAME_NOREPLACE`: `EEXIST` for a
    /// destination that exists, decided with the move by the filesystem.
    pub fn rename2(&self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        let (fs, rel_from) = self.resolve(from)?;
        let (fs_to, rel_to) = self.resolve(to)?;
        // Crossing a mount boundary needs a copy, which Linux also refuses.
        if !Arc::ptr_eq(fs, fs_to) {
            return Err(Errno::EXDEV);
        }
        fs.rename2(&rel_from, &rel_to, noreplace)
    }

    pub fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let (fs, rel) = self.resolve(path)?;
        fs.chmod(&rel, mode)
    }

    /// Set the modification time of `path`, following a final symlink when
    /// `follow` (as `utimensat` does without `AT_SYMLINK_NOFOLLOW`).
    pub fn set_mtime(&self, path: &str, secs: i64, follow: bool) -> Result<(), Errno> {
        self.set_times(path, None, Some((secs, 0)), follow)
    }

    /// Set the access and modification times of `path` (`None`: leave it),
    /// following a final symlink when `follow`.
    pub fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, follow: bool) -> Result<(), Errno> {
        let mut current = normalize(path, &self.cwd);
        // On the root filesystem one walk names the node (the link itself
        // without `follow`), and setting its times says `ENOENT` for a
        // name that is not there.
        if let Some((fs, rel)) = self.resolve_walked(&current, follow)? {
            return fs.set_times(&rel, atime, mtime);
        }
        for _ in 0..40 {
            let (fs, rel) = self.resolve(&current)?;
            match fs.read_link(&rel) {
                Ok(target) if follow => {
                    current = if target.starts_with('/') {
                        normalize(&target, "/")
                    } else {
                        normalize(&target, &parent_of(&current))
                    };
                }
                // The link itself (AT_SYMLINK_NOFOLLOW), whether or not
                // what it names exists yet: dpkg stamps each link it unpacks
                // as `NAME.dpkg-new`, before its target has that name.
                Ok(_) => return fs.set_times(&rel, atime, mtime),
                Err(_) => {
                    fs.metadata(&rel)?;
                    return fs.set_times(&rel, atime, mtime);
                }
            }
        }
        Err(Errno::ELOOP)
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

    fn set_cwd(&mut self, path: &str) -> Result<(), Errno> {
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

/// Where [`walk_links`] got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    /// The absolute path reached.
    pub path: String,
    /// It stopped at another mount (`mounts`), which resolves its own names:
    /// the caller carries on from `path` there.
    pub at_mount: bool,
}

/// Follow the root filesystem's symlinks through absolute path `abs`: every
/// directory's, and the last component's too with `follow_final`. Stops,
/// with `at_mount`, at the first prefix inside one of `mounts` (absolute,
/// not `/`), whose filesystem names its own links. `read_link` takes a path
/// relative to the root. At most 40 links, as Linux.
///
/// The one walk both the per-process [`Vfs`] and the browser's global kernel
/// run, so a whole lookup there is one call (it was one round trip per
/// directory per lookup: an `openat` of a file five directories down made a
/// dozen).
pub fn walk_links(
    mut abs: String,
    follow_final: bool,
    mounts: &str,
    read_link: &dyn Fn(&str) -> Result<String, Errno>,
) -> Result<Walked, Errno> {
    let under = |p: &str| mounts.split('\0').any(|m| !m.is_empty() && within(p, m));
    if !is_normal(&abs) {
        abs = normalize(&abs, "/");
    }
    'again: for _ in 0..40 {
        // Each directory's prefix is a slice of `abs` (normalised: one slash
        // before every component), so a walk that meets no link builds
        // nothing.
        let last = abs.rfind('/').unwrap_or(0);
        let mut end = 0;
        while end < last {
            end = abs[end + 1..].find('/').map_or(abs.len(), |i| end + 1 + i);
            if end > last {
                break;
            }
            let prefix = &abs[..end];
            if under(prefix) {
                return Ok(Walked { path: abs, at_mount: true });
            }
            if let Ok(target) = read_link(&prefix[1..]) {
                let base = if target.starts_with('/') {
                    normalize(&target, "/")
                } else {
                    normalize(&target, &parent_of(prefix))
                };
                abs = normalize(&abs[end + 1..], &base);
                continue 'again;
            }
        }
        if !follow_final {
            return Ok(Walked { path: abs, at_mount: false });
        }
        if under(&abs) {
            return Ok(Walked { path: abs, at_mount: true });
        }
        match read_link(abs.trim_start_matches('/')) {
            Ok(target) => {
                abs = if target.starts_with('/') {
                    normalize(&target, "/")
                } else {
                    normalize(&target, &parent_of(&abs))
                };
            }
            Err(_) => return Ok(Walked { path: abs, at_mount: false }),
        }
    }
    Err(Errno::ELOOP)
}

/// Make a path absolute and remove `.`, `..` and redundant separators.
///
/// Purely lexical, like `realpath -m`: symlinks are resolved separately, by
/// the caller, so that a loop can be bounded.
pub fn normalize(path: &str, cwd: &str) -> String {
    if is_normal(path) {
        return path.to_string();
    }
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

/// Whether `path` is already what [`normalize`] makes of it: absolute, and
/// no empty, `.` or `..` component (so no doubled or trailing slash).
pub fn is_normal(path: &str) -> bool {
    let b = path.as_bytes();
    if b.first() != Some(&b'/') {
        return false;
    }
    if b.len() == 1 {
        return true;
    }
    b[1..].split(|&c| c == b'/').all(|c| !c.is_empty() && c != b"." && c != b"..")
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
        rec[0..8].copy_from_slice(&e.ino.to_ge_bytes());
        rec[8..16].copy_from_slice(&((i + 1) as i64).to_ge_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_ge_bytes());
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
        // Already normal: returned as is, whatever the cwd.
        for p in ["/", "/a", "/a/b.c/..d"] {
            assert!(is_normal(p), "{p}");
            assert_eq!(normalize(p, "/x"), p);
        }
        for p in ["", "a", "//", "/a/", "/a//b", "/a/./b", "/a/..", "/."] {
            assert!(!is_normal(p), "{p}");
        }
    }

    #[test]
    fn tmpfs_walks_under_one_lock_as_walk_links_does() {
        let fs = TmpFs::new();
        fs.add_dir("usr");
        fs.add_dir("usr/bin");
        fs.add_file("usr/bin/dash", vec![]);
        fs.symlink("usr/bin", "bin").unwrap();
        fs.symlink("dash", "usr/bin/sh").unwrap();
        let w = |p: &str, f: bool| fs.walk(p, f, "/proc").unwrap();
        assert_eq!(w("/bin/sh", false), Walked { path: "/usr/bin/sh".into(), at_mount: false });
        assert_eq!(w("/bin/sh", true), Walked { path: "/usr/bin/dash".into(), at_mount: false });
        assert_eq!(w("/proc/1/exe", true), Walked { path: "/proc/1/exe".into(), at_mount: true });
        assert_eq!(w("//bin/./sh", true), Walked { path: "/usr/bin/dash".into(), at_mount: false });
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
    fn directory_symlinks_are_followed() {
        // A merged /usr: /bin -> usr/bin, /lib -> /usr/lib (absolute).
        let mut vfs = Vfs::new();
        let root = Arc::new(TmpFs::new());
        root.add_file("usr/bin/dash", b"sh".to_vec());
        root.add_file("usr/lib/ld.so", b"ld".to_vec());
        vfs.mount("/", root);
        vfs.symlink("usr/bin", "/bin").unwrap();
        vfs.symlink("/usr/lib", "/lib").unwrap();
        vfs.symlink("dash", "/usr/bin/sh").unwrap();
        assert_eq!(vfs.resolve("/bin/dash").unwrap().1, "usr/bin/dash");
        assert_eq!(vfs.resolve("/lib/ld.so").unwrap().1, "usr/lib/ld.so");
        // The final component is left to the caller.
        assert_eq!(vfs.resolve("/bin").unwrap().1, "bin");
        assert_eq!(vfs.read_link("/bin/sh").unwrap(), "dash");
        assert!(vfs.metadata("/bin/sh").is_ok());
        assert!(vfs.metadata("/bin/../lib/ld.so").is_ok());
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
        let reclen = u16::from_ge_bytes(buf[16..18].try_into().unwrap()) as usize;
        assert_eq!(reclen % 8, 0);
        assert_eq!(buf[18], FileType::Directory.dirent_type());
        assert_eq!(&buf[19..20], b".");
        // A buffer that fits nothing consumes nothing.
        let (buf, n) = encode_dirents(&entries, 0, 8);
        assert!(buf.is_empty());
        assert_eq!(n, 0);
    }

    #[test]
    fn one_lookup_answers_eisdir_enoent_and_a_links_own_times() {
        use crate::fs::oflags::{O_CREAT, O_RDONLY, O_RDWR, O_WRONLY};
        let fs = Arc::new(tmpfs::TmpFs::new());
        fs.add_dir("d");
        fs.add_file("d/f", b"x".to_vec());
        fs.symlink("gone", "d/dangling").unwrap();
        let mut vfs = Vfs::new();
        vfs.mount("/", fs.clone());
        // A directory opens for reading only, with or without O_CREAT.
        assert!(vfs.open("/d", O_RDONLY, 0).is_ok());
        assert_eq!(vfs.open("/d", O_WRONLY, 0).unwrap_err(), Errno::EISDIR);
        assert_eq!(vfs.open("/d", O_RDWR | O_CREAT, 0o644).unwrap_err(), Errno::EISDIR);
        // Times on a missing name: ENOENT, following or not.
        let t = Some((5, 0));
        assert_eq!(vfs.set_times("/d/nope", t, t, true), Err(Errno::ENOENT));
        assert_eq!(vfs.set_times("/d/nope", t, t, false), Err(Errno::ENOENT));
        // A dangling link's own times are set; through it there is nothing.
        assert_eq!(vfs.set_times("/d/dangling", t, t, false), Ok(()));
        assert_eq!(vfs.set_times("/d/dangling", t, t, true), Err(Errno::ENOENT));
        assert_eq!(vfs.set_times("/d/f", t, Some((7, 0)), true), Ok(()));
        assert_eq!(vfs.metadata("/d/f").unwrap().mtime, 7);
        // fchmod by handle: the name it was opened under.
        let h = vfs.open("/d/f", O_RDWR, 0).unwrap();
        assert_eq!(h.chmod(0o600), Some(Ok(())));
        assert_eq!(vfs.metadata("/d/f").unwrap().mode & 0o7777, 0o600);
    }

    #[test]
    fn walk_links_follows_dirs_final_and_stops_at_mounts() {
        let links = |rel: &str| match rel {
            "bin" => Ok("usr/bin".to_string()),
            "usr/bin/sh" => Ok("dash".to_string()),
            "loop" => Ok("/loop".to_string()),
            "var/run" => Ok("/run".to_string()),
            _ => Err(Errno::EINVAL),
        };
        let w = |p: &str, f: bool| walk_links(p.to_string(), f, "/proc\0/run", &links);
        assert_eq!(w("/bin/sh", false).unwrap(), Walked { path: "/usr/bin/sh".into(), at_mount: false });
        assert_eq!(w("/bin/sh", true).unwrap(), Walked { path: "/usr/bin/dash".into(), at_mount: false });
        assert_eq!(w("/proc/self/exe", true).unwrap(), Walked { path: "/proc/self/exe".into(), at_mount: true });
        assert_eq!(w("/var/run/x/y", false).unwrap(), Walked { path: "/run/x/y".into(), at_mount: true });
        assert_eq!(w("/run", true).unwrap(), Walked { path: "/run".into(), at_mount: true });
        assert_eq!(w("/loop", true), Err(Errno::ELOOP));
        assert_eq!(w("/", true).unwrap(), Walked { path: "/".into(), at_mount: false });
    }
}
