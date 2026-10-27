//! `inotify`: a descriptor that reads what happened to watched files and
//! directories.
//!
//! The events come from the filesystems' own changes, made where the
//! filesystem lives: a write, a create, an unlink, a rename, a chmod or a
//! close after writing, each said by the filesystem through [`event`].
//! Instances and their watches live beside those filesystems (a static of
//! this module), so in the page they are the global kernel's, whose tree
//! every process shares, and a process reaches them through [`Remote`].
//!
//! What is watched is an inode of one filesystem ([`Key`]), as Linux has
//! it: a watched file renamed is the same watch, and a watch on a directory
//! hears about its entries by name.
//!
//! A host-mounted tree (`hostfs`) hears only the changes gex itself makes to
//! it, in this process: a change by a host program, or by another gex process
//! (another memory, another copy of this module), is not seen.
//!
//! Cost when nothing is watched: [`event`] and [`any`] are one relaxed load.

use crate::errno::{Errno, SysResult};
use crate::vfs::{FileHandle, FileType, Metadata};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// The `IN_*` bits, as the guest spells them.
pub mod mask {
    pub const ACCESS: u32 = 0x1;
    pub const MODIFY: u32 = 0x2;
    pub const ATTRIB: u32 = 0x4;
    pub const CLOSE_WRITE: u32 = 0x8;
    pub const CLOSE_NOWRITE: u32 = 0x10;
    pub const OPEN: u32 = 0x20;
    pub const MOVED_FROM: u32 = 0x40;
    pub const MOVED_TO: u32 = 0x80;
    pub const CREATE: u32 = 0x100;
    pub const DELETE: u32 = 0x200;
    pub const DELETE_SELF: u32 = 0x400;
    pub const MOVE_SELF: u32 = 0x800;
    pub const Q_OVERFLOW: u32 = 0x4000;
    pub const IGNORED: u32 = 0x8000;
    pub const ONLYDIR: u32 = 0x0100_0000;
    pub const DONT_FOLLOW: u32 = 0x0200_0000;
    pub const EXCL_UNLINK: u32 = 0x0400_0000;
    pub const MASK_CREATE: u32 = 0x1000_0000;
    pub const MASK_ADD: u32 = 0x2000_0000;
    pub const ISDIR: u32 = 0x4000_0000;
    pub const ONESHOT: u32 = 0x8000_0000;
    /// Every event a watch can ask for.
    pub const ALL_EVENTS: u32 = 0xfff;
    /// The events said to a watch on the object itself, never with a name.
    pub const SELF_ONLY: u32 = DELETE_SELF | MOVE_SELF;
}

/// `struct inotify_event` without its name: wd, mask, cookie, len.
const HEADER: usize = 16;

/// The most events an instance holds unread (Linux's default
/// `max_queued_events`); past it one `IN_Q_OVERFLOW` and nothing more.
const MAX_QUEUED: usize = 16384;

/// One filesystem's inode: the filesystem by an address of its own (unique
/// within this module while it is mounted) and the inode number it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub fs: usize,
    pub ino: u64,
}

#[derive(Debug)]
struct Watch {
    key: Option<Key>,
    mask: u32,
}

#[derive(Debug)]
struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Option<String>,
}

impl Event {
    fn len(&self) -> usize {
        HEADER + self.name.as_ref().map_or(0, |n| (n.len() + 1).next_multiple_of(HEADER))
    }
}

#[derive(Debug)]
struct Instance {
    refs: u32,
    next_wd: i32,
    watches: BTreeMap<i32, Watch>,
    events: VecDeque<Event>,
    bytes: usize,
    overflowed: bool,
    queue: crate::wait::Queue,
}

impl Instance {
    fn push(&mut self, ev: Event) {
        if self.overflowed {
            return;
        }
        // The same event again, unread: Linux merges it with the last one.
        if let Some(last) = self.events.back() {
            if last.wd == ev.wd && last.mask == ev.mask && last.cookie == ev.cookie && last.name == ev.name {
                return;
            }
        }
        let ev = if self.events.len() + 1 >= MAX_QUEUED {
            self.overflowed = true;
            Event { wd: -1, mask: mask::Q_OVERFLOW, cookie: 0, name: None }
        } else {
            ev
        };
        self.bytes += ev.len();
        self.events.push_back(ev);
    }
}

