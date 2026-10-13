//! An in-memory filesystem.
//!
//! This is the writable layer for everything the guest creates and the one
//! backend that works identically on every host, so it is what `/tmp`,
//! `/dev/shm` and an overlay's upper layer are built from.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use crate::fs::oflags;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// Bytes tmpfs is holding, summed as `Vec` capacity rather than length: a
/// buffer that grew geometrically keeps the larger allocation whatever its
/// length says. For the memory census.
pub static TMPFS_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn note_capacity(before: usize, after: usize) {
    if after >= before {
        TMPFS_BYTES.fetch_add((after - before) as u64, core::sync::atomic::Ordering::Relaxed);
    } else {
        TMPFS_BYTES.fetch_sub((before - after) as u64, core::sync::atomic::Ordering::Relaxed);
    }
}

/// A file's contents, held apart from any name so an open descriptor survives
/// `unlink`: SQLite unlinks its temp files at once and keeps writing through
/// the descriptor it still holds.
#[derive(Debug, Default)]
pub struct FileNode {
    data: Mutex<Vec<u8>>,
    /// Contents fetched on demand (`TmpFs::add_lazy`); `None` once the file
    /// is an ordinary one (never, or since a write made it one).
    lazy: Mutex<Option<Lazy>>,
    ino: u64,
    /// Modification time, in seconds. On the node, not the name, so it follows
    /// a rename or a link and an open descriptor's `fstat` sees it -- `gzip`
    /// records its input's time from `fstat` of the open file.
    mtime: AtomicI64,
    /// Access and change times and the nanoseconds (`Times`), once
    /// `utimensat` set any; until then every time is `mtime`.
    times: Mutex<super::Times>,
}

impl FileNode {
    fn new(data: Vec<u8>, ino: u64) -> FileNode {
        FileNode {
            data: Mutex::new(data),
            lazy: Mutex::new(None),
            ino,
            mtime: AtomicI64::new(now()),
            times: Mutex::new(Default::default()),
        }
    }
    fn touch(&self) {
        self.stamp(now());
    }
    /// Modified at `secs`: the change time with it, once the times are kept.
    fn stamp(&self, secs: i64) {
        self.mtime.store(secs, Ordering::Relaxed);
        let mut t = self.times.lock().unwrap();
        if *t != super::Times::default() {
            t.mtime_ns = 0;
            t.ctime = secs;
            t.ctime_ns = 0;
        }
    }
    /// `utimensat`/`futimens`: either time (`None`: leave it), and the
    /// change time is `now`.
    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) {
        let mut t = self.times.lock().unwrap();
        if *t == super::Times::default() {
            let m = self.mtime.load(Ordering::Relaxed);
            t.atime = m;
            t.ctime = m;
        }
        if let Some((s, ns)) = atime {
            t.atime = s;
            t.atime_ns = ns;
        }
        if let Some((s, ns)) = mtime {
            self.mtime.store(s, Ordering::Relaxed);
            t.mtime_ns = ns;
        }
        t.ctime = now;
        t.ctime_ns = 0;
    }
    fn times(&self) -> super::Times {
        *self.times.lock().unwrap()
    }

    /// The size, lazy or not.
    fn size(&self) -> u64 {
        if let Some(l) = self.lazy.lock().unwrap().as_ref() {
            return l.size;
        }
        self.data.lock().unwrap().len() as u64
    }

    /// Make a lazy file an ordinary one (all of it fetched), before a write
    /// or a truncation changes it.
    fn materialize(&self) -> Result<(), Errno> {
        let mut lazy = self.lazy.lock().unwrap();
        let Some(l) = lazy.as_mut() else { return Ok(()) };
        let mut all = vec![0u8; l.size as usize];
        l.read(0, &mut all)?;
        let mut data = self.data.lock().unwrap();
        let was = data.capacity();
        *data = all;
        note_capacity(was, data.capacity());
        TMPFS_BYTES.fetch_sub(l.held, core::sync::atomic::Ordering::Relaxed);
        *lazy = None;
        Ok(())
    }
}

