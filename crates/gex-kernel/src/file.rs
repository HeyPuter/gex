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

    /// Whether a `read` on this descriptor would have to wait.
    ///
    /// A descriptor opened `O_NONBLOCK` never waits -- it is asking for the
    /// `EAGAIN` -- so it answers no however empty the stream is.
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
}

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
        }
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
        let min = min.max(0) as usize;
        while self.entries.len() <= min {
            self.entries.push(None);
            self.cloexec.push(false);
        }
        let slot = self.entries[min..]
            .iter()
            .position(|e| e.is_none())
            .map(|p| p + min);
        let idx = match slot {
            Some(i) => i,
            None => {
                if self.entries.len() >= 4096 {
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
        if fd < 0 {
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

    pub fn set_cloexec(&mut self, fd: i32, on: bool) -> Result<(), Errno> {
        if fd < 0 || fd as usize >= self.cloexec.len() || self.entries[fd as usize].is_none() {
            return Err(Errno::EBADF);
        }
        self.cloexec[fd as usize] = on;
        Ok(())
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        // A closed descriptor still has a slot in the flag vector, so the
        // entry has to be checked too -- otherwise `F_GETFD` answers for a
        // descriptor that is not there, which is how a program decides an
        // `execve` failed to close what it asked to be closed.
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
        for (i, flag) in self.cloexec.iter().enumerate() {
            if *flag {
                self.entries[i] = None;
            }
        }
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
            ino: 1,
            mtime: 0,
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