#[derive(Default)]
struct Registry {
    next_id: i32,
    instances: BTreeMap<i32, Instance>,
    /// Who watches each inode: (instance, wd).
    by_key: HashMap<Key, Vec<(i32, i32)>>,
}

impl Registry {
    fn forget_watch(&mut self, id: i32, wd: i32, key: Option<Key>) {
        if let Some(k) = key {
            if let Some(v) = self.by_key.get_mut(&k) {
                v.retain(|&(i, w)| !(i == id && w == wd));
                if v.is_empty() {
                    self.by_key.remove(&k);
                }
            }
        }
    }
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
/// Live watches on real keys: [`event`] does nothing while it is 0.
static WATCHES: AtomicUsize = AtomicUsize::new(0);
static COOKIE: AtomicU32 = AtomicU32::new(0);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether anything here is watched: a filesystem asks before working out
/// what to say.
#[inline]
pub fn any() -> bool {
    WATCHES.load(Ordering::Relaxed) != 0
}

/// A cookie tying a rename's `IN_MOVED_FROM` to its `IN_MOVED_TO`.
pub fn cookie() -> u32 {
    COOKIE.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1)
}

/// Something happened to inode `key`: said to its own watches (with no
/// name), or with `name` to the watches of the directory `key` is, about
/// its entry `name`. `isdir` is whether the inode the event is about is a
/// directory. Self events (`IN_DELETE_SELF`, `IN_MOVE_SELF`) end a watch
/// asked with `IN_ONESHOT` as any event does; `IN_DELETE_SELF` ends every
/// watch on the inode, with `IN_IGNORED`.
pub fn event(key: Key, ev: u32, cookie: u32, name: Option<&str>, isdir: bool) {
    if !any() {
        return;
    }
    let mut notify = Vec::new();
    {
        let mut reg = registry();
        let Some(who) = reg.by_key.get(&key).cloned() else { return };
        let mut ended = Vec::new();
        for (id, wd) in who {
            let Some(inst) = reg.instances.get_mut(&id) else { continue };
            let Some(w) = inst.watches.get(&wd) else { continue };
            let wmask = w.mask;
            let deleted = name.is_none() && ev & mask::DELETE_SELF != 0;
            if wmask & ev & mask::ALL_EVENTS != 0 {
                let dir_bit = if isdir && ev & mask::SELF_ONLY == 0 { mask::ISDIR } else { 0 };
                inst.push(Event { wd, mask: ev | dir_bit, cookie, name: name.map(str::to_string) });
                notify.push(inst.queue.id());
                if wmask & mask::ONESHOT != 0 {
                    ended.push((id, wd));
                    continue;
                }
            }
            if deleted {
                ended.push((id, wd));
            }
        }
        for (id, wd) in ended {
            let Some(inst) = reg.instances.get_mut(&id) else { continue };
            if let Some(w) = inst.watches.remove(&wd) {
                inst.push(Event { wd, mask: mask::IGNORED, cookie: 0, name: None });
                notify.push(inst.queue.id());
                if w.key.is_some() {
                    WATCHES.fetch_sub(1, Ordering::Relaxed);
                }
                reg.forget_watch(id, wd, w.key);
            }
        }
    }
    notify.sort_unstable();
    notify.dedup();
    for q in notify {
        crate::wait::notify(q);
    }
}

/// An event about entry `name` of directory `dir`, and about the entry's
/// own inode `own` (when the filesystem knows it): what a change to one
/// name says to both of its watchers.
pub fn entry_event(dir: Key, name: &str, own: Option<Key>, ev: u32, cookie: u32, isdir: bool) {
    if !any() {
        return;
    }
    if let Some(k) = own {
        event(k, ev, cookie, None, isdir);
    }
    event(dir, ev, cookie, Some(name), isdir);
}

/// A new instance; its id.
pub fn create() -> i32 {
    let mut reg = registry();
    reg.next_id += 1;
    let id = reg.next_id;
    reg.instances.insert(
        id,
        Instance { refs: 1, next_wd: 0, watches: BTreeMap::new(), events: VecDeque::new(), bytes: 0, overflowed: false, queue: crate::wait::Queue::new() },
    );
    id
}

