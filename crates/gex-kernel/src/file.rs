//! Open files and the descriptor table.
//!
//! A descriptor points at a shared *open-file description* holding the seek
//! offset and status flags, which is what makes `dup` share a position with
//! its original while a second `open` of the same path does not. The
//! close-on-exec flag is the one property that belongs to the descriptor
//! rather than the description.

use crate::errno::{Errno, SysResult};
use crate::pipe::Pipe;
use crate::vfs::{DirEntry, FileHandle, FileType, Metadata};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A shared open-file description.
#[derive(Debug)]
pub struct OpenFile {
    pub handle: Arc<dyn FileHandle>,
    offset: AtomicU64,
    flags: AtomicU64,
    /// Absolute path, for `/proc/self/fd` and diagnostics.
    pub path: String,
    /// Held across a positioned `read`, `write`, `readv`, `writev` and
    /// `lseek` of a seekable file: the offset's load, the transfer and the
    /// offset's update are one step, as Linux's `f_pos_lock` makes them (two
    /// threads reading one descriptor never get the same bytes). These calls
    /// run without the kernel lock (docs/kernel-locking.md §3.4). A stream
    /// has no position and locks itself.
    pos: Mutex<()>,
    /// Cursor into a directory listing, for `getdents64`.
    dir_pos: AtomicUsize,
    /// Cached listing, so a sequence of `getdents64` calls sees a stable
    /// snapshot even if the directory changes underneath.
    dir_cache: Mutex<Option<Vec<DirEntry>>>,
    pub readable: bool,
    pub writable: bool,
    /// A path to unlink when the last descriptor to this file closes. Set for
    /// `O_TMPFILE`: Linux makes such a file nameless at once, but gex keeps it
    /// named (`.gex-tmpfile-*`) so the `linkat("/proc/self/fd/N", target,
    /// AT_SYMLINK_FOLLOW)` idiom -- how QSaveFile, and thus every Qt/KDE app,
    /// materialises an atomic write -- can find it. Cleaned up when the
    /// description goes (`Drop`) so it does not linger visibly in the
    /// directory.
    pub unlink_on_close: Option<TmpName>,
    /// The `flock` table this description has taken a lock in, if any: its
    /// lock goes when the description does (`Drop`), as Linux's last `fput`
    /// releases it. Not when a descriptor's `close` counts what is left:
    /// without a kernel lock two threads closing the last two copies at
    /// once each still hold one, and a descriptor table's replaced copies
    /// hold one until their readers move on.
    locks: OnceLock<Arc<crate::unix::FileLocks>>,
}

/// An `O_TMPFILE`'s name, and the namespace it was made in.
pub struct TmpName {
    pub path: String,
    ns: Arc<crate::vfs::Namespace>,
}

impl std::fmt::Debug for TmpName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TmpName").field(&self.path).finish()
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        if let Some(locks) = self.locks.get() {
            locks.release(self as *const OpenFile as usize);
        }
        // If a `linkat` gave it another name (the atomic-write idiom), that
        // name keeps the inode.
        if let Some(t) = &self.unlink_on_close {
            let _ = t.ns.unlink(&t.path);
        }
    }
}

