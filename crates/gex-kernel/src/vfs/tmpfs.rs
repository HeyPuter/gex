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
use std::sync::{Arc, LockResult, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Bytes tmpfs is holding, summed as `Vec` capacity rather than length: a
/// buffer that grew geometrically keeps the larger allocation whatever its
/// length says. For the memory census.
pub static TMPFS_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The directory a page keeps (its persist store's root, `watch`), without
/// its leading `/`, and how many times the program has finished a change
/// under it: a close after a write (or a create or truncation), a rename
/// into or out of it, an unlink. The store snapshots soon after the count moves
/// (web/src/persist-worker.js), so a save outlives the page without waiting
/// for a timer. Unset, nothing is counted.
static WATCHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
pub static SAVES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count changes finished under `root` ([`SAVES`]); the first root set stays.
pub fn watch(root: &str) {
    let _ = WATCHED.set(root.trim_matches('/').to_string());
}

/// A change to `path` (a tmpfs path, no leading `/`) finished: counted when
/// it is under the watched directory.
fn saved(path: &str) {
    let Some(w) = WATCHED.get() else { return };
    if path.starts_with(w.as_str()) && (w.is_empty() || path.len() == w.len() || path.as_bytes()[w.len()] == b'/') {
        SAVES.fetch_add(1, Ordering::Relaxed);
    }
}

/// This filesystem's half of an [`inotify::Key`](crate::inotify::Key).
fn fs_key(shared: &Arc<Shared>) -> usize {
    Arc::as_ptr(shared) as *const () as usize
}

/// A path's directory and its last name.
fn split_name(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// The inode `path` names and whether it is a directory.
fn node_ino(inner: &Inner, path: &str) -> Option<(u64, bool)> {
    match inner.nodes.get(path)? {
        Node::File(f) => Some((f.ino, false)),
        Node::Dir => Some((TmpFs::ino_of(inner, path), true)),
        _ => Some((TmpFs::ino_of(inner, path), false)),
    }
}

/// `ev` about name `path`, said to its directory's watches and (as `own`,
/// an inode and whether it is a directory) to its own. Nothing is worked
/// out while nothing is watched.
fn note(inner: &Inner, fs: usize, path: &str, own: Option<(u64, bool)>, ev: u32, cookie: u32) {
    use crate::inotify::{entry_event, Key};
    if !crate::inotify::any() {
        return;
    }
    let (dir, name) = split_name(path);
    let isdir = own.is_some_and(|(_, d)| d);
    entry_event(Key { fs, ino: TmpFs::ino_of(inner, dir) }, name, own.map(|(ino, _)| Key { fs, ino }), ev, cookie, isdir);
}

/// [`note`] of what `path` names now.
fn note_path(inner: &Inner, fs: usize, path: &str, ev: u32) {
    if crate::inotify::any() {
        note(inner, fs, path, node_ino(inner, path), ev, 0);
    }
}

/// A name removed (`own`: the inode it named and whether that is a
/// directory): to the inode `IN_ATTRIB` (a file's link count changed), to
/// the directory `IN_DELETE`, and with the inode's last name gone
/// `IN_DELETE_SELF` to the inode, as Linux orders them.
fn note_removed(inner: &Inner, fs: usize, path: &str, own: (u64, bool)) {
    use crate::inotify::{event, mask, Key};
    if !crate::inotify::any() {
        return;
    }
    let key = Key { fs, ino: own.0 };
    let (dir, name) = split_name(path);
    if !own.1 {
        event(key, mask::ATTRIB, 0, None, false);
    }
    event(Key { fs, ino: TmpFs::ino_of(inner, dir) }, mask::DELETE, 0, Some(name), own.1);
    if own.1 || inner.nlink(own.0) == 0 {
        event(key, mask::DELETE_SELF, 0, None, own.1);
    }
}

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
/// The time of a lazy file whose site recorded none ([`TmpFs::add_lazy`]).
pub const LAZY_MTIME: i64 = 0;

/// The next first use's number ([`FileNode::read`]), shared by every
/// tmpfs: a site's first bundle is in the order its files were first used.
static READ_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

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
    /// Whether anything has used the file's bytes ([`TmpFs::read_set`]): a
    /// read of any kind (`read`, `pread`, `readv`, the source of `sendfile`,
    /// `splice` or `copy_file_range`), a mapping's copy, an advice that
    /// fetches, a write into it. Not an open, a `stat` or an `fstat`: Wine
    /// opens every SystemLink font to stat it and never reads one before the
    /// desktop. What a run needed of its bundle, which gex-eject's split
    /// keeps in the bundle. The first use's place among all files' first
    /// uses ([`READ_SEQ`]), from 1; 0 for none yet.
    read: core::sync::atomic::AtomicU64,
    /// Whether the program has made or changed the file since the root was
    /// seeded ([`TmpFs::written`]): a write, a truncation, a create, a
    /// rename. Not a time set (`utimensat`, which a program can aim at any
    /// file), not a lazy file's contents arriving, not the seeding itself.
    /// What a page's persist store snapshots (web/src/persist-worker.js),
    /// so a snapshot never reads a file the program did not touch.
    written: core::sync::atomic::AtomicBool,
    /// [`FileHandle::changes`]: bumped with the contents' lock held, so a
    /// reader that saw the same count before and after its read read
    /// contents no change was halfway through. From 1, as 0 is no count.
    changes: core::sync::atomic::AtomicU64,
}

impl FileNode {
    fn new(data: Vec<u8>, ino: u64) -> FileNode {
        FileNode {
            data: Mutex::new(data),
            lazy: Mutex::new(None),
            ino,
            mtime: AtomicI64::new(now()),
            times: Mutex::new(Default::default()),
            read: core::sync::atomic::AtomicU64::new(0),
            written: core::sync::atomic::AtomicBool::new(false),
            changes: core::sync::atomic::AtomicU64::new(1),
        }
    }
    /// One more change of the contents; called with `data` locked (the
    /// count's reader pairs with this through that lock: [`FileNode::changes`]).
    fn changed(&self) {
        self.changes.fetch_add(1, Ordering::AcqRel);
    }
    fn touch(&self) {
        self.mark_written();
        self.stamp(now());
    }
    /// Its bytes used ([`FileNode::read`]). A load first: a store to a line
    /// every Worker reads would bounce it at each read.
    fn mark_read(&self) {
        if self.read.load(Ordering::Relaxed) == 0 {
            let n = READ_SEQ.fetch_add(1, Ordering::Relaxed);
            let _ = self.read.compare_exchange(0, n, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
    fn mark_written(&self) {
        if !self.written.load(Ordering::Relaxed) {
            self.written.store(true, Ordering::Relaxed);
        }
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

    /// A bundled copy shorter than the lazy file of `size` bytes at `url`,
    /// and a whole number of chunks long, is that file's head: what a site's
    /// first bundle carries of a file read only in part before its first
    /// frame (a game's music, its first track's start), the rest left in the
    /// rest bundle. A copy a whole number of chunks and then the file's last,
    /// partial chunk long is a head and that tail: an archive whose table is
    /// at its end. The file becomes the lazy one with those chunks already
    /// held, so those reads fetch nothing. Any other copy wins as is.
    fn seed_head(&self, size: u64, url: &str) {
        let mut lazy = self.lazy.lock().unwrap();
        if lazy.is_some() {
            return;
        }
        let mut data = self.data.lock().unwrap();
        let have = data.len() as u64;
        let last = size % LAZY_CHUNK;
        let head = if have % LAZY_CHUNK == 0 {
            have
        } else if last != 0 && have % LAZY_CHUNK == last {
            have - last
        } else {
            return;
        };
        if have == 0 || have >= size {
            return;
        }
        // Kept for good: the rest bundle at `url` does not carry them.
        let mut chunks: std::collections::HashMap<u64, (Box<[u8]>, u64)> = data[..head as usize]
            .chunks(LAZY_CHUNK as usize)
            .enumerate()
            .map(|(k, c)| (k as u64, (c.to_vec().into_boxed_slice(), SEEDED)))
            .collect();
        if have > head {
            chunks.insert(size / LAZY_CHUNK, (data[head as usize..].to_vec().into_boxed_slice(), SEEDED));
        }
        let was = data.capacity();
        *data = Vec::new();
        note_capacity(was, 0);
        TMPFS_BYTES.fetch_add(have, core::sync::atomic::Ordering::Relaxed);
        self.changed();
        *lazy = Some(Lazy { url: url.to_string(), size, chunks, held: have, ahead: 0, reads: 0 });
    }

    /// Make a lazy file an ordinary one (all of it fetched), before a write
    /// or a truncation changes it.
    fn materialize(&self) -> Result<(), Errno> {
        // What a write or a truncation leaves of the file is the bundle's.
        self.mark_read();
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
/// The most chunks past the ones read that a sequential read fetches with
/// them. A miss fetches what it reads, rounded to chunks; a miss right
/// after a chunk already held reads ahead, twice as far as the last
/// sequential miss did (1, 2, 4, 8, then this), and any other miss starts
/// over. One synchronous request per MiB was 0.8 s of apt installing a 9 MB
/// .deb of ours from the page; 16 MiB from offset 0 at once was a whole
/// 22 MB DLL fetched (24 s at 0.7 MB/s) for a look at its headers and an
/// icon.
const LAZY_READAHEAD: u64 = 15;
/// The most of a lazy file's bytes kept: past it, the chunks read longest
/// ago go (a lazy file is never written; a chunk read again is fetched
/// again). A game's data reads its gigabytes once, mostly.
const LAZY_HELD_MAX: u64 = if cfg!(test) { 8 << 20 } else { 256 << 20 };
/// A lazy file's chunk that came with a bundle ([`FileNode::seed_head`]),
/// as its last use: it is kept, since the file's URL does not carry it.
const SEEDED: u64 = u64::MAX;

/// A file whose bytes live elsewhere (a URL) and arrive a chunk at a time,
/// as they are read: a game's gigabytes of data served to a page.
#[derive(Debug)]
struct Lazy {
    url: String,
    size: u64,
    /// Each chunk held, and the read that last used it ([`SEEDED`]: one
    /// seeded from a bundle, never evicted).
    chunks: std::collections::HashMap<u64, (Box<[u8]>, u64)>,
    held: u64,
    /// The readahead of the last sequential miss, in chunks (0: none yet).
    ahead: u64,
    /// Reads so far: the clock of the chunks' last uses.
    reads: u64,
}

impl Lazy {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<u64, Errno> {
        self.read_ahead(offset, buf, true)
    }

    /// [`Lazy::read`], with or without the readahead.
    fn read_ahead(&mut self, offset: u64, buf: &mut [u8], ahead: bool) -> Result<u64, Errno> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let end = (offset + buf.len() as u64).min(self.size);
        self.fetch(offset / LAZY_CHUNK, (end - 1) / LAZY_CHUNK, ahead)?;
        self.reads += 1;
        let mut at = offset;
        while at < end {
            let k = at / LAZY_CHUNK;
            let (chunk, used) = self.chunks.get_mut(&k).expect("fetched");
            if *used != SEEDED {
                *used = self.reads;
            }
            let from = (at - k * LAZY_CHUNK) as usize;
            let n = ((end - at) as usize).min(chunk.len() - from);
            let o = (at - offset) as usize;
            buf[o..o + n].copy_from_slice(&chunk[from..from + n]);
            at += n as u64;
        }
        Ok(end - offset)
    }

    /// Have chunks `first..=last` here: the missing ones in runs, one
    /// request a run (and with `ahead`, a sequential run reads ahead).
    fn fetch(&mut self, first: u64, last: u64, ahead: bool) -> Result<(), Errno> {
        let mut c = first;
        while c <= last {
            if self.chunks.contains_key(&c) {
                c += 1;
                continue;
            }
            let limit = if ahead {
                let sequential = c > 0 && self.chunks.contains_key(&(c - 1));
                self.ahead = if sequential { (self.ahead * 2).clamp(1, LAZY_READAHEAD) } else { 0 };
                last + self.ahead
            } else {
                last
            };
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
                self.chunks.insert(k, (run[a..b].to_vec().into_boxed_slice(), self.reads));
                self.held += (b - a) as u64;
                TMPFS_BYTES.fetch_add((b - a) as u64, core::sync::atomic::Ordering::Relaxed);
            }
            self.evict(first, last.max(stop));
            c = stop + 1;
        }
        Ok(())
    }

    /// Past [`LAZY_HELD_MAX`], the chunks used longest ago go, never one of
    /// `keep_first..=keep_last` (the read being served needs them, however
    /// many) nor a [`SEEDED`] one.
    fn evict(&mut self, keep_first: u64, keep_last: u64) {
        while self.held > LAZY_HELD_MAX {
            let Some(k) = self.chunks.iter().filter(|(k, (_, used))| *used != SEEDED && !(keep_first..=keep_last).contains(*k)).min_by_key(|(_, (_, used))| *used).map(|(k, _)| *k) else { return };
            let (c, _) = self.chunks.remove(&k).expect("listed");
            self.held -= c.len() as u64;
            TMPFS_BYTES.fetch_sub(c.len() as u64, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// `posix_fadvise` of this lazy file: `WILLNEED` fetches the chunks of
    /// the range (no readahead), [`super::FADV_GEX_IMAGE`] those the
    /// wineserver reads of the image ([`super::peimage::server_reads`]).
    fn advise(&mut self, offset: u64, len: u64, advice: u32) -> Result<(), Errno> {
        match advice {
            super::FADV_WILLNEED if offset < self.size => {
                let end = if len == 0 { self.size } else { offset.saturating_add(len).min(self.size) };
                self.fetch(offset / LAZY_CHUNK, (end - 1) / LAZY_CHUNK, false)
            }
            super::FADV_GEX_IMAGE => {
                let mut failed = None;
                super::peimage::server_reads(&mut |at, n| {
                    let mut b = vec![0u8; n];
                    match self.read_ahead(at, &mut b, false) {
                        Ok(got) => {
                            b.truncate(got as usize);
                            Some(b)
                        }
                        Err(e) => {
                            failed = Some(e);
                            None
                        }
                    }
                });
                failed.map_or(Ok(()), Err)
            }
            _ => Ok(()),
        }
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

/// The same clock in nanoseconds, for a device that paces itself by it
/// (`oss`): `std::time` panics in the browser.
pub(crate) fn now_ns() -> u64 {
    let (secs, nanos) = (CLOCK.get().copied().unwrap_or(crate::syscalls::system_clock))();
    secs.saturating_mul(1_000_000_000).saturating_add(nanos as u64)
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
    /// How many names each inode in `inos` has: a file's link count, kept
    /// as names come and go (counting `inos` at every `stat` scanned the
    /// whole tree, 16 us a call once a Python was installed).
    links: BTreeMap<u64, u32>,
    /// How many times each directory that has moved has moved, itself or
    /// with a directory above it ([`super::Moved::epoch`]). Sparse.
    moves: BTreeMap<u64, u64>,
    /// Extended attributes (`user.*`), by inode, so they follow a rename
    /// and every hard link; dropped with the inode's last name. Sparse:
    /// Wine's DOS attributes (`user.DOSATTRIB`) are on a few files.
    xattrs: BTreeMap<u64, BTreeMap<String, Vec<u8>>>,
}

impl Inner {
    /// `chmod` on `path`: its mode kept only where it is not the default.
    fn chmod(&mut self, path: &str, mode: u32) -> Result<(), Errno> {
        let default = match self.nodes.get(path) {
            None => return Err(Errno::ENOENT),
            Some(Node::Dir) => Metadata::dir().mode,
            Some(_) => 0o644,
        };
        let mode = mode & 0o7777;
        if mode == default {
            self.modes.remove(path);
        } else {
            self.modes.insert(path.to_string(), mode);
        }
        Ok(())
    }

    /// Name `path` inode `ino`, and count the link.
    fn set_ino(&mut self, path: String, ino: u64) {
        if let Some(old) = self.inos.insert(path, ino) {
            self.unlink_ino(old);
        }
        *self.links.entry(ino).or_insert(0) += 1;
    }

    /// Forget `path`'s inode, and the link; the inode it had.
    fn drop_ino(&mut self, path: &str) -> Option<u64> {
        let old = self.inos.remove(path)?;
        self.unlink_ino(old);
        Some(old)
    }

    fn unlink_ino(&mut self, ino: u64) {
        if let Some(n) = self.links.get_mut(&ino) {
            *n -= 1;
            if *n == 0 {
                self.links.remove(&ino);
                self.xattrs.remove(&ino);
            }
        }
    }

    /// An extended-attribute call on inode `ino` (`symlink`: the node is a
    /// link, whose `user.` attributes Linux refuses to set).
    fn xattr(&mut self, ino: u64, symlink: bool, op: super::XattrOp<'_>) -> Result<usize, Errno> {
        if symlink && matches!(op, super::XattrOp::Set { .. }) {
            return Err(Errno::EPERM);
        }
        let mut none = BTreeMap::new();
        let attrs = match &op {
            super::XattrOp::Set { .. } => self.xattrs.entry(ino).or_default(),
            _ => self.xattrs.get_mut(&ino).unwrap_or(&mut none),
        };
        let r = super::xattr_apply(attrs, op);
        if self.xattrs.get(&ino).is_some_and(|a| a.is_empty()) {
            self.xattrs.remove(&ino);
        }
        r
    }

    fn nlink(&self, ino: u64) -> u32 {
        self.links.get(&ino).copied().unwrap_or(0)
    }

    /// Where directory `ino`, opened at `was`, is now: `was` unless it has
    /// been renamed since (or something under it has been: a directory
    /// moves with its parent), `None` once it is removed. A directory has
    /// one name, so its inode names it.
    fn dir_now(&self, was: &str, ino: u64) -> Option<String> {
        if self.inos.get(was) == Some(&ino) && matches!(self.nodes.get(was), Some(Node::Dir)) {
            return Some(was.to_string());
        }
        self.inos
            .iter()
            .find(|(p, i)| **i == ino && matches!(self.nodes.get(p.as_str()), Some(Node::Dir)))
            .map(|(p, _)| p.clone())
    }

    /// Whether directory `path` has anything in it.
    fn has_children(&self, path: &str) -> bool {
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        self.nodes.range::<String, _>(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix) && k.len() > prefix.len())
    }

    /// Remove name `path` and what it keeps beside the node.
    fn forget(&mut self, path: &str) {
        self.nodes.remove(path);
        self.drop_ino(path);
        self.modes.remove(path);
        self.mtimes.remove(path);
    }

    /// Move everything named under directory `from` to the same names under
    /// `to`: nodes, inodes, modes and times (a link count does not change,
    /// a name only moves).
    fn move_under(&mut self, from: &str, to: &str) {
        fn rekey<V>(map: &mut BTreeMap<String, V>, from: &str, to: &str) {
            // A directory's descendants are the keys from "from/" up to
            // "from0", '0' following '/'.
            let lo = format!("{from}/");
            let hi = format!("{from}0");
            let keys: Vec<String> = map.range::<String, _>(lo..hi).map(|(k, _)| k.clone()).collect();
            for k in keys {
                let v = map.remove(&k).unwrap();
                map.insert(format!("{to}{}", &k[from.len()..]), v);
            }
        }
        rekey(&mut self.nodes, from, to);
        rekey(&mut self.inos, from, to);
        rekey(&mut self.modes, from, to);
        rekey(&mut self.mtimes, from, to);
    }
}

/// The tree, and its rename lock ([`super::RenameGate`]), taken before the
/// tree's. A reader-writer lock: lookups (`stat`, the walks, `readlink`)
/// share it, so threads asking at once do not queue behind each other
/// (the global kernel's root is one of these, for every process).
#[derive(Debug)]
struct Shared {
    m: RwLock<Inner>,
    gate: super::RenameGate,
}

impl Shared {
    /// The tree to change.
    fn lock(&self) -> LockResult<RwLockWriteGuard<'_, Inner>> {
        self.m.write()
    }
    /// The tree to look at.
    fn read(&self) -> LockResult<RwLockReadGuard<'_, Inner>> {
        self.m.read()
    }
}

/// An in-memory filesystem.
pub struct TmpFs {
    inner: Arc<Shared>,
}

impl TmpFs {
    /// `utimensat` with the change time given (see
    /// [`FileHandle::set_times_now`]). A file keeps both times to the
    /// nanosecond; anything else keeps whole-second modification times.
    pub fn set_times_now(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            None => return Err(Errno::ENOENT),
            Some(Node::File(f)) => f.set_times(atime, mtime, now),
            Some(_) => {
                if let Some((s, _)) = mtime {
                    inner.mtimes.insert(path.to_string(), s);
                }
            }
        }
        note_path(&inner, fs_key(&self.inner), path, crate::inotify::mask::ATTRIB);
        Ok(())
    }

    /// [`super::walk_plain`] here: the walk's answer for `abs` with no link
    /// on it, `None` for [`FileSystem::walk`] to follow them.
    pub fn walk_plain(&self, abs: &str, follow_final: bool, mounts: &str) -> Option<bool> {
        let inner = self.inner.read().unwrap();
        super::walk_plain(abs, follow_final, mounts, &|rel| matches!(inner.nodes.get(rel), Some(Node::Symlink(_))))
    }

    /// How many directories have been renamed here
    /// ([`FileHandle::rename_epoch`]).
    pub fn renames(&self) -> u64 {
        self.inner.gate.epoch()
    }

    /// [`FileSystem::rename2`] by a caller holding `own` of the directory
    /// renames held off ([`super::own_holds`]; the global kernel's caller
    /// says, `k_fs_rename`). A directory moves under the rename lock, which
    /// waits while another thread's lookup holds directory renames off.
    /// What `from` is is checked again under the tree's lock: `EAGAIN` from
    /// `rename_locked` is "it changed meanwhile".
    pub fn rename2_as(&self, from: &str, to: &str, noreplace: bool, own: u32) -> Result<(), Errno> {
        loop {
            let dir = matches!(self.inner.read().unwrap().nodes.get(from), Some(Node::Dir));
            match self.inner.gate.rename(own, dir, || self.rename_locked(from, to, noreplace, dir)) {
                Err(Errno::EAGAIN) => continue,
                r => {
                    if r.is_ok() {
                        saved(from);
                        saved(to);
                    }
                    return r;
                }
            }
        }
    }

    fn rename_locked(&self, from: &str, to: &str, noreplace: bool, dir: bool) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.get(from) else {
            return Err(Errno::ENOENT);
        };
        let is_dir = matches!(node, Node::Dir);
        if is_dir != dir {
            return Err(Errno::EAGAIN);
        }
        let parent = to.rfind('/').map_or("", |i| &to[..i]);
        if !matches!(inner.nodes.get(parent), Some(Node::Dir)) {
            return Err(if inner.nodes.contains_key(parent) { Errno::ENOTDIR } else { Errno::ENOENT });
        }
        if from == to {
            return if noreplace { Err(Errno::EEXIST) } else { Ok(()) };
        }
        if is_dir && to.starts_with(from) && (from.is_empty() || to.as_bytes().get(from.len()) == Some(&b'/')) {
            return Err(Errno::EINVAL);
        }
        let mut replaced = None;
        if let Some(dst) = inner.nodes.get(to) {
            if noreplace {
                return Err(Errno::EEXIST);
            }
            match (is_dir, matches!(dst, Node::Dir)) {
                (false, true) => return Err(Errno::EISDIR),
                (true, false) => return Err(Errno::ENOTDIR),
                (true, true) if inner.has_children(to) => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
            // Two names for one file: Linux leaves both.
            if !is_dir && inner.inos.get(from).is_some() && inner.inos.get(from) == inner.inos.get(to) {
                return Ok(());
            }
            replaced = node_ino(&inner, to);
            inner.forget(to);
        }
        let own = node_ino(&inner, from);
        let node = inner.nodes.remove(from).unwrap();
        let ino = inner.inos.remove(from).unwrap_or(1);
        // A file under a new name is a change to keep; a directory's files
        // are marked one by one, since each now has a new path.
        if let Node::File(f) = &node {
            f.mark_written();
        }
        // The mode travels with the name. `dpkg` unpacks a program to
        // `foo.dpkg-new`, chmods it, and renames it into place; leaving the
        // mode behind here means every binary a package installs arrives
        // without its execute bit, and the failure surfaces later as a shell
        // saying the program does not exist.
        if let Some(m) = inner.modes.remove(from) {
            inner.modes.insert(to.to_string(), m);
        }
        if let Some(t) = inner.mtimes.remove(from) {
            inner.mtimes.insert(to.to_string(), t);
        }
        inner.nodes.insert(to.to_string(), node);
        inner.inos.insert(to.to_string(), ino);
        if is_dir {
            inner.move_under(from, to);
            let lo = format!("{to}/");
            let hi = format!("{to}0");
            for (_, n) in inner.nodes.range::<String, _>(lo..hi) {
                if let Node::File(f) = n {
                    f.mark_written();
                }
            }
            // It and every directory under it are somewhere else now.
            let lo = format!("{to}/");
            let hi = format!("{to}0");
            let mut moved: Vec<u64> = inner
                .nodes
                .range::<String, _>(lo..hi)
                .filter(|(_, n)| matches!(n, Node::Dir))
                .filter_map(|(k, _)| inner.inos.get(k).copied())
                .collect();
            moved.push(ino);
            for i in moved {
                *inner.moves.entry(i).or_insert(0) += 1;
            }
        }
        if crate::inotify::any() {
            use crate::inotify::{event, mask, Key};
            let fs = fs_key(&self.inner);
            let cookie = crate::inotify::cookie();
            let (fd, fname) = split_name(from);
            let (td, tname) = split_name(to);
            event(Key { fs, ino: TmpFs::ino_of(&inner, fd) }, mask::MOVED_FROM, cookie, Some(fname), is_dir);
            event(Key { fs, ino: TmpFs::ino_of(&inner, td) }, mask::MOVED_TO, cookie, Some(tname), is_dir);
            if let Some((ino, d)) = own {
                event(Key { fs, ino }, mask::MOVE_SELF, 0, None, d);
            }
            // The name moved over: its inode loses that name.
            if let Some((ino, d)) = replaced {
                if !d {
                    event(Key { fs, ino }, mask::ATTRIB, 0, None, false);
                }
                if d || inner.nlink(ino) == 0 {
                    event(Key { fs, ino }, mask::DELETE_SELF, 0, None, d);
                }
            }
        }
        Ok(())
    }

    pub fn new() -> TmpFs {
        let mut inner = Inner {
            next_ino: 2,
            ..Default::default()
        };
        inner.nodes.insert(String::new(), Node::Dir);
        inner.set_ino(String::new(), 1);
        TmpFs {
            inner: Arc::new(Shared { m: RwLock::new(inner), gate: Default::default() }),
        }
    }

    /// Pre-populate a file, for seeding a root filesystem at startup.
    pub fn add_file(&self, path: &str, contents: Vec<u8>) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.set_ino(path.clone(), ino);
        inner.nodes.insert(
            path,
            Node::File(Arc::new(FileNode::new(contents, ino))),
        );
    }

    /// Bytes of a seeded file ([`TmpFs::add_file`]) at `offset`, for a seeding
    /// that sends a large file in parts: the file grows to hold them and
    /// stays as seeded, not made or changed by the program
    /// ([`TmpFs::written`]). ENOENT when no file is at `path`.
    pub fn add_file_part(&self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
        let path = path.trim_start_matches('/');
        let f = match self.inner.lock().unwrap().nodes.get(path) {
            Some(Node::File(f)) => Arc::clone(f),
            _ => return Err(Errno::ENOENT),
        };
        let mut data = f.data.lock().unwrap();
        let start = offset as usize;
        let end = start + bytes.len();
        if end > data.len() {
            let was = data.capacity();
            reserve(&mut data, end)?;
            data.resize(end, 0);
            note_capacity(was, data.capacity());
        }
        data[start..end].copy_from_slice(bytes);
        Ok(())
    }

    /// A file of `size` bytes whose contents are fetched from `url` as they
    /// are read (`set_lazy_fetch`). A path already present is left alone:
    /// a bundled copy wins, unless it is the file's head ([`FileNode::seed_head`]).
    ///
    /// Its time is [`LAZY_MTIME`] until the site's recorded one is set
    /// (`set_mtime`): the same on every load, never the load's own clock, so
    /// what keys on a file's size and time (the translation cache across
    /// loads) still matches the next time the same site is loaded.
    pub fn add_lazy(&self, path: &str, size: u64, url: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        if let Some(node) = inner.nodes.get(&path) {
            if let Node::File(f) = node {
                f.seed_head(size, url);
            }
            return;
        }
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.set_ino(path.clone(), ino);
        let f = FileNode::new(Vec::new(), ino);
        f.mtime.store(LAZY_MTIME, Ordering::Relaxed);
        *f.lazy.lock().unwrap() = Some(Lazy { url: url.to_string(), size, chunks: Default::default(), held: 0, ahead: 0, reads: 0 });
        inner.nodes.insert(path, Node::File(Arc::new(f)));
    }

    /// Every file the program has made or changed since the root was seeded
    /// (`FileNode::written`), by path with its leading `/`, sorted.
    pub fn written(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .nodes
            .iter()
            .filter_map(|(p, n)| match n {
                Node::File(f) if f.written.load(Ordering::Relaxed) => Some(format!("/{p}")),
                _ => None,
            })
            .collect()
    }

    /// Where file contents lie in this address space, for a session
    /// snapshot that takes the pages still holding a source's bytes by
    /// reference: `(0, address, length, 0, path)` for a file's bytes (its
    /// path with the leading `/`), `(1, address, length, offset, url)` for a
    /// lazy file's chunk at `offset`. A file another thread holds is left out
    /// (the snapshot copies its pages, as it does any it cannot match).
    pub fn extents(&self) -> Vec<(u8, usize, usize, u64, String)> {
        let inner = self.inner.read().unwrap();
        let mut out = Vec::new();
        for (p, n) in inner.nodes.iter() {
            let Node::File(f) = n else { continue };
            if let Ok(d) = f.data.try_lock() {
                if !d.is_empty() {
                    out.push((0, d.as_ptr() as usize, d.len(), 0, format!("/{p}")));
                }
            }
            if let Ok(l) = f.lazy.try_lock() {
                if let Some(l) = l.as_ref() {
                    for (k, (c, _)) in l.chunks.iter() {
                        out.push((1, c.as_ptr() as usize, c.len(), k * LAZY_CHUNK, l.url.clone()));
                    }
                }
            }
        }
        out
    }

    /// Every file whose bytes something used ([`FileNode::read`]), by path
    /// (with its leading `/`; a hard-linked file under each of its names),
    /// in the order of their first uses (by path among one file's names). A
    /// recorded run's read set: gex-eject's bundle split keeps these in the
    /// bundle and serves the rest as lazy files, and a site's first bundle
    /// holds them in this order.
    pub fn read_set(&self) -> Vec<String> {
        let inner = self.inner.read().unwrap();
        let mut out: Vec<(u64, String)> = inner
            .nodes
            .iter()
            .filter_map(|(p, n)| match n {
                Node::File(f) => match f.read.load(Ordering::Relaxed) {
                    0 => None,
                    k => Some((k, format!("/{p}"))),
                },
                _ => None,
            })
            .collect();
        out.sort();
        out.into_iter().map(|(_, p)| p).collect()
    }

    pub fn add_dir(&self, path: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        if !inner.inos.contains_key(&path) {
            inner.set_ino(path.clone(), ino);
        }
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
        inner.set_ino(path.clone(), ino);
        inner.nodes.insert(path, Node::Socket);
    }

    pub fn add_symlink(&self, path: &str, target: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.set_ino(path.clone(), ino);
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
                inner.set_ino(acc.clone(), ino);
                inner.nodes.insert(acc.clone(), Node::Dir);
            }
        }
    }

    /// The metadata of the node at `path` (relative), from the tree.
    /// The node first: a miss (most of a lookup's answers) asks no more.
    fn meta_at(inner: &Inner, path: &str) -> Result<Metadata, Errno> {
        let Some(node) = inner.nodes.get(path) else {
            return Err(Errno::ENOENT);
        };
        TmpFs::meta_of(inner, path, node)
    }

    /// [`TmpFs::meta_at`] of `node`, found at `path`.
    fn meta_of(inner: &Inner, path: &str, node: &Node) -> Result<Metadata, Errno> {
        let ino = TmpFs::ino_of(inner, path);
        let mtime = || inner.mtimes.get(path).copied().unwrap_or(0);
        match node {
            Node::File(f) => Ok(Metadata {
                file_type: FileType::Regular,
                size: f.size(),
                mode: inner.modes.get(path).copied().unwrap_or(0o644),
                ino,
                mtime: f.mtime.load(Ordering::Relaxed),
                // Every name that maps to this inode is a hard link to it.
                nlink: inner.nlink(ino),
                rdev: 0,
                times: f.times(),
            }),
            Node::Dir => {
                let d = Metadata::dir();
                Ok(Metadata {
                    ino,
                    mtime: mtime(),
                    nlink: TmpFs::dir_nlink(inner, path),
                    mode: inner.modes.get(path).copied().unwrap_or(d.mode),
                    ..d
                })
            }
            Node::Symlink(t) => Ok(Metadata {
                file_type: FileType::Symlink,
                size: t.len() as u64,
                mode: 0o777,
                ino,
                mtime: mtime(),
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            }),
            Node::Socket => Ok(Metadata {
                file_type: FileType::Socket,
                size: 0,
                mode: 0o755,
                ino,
                mtime: mtime(),
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            }),
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
        // The immediate children only: past each child's own subtree in one
        // seek (a child's descendants are the keys from "child/" up to
        // "child0", '0' following '/'), where a scan of every node in the
        // tree was a directory `stat`. One pass otherwise: a seek from the
        // top per child was most of an `fstat` of a large directory (Wine's
        // `fdopendir` makes one per case-insensitive lookup).
        use std::ops::Bound::{Included, Unbounded};
        let mut subdirs = 0u32;
        let mut names = inner.nodes.range::<String, _>((Included(prefix.clone()), Unbounded));
        while let Some((k, v)) = names.next() {
            let Some(rest) = k.strip_prefix(&prefix) else {
                break;
            };
            match rest.find('/') {
                Some(i) => {
                    names = inner.nodes.range::<String, _>((Included(format!("{prefix}{}0", &rest[..i])), Unbounded));
                }
                None => {
                    if !rest.is_empty() && matches!(v, Node::Dir) {
                        subdirs += 1;
                    }
                }
            }
        }
        2 + subdirs
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
    inner: Arc<Shared>,
    path: String,
    /// The contents, when this handle is onto a file. Held directly, so the
    /// descriptor keeps working after the name is unlinked or renamed.
    file: Option<Arc<FileNode>>,
    /// The inode, when this handle is onto a directory: what it follows
    /// through a rename of the directory or of one above it, as a Linux
    /// descriptor follows its dentry (`Inner::dir_now`).
    dir: Option<u64>,
    /// Whether this handle wrote or truncated: its close is then a change
    /// finished ([`saved`]).
    wrote: core::sync::atomic::AtomicBool,
    /// Opened to write: its close is `IN_CLOSE_WRITE`, else `IN_CLOSE_NOWRITE`.
    for_write: bool,
}

impl TmpHandle {
    fn note_write(&self) {
        if !self.wrote.load(Ordering::Relaxed) {
            self.wrote.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for TmpHandle {
    fn drop(&mut self) {
        // By the name it was opened by: a drop takes no lock. A save is
        // closed before it is renamed into place, and the rename is counted.
        if self.wrote.load(Ordering::Relaxed) {
            saved(&self.path);
        }
        if crate::inotify::any() {
            use crate::inotify::mask;
            let ev = if self.for_write { mask::CLOSE_WRITE } else { mask::CLOSE_NOWRITE };
            self.note_here(ev);
        }
    }
}

impl TmpHandle {
    /// `ev` about this handle's node, by the name it has now (to its own
    /// watches only, once it has none).
    fn note_here(&self, ev: u32) {
        if !crate::inotify::any() {
            return;
        }
        let inner = self.inner.read().unwrap();
        let own = match (&self.file, self.dir) {
            (Some(f), _) => Some((f.ino, false)),
            (None, Some(d)) => Some((d, true)),
            _ => None,
        };
        let fs = fs_key(&self.inner);
        match self.here(&inner) {
            Some(path) => note(&inner, fs, &path, own.or_else(|| node_ino(&inner, &path)), ev, 0),
            None => {
                if let Some((ino, isdir)) = own {
                    crate::inotify::event(crate::inotify::Key { fs, ino }, ev, 0, None, isdir);
                }
            }
        }
    }

    /// The name this handle's node has now: a directory's where it has
    /// moved to, a file's one of its names (`None` once removed or
    /// unlinked), anything else's as opened.
    fn here(&self, inner: &Inner) -> Option<String> {
        match (self.dir, &self.file) {
            (Some(ino), _) => inner.dir_now(&self.path, ino),
            (None, Some(f)) => {
                if inner.inos.get(&self.path) == Some(&f.ino) {
                    return Some(self.path.clone());
                }
                inner.inos.iter().find(|(_, i)| **i == f.ino).map(|(p, _)| p.clone())
            }
            (None, None) => Some(self.path.clone()),
        }
    }
}

impl FileHandle for TmpHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let Some(file) = &self.file else {
            return Err(Errno::EISDIR);
        };
        file.mark_read();
        if let Some(l) = file.lazy.lock().unwrap().as_mut() {
            return l.read(offset, buf);
        }
        let data = file.data.lock().unwrap();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n as u64)
    }

    fn changes(&self) -> u64 {
        self.file.as_ref().map_or(0, |f| f.changes.load(Ordering::Acquire))
    }

    fn read_with(&self, offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Option<SysResult> {
        let file = self.file.as_ref()?;
        if file.lazy.lock().unwrap().is_some() {
            return None;
        }
        file.mark_read();
        let data = file.data.lock().unwrap();
        let start = (offset as usize).min(data.len());
        let n = len.min(data.len() - start);
        f(&data[start..start + n]);
        Some(Ok(n as u64))
    }

    fn advise(&self, offset: u64, len: u64, advice: u32) -> Result<(), Errno> {
        let Some(file) = &self.file else { return Ok(()) };
        // The advice that fetches: a range wanted, an image mapped.
        if advice == super::FADV_WILLNEED || advice == super::FADV_GEX_IMAGE {
            file.mark_read();
        }
        match file.lazy.lock().unwrap().as_mut() {
            Some(l) => l.advise(offset, len, advice),
            None => Ok(()),
        }
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
            reserve(&mut data, start + buf.len())?;
            data.resize(start + buf.len(), 0);
            note_capacity(was, data.capacity());
        }
        data[start..start + buf.len()].copy_from_slice(buf);
        file.changed();
        drop(data);
        file.touch();
        self.note_write();
        self.note_here(crate::inotify::mask::MODIFY);
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
        let inner = self.inner.read().unwrap();
        let Some(path) = self.here(&inner) else {
            // A directory removed while open: still a directory, with no
            // names left, as `fstat` of one says on Linux.
            return Ok(Metadata { ino: self.dir.unwrap_or(1), nlink: 0, ..Metadata::dir() });
        };
        TmpFs::meta_at(&inner, &path)
    }

    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let inner = self.inner.read().unwrap();
        // A directory removed while open lists nothing, not even `.`.
        let Some(path) = self.here(&inner) else { return Ok(Vec::new()) };
        if !matches!(inner.nodes.get(&path), Some(Node::Dir)) {
            return Err(Errno::ENOTDIR);
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut out = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: TmpFs::ino_of(&inner, &path),
            },
            DirEntry {
                name: "..".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
        ];
        // The immediate children only, each child's own subtree passed in one
        // seek (as `dir_nlink`). A scan of every node in the filesystem was
        // each listing's cost: a page start with a lazily mounted tree of
        // ~29,000 files spent 43 of 60 profiled seconds here, in
        // Wine's case-insensitive lookups (each a directory read) and its
        // directory scans, with the loader lock held. Otherwise one pass over
        // the names in order, the inode numbers (same keys) walked beside
        // it: a seek from the top per entry, and another for its inode, were
        // 48% of a page directory scan of 2,000 files.
        use std::ops::Bound::{Included, Unbounded};
        let from = |at: String| (Included(at), Unbounded);
        let mut names = inner.nodes.range::<String, _>(from(prefix.clone()));
        let mut inos = inner.inos.range::<String, _>(from(prefix.clone())).peekable();
        while let Some((path, node)) = names.next() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                break;
            };
            if let Some(i) = rest.find('/') {
                // A descendant whose own directory (`rest[..i]`) is listed
                // already, or is missing: past its subtree either way.
                let past = format!("{prefix}{}0", &rest[..i]);
                names = inner.nodes.range::<String, _>(from(past.clone()));
                inos = inner.inos.range::<String, _>(from(past)).peekable();
                continue;
            }
            if rest.is_empty() {
                continue;
            }
            while inos.next_if(|(k, _)| k.as_str() < path.as_str()).is_some() {}
            let ino = match inos.peek() {
                Some((k, v)) if *k == path => **v,
                _ => 1,
            };
            out.push(DirEntry {
                name: rest.to_string(),
                file_type: match node {
                    Node::File(_) => FileType::Regular,
                    Node::Dir => FileType::Directory,
                    Node::Symlink(_) => FileType::Symlink,
                    Node::Socket => FileType::Socket,
                },
                ino,
            });
        }
        Ok(out)
    }

    /// The stamp a write or a truncating open leaves (`utimensat` comes
    /// through `set_times_now`): no event of its own, the change already
    /// made one.
    fn set_mtime(&self, secs: i64) -> Result<(), Errno> {
        match &self.file {
            Some(f) => {
                f.stamp(secs);
                Ok(())
            }
            None => {
                let path = self.here(&self.inner.read().unwrap()).ok_or(Errno::ENOENT)?;
                FileSystem::set_mtime(&TmpFs { inner: Arc::clone(&self.inner) }, &path, secs)
            }
        }
    }

    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.set_times_now(atime, mtime, now())
    }

    fn set_times_now(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>, now: i64) -> Result<(), Errno> {
        match &self.file {
            Some(f) => {
                f.set_times(atime, mtime, now);
                self.note_here(crate::inotify::mask::ATTRIB);
                Ok(())
            }
            None => {
                let path = self.here(&self.inner.read().unwrap()).ok_or(Errno::ENOENT)?;
                TmpFs { inner: Arc::clone(&self.inner) }.set_times_now(&path, atime, mtime, now)
            }
        }
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let Some(file) = &self.file else {
            return Err(Errno::EINVAL);
        };
        file.materialize()?;
        self.note_write();
        {
            let mut data = file.data.lock().unwrap();
            let was = data.capacity();
            reserve(&mut data, len as usize)?;
            data.resize(len as usize, 0);
            note_capacity(was, data.capacity());
            file.changed();
        }
        file.touch();
        self.note_here(crate::inotify::mask::MODIFY);
        Ok(())
    }

    /// By the name it has now, as `chmod` on that name: a mode lives with
    /// the name here, not the node. A file with no name left has no mode to
    /// keep, and the call succeeds as on Linux.
    fn chmod(&self, mode: u32) -> Option<Result<(), Errno>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(path) = self.here(&inner) else {
            return Some(if self.file.is_some() { Ok(()) } else { Err(Errno::ENOENT) });
        };
        if matches!(inner.nodes.get(&path), Some(Node::Symlink(_))) {
            return Some(Err(Errno::EOPNOTSUPP));
        }
        let r = inner.chmod(&path, mode);
        if r.is_ok() {
            note_path(&inner, fs_key(&self.inner), &path, crate::inotify::mask::ATTRIB);
        }
        Some(r)
    }

    fn xattr(&self, op: super::XattrOp<'_>) -> Option<Result<usize, Errno>> {
        let mut inner = self.inner.lock().unwrap();
        // The node itself: a file's inode goes with its contents (an
        // unlinked file keeps none), a directory's as it moved.
        let ino = match (&self.file, self.dir) {
            (Some(f), _) => f.ino,
            (None, Some(d)) => d,
            (None, None) => {
                let path = self.here(&inner)?;
                *inner.inos.get(&path)?
            }
        };
        let changes = !matches!(op, super::XattrOp::Get { .. } | super::XattrOp::List { .. });
        let r = inner.xattr(ino, false, op);
        if changes && r.is_ok() {
            drop(inner);
            self.note_here(crate::inotify::mask::ATTRIB);
        }
        Some(r)
    }

    fn rename_epoch(&self) -> Option<u64> {
        Some(self.inner.gate.epoch())
    }

    fn hold_renames(&self, owner: i32, on: bool) -> bool {
        self.inner.gate.hold(owner, on);
        true
    }

    fn moved(&self) -> Option<Result<super::Moved, Errno>> {
        let ino = self.dir?;
        let inner = self.inner.read().unwrap();
        Some(
            inner
                .dir_now(&self.path, ino)
                .map(|now| super::Moved { was: self.path.clone(), now, epoch: inner.moves.get(&ino).copied().unwrap_or(0) })
                .ok_or(Errno::ENOENT),
        )
    }
}

impl FileSystem for TmpFs {
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let path = path.to_string();
        let mut dir = None;
        let mut wrote = false;
        let mut created = false;
        let file = {
            let mut inner = self.inner.lock().unwrap();
            match inner.nodes.get(&path) {
                Some(node) => {
                    if flags & oflags::O_EXCL != 0 && flags & oflags::O_CREAT != 0 {
                        return Err(Errno::EEXIST);
                    }
                    // A directory opens for reading only.
                    let acc = flags & oflags::ACCMODE;
                    if matches!(node, Node::Dir) && (acc == oflags::O_WRONLY || acc == oflags::O_RDWR) {
                        return Err(Errno::EISDIR);
                    }
                    if matches!(node, Node::Dir) {
                        dir = Some(TmpFs::ino_of(&inner, &path));
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
                                f.changed();
                            }
                            f.touch();
                            wrote = true;
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
                    inner.set_ino(path.clone(), ino);
                    let f = Arc::new(FileNode::new(Vec::new(), ino));
                    f.mark_written();
                    wrote = true;
                    created = true;
                    inner.nodes.insert(path.clone(), Node::File(Arc::clone(&f)));
                    Some(f)
                }
            }
        };
        let acc = flags & oflags::ACCMODE;
        let h = TmpHandle {
            inner: Arc::clone(&self.inner),
            path,
            file,
            dir,
            wrote: core::sync::atomic::AtomicBool::new(wrote),
            for_write: acc == oflags::O_WRONLY || acc == oflags::O_RDWR,
        };
        if crate::inotify::any() {
            use crate::inotify::mask;
            if created {
                h.note_here(mask::CREATE);
            } else if wrote {
                h.note_here(mask::MODIFY);
            }
            h.note_here(mask::OPEN);
        }
        Ok(Arc::new(h))
    }

    fn watch_key(&self, path: &str) -> Result<(Option<crate::inotify::Key>, bool), Errno> {
        let inner = self.inner.read().unwrap();
        let (ino, isdir) = node_ino(&inner, path).ok_or(Errno::ENOENT)?;
        Ok((Some(crate::inotify::Key { fs: fs_key(&self.inner), ino }), isdir))
    }

    /// From the tree, under one lock: no handle made (with its own copy of
    /// the path) for one question.
    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        TmpFs::meta_at(&self.inner.read().unwrap(), path)
    }

    fn xattr(&self, path: &str, op: super::XattrOp<'_>) -> Result<usize, Errno> {
        let mut inner = self.inner.lock().unwrap();
        let symlink = match inner.nodes.get(path) {
            None => return Err(Errno::ENOENT),
            Some(n) => matches!(n, Node::Symlink(_)),
        };
        let ino = TmpFs::ino_of(&inner, path);
        let changes = !matches!(op, super::XattrOp::Get { .. } | super::XattrOp::List { .. });
        let r = inner.xattr(ino, symlink, op);
        if changes && r.is_ok() {
            note_path(&inner, fs_key(&self.inner), path, crate::inotify::mask::ATTRIB);
        }
        r
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        inner.chmod(path, mode)?;
        note_path(&inner, fs_key(&self.inner), path, crate::inotify::mask::ATTRIB);
        Ok(())
    }

    /// A name here is its node, link or not: `metadata` never follows.
    fn metadata_nofollow(&self, path: &str) -> Result<Metadata, Errno> {
        self.metadata(path)
    }

    fn set_mtime(&self, path: &str, secs: i64) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            None => return Err(Errno::ENOENT),
            Some(Node::File(f)) => f.stamp(secs),
            Some(_) => {
                inner.mtimes.insert(path.to_string(), secs);
            }
        }
        note_path(&inner, fs_key(&self.inner), path, crate::inotify::mask::ATTRIB);
        Ok(())
    }

    fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.set_times_now(path, atime, mtime, now())
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let inner = self.inner.read().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Symlink(t)) => Ok(t.clone()),
            Some(_) => Err(Errno::EINVAL),
            // A name that is not there is ENOENT (or ENOTDIR), not EINVAL:
            // `realpath` takes EINVAL for "there, and not a link" and
            // resolves a file that does not exist.
            None => {
                let mut at = path;
                while let Some(i) = at.rfind('/') {
                    at = &at[..i];
                    match inner.nodes.get(at) {
                        Some(Node::Dir) => break,
                        Some(_) => return Err(Errno::ENOTDIR),
                        None => {}
                    }
                }
                Err(Errno::ENOENT)
            }
        }
    }

    /// The walk and the metadata under one lock.
    fn walk_metadata(&self, abs: &str, mounts: &str) -> Result<Option<Metadata>, Errno> {
        let inner = self.inner.read().unwrap();
        let is_link = |rel: &str| matches!(inner.nodes.get(rel), Some(Node::Symlink(_)));
        // The directories without a link (or a mount) on them: the last
        // name's node is then looked up once, for its link and its metadata.
        match super::walk_plain(abs, false, mounts, &is_link) {
            Some(true) => return Ok(None),
            Some(false) if !super::within_any(abs, mounts) => {
                let rel = abs.trim_start_matches('/');
                match inner.nodes.get(rel) {
                    None => return Err(Errno::ENOENT),
                    Some(Node::Symlink(_)) => {}
                    Some(node) => return TmpFs::meta_of(&inner, rel, node).map(Some),
                }
            }
            _ => {}
        }
        let w = super::walk_links(abs.to_string(), true, mounts, &|rel| match inner.nodes.get(rel) {
            Some(Node::Symlink(t)) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        })?;
        if w.at_mount {
            return Ok(None);
        }
        TmpFs::meta_at(&inner, w.path.trim_start_matches('/')).map(Some)
    }

    /// One lock for the whole walk, not one per component.
    fn walk(&self, abs: &str, follow_final: bool, mounts: &str) -> Result<super::Walked, Errno> {
        let inner = self.inner.read().unwrap();
        super::walk_links(abs.to_string(), follow_final, mounts, &|rel| match inner.nodes.get(rel) {
            Some(Node::Symlink(t)) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        })
    }

    fn unlink(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Dir) => Err(Errno::EISDIR),
            Some(_) => {
                let own = node_ino(&inner, path);
                inner.nodes.remove(path);
                inner.drop_ino(path);
                inner.mtimes.remove(path);
                saved(path);
                if let Some(own) = own {
                    note_removed(&inner, fs_key(&self.inner), path, own);
                }
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
        inner.set_ino(path.to_string(), ino);
        inner.nodes.insert(path.to_string(), Node::Dir);
        note(&inner, fs_key(&self.inner), path, Some((ino, true)), crate::inotify::mask::CREATE, 0);
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
        let own = node_ino(&inner, path);
        inner.nodes.remove(path);
        inner.drop_ino(path);
        inner.modes.remove(path);
        if let Some(own) = own {
            note_removed(&inner, fs_key(&self.inner), path, own);
        }
        // A directory gone is a directory whose path changed, for whoever
        // holds it ([`FileHandle::rename_epoch`]).
        self.inner.gate.bump();
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.rename2(from, to, false)
    }

    /// `rename`, under the one lock, so `RENAME_NOREPLACE`'s check and the
    /// move are one step. A directory moves with everything under it, as
    /// on Linux (the map is of whole paths, so each name below is moved).
    /// Linux's refusals: a directory into itself (`EINVAL`), over a file
    /// (`ENOTDIR`) or over a directory that is not empty (`ENOTEMPTY`); a
    /// file over a directory (`EISDIR`); a destination whose directory is
    /// not there (`ENOENT`).
    fn rename2(&self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        self.rename2_as(from, to, noreplace, super::own_holds())
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
        inner.set_ino(to.to_string(), ino);
        if let Some(m) = mode {
            inner.modes.insert(to.to_string(), m);
        }
        if crate::inotify::any() {
            let fs = fs_key(&self.inner);
            crate::inotify::event(crate::inotify::Key { fs, ino }, crate::inotify::mask::ATTRIB, 0, None, false);
            note(&inner, fs, to, None, crate::inotify::mask::CREATE, 0);
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
        inner.set_ino(path.to_string(), ino);
        inner.nodes.insert(path.to_string(), Node::Socket);
        note(&inner, fs_key(&self.inner), path, Some((ino, false)), crate::inotify::mask::CREATE, 0);
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(path) {
            return Err(Errno::EEXIST);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.set_ino(path.to_string(), ino);
        inner
            .nodes
            .insert(path.to_string(), Node::Symlink(target.to_string()));
        note(&inner, fs_key(&self.inner), path, Some((ino, false)), crate::inotify::mask::CREATE, 0);
        Ok(())
    }
}

/// Room for a file's contents to reach `len` bytes, or ENOSPC: a tmpfs file
/// lives in the kernel's memory, and a `resize` the allocator cannot meet is an
/// abort there, with the file's lock (and every process's kernel) held for good.
/// The usual doubling first, then exactly `len` when doubling is what failed.
fn reserve(data: &mut Vec<u8>, len: usize) -> Result<(), Errno> {
    let more = len.saturating_sub(data.len());
    if more == 0 || data.capacity() >= len {
        return Ok(());
    }
    data.try_reserve(more).or_else(|_| data.try_reserve_exact(more)).map_err(|_| Errno::ENOSPC)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lazy file's time does not come from the clock: two loads of the same
    /// site, a second apart, see the same time for it, and a recorded time
    /// replaces it.
    #[test]
    fn a_change_of_the_contents_moves_the_count_and_a_read_does_not() {
        use crate::fs::oflags::{O_CREAT, O_RDWR, O_TRUNC};
        let fs = TmpFs::new();
        let h = fs.open("f", O_RDWR | O_CREAT, 0o644).unwrap();
        let c0 = h.changes();
        assert_ne!(c0, 0);
        h.write_at(0, b"abc").unwrap();
        let c1 = h.changes();
        assert!(c1 > c0);
        let mut b = [0u8; 3];
        h.read_at(0, &mut b).unwrap();
        h.set_mtime(1_700_000_000).unwrap();
        assert_eq!(h.changes(), c1, "a read or a time set is no change");
        h.truncate(1).unwrap();
        let c2 = h.changes();
        assert!(c2 > c1);
        let again = fs.open("f", O_RDWR | O_TRUNC, 0o644).unwrap();
        assert!(again.changes() > c2, "O_TRUNC is a change, seen through every handle");
        assert_eq!(h.changes(), again.changes());
    }

    #[test]
    fn a_lazy_file_has_the_same_time_on_every_load() {
        let load = || {
            let fs = TmpFs::new();
            fs.add_lazy("lib/a.dll", 4096, "/lazy/h/a");
            fs
        };
        let a = load();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = load();
        let t = |fs: &TmpFs| FileSystem::metadata(fs, "lib/a.dll").unwrap().mtime;
        assert_eq!(t(&a), LAZY_MTIME);
        assert_eq!(t(&a), t(&b));
        FileSystem::set_mtime(&b, "lib/a.dll", 1_700_000_000).unwrap();
        assert_eq!(t(&b), 1_700_000_000);
    }

    /// A listing names a directory's immediate children, and only them:
    /// names sorting around `/` ("a-b", "a.txt" between "a" and "a/x"),
    /// a child's subtree skipped, the root, and a deep tree under a sibling.
    #[test]
    fn a_listing_names_the_immediate_children() {
        let fs = TmpFs::new();
        for d in ["a", "a/x", "a/x/y", "b", "b/a", "b/a0"] {
            fs.mkdir(d, 0o755).unwrap();
        }
        for f in ["a-b", "a.txt", "a/x/y/f", "a/z", "a0", "b/a/q", "b/a-", "c"] {
            fs.add_file(f, Vec::new());
        }
        for i in 0..2000 {
            fs.add_file(&format!("a/x/y/n{i}"), Vec::new());
        }
        let list = |d: &str| {
            let mut v: Vec<String> = fs.open(d, 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
            v.sort();
            v
        };
        assert_eq!(list(""), [".", "..", "a", "a-b", "a.txt", "a0", "b", "c"]);
        assert_eq!(list("a"), [".", "..", "x", "z"]);
        assert_eq!(list("b"), [".", "..", "a", "a-", "a0"]);
        assert_eq!(list("b/a"), [".", "..", "q"]);
        assert_eq!(list("a/x/y").len(), 2 + 2001);
    }

    /// Every directory's listing, against a scan of the whole filesystem
    /// (what `read_dir` did before it skipped subtrees).
    fn check_listings(fs: &TmpFs) {
        let (dirs, all): (Vec<String>, Vec<String>) = {
            let inner = fs.inner.read().unwrap();
            let dirs = inner.nodes.iter().filter(|(_, n)| matches!(n, Node::Dir)).map(|(p, _)| p.clone());
            (std::iter::once(String::new()).chain(dirs).collect(), inner.nodes.keys().cloned().collect())
        };
        for d in dirs {
            let prefix = if d.is_empty() { String::new() } else { format!("{d}/") };
            let mut want: Vec<String> = all
                .iter()
                .filter_map(|p| p.strip_prefix(&prefix))
                .filter(|r| !r.is_empty() && !r.contains('/'))
                .map(String::from)
                .chain([".".into(), "..".into()])
                .collect();
            want.sort();
            want.dedup();
            let mut got: Vec<String> = fs.open(&d, 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
            got.sort();
            assert_eq!(got, want, "listing of {d:?}");
        }
    }

    /// Listings after renames across directories (a file, a directory with
    /// its subtree, onto names that sort next to the old ones), and a deep
    /// tree: each directory lists its immediate children, as a scan of
    /// everything says.
    #[test]
    fn listings_follow_renames_across_directories() {
        let fs = TmpFs::new();
        // A deep tree, with siblings at every level sorting around '/'.
        let mut d = String::from("deep");
        fs.mkdir(&d, 0o755).unwrap();
        for i in 0..40 {
            fs.add_file(&format!("{d}-{i}"), Vec::new());
            fs.add_file(&format!("{d}.{i}"), Vec::new());
            fs.add_file(&format!("{d}/f{i}"), Vec::new());
            d = format!("{d}/d{i}");
            fs.mkdir(&d, 0o755).unwrap();
        }
        for x in ["a", "a/b", "a/b/c", "x", "x/y"] {
            fs.mkdir(x, 0o755).unwrap();
        }
        for f in ["a/b/c/f", "a/b/g", "a/b-", "a/b0", "x/y/h"] {
            fs.add_file(f, Vec::new());
        }
        check_listings(&fs);
        fs.rename("a/b/g", "x/y/g").unwrap(); // a file, across directories
        check_listings(&fs);
        fs.rename("a/b", "x/y/b").unwrap(); // a directory and its subtree
        assert_eq!(fs.metadata("x/y/b/c/f").unwrap().file_type, FileType::Regular);
        check_listings(&fs);
        fs.rename("x/y/b", "a/b.").unwrap(); // back, to a name sorting before '/'
        check_listings(&fs);
        fs.rename("deep/d0/d1", "a/b./d1").unwrap(); // 38 levels deep, moved up
        check_listings(&fs);
        fs.rename("a", "deep/d0/a").unwrap(); // and under where it came from
        check_listings(&fs);
        let list = |p: &str| fs.open(p, 0, 0).unwrap().read_dir().unwrap().len();
        assert_eq!(list("deep/d0/a/b./d1"), 2 + 4); // f2, d2, d2-3, d2.3
        assert_eq!(list("deep/d0"), 2 + 4); // f1, d1-2, d1.2, a
    }

    /// Unlinks while a directory is open and being listed: each later
    /// listing of the same handle has what is left, and an entry that came
    /// back (created again) is listed once.
    #[test]
    fn unlinks_during_a_listing() {
        let fs = TmpFs::new();
        fs.mkdir("d", 0o755).unwrap();
        fs.mkdir("d/sub", 0o755).unwrap();
        for i in 0..300 {
            fs.add_file(&format!("d/f{i:03}"), Vec::new());
            fs.add_file(&format!("d/sub/g{i}"), Vec::new());
        }
        let h = fs.open("d", 0, 0).unwrap();
        let names = |h: &Arc<dyn FileHandle>| -> Vec<String> { h.read_dir().unwrap().into_iter().map(|e| e.name).collect() };
        let first = names(&h);
        assert_eq!(first.len(), 2 + 301);
        // Remove every other entry the listing returned, as `rm -r` does
        // between reads, then list again through the same descriptor.
        for (i, n) in first.iter().enumerate().filter(|(_, n)| n.starts_with('f')) {
            if i % 2 == 0 {
                fs.unlink(&format!("d/{n}")).unwrap();
            }
        }
        let second = names(&h);
        assert_eq!(second.len(), 2 + 1 + 150);
        check_listings(&fs);
        // The subdirectory emptied and removed mid-listing; a name re-created.
        for i in 0..300 {
            fs.unlink(&format!("d/sub/g{i}")).unwrap();
        }
        fs.rmdir("d/sub").unwrap();
        fs.add_file("d/f000", Vec::new());
        let third = names(&h);
        assert_eq!(third.iter().filter(|n| *n == "f000").count(), 1);
        assert!(!third.iter().any(|n| n == "sub"));
        check_listings(&fs);
    }

    /// A directory renamed takes everything under it, with its inodes,
    /// modes and times; Linux's refusals, and `RENAME_NOREPLACE`.
    #[test]
    fn a_renamed_directory_takes_its_subtree() {
        let fs = TmpFs::new();
        fs.mkdir("a", 0o755).unwrap();
        fs.mkdir("a/b", 0o700).unwrap();
        fs.add_file("a/b/f", b"hello".to_vec());
        fs.symlink("f", "a/b/l").unwrap();
        let ino = fs.metadata("a/b/f").unwrap().ino;
        fs.rename("a", "c").unwrap();
        assert_eq!(fs.metadata("a/b/f").err(), Some(Errno::ENOENT));
        assert_eq!(fs.metadata("a").err(), Some(Errno::ENOENT));
        assert_eq!(fs.metadata("c/b/f").unwrap().ino, ino);
        assert_eq!(fs.metadata("c/b").unwrap().mode, 0o700);
        assert_eq!(fs.read_link("c/b/l").unwrap(), "f");
        let mut buf = [0u8; 5];
        fs.open("c/b/f", 0, 0).unwrap().read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(fs.open("c/b", 0, 0).unwrap().read_dir().unwrap().len(), 4);
        // Not into itself; not over a file or a full directory; a file not
        // over a directory; not into a directory that is not there.
        assert_eq!(fs.rename("c", "c/b/x"), Err(Errno::EINVAL));
        assert_eq!(fs.rename("c", "c/b/f"), Err(Errno::EINVAL));
        fs.add_file("g", vec![]);
        fs.mkdir("e", 0o755).unwrap();
        fs.mkdir("full", 0o755).unwrap();
        fs.add_file("full/x", vec![]);
        assert_eq!(fs.rename("c", "g"), Err(Errno::ENOTDIR));
        assert_eq!(fs.rename("c", "full"), Err(Errno::ENOTEMPTY));
        assert_eq!(fs.rename("g", "e"), Err(Errno::EISDIR));
        assert_eq!(fs.rename("g", "nowhere/g"), Err(Errno::ENOENT));
        assert_eq!(fs.rename2("c", "e", true), Err(Errno::EEXIST));
        // Over an empty directory.
        fs.rename("c", "e").unwrap();
        assert_eq!(fs.metadata("e/b/f").unwrap().ino, ino);
        // A name with the directory's as a prefix is not under it.
        fs.mkdir("e2", 0o755).unwrap();
        fs.rename("e", "z").unwrap();
        assert!(fs.metadata("e2").is_ok());
        assert!(fs.metadata("z/b/f").is_ok());
    }

    /// An open directory follows its renames, and those of a directory
    /// above it, as a Linux descriptor does; once removed it is nowhere.
    #[test]
    fn an_open_directory_follows_its_renames() {
        let fs = TmpFs::new();
        fs.mkdir("a", 0o755).unwrap();
        fs.mkdir("a/b", 0o755).unwrap();
        fs.add_file("a/b/f", vec![]);
        let h = fs.open("a/b", 0, 0).unwrap();
        let at = |now: &str, epoch| Some(Ok(super::super::Moved { was: "a/b".into(), now: now.into(), epoch }));
        assert_eq!(h.moved(), at("a/b", 0));
        fs.rename("a", "c").unwrap();
        assert_eq!(h.moved(), at("c/b", 1));
        assert!(h.read_dir().unwrap().iter().any(|e| e.name == "f"));
        fs.rename("c/b", "d").unwrap();
        assert_eq!(h.moved(), at("d", 2));
        assert_eq!(h.metadata().unwrap().file_type, FileType::Directory);
        fs.unlink("d/f").unwrap();
        fs.rmdir("d").unwrap();
        assert_eq!(h.moved(), Some(Err(Errno::ENOENT)));
        assert!(h.read_dir().unwrap().is_empty());
        // A file's handle has its node, not a directory to follow.
        fs.add_file("g", vec![]);
        assert_eq!(fs.open("g", 0, 0).unwrap().moved(), None);
    }

    /// A lookup's hold keeps directories where they are until it lets go;
    /// files still move, and the holder's own thread is not held off.
    #[test]
    fn a_hold_keeps_directory_renames_off() {
        let fs = Arc::new(TmpFs::new());
        fs.mkdir("a", 0o755).unwrap();
        fs.add_file("f", vec![]);
        let h = fs.open("a", 0, 0).unwrap();
        assert!(h.hold_renames(7, true));
        crate::vfs::note_own_hold(true);
        fs.rename("f", "g").unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let t = {
            let (fs, done) = (Arc::clone(&fs), Arc::clone(&done));
            std::thread::spawn(move || {
                fs.rename("a", "b").unwrap();
                done.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!done.load(Ordering::SeqCst), "a directory moved under a hold");
        assert_eq!(h.moved().unwrap().unwrap().now, "a");
        // The holder's own renames go through.
        fs.mkdir("c", 0o755).unwrap();
        fs.rename("c", "d").unwrap();
        h.hold_renames(7, false);
        crate::vfs::note_own_hold(false);
        t.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(h.moved().unwrap().unwrap().now, "b");
    }

    #[test]
    fn link_counts_follow_links_unlinks_and_renames() {
        let fs = TmpFs::new();
        fs.add_dir("d");
        fs.add_dir("d/a");
        fs.add_dir("d/a-b");
        fs.add_dir("d/a/x");
        fs.add_dir("d/a-b/y");
        fs.add_file("d/f", b"1".to_vec());
        fs.add_file("d/a/x/g", b"2".to_vec());
        assert_eq!(fs.metadata("d").unwrap().nlink, 4);
        assert_eq!(fs.metadata("d/a").unwrap().nlink, 3);
        assert_eq!(fs.metadata("").unwrap().nlink, 3);
        assert_eq!(fs.metadata("d/f").unwrap().nlink, 1);
        fs.link("d/f", "d/h").unwrap();
        assert_eq!(fs.metadata("d/f").unwrap().nlink, 2);
        fs.rename("d/h", "d/a/x/g").unwrap();
        assert_eq!(fs.metadata("d/f").unwrap().nlink, 2);
        fs.unlink("d/a/x/g").unwrap();
        assert_eq!(fs.metadata("d/f").unwrap().nlink, 1);
        fs.rmdir("d/a-b/y").unwrap();
        assert_eq!(fs.metadata("d/a-b").unwrap().nlink, 2);
    }

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
    fn read_link_tells_a_missing_name_from_one_that_is_not_a_link() {
        // glibc's `realpath` reads the difference: EINVAL is "there, and not
        // a link", so a missing name answered with it came back resolved,
        // and KService took every mimeapps.list it looked for as present.
        let fs = TmpFs::new();
        fs.add_file("f", b"x".to_vec());
        fs.symlink("f", "l").unwrap();
        assert_eq!(fs.read_link("l"), Ok("f".to_string()));
        assert_eq!(fs.read_link("f"), Err(Errno::EINVAL));
        assert_eq!(fs.read_link(""), Err(Errno::EINVAL));
        assert_eq!(fs.read_link("nothing-here"), Err(Errno::ENOENT));
        assert_eq!(fs.read_link("no-dir/x"), Err(Errno::ENOENT));
        assert_eq!(fs.read_link("f/x"), Err(Errno::ENOTDIR));
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

    /// `/data/pe`: [`PE`]'s bytes, each request noted in [`ASKED`]. Byte `i`
    /// of every other lazy file here is `i % 251`.
    fn fake(url: &str, offset: u64, into: &mut [u8]) -> bool {
        if url == "/data/r" {
            return counting(url, offset, into);
        }
        if url == "/data/h" {
            HEAD_ASKED.lock().unwrap().push((offset, into.len() as u64));
            return counting(url, offset, into);
        }
        if url == "/data/t" {
            TAIL_ASKED.lock().unwrap().push((offset, into.len() as u64));
            return counting(url, offset, into);
        }
        if url == "/data/q" {
            *FETCHES_Q.lock().unwrap() += 1;
        }
        if url == "/data/s" {
            *FETCHES_S.lock().unwrap() += 1;
        }
        if url == "/data/pe" {
            let img = PE.get().unwrap();
            ASKED.lock().unwrap().push((offset, into.len() as u64));
            into.copy_from_slice(&img[offset as usize..offset as usize + into.len()]);
            return true;
        }
        assert!(url == "/data/f" || url == "/data/q" || url == "/data/s", "{url}");
        for (k, b) in into.iter_mut().enumerate() {
            *b = ((offset + k as u64) % 251) as u8;
        }
        true
    }
    static PE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static ASKED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

    /// The image advice fetches the chunks the server's reads touch and no
    /// more (no readahead), and the server's reads after it fetch nothing:
    /// what a door call then reads is here.
    #[test]
    fn image_advice_fetches_what_the_server_reads() {
        set_lazy_fetch(fake);
        let size = 20 * LAZY_CHUNK as usize + 77;
        // .rsrc at 4 MiB, the version data 14 MiB into it (18 MiB)
        let _ = PE.set(super::super::peimage::tests::image(size, 4 << 20, 14 << 20));
        let fs = TmpFs::new();
        fs.add_lazy("/d/pe", size as u64, "/data/pe");
        let h = fs.open("d/pe", 0, 0).unwrap();
        h.advise(0, 0, super::super::FADV_GEX_IMAGE).unwrap();
        let mut chunks: Vec<u64> = ASKED.lock().unwrap().iter().flat_map(|&(o, n)| o / LAZY_CHUNK..(o + n).div_ceil(LAZY_CHUNK)).collect();
        chunks.sort();
        assert_eq!(chunks, vec![0, 4, 18]);
        let before = ASKED.lock().unwrap().len();
        // the server's reads, as get_image_params makes them
        super::super::peimage::server_reads(&mut |at, n| {
            let mut b = vec![0u8; n];
            let got = h.read_at(at, &mut b).ok()?;
            b.truncate(got as usize);
            Some(b)
        });
        assert_eq!(ASKED.lock().unwrap().len(), before, "a read after the advice fetched");
        // WILLNEED: the range's chunks, no readahead
        h.advise(10 * LAZY_CHUNK + 5, 10, super::super::FADV_WILLNEED).unwrap();
        assert_eq!(*ASKED.lock().unwrap().last().unwrap(), (10 * LAZY_CHUNK, LAZY_CHUNK));
    }

    #[test]
    fn read_set_lists_the_files_whose_bytes_were_used() {
        set_lazy_fetch(fake);
        let fs = TmpFs::new();
        for f in ["/a/read", "/a/stat", "/a/pread", "/a/with", "/a/mapped", "/a/wrote", "/a/unread"] {
            fs.add_file(f, b"xyz".to_vec());
        }
        fs.add_lazy("/a/lazy", 5, "/data/f");
        fs.add_lazy("/a/lazystat", 5, "/data/f");
        fs.add_dir("/a/dir");
        assert!(fs.read_set().is_empty());
        // An open and an fstat, of a file and of a lazy one: no bytes used.
        for f in ["a/stat", "a/lazystat", "a/dir"] {
            fs.open(f, oflags::O_RDONLY, 0).unwrap().metadata().unwrap();
        }
        FileSystem::metadata(&fs, "a/unread").unwrap();
        assert!(fs.open("a/missing", 0, 0).is_err());
        assert!(fs.read_set().is_empty(), "{:?}", fs.read_set());
        // Then an open and a read marks it, and every other use of the bytes.
        let mut buf = [0u8; 2];
        fs.open("a/read", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        fs.open("a/pread", oflags::O_RDONLY, 0).unwrap().read_at(1, &mut buf).unwrap();
        fs.open("a/lazy", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        fs.open("a/with", oflags::O_RDONLY, 0).unwrap().read_with(0, 3, &mut |_| ()).unwrap().unwrap();
        fs.open("a/mapped", oflags::O_RDONLY, 0).unwrap().advise(0, 0, super::super::FADV_GEX_IMAGE).unwrap();
        fs.open("a/wrote", oflags::O_RDWR, 0).unwrap().write_at(1, b"q").unwrap();
        assert_eq!(
            fs.read_set(),
            ["/a/read", "/a/pread", "/a/lazy", "/a/with", "/a/mapped", "/a/wrote"].map(String::from).to_vec(),
            "in the order of first use"
        );
        fs.open("a/read", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        assert_eq!(fs.read_set()[0], "/a/read", "a second use does not move it");
    }

    /// `written` lists what the program made or changed -- a create, a
    /// write, a truncation, a rename, a directory's files moved with it --
    /// and not the seeded files, a read (a lazy one's contents arriving
    /// included), or a time set: what a page's persist store snapshots.
    #[test]
    fn written_lists_what_the_program_changed() {
        // The module's one fetcher (the first set wins, and the tests run in
        // parallel), and a URL it serves.
        set_lazy_fetch(fake);
        let fs = TmpFs::new();
        for f in ["a/kept", "a/wrote", "a/cut", "a/moved", "a/timed", "d/in"] {
            fs.add_file(f, b"seed".to_vec());
        }
        fs.add_lazy("/a/lazy", 5, "/data/f");
        assert!(fs.written().is_empty());
        let mut buf = [0u8; 4];
        fs.open("a/kept", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        fs.open("a/lazy", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        FileSystem::set_mtime(&fs, "a/timed", 1).unwrap();
        assert!(fs.written().is_empty(), "{:?}", fs.written());
        fs.open("a/wrote", oflags::O_RDWR, 0).unwrap().write_at(0, b"x").unwrap();
        fs.open("a/cut", oflags::O_RDWR, 0).unwrap().truncate(1).unwrap();
        fs.open("a/new", oflags::O_CREAT | oflags::O_RDWR, 0o644).unwrap();
        FileSystem::rename(&fs, "a/moved", "a/there").unwrap();
        FileSystem::rename(&fs, "d", "e").unwrap();
        assert_eq!(
            fs.written(),
            ["/a/cut", "/a/new", "/a/there", "/a/wrote", "/e/in"].map(String::from).to_vec()
        );
    }

    /// A file seeded in parts (`add_file`, then `add_file_part`) holds its
    /// bytes and is not listed as written: only the program's changes are.
    #[test]
    fn a_file_seeded_in_parts_is_not_written() {
        let fs = TmpFs::new();
        fs.add_file("p/big", b"ab".to_vec());
        fs.add_file_part("/p/big", 2, b"cd").unwrap();
        fs.add_file_part("p/big", 6, b"ef").unwrap();
        assert!(fs.written().is_empty(), "{:?}", fs.written());
        let mut buf = [0u8; 8];
        assert_eq!(fs.open("p/big", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf, b"abcd\0\0ef");
        assert_eq!(fs.add_file_part("p/none", 0, b"x"), Err(Errno::ENOENT));
        fs.open("p/big", oflags::O_RDWR, 0).unwrap().write_at(0, b"x").unwrap();
        assert_eq!(fs.written(), ["/p/big"].map(String::from).to_vec());
    }

    /// A change the program finishes under the watched directory moves
    /// `SAVES`: a close after a write, a create or a truncation, a rename
    /// into or out of it, an unlink. A read, an open alone, and anything
    /// outside it do not. (The only test that sets the watch.)
    #[test]
    fn saves_count_finished_changes_under_the_watched_directory() {
        watch("/watched-saves/");
        let fs = TmpFs::new();
        fs.mkdir("watched-saves", 0o755).unwrap();
        fs.mkdir("watched-savesx", 0o755).unwrap();
        fs.add_file("watched-saves/old", b"seed".to_vec());
        fs.add_file("other", b"seed".to_vec());
        let n = || SAVES.load(Ordering::Relaxed);
        let mut buf = [0u8; 4];
        let at = n();
        fs.open("watched-saves/old", oflags::O_RDONLY, 0).unwrap().read_at(0, &mut buf).unwrap();
        drop(fs.open("watched-saves/old", oflags::O_RDWR, 0).unwrap());
        fs.open("other", oflags::O_RDWR, 0).unwrap().write_at(0, b"x").unwrap();
        fs.open("watched-savesx/f", oflags::O_CREAT | oflags::O_RDWR, 0o644).unwrap().write_at(0, b"x").unwrap();
        assert_eq!(n(), at);
        let h = fs.open("watched-saves/old", oflags::O_RDWR, 0).unwrap();
        h.write_at(0, b"x").unwrap();
        assert_eq!(n(), at, "counted at the close, not the write");
        drop(h);
        assert_eq!(n(), at + 1);
        drop(fs.open("watched-saves/tmp", oflags::O_CREAT | oflags::O_WRONLY, 0o644).unwrap());
        assert_eq!(n(), at + 2);
        drop(fs.open("watched-saves/old", oflags::O_WRONLY | oflags::O_TRUNC, 0).unwrap());
        assert_eq!(n(), at + 3);
        FileSystem::rename(&fs, "other", "watched-saves/in").unwrap();
        assert_eq!(n(), at + 4);
        FileSystem::rename(&fs, "watched-saves/tmp", "outside").unwrap();
        assert_eq!(n(), at + 5);
        FileSystem::unlink(&fs, "watched-saves/in").unwrap();
        assert_eq!(n(), at + 6);
        FileSystem::unlink(&fs, "outside").unwrap();
        assert_eq!(n(), at + 6);
    }

    /// Requests of `/data/h`, each `(offset, len)`.
    static HEAD_ASKED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
    /// Requests of `/data/t`, each `(offset, len)`.
    static TAIL_ASKED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

    /// A bundled copy that is a lazy file's head, whole chunks long: the
    /// file is the lazy one at its full size, a read inside the head fetches
    /// nothing, and one past it fetches from the head's end. A copy of any
    /// other length is left as it is.
    #[test]
    fn a_bundled_head_seeds_a_lazy_file() {
        set_lazy_fetch(fake);
        let size = 5 * LAZY_CHUNK + 7;
        let head: Vec<u8> = (0..2 * LAZY_CHUNK).map(|i| (i % 251) as u8).collect();
        let fs = TmpFs::new();
        fs.add_file("/m/bgm", head.clone());
        fs.add_lazy("/m/bgm", size, "/data/h");
        fs.add_file("/m/odd", vec![1; 1000]);
        fs.add_lazy("/m/odd", size, "/data/h");
        assert_eq!(FileSystem::metadata(&fs, "m/bgm").unwrap().size, size);
        assert_eq!(FileSystem::metadata(&fs, "m/odd").unwrap().size, 1000, "not a whole chunk: the copy wins");
        let h = fs.open("m/bgm", oflags::O_RDONLY, 0).unwrap();
        let mut b = vec![0u8; 4096];
        h.read_at(LAZY_CHUNK + 100, &mut b).unwrap();
        assert_eq!(b[..], head[(LAZY_CHUNK + 100) as usize..(LAZY_CHUNK + 100) as usize + 4096]);
        assert!(HEAD_ASKED.lock().unwrap().is_empty(), "a read in the head fetched");
        h.read_at(2 * LAZY_CHUNK - 10, &mut b).unwrap();
        assert_eq!(HEAD_ASKED.lock().unwrap().first().map(|r| r.0), Some(2 * LAZY_CHUNK));
        assert!(b.iter().enumerate().all(|(k, &x)| x == ((2 * LAZY_CHUNK - 10 + k as u64) % 251) as u8));
        let mut tail = vec![0u8; 64];
        assert_eq!(h.read_at(size - 7, &mut tail).unwrap(), 7);
    }

    /// A copy of a whole chunk and then the file's last, partial one is its
    /// head and tail: reads in either fetch nothing, a read between does.
    #[test]
    fn a_bundled_head_and_tail_seed_a_lazy_file() {
        set_lazy_fetch(fake);
        let size = 5 * LAZY_CHUNK + 7;
        let byte = |i: u64| (i % 251) as u8;
        let mut copy: Vec<u8> = (0..LAZY_CHUNK).map(byte).collect();
        copy.extend((5 * LAZY_CHUNK..size).map(byte));
        let fs = TmpFs::new();
        fs.add_file("/m/arc", copy);
        fs.add_lazy("/m/arc", size, "/data/t");
        assert_eq!(FileSystem::metadata(&fs, "m/arc").unwrap().size, size);
        let h = fs.open("m/arc", oflags::O_RDONLY, 0).unwrap();
        let mut b = vec![0u8; 7];
        assert_eq!(h.read_at(size - 7, &mut b).unwrap(), 7);
        assert!(b.iter().enumerate().all(|(k, &x)| x == byte(size - 7 + k as u64)));
        h.read_at(100, &mut b).unwrap();
        assert!(b.iter().enumerate().all(|(k, &x)| x == byte(100 + k as u64)));
        assert!(TAIL_ASKED.lock().unwrap().is_empty(), "a read in the head or tail fetched");
        h.read_at(3 * LAZY_CHUNK, &mut b).unwrap();
        assert_eq!(TAIL_ASKED.lock().unwrap().first().map(|r| r.0), Some(3 * LAZY_CHUNK));
        assert!(b.iter().enumerate().all(|(k, &x)| x == byte(3 * LAZY_CHUNK + k as u64)));
    }


    /// A seeded head's chunks stay past the budget: the file's URL does
    /// not carry them.
    #[test]
    fn a_seeded_head_is_never_evicted() {
        set_lazy_fetch(fake);
        let max = LAZY_HELD_MAX / LAZY_CHUNK;
        let size = (max + 10) * LAZY_CHUNK;
        let fs = TmpFs::new();
        fs.add_file("/m/big", (0..2 * LAZY_CHUNK).map(|i| (i % 251) as u8).collect());
        fs.add_lazy("/m/big", size, "/data/s");
        let h = fs.open("m/big", oflags::O_RDONLY, 0).unwrap();
        let mut b = vec![0u8; 16];
        for k in (2..max + 10).step_by(2) {
            h.read_at(k * LAZY_CHUNK, &mut b).unwrap();
        }
        let was = *FETCHES_S.lock().unwrap();
        h.read_at(100, &mut b).unwrap();
        h.read_at(LAZY_CHUNK + 100, &mut b).unwrap();
        assert_eq!(*FETCHES_S.lock().unwrap(), was, "a seeded chunk was fetched");
        assert_eq!(b[0], ((LAZY_CHUNK + 100) % 251) as u8);
    }

    /// How many requests of `/data/q` there were: only
    /// `lazy_chunks_past_the_budget_go_least_recent_first` reads it (the
    /// tests run in parallel, so each counted URL has one reader).
    static FETCHES_Q: Mutex<u32> = Mutex::new(0);
    /// How many requests of `/data/s` there were: only
    /// `a_seeded_head_is_never_evicted` reads it.
    static FETCHES_S: Mutex<u32> = Mutex::new(0);

    /// Requests of `/data/r`, each `(offset, len)`.
    static RUNS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
    fn counting(url: &str, offset: u64, into: &mut [u8]) -> bool {
        if url == "/data/r" {
            RUNS.lock().unwrap().push((offset, into.len() as u64));
        }
        for (k, b) in into.iter_mut().enumerate() {
            *b = ((offset + k as u64) % 251) as u8;
        }
        true
    }

    /// A read at the start fetches its chunk only; sequential misses read
    /// ahead 1, 2, 4... chunks; a jump starts over.
    #[test]
    fn lazy_readahead_grows_only_when_sequential() {
        set_lazy_fetch(fake);
        let fs = TmpFs::new();
        fs.add_lazy("/r/f", 40 * LAZY_CHUNK, "/data/r");
        let h = fs.open("r/f", 0, 0).unwrap();
        let mut b = vec![0u8; 4096];
        let mut at = 0;
        while at < 12 * LAZY_CHUNK {
            h.read_at(at, &mut b).unwrap();
            at += 4096;
        }
        h.read_at(30 * LAZY_CHUNK, &mut b).unwrap();
        let runs: Vec<(u64, u64)> = RUNS.lock().unwrap().iter().map(|&(o, n)| (o / LAZY_CHUNK, n / LAZY_CHUNK)).collect();
        // (start chunk, chunks): the read's own, then 1, 2, 4, 8 ahead
        assert_eq!(runs, vec![(0, 1), (1, 2), (3, 3), (6, 5), (11, 9), (30, 1)]);
    }

    /// Past the budget, the chunks read longest ago go, and one read again
    /// is fetched again; a read larger than the budget is still served whole.
    #[test]
    fn lazy_chunks_past_the_budget_go_least_recent_first() {
        set_lazy_fetch(fake);
        let fs = TmpFs::new();
        let max = LAZY_HELD_MAX / LAZY_CHUNK;
        fs.add_lazy("/r/g", (max + 40) * LAZY_CHUNK, "/data/q");
        let h = fs.open("r/g", 0, 0).unwrap();
        let mut b = vec![0u8; 16];
        let at = |k: u64| k * LAZY_CHUNK + 7;
        // Every other chunk, so no read is sequential (no readahead).
        let mut k = 0;
        while k < 2 * (max + 4) {
            h.read_at(at(k), &mut b).unwrap();
            assert_eq!(b[0], (at(k) % 251) as u8);
            if k == 2 * (max - 1) {
                // chunk 0 used again, the last before the budget is reached
                h.read_at(at(0), &mut b).unwrap();
            }
            k += 2;
        }
        let held = |fs: &TmpFs| {
            let inner = fs.inner.read().unwrap();
            let Some(Node::File(f)) = inner.nodes.get("r/g") else { panic!() };
            let l = f.lazy.lock().unwrap();
            let l = l.as_ref().unwrap();
            let mut keys: Vec<u64> = l.chunks.keys().copied().collect();
            keys.sort();
            (l.held, keys)
        };
        let (bytes, keys) = held(&fs);
        assert_eq!(bytes, max * LAZY_CHUNK);
        // max + 4 chunks read: the four used longest ago went, 0 (read again) not.
        assert!(keys.contains(&0) && !keys.contains(&2) && !keys.contains(&4) && !keys.contains(&8) && keys.contains(&10));
        let was = *FETCHES_Q.lock().unwrap();
        h.read_at(at(2), &mut b).unwrap();
        assert_eq!(b[0], (at(2) % 251) as u8);
        assert_eq!(*FETCHES_Q.lock().unwrap(), was + 1, "an evicted chunk is fetched again");
        // A read past the budget keeps every chunk it reads until it is served.
        let mut big = vec![0u8; ((max + 2) * LAZY_CHUNK) as usize];
        assert_eq!(h.read_at(LAZY_CHUNK, &mut big).unwrap(), big.len() as u64);
        assert!(big.iter().enumerate().step_by(4093).all(|(i, &v)| v == ((LAZY_CHUNK + i as u64) % 251) as u8));
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