/// One more holder of instance `id` (a descriptor handed to another process).
pub fn dup(id: i32) -> SysResult {
    let mut reg = registry();
    let inst = reg.instances.get_mut(&id).ok_or(Errno::EBADF)?;
    inst.refs += 1;
    Ok(0)
}

/// A holder let go of instance `id`; the last takes its watches with it.
pub fn close(id: i32) {
    let mut reg = registry();
    let Some(inst) = reg.instances.get_mut(&id) else { return };
    inst.refs -= 1;
    if inst.refs > 0 {
        return;
    }
    let inst = reg.instances.remove(&id).unwrap();
    for (wd, w) in inst.watches {
        if w.key.is_some() {
            WATCHES.fetch_sub(1, Ordering::Relaxed);
        }
        reg.forget_watch(id, wd, w.key);
    }
}

/// `inotify_add_watch` on what [`Key`] names (`None`: an inode no change is
/// said for, as `/proc`'s, which still gets a descriptor). `isdir` is what
/// the path is, for `IN_ONLYDIR`.
pub fn add_watch(id: i32, key: Option<Key>, isdir: bool, m: u32) -> SysResult {
    if m & mask::ALL_EVENTS == 0 || (m & mask::MASK_ADD != 0 && m & mask::MASK_CREATE != 0) {
        return Err(Errno::EINVAL);
    }
    if m & mask::ONLYDIR != 0 && !isdir {
        return Err(Errno::ENOTDIR);
    }
    let mut reg = registry();
    let inst = reg.instances.get_mut(&id).ok_or(Errno::EINVAL)?;
    // A second watch on the same inode is the first one, its mask replaced
    // or added to.
    let keep = m & (mask::ALL_EVENTS | mask::ONESHOT | mask::EXCL_UNLINK);
    if let Some(k) = key {
        if let Some((&wd, w)) = inst.watches.iter_mut().find(|(_, w)| w.key == Some(k)) {
            if m & mask::MASK_CREATE != 0 {
                return Err(Errno::EEXIST);
            }
            w.mask = if m & mask::MASK_ADD != 0 { w.mask | keep } else { keep };
            return Ok(wd as u64);
        }
    }
    inst.next_wd += 1;
    let wd = inst.next_wd;
    inst.watches.insert(wd, Watch { key, mask: keep });
    if let Some(k) = key {
        reg.by_key.entry(k).or_default().push((id, wd));
        WATCHES.fetch_add(1, Ordering::Relaxed);
    }
    Ok(wd as u64)
}

/// `inotify_rm_watch`: the watch ends, with `IN_IGNORED`.
pub fn rm_watch(id: i32, wd: i32) -> SysResult {
    let q;
    {
        let mut reg = registry();
        let inst = reg.instances.get_mut(&id).ok_or(Errno::EINVAL)?;
        let w = inst.watches.remove(&wd).ok_or(Errno::EINVAL)?;
        inst.push(Event { wd, mask: mask::IGNORED, cookie: 0, name: None });
        q = inst.queue.id();
        if w.key.is_some() {
            WATCHES.fetch_sub(1, Ordering::Relaxed);
        }
        reg.forget_watch(id, wd, w.key);
    }
    crate::wait::notify(q);
    Ok(0)
}

/// Whole events into `buf`, as many as fit: `EAGAIN` with none queued,
/// `EINVAL` when the first does not fit.
pub fn read(id: i32, buf: &mut [u8]) -> SysResult {
    let mut reg = registry();
    let inst = reg.instances.get_mut(&id).ok_or(Errno::EBADF)?;
    if inst.events.is_empty() {
        return Err(Errno::EAGAIN);
    }
    let mut at = 0;
    while let Some(ev) = inst.events.front() {
        let n = ev.len();
        if at + n > buf.len() {
            if at == 0 {
                return Err(Errno::EINVAL);
            }
            break;
        }
        let out = &mut buf[at..at + n];
        out.fill(0);
        out[0..4].copy_from_slice(&ev.wd.to_le_bytes());
        out[4..8].copy_from_slice(&ev.mask.to_le_bytes());
        out[8..12].copy_from_slice(&ev.cookie.to_le_bytes());
        out[12..16].copy_from_slice(&((n - HEADER) as u32).to_le_bytes());
        if let Some(name) = &ev.name {
            out[HEADER..HEADER + name.len()].copy_from_slice(name.as_bytes());
        }
        at += n;
        inst.bytes -= n;
        if ev.mask == mask::Q_OVERFLOW {
            inst.overflowed = false;
        }
        inst.events.pop_front();
    }
    Ok(at as u64)
}

