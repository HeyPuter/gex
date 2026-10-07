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
use std::sync::{Arc, Mutex};

/// A shared open-file description.
#[derive(Debug)]
pub struct OpenFile {
    pub handle: Arc<dyn FileHandle>,
    offset: AtomicU64,
    flags: AtomicU64,
    /// Absolute path, for `/proc/self/fd` and diagnostics.
    pub path: String,
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
    /// materialises an atomic write -- can find it. Cleaned up here so it does
    /// not linger visibly in the directory.
    pub unlink_on_close: Option<String>,
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
            dir_pos: AtomicUsize::new(0),
            dir_cache: Mutex::new(None),
            readable,
            writable,
            unlink_on_close: None,
        })
    }

    /// An `O_TMPFILE` descriptor: named at `path` while open, unlinked when the
    /// last reference closes. See `unlink_on_close`.
    pub fn tmpfile(
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
            unlink_on_close: Some(path.clone()),
            path,
            dir_pos: AtomicUsize::new(0),
            dir_cache: Mutex::new(None),
            readable,
            writable,
        })
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

    pub fn read(&self, buf: &mut [u8]) -> SysResult {
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

/// The per-process descriptor table.
#[derive(Clone)]
pub struct FdTable {
    entries: Vec<Option<Arc<OpenFile>>>,
    cloexec: Vec<bool>,
    /// Bumped by every change, so `/proc/self/fd` can skip rebuilding its
    /// listing when nothing changed -- as `MemoryMap::generation` does.
    generation: u64,
    /// `RLIMIT_NOFILE`'s soft limit: no descriptor at or above it.
    limit: usize,
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

impl FdTable {
    pub fn new() -> FdTable {
        FdTable {
            entries: Vec::new(),
            cloexec: Vec::new(),
            generation: 0,
            limit: NOFILE_SOFT,
        }
    }

    /// `RLIMIT_NOFILE`'s soft limit (descriptors already above it stay).
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.min(NOFILE_HARD);
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        self.entries
            .get(fd as usize)
            .and_then(|e| e.clone())
            .ok_or(Errno::EBADF)
    }

    /// Install a description at the lowest free descriptor at or above `min`.
    pub fn insert_at(&mut self, file: Arc<OpenFile>, min: i32, cloexec: bool) -> SysResult {
        self.generation += 1;
        // F_DUPFD's argument: negative or past the limit is EINVAL
        if min < 0 || min as usize >= self.limit {
            return Err(Errno::EINVAL);
        }
        let min = min as usize;
        while self.entries.len() <= min {
            self.entries.push(None);
            self.cloexec.push(false);
        }
        let slot = self.entries[min..]
            .iter()
            .position(|e| e.is_none())
            .map(|p| p + min);
        let idx = match slot {
            Some(i) if i < self.limit => i,
            Some(_) => return Err(Errno::EMFILE),
            None => {
                if self.entries.len() >= self.limit {
                    return Err(Errno::EMFILE);
                }
                self.entries.push(None);
                self.cloexec.push(false);
                self.entries.len() - 1
            }
        };
        self.entries[idx] = Some(file);
        self.cloexec[idx] = cloexec;
        Ok(idx as u64)
    }

    pub fn insert(&mut self, file: Arc<OpenFile>, cloexec: bool) -> SysResult {
        self.insert_at(file, 0, cloexec)
    }

    /// Force a description into a specific descriptor, closing what was there.
    pub fn install(&mut self, fd: i32, file: Arc<OpenFile>, cloexec: bool) -> SysResult {
        self.generation += 1;
        // dup2/dup3 to a descriptor past RLIMIT_NOFILE: EBADF, as Linux
        if fd < 0 || fd as usize >= self.limit {
            return Err(Errno::EBADF);
        }
        let fd = fd as usize;
        while self.entries.len() <= fd {
            self.entries.push(None);
            self.cloexec.push(false);
        }
        self.entries[fd] = Some(file);
        self.cloexec[fd] = cloexec;
        Ok(fd as u64)
    }

    /// Drop every descriptor, as the kernel does when a process dies. A pipe
    /// end left open would leave a reader waiting for an EOF that never comes.
    pub fn close_all(&mut self) {
        self.generation += 1;
        self.entries.clear();
        self.cloexec.clear();
    }

    /// Close (or mark close-on-exec) every open descriptor in `[first, last]`.
    /// Gaps in the range are skipped, not faulted -- the point is "shut
    /// everything from here up" without knowing which are open.
    pub fn close_range(&mut self, first: u32, last: u32, cloexec_only: bool) {
        self.generation += 1;
        let hi = (last as usize).min(self.entries.len().saturating_sub(1));
        for fd in (first as usize)..=hi {
            if self.entries.get(fd).is_none_or(|e| e.is_none()) {
                continue;
            }
            if cloexec_only {
                if fd < self.cloexec.len() {
                    self.cloexec[fd] = true;
                }
            } else {
                self.entries[fd] = None;
            }
        }
    }

    pub fn close(&mut self, fd: i32) -> Result<(), Errno> {
        self.generation += 1;
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

    pub fn set_cloexec(&mut self, fd: i32, on: bool) -> Result<(), Errno> {
        self.generation += 1;
        if fd < 0 || fd as usize >= self.cloexec.len() || self.entries[fd as usize].is_none() {
            return Err(Errno::EBADF);
        }
        self.cloexec[fd as usize] = on;
        Ok(())
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        // A closed descriptor still has a slot in the flag vector, so check
        // the entry too -- otherwise `F_GETFD` answers for a descriptor that
        // is not there.
        if fd < 0
            || fd as usize >= self.cloexec.len()
            || self.entries.get(fd as usize).is_none_or(|e| e.is_none())
        {
            return Err(Errno::EBADF);
        }
        Ok(self.cloexec[fd as usize])
    }

    /// Drop every close-on-exec descriptor, as `execve` does.
    pub fn close_on_exec(&mut self) {
        self.generation += 1;
        for (i, flag) in self.cloexec.iter().enumerate() {
            if *flag {
                self.entries[i] = None;
            }
        }
    }

    /// How many times the table has changed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every open descriptor, with its description. `fork` needs this when the
    /// child is a separate address space: a cloned `Arc` names a memory the
    /// child lacks, so it is sent a description of each fd and rebuilds it
    /// (`gex_fork_fds` in the browser shim).
    pub fn entries(&self) -> Vec<(i32, Arc<OpenFile>, bool)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                e.as_ref()
                    .map(|f| (i as i32, Arc::clone(f), self.cloexec[i]))
            })
            .collect()
    }

    /// Every open descriptor and the path behind it.
    pub fn list(&self) -> Vec<(i32, String)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|f| (i as i32, f.path.clone())))
            .collect()
    }

    /// Duplicate the table, sharing every open-file description. This is what
    /// `fork` needs: the child sees the same offsets.
    pub fn clone_table(&self) -> FdTable {
        FdTable {
            entries: self.entries.clone(),
            cloexec: self.cloexec.clone(),
            generation: self.generation,
            limit: self.limit,
        }
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
    fn poll_readable(&self) -> bool {
        self.write || self.pipe.readable()
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
        })
    }

    fn is_stream(&self) -> bool {
        true
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.pipe.close_end(self.write);
    }
}
