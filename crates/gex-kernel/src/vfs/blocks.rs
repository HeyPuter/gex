//! File contents as blocks kept by the host (a page's origin private file
//! system: `OPFSFilesystem`), with a bounded cache of them here. The tree
//! (names, inodes, modes, times) stays in tmpfs. Off unless the host
//! configures it ([`configure`]) before files are made; a file made while it
//! is off keeps its bytes in a `Vec` as before, and so does one no longer than
//! what stays inline, and a seeded one (the image's, laid at boot) until a
//! program's write moves it here: boot writes nothing to the store.
//!
//! The host's half is one function ([`set_io`]): read or write `buf` at
//! `offset` of the one store file, or make that file at least so long. Block
//! numbers are allocated here, lowest free first; the index is each file's
//! block list.
//!
//! The cache is write-back: a block written stays here, dirty, until it is
//! evicted (CLOCK), it has been dirty for the host's write-back age
//! ([`tick`]), or a sync asks for it ([`sync_all`], [`PFile::sync`]); it is
//! written to the store with the dirty blocks next to it there, and a sync
//! or a tick that wrote anything then asks the store to make it durable
//! ([`OP_FLUSH`]). A read that misses fetches the block; a run of sequential
//! misses reads ahead, in one request. Every request's result is checked:
//! one the store could not make is EIO (ENOSPC when it was full), its block
//! kept dirty.
//!
//! A read or write of [`BYPASS`] bytes or more moves the whole blocks it
//! spans that are not held here straight between the store and the
//! caller's buffer, a run of them adjacent in the store in one request, and
//! keeps none of them: a big copy does not pass through the cache twice nor
//! push out what is in it. A direct write the store refuses goes through the
//! cache instead, so its data is kept as any other write's.
//!
//! CLOCK is scan-resistant: a block comes in unreferenced, and only a second
//! use (a later read or write of it, not the one that brought it in nor a
//! read running on within the same block) marks it referenced, and the hand
//! passes over referenced blocks while they are at most three quarters of
//! the cache: a scan of blocks used once cycles through the rest.

use crate::errno::Errno;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// `(op, offset, buf)`: done, or why not (EIO; ENOSPC for a full store).
pub type BlockIo = fn(u32, u64, &mut [u8]) -> Result<(), Errno>;
static IO: OnceLock<BlockIo> = OnceLock::new();
pub const OP_READ: u32 = 1;
pub const OP_WRITE: u32 = 2;
/// Make the store file at least `offset` bytes long (no buffer).
pub const OP_GROW: u32 = 4;
/// Make what was written durable (no offset, no buffer).
pub const OP_FLUSH: u32 = 8;

pub fn set_io(f: BlockIo) {
    let _ = IO.set(f);
}

/// A write of the store from bytes the host only reads: `(offset, bytes)`.
pub type BlockWrite = fn(u64, &[u8]) -> Result<(), Errno>;
static IO_WRITE: OnceLock<BlockWrite> = OnceLock::new();

/// The host's write that takes the caller's bytes as they are (no copy);
/// without one a write is copied into a buffer of its own for [`set_io`]'s.
pub fn set_io_write(f: BlockWrite) {
    let _ = IO_WRITE.set(f);
}

static BS: AtomicU32 = AtomicU32::new(0);
static BUDGET: AtomicU64 = AtomicU64::new(0);
static INLINE: AtomicU64 = AtomicU64::new(0);
/// Debugging (a switch, off by default): a sum of every block written to the
/// store, checked when it is read back ([`check_read`]).
static CHECK: AtomicBool = AtomicBool::new(false);
static SUMS: Mutex<Option<HashMap<u64, (u64, u64)>>> = Mutex::new(None);

/// Check every block read from the store against what was written there.
pub fn set_check(on: bool) {
    CHECK.store(on, Ordering::Relaxed);
}

