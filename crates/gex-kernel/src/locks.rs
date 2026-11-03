//! Advisory file locks: `flock` and `fcntl`'s record locks (`F_SETLK`,
//! `F_SETLKW`, `F_GETLK` and their open-file-description forms).
//!
//! One table for every process: natively the kernel's (shared by its
//! forks), in the page the global kernel's ([`share_locks`]), so two
//! Workers' processes contend as two Linux processes do.
//!
//! Who owns a lock:
//! - `flock`: the open file description ([`Owner::Desc`]). `dup` and a
//!   fork share it; a second `open` contends. Gone when the description is.
//! - a record lock (`F_SETLK`): the process ([`Owner::Pid`]). Every one the
//!   process has on a file goes when it closes *any* descriptor of the file
//!   ([`crate::file::FdTable`]'s close rule), and all of them at exit; a
//!   fork's child has none.
//! - an OFD lock (`F_OFD_SETLK`): the description again, its record lock
//!   kind ([`Owner::Desc`]); `F_GETLK` says pid -1 for it.
//!
//! A request that cannot be granted answers `EAGAIN` (`EWOULDBLOCK` for
//! `flock`); a blocking one then parks ([`crate::syscalls::BlockOp::FileLock`])
//! until [`FileLocks::would_grant`] says yes, woken by every release
//! ([`FileLocks::queue_id`]) and made again.
//!
//! A file is its path, as `flock`'s table always named it.

use crate::errno::{Errno, SysResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `flock`'s operations.
pub mod lock_op {
    pub const SH: u64 = 1;
    pub const EX: u64 = 2;
    pub const NB: u64 = 4;
    pub const UN: u64 = 8;
}

/// `struct flock`'s `l_type`.
pub mod lk {
    pub const RDLCK: u32 = 0;
    pub const WRLCK: u32 = 1;
    pub const UNLCK: u32 = 2;
}

/// Who holds a lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A process's record lock.
    Pid(i32),
    /// An open file description's (`flock`, OFD): its key
    /// ([`crate::file::OpenFile::lock_key`]).
    Desc(u64),
}

impl Owner {
    pub fn encode(self) -> u64 {
        match self {
            Owner::Pid(p) => 1 << 63 | p as u32 as u64,
            Owner::Desc(k) => k & !(1 << 63),
        }
    }

    fn decode(v: u64) -> Owner {
        if v & 1 << 63 != 0 {
            Owner::Pid(v as u32 as i32)
        } else {
            Owner::Desc(v)
        }
    }
}

/// A byte range lock, `[start, end)`; `end` `u64::MAX` is to the end of the
/// file however far it grows (`l_len` 0).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Range {
    file: String,
    owner: Owner,
    /// The process that took it, for `F_GETLK` (-1 for a description's).
    pid: i32,
    write: bool,
    start: u64,
    end: u64,
}

/// A `flock` lock.
#[derive(Clone, Debug)]
struct Whole {
    file: String,
    key: u64,
    exclusive: bool,
}

/// One request, as a blocking caller makes it again ([`FileLocks::apply`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockReq {
    /// `flock(op)`, `op` without `LOCK_NB`.
    Flock { file: String, key: u64, op: u64 },
    /// A record lock or unlock of `[start, end)` (`typ` an [`lk`]).
    Set { file: String, owner: Owner, pid: i32, typ: u32, start: u64, end: u64 },
}

/// What `F_GETLK` reports: the lock in the way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub typ: u32,
    pub start: u64,
    pub end: u64,
    pub pid: i32,
}

#[derive(Default, Debug)]
struct Table {
    whole: Vec<Whole>,
    ranges: Vec<Range>,
    /// Who waits for whom: a blocked record-lock request's process and the
    /// request, for `EDEADLK`. Dropped when the process next asks anything
    /// of the table, or exits.
    waits: Vec<(i32, LockReq)>,
}

/// The lock table ([`crate::unix::FileLocks`]).
#[derive(Debug, Default)]
pub struct FileLocks {
    t: Mutex<Table>,
    /// Every release (an unlock, a close, an exit): what a parked request
    /// waits on.
    queue: crate::wait::Queue,
    /// How many releases there have been.
    releases: AtomicU64,
}