/// Bytes of events queued (`FIONREAD`), or `None` for no such instance.
pub fn queued(id: i32) -> Option<usize> {
    registry().instances.get(&id).map(|i| i.bytes)
}

/// The instance's wait queue.
pub fn queue(id: i32) -> u32 {
    registry().instances.get(&id).map_or(0, |i| i.queue.id())
}

/// How a process reaches instances that live in another module (the page's
/// global kernel). Every return is a count, an id or a negated errno; every
/// address is in the caller's memory.
#[derive(Clone, Copy)]
pub struct Remote {
    pub create: extern "C" fn() -> i32,
    pub dup: extern "C" fn(id: i32) -> i32,
    pub close: extern "C" fn(id: i32),
    /// A watch on `path`, relative to the remote tree's root.
    pub add_watch: extern "C" fn(id: i32, path: crate::PAddr, path_len: u32, mask: u32) -> i32,
    /// A watch on something whose changes are not heard (a local mount).
    pub add_dead: extern "C" fn(id: i32, mask: u32, isdir: u32) -> i32,
    pub rm_watch: extern "C" fn(id: i32, wd: i32) -> i32,
    pub read: extern "C" fn(id: i32, at: crate::PAddr, len: u32) -> i32,
    /// Bytes queued, or a negated errno.
    pub queued: extern "C" fn(id: i32) -> i32,
    pub queue: extern "C" fn(id: i32) -> u32,
}

static REMOTE: OnceLock<Remote> = OnceLock::new();

/// Instances live in another module from now on (once per module instance).
pub fn set_remote(r: Remote) {
    let _ = REMOTE.set(r);
}

pub fn remote() -> Option<Remote> {
    REMOTE.get().copied()
}

/// An instance, as the descriptor `inotify_init1` gives.
#[derive(Debug)]
pub struct Inotify {
    pub id: i32,
    /// Kept in another module ([`Remote`]).
    pub remote: bool,
}

impl Inotify {
    /// A new instance where this module keeps them.
    pub fn new() -> Result<Inotify, Errno> {
        match remote() {
            Some(r) => match (r.create)() {
                id if id > 0 => Ok(Inotify { id, remote: true }),
                e => Err(Errno(-e)),
            },
            None => Ok(Inotify { id: create(), remote: false }),
        }
    }

    /// Another holder of a remote instance, handed over by its id.
    pub fn adopt_remote(id: i32) -> Inotify {
        Inotify { id, remote: true }
    }

    /// Another reference for another process: the id it goes by.
    pub fn dup_id(&self) -> Option<i32> {
        if self.remote {
            let r = remote()?;
            ((r.dup)(self.id) >= 0).then_some(self.id)
        } else {
            None
        }
    }

    pub fn rm_watch(&self, wd: i32) -> SysResult {
        match (self.remote, remote()) {
            (true, Some(r)) => ret((r.rm_watch)(self.id, wd)),
            _ => rm_watch(self.id, wd),
        }
    }

    /// A watch on something only this module hears changes to.
    pub fn add_local(&self, key: Option<Key>, isdir: bool, m: u32) -> SysResult {
        match (self.remote, remote()) {
            // A remote instance cannot hear this module's changes.
            (true, Some(r)) => ret((r.add_dead)(self.id, m, isdir as u32)),
            _ => add_watch(self.id, key, isdir, m),
        }
    }

    /// A watch on `path` of the remote tree.
    pub fn add_remote(&self, path: &str, isdir: bool, m: u32) -> SysResult {
        match (self.remote, remote()) {
            (true, Some(r)) => ret((r.add_watch)(self.id, path.as_ptr() as crate::PAddr, path.len() as u32, m)),
            // An instance here cannot hear the other module's changes.
            _ => add_watch(self.id, None, isdir, m),
        }
    }
}