fn sum(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in b.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn note_written(off: u64, buf: &[u8]) {
    let bs = block() as usize;
    let mut g = SUMS.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    for (i, c) in buf.chunks(bs).enumerate() {
        let e = m.entry(off + (i * bs) as u64).or_insert((0, 0));
        *e = (sum(c), e.0);
    }
}

fn check_read(off: u64, buf: &[u8]) {
    let bs = block() as usize;
    let g = SUMS.lock().unwrap();
    let Some(m) = g.as_ref() else { return };
    for (i, c) in buf.chunks(bs).enumerate() {
        let at = off + (i * bs) as u64;
        let h = sum(c);
        match m.get(&at) {
            Some(&(cur, _)) if cur == h => {}
            Some(&(_, prev)) => {
                if ST[BAD].fetch_add(1, Ordering::Relaxed) == 0 {
                    ST[FIRST_BAD].store(at / bs as u64 + 1, Ordering::Relaxed);
                    // What it holds instead: the block's previous contents
                    // (a write lost), another block's (a write misplaced),
                    // or neither.
                    let what = if prev == h {
                        1
                    } else if let Some((&o, _)) = m.iter().find(|(_, &(cur, _))| cur == h) {
                        2 + o / bs as u64
                    } else if h == sum(&vec![0u8; c.len()]) {
                        u64::MAX
                    } else {
                        0
                    };
                    ST[BAD_WHAT].store(what, Ordering::Relaxed);
                }
            }
            None => {
                if ST[UNWRITTEN].fetch_add(1, Ordering::Relaxed) == 0 {
                    ST[FIRST_UNWRITTEN].store(at / bs as u64 + 1, Ordering::Relaxed);
                }
            }
        }
    }
}

fn forget_sum(off: u64) {
    if CHECK.load(Ordering::Relaxed) {
        if let Some(m) = SUMS.lock().unwrap().as_mut() {
            m.remove(&off);
        }
    }
}
/// Readahead's largest run, in bytes.
const AHEAD_MAX: u64 = 1 << 20;
/// The shortest read or write that skips the cache for its whole blocks.
pub const BYPASS: u64 = 1 << 20;
/// The longest request such a read or write makes (a longer one is several):
/// the host's answer is a 32-bit signed count.
const DIRECT_MAX: u64 = 64 << 20;

/// Turn it on (`block` 0: off): blocks of `block` bytes, at most `cache`
/// bytes of them held here; files up to `inline` bytes keep their bytes in
/// the kernel's memory as before (a file moves to the store as it grows past
/// it).
pub fn configure(block: u32, cache: u64, inline: u64) {
    // Block 0 is the superblocks' (persist.rs).
    {
        let mut a = ALLOCATOR.lock().unwrap();
        a.0 = a.0.max(1);
    }
    INLINE.store(inline, Ordering::Relaxed);
    BUDGET.store(cache, Ordering::Relaxed);
    BS.store(block, Ordering::Release);
}

/// Whether a file `end` bytes long goes to the store.
pub fn wants(end: u64) -> bool {
    block() != 0 && end > INLINE.load(Ordering::Relaxed)
}

/// The block size, 0 while off.
pub fn block() -> u64 {
    BS.load(Ordering::Acquire) as u64
}

/// Counters, for the report: see [`stats`].
pub static ST: [AtomicU64; 20] = [const { AtomicU64::new(0) }; 20];
const HIT: usize = 0;
const MISS: usize = 1;
const IO_R: usize = 2;
const IO_RB: usize = 3;
const IO_W: usize = 4;
const IO_WB: usize = 5;
const EVICT: usize = 6;
const EVICT_DIRTY: usize = 7;
const AHEAD: usize = 8;
const ALLOC: usize = 9;
const FREED: usize = 10;
const HELD: usize = 11;
const CLUSTERED: usize = 12;
const BAD: usize = 13;
const UNWRITTEN: usize = 14;
const FIRST_BAD: usize = 15;
const FIRST_UNWRITTEN: usize = 16;
const BAD_WHAT: usize = 17;
const DIRECT_R: usize = 18;
const DIRECT_W: usize = 19;

/// hits, misses, store reads, bytes, store writes, bytes, evictions, dirty
/// evictions, read-ahead blocks, blocks allocated, freed, bytes held here,
/// dirty neighbours written with an evicted block; with the check on, blocks
/// read back wrong, read never written, and the first of each (+1); blocks
/// read and written straight to the caller ([`BYPASS`]).
pub fn stats() -> [u64; 20] {
    core::array::from_fn(|i| ST[i].load(Ordering::Relaxed))
}

fn bump(i: usize, n: u64) {
    ST[i].fetch_add(n, Ordering::Relaxed);
}

/// The one store file's block numbers: the next never used, and the freed.
/// The lowest freed block first, so that the store file grows only when
/// every block below its end is in use (a block above what was ever written
/// costs the origin's quota).
static ALLOCATOR: Mutex<(u32, std::collections::BinaryHeap<std::cmp::Reverse<u32>>)> = Mutex::new((0, std::collections::BinaryHeap::new()));
/// How many blocks long the store file has been made ([`OP_GROW`]): it is
/// grown ahead, [`GROW`] at a time, so that a write-back never extends it
/// (a sync access handle's write that has to grow the file can fail, and
/// says so only through its count).
static RESERVED: AtomicU64 = AtomicU64::new(0);
const GROW: u64 = 64 << 20;

const NONE: u64 = u64::MAX;

struct Slot {
    key: u64,
    valid: bool,
    dirty: bool,
    /// The [`tick`] it was made dirty in.
    since: u32,
    /// Brought in and not used again since (scan resistance: see the module).
    fresh: bool,
    off: u64,
    data: Box<[u8]>,
}

/// How many slots are marked used twice (CLOCK's protected set).
static PROTECTED: AtomicU64 = AtomicU64::new(0);

/// The write-back's clock: one per [`tick`].
static EPOCH: AtomicU32 = AtomicU32::new(0);

/// `f` on a slot's bytes and dirty bit, its age started when it becomes dirty.
fn on_slot<R>(s: &mut Slot, f: impl FnOnce(&mut [u8], &mut bool) -> R) -> R {
    let was = s.dirty;
    let Slot { data, dirty, .. } = &mut *s;
    let r = f(data, dirty);
    if s.dirty && !was {
        s.since = EPOCH.load(Ordering::Relaxed);
    }
    r
}

struct Cell {
    /// Someone is moving this slot to another block (choose a different one).
    busy: AtomicBool,
    /// CLOCK's referenced bit.
    used: AtomicBool,
    m: Mutex<Slot>,
}

#[derive(Default)]
struct Index {
    map: HashMap<u64, u32>,
    cells: Vec<Arc<Cell>>,
    hand: usize,
}

static INDEX: Mutex<Option<Index>> = Mutex::new(None);

fn index<R>(f: impl FnOnce(&mut Index) -> R) -> R {
    let mut g = INDEX.lock().unwrap();
    f(g.get_or_insert_with(Default::default))
}

/// Where a file's block is: its cache key and its place in the store.
#[derive(Clone, Copy)]
struct Loc {
    key: u64,
    off: u64,
}

fn io(op: u32, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let f = IO.get().ok_or(Errno::EIO)?;
    if op == OP_READ {
        bump(IO_R, 1);
        bump(IO_RB, buf.len() as u64);
    } else if op == OP_WRITE {
        bump(IO_W, 1);
        bump(IO_WB, buf.len() as u64);
    }
    f(op, off, buf)?;
    if CHECK.load(Ordering::Relaxed) {
        if op == OP_WRITE {
            note_written(off, buf);
        } else if op == OP_READ {
            check_read(off, buf);
        }
    }
    Ok(())
}

/// A write to the store from `buf` as it is (the host's [`set_io_write`]),
/// counted and checked as [`io`]'s.
fn write_from(off: u64, buf: &[u8]) -> Result<(), Errno> {
    let Some(w) = IO_WRITE.get() else {
        return io(OP_WRITE, off, &mut buf.to_vec());
    };
    bump(IO_W, 1);
    bump(IO_WB, buf.len() as u64);
    w(off, buf)?;
    if CHECK.load(Ordering::Relaxed) {
        note_written(off, buf);
    }
    Ok(())
}

/// How a block missing here is filled.
#[derive(Clone, Copy)]
enum Fill<'a> {
    /// From the store.
    Load,
    /// Zeros: a block just allocated.
    Zero,
    /// From these bytes (a read-ahead run's).
    From(&'a [u8]),
}

/// The block at `loc` held here (fetched or made as `fill` says), and `f`
/// run on its bytes and dirty bit. `touch`: this is a use of the block that
/// counts for CLOCK (a read-ahead's fill, a truncation, a read on within the
/// block last read are not).
fn with_block<R>(loc: Loc, fill: Fill<'_>, touch: bool, f: impl FnOnce(&mut [u8], &mut bool) -> R) -> Result<R, Errno> {
    let bs = block() as usize;
    loop {
        if let Some(cell) = index(|ix| ix.map.get(&loc.key).map(|&i| Arc::clone(&ix.cells[i as usize]))) {
            let mut s = cell.m.lock().unwrap();
            if s.key == loc.key && s.valid {
                if touch {
                    if s.fresh {
                        s.fresh = false;
                    } else if !cell.used.swap(true, Ordering::Relaxed) {
                        PROTECTED.fetch_add(1, Ordering::Relaxed);
                    }
                }
                bump(HIT, 1);
                return Ok(on_slot(&mut s, f));
            }
            // Moved on, or still arriving and failed: look again.
            continue;
        }
        // A miss: a slot for it (a new one under the budget, else CLOCK's
        // choice), held while it is filled.
        let budget = BUDGET.load(Ordering::Relaxed) as usize;
        let cell = index(|ix| {
            if ix.cells.len() * bs < budget.max(bs * 4) {
                let c = Arc::new(Cell {
                    busy: AtomicBool::new(true),
                    used: AtomicBool::new(false),
                    m: Mutex::new(Slot { key: NONE, valid: false, dirty: false, since: 0, fresh: false, off: 0, data: vec![0u8; bs].into_boxed_slice() }),
                });
                bump(HELD, bs as u64);
                ix.cells.push(Arc::clone(&c));
                return (c, ix.cells.len() as u32 - 1);
            }
            let n = ix.cells.len();
            let mut seen = 0;
            loop {
                let h = ix.hand % n;
                ix.hand = h + 1;
                seen += 1;
                let c = &ix.cells[h];
                if c.busy.load(Ordering::Relaxed) {
                    continue;
                }
                if c.used.load(Ordering::Relaxed) {
                    // A block used twice passes while those are at most
                    // three quarters of the cache (the rest is where blocks
                    // used once come and go); past that, or once the hand
                    // has gone round with nothing to take, it loses its
                    // mark and goes next time round.
                    if PROTECTED.load(Ordering::Relaxed) as usize * 4 <= n * 3 && seen <= n {
                        continue;
                    }
                    if c.used.swap(false, Ordering::Relaxed) {
                        PROTECTED.fetch_sub(1, Ordering::Relaxed);
                    }
                    continue;
                }
                c.busy.store(true, Ordering::Relaxed);
                return (Arc::clone(c), h as u32);
            }
        });
        let (cell, i) = cell;
        let mut s = cell.m.lock().unwrap();
        // The block it held goes: written back first while still findable
        // (a reader of it waits on this slot and reads it here).
        if s.key != NONE {
            bump(EVICT, 1);
            if s.dirty && s.valid {
                bump(EVICT_DIRTY, 1);
                if let Err(e) = write_back(&mut s, bs) {
                    cell.busy.store(false, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        let raced = index(|ix| {
            if s.key != NONE && ix.map.get(&s.key) == Some(&i) {
                ix.map.remove(&s.key);
            }
            if ix.map.contains_key(&loc.key) {
                return true;
            }
            ix.map.insert(loc.key, i);
            false
        });
        s.key = NONE;
        s.valid = false;
        if raced {
            // Another miss of the same block took it first: use that one.
            cell.busy.store(false, Ordering::Relaxed);
            continue;
        }
        s.key = loc.key;
        s.off = loc.off;
        bump(MISS, 1);
        let ok = match fill {
            Fill::Zero => {
                s.data.fill(0);
                Ok(())
            }
            Fill::From(b) => {
                s.data[..b.len()].copy_from_slice(b);
                s.data[b.len()..].fill(0);
                Ok(())
            }
            Fill::Load => io(OP_READ, loc.off, &mut s.data),
        };
        if let Err(e) = ok {
            s.key = NONE;
            index(|ix| {
                if ix.map.get(&loc.key) == Some(&i) {
                    ix.map.remove(&loc.key);
                }
            });
            cell.busy.store(false, Ordering::Relaxed);
            return Err(e);
        }
        s.valid = true;
        s.dirty = false;
        // In unreferenced: its first use is this one, or for a read-ahead's
        // block the read that reaches it.
        s.fresh = !touch;
        cell.busy.store(false, Ordering::Relaxed);
        cell.used.store(false, Ordering::Relaxed);
        return Ok(on_slot(&mut s, f));
    }
}

/// Write back the dirty block in `s`, and with it, in the same request, the
/// dirty blocks held here that sit next to it in the store (up to the
/// read-ahead's run either way), when their slots are free to take now.
fn write_back(s: &mut Slot, bs: usize) -> Result<(), Errno> {
    let (key, off) = (s.key, s.off);
    let max = (AHEAD_MAX as usize / bs).max(1) as u64 - 1;
    let near = |k: i64| -> Option<Arc<Cell>> {
        let nk = key.checked_add_signed(k)?;
        index(|ix| ix.map.get(&nk).map(|&i| Arc::clone(&ix.cells[i as usize])))
    };
    let fits = |n: &Slot, k: i64| n.valid && n.dirty && n.key == key.wrapping_add_signed(k) && Some(n.off) == off.checked_add_signed(k * bs as i64);
    let mut before = Vec::new();
    for k in 1..=max as i64 {
        match near(-k) {
            Some(c) => before.push(c),
            None => break,
        }
    }
    let mut after = Vec::new();
    for k in 1..=max.saturating_sub(before.len() as u64) as i64 {
        match near(k) {
            Some(c) => after.push(c),
            None => break,
        }
    }
    let mut lo = Vec::new();
    for (k, c) in before.iter().enumerate() {
        match c.m.try_lock() {
            Ok(g) if fits(&g, -(k as i64 + 1)) => lo.push(g),
            _ => break,
        }
    }
    let mut hi = Vec::new();
    for (k, c) in after.iter().enumerate() {
        match c.m.try_lock() {
            Ok(g) if fits(&g, k as i64 + 1) => hi.push(g),
            _ => break,
        }
    }
    if lo.is_empty() && hi.is_empty() {
        io(OP_WRITE, off, &mut s.data)?;
        s.dirty = false;
        return Ok(());
    }
    let n = lo.len() + 1 + hi.len();
    let mut buf = Vec::with_capacity(n * bs);
    for g in lo.iter().rev() {
        buf.extend_from_slice(&g.data);
    }
    buf.extend_from_slice(&s.data);
    for g in &hi {
        buf.extend_from_slice(&g.data);
    }
    bump(CLUSTERED, (n - 1) as u64);
    io(OP_WRITE, off - (lo.len() * bs) as u64, &mut buf)?;
    s.dirty = false;
    for g in lo.iter_mut().chain(hi.iter_mut()) {
        g.dirty = false;
    }
    Ok(())
}

/// Every slot held here.
fn cells() -> Vec<Arc<Cell>> {
    index(|ix| ix.cells.clone())
}

/// Write back what is dirty in `s`, if it is (with its neighbours).
fn clean(s: &mut Slot) -> Result<bool, Errno> {
    if !(s.valid && s.dirty && s.key != NONE) {
        return Ok(false);
    }
    write_back(s, block() as usize)?;
    Ok(true)
}

/// The store made durable, when it is on.
fn flush() -> Result<(), Errno> {
    if block() == 0 {
        return Ok(());
    }
    io(OP_FLUSH, 0, &mut [])
}

/// `sync(2)` and `syncfs`: every dirty block written to the store, then the
/// store made durable. A block that could not be written stays dirty, and
/// the first failure is the answer, after the rest were tried.
pub fn sync_all() -> Result<(), Errno> {
    if block() == 0 {
        return Ok(());
    }
    let mut first = Ok(());
    for c in cells() {
        let mut s = c.m.lock().unwrap();
        if let Err(e) = clean(&mut s) {
            first = first.and(Err(e));
        }
    }
    first.and(flush())
}

/// The periodic write-back, called by the host every so often: the blocks
/// dirty for `age` ticks or more are written to the store, and if any were,
/// the store is made durable. A slot busy now is left for the next tick.
/// The blocks written, or the first failure.
pub fn tick(age: u32) -> Result<u32, Errno> {
    let now = EPOCH.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if block() == 0 {
        return Ok(0);
    }
    let (mut n, mut first) = (0, Ok(()));
    for c in cells() {
        let Ok(mut s) = c.m.try_lock() else { continue };
        if s.dirty && now.wrapping_sub(s.since) >= age {
            match clean(&mut s) {
                Ok(true) => n += 1,
                Ok(false) => {}
                Err(e) => first = first.and(Err(e)),
            }
        }
    }
    first?;
    if n > 0 {
        flush()?;
    }
    Ok(n)
}

fn cached(key: u64) -> bool {
    index(|ix| ix.map.contains_key(&key))
}

/// Drop a block held here without writing it back (its file let it go).
fn forget(key: u64) {
    let Some(cell) = index(|ix| ix.map.remove(&key).map(|i| Arc::clone(&ix.cells[i as usize]))) else { return };
    let mut s = cell.m.lock().unwrap();
    if s.key == key {
        s.key = NONE;
        s.valid = false;
        s.dirty = false;
        if cell.used.swap(false, Ordering::Relaxed) {
            PROTECTED.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// A file's contents: its size and where each of its blocks is (0 a hole).
#[derive(Debug)]
pub struct PFile {
    pub size: u64,
    /// The store file's block number plus one.
    map: Vec<u32>,
    /// The last block read and the read-ahead run's length, in blocks.
    last: u64,
    ahead: u64,
}

impl PFile {
    pub fn new() -> PFile {
        PFile { size: 0, map: Vec::new(), last: u64::MAX, ahead: 0 }
    }

    fn loc(&self, b: u64) -> Loc {
        let bs = block();
        let p = (self.map[b as usize] - 1) as u64;
        Loc { key: p, off: p * bs }
    }

    pub fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<u64, Errno> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let bs = block();
        let end = (offset + buf.len() as u64).min(self.size);
        let direct = end - offset >= BYPASS;
        let mut at = offset;
        while at < end {
            let b = at / bs;
            let from = (at % bs) as usize;
            let n = ((end - at) as usize).min(bs as usize - from);
            let o = (at - offset) as usize;
            if self.map.get(b as usize).copied().unwrap_or(0) == 0 {
                buf[o..o + n].fill(0);
            } else {
                if direct && from == 0 {
                    let k = self.run(b, ((end - at) / bs).min(DIRECT_MAX / bs));
                    if k > 0 {
                        let len = (k * bs) as usize;
                        io(OP_READ, self.loc(b).off, &mut buf[o..o + len])?;
                        bump(DIRECT_R, k);
                        self.last = b + k - 1;
                        at += k * bs;
                        continue;
                    }
                }
                let loc = self.loc(b);
                if !cached(loc.key) {
                    self.ahead(b)?;
                }
                let touch = self.last != b;
                with_block(loc, Fill::Load, touch, |d, _| buf[o..o + n].copy_from_slice(&d[from..from + n]))?;
            }
            self.last = b;
            at += n as u64;
        }
        Ok(end - offset)
    }

    /// From block `b`, how many of the next `max` have a place, sit one
    /// after another in the store and are not held here: a run a direct
    /// read takes in one request.
    fn run(&self, b: u64, max: u64) -> u64 {
        let bs = block();
        let first = self.loc(b).off;
        let mut k = 0;
        while k < max {
            if self.map.get((b + k) as usize).copied().unwrap_or(0) == 0 {
                break;
            }
            let l = self.loc(b + k);
            if l.off != first + k * bs || cached(l.key) {
                break;
            }
            k += 1;
        }
        k
    }

    /// Blocks `b` onwards, up to `max` of them, wholly rewritten from
    /// `data` straight to the store: a run of them that sits one after
    /// another there (given places as needed), its copies held here
    /// dropped. How many were written; 0 when none could be (no place for
    /// `b`, or the store refused the write: the caller writes them through
    /// the cache, which keeps them until the store takes them).
    fn write_direct(&mut self, b: u64, max: u64, data: &[u8]) -> Result<u64, Errno> {
        let bs = block();
        self.alloc(b)?;
        let first = self.loc(b).off;
        let mut k = 1;
        while k < max {
            // Out of room part way: the run so far, and the rest through
            // the cache (which says so).
            if self.alloc(b + k).is_err() || self.loc(b + k).off != first + k * bs {
                break;
            }
            k += 1;
        }
        for i in 0..k {
            forget(self.loc(b + i).key);
        }
        if write_from(first, &data[..(k * bs) as usize]).is_err() {
            return Ok(0);
        }
        bump(DIRECT_W, k);
        Ok(k)
    }

    /// A miss at block `b`: after a read of `b - 1`, the run from `b` that is
    /// adjacent in the store and not held here, read in one request, its
    /// length doubling from one block to [`AHEAD_MAX`]; otherwise just `b`.
    fn ahead(&mut self, b: u64) -> Result<(), Errno> {
        let bs = block();
        let max = (AHEAD_MAX / bs).max(1);
        self.ahead = if b > 0 && self.last == b - 1 { (self.ahead * 2).clamp(1, max) } else { 0 };
        if self.ahead <= 1 {
            return Ok(());
        }
        let first = self.loc(b);
        let mut n = 1;
        while n < self.ahead {
            let k = b + n;
            if self.map.get(k as usize).copied().unwrap_or(0) == 0 {
                break;
            }
            let l = self.loc(k);
            if l.off != first.off + n * bs || cached(l.key) {
                break;
            }
            n += 1;
        }
        if n <= 1 {
            return Ok(());
        }
        let mut run = vec![0u8; (n * bs) as usize];
        io(OP_READ, first.off, &mut run)?;
        bump(AHEAD, n - 1);
        for i in 0..n {
            let l = self.loc(b + i);
            let part = &run[(i * bs) as usize..((i + 1) * bs) as usize];
            with_block(l, Fill::From(part), false, |_, _| ())?;
        }
        Ok(())
    }

    /// Block `b` given a place, when it has none.
    fn alloc(&mut self, b: u64) -> Result<bool, Errno> {
        if self.map.len() <= b as usize {
            self.map.resize(b as usize + 1, 0);
        }
        if self.map[b as usize] != 0 {
            return Ok(false);
        }
        bump(ALLOC, 1);
        match alloc_place() {
            Ok(p) => self.map[b as usize] = p + 1,
            Err(e) => {
                self.map[b as usize] = 0;
                return Err(e);
            }
        }
        Ok(true)
    }

    pub fn write(&mut self, offset: u64, buf: &[u8]) -> Result<u64, Errno> {
        let bs = block();
        let end = offset + buf.len() as u64;
        let mut direct = buf.len() as u64 >= BYPASS;
        let mut at = offset;
        while at < end {
            let b = at / bs;
            let from = (at % bs) as usize;
            let n = ((end - at) as usize).min(bs as usize - from);
            let o = (at - offset) as usize;
            if direct && from == 0 && end - at >= bs {
                let k = self.write_direct(b, ((end - at) / bs).min(DIRECT_MAX / bs), &buf[o..])?;
                if k > 0 {
                    self.last = b + k - 1;
                    at += k * bs;
                    continue;
                }
                direct = false;
            }
            let fresh = self.alloc(b)?;
            // A block wholly rewritten, or one with nothing yet, is not read.
            let whole = from == 0 && n == bs as usize;
            let fill = if fresh || whole { Fill::Zero } else { Fill::Load };
            let loc = self.loc(b);
            let touch = self.last != b;
            with_block(loc, fill, touch, |d, dirty| {
                d[from..from + n].copy_from_slice(&buf[o..o + n]);
                *dirty = true;
            })?;
            self.last = b;
            at += n as u64;
        }
        self.size = self.size.max(end);
        Ok(buf.len() as u64)
    }

    pub fn truncate(&mut self, len: u64) -> Result<(), Errno> {
        let bs = block();
        if len < self.size {
            let keep = len.div_ceil(bs) as usize;
            for b in keep..self.map.len() {
                self.free_block(b);
            }
            self.map.truncate(keep);
            // The last block's tail reads as zeros if the file grows again.
            if len % bs != 0 && self.map.get(keep - 1).copied().unwrap_or(0) != 0 {
                let from = (len % bs) as usize;
                let loc = self.loc(keep as u64 - 1);
                with_block(loc, Fill::Load, false, |d, dirty| {
                    d[from..].fill(0);
                    *dirty = true;
                })?;
            }
        }
        self.size = len;
        Ok(())
    }

    /// `fsync` and `fdatasync`: this file's dirty blocks written to the
    /// store, then the store made durable (the tree is the kernel's memory:
    /// it is kept with persistence, not here).
    pub fn sync(&self) -> Result<(), Errno> {
        let mut first = Ok(());
        for b in 0..self.map.len() {
            if self.map[b] == 0 {
                continue;
            }
            let key = self.loc(b as u64).key;
            let Some(cell) = index(|ix| ix.map.get(&key).map(|&i| Arc::clone(&ix.cells[i as usize]))) else { continue };
            let mut s = cell.m.lock().unwrap();
            if s.key == key {
                if let Err(e) = clean(&mut s) {
                    first = first.and(Err(e));
                }
            }
        }
        first.and(flush())
    }

    fn free_block(&mut self, b: usize) {
        if self.map[b] == 0 {
            return;
        }
        bump(FREED, 1);
        let loc = self.loc(b as u64);
        forget(loc.key);
        forget_sum(loc.off);
        free(self.map[b] - 1);
        self.map[b] = 0;
    }

    /// A file whose contents are the store's blocks `map` (each the block
    /// number plus one, 0 a hole), `size` bytes long: one a load kept
    /// (persist.rs). The blocks are counted as in use by [`rebuild`].
    pub fn from_blocks(size: u64, map: Vec<u32>) -> PFile {
        PFile { size, map, last: u64::MAX, ahead: 0 }
    }

    /// Where each block is in the store, plus one (0 a hole).
    pub fn blocks(&self) -> &[u32] {
        &self.map
    }
}

/// While the root is kept across loads (persist.rs), a block freed waits
/// here until the next generation is written: the last one written may
/// still name it.
static DEFER: AtomicBool = AtomicBool::new(false);
static PENDING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn free(p: u32) {
    if DEFER.load(Ordering::Relaxed) {
        PENDING.lock().unwrap().push(p);
    } else {
        ALLOCATOR.lock().unwrap().1.push(std::cmp::Reverse(p));
    }
}

/// Blocks kept back for persist.rs's records: on a full store, a delete's
/// blocks are free only once a generation that no longer names them is
/// written, and writing it needs blocks of its own. Files never take these.
static RESERVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
const RESERVE_BLOCKS: usize = 64;

/// The store's next block past the highest, the store grown ahead when it
/// has none (ENOSPC when it cannot grow).
fn grow_one(a: &mut (u32, std::collections::BinaryHeap<std::cmp::Reverse<u32>>)) -> Result<u32, Errno> {
    let bs = block();
    let have = RESERVED.load(Ordering::Relaxed);
    if a.0 as u64 >= have {
        // Grown ahead, so that a quota failure comes here, at allocation,
        // as ENOSPC, not at a write-back; one block at a time once the room
        // left is less than that.
        let mut want = have.max(a.0 as u64) + (GROW / bs).max(1);
        if io(OP_GROW, want * bs, &mut []).is_err() {
            want = a.0 as u64 + 1;
            if io(OP_GROW, want * bs, &mut []).is_err() {
                return Err(Errno::ENOSPC);
            }
        }
        RESERVED.store(want, Ordering::Relaxed);
    }
    a.0 += 1;
    Ok(a.0 - 1)
}

/// A free block, the lowest first, else a new one.
fn take(a: &mut (u32, std::collections::BinaryHeap<std::cmp::Reverse<u32>>)) -> Result<u32, Errno> {
    match a.1.pop() {
        Some(std::cmp::Reverse(p)) => Ok(p),
        None => grow_one(a),
    }
}

/// A place in the store for a file's block, the lowest free first, the
/// store grown ahead when there is none; the records' reserve kept full
/// first, while the root is kept.
fn alloc_place() -> Result<u32, Errno> {
    let mut a = ALLOCATOR.lock().unwrap();
    if DEFER.load(Ordering::Relaxed) {
        let mut r = RESERVE.lock().unwrap();
        while r.len() < RESERVE_BLOCKS {
            match take(&mut a) {
                Ok(p) => r.push(p),
                Err(_) => break,
            }
        }
    }
    take(&mut a)
}

/// For persist.rs: frees wait for the next generation from now on.
pub fn defer_frees() {
    DEFER.store(true, Ordering::Relaxed);
}

/// The blocks freed since the last call (persist.rs: a generation about to
/// be written no longer names them).
pub fn take_pending() -> Vec<u32> {
    std::mem::take(&mut *PENDING.lock().unwrap())
}

/// Blocks given back to the pending list (a generation that failed).
pub fn repend(v: Vec<u32>) {
    PENDING.lock().unwrap().extend(v);
}

/// Blocks free for use again now.
pub fn release(v: impl IntoIterator<Item = u32>) {
    let mut a = ALLOCATOR.lock().unwrap();
    for p in v {
        a.1.push(std::cmp::Reverse(p));
    }
}

/// A block for persist.rs's own records, outside the cache: the reserve's
/// when the store has no other.
pub fn alloc_meta() -> Result<u32, Errno> {
    let mut a = ALLOCATOR.lock().unwrap();
    take(&mut a).or_else(|e| RESERVE.lock().unwrap().pop().ok_or(e))
}

/// How many blocks of the store are in use or free below the highest ever
/// allocated (block 0 included).
pub fn high() -> u32 {
    ALLOCATOR.lock().unwrap().0
}

/// After a load's root was read back: `high` blocks long, those `used`
/// taken (block 0 always), the rest free.
pub fn rebuild(high: u32, used: &[bool]) {
    let mut a = ALLOCATOR.lock().unwrap();
    RESERVE.lock().unwrap().clear();
    a.0 = high.max(1);
    a.1.clear();
    for p in 1..a.0 {
        if !used.get(p as usize).copied().unwrap_or(false) {
            a.1.push(std::cmp::Reverse(p));
        }
    }
    RESERVED.store(RESERVED.load(Ordering::Relaxed).max(a.0 as u64), Ordering::Relaxed);
}

/// A test's fresh start: nothing held, nothing allocated, frees at once.
#[cfg(test)]
pub fn reset() {
    *INDEX.lock().unwrap() = None;
    PROTECTED.store(0, Ordering::Relaxed);
    *ALLOCATOR.lock().unwrap() = (1, Default::default());
    RESERVE.lock().unwrap().clear();
    PENDING.lock().unwrap().clear();
    DEFER.store(false, Ordering::Relaxed);
    RESERVED.store(0, Ordering::Relaxed);
}

/// The store read or written at `off` directly, not through the cache nor
/// the debug sums (persist.rs's superblocks and records).
pub fn raw(op: u32, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let f = IO.get().ok_or(Errno::EIO)?;
    f(op, off, buf)
}

/// Every dirty block written to the store, without the flush (persist.rs
/// flushes once for the data and its records).
pub fn write_back_all() -> Result<(), Errno> {
    if block() == 0 {
        return Ok(());
    }
    let mut first = Ok(());
    for c in cells() {
        let mut s = c.m.lock().unwrap();
        if let Err(e) = clean(&mut s) {
            first = first.and(Err(e));
        }
    }
    first
}

/// The store made durable.
pub fn flush_store() -> Result<(), Errno> {
    flush()
}

/// The write-back's clock moved on one tick: the new tick.
pub fn next_epoch() -> u32 {
    EPOCH.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// The write-back's clock now.
pub fn epoch() -> u32 {
    EPOCH.load(Ordering::Relaxed)
}

/// Whether a block held here has been dirty for `age` ticks or more.
pub fn aged(age: u32) -> bool {
    let now = epoch();
    cells().iter().any(|c| c.m.try_lock().is_ok_and(|s| s.valid && s.dirty && now.wrapping_sub(s.since) >= age))
}

impl Drop for PFile {
    fn drop(&mut self) {
        if block() == 0 {
            return;
        }
        for b in 0..self.map.len() {
            self.free_block(b);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vfs::{FileSystem, TmpFs};

    pub(crate) static STORE_FILE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    static FLUSHES: AtomicU32 = AtomicU32::new(0);
    /// The fake store refuses every write.
    static FAIL: AtomicBool = AtomicBool::new(false);

    pub(crate) fn fake(op: u32, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let mut f = STORE_FILE.lock().unwrap();
        let (a, b) = (off as usize, off as usize + buf.len());
        match op {
            OP_READ => {
                for (i, x) in buf.iter_mut().enumerate() {
                    *x = f.get(a + i).copied().unwrap_or(0);
                }
            }
            OP_WRITE if FAIL.load(Ordering::Relaxed) => return Err(Errno::EIO),
            OP_WRITE => {
                if f.len() < b {
                    f.resize(b, 0);
                }
                f[a..b].copy_from_slice(buf);
            }
            OP_GROW => {
                if f.len() < off as usize {
                    f.resize(off as usize, 0);
                }
            }
            OP_FLUSH => {
                FLUSHES.fetch_add(1, Ordering::Relaxed);
            }
            _ => return Err(Errno::EIO),
        }
        Ok(())
    }

    /// What a file's writes left in the cache reaches the store at its
    /// fsync, at sync, or once it has been dirty for the write-back's age,
    /// and each of those flushes the store; a tick with nothing old enough
    /// writes and flushes nothing.
    #[test]
    #[ignore]
    fn sync_and_the_write_back_age_reach_the_store() {
        set_io(fake);
        configure(4096, 64 * 4096, 0);
        let fs = TmpFs::new();
        fs.use_store();
        let o = crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR;
        let a = fs.open("a", o, 0o644).unwrap();
        let w0 = stats()[IO_W];
        a.write_at(0, &[1u8; 8192]).unwrap();
        assert_eq!(stats()[IO_W], w0, "a write stays in the cache");
        let f0 = FLUSHES.load(Ordering::Relaxed);
        a.sync(false).unwrap();
        assert_eq!(FLUSHES.load(Ordering::Relaxed), f0 + 1);
        assert!(stats()[IO_W] > w0);
        // Where file a's blocks are is the allocator's: find them by content.
        assert!(STORE_FILE.lock().unwrap().windows(8192).any(|w| w.iter().all(|&x| x == 1)));
        // The age: nothing at the first tick, the block at the second.
        let b = fs.open("b", o, 0o644).unwrap();
        b.write_at(0, &[2u8; 4096]).unwrap();
        let w1 = stats()[IO_W];
        assert_eq!(tick(2).unwrap(), 0);
        assert_eq!(stats()[IO_W], w1);
        assert!(tick(2).unwrap() >= 1);
        assert!(STORE_FILE.lock().unwrap().windows(4096).any(|w| w.iter().all(|&x| x == 2)));
        let f1 = FLUSHES.load(Ordering::Relaxed);
        assert_eq!(tick(2).unwrap(), 0, "nothing dirty is left");
        assert_eq!(FLUSHES.load(Ordering::Relaxed), f1);
        // sync(2): every file's.
        let c = fs.open("c", o, 0o644).unwrap();
        c.write_at(0, &[3u8; 4096]).unwrap();
        sync_all().unwrap();
        assert!(STORE_FILE.lock().unwrap().windows(4096).any(|w| w.iter().all(|&x| x == 3)));
        for n in ["a", "b", "c"] {
            fs.unlink(n).unwrap();
        }
        configure(0, 0, 0);
    }

    /// Only a tmpfs that uses the store (the root) puts files there: one
    /// mounted beside it (`/dev/shm`, a program's own) keeps them in memory.
    #[test]
    #[ignore]
    fn only_the_root_uses_the_store() {
        set_io(fake);
        reset();
        configure(4096, 16 * 4096, 0);
        let o = crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR;
        let shm = TmpFs::new();
        let a0 = stats()[ALLOC];
        shm.open("seg", o, 0o600).unwrap().write_at(0, &[1u8; 3 * 4096]).unwrap();
        assert_eq!(stats()[ALLOC], a0, "in memory");
        let root = TmpFs::new();
        root.use_store();
        root.open("f", o, 0o644).unwrap().write_at(0, &[2u8; 3 * 4096]).unwrap();
        assert_eq!(stats()[ALLOC], a0 + 3, "in the store");
        shm.unlink("seg").unwrap();
        root.unlink("f").unwrap();
        configure(0, 0, 0);
    }

    /// A read or write of a MiB or more moves its whole blocks straight
    /// between the store and the caller, a block held here (dirty) read from
    /// here in the middle of it; the ragged ends go through the cache; a
    /// direct write the store refuses is kept in the cache instead, and
    /// reaches the store once it takes writes again.
    #[test]
    #[ignore]
    fn big_reads_and_writes_skip_the_cache() {
        set_io(fake);
        reset();
        configure(4096, 400 * 4096, 0);
        let bs = 4096usize;
        let len = (2 << 20) + 100;
        let mut want: Vec<u8> = (0..len).map(|i| (i * 13 + i / 4096) as u8).collect();
        let mut f = PFile::new();
        let (w0, m0) = (stats()[DIRECT_W], stats()[MISS]);
        f.write(0, &want).unwrap();
        assert_eq!(stats()[DIRECT_W] - w0, 512, "the whole blocks, straight to the store");
        assert_eq!(stats()[MISS] - m0, 1, "the ragged end, through the cache");
        // Block 2 rewritten in part: held here, dirty.
        f.write(2 * bs as u64 + 10, &[0xee; 100]).unwrap();
        want[2 * bs + 10..2 * bs + 110].fill(0xee);
        let (r0, h0) = (stats()[DIRECT_R], stats()[HELD]);
        let mut got = vec![0u8; len];
        assert_eq!(f.read(0, &mut got).unwrap() as usize, len);
        assert!(got == want, "a direct read and the dirty block held here");
        assert_eq!(stats()[DIRECT_R] - r0, 511);
        assert_eq!(stats()[HELD], h0, "nothing new held");
        // In small pieces, through the cache: the same bytes.
        let mut at = 0;
        while at < len {
            let mut b = vec![0u8; 7000.min(len - at)];
            let n = f.read(at as u64, &mut b).unwrap() as usize;
            assert!(b[..n] == want[at..at + n], "at {at}");
            at += n;
        }
        // A refusing store: the write is kept here, and goes out at a sync.
        FAIL.store(true, Ordering::Relaxed);
        let more: Vec<u8> = (0..1 << 20).map(|i| (i * 7) as u8).collect();
        let w1 = stats()[DIRECT_W];
        f.write(4 << 20, &more).unwrap();
        assert_eq!(stats()[DIRECT_W], w1);
        assert!(sync_all().is_err(), "the store still refuses");
        FAIL.store(false, Ordering::Relaxed);
        sync_all().unwrap();
        // Dropped from the cache: read back from the store, directly.
        for b in 0..f.map.len() {
            if f.map[b] != 0 {
                forget(f.loc(b as u64).key);
            }
        }
        let mut back = vec![0u8; 1 << 20];
        let r1 = stats()[DIRECT_R];
        f.read(4 << 20, &mut back).unwrap();
        assert!(back == more);
        assert_eq!(stats()[DIRECT_R] - r1, 256);
        drop(f);
        configure(0, 0, 0);
    }

    /// A block used twice stays held while a file four times the cache is
    /// read through three times in small pieces: the scan's blocks come and
    /// go in the rest of the cache.
    #[test]
    #[ignore]
    fn a_scan_does_not_push_out_what_is_used() {
        set_io(fake);
        reset();
        configure(4096, 16 * 4096, 0);
        let mut hot = PFile::new();
        hot.write(0, &[5u8; 2 * 4096]).unwrap();
        let mut cold = PFile::new();
        let data: Vec<u8> = (0..64 * 4096).map(|i| i as u8).collect();
        cold.write(0, &data).unwrap();
        let mut b = vec![0u8; 4096];
        // Used, and used again: block 0, then 1, then 0.
        for blk in [0u64, 1, 0] {
            hot.read(blk * 4096, &mut b).unwrap();
        }
        for round in 0..3 {
            let mut at = 0;
            while at < data.len() {
                let mut p = vec![0u8; 3000];
                let n = cold.read(at as u64, &mut p).unwrap() as usize;
                assert!(p[..n] == data[at..at + n], "round {round} at {at}");
                at += n;
            }
        }
        let m = stats()[MISS];
        hot.read(0, &mut b).unwrap();
        assert_eq!(stats()[MISS], m, "the block used twice is still held");
        assert!(b.iter().all(|&x| x == 5));
        drop((hot, cold));
        configure(0, 0, 0);
    }

    /// Files written, rewritten in part, truncated and read back through a
    /// cache much smaller than them match a plain copy. Run alone: the
    /// switch is global (`cargo test -p gex-kernel --lib vfs::blocks -- --ignored --test-threads=1`).
    #[test]
    #[ignore]
    fn blocks_round_trip_through_a_small_cache() {
        for inline in [0, 5000] {
            set_io(fake);
            configure(4096, 8 * 4096, inline);
            let fs = TmpFs::new();
            fs.use_store();
            let mut want: Vec<Vec<u8>> = Vec::new();
            let mut x: u64 = 7;
            let mut rnd = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            for i in 0..6 {
                let len = 1000 + (rnd() % 60000) as usize;
                let d: Vec<u8> = (0..len).map(|j| (j * 7 + i) as u8).collect();
                let h = fs.open(&format!("f{i}"), crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR, 0o644).unwrap();
                h.write_at(0, &d).unwrap();
                want.push(d);
            }
            for _ in 0..400 {
                let i = (rnd() % 6) as usize;
                let h = fs.open(&format!("f{i}"), crate::fs::oflags::O_RDWR, 0).unwrap();
                match rnd() % 4 {
                    0 => {
                        let at = (rnd() % 70000) as usize;
                        let n = (rnd() % 9000) as usize;
                        let b: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
                        h.write_at(at as u64, &b).unwrap();
                        if want[i].len() < at + n {
                            want[i].resize(at + n, 0);
                        }
                        want[i][at..at + n].copy_from_slice(&b);
                    }
                    1 => {
                        let n = (rnd() % 70000) as usize;
                        h.truncate(n as u64).unwrap();
                        want[i].resize(n, 0);
                    }
                    _ => {
                        let at = (rnd() % 70000) as usize;
                        let mut b = vec![0u8; (rnd() % 20000) as usize];
                        let got = h.read_at(at as u64, &mut b).unwrap() as usize;
                        let w = &want[i];
                        let exp = w.len().saturating_sub(at).min(b.len());
                        assert_eq!(got, exp);
                        assert!(got == 0 || b[..got] == w[at..at + got], "f{i} at {at}");
                    }
                }
                assert_eq!(h.metadata().unwrap().size as usize, want[i].len());
            }
            for i in 0..6 {
                fs.unlink(&format!("f{i}")).unwrap();
            }
            configure(0, 0, 0);
        }
        let s = stats();
        assert!(s[EVICT_DIRTY] > 0 && s[MISS] > 0 && s[CLUSTERED] > 0, "{s:?}");
    }

    /// Several threads at once, each with files of its own made, rewritten,
    /// read, renamed over and removed, through a cache much smaller than
    /// them: every read matches a plain copy (blocks freed by one thread are
    /// reused by another).
    #[test]
    #[ignore]
    fn threads_share_a_small_cache() {
        set_check(true);
        for inline in [0, 3000] {
            set_io(fake);
            configure(4096, 12 * 4096, inline);
            let fs = std::sync::Arc::new(TmpFs::new());
            fs.use_store();
            let ts: Vec<_> = (0..8u64)
                .map(|t| {
                    let fs = std::sync::Arc::clone(&fs);
                    std::thread::spawn(move || {
                        let mut x: u64 = 0x9e37_79b9 ^ (t + 1) * 0x1234_5678_9abc;
                        let mut rnd = move || {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            x
                        };
                        let mut want: Vec<Vec<u8>> = vec![Vec::new(); 3];
                        for i in 0..3 {
                            fs.open(&format!("t{t}f{i}"), crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR, 0o644).unwrap();
                        }
                        for step in 0..600 {
                            let i = (rnd() % 3) as usize;
                            let name = format!("t{t}f{i}");
                            match rnd() % 6 {
                                0 | 1 => {
                                    let h = fs.open(&name, crate::fs::oflags::O_RDWR, 0).unwrap();
                                    let at = (rnd() % 40000) as usize;
                                    let n = (rnd() % 12000) as usize;
                                    let b: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
                                    h.write_at(at as u64, &b).unwrap();
                                    if want[i].len() < at + n {
                                        want[i].resize(at + n, 0);
                                    }
                                    want[i][at..at + n].copy_from_slice(&b);
                                }
                                2 => {
                                    // A new copy written aside and renamed over (dpkg's way).
                                    let n = (rnd() % 50000) as usize;
                                    let b: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
                                    let tmp = format!("{name}.new");
                                    let h = fs.open(&tmp, crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR | crate::fs::oflags::O_TRUNC, 0o644).unwrap();
                                    let mut at = 0;
                                    while at < n {
                                        let k = (n - at).min(1 + (rnd() % 9000) as usize);
                                        h.write_at(at as u64, &b[at..at + k]).unwrap();
                                        at += k;
                                    }
                                    drop(h);
                                    fs.rename(&tmp, &name).unwrap();
                                    want[i] = b;
                                }
                                3 => {
                                    let h = fs.open(&name, crate::fs::oflags::O_RDWR, 0).unwrap();
                                    let n = (rnd() % 45000) as usize;
                                    h.truncate(n as u64).unwrap();
                                    want[i].resize(n, 0);
                                }
                                _ => {
                                    let h = fs.open(&name, crate::fs::oflags::O_RDONLY, 0).unwrap();
                                    let w = &want[i];
                                    let mut at = 0usize;
                                    // Read through, sequentially, in odd pieces (read-ahead).
                                    while at < w.len() {
                                        let mut b = vec![0u8; 1 + (rnd() % 7000) as usize];
                                        let got = h.read_at(at as u64, &mut b).unwrap() as usize;
                                        assert!(got > 0, "t{t} f{i} step {step} at {at}");
                                        assert!(b[..got] == w[at..at + got], "t{t} f{i} step {step} at {at} len {}", w.len());
                                        at += got;
                                    }
                                }
                            }
                        }
                        for i in 0..3 {
                            fs.unlink(&format!("t{t}f{i}")).unwrap();
                        }
                    })
                })
                .collect();
            for t in ts {
                t.join().unwrap();
            }
            configure(0, 0, 0);
        }
        let s = stats();
        assert!(s[BAD] == 0 && s[UNWRITTEN] == 0, "{s:?}");
        set_check(false);
    }
}