/// The global kernel's table, for a process in the page ([`share_locks`]).
#[derive(Clone, Copy, Debug)]
pub struct LockRemote {
    /// One encoded call ([`encode`]): its answer, a [`Conflict`] written
    /// to `out` for `GET` (16 + 4 + 4 bytes), or a negated errno.
    pub call: fn(req: &[u8], out: &mut [u8]) -> i64,
    /// The table's release queue.
    pub queue: fn() -> u32,
    /// [`write_seq`], the global kernel's.
    pub seq: fn() -> u64,
}

static REMOTE: OnceLock<LockRemote> = OnceLock::new();

/// Every table of this module is the global kernel's from now on.
pub fn share_locks(r: LockRemote) {
    let _ = REMOTE.set(r);
}

/// The calls [`LockRemote::call`] carries.
pub mod call {
    pub const FLOCK: u8 = 1;
    pub const SET: u8 = 2;
    pub const SET_WAIT: u8 = 3;
    pub const GET: u8 = 4;
    pub const TEST: u8 = 5;
    pub const TEST_FLOCK: u8 = 6;
    pub const RELEASE: u8 = 7;
    pub const RELEASE_PID_FILE: u8 = 8;
    pub const RELEASE_PID: u8 = 9;
    pub const WROTE: u8 = 10;
    pub const WRITTEN: u8 = 11;
}

/// A call as bytes: op, owner (or key), pid, type (or flock op), start,
/// end, the file's handle in the global kernel (-1: none), then the path.
pub fn encode(op: u8, owner: u64, pid: i32, typ: u64, start: u64, end: u64, file: &str) -> Vec<u8> {
    encode_with(op, owner, pid, typ, start, end, -1, file)
}

/// [`encode`] naming the file's global-kernel handle too: a granted lock
/// or flock call answers the file's change count with it, into the first
/// 8 bytes of `out` ([`FileLocks::set_seen`]).
#[allow(clippy::too_many_arguments)]
pub fn encode_with(op: u8, owner: u64, pid: i32, typ: u64, start: u64, end: u64, handle: i32, file: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(41 + file.len());
    b.push(op);
    b.extend_from_slice(&owner.to_le_bytes());
    b.extend_from_slice(&pid.to_le_bytes());
    b.extend_from_slice(&typ.to_le_bytes());
    b.extend_from_slice(&start.to_le_bytes());
    b.extend_from_slice(&end.to_le_bytes());
    b.extend_from_slice(&handle.to_le_bytes());
    b.extend_from_slice(file.as_bytes());
    b
}

/// [`encode`]'s fields back.
pub struct Decoded {
    pub op: u8,
    pub owner: u64,
    pub pid: i32,
    pub typ: u64,
    pub start: u64,
    pub end: u64,
    pub handle: i32,
    pub file: String,
}

pub fn decode(b: &[u8]) -> Option<Decoded> {
    if b.len() < 41 {
        return None;
    }
    let u = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    Some(Decoded {
        op: b[0],
        owner: u(1),
        pid: i32::from_le_bytes(b[9..13].try_into().unwrap()),
        typ: u(13),
        start: u(21),
        end: u(29),
        handle: i32::from_le_bytes(b[37..41].try_into().unwrap()),
        file: String::from_utf8_lossy(&b[41..]).into_owned(),
    })
}

fn overlaps(a: &Range, start: u64, end: u64) -> bool {
    a.start < end && start < a.end
}

impl Table {
    /// The first record lock in the way of `owner` taking `[start, end)`.
    fn conflict(&self, file: &str, owner: Owner, write: bool, start: u64, end: u64) -> Option<&Range> {
        self.ranges
            .iter()
            .find(|r| r.file == file && r.owner != owner && (write || r.write) && overlaps(r, start, end))
    }