impl OpenFile {
    pub fn new(
        handle: Arc<dyn FileHandle>,
        path: String,
        flags: u64,
        readable: bool,
        writable: bool,
    ) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            handle,
            offset: AtomicU64::new(0),
            flags: AtomicU64::new(flags),
            path,
            pos: Mutex::new(()),
            dir_pos: AtomicUsize::new(0),
            dir_cache: Mutex::new(None),
            readable,
            writable,
            unlink_on_close: None,
            locks: OnceLock::new(),
        })
    }

    /// An `O_TMPFILE` descriptor: named at `path` while open, unlinked when the
    /// last reference closes. See `unlink_on_close`.
    pub fn tmpfile(
        handle: Arc<dyn FileHandle>,
        path: String,
        ns: Arc<crate::vfs::Namespace>,
        flags: u64,
        readable: bool,
        writable: bool,
    ) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            handle,
            offset: AtomicU64::new(0),
            flags: AtomicU64::new(flags),
            unlink_on_close: Some(TmpName { path: path.clone(), ns }),
            locks: OnceLock::new(),
            path,
            pos: Mutex::new(()),
            dir_pos: AtomicUsize::new(0),
            dir_cache: Mutex::new(None),
            readable,
            writable,
        })
    }

    /// Its `flock` key, the description's address: a stable name while it
    /// lives, and released before it is reused (`Drop`).
    pub fn lock_key(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    /// Note the `flock` table it has a lock in, for its `Drop` to release.
    pub fn note_locks(&self, locks: &Arc<crate::unix::FileLocks>) {
        let _ = self.locks.get_or_init(|| Arc::clone(locks));
    }

    /// Whether its going undoes something (a lock, a temporary name), which
    /// a `close` waits to see done ([`FdTable::settle`]).
    pub fn undoes_on_last_close(&self) -> bool {
        self.locks.get().is_some() || self.unlink_on_close.is_some()
    }

    pub fn flags(&self) -> u64 {
        self.flags.load(Ordering::Relaxed)
    }

    pub fn set_flags(&self, f: u64) {
        self.flags.store(f, Ordering::Relaxed);
    }

    pub fn offset(&self) -> u64 {
        self.offset.load(Ordering::Relaxed)
    }

    /// Put the cursor where another process's copy had it. A forked child is a
    /// separate address space, so it cannot truly share the open-file
    /// description; starting from the same offset is the nearest to it.
    pub fn set_offset(&self, at: u64) {
        self.offset.store(at, Ordering::Relaxed);
    }

    /// Whether a blocking `write` must be carried to completion by the syscall
    /// layer rather than issued once. A pipe must not wait (it runs with the
    /// kernel locked, and the reader needs that lock), so it takes what fits
    /// and reports a short count; something outside the lock writes the rest.
    pub fn write_is_atomic(&self) -> bool {
        self.writable
            && self.flags.load(Ordering::Relaxed) & crate::fs::oflags::O_NONBLOCK == 0
            && self.handle.is_stream()
    }

    /// Whether a `write` on this descriptor would have to wait.
    pub fn write_would_block(&self) -> bool {
        self.writable
            && self.flags.load(Ordering::Relaxed) & crate::fs::oflags::O_NONBLOCK == 0
            && !self.handle.poll_writable()
    }

    pub fn would_block(&self) -> bool {
        self.readable
            && self.flags.load(Ordering::Relaxed) & crate::fs::oflags::O_NONBLOCK == 0
            && !self.handle.poll_readable()
    }

    /// The position lock (`pos`) for a seekable file, `None` for a stream.
    /// [`read_here`](Self::read_here) and [`write_here`](Self::write_here)
    /// want it held; `read` and `write` take it themselves.
    pub fn pos_lock(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        (!self.handle.is_stream()).then(|| self.pos.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn read(&self, buf: &mut [u8]) -> SysResult {
        let _pos = self.pos_lock();
        self.read_here(buf)
    }

    /// `read` with the position lock already held (a vectored read holds it
    /// across its vectors).
    pub fn read_here(&self, buf: &mut [u8]) -> SysResult {
        if !self.readable {
            return Err(Errno::EBADF);
        }
        let off = self.offset.load(Ordering::Relaxed);
        let n = self.handle.read_at(off, buf)?;
        if !self.handle.is_stream() {
            self.offset.fetch_add(n, Ordering::Relaxed);
        }
        Ok(n)
    }

    pub fn write(&self, buf: &[u8]) -> SysResult {
        let _pos = self.pos_lock();
        self.write_here(buf)
    }

    /// `write` with the position lock already held.
    pub fn write_here(&self, buf: &[u8]) -> SysResult {
        if !self.writable {
            return Err(Errno::EBADF);
        }
        // O_APPEND always writes at the current end, which has to be
        // re-read each time because another writer may have extended it.
        let off = if self.flags() & crate::fs::oflags::O_APPEND != 0 {
            self.handle.metadata().map(|m| m.size).unwrap_or(0)
        } else {
            self.offset.load(Ordering::Relaxed)
        };
        let n = self.handle.write_at(off, buf)?;
        if !self.handle.is_stream() {
            self.offset.store(off + n, Ordering::Relaxed);
        }
        Ok(n)
    }

    pub fn pread(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        if !self.readable {
            return Err(Errno::EBADF);
        }
        self.handle.read_at(offset, buf)
    }

    pub fn pwrite(&self, offset: u64, buf: &[u8]) -> SysResult {
        if !self.writable {
            return Err(Errno::EBADF);
        }
        self.handle.write_at(offset, buf)
    }

    pub fn seek(&self, offset: i64, whence: u32) -> SysResult {
        if let Some(r) = self.handle.seek(offset, whence) {
            return r;
        }
        if self.handle.is_stream() {
            return Err(Errno::ESPIPE);
        }
        let _pos = self.pos_lock();
        let base = match whence {
            0 => 0,
            1 => self.offset.load(Ordering::Relaxed),
            2 => self.handle.metadata()?.size,
            _ => return Err(Errno::EINVAL),
        };
        let new = base as i64 + offset;
        if new < 0 {
            return Err(Errno::EINVAL);
        }
        self.offset.store(new as u64, Ordering::Relaxed);
        Ok(new as u64)
    }

    pub fn metadata(&self) -> Result<Metadata, Errno> {
        self.handle.metadata()
    }

    /// Fill `buf` with `getdents64` records, advancing the cursor.
    pub fn read_dirents(&self, buf_len: usize) -> Result<Vec<u8>, Errno> {
        let mut cache = self.dir_cache.lock().unwrap();
        if cache.is_none() {
            *cache = Some(self.handle.read_dir()?);
        }
        let entries = cache.as_ref().unwrap();
        let start = self.dir_pos.load(Ordering::Relaxed);
        let (bytes, consumed) = crate::vfs::encode_dirents(entries, start, buf_len);
        // A buffer too small for even one record is an error, not an
        // end-of-directory signal; otherwise callers loop forever.
        if consumed == 0 && start < entries.len() {
            return Err(Errno::EINVAL);
        }
        self.dir_pos.store(start + consumed, Ordering::Relaxed);
        Ok(bytes)
    }

    pub fn is_tty(&self) -> bool {
        self.handle.is_tty()
    }
}

/// The descriptors themselves: what one published snapshot of an
/// [`FdTable`] holds.
#[derive(Clone, Default)]
struct Table {
    entries: Vec<Option<Arc<OpenFile>>>,
    cloexec: Vec<bool>,
}

/// How many reader slots a table's epoch array has: one per Worker of the
/// page (`k_ws_self` is 0..=128, a reader's id that plus one), so a few more
/// than that. A reader whose id falls outside takes the writers' mutex.
pub const FD_READER_SLOTS: usize = 130;

/// The per-process descriptor table (docs/kernel-locking.md §3.4).
///
/// No kernel lock: a lookup reads the published snapshot
/// ([`gex_atomic::rcu::Published`], announced in its epoch array, §4.2) and
/// takes its own `Arc` to the description; a change (open, close, dup,
/// F_DUPFD, pipe, socket, accept, exec, exit) takes the table's mutex,
/// copies the snapshot, changes the copy and publishes it. The snapshot it
/// replaced is dropped as soon as no reader can still be looking at it --
/// at once, almost always, since a lookup holds its pin for a load and a
/// reference count -- so a `close` closes the description when it returns,
/// as on Linux: a pipe's reader sees its end of file without waiting for
/// another change to the table.
///
/// A read on a descriptor another thread closes keeps the description
/// alive through its own `Arc` until it is done, as Linux does.
pub struct FdTable {
    cur: gex_atomic::rcu::Published<Table>,
    /// Bumped by every change, so `/proc/self/fd` can skip rebuilding its
    /// listing when nothing changed -- as `MemoryMap::generation` does.
    generation: AtomicU64,
    /// `RLIMIT_NOFILE`'s soft limit: no descriptor at or above it.
    limit: AtomicUsize,
}

/// `RLIMIT_NOFILE`, as Linux starts a process: 1024 soft, and the hard
/// limit is what the table can hold.
pub const NOFILE_SOFT: usize = 1024;
pub const NOFILE_HARD: usize = 4096;

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FdTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FdTable").field("open", &self.list().len()).finish()
    }
}

