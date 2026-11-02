//! File contents as blocks kept by the host (a page's origin private file
//! system: `OPFSFilesystem`), with a bounded cache of them here. The tree
//! (names, inodes, modes, times) stays in tmpfs. Off unless the host
//! configures it ([`configure`]) before files are made; a file made while it
//! is off keeps its bytes in a `Vec` as before, and so does one no longer than
//! what stays inline.
//!
//! The host's half is one function ([`set_io`]): read or write `buf` at
//! `offset` of the one store file, or make that file at least so long. Block
//! numbers are allocated here, lowest free first; the index is each file's
//! block list.
//!
//! The cache is write-back: a block written stays here, dirty, until it is
//! evicted (CLOCK), when it is written to the store, with the dirty blocks
//! next to it there. A read that misses fetches the block; a run of
//! sequential misses reads ahead, in one request. Every request's result is
//! checked: one the store could not make is EIO, its block kept dirty.

use crate::errno::Errno;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// `(op, offset, buf)`: true when done.
pub type BlockIo = fn(u32, u64, &mut [u8]) -> bool;
static IO: OnceLock<BlockIo> = OnceLock::new();
pub const OP_READ: u32 = 1;
pub const OP_WRITE: u32 = 2;
/// Make the store file at least `offset` bytes long (no buffer).
pub const OP_GROW: u32 = 4;

pub fn set_io(f: BlockIo) {
    let _ = IO.set(f);
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

/// Turn it on (`block` 0: off): blocks of `block` bytes, at most `cache`
/// bytes of them held here; files up to `inline` bytes keep their bytes in
/// the kernel's memory as before (a file moves to the store as it grows past
/// it).
pub fn configure(block: u32, cache: u64, inline: u64) {
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
pub static ST: [AtomicU64; 18] = [const { AtomicU64::new(0) }; 18];
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

/// hits, misses, store reads, bytes, store writes, bytes, evictions, dirty
/// evictions, read-ahead blocks, blocks allocated, freed, bytes held here,
/// dirty neighbours written with an evicted block; with the check on, blocks
/// read back wrong, read never written, and the first of each (+1).
pub fn stats() -> [u64; 18] {
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
    off: u64,
    data: Box<[u8]>,
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
    if !f(op, off, buf) {
        return Err(Errno::EIO);
    }
    if CHECK.load(Ordering::Relaxed) {
        if op == OP_WRITE {
            note_written(off, buf);
        } else if op == OP_READ {
            check_read(off, buf);
        }
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
/// run on its bytes and dirty bit.
fn with_block<R>(loc: Loc, fill: Fill<'_>, f: impl FnOnce(&mut [u8], &mut bool) -> R) -> Result<R, Errno> {
    let bs = block() as usize;
    loop {
        if let Some(cell) = index(|ix| ix.map.get(&loc.key).map(|&i| Arc::clone(&ix.cells[i as usize]))) {
            let mut s = cell.m.lock().unwrap();
            if s.key == loc.key && s.valid {
                cell.used.store(true, Ordering::Relaxed);
                bump(HIT, 1);
                let Slot { data, dirty, .. } = &mut *s;
                return Ok(f(data, dirty));
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
                    used: AtomicBool::new(true),
                    m: Mutex::new(Slot { key: NONE, valid: false, dirty: false, off: 0, data: vec![0u8; bs].into_boxed_slice() }),
                });
                bump(HELD, bs as u64);
                ix.cells.push(Arc::clone(&c));
                return (c, ix.cells.len() as u32 - 1);
            }
            loop {
                let h = ix.hand % ix.cells.len();
                ix.hand = h + 1;
                let c = &ix.cells[h];
                if c.busy.load(Ordering::Relaxed) {
                    continue;
                }
                if c.used.swap(false, Ordering::Relaxed) {
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
        cell.busy.store(false, Ordering::Relaxed);
        cell.used.store(true, Ordering::Relaxed);
        let Slot { data, dirty, .. } = &mut *s;
        return Ok(f(data, dirty));
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
        let mut at = offset;
        while at < end {
            let b = at / bs;
            let from = (at % bs) as usize;
            let n = ((end - at) as usize).min(bs as usize - from);
            let o = (at - offset) as usize;
            if self.map.get(b as usize).copied().unwrap_or(0) == 0 {
                buf[o..o + n].fill(0);
            } else {
                let loc = self.loc(b);
                if !cached(loc.key) {
                    self.ahead(b)?;
                }
                with_block(loc, Fill::Load, |d, _| buf[o..o + n].copy_from_slice(&d[from..from + n]))?;
            }
            self.last = b;
            at += n as u64;
        }
        Ok(end - offset)
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
            with_block(l, Fill::From(part), |_, _| ())?;
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
        let mut a = ALLOCATOR.lock().unwrap();
        self.map[b as usize] = match a.1.pop() {
            Some(std::cmp::Reverse(p)) => p + 1,
            None => {
                let bs = block();
                let have = RESERVED.load(Ordering::Relaxed);
                if a.0 as u64 >= have {
                    // Grown ahead, so that a quota failure comes here, at
                    // allocation, as ENOSPC, not at a write-back.
                    let want = have + (GROW / bs).max(1);
                    if io(OP_GROW, want * bs, &mut []).is_err() {
                        self.map[b as usize] = 0;
                        return Err(Errno::ENOSPC);
                    }
                    RESERVED.store(want, Ordering::Relaxed);
                }
                a.0 += 1;
                a.0
            }
        };
        Ok(true)
    }

    pub fn write(&mut self, offset: u64, buf: &[u8]) -> Result<u64, Errno> {
        let bs = block();
        let end = offset + buf.len() as u64;
        let mut at = offset;
        while at < end {
            let b = at / bs;
            let from = (at % bs) as usize;
            let n = ((end - at) as usize).min(bs as usize - from);
            let o = (at - offset) as usize;
            let fresh = self.alloc(b)?;
            // A block wholly rewritten, or one with nothing yet, is not read.
            let whole = from == 0 && n == bs as usize;
            let fill = if fresh || whole { Fill::Zero } else { Fill::Load };
            let loc = self.loc(b);
            with_block(loc, fill, |d, dirty| {
                d[from..from + n].copy_from_slice(&buf[o..o + n]);
                *dirty = true;
            })?;
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
                with_block(loc, Fill::Load, |d, dirty| {
                    d[from..].fill(0);
                    *dirty = true;
                })?;
            }
        }
        self.size = len;
        Ok(())
    }

    fn free_block(&mut self, b: usize) {
        if self.map[b] == 0 {
            return;
        }
        bump(FREED, 1);
        let loc = self.loc(b as u64);
        forget(loc.key);
        forget_sum(loc.off);
        ALLOCATOR.lock().unwrap().1.push(std::cmp::Reverse(self.map[b] - 1));
        self.map[b] = 0;
    }
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
mod tests {
    use super::*;
    use crate::vfs::{FileSystem, TmpFs};

    static STORE_FILE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    fn fake(op: u32, off: u64, buf: &mut [u8]) -> bool {
        let mut f = STORE_FILE.lock().unwrap();
        let (a, b) = (off as usize, off as usize + buf.len());
        match op {
            OP_READ => {
                for (i, x) in buf.iter_mut().enumerate() {
                    *x = f.get(a + i).copied().unwrap_or(0);
                }
            }
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
            _ => return false,
        }
        true
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