    /// `owner`'s locks on `file` over `[start, end)` replaced by `new`
    /// (none: an unlock), split where the range cuts one and merged with
    /// the owner's touching locks of the same kind, as Linux keeps them.
    fn put(&mut self, file: &str, owner: Owner, pid: i32, new: Option<bool>, start: u64, end: u64) {
        let mut kept = Vec::with_capacity(self.ranges.len() + 1);
        let (mut lo, mut hi) = (start, end);
        for r in self.ranges.drain(..) {
            if r.file != file || r.owner != owner {
                kept.push(r);
                continue;
            }
            // A touching or overlapping lock of the same kind joins the new one.
            if new == Some(r.write) && r.start <= hi && lo <= r.end {
                lo = lo.min(r.start);
                hi = hi.max(r.end);
                continue;
            }
            if !overlaps(&r, start, end) {
                kept.push(r);
                continue;
            }
            if r.start < start {
                kept.push(Range { end: start, ..r.clone() });
            }
            if r.end > end {
                kept.push(Range { start: end, ..r });
            }
        }
        if let Some(write) = new {
            kept.push(Range { file: file.to_string(), owner, pid, write, start: lo, end: hi });
        }
        self.ranges = kept;
    }

    fn flock_conflict(&self, file: &str, key: u64, exclusive: bool) -> bool {
        self.whole.iter().any(|h| h.file == file && h.key != key && (exclusive || h.exclusive))
    }

    /// Whether `pid` waiting for the holders of what `req` wants would close
    /// a cycle: one of them already waits for something `pid` holds (two
    /// processes, or a longer chain of record locks).
    fn deadlocks(&self, pid: i32, req: &LockReq) -> bool {
        let LockReq::Set { file, owner, typ, start, end, .. } = req else { return false };
        let mut seen = Vec::new();
        let mut frontier: Vec<i32> = self
            .ranges
            .iter()
            .filter(|r| &r.file == file && r.owner != *owner && (*typ == lk::WRLCK || r.write) && overlaps(r, *start, *end))
            .filter_map(|r| match r.owner {
                Owner::Pid(p) => Some(p),
                Owner::Desc(_) => None,
            })
            .collect();
        while let Some(h) = frontier.pop() {
            if h == pid {
                return true;
            }
            if seen.contains(&h) || seen.len() > 64 {
                continue;
            }
            seen.push(h);
            for (_, w) in self.waits.iter().filter(|(p, _)| *p == h) {
                if let LockReq::Set { file, owner, typ, start, end, .. } = w {
                    for r in self.ranges.iter().filter(|r| &r.file == file && r.owner != *owner && (*typ == lk::WRLCK || r.write) && overlaps(r, *start, *end)) {
                        if let Owner::Pid(p) = r.owner {
                            frontier.push(p);
                        }
                    }
                }
            }
        }
        false
    }
}

impl FileLocks {
    pub fn new() -> FileLocks {
        FileLocks::default()
    }