/// A copy of the table that shares every open-file description: what `fork`
/// needs, so the child sees the same offsets. The copy is a table of its own.
impl Clone for FdTable {
    fn clone(&self) -> FdTable {
        let t = self.read(|t| t.clone());
        FdTable::from_parts(t, self.generation(), self.limit())
    }
}

impl FdTable {
    pub fn new() -> FdTable {
        FdTable::from_parts(Table::default(), 0, NOFILE_SOFT)
    }

    fn from_parts(t: Table, generation: u64, limit: usize) -> FdTable {
        FdTable {
            cur: gex_atomic::rcu::Published::new(t, FD_READER_SLOTS),
            generation: AtomicU64::new(generation),
            limit: AtomicUsize::new(limit),
        }
    }

    /// Look at the current snapshot, from this thread's reader slot (its
    /// [`crate::sync::me`] id; none, and the writers' mutex, without one).
    fn read<R>(&self, f: impl FnOnce(&Table) -> R) -> R {
        self.cur.read(crate::sync::me() as usize, f)
    }

    /// Change the table: copy the snapshot, let `f` change the copy, and
    /// publish it unless `f` failed. Every change bumps the generation, as
    /// before (a failed one included).
    fn update<R, E>(&self, f: impl FnOnce(&mut Table) -> Result<R, E>) -> Result<R, E> {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.cur.write(|old| {
            let mut t = old.clone();
            match f(&mut t) {
                Ok(r) => (Some(t), Ok(r)),
                Err(e) => (None, Err(e)),
            }
        })
    }