fn ret(n: i32) -> SysResult {
    if n < 0 { Err(Errno(-n)) } else { Ok(n as u64) }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        match (self.remote, remote()) {
            (true, Some(r)) => (r.close)(self.id),
            _ => close(self.id),
        }
    }
}

impl FileHandle for Inotify {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        match (self.remote, remote()) {
            (true, Some(r)) => ret((r.read)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32)),
            _ => read(self.id, buf),
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: crate::vfs::anon_ino(self as *const Inotify as *const u8),
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        self.available().unwrap_or(0) > 0
    }

    fn poll_writable(&self) -> bool {
        false
    }

    fn available(&self) -> Option<u64> {
        match (self.remote, remote()) {
            (true, Some(r)) => Some((r.queued)(self.id).max(0) as u64),
            _ => Some(queued(self.id).unwrap_or(0) as u64),
        }
    }

    fn wait_queue(&self) -> u32 {
        match (self.remote, remote()) {
            (true, Some(r)) => (r.queue)(self.id),
            _ => queue(self.id),
        }
    }

    fn as_inotify(self: std::sync::Arc<Self>) -> Option<std::sync::Arc<Inotify>> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(id: i32) -> Vec<(i32, u32, u32, String)> {
        let mut buf = vec![0u8; 4096];
        let n = match read(id, &mut buf) {
            Ok(n) => n as usize,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let mut at = 0;
        while at < n {
            let w = |o: usize| u32::from_le_bytes(buf[at + o..at + o + 4].try_into().unwrap());
            let len = w(12) as usize;
            let name = String::from_utf8_lossy(&buf[at + HEADER..at + HEADER + len]).trim_end_matches('\0').to_string();
            out.push((w(0) as i32, w(4), w(8), name));
            at += HEADER + len;
        }
        out
    }

    #[test]
    fn a_directory_watch_hears_its_entries_by_name() {
        let id = create();
        let dir = Key { fs: 1, ino: 10 };
        let file = Key { fs: 1, ino: 11 };
        let wd = add_watch(id, Some(dir), true, mask::CREATE | mask::MODIFY).unwrap() as i32;
        entry_event(dir, "a", Some(file), mask::CREATE, 0, false);
        entry_event(dir, "a", Some(file), mask::MODIFY, 0, false);
        entry_event(dir, "a", Some(file), mask::MODIFY, 0, false);
        entry_event(dir, "a", Some(file), mask::DELETE, 0, false);
        assert_eq!(events(id), vec![(wd, mask::CREATE, 0, "a".into()), (wd, mask::MODIFY, 0, "a".into())]);
        close(id);
    }

    #[test]
    fn a_deleted_inode_ends_its_watch() {
        let id = create();
        let file = Key { fs: 2, ino: 5 };
        let wd = add_watch(id, Some(file), false, mask::DELETE_SELF).unwrap() as i32;
        event(file, mask::DELETE_SELF, 0, None, false);
        assert_eq!(events(id), vec![(wd, mask::DELETE_SELF, 0, String::new()), (wd, mask::IGNORED, 0, String::new())]);
        assert_eq!(rm_watch(id, wd).err(), Some(Errno::EINVAL));
        close(id);
    }

    #[test]
    fn a_short_read_is_einval_and_a_second_watch_is_the_first() {
        let id = create();
        let dir = Key { fs: 3, ino: 1 };
        let a = add_watch(id, Some(dir), true, mask::CREATE).unwrap();
        assert_eq!(add_watch(id, Some(dir), true, mask::DELETE | mask::MASK_ADD).unwrap(), a);
        assert_eq!(add_watch(id, Some(dir), true, mask::DELETE | mask::MASK_CREATE).err(), Some(Errno::EEXIST));
        entry_event(dir, "long-name", None, mask::DELETE, 0, false);
        let mut small = [0u8; 20];
        assert_eq!(read(id, &mut small).err(), Some(Errno::EINVAL));
        assert_eq!(queued(id), Some(32));
        close(id);
    }
}