    fn remote() -> Option<LockRemote> {
        REMOTE.get().copied()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.t.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn released(&self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
        self.queue.notify();
    }

    /// The queue every release notifies, for a parked request to register on.
    pub fn queue_id(&self) -> u32 {
        match Self::remote() {
            Some(r) => (r.queue)(),
            None => self.queue.id(),
        }
    }

    /// `flock` for one open file description: `EWOULDBLOCK` when another
    /// holds it in the way, blocking or not (the caller parks).
    pub fn flock(&self, file: &str, key: u64, op: u64) -> SysResult {
        self.flock_seen(file, key, op, None).0
    }

    /// [`FileLocks::flock`], with the file's change count when granted in
    /// the page and the global kernel holds the file as `handle` (one call
    /// for both, where a lock taken needs both: [`crate::syscalls::sync_shared_file`]).
    pub fn flock_seen(&self, file: &str, key: u64, op: u64, handle: Option<i32>) -> (SysResult, Option<u64>) {
        if let Some(r) = Self::remote() {
            let mut out = [0u8; 8];
            let v = (r.call)(&encode_with(call::FLOCK, key, 0, op, 0, 0, handle.unwrap_or(-1), file), &mut out);
            return (remote_answer(v), seen_count(v, &out));
        }
        (self.flock_local(file, key, op), None)
    }

    fn flock_local(&self, file: &str, key: u64, op: u64) -> SysResult {
        let mut t = self.table();
        if op & lock_op::UN != 0 {
            let before = t.whole.len();
            t.whole.retain(|h| !(h.key == key && h.file == file));
            let gone = t.whole.len() != before;
            drop(t);
            if gone {
                self.released();
            }
            return Ok(0);
        }
        let exclusive = op & lock_op::EX != 0;
        if !exclusive && op & lock_op::SH == 0 {
            return Err(Errno::EINVAL);
        }
        if t.flock_conflict(file, key, exclusive) {
            return Err(Errno::EWOULDBLOCK);
        }
        // Re-locking the same description converts the mode, not stacks a
        // lock; a conversion to shared is a release to anyone waiting.
        let was_ex = t.whole.iter().any(|h| h.file == file && h.key == key && h.exclusive);
        t.whole.retain(|h| !(h.file == file && h.key == key));
        t.whole.push(Whole { file: file.to_string(), key, exclusive });
        drop(t);
        if was_ex && !exclusive {
            self.released();
        }
        Ok(0)
    }

    /// A record lock or unlock (`typ` an [`lk`]) of `[start, end)` by
    /// `owner` (process `pid`). `EAGAIN` when another owner's lock is in
    /// the way; `EDEADLK`, for a request that would wait (`wait`), when
    /// waiting would close a cycle of waiters.
    pub fn set(&self, file: &str, owner: Owner, pid: i32, typ: u32, start: u64, end: u64, wait: bool) -> SysResult {
        self.set_seen(file, owner, pid, typ, start, end, wait, None).0
    }

    /// [`FileLocks::set`], with the file's change count as
    /// [`FileLocks::flock_seen`] has it.
    #[allow(clippy::too_many_arguments)]
    pub fn set_seen(&self, file: &str, owner: Owner, pid: i32, typ: u32, start: u64, end: u64, wait: bool, handle: Option<i32>) -> (SysResult, Option<u64>) {
        if let Some(r) = Self::remote() {
            let op = if wait { call::SET_WAIT } else { call::SET };
            let mut out = [0u8; 8];
            let v = (r.call)(&encode_with(op, owner.encode(), pid, typ as u64, start, end, handle.unwrap_or(-1), file), &mut out);
            return (remote_answer(v), seen_count(v, &out));
        }
        (self.set_local(file, owner, pid, typ, start, end, wait), None)
    }

    #[allow(clippy::too_many_arguments)]
    fn set_local(&self, file: &str, owner: Owner, pid: i32, typ: u32, start: u64, end: u64, wait: bool) -> SysResult {
        if start >= end {
            return Ok(0);
        }
        let mut t = self.table();
        t.waits.retain(|(p, _)| *p != pid);
        if typ == lk::UNLCK {
            let before = t.ranges.clone();
            t.put(file, owner, pid, None, start, end);
            let changed = t.ranges != before;
            drop(t);
            if changed {
                self.released();
            }
            return Ok(0);
        }
        let write = typ == lk::WRLCK;
        if t.conflict(file, owner, write, start, end).is_some() {
            if wait {
                let req = LockReq::Set { file: file.to_string(), owner, pid, typ, start, end };
                if t.deadlocks(pid, &req) {
                    return Err(Errno::EDEADLK);
                }
                t.waits.push((pid, req));
            }
            return Err(Errno::EAGAIN);
        }
        // A write lock turned read, or a range given up, is a release too.
        let down = t.ranges.iter().any(|r| r.file == file && r.owner == owner && overlaps(r, start, end) && r.write && !write);
        t.put(file, owner, pid, Some(write), start, end);
        drop(t);
        if down {
            self.released();
        }
        Ok(0)
    }

    /// `F_GETLK`: the lock that would stop `owner` taking `[start, end)`.
    pub fn get(&self, file: &str, owner: Owner, typ: u32, start: u64, end: u64) -> Option<Conflict> {
        if let Some(r) = Self::remote() {
            let mut out = [0u8; 24];
            let n = (r.call)(&encode(call::GET, owner.encode(), 0, typ as u64, start, end, file), &mut out);
            if n <= 0 {
                return None;
            }
            let u = |i: usize| u64::from_le_bytes(out[i..i + 8].try_into().unwrap());
            return Some(Conflict {
                start: u(0),
                end: u(8),
                typ: u32::from_le_bytes(out[16..20].try_into().unwrap()),
                pid: i32::from_le_bytes(out[20..24].try_into().unwrap()),
            });
        }
        let t = self.table();
        t.conflict(file, owner, typ == lk::WRLCK, start, end.max(start + 1)).map(|r| Conflict {
            typ: if r.write { lk::WRLCK } else { lk::RDLCK },
            start: r.start,
            end: r.end,
            pid: r.pid,
        })
    }

    /// Whether `req` would be granted now (a parked request's question).
    pub fn would_grant(&self, req: &LockReq) -> bool {
        if let Some(r) = Self::remote() {
            let b = match req {
                LockReq::Flock { file, key, op } => encode(call::TEST_FLOCK, *key, 0, *op, 0, 0, file),
                LockReq::Set { file, owner, pid, typ, start, end } => encode(call::TEST, owner.encode(), *pid, *typ as u64, *start, *end, file),
            };
            return (r.call)(&b, &mut []) > 0;
        }
        let t = self.table();
        match req {
            LockReq::Flock { file, key, op } => !t.flock_conflict(file, *key, op & lock_op::EX != 0),
            LockReq::Set { file, owner, typ, start, end, .. } => *typ == lk::UNLCK || t.conflict(file, *owner, *typ == lk::WRLCK, *start, *end).is_none(),
        }
    }

    /// The request again, as the blocking call it is (`EAGAIN`: still in
    /// the way).
    pub fn apply(&self, req: &LockReq) -> SysResult {
        match req {
            LockReq::Flock { file, key, op } => self.flock(file, *key, *op),
            LockReq::Set { file, owner, pid, typ, start, end } => self.set(file, *owner, *pid, *typ, *start, *end, true),
        }
    }

    /// Drop everything a description held (`flock`'s and OFD locks), when
    /// it is closed.
    pub fn release(&self, key: u64) {
        if let Some(r) = Self::remote() {
            (r.call)(&encode(call::RELEASE, key, 0, 0, 0, 0, ""), &mut []);
            return;
        }
        let mut t = self.table();
        let (w, r) = (t.whole.len(), t.ranges.len());
        t.whole.retain(|h| h.key != key);
        t.ranges.retain(|h| h.owner != Owner::Desc(key));
        let gone = t.whole.len() != w || t.ranges.len() != r;
        drop(t);
        if gone {
            self.released();
        }
    }

    /// Drop process `pid`'s record locks on `file` (it closed a descriptor
    /// of it), or on every file (`None`: it exited).
    pub fn release_pid(&self, pid: i32, file: Option<&str>) {
        if let Some(r) = Self::remote() {
            let op = if file.is_some() { call::RELEASE_PID_FILE } else { call::RELEASE_PID };
            (r.call)(&encode(op, 0, pid, 0, 0, 0, file.unwrap_or("")), &mut []);
            return;
        }
        let mut t = self.table();
        let before = t.ranges.len();
        t.ranges.retain(|h| !(h.owner == Owner::Pid(pid) && file.is_none_or(|f| f == h.file)));
        if file.is_none() {
            t.waits.retain(|(p, _)| *p != pid);
        }
        let gone = t.ranges.len() != before;
        drop(t);
        if gone {
            self.released();
        }
    }

    /// How many releases there have been.
    pub fn releases(&self) -> u64 {
        self.releases.load(Ordering::SeqCst)
    }

    /// One [`encode`]d call, answered from this table: the global kernel's
    /// side of [`LockRemote::call`]. `caller` is the calling process, whose
    /// descriptions' keys are only its own (each process has its memory).
    pub fn serve(&self, caller: i32, req: &[u8], out: &mut [u8]) -> i64 {
        let Some(d) = decode(req) else { return -(Errno::EINVAL.0 as i64) };
        let key = |k: u64| (caller as u32 as u64) << 32 | (k & 0xffff_ffff);
        let owner = match Owner::decode(d.owner) {
            Owner::Desc(k) => Owner::Desc(key(k)),
            o => o,
        };
        let answer = |r: SysResult| match r {
            Ok(v) => v as i64,
            Err(e) => -(e.0 as i64),
        };
        match d.op {
            call::FLOCK => answer(self.flock(&d.file, key(d.owner), d.typ)),
            call::SET | call::SET_WAIT => answer(self.set(&d.file, owner, d.pid, d.typ as u32, d.start, d.end, d.op == call::SET_WAIT)),
            call::GET => match self.get(&d.file, owner, d.typ as u32, d.start, d.end) {
                Some(c) if out.len() >= 24 => {
                    out[0..8].copy_from_slice(&c.start.to_le_bytes());
                    out[8..16].copy_from_slice(&c.end.to_le_bytes());
                    out[16..20].copy_from_slice(&c.typ.to_le_bytes());
                    out[20..24].copy_from_slice(&c.pid.to_le_bytes());
                    1
                }
                _ => 0,
            },
            call::TEST => self.would_grant(&LockReq::Set { file: d.file, owner, pid: d.pid, typ: d.typ as u32, start: d.start, end: d.end }) as i64,
            call::TEST_FLOCK => self.would_grant(&LockReq::Flock { file: d.file, key: key(d.owner), op: d.typ }) as i64,
            call::RELEASE => {
                self.release(key(d.owner));
                0
            }
            call::RELEASE_PID_FILE => {
                self.release_pid(d.pid, Some(&d.file));
                0
            }
            call::RELEASE_PID => {
                self.release_pid(d.pid, None);
                0
            }
            call::WROTE => note_written(&d.file) as i64,
            call::WRITTEN => written(&d.file) as i64,
            _ => -(Errno::EINVAL.0 as i64),
        }
    }
}

// -- write generations -------------------------------------------------------
//
// How many times any process has written a shared mapping's pages back to a
// file (by path): what a mapping of it in another process compares with the
// count it last read the file at, for a file whose filesystem keeps no change
// count of its own (a host directory natively), so that taking a lock costs
// a look at one number when nobody else wrote ([`crate::syscalls::sync_shared_file`]).
// Natively one table for the whole run (every process is in this host
// process); in the page the global kernel's ([`share_locks`]).

static WRITES: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// Write-backs of any file, all told: what a process with shared mappings
/// compares at each system call's entry ([`crate::syscalls::SharedMapView::catch_up`]).
static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many write-backs of shared mappings there have been, of any file.
pub fn write_seq() -> u64 {
    if let Some(r) = FileLocks::remote() {
        return (r.seq)();
    }
    WRITE_SEQ.load(Ordering::Acquire)
}

/// A process wrote mapped pages of `file` back: the file's generation now.
pub fn note_written(file: &str) -> u64 {
    if let Some(r) = FileLocks::remote() {
        return (r.call)(&encode(call::WROTE, 0, 0, 0, 0, 0, file), &mut []).max(0) as u64;
    }
    let mut w = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    WRITE_SEQ.fetch_add(1, Ordering::AcqRel);
    match w.iter_mut().find(|(f, _)| f == file) {
        Some((_, n)) => {
            *n += 1;
            *n
        }
        None => {
            w.push((file.to_string(), 1));
            1
        }
    }
}

/// How many times mapped pages of `file` have been written back (0: never).
pub fn written(file: &str) -> u64 {
    if let Some(r) = FileLocks::remote() {
        return (r.call)(&encode(call::WRITTEN, 0, 0, 0, 0, 0, file), &mut []).max(0) as u64;
    }
    let w = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    w.iter().find(|(f, _)| f == file).map_or(0, |(_, n)| *n)
}

/// The change count a granted remote lock call answered (0: none).
fn seen_count(v: i64, out: &[u8; 8]) -> Option<u64> {
    let c = u64::from_le_bytes(*out);
    (v >= 0 && c != 0).then_some(c)
}

fn remote_answer(v: i64) -> SysResult {
    if v < 0 {
        Err(Errno((-v) as i32))
    } else {
        Ok(v as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const W: u32 = lk::WRLCK;
    const R: u32 = lk::RDLCK;
    const U: u32 = lk::UNLCK;
    const EOF: u64 = u64::MAX;

    #[test]
    fn an_exclusive_flock_excludes_another_description() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::EX), Ok(0));
        assert_eq!(l.flock("/a", 2, lock_op::EX | lock_op::NB), Err(Errno::EWOULDBLOCK));
        assert_eq!(l.flock("/a", 2, lock_op::SH | lock_op::NB), Err(Errno::EWOULDBLOCK));
        l.flock("/a", 1, lock_op::UN).unwrap();
        assert_eq!(l.flock("/a", 2, lock_op::EX), Ok(0));
    }