/// How a lazy file's bytes are fetched: `(url, offset, into)`, true when
/// all of `into` was filled. Installed by the host (the browser's global
/// kernel: a synchronous ranged request from the Worker that reads).
pub type LazyFetch = fn(&str, u64, &mut [u8]) -> bool;
static LAZY_FETCH: std::sync::OnceLock<LazyFetch> = std::sync::OnceLock::new();

/// Set how lazy files are fetched (the first call wins).
pub fn set_lazy_fetch(f: LazyFetch) {
    let _ = LAZY_FETCH.set(f);
}

/// Lazy files are fetched and kept in pieces this big.
const LAZY_CHUNK: u64 = 1 << 20;
/// How many chunks past the one read a sequential read fetches with it: a
/// read at the start of a file, or right after a chunk already held. One
/// synchronous request per MiB was 0.8 s of apt installing a 9 MB .deb of
/// ours from the page; a random read still fetches only what it reads.
const LAZY_READAHEAD: u64 = 15;

/// A file whose bytes live elsewhere (a URL) and arrive a chunk at a time,
/// as they are read: a game's gigabytes of data served to a page.
#[derive(Debug)]
struct Lazy {
    url: String,
    size: u64,
    chunks: std::collections::HashMap<u64, Box<[u8]>>,
    held: u64,
}

impl Lazy {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<u64, Errno> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let end = (offset + buf.len() as u64).min(self.size);
        let (first, last) = (offset / LAZY_CHUNK, (end - 1) / LAZY_CHUNK);
        // Missing chunks in runs, one request a run.
        let mut c = first;
        while c <= last {
            if self.chunks.contains_key(&c) {
                c += 1;
                continue;
            }
            let sequential = c == 0 || self.chunks.contains_key(&(c - 1));
            let limit = if sequential { last + LAZY_READAHEAD } else { last };
            let limit = limit.min((self.size - 1) / LAZY_CHUNK);
            let mut stop = c;
            while stop < limit && !self.chunks.contains_key(&(stop + 1)) {
                stop += 1;
            }
            let lo = c * LAZY_CHUNK;
            let hi = ((stop + 1) * LAZY_CHUNK).min(self.size);
            let mut run = vec![0u8; (hi - lo) as usize];
            let fetch = LAZY_FETCH.get().ok_or(Errno::EIO)?;
            if !fetch(&self.url, lo, &mut run) {
                return Err(Errno::EIO);
            }
            for k in c..=stop {
                let a = ((k - c) * LAZY_CHUNK) as usize;
                let b = (a + LAZY_CHUNK as usize).min(run.len());
                self.chunks.insert(k, run[a..b].to_vec().into_boxed_slice());
                self.held += (b - a) as u64;
                TMPFS_BYTES.fetch_add((b - a) as u64, core::sync::atomic::Ordering::Relaxed);
            }
            c = stop + 1;
        }
        let mut at = offset;
        while at < end {
            let k = at / LAZY_CHUNK;
            let chunk = &self.chunks[&k];
            let from = (at - k * LAZY_CHUNK) as usize;
            let n = ((end - at) as usize).min(chunk.len() - from);
            let o = (at - offset) as usize;
            buf[o..o + n].copy_from_slice(&chunk[from..from + n]);
            at += n as u64;
        }
        Ok(end - offset)
    }
}

/// Where a tmpfs gets the time for a file it writes. A host without
/// `std::time` (the browser) installs its own, the clock it gives the kernel.
static CLOCK: std::sync::OnceLock<crate::syscalls::ClockFn> = std::sync::OnceLock::new();

/// Set the clock every tmpfs in this module instance stamps writes with. The
/// first call wins.
pub fn set_clock(clock: crate::syscalls::ClockFn) {
    let _ = CLOCK.set(clock);
}

pub(crate) fn now() -> i64 {
    (CLOCK.get().copied().unwrap_or(crate::syscalls::system_clock))().0 as i64
}