    /// `RLIMIT_NOFILE`'s soft limit (descriptors already above it stay).
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit.min(NOFILE_HARD), Ordering::Relaxed);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        self.read(|t| t.entries.get(fd as usize).and_then(|e| e.clone()))
            .ok_or(Errno::EBADF)
    }

    /// Install a description at the lowest free descriptor at or above `min`.
    pub fn insert_at(&self, file: Arc<OpenFile>, min: i32, cloexec: bool) -> SysResult {
        let limit = self.limit();
        // F_DUPFD's argument: negative or past the limit is EINVAL
        if min < 0 || min as usize >= limit {
            self.generation.fetch_add(1, Ordering::Relaxed);
            return Err(Errno::EINVAL);
        }
        let min = min as usize;
        self.update(|t| {
            while t.entries.len() <= min {
                t.entries.push(None);
                t.cloexec.push(false);
            }
            let slot = t.entries[min..]
                .iter()
                .position(|e| e.is_none())
                .map(|p| p + min);
            let idx = match slot {
                Some(i) if i < limit => i,
                Some(_) => return Err(Errno::EMFILE),
                None => {
                    if t.entries.len() >= limit {
                        return Err(Errno::EMFILE);
                    }
                    t.entries.push(None);
                    t.cloexec.push(false);
                    t.entries.len() - 1
                }
            };
            t.entries[idx] = Some(file);
            t.cloexec[idx] = cloexec;
            Ok(idx as u64)
        })
    }

    pub fn insert(&self, file: Arc<OpenFile>, cloexec: bool) -> SysResult {
        self.insert_at(file, 0, cloexec)
    }

    /// Force a description into a specific descriptor, closing what was there.
    pub fn install(&self, fd: i32, file: Arc<OpenFile>, cloexec: bool) -> SysResult {
        // dup2/dup3 to a descriptor past RLIMIT_NOFILE: EBADF, as Linux
        if fd < 0 || fd as usize >= self.limit() {
            self.generation.fetch_add(1, Ordering::Relaxed);
            return Err(Errno::EBADF);
        }
        let fd = fd as usize;
        self.update(|t| {
            while t.entries.len() <= fd {
                t.entries.push(None);
                t.cloexec.push(false);
            }
            t.entries[fd] = Some(file);
            t.cloexec[fd] = cloexec;
            Ok::<_, Errno>(fd as u64)
        })
    }

    /// Drop every descriptor, as the kernel does when a process dies. A pipe
    /// end left open would leave a reader waiting for an EOF that never comes.
    pub fn close_all(&self) {
        let _ = self.update(|t| {
            t.entries.clear();
            t.cloexec.clear();
            Ok::<_, ()>(())
        });
    }

    /// Close (or mark close-on-exec) every open descriptor in `[first, last]`.
    /// Gaps in the range are skipped, not faulted -- the point is "shut
    /// everything from here up" without knowing which are open.
    pub fn close_range(&self, first: u32, last: u32, cloexec_only: bool) {
        let _ = self.update(|t| {
            let hi = (last as usize).min(t.entries.len().saturating_sub(1));
            for fd in (first as usize)..=hi {
                if t.entries.get(fd).is_none_or(|e| e.is_none()) {
                    continue;
                }
                if cloexec_only {
                    if fd < t.cloexec.len() {
                        t.cloexec[fd] = true;
                    }
                } else {
                    t.entries[fd] = None;
                }
            }
            Ok::<_, ()>(())
        });
    }

    /// Wait until no replaced copy of the table outlives this call: what
    /// the copies held that nothing else does is dropped by then. A
    /// `close` whose description undoes something when it goes calls it,
    /// so the lock or name is gone when the call returns, as on Linux.
    pub fn settle(&self) {
        self.cur.settle();
    }

    pub fn close(&self, fd: i32) -> Result<(), Errno> {
        if fd < 0 {
            self.generation.fetch_add(1, Ordering::Relaxed);
            return Err(Errno::EBADF);
        }
        self.update(|t| match t.entries.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(Errno::EBADF),
        })
    }

    pub fn set_cloexec(&self, fd: i32, on: bool) -> Result<(), Errno> {
        self.update(|t| {
            if fd < 0 || fd as usize >= t.cloexec.len() || t.entries[fd as usize].is_none() {
                return Err(Errno::EBADF);
            }
            t.cloexec[fd as usize] = on;
            Ok(())
        })
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        // A closed descriptor still has a slot in the flag vector, so check
        // the entry too -- otherwise `F_GETFD` answers for a descriptor that
        // is not there.
        self.read(|t| {
            if fd < 0
                || fd as usize >= t.cloexec.len()
                || t.entries.get(fd as usize).is_none_or(|e| e.is_none())
            {
                return Err(Errno::EBADF);
            }
            Ok(t.cloexec[fd as usize])
        })
    }

    /// Drop every close-on-exec descriptor, as `execve` does.
    pub fn close_on_exec(&self) {
        let _ = self.update(|t| {
            for (i, flag) in t.cloexec.iter().enumerate() {
                if *flag {
                    t.entries[i] = None;
                }
            }
            Ok::<_, ()>(())
        });
    }

    /// How many times the table has changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Every open descriptor, with its description. `fork` needs this when the
    /// child is a separate address space: a cloned `Arc` names a memory the
    /// child lacks, so it is sent a description of each fd and rebuilds it
    /// (`gex_fork_fds` in the browser shim).
    pub fn entries(&self) -> Vec<(i32, Arc<OpenFile>, bool)> {
        self.read(|t| {
            t.entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.as_ref().map(|f| (i as i32, Arc::clone(f), t.cloexec[i])))
                .collect()
        })
    }

    /// Every open descriptor and the path behind it.
    pub fn list(&self) -> Vec<(i32, String)> {
        self.read(|t| {
            t.entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.as_ref().map(|f| (i as i32, f.path.clone())))
                .collect()
        })
    }

    /// Duplicate the table, sharing every open-file description. This is what
    /// `fork` needs: the child sees the same offsets.
    pub fn clone_table(&self) -> FdTable {
        self.clone()
    }
}