    #[test]
    fn flocks_on_other_files_and_shared_ones_coexist() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::EX), Ok(0));
        assert_eq!(l.flock("/b", 2, lock_op::EX), Ok(0));
        assert_eq!(l.flock("/c", 1, lock_op::SH), Ok(0));
        assert_eq!(l.flock("/c", 2, lock_op::SH), Ok(0));
        assert_eq!(l.flock("/c", 3, lock_op::EX | lock_op::NB), Err(Errno::EWOULDBLOCK));
    }

    #[test]
    fn the_same_description_may_relock_and_convert() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::SH), Ok(0));
        assert_eq!(l.flock("/a", 1, lock_op::EX), Ok(0), "converting is not contention");
        assert_eq!(l.flock("/a", 2, lock_op::SH | lock_op::NB), Err(Errno::EWOULDBLOCK));
    }

    #[test]
    fn closing_a_description_releases_and_counts() {
        let l = FileLocks::new();
        l.flock("/a", 1, lock_op::EX).unwrap();
        l.set("/b", Owner::Desc(1), -1, W, 0, 10, false).unwrap();
        let n = l.releases();
        l.release(1);
        assert_eq!(l.releases(), n + 1);
        assert_eq!(l.flock("/a", 2, lock_op::EX), Ok(0));
        assert_eq!(l.set("/b", Owner::Pid(7), 7, W, 0, 10, false), Ok(0));
    }

    #[test]
    fn record_locks_conflict_only_where_they_overlap_and_one_writes() {
        let l = FileLocks::new();
        let (a, b) = (Owner::Pid(1), Owner::Pid(2));
        assert_eq!(l.set("/f", a, 1, W, 0, 10, false), Ok(0));
        assert_eq!(l.set("/f", b, 2, W, 10, 20, false), Ok(0), "adjacent");
        assert_eq!(l.set("/f", b, 2, R, 5, 6, false), Err(Errno::EAGAIN));
        assert_eq!(l.set("/f", a, 1, R, 0, 10, false), Ok(0), "its own: converts");
        assert_eq!(l.set("/f", b, 2, R, 5, 6, false), Ok(0), "two readers");
        assert_eq!(l.set("/f", b, 2, W, 5, 6, false), Err(Errno::EAGAIN));
        assert_eq!(l.set("/g", b, 2, W, 0, EOF, false), Ok(0), "another file");
    }

    #[test]
    fn a_partial_unlock_splits_and_neighbours_merge() {
        let l = FileLocks::new();
        let a = Owner::Pid(1);
        l.set("/f", a, 1, W, 0, 100, false).unwrap();
        l.set("/f", a, 1, U, 40, 60, false).unwrap();
        let t = l.table();
        let mut got: Vec<_> = t.ranges.iter().map(|r| (r.start, r.end)).collect();
        got.sort();
        assert_eq!(got, vec![(0, 40), (60, 100)]);
        drop(t);
        assert_eq!(l.set("/f", Owner::Pid(2), 2, W, 45, 55, false), Ok(0), "the hole is free");
        l.set("/f", Owner::Pid(2), 2, U, 0, EOF, false).unwrap();
        l.set("/f", a, 1, W, 40, 60, false).unwrap();
        let t = l.table();
        assert_eq!(t.ranges.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(), vec![(0, 100)], "merged back");
    }

    #[test]
    fn getlk_reports_the_holder_and_pid() {
        let l = FileLocks::new();
        l.set("/f", Owner::Pid(9), 9, W, 10, 20, false).unwrap();
        l.set("/f", Owner::Desc(5), -1, R, 30, EOF, false).unwrap();
        assert_eq!(l.get("/f", Owner::Pid(1), R, 0, 15), Some(Conflict { typ: W, start: 10, end: 20, pid: 9 }));
        assert_eq!(l.get("/f", Owner::Pid(1), R, 30, 40), None, "readers share");
        assert_eq!(l.get("/f", Owner::Pid(1), W, 35, 36), Some(Conflict { typ: R, start: 30, end: EOF, pid: -1 }));
        assert_eq!(l.get("/f", Owner::Pid(9), W, 10, 20), None, "its own");
    }

    #[test]
    fn a_process_loses_its_record_locks_on_a_file_or_all_at_exit() {
        let l = FileLocks::new();
        l.set("/f", Owner::Pid(1), 1, W, 0, EOF, false).unwrap();
        l.set("/g", Owner::Pid(1), 1, W, 0, EOF, false).unwrap();
        l.set("/f", Owner::Desc(3), -1, W, 0, 0x10, false).unwrap_err();
        l.release_pid(1, Some("/f"));
        assert_eq!(l.set("/f", Owner::Desc(3), -1, W, 0, 10, false), Ok(0));
        assert_eq!(l.set("/g", Owner::Pid(2), 2, W, 0, 10, false), Err(Errno::EAGAIN));
        l.release_pid(1, None);
        assert_eq!(l.set("/g", Owner::Pid(2), 2, W, 0, 10, false), Ok(0));
    }

    #[test]
    fn two_processes_waiting_on_each_other_deadlock() {
        let l = FileLocks::new();
        l.set("/f", Owner::Pid(1), 1, W, 0, 1, false).unwrap();
        l.set("/f", Owner::Pid(2), 2, W, 1, 2, false).unwrap();
        assert_eq!(l.set("/f", Owner::Pid(1), 1, W, 1, 2, true), Err(Errno::EAGAIN), "1 waits for 2");
        assert_eq!(l.set("/f", Owner::Pid(2), 2, W, 0, 1, true), Err(Errno::EDEADLK), "2 would wait for 1");
        assert!(!l.would_grant(&LockReq::Set { file: "/f".into(), owner: Owner::Pid(1), pid: 1, typ: W, start: 1, end: 2 }));
        l.set("/f", Owner::Pid(2), 2, U, 0, EOF, false).unwrap();
        assert!(l.would_grant(&LockReq::Set { file: "/f".into(), owner: Owner::Pid(1), pid: 1, typ: W, start: 1, end: 2 }));
    }

    #[test]
    fn the_served_form_matches_the_table() {
        let g = FileLocks::new();
        let mut out = [0u8; 24];
        let b = encode(call::SET, Owner::Pid(4).encode(), 4, W as u64, 0, 8, "/f");
        assert_eq!(g.serve(4, &b, &mut out), 0);
        let b = encode(call::SET, Owner::Pid(5).encode(), 5, R as u64, 0, 8, "/f");
        assert_eq!(g.serve(5, &b, &mut out), -(Errno::EAGAIN.0 as i64));
        let b = encode(call::GET, Owner::Pid(5).encode(), 5, R as u64, 0, 8, "/f");
        assert_eq!(g.serve(5, &b, &mut out), 1);
        assert_eq!(i32::from_le_bytes(out[20..24].try_into().unwrap()), 4);
        // Description keys are the caller's: two processes' key 1 differ.
        assert_eq!(g.serve(4, &encode(call::FLOCK, 1, 0, lock_op::EX, 0, 0, "/h"), &mut []), 0);
        assert_eq!(g.serve(5, &encode(call::FLOCK, 1, 0, lock_op::EX, 0, 0, "/h"), &mut []), -(Errno::EWOULDBLOCK.0 as i64));
        g.serve(4, &encode(call::RELEASE, 1, 0, 0, 0, 0, ""), &mut []);
        assert_eq!(g.serve(5, &encode(call::FLOCK, 1, 0, lock_op::EX, 0, 0, "/h"), &mut []), 0);
    }
}