#[derive(Debug)]
enum Node {
    File(Arc<FileNode>),
    Dir,
    Symlink(String),
    /// A bound UNIX socket. It holds no data -- reading or writing goes
    /// through the socket, not the path -- but it has to be visible to
    /// `stat` and removable by `unlink`.
    Socket,
}

#[derive(Debug, Default)]
struct Inner {
    /// Path (relative to the mount, no leading slash) to node.
    nodes: BTreeMap<String, Node>,
    next_ino: u64,
    inos: BTreeMap<String, u64>,
    /// Modes for the files that are not the 0644 default. Sparse: only the
    /// few hundred binaries in a rootfs differ. Needed because dpkg rejects a
    /// `/bin/sh` whose execute bit was silently dropped.
    modes: BTreeMap<String, u32>,
    /// Modification times of the nodes that are not files (a file keeps its
    /// own, see `FileNode`), set by `utimensat`; sparse like `modes`, and a
    /// path without one reports 0.
    mtimes: BTreeMap<String, i64>,
}

/// An in-memory filesystem.
pub struct TmpFs {
    inner: Arc<Mutex<Inner>>,
}

impl TmpFs {
    /// `utimensat` with the change time given (see
    /// [`FileHandle::set_times_now`]). A file keeps both times to the
    /// nanosecond; anything else keeps whole-second modification times.
    pub fn set_times_now(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            None => Err(Errno::ENOENT),
            Some(Node::File(f)) => {
                f.set_times(atime, mtime, now);
                Ok(())
            }
            Some(_) => {
                if let Some((s, _)) = mtime {
                    inner.mtimes.insert(path.to_string(), s);
                }
                Ok(())
            }
        }
    }

    pub fn new() -> TmpFs {
        let mut inner = Inner {
            next_ino: 2,
            ..Default::default()
        };
        inner.nodes.insert(String::new(), Node::Dir);
        inner.inos.insert(String::new(), 1);
        TmpFs {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Pre-populate a file, for seeding a root filesystem at startup.
    pub fn add_file(&self, path: &str, contents: Vec<u8>) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        inner.nodes.insert(
            path,
            Node::File(Arc::new(FileNode::new(contents, ino))),
        );
    }

    /// A file of `size` bytes whose contents are fetched from `url` as they
    /// are read (`set_lazy_fetch`). A path already present is left alone:
    /// a bundled copy wins.
    pub fn add_lazy(&self, path: &str, size: u64, url: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(&path) {
            return;
        }
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        let f = FileNode::new(Vec::new(), ino);
        *f.lazy.lock().unwrap() = Some(Lazy { url: url.to_string(), size, chunks: Default::default(), held: 0 });
        inner.nodes.insert(path, Node::File(Arc::new(f)));
    }

    pub fn add_dir(&self, path: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.entry(path.clone()).or_insert(ino);
        inner.nodes.entry(path).or_insert(Node::Dir);
    }

    /// A socket node the host serves. Must be visible to `stat`, not just
    /// `connect`, so a program that checks for the service before connecting
    /// finds it.
    pub fn add_socket(&self, path: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        inner.nodes.insert(path, Node::Socket);
    }

    pub fn add_symlink(&self, path: &str, target: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        inner.nodes.insert(path, Node::Symlink(target.to_string()));
    }

    /// Create every missing directory above `path`.
    fn ensure_parents(inner: &mut Inner, path: &str) {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if part.is_empty() {
                continue;
            }
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if !inner.nodes.contains_key(&acc) {
                let ino = inner.next_ino;
                inner.next_ino += 1;
                inner.inos.insert(acc.clone(), ino);
                inner.nodes.insert(acc.clone(), Node::Dir);
            }
        }
    }

    fn ino_of(inner: &Inner, path: &str) -> u64 {
        inner.inos.get(path).copied().unwrap_or(1)
    }

    /// A directory's link count: `.`, its name in the parent, and a `..` from
    /// each immediate subdirectory -- i.e. `2 + immediate subdirs`. An empty
    /// directory is 2, which is what programs test to decide it has no
    /// subdirectories without reading it.
    fn dir_nlink(inner: &Inner, path: &str) -> u32 {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let subdirs = inner
            .nodes
            .iter()
            .filter(|(k, v)| {
                matches!(v, Node::Dir)
                    && k.strip_prefix(&prefix)
                        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .count();
        2 + subdirs as u32
    }
}

impl Default for TmpFs {
    fn default() -> Self {
        Self::new()
    }
}

/// A handle onto one tmpfs node.
#[derive(Debug)]
struct TmpHandle {
    inner: Arc<Mutex<Inner>>,
    path: String,
    /// The contents, when this handle is onto a file. Held directly, so the
    /// descriptor keeps working after the name is unlinked or renamed.
    file: Option<Arc<FileNode>>,
}

impl FileHandle for TmpHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let Some(file) = &self.file else {
            return Err(Errno::EISDIR);
        };
        if let Some(l) = file.lazy.lock().unwrap().as_mut() {
            return l.read(offset, buf);
        }
        let data = file.data.lock().unwrap();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n as u64)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let Some(file) = &self.file else {
            return Err(Errno::EISDIR);
        };
        file.materialize()?;
        let mut data = file.data.lock().unwrap();
        let start = offset as usize;
        if start + buf.len() > data.len() {
            let was = data.capacity();
            data.resize(start + buf.len(), 0);
            note_capacity(was, data.capacity());
        }
        data[start..start + buf.len()].copy_from_slice(buf);
        file.touch();
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        // An unlinked file still has a size and an inode number, so a handle
        // onto one answers from the contents rather than the tree.
        if let Some(file) = &self.file {
            return Ok(Metadata {
                file_type: FileType::Regular,
                size: file.size(),
                mode: 0o644,
                ino: file.ino,
                mtime: file.mtime.load(Ordering::Relaxed),
                nlink: 1,
                rdev: 0,
                times: file.times(),
            });
        }
        let inner = self.inner.lock().unwrap();
        let ino = TmpFs::ino_of(&inner, &self.path);
        let mtime = inner.mtimes.get(&self.path).copied().unwrap_or(0);
        match inner.nodes.get(&self.path) {
            Some(Node::File(f)) => Ok(Metadata {
                file_type: FileType::Regular,
                size: f.size(),
                mode: inner.modes.get(&self.path).copied().unwrap_or(0o644),
                ino,
                mtime: f.mtime.load(Ordering::Relaxed),
                // Every name that maps to this inode is a hard link to it.
                nlink: inner.inos.values().filter(|&&v| v == ino).count() as u32,
                rdev: 0,
                times: f.times(),
            }),
            Some(Node::Dir) => {
                let d = Metadata::dir();
                Ok(Metadata {
                    ino,
                    mtime,
                    nlink: TmpFs::dir_nlink(&inner, &self.path),
                    mode: inner.modes.get(&self.path).copied().unwrap_or(d.mode),
                    ..d
                })
            }
            Some(Node::Symlink(t)) => Ok(Metadata {
                file_type: FileType::Symlink,
                size: t.len() as u64,
                mode: 0o777,
                ino,
                mtime,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            }),
            Some(Node::Socket) => Ok(Metadata {
                file_type: FileType::Socket,
                size: 0,
                mode: 0o755,
                ino,
                mtime,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            }),
            None => Err(Errno::ENOENT),
        }
    }

    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let inner = self.inner.lock().unwrap();
        if !matches!(inner.nodes.get(&self.path), Some(Node::Dir)) {
            return Err(Errno::ENOTDIR);
        }
        let prefix = if self.path.is_empty() {
            String::new()
        } else {
            format!("{}/", self.path)
        };
        let mut out = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: TmpFs::ino_of(&inner, &self.path),
            },
            DirEntry {
                name: "..".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
        ];
        for (path, node) in &inner.nodes {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            // Only immediate children.
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            out.push(DirEntry {
                name: rest.to_string(),
                file_type: match node {
                    Node::File(_) => FileType::Regular,
                    Node::Dir => FileType::Directory,
                    Node::Symlink(_) => FileType::Symlink,
                    Node::Socket => FileType::Socket,
                },
                ino: TmpFs::ino_of(&inner, path),
            });
        }
        Ok(out)
    }

    fn set_mtime(&self, secs: i64) -> Result<(), Errno> {
        match &self.file {
            Some(f) => {
                f.stamp(secs);
                Ok(())
            }
            None => FileSystem::set_mtime(&TmpFs { inner: Arc::clone(&self.inner) }, &self.path, secs),
        }
    }

    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.set_times_now(atime, mtime, now())
    }

    fn set_times_now(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) -> Result<(), Errno> {
        match &self.file {
            Some(f) => {
                f.set_times(atime, mtime, now);
                Ok(())
            }
            None => TmpFs { inner: Arc::clone(&self.inner) }.set_times_now(&self.path, atime, mtime, now),
        }
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let Some(file) = &self.file else {
            return Err(Errno::EINVAL);
        };
        file.materialize()?;
        {
            let mut data = file.data.lock().unwrap();
            let was = data.capacity();
            data.resize(len as usize, 0);
            note_capacity(was, data.capacity());
        }
        file.touch();
        Ok(())
    }
}