/// A handle over one end of a pipe.
#[derive(Debug)]
pub struct PipeEnd {
    pub pipe: Arc<Pipe>,
    pub write: bool,
}

impl FileHandle for PipeEnd {
    /// An empty pipe is not readable -- the trait default "yes" is right for a
    /// file and wrong here, and a `select` on it would spin ready/read-nothing.
    /// A write end is never readable (select would report it ready for
    /// reading when asked about both directions; Linux does not).
    fn poll_readable(&self) -> bool {
        !self.write && self.pipe.readable()
    }

    fn available(&self) -> Option<u64> {
        Some(if self.write { 0 } else { self.pipe.buffered() as u64 })
    }

    fn poll_writable(&self) -> bool {
        !self.write || self.pipe.writable()
    }

    /// The read end hangs up when the last writer is gone, data or not.
    fn poll_hup(&self) -> bool {
        !self.write && self.pipe.hung_up()
    }

    fn write_room(&self) -> Option<u64> {
        self.write.then(|| self.pipe.room() as u64)
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if self.write {
            return Err(Errno::EBADF);
        }
        self.pipe.read(buf)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if !self.write {
            return Err(Errno::EBADF);
        }
        self.pipe.write(buf)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: crate::vfs::anon_ino(Arc::as_ptr(&self.pipe) as *const u8),
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn wait_queue(&self) -> u32 {
        self.pipe.wait_queue()
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.pipe.close_end(self.write);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe_pair() -> (Arc<OpenFile>, Arc<OpenFile>) {
        let p = Arc::new(Pipe::new());
        let r = OpenFile::new(Arc::new(PipeEnd { pipe: Arc::clone(&p), write: false }), "r".into(), 0, true, false);
        let w = OpenFile::new(Arc::new(PipeEnd { pipe: p, write: true }), "w".into(), 1, false, true);
        (r, w)
    }

    /// A close drops the description when it returns, with readers about:
    /// the pipe's reader sees end of file at once (§3.4).
    #[test]
    fn a_close_closes_the_description_at_once() {
        let t = FdTable::new();
        let (r, w) = pipe_pair();
        let weak = Arc::downgrade(&w);
        t.install(3, r, false).unwrap();
        t.install(4, w, false).unwrap();
        let rd = t.get(3).unwrap();
        t.close(4).unwrap();
        assert!(weak.upgrade().is_none(), "the write end outlived its close");
        assert!(rd.handle.poll_hup());
    }

    /// Readers in their own slots (the page's Workers) look descriptors up
    /// while another thread closes and replaces them: every lookup finds one
    /// description or none, and nothing freed is ever read.
    #[test]
    fn lookups_race_closes_and_replacements() {
        use std::sync::atomic::AtomicBool;
        let t = Arc::new(FdTable::new());
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (1..5)
            .map(|id| {
                let (t, stop) = (t.clone(), stop.clone());
                std::thread::spawn(move || {
                    crate::sync::set_thread_id(id);
                    let (mut found, mut gone) = (0u64, 0u64);
                    while !stop.load(Ordering::Relaxed) {
                        match t.get(7) {
                            Ok(f) => {
                                assert!(f.path == "r" || f.path == "w");
                                found += 1;
                            }
                            Err(e) => {
                                assert_eq!(e, Errno::EBADF);
                                gone += 1;
                            }
                        }
                    }
                    (found, gone)
                })
            })
            .collect();
        for i in 0..20_000 {
            let (r, w) = pipe_pair();
            t.install(7, if i % 2 == 0 { r } else { w }, false).unwrap();
            if i % 3 == 0 {
                t.close(7).unwrap();
            }
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            let (found, _) = r.join().unwrap();
            assert!(found > 0);
        }
    }
}