impl FileSystem for TmpFs {
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let path = path.to_string();
        let file = {
            let mut inner = self.inner.lock().unwrap();
            match inner.nodes.get(&path) {
                Some(_) => {
                    if flags & oflags::O_EXCL != 0 && flags & oflags::O_CREAT != 0 {
                        return Err(Errno::EEXIST);
                    }
                    if flags & oflags::O_TRUNC != 0 {
                        if let Some(Node::File(f)) = inner.nodes.get(&path) {
                            if let Some(l) = f.lazy.lock().unwrap().take() {
                                TMPFS_BYTES.fetch_sub(l.held, core::sync::atomic::Ordering::Relaxed);
                            }
                            {
                                let mut data = f.data.lock().unwrap();
                                let was = data.capacity();
                                data.clear();
                                data.shrink_to_fit();
                                note_capacity(was, data.capacity());
                            }
                            f.touch();
                        }
                    }
                    match inner.nodes.get(&path) {
                        Some(Node::File(f)) => Some(Arc::clone(f)),
                        _ => None,
                    }
                }
                None => {
                    if flags & oflags::O_CREAT == 0 {
                        return Err(Errno::ENOENT);
                    }
                    // The parent must exist, as it must on a real filesystem.
                    let parent = match path.rfind('/') {
                        Some(i) => path[..i].to_string(),
                        None => String::new(),
                    };
                    if !matches!(inner.nodes.get(&parent), Some(Node::Dir)) {
                        return Err(Errno::ENOENT);
                    }
                    let ino = inner.next_ino;
                    inner.next_ino += 1;
                    inner.inos.insert(path.clone(), ino);
                    let f = Arc::new(FileNode::new(Vec::new(), ino));
                    inner.nodes.insert(path.clone(), Node::File(Arc::clone(&f)));
                    Some(f)
                }
            }
        };
        Ok(Arc::new(TmpHandle {
            inner: Arc::clone(&self.inner),
            path,
            file,
        }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let handle = TmpHandle {
            inner: Arc::clone(&self.inner),
            path: path.to_string(),
            file: None,
        };
        handle.metadata()
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.nodes.contains_key(path) {
            return Err(Errno::ENOENT);
        }
        let mode = mode & 0o7777;
        let default = if matches!(inner.nodes.get(path), Some(Node::Dir)) { Metadata::dir().mode } else { 0o644 };
        if mode == default {
            inner.modes.remove(path);
        } else {
            inner.modes.insert(path.to_string(), mode);
        }
        Ok(())
    }

    fn set_mtime(&self, path: &str, secs: i64) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            None => return Err(Errno::ENOENT),
            Some(Node::File(f)) => {
                f.stamp(secs);
                return Ok(());
            }
            Some(_) => {}
        }
        inner.mtimes.insert(path.to_string(), secs);
        Ok(())
    }

    fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.set_times_now(path, atime, mtime, now())
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Symlink(t)) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        }
    }

    fn unlink(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Dir) => Err(Errno::EISDIR),
            Some(_) => {
                inner.nodes.remove(path);
                inner.inos.remove(path);
                inner.mtimes.remove(path);
                Ok(())
            }
            None => Err(Errno::ENOENT),
        }
    }

    fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(path) {
            return Err(Errno::EEXIST);
        }
        let mode = mode & 0o7777;
        if mode != Metadata::dir().mode {
            inner.modes.insert(path.to_string(), mode);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.to_string(), ino);
        inner.nodes.insert(path.to_string(), Node::Dir);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        // ENOENT for a missing name, ENOTDIR for a name that is not a
        // directory -- Linux distinguishes them and dpkg relies on it:
        // ENOTDIR sends it down its "it is a file" path where a later ENOENT
        // is fatal and ends `apt-get install` at the unpack.
        match inner.nodes.get(path) {
            Some(Node::Dir) => {}
            Some(_) => return Err(Errno::ENOTDIR),
            None => return Err(Errno::ENOENT),
        }
        let prefix = format!("{path}/");
        if inner.nodes.keys().any(|k| k.starts_with(&prefix)) {
            return Err(Errno::ENOTEMPTY);
        }
        inner.nodes.remove(path);
        inner.inos.remove(path);
        inner.modes.remove(path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.remove(from) else {
            return Err(Errno::ENOENT);
        };
        let ino = inner.inos.remove(from).unwrap_or(1);
        // The mode travels with the name. `dpkg` unpacks a program to
        // `foo.dpkg-new`, chmods it, and renames it into place; leaving the
        // mode behind here means every binary a package installs arrives
        // without its execute bit, and the failure surfaces later as a shell
        // saying the program does not exist.
        let mode = inner.modes.remove(from);
        match inner.mtimes.remove(from) {
            Some(t) => {
                inner.mtimes.insert(to.to_string(), t);
            }
            None => {
                inner.mtimes.remove(to);
            }
        }
        inner.nodes.insert(to.to_string(), node);
        inner.inos.insert(to.to_string(), ino);
        match mode {
            Some(m) => {
                inner.modes.insert(to.to_string(), m);
            }
            // The destination may have had a mode of its own, and this is a
            // different file now.
            None => {
                inner.modes.remove(to);
            }
        }
        Ok(())
    }

    /// A second name for the same node. The node is behind an `Arc`, so both
    /// names see one file and a write through either is a write to both --
    /// which is what a hard link is.
    fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        // Only a regular file can gain a second name; a directory cannot,
        // which is what Linux says too.
        let node = match inner.nodes.get(from) {
            Some(Node::File(f)) => Node::File(Arc::clone(f)),
            Some(Node::Symlink(t)) => Node::Symlink(t.clone()),
            Some(Node::Dir) => return Err(Errno::EPERM),
            // A socket has one name, the one it was bound to.
            Some(Node::Socket) => return Err(Errno::EPERM),
            None => return Err(Errno::ENOENT),
        };
        if inner.nodes.contains_key(to) {
            return Err(Errno::EEXIST);
        }
        let ino = inner.inos.get(from).copied().unwrap_or(1);
        // Two names for one inode, so one mode: `/usr/bin/perl` is a hard
        // link and has to be as executable as `perl5.36.0` is.
        let mode = inner.modes.get(from).copied();
        inner.nodes.insert(to.to_string(), node);
        inner.inos.insert(to.to_string(), ino);
        if let Some(m) = mode {
            inner.modes.insert(to.to_string(), m);
        }
        Ok(())
    }

    fn mksocket(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        // Rebinding over a live name is the caller's error, but a stale node
        // left by a crashed server is ordinary: the X server unlinks the path
        // before it binds, and so does everything else.
        if inner.nodes.contains_key(path) {
            return Err(Errno::EADDRINUSE);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.to_string(), ino);
        inner.nodes.insert(path.to_string(), Node::Socket);
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(path) {
            return Err(Errno::EEXIST);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.to_string(), ino);
        inner
            .nodes
            .insert(path.to_string(), Node::Symlink(target.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_read_roundtrip() {
        let fs = TmpFs::new();
        let f = fs
            .open("hello.txt", oflags::O_CREAT | oflags::O_RDWR, 0o644)
            .unwrap();
        assert_eq!(f.write_at(0, b"gex").unwrap(), 3);
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"gex");
        assert_eq!(f.metadata().unwrap().size, 3);
    }

    #[test]
    fn writing_past_the_end_extends_with_zeros() {
        let fs = TmpFs::new();
        let f = fs.open("f", oflags::O_CREAT | oflags::O_RDWR, 0).unwrap();
        f.write_at(4, b"xy").unwrap();
        let mut buf = [0xffu8; 6];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 6);
        assert_eq!(&buf, b"\0\0\0\0xy");
    }

    #[test]
    fn opening_a_missing_file_without_creat_fails() {
        let fs = TmpFs::new();
        assert_eq!(fs.open("nope", 0, 0).err(), Some(Errno::ENOENT));
    }

    #[test]
    fn directories_list_only_immediate_children() {
        let fs = TmpFs::new();
        fs.add_file("a/b/deep.txt", b"x".to_vec());
        fs.add_file("a/top.txt", b"y".to_vec());
        let d = fs.open("a", 0, 0).unwrap();
        let names: Vec<String> = d.read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"b".to_string()));
        assert!(names.contains(&"top.txt".to_string()));
        assert!(!names.contains(&"deep.txt".to_string()));
    }

    #[test]
    fn rmdir_tells_a_missing_name_from_one_that_is_not_a_directory() {
        // dpkg reads the difference. Making sure `/var/lib/dpkg/tmp.ci` is
        // absent it calls `rmdir`, and `ENOTDIR` sends it down the "it is a
        // file" path, where the `ENOENT` from the unlink becomes fatal.
        let fs = TmpFs::new();
        fs.add_file("f", b"x".to_vec());
        assert_eq!(fs.rmdir("nothing-here").err(), Some(Errno::ENOENT));
        assert_eq!(fs.rmdir("f").err(), Some(Errno::ENOTDIR));
    }

    #[test]
    fn a_renamed_file_keeps_its_mode() {
        // What dpkg does with every program it installs: unpack to
        // `.dpkg-new`, chmod, rename into place.
        let fs = TmpFs::new();
        fs.add_file("p.dpkg-new", b"ELF".to_vec());
        fs.chmod("p.dpkg-new", 0o755).unwrap();
        fs.rename("p.dpkg-new", "p").unwrap();
        assert_eq!(fs.metadata("p").unwrap().mode, 0o755);
    }

    #[test]
    fn a_file_has_a_time_and_an_open_descriptor_sees_it() {
        // `gzip` stores its input's time from `fstat` of the open file; a
        // tmpfs that answered 0 there made every archive say 1970.
        let fs = TmpFs::new();
        fs.add_file("in", b"x".to_vec());
        assert!(fs.metadata("in").unwrap().mtime > 1_600_000_000, "a new file is stamped now");
        fs.set_mtime("in", 1_234_567_890).unwrap();
        let h = fs.open("in", 0, 0).unwrap();
        assert_eq!(h.metadata().unwrap().mtime, 1_234_567_890);
        // A rename keeps it; a write moves it on.
        fs.rename("in", "out").unwrap();
        assert_eq!(fs.metadata("out").unwrap().mtime, 1_234_567_890);
        h.write_at(0, b"y").unwrap();
        assert!(fs.metadata("out").unwrap().mtime > 1_600_000_000);
        // Through the descriptor, as `futimens` would.
        h.set_mtime(7).unwrap();
        assert_eq!(fs.metadata("out").unwrap().mtime, 7);
    }

    #[test]
    fn rmdir_refuses_a_non_empty_directory() {
        let fs = TmpFs::new();
        fs.add_file("d/f", b"x".to_vec());
        assert_eq!(fs.rmdir("d").err(), Some(Errno::ENOTEMPTY));
        fs.unlink("d/f").unwrap();
        assert!(fs.rmdir("d").is_ok());
    }

    #[test]
    fn an_unlinked_file_stays_usable_through_an_open_descriptor() {
        // What SQLite does with its temporary files: create, unlink at once,
        // then keep writing through the descriptor it still holds.
        let fs = TmpFs::new();
        let f = fs
            .open("scratch", oflags::O_CREAT | oflags::O_RDWR, 0o600)
            .unwrap();
        fs.unlink("scratch").unwrap();
        assert_eq!(fs.metadata("scratch").err(), Some(Errno::ENOENT));
        assert_eq!(f.write_at(0, b"still here").unwrap(), 10);
        let mut buf = [0u8; 10];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 10);
        assert_eq!(&buf, b"still here");
        assert_eq!(f.metadata().unwrap().size, 10);
    }

    #[test]
    fn renaming_does_not_disturb_an_open_descriptor() {
        let fs = TmpFs::new();
        let f = fs.open("a", oflags::O_CREAT | oflags::O_RDWR, 0).unwrap();
        f.write_at(0, b"xyz").unwrap();
        fs.rename("a", "b").unwrap();
        let mut buf = [0u8; 3];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"xyz");
    }

    #[test]
    fn symlinks_report_their_target() {
        let fs = TmpFs::new();
        fs.add_file("real", b"data".to_vec());
        fs.symlink("real", "link").unwrap();
        assert_eq!(fs.read_link("link").unwrap(), "real");
        assert_eq!(fs.read_link("real").err(), Some(Errno::EINVAL));
    }
}

#[cfg(test)]
mod lazy_tests {
    use super::*;

    /// Byte `i` of every lazy file here is `i % 251`.
    fn fake(url: &str, offset: u64, into: &mut [u8]) -> bool {
        assert_eq!(url, "/data/f");
        for (k, b) in into.iter_mut().enumerate() {
            *b = ((offset + k as u64) % 251) as u8;
        }
        true
    }

    #[test]
    fn lazy_reads_fetch_what_they_touch() {
        set_lazy_fetch(fake);
        let fs = TmpFs::new();
        let size = 3 * LAZY_CHUNK + 123;
        fs.add_lazy("/d/f", size, "/data/f");
        let h = fs.open("d/f", 0, 0).unwrap();
        assert_eq!(h.metadata().unwrap().size, size);
        let mut buf = vec![0u8; 5000];
        let at = LAZY_CHUNK - 2000;
        assert_eq!(h.read_at(at, &mut buf).unwrap(), 5000);
        assert!(buf.iter().enumerate().all(|(k, &b)| b == ((at + k as u64) % 251) as u8));
        let mut tail = vec![0u8; 1000];
        assert_eq!(h.read_at(size - 100, &mut tail).unwrap(), 100);
        assert_eq!(tail[99], ((size - 1) % 251) as u8);
        // A write makes it an ordinary file with all its bytes.
        h.write_at(0, b"xy").unwrap();
        let mut two = [0u8; 4];
        h.read_at(0, &mut two).unwrap();
        assert_eq!(&two, &[b'x', b'y', 2, 3]);
        assert_eq!(h.metadata().unwrap().size, size);
        // A bundled file of the same name wins.
        fs.add_file("/e/g", b"abc".to_vec());
        fs.add_lazy("/e/g", 99, "/data/f");
        assert_eq!(fs.open("e/g", 0, 0).unwrap().metadata().unwrap().size, 3);
    }
}
