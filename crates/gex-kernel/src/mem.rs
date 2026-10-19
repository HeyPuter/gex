//! The guest's virtual memory bookkeeping.
//!
//! gex *is* the allocator for the guest address space (PLAN §2.3), so `mmap`,
//! `brk` and `munmap` are ordinary data-structure operations over a list of
//! mappings — there is no host `mmap` involved and no page table to program.
//!
//! Protection bits are recorded, not enforced: wasm has no way to fault on a
//! stray load or store (PLAN §7.2). They still matter, because `/proc/self/maps`
//! reports them, translation invalidation keys off `PROT_EXEC`, and any access
//! the *kernel* performs on the guest's behalf does check them.

use crate::errno::{Errno, SysResult};
use gex_abi::{GUEST_LIMIT, MMAP_BOTTOM, MMAP_TOP};

pub const PAGE_SIZE: u64 = 4096;

/// `mmap` protection bits.
pub mod prot {
    pub const NONE: u32 = 0;
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const EXEC: u32 = 4;
}

/// `mmap` flag bits.
pub mod flags {
    pub const SHARED: u64 = 0x01;
    pub const PRIVATE: u64 = 0x02;
    pub const FIXED: u64 = 0x10;
    /// `MAP_FIXED_NOREPLACE`: at exactly this address, but fail with `EEXIST`
    /// rather than replace anything already mapped there.
    pub const FIXED_NOREPLACE: u64 = 0x100000;
    pub const ANONYMOUS: u64 = 0x20;
    pub const NORESERVE: u64 = 0x4000;
    /// x86-64's `MAP_32BIT`: below 2 GiB. In an M64 process it keeps an
    /// anonymous mapping out of the high arena (the guest GL library's command
    /// ring, whose address the browser host takes as 32 bits).
    pub const MAP_32BIT: u64 = 0x40;
    pub const STACK: u64 = 0x20000;
}

/// One contiguous mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    /// A short label for `/proc/self/maps`.
    pub name: String,
    /// Where these bytes can be *rebuilt* from: the file, and the offset
    /// within it that `start` corresponds to. Only a mapping that has never
    /// been writable has one, because only that one can be reconstructed by
    /// re-reading.
    pub origin: Option<(String, u64)>,
    /// Where these bytes were *loaded* from, whatever happened afterwards.
    ///
    /// Lets two processes that map the same library at the same address agree
    /// the code is the same and share the translations for it. Separate from
    /// `origin`: `origin` asks "can this be rebuilt by re-reading the file"
    /// (no, for an image segment with a zeroed tail, and no once `mprotect`
    /// has touched it), which is a different question from whether the code is
    /// the same code.
    pub code: Option<(String, u64)>,
}

impl Vma {
    /// Whether `next`, which starts where this ends, is the same mapping
    /// continued: the same protection and name, and its file offsets (if
    /// any) carrying on from this one's.
    fn joins(&self, next: &Vma) -> bool {
        let continues = |a: &Option<(String, u64)>, b: &Option<(String, u64)>| match (a, b) {
            (None, None) => true,
            (Some((x, o)), Some((y, p))) => x == y && o + (self.end - self.start) == *p,
            _ => false,
        };
        self.prot == next.prot && self.name == next.name && continues(&self.origin, &next.origin) && continues(&self.code, &next.code)
    }

    /// A piece of this mapping, with the origin offset moved to match.
    fn slice(&self, start: u64, end: u64, prot: u32) -> Vma {
        Vma {
            start,
            end,
            prot,
            name: self.name.clone(),
            origin: self
                .origin
                .as_ref()
                .map(|(id, off)| (id.clone(), off + (start - self.start))),
            code: self
                .code
                .as_ref()
                .map(|(id, off)| (id.clone(), off + (start - self.start))),
        }
    }
}

/// What a reader that does not own the map sees of it: each mapping's
/// range and protection, published after every change
/// (docs/kernel-locking.md §3.5). A futex's "is the word mapped" asks this,
/// from any thread, with no lock shared with the map's writers.
///
/// A reader on a view one change old can see a mapping a concurrent
/// `munmap` has just removed, or miss one a concurrent `mmap` is adding:
/// the window Linux also has between a lookup and the access.
///
/// Not kept in the single-threaded page build (wasm32 without atomics),
/// which has no other reader and must not pay for one: there
/// [`MapView::mapped`] is never asked, and the host reads the map itself.
pub struct MapView {
    cur: gex_atomic::rcu::Published<Vec<(u64, u64, u32)>>,
}

/// Reader slots in a view's epoch array, as the descriptor table's
/// ([`crate::file::FD_READER_SLOTS`]): one per page Worker.
const VIEW_READER_SLOTS: usize = crate::file::FD_READER_SLOTS;

/// Whether this build keeps [`MapView`]s: every build but the
/// single-threaded page's.
pub const KEEPS_VIEWS: bool = !cfg!(all(target_family = "wasm", not(target_feature = "atomics")));

impl MapView {
    fn new(vmas: &[Vma]) -> MapView {
        MapView { cur: gex_atomic::rcu::Published::new(Self::ranges(vmas), VIEW_READER_SLOTS) }
    }

    fn ranges(vmas: &[Vma]) -> Vec<(u64, u64, u32)> {
        if !KEEPS_VIEWS {
            return Vec::new();
        }
        vmas.iter().map(|v| (v.start, v.end, v.prot)).collect()
    }

    /// Whether `[addr, addr + len)` lies inside one mapping, of any
    /// protection, as [`MemoryMap::vmas`] would say.
    pub fn mapped(&self, addr: u64, len: u64) -> bool {
        let end = addr.saturating_add(len);
        self.cur.read(crate::sync::me() as usize, |r| {
            let i = r.partition_point(|&(s, _, _)| s <= addr);
            r[..i].last().is_some_and(|&(_, e, _)| end <= e)
        })
    }

    /// The protection of the mapping holding `addr`, as
    /// [`MemoryMap::vma_at`] would say; `None` where nothing is mapped.
    pub fn prot_at(&self, addr: u64) -> Option<u32> {
        self.cur.read(crate::sync::me() as usize, |r| {
            let i = r.partition_point(|&(s, _, _)| s <= addr);
            r[..i].last().filter(|&&(_, e, _)| addr < e).map(|&(_, _, p)| p)
        })
    }
}

/// A map's view, which a copy of the map does not share: a forked child's
/// changes are its own.
struct ViewCell(std::sync::Arc<MapView>);

impl Clone for ViewCell {
    fn clone(&self) -> ViewCell {
        ViewCell(std::sync::Arc::new(MapView {
            cur: gex_atomic::rcu::Published::new(self.0.cur.read(crate::sync::me() as usize, |r| r.clone()), VIEW_READER_SLOTS),
        }))
    }
}

impl std::fmt::Debug for ViewCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MapView")
    }
}

/// A process's memory map behind its own lock, the process's mm lock
/// (docs/kernel-locking.md §3.5): what a mapping call changes and every
/// reader that needs more than the published view ([`MapView`]) looks at.
/// Held for a leaf computation only, never across a trap back to JS.
///
/// A copy is a copy of the map, as a fork's or a vfork child's is: its own
/// lock and its own view. [`Mm::shared`] is the same map, for a holder of
/// the process's state outside its kernel.
pub struct Mm {
    map: std::sync::Arc<crate::sync::Mutex<MemoryMap>>,
    /// The map's published view, the same one for the map's life (`renew`
    /// keeps it): read without the lock.
    view: std::sync::Arc<MapView>,
}

impl Mm {
    pub fn new(map: MemoryMap) -> Mm {
        let view = std::sync::Arc::clone(map.view());
        Mm { map: std::sync::Arc::new(crate::sync::Mutex::ranked(map, crate::sync::rank::MM)), view }
    }

    pub fn lock(&self) -> crate::sync::MutexGuard<'_, MemoryMap> {
        self.map.lock()
    }

    /// The same map, not a copy.
    pub fn shared(&self) -> Mm {
        Mm { map: std::sync::Arc::clone(&self.map), view: std::sync::Arc::clone(&self.view) }
    }

    /// The map's published view.
    pub fn view(&self) -> &std::sync::Arc<MapView> {
        &self.view
    }

    /// Whether `other` is this same map (one [`Mm::shared`] the other).
    pub fn same(&self, other: &Mm) -> bool {
        std::sync::Arc::ptr_eq(&self.map, &other.map)
    }
}

impl Clone for Mm {
    fn clone(&self) -> Mm {
        let map = self.lock().clone();
        Mm::new(map)
    }
}

impl std::fmt::Debug for Mm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mm")
    }
}

/// What a host's `back` hook answers for a range it keeps as an island
/// (see `MemoryMap::grows_on_demand`).
pub const BACKED_ISLAND: u64 = u64::MAX;

/// The process's address space.
#[derive(Debug, Clone)]
pub struct MemoryMap {
    vmas: Vec<Vma>,
    /// What readers that do not own the map see of it ([`MapView`]).
    view: ViewCell,
    brk_start: u64,
    brk_cur: u64,
    /// Next address the top-down allocator will try.
    mmap_next: u64,
    /// Where the top-down allocator starts over when the cursor runs out:
    /// the holes `munmap` left above it (MMAP_TOP, or `set_mmap_top`).
    mmap_ceiling: u64,
    /// How far up the arena has been claimed, and who to ask for more.
    ///
    /// A guest in a browser lives in a `WebAssembly.Memory` allocated up to its
    /// highest used address, one per process, so the arena is claimed as needed
    /// and the memory grown to match. `None` is the native case: everything is
    /// available already. `arena` is the spans handed out, in order; addresses
    /// only ever come from inside them, because the same growing region also
    /// feeds the runtime's own heap.
    arena: Vec<(u64, u64)>,
    take: Option<fn(u64) -> u64>,
    /// Make the host's memory reach an address, for a mapping the guest
    /// placed itself rather than one the arena handed out.
    ///
    /// Called with the mapping's `[start, end)`. It answers where the
    /// shared cursor stood before (`None` when the memory would not grow).
    /// The cursor only moves up, atomically, so the bytes between that and
    /// `end` are this map's alone, and are recorded as arena for later
    /// mappings to use. A host may instead leave the cursor where it is and
    /// answer [`BACKED_ISLAND`], keeping the range as an island its cursor
    /// steps over (W3 in the browser): backed, the guest's for mappings placed
    /// there, and never arena.
    back: Option<fn(u64, u64) -> Option<u64>>,
    /// Bumped by every change to the map. `/proc/self/maps` is derived from the
    /// tree; a counter lets a cached rendering be reused unchanged instead of
    /// reformatting on every read (measured half of an X server's time in
    /// `core::fmt` before this).
    generation: u64,
    /// An M64 process's high arena, `[HIGH_ARENA_BASE, LIMIT64)`
    /// (docs/mem64.md §3): non-executable mappings without a fixed address go
    /// there. `None` for every M32 process, whose map behaves exactly as it
    /// always has.
    high: Option<(u64, u64)>,
    /// Ranges known to hold only zero bytes, sorted and disjoint: memory that
    /// has not been mapped with contents or writable since it was zeroed (a
    /// fresh process's, a reservation the kernel zeroed). Empty means nothing
    /// is known, which is always safe.
    ///
    /// Lets an anonymous `mmap` skip zeroing what is zero already, and a
    /// `fork` skip copying it. Wine under W3 reserves ~3 GiB of PROT_NONE
    /// address space at each process start and forks twice per child
    /// process: scanning and copying those reservations was 2-3 s of every
    /// Windows process start.
    ///
    /// Trusts the protection: a page that is never writable is never written.
    /// gex does not fault every store to a non-writable page (a wasm32
    /// program's are not checked at all), so a program that writes one
    /// anyway -- which faults on Linux -- may see that page zeroed by a
    /// later `mmap` over it, or not copied into a forked child.
    clean: Vec<(u64, u64)>,
    /// The span the last [`Self::extend_arena`] took, until the caller asks
    /// ([`Self::take_fresh`]): memory nothing has been in, so zero.
    fresh: Option<(u64, u64)>,
    /// Ranges a mapping without a fixed address is never placed in
    /// ([`Self::keep_fixed_only`]), sorted and disjoint.
    fixed_only: Vec<(u64, u64)>,
    /// The end of the span the cursor last handed this map (`extend_arena`):
    /// where a free tail is looked for. Not simply the highest span, which
    /// may be an island above the cursor (see `back`).
    take_end: u64,
    /// What the mapping calls leave for the host and for each other, under
    /// the same lock as the map they describe ([`MapLog`]).
    pub log: MapLog,
}

/// What a process's mapping calls note beside its map (docs/kernel-locking.md
/// §3.5, L12b): kept with the map, under the mm lock, so a mapping call needs
/// nothing else of its kernel and is made without the kernel lock.
///
/// A copy (a fork's, a vfork child's) starts empty: the parent's notes are
/// the parent's host's to act on. An exec or a restore keeps it, as it keeps
/// the view ([`MemoryMap::renew`]).
#[derive(Debug, Default)]
pub struct MapLog {
    /// The base the next anonymous reservation should get, and for what.
    ///
    /// `ld.so` does not map an object in one call. It opens the file, reads
    /// the header, reserves the whole span with an *anonymous* `PROT_NONE`
    /// mapping, and only then maps the segments into it with `MAP_FIXED`. The
    /// reservation is the one address gex chooses and the only one worth
    /// pinning -- and being anonymous it carries no file to key on. So the
    /// `openat` immediately before it is the signal, and this is what that
    /// signal sets.
    ///
    /// The predicate has to include `PROT_NONE`: a real trace of glibc's
    /// loader shows a 8 KiB `PROT_READ|WRITE` allocation of its own between
    /// the `fstat` and the reservation, so "the next anonymous mapping" would
    /// pin the wrong one.
    pub armed_pin: Option<(String, u64)>,
    /// Which object the next anonymous reservation is for (`Kernel::place`).
    ///
    /// The same signal [`Self::armed_pin`] uses and for the same reason: the
    /// reservation carries no file, so the `openat` before it is what names
    /// the object. Kept separate because a pin knows its address already and
    /// this one has to ask, and can only ask once the reservation says how
    /// long the object is.
    pub armed_place: Option<String>,
    /// Where each pinned object actually landed, for checking afterwards.
    ///
    /// A pin that silently does not take is the worst outcome available: the
    /// module is built for one address and the code runs at another, and the
    /// guest branches into whatever is there. Recorded so a mismatch can be
    /// reported rather than executed.
    pub pin_landed: Vec<(String, u64, u64)>,
    /// File mappings whose writes are meant to reach the file.
    pub shared_maps: crate::syscalls::SharedMaps,
    /// Executable mappings made since the host last looked.
    pub new_code: Vec<crate::syscalls::CodeMap>,
    /// Executable ranges *unmapped* since the host last looked.
    ///
    /// A translation is only valid while the code it was made from is still
    /// there. `dlclose` unmaps a library and a later `dlopen` maps a
    /// different one over the same addresses, and without this the guest
    /// branches into the previous library's translated code -- the dispatch
    /// slot still answers for the page. Nothing reports an error; the guest
    /// simply runs the wrong function. A long-running X server does exactly
    /// this every time its last client disconnects and it resets.
    pub retired_code: Vec<(u64, u64)>,
    /// Ranges made executable again by `mprotect` (read+execute, no write):
    /// the function tier compares its units' pages there (a loader that lifted
    /// `.text` to fill an import table and put it back, `cgen::on_retire`).
    /// Unlike [`Self::retired_code`], nothing is forgotten: the write that
    /// could have changed the code was retired when the page became writable,
    /// and forgetting the dispatch pages here dropped the bridges of the units
    /// and native modules on them for good (a native Wine DLL's `ud2` stub
    /// then ran as code: SIGILL in Undertale's GL start).
    pub recheck_code: Vec<(u64, u64)>,
}

impl Clone for MapLog {
    fn clone(&self) -> MapLog {
        MapLog::default()
    }
}

/// The parts of `[start, end)` outside `clean` (sorted, disjoint), in order:
/// [`MemoryMap::dirty_parts`], or the same against a copy taken before a
/// change.
pub fn dirty_parts(clean: &[(u64, u64)], start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = start;
    let first = clean.partition_point(|&(_, e)| e <= start);
    for &(s, e) in &clean[first..] {
        if s >= end {
            break;
        }
        if s > at {
            out.push((at, s));
        }
        at = at.max(e);
    }
    if at < end {
        out.push((at, end));
    }
    out
}

/// Bytes an M32 process asked for and could not be given for lack of address
/// space (not of memory): the run that sees it records the program for a
/// 64-bit memory next time (docs/mem64.md §5). Zero until it happens.
/// Where this module's rare diagnostics go: stderr unless the host says
/// otherwise (a browser has none; its shim points this at the console).
static SAY: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

pub fn set_say(f: fn(&str)) {
    let _ = SAY.set(f);
}

fn say(s: &str) {
    match SAY.get() {
        Some(f) => f(s),
        None => eprint!("{s}"),
    }
}

/// The smallest refused mapping [`MemoryMap::report_short`] explains (Wine
/// probes the top of its x86-64 user space with a 1 MiB one in every process).
const SHORT_REPORT_MIN: u64 = 4 << 20;

pub static ADDRESS_SPACE_SHORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Note that an M32 process ran out of address space asking for `len` bytes.
pub fn note_address_space_short(len: u64) {
    ADDRESS_SPACE_SHORT.fetch_max(len.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// Whether `[start, end)` touches guest memory the runtime owns but no VMA
/// records, so `is_free` would call it free:
///
/// - below [`gex_abi::DISPATCH_DIR_END`]: the thread contexts and the dispatch
///   directory. A guest mapping there wipes directory entries, and a hot block
///   then misses forever.
/// - `[STACK_TOP, MMAP_BOTTOM)`, the gap between the guest's stack and the
///   arena: in the browser the shim shares the guest's memory and keeps its
///   own stack and data there. Natively it is only unused, and refusing it
///   moves a guest's mapping elsewhere, which is harmless.
pub fn hits_runtime(start: u64, end: u64) -> bool {
    start < gex_abi::DISPATCH_DIR_END || (start < MMAP_BOTTOM && end > gex_abi::STACK_TOP)
}

pub fn page_align_up(v: u64) -> u64 {
    (v + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

pub fn page_align_down(v: u64) -> u64 {
    v & !(PAGE_SIZE - 1)
}

impl MemoryMap {
    pub fn new(brk_start: u64) -> MemoryMap {
        MemoryMap {
            vmas: Vec::new(),
            view: ViewCell(std::sync::Arc::new(MapView::new(&[]))),
            generation: 0,
            brk_start: page_align_up(brk_start),
            brk_cur: page_align_up(brk_start),
            mmap_next: MMAP_TOP,
            mmap_ceiling: MMAP_TOP,
            arena: Vec::new(),
            take: None,
            back: None,
            // Every process of a host with a 64-bit guest memory has the
            // high arena (docs/mem64.md §3).
            high: gex_abi::mem64().then_some((gex_abi::HIGH_ARENA_BASE, gex_abi::LIMIT64)),
            clean: Vec::new(),
            fresh: None,
            take_end: 0,
            fixed_only: Vec::new(),
            log: MapLog::default(),
        }
    }

    /// Start again with an empty map (an exec, a snapshot's restore): the
    /// same [`MapView`], which readers without the kernel lock already hold
    /// (a new map would leave them reading the old program's).
    pub fn renew(&mut self, brk_start: u64) {
        let mut fresh = MemoryMap::new(brk_start);
        std::mem::swap(&mut fresh.view, &mut self.view);
        std::mem::swap(&mut fresh.log, &mut self.log);
        *self = fresh;
        self.publish();
    }

    /// Record that `[start, end)` holds only zeros (the caller just zeroed it,
    /// or the memory is new) and is not writable.
    pub fn mark_clean(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.mark_dirty(start, end);
        let at = self.clean.partition_point(|&(s, _)| s < start);
        self.clean.insert(at, (start, end));
        // Coalesce with touching neighbours.
        if at + 1 < self.clean.len() && self.clean[at + 1].0 == end {
            self.clean[at].1 = self.clean.remove(at + 1).1;
        }
        if at > 0 && self.clean[at - 1].1 == start {
            self.clean[at - 1].1 = self.clean.remove(at).1;
        }
    }

    /// Forget that anything in `[start, end)` is known to be zero: it was
    /// mapped with contents or made writable.
    pub fn mark_dirty(&mut self, start: u64, end: u64) {
        if start >= end || self.clean.is_empty() {
            return;
        }
        let lo = self.clean.partition_point(|&(_, e)| e <= start);
        let hi = self.clean.partition_point(|&(s, _)| s < end);
        if lo >= hi {
            return;
        }
        let mut keep = Vec::with_capacity(2);
        let (first, last) = (self.clean[lo], self.clean[hi - 1]);
        if first.0 < start {
            keep.push((first.0, start));
        }
        if last.1 > end {
            keep.push((end, last.1));
        }
        self.clean.splice(lo..hi, keep);
    }

    /// The parts of `[start, end)` not known to be zero, in order.
    pub fn dirty_parts(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        dirty_parts(&self.clean, start, end)
    }

    /// The ranges known to hold only zeros (see `clean`).
    pub fn clean_ranges(&self) -> &[(u64, u64)] {
        &self.clean
    }

    /// Give this process a 64-bit memory's high arena (docs/mem64.md §3).
    pub fn set_high_arena(&mut self, lo: u64, hi: u64) {
        self.high = Some((lo, hi));
    }

    /// The highest address a mapping may end at, exclusive.
    fn limit(&self) -> u64 {
        self.high.map_or(GUEST_LIMIT, |(_, hi)| hi)
    }

    /// First fit, bottom-up, in the high arena: the browser grows memory to
    /// the highest address used, so low and dense is cheap.
    fn find_high(&self, len: u64) -> Option<u64> {
        let (lo, hi) = self.high?;
        let mut candidate = lo;
        loop {
            if candidate + len > hi {
                return None;
            }
            match self.vmas.iter().filter(|v| v.start < candidate + len && v.end > candidate).map(|v| v.end).max() {
                Some(end) => candidate = page_align_up(end),
                None => return Some(candidate),
            }
        }
    }

    /// How many times the map has changed. Two equal readings mean the VMA
    /// tree is byte-for-byte the same, so anything derived from it still
    /// holds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The mapping holding `addr`, if any.
    pub fn vma_at(&self, addr: u64) -> Option<&Vma> {
        // Sorted by start and disjoint: the last one starting at or below.
        let i = self.vmas.partition_point(|v| v.start <= addr);
        self.vmas[..i].last().filter(|v| addr < v.end)
    }

    pub fn vmas(&self) -> &[Vma] {
        &self.vmas
    }

    /// The map as readers that do not own it see it ([`MapView`]): the same
    /// view for the map's whole life, republished as it changes.
    pub fn view(&self) -> &std::sync::Arc<MapView> {
        &self.view.0
    }

    /// Publish the map as it stands to its [`MapView`].
    fn publish(&self) {
        if KEEPS_VIEWS {
            let ranges = MapView::ranges(&self.vmas);
            self.view.0.cur.write(|_| (Some(ranges), ()));
        }
    }

    /// Hand out arena addresses bottom-up, for a host whose memory must be
    /// allocated up to whatever it hands out.
    pub fn grows_on_demand(&mut self, take: fn(u64) -> u64, back: fn(u64, u64) -> Option<u64>) {
        self.take = Some(take);
        self.back = Some(back);
        self.arena.clear();
        self.take_end = 0;
    }

    /// Take `[lo, hi)` as this map's own arena: the host has backed it and
    /// handed it to the process (the browser's Windows address space, W3),
    /// so mappings land in it, fixed or not, without asking the host again.
    pub fn adopt_arena(&mut self, lo: u64, hi: u64) {
        if lo >= hi {
            return;
        }
        self.arena.push((lo, hi));
        self.arena.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.arena.len());
        for (a, b) in std::mem::take(&mut self.arena) {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.arena = merged;
    }

    /// Never place a mapping that names no address in `[lo, hi)`: it is
    /// for mappings placed there (W3's and W64's exe slot and pinned-DLL
    /// band, `shims/browser/src/nt.rs` `keep_exe_slot`). Once a fixed
    /// mapping there made it arena, its holes took anonymous mappings
    /// bottom-up (64 MiB from 0x19711000, just above lime.ndll, in W64
    /// Dicey's game), and a DLL mapped later found its pin taken.
    pub fn keep_fixed_only(&mut self, lo: u64, hi: u64) {
        if lo >= hi || self.fixed_only.iter().any(|&(a, b)| a <= lo && hi <= b) {
            return;
        }
        self.fixed_only.push((lo, hi));
        self.fixed_only.sort_unstable();
    }

    /// The arena spans this map has been handed (sorted, disjoint).
    pub fn arena_spans(&self) -> &[(u64, u64)] {
        &self.arena
    }

    /// Give this map arena that is backed already and holds nothing anyone
    /// uses: an `execve`'s dead address space (the old image's mappings, and a
    /// forked child's inherited copy of its parent's). Without it the new
    /// program's mappings all go above the old ones, and a program exec'd from
    /// a fork of a large parent (node's child_process: a 1.3 GiB parent) ran
    /// against the 2 GiB ceiling at twice the time. Nothing here is known to be
    /// zero, so an anonymous `mmap` zeroes what it hands out of these, page by
    /// page, as it would any reused hole.
    pub fn donate_arena(&mut self, spans: &[(u64, u64)]) {
        for &(lo, hi) in spans {
            let (lo, hi) = (page_align_up(lo), page_align_down(hi));
            if lo >= hi {
                continue;
            }
            self.mark_dirty(lo, hi);
            let at = self.arena.partition_point(|&(s, _)| s < lo);
            // Disjoint from what is there already, or not given at all.
            if at > 0 && self.arena[at - 1].1 > lo {
                continue;
            }
            if at < self.arena.len() && self.arena[at].0 < hi {
                continue;
            }
            self.arena.insert(at, (lo, hi));
            if at + 1 < self.arena.len() && self.arena[at + 1].0 == hi {
                self.arena[at].1 = self.arena.remove(at + 1).1;
            }
            if at > 0 && self.arena[at - 1].1 == lo {
                self.arena[at - 1].1 = self.arena.remove(at).1;
            }
        }
    }

    /// The address one past the highest byte in use, which is how much memory
    /// a process actually needs.
    pub fn high_water(&self) -> u64 {
        self.vmas
            .iter()
            .map(|v| v.end)
            .chain(std::iter::once(self.brk_cur))
            .max()
            .unwrap_or(MMAP_BOTTOM)
    }

    /// Whether `[start, end)` lies in this map's own spans of the arena. The
    /// spans are sorted and disjoint (the cursor only climbs).
    fn in_arena(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        for &(lo, hi) in &self.arena {
            if hi <= at {
                continue;
            }
            if lo > at {
                return false;
            }
            at = hi;
            if at >= end {
                return true;
            }
        }
        at >= end
    }

    /// Make `[start, end)` this process's to map, for a mapping at an address
    /// the guest chose (MAP_FIXED, an honoured hint, an mremap growing in
    /// place): backed, and not memory the runtime holds.
    ///
    /// Natively everything is. In the browser the arena above
    /// [`gex_abi::ARENA_BOTTOM`] is shared with the shim's own heap through
    /// one cursor, so a range there must lie in this map's spans, or above
    /// the cursor, which `back` then moves past it. Below `ARENA_BOTTOM` is
    /// the guest's fixed layout and the placement window, which the heap
    /// never takes from. A range the heap has a span in is ENOMEM: honouring
    /// it put a guest mapping over the shim's heap, and a program whose
    /// malloc passed mmap hints (musl built by zig) trapped in the shim after
    /// a hundred or so large allocations.
    fn claim(&mut self, start: u64, end: u64) -> Result<(), Errno> {
        if self.take.is_none() {
            return Ok(());
        }
        let floor = gex_abi::ARENA_BOTTOM;
        let ours = |m: &Self| end <= floor || m.in_arena(start.max(floor), end);
        if ours(self) {
            // Below the floor the memory may still need growing (the
            // placement window lies above what a process starts with).
            if end <= floor {
                if let Some(back) = self.back {
                    back(start, end).ok_or(Errno::ENOMEM)?;
                }
            }
            return Ok(());
        }
        let Some(back) = self.back.filter(|_| end <= MMAP_TOP) else {
            return Err(Errno::ENOMEM);
        };
        let Some(prev) = back(start, end) else {
            return Err(Errno::ENOMEM);
        };
        // An island's range (W3's pinned DLL band, in the browser): backed and
        // the guest's, but never this map's arena. Adopted, an image's slot
        // went to the next bottom-up mapping once the image was unmapped, and
        // the image loaded again (uxtheme, in Paint) relocated and missed its
        // unit.
        if prev == BACKED_ISLAND {
            return Ok(());
        }
        // What the cursor stepped over is backed now and nobody else's:
        // arena, so the next mapping lands in it rather than growing the
        // memory again. Without this a MAP_FIXED high up (a pinned library,
        // a thread stack placed by hint) stranded everything between the old
        // cursor and itself.
        let prev = prev.max(floor);
        if prev < end {
            // Sorted, not pushed: an island lies above spans taken later.
            self.adopt_arena(prev, end);
        }
        if ours(self) {
            Ok(())
        } else {
            Err(Errno::ENOMEM)
        }
    }

    /// Say once what fills the address space below [`MMAP_TOP`] when a
    /// large mapping is refused for want of space (Wine's x86-64 user space
    /// is 4 GiB to it, and it asks above 2 GiB only once nothing below
    /// fits). Totals by kind, the biggest mappings and names, the free holes
    /// and the arena, so "the game's working set" and "our leak" can be told
    /// apart from the log.
    fn report_short(&self, addr: u64, len: u64) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.swap(true, Ordering::Relaxed) {
            return;
        }
        let mib = |b: u64| b as f64 / 1048576.0;
        let low: Vec<&Vma> = self.vmas.iter().filter(|v| v.start < MMAP_TOP).collect();
        let size = |v: &Vma| v.end.min(MMAP_TOP) - v.start;
        let (mut file, mut none, mut anon) = (0u64, 0u64, 0u64);
        let mut by_name: std::collections::BTreeMap<String, (u64, u32)> = Default::default();
        for v in &low {
            let n = size(v);
            if v.prot == prot::NONE {
                none += n;
            } else if v.name.starts_with('/') {
                file += n;
            } else {
                anon += n;
            }
            let key = format!("{} {}", if v.prot == prot::NONE { "---" } else { "rw?" }, if v.name.is_empty() { "[anon]" } else { &v.name });
            let e = by_name.entry(key).or_default();
            e.0 += n;
            e.1 += 1;
        }
        let mut holes = Vec::new();
        let mut at = MMAP_BOTTOM;
        for v in &low {
            if v.end <= MMAP_BOTTOM {
                continue;
            }
            if v.start > at {
                holes.push(v.start - at);
            }
            at = at.max(v.end);
        }
        if MMAP_TOP > at {
            holes.push(MMAP_TOP - at);
        }
        holes.sort_unstable_by(|a, b| b.cmp(a));
        let arena: u64 = self.arena.iter().map(|&(a, b)| b.min(MMAP_TOP).saturating_sub(a)).sum();
        let mut s = format!(
            "[gex] address space: {:.1} MiB at {addr:#x} refused (ENOMEM); below {:#x} {} mappings: files {:.1} MiB, \
             anonymous {:.1} MiB, PROT_NONE reservations {:.1} MiB; this process's arena {:.1} MiB of {:.1} MiB \
             ({:#x}-{:#x}; the rest is the runtime's heap or other processes'); holes in {:#x}-{:#x}: {:.1} MiB in {}, \
             largest {:.1} MiB\n",
            mib(len),
            MMAP_TOP,
            low.len(),
            mib(file),
            mib(anon),
            mib(none),
            mib(arena),
            mib(MMAP_TOP - gex_abi::ARENA_BOTTOM),
            gex_abi::ARENA_BOTTOM,
            MMAP_TOP,
            MMAP_BOTTOM,
            MMAP_TOP,
            mib(holes.iter().sum()),
            holes.len(),
            mib(holes.first().copied().unwrap_or(0)),
        );
        let mut names: Vec<_> = by_name.into_iter().collect();
        names.sort_unstable_by(|a, b| b.1 .0.cmp(&a.1 .0));
        for (n, (b, c)) in names.iter().take(16) {
            s.push_str(&format!("[gex] address space:   {:8.1} MiB in {c:5} {n}\n", mib(*b)));
        }
        let mut big = low.clone();
        big.sort_unstable_by_key(|v| std::cmp::Reverse(size(v)));
        for v in big.iter().take(12) {
            s.push_str(&format!("[gex] address space:   {:08x}-{:08x} {:8.1} MiB prot {} {}\n", v.start, v.end, mib(size(v)), v.prot, v.name));
        }
        say(&s);
    }

    /// How much of the top of the arena is free: the bytes between the highest
    /// mapping and the end of the highest span. A fresh claim continues from
    /// here so the tail the last user gave back is spent first.
    fn free_tail(&self) -> u64 {
        let Some(&(lo, hi)) = self.arena.iter().find(|s| s.1 == self.take_end) else {
            return 0;
        };
        let top = self
            .vmas
            .iter()
            .filter(|v| v.end > lo && v.start < hi)
            .map(|v| v.end)
            .max()
            .unwrap_or(lo);
        hi.saturating_sub(top.max(lo))
    }

    /// Claim `len` more bytes of arena, growing the memory behind it.
    fn extend_arena(&mut self, len: u64) -> Result<u64, Errno> {
        let Some(take) = self.take else {
            return Err(Errno::ENOMEM);
        };
        // A chunk at a time, so a guest that maps a page at a time does not
        // ask the host to grow its memory once per page.
        const CHUNK: u64 = 8 * 1024 * 1024;
        let need = page_align_up(len);
        // Carry on from the free tail instead of above it: glibc's `new_heap`
        // frees most of a `HEAP_MAX_SIZE` back and then asks for more, so
        // claiming a full `len` above the tail strands it every time. `apt-get
        // update` hit the 2 GiB ceiling this way.
        let tail = self.free_tail();
        // Subtracting the free tail lets a small heap grow carry on from it
        // instead of stranding it (glibc's `new_heap` free-and-remap, which hit
        // the 2 GiB ceiling otherwise). But that reuse only actually happens in
        // the *adjacent* branch below, where the span starts `tail` bytes before
        // `start`. For a mapping larger than a CHUNK the reduced `want` is what
        // gets taken and recorded, and if the take lands non-adjacent (the
        // shared cursor moved) the tail is never reused and the span is short by
        // `tail` -- a 107 MiB libLLVM mmap then hands back an address whose last
        // `tail` bytes were never backed, faulting OOB in Mesa's DRI screen init
        // (no GLX fbconfigs) and crashing glxinfo outright. Small grows floor to
        // CHUNK >= need and are backed regardless, so only shrink `want` for
        // them; back a large mapping in full.
        let want = if need > CHUNK {
            need
        } else {
            need.saturating_sub(tail).max(CHUNK)
        };
        let start = take(want);
        if start == 0 {
            if self.high.is_none() {
                note_address_space_short(len);
            }
            return Err(Errno::ENOMEM);
        }
        // Join to the span below when adjacent rather than recording a second
        // box: the take-cursor only goes up, so consecutive claims are really
        // one region, but the free-hole search runs within a single span and
        // would miss a hole straddling a boundary -- glibc's ordinary heap
        // pattern, which reached the 2 GiB ceiling and aborted.
        let take_end = self.take_end;
        if let Some(last) = self.arena.iter_mut().find(|s| s.1 == start && start == take_end) {
            last.1 = start + want;
            self.take_end = start + want;
            self.fresh = Some((start, start + want));
            // An island may begin right where this span now ends.
            self.adopt_arena(start, start + want);
            return Ok(start - tail);
        }
        // Record the span and hand back its start. A single `take` is one
        // contiguous claim whatever the cursor does; a `29f6e7f` change added a
        // SECOND `take` here to "top up" the tail `want` was shortened by, but
        // the arena cursor is shared between a process's threads, so a racing
        // claim from another thread landed between the two takes and the span
        // recorded here then covered memory that claim owned -- multithreaded
        // Xvfb corrupted into an OOB/SIGABRT, which was the virgl regression.
        // The common mapping (a pthread stack is 8 MiB == CHUNK, so `want` is
        // floored to CHUNK >= need) is fully backed by the one take above; the
        // only under-backed case is a mapping larger than a CHUNK that followed
        // a freed tail, which is rare and predates this code -- fix that in the
        // mmap path if it ever bites, not with a second racing take.
        self.adopt_arena(start, start + want);
        self.take_end = start + want;
        self.fresh = Some((start, start + want));
        Ok(start)
    }

    /// The span of arena the last mapping had to claim, which nothing has
    /// been in yet (`arena_take` hands out memory past everything the
    /// process used, and a recycled memory comes zeroed): zero without
    /// looking. Cleared by asking.
    pub fn take_fresh(&mut self) -> Option<(u64, u64)> {
        self.fresh.take()
    }

    pub fn brk_current(&self) -> u64 {
        self.brk_cur
    }

    /// Record a mapping made outside the allocator, such as an ELF segment or
    /// the initial stack.
    pub fn insert(&mut self, start: u64, end: u64, prot: u32, name: &str) {
        self.generation += 1;
        self.insert_from(start, end, prot, name, None);
    }

    /// Record a region exactly as another process had it, for `fork`: keep the
    /// parent's name and code identity so the child can still find the code in
    /// the shared store instead of re-translating from cold. No `origin`: that
    /// is a claim about a mapping this process made, not one to inherit.
    pub fn adopt(&mut self, start: u64, end: u64, prot: u32, name: &str, code: Option<(String, u64)>) {
        let r = self.adopt_unpublished(start, end, prot, name, code);
        self.publish();
        r
    }

    /// [`Self::adopt`] before it is published.
    fn adopt_unpublished(&mut self, start: u64, end: u64, prot: u32, name: &str, code: Option<(String, u64)>) {
        self.generation += 1;
        self.mark_dirty(page_align_down(start), page_align_up(end));
        self.vmas.push(Vma {
            start: page_align_down(start),
            end: page_align_up(end),
            prot,
            name: name.to_string(),
            origin: None,
            code,
        });
        self.vmas.sort_by_key(|v| v.start);
    }

    /// As [`Self::insert`], for bytes a loader placed: the file and offset name
    /// the *code*, but the mapping cannot be rebuilt by re-reading so it gets no
    /// `origin`. This is how an ELF segment arrives -- its `mem_size` runs past
    /// the file into a zeroed tail, yet its executable bytes are exactly a range
    /// of the file.
    pub fn insert_loaded(
        &mut self,
        start: u64,
        end: u64,
        prot: u32,
        name: &str,
        code: Option<(String, u64)>,
    ) {
        self.insert_from(start, end, prot, name, code);
        if let Some(v) = self.vmas.iter_mut().find(|v| v.start == page_align_down(start)) {
            v.origin = None;
        }
    }

    /// As [`Self::insert`], recording where the bytes came from.
    pub fn insert_from(&mut self, start: u64, end: u64, prot: u32, name: &str, origin: Option<(String, u64)>) {
        let r = self.insert_from_unpublished(start, end, prot, name, origin);
        self.publish();
        r
    }

    /// [`Self::insert_from`] before it is published.
    fn insert_from_unpublished(
        &mut self,
        start: u64,
        end: u64,
        prot: u32,
        name: &str,
        origin: Option<(String, u64)>,
    ) {
        self.generation += 1;
        let aligned = page_align_down(start);
        // Whatever it will hold is the caller's to write; an anonymous
        // mapping the kernel zeroed is marked clean again after.
        self.mark_dirty(aligned, page_align_up(end));
        // The offset moves with the start: rounding down to a page boundary
        // walks backwards into the file by the same amount.
        let code = origin.map(|(id, off)| (id, off.saturating_sub(start - aligned)));
        // `origin` is only for a mapping that has *never* been writable. `ld.so`
        // maps a library's data segment RW from the file, so libc's GOT would
        // otherwise claim to be reconstructable by re-reading the very bytes the
        // linker overwrites. `code` is the different, surviving claim: where the
        // bytes were loaded, whatever happened after.
        let origin = if prot & prot::WRITE != 0 { None } else { code.clone() };
        self.vmas.push(Vma {
            start: aligned,
            end: page_align_up(end),
            prot,
            name: name.to_string(),
            code,
            origin,
        });
        // Sorted already but for the one just pushed: move it into place.
        let at = self.vmas[..self.vmas.len() - 1].partition_point(|v| v.start < aligned);
        let v = self.vmas.pop().expect("just pushed");
        self.vmas.insert(at, v);
        self.merge_adjacent();
    }

    /// Fold touching anonymous mappings with the same permissions and name
    /// into one, as Linux does. A program that reserves address space a
    /// chunk at a time otherwise leaves thousands of VMAs, which every
    /// placement scans and `/proc/self/maps` lists line by line (Wine
    /// re-parses that file with `sscanf`). Only mappings with no `code` or
    /// `origin` qualify: those identities are keyed on the exact span.
    fn merge_adjacent(&mut self) {
        let mut i = 0;
        while i + 1 < self.vmas.len() {
            if !self.merge_at(i) {
                i += 1;
            }
        }
    }

    /// Fold mapping `i + 1` into mapping `i` if they qualify
    /// ([`Self::merge_adjacent`]). Returns whether it did.
    fn merge_at(&mut self, i: usize) -> bool {
        let (Some(a), Some(b)) = (self.vmas.get(i), self.vmas.get(i + 1)) else { return false };
        if a.end == b.start
            && a.prot == b.prot
            && a.name == b.name
            && a.code.is_none()
            && b.code.is_none()
            && a.origin.is_none()
            && b.origin.is_none()
        {
            self.vmas[i].end = self.vmas[i + 1].end;
            self.vmas.remove(i + 1);
            return true;
        }
        false
    }

    /// `brk(0)` reports the break; `brk(addr)` moves it and returns the new
    /// value. Linux returns the *current* break on failure rather than an
    /// error, which is what libc's allocator probes for.
    ///
    /// The second return value is the range the break just grew over, which
    /// the caller must zero. See [`Self::brk`]'s note on why that is not
    /// optional.
    pub fn brk_grow(&mut self, addr: u64) -> (u64, Option<(u64, u64)>) {
        self.generation += 1;
        if addr == 0 {
            return (self.brk_cur, None);
        }
        let new = page_align_up(addr);
        if new < self.brk_start || new >= gex_abi::BRK_TOP {
            return (self.brk_cur, None);
        }
        let grew = (new > self.brk_cur).then_some((self.brk_cur, new));
        if let Some((from, to)) = grew {
            // Linux will not grow the break into an existing mapping: it
            // leaves the break where it was and returns it, and libc's
            // allocator falls back to mmap. Without this the break grows
            // straight over an mmap and a heap object aliases whatever was
            // mapped there (Wine reserves address space just above the heap).
            // The heap's own records are not in the way: a shrink leaves the
            // pages above the break recorded, and growing back over them is
            // what the break is for.
            if self.vmas.iter().any(|v| v.start < to && v.end > from && v.name != "[heap]") {
                return (self.brk_cur, None);
            }
            self.insert(from, to, prot::READ | prot::WRITE, "[heap]");
        }
        self.brk_cur = new;
        (addr, grew)
    }

    /// Move the break without zeroing what it exposed. Only for callers with no
    /// guest memory to hand (tests); a syscall must use [`Self::brk_grow`] and
    /// zero the range, because glibc's `calloc` skips its own memset for fresh
    /// `sbrk` memory and relies on Linux's zero guarantee.
    pub fn brk(&mut self, addr: u64) -> u64 {
        self.generation += 1;
        self.brk_grow(addr).0
    }

    /// Hand out no mapping above `top` unless asked for there (a process
    /// hosting Windows code, W3: Windows keeps 0x7ffe0000 for
    /// KUSER_SHARED_DATA and the top of user space for itself).
    pub fn set_mmap_top(&mut self, top: u64) {
        self.mmap_next = self.mmap_next.min(page_align_down(top));
        self.mmap_ceiling = self.mmap_ceiling.min(page_align_down(top));
    }

    /// Find and reserve a free range of `len` bytes. Top-down from [`MMAP_TOP`],
    /// matching Linux, so the heap and mappings grow apart.
    pub fn mmap(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str) -> SysResult {
        self.generation += 1;
        self.mmap_from(addr, len, prot, flags, name, None)
    }

    /// [`Self::mmap`] below the high arena whatever the protection: a
    /// mapping that will hold code (the dynamic loader's object reservation).
    pub fn mmap_low(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str) -> SysResult {
        let high = self.high.take();
        let r = self.mmap(addr, len, prot, flags, name);
        self.high = high;
        r
    }

    /// As [`Self::mmap`], recording the file and offset the bytes came from.
    pub fn mmap_from(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str, origin: Option<(String, u64)>) -> SysResult {
        let r = self.mmap_from_unpublished(addr, len, prot, flags, name, origin);
        if r == Err(Errno::ENOMEM) && len >= SHORT_REPORT_MIN {
            self.report_short(addr, len);
        }
        self.publish();
        r
    }

    /// [`Self::mmap_from`] before it is published.
    fn mmap_from_unpublished(
        &mut self,
        addr: u64,
        len: u64,
        prot: u32,
        flags: u64,
        name: &str,
        origin: Option<(String, u64)>,
    ) -> SysResult {
        self.generation += 1;
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);

        if flags & (flags::FIXED | flags::FIXED_NOREPLACE) != 0 {
            if !addr.is_multiple_of(PAGE_SIZE) {
                return Err(Errno::EINVAL);
            }
            let end = addr.checked_add(len).ok_or(Errno::ENOMEM)?;
            if end > self.limit() {
                return Err(Errno::ENOMEM);
            }
            // Code above 4 GiB is beyond the dispatch directory (docs/mem64.md
            // §3), as for mprotect.
            if prot & prot::EXEC != 0 && self.high.is_some() && end > gex_abi::HIGH_ARENA_BASE {
                eprintln!("gex: an executable mapping at {addr:#x}..{end:#x}, above 4 GiB in a 64-bit memory: not supported yet (docs/mem64.md)");
                return Err(Errno::EACCES);
            }
            // `MAP_FIXED_NOREPLACE` (which wins over `MAP_FIXED`, as on
            // Linux) fails over any existing mapping, a PROT_NONE
            // reservation included: a program that probes for free address
            // space with it (Wine does, for every thread's stack) must not be
            // told that a taken range is free. The runtime's own regions
            // count as taken.
            if flags & flags::FIXED_NOREPLACE != 0 && (!self.is_free(addr, end) || hits_runtime(addr, end)) {
                return Err(Errno::EEXIST);
            }
            // A plain `MAP_FIXED` replaces the guest's mappings, never the
            // runtime's.
            if hits_runtime(addr, end) {
                return Err(Errno::ENOMEM);
            }
            // MAP_FIXED replaces whatever was there.
            self.claim(addr, end)?;
            self.unmap(addr, len)?;
            self.insert_from(addr, end, prot, name, origin);
            return Ok(addr);
        }

        // An address hint is honoured when the range happens to be free, and
        // is not the runtime's.
        if addr != 0 && addr.is_multiple_of(PAGE_SIZE) {
            let end = addr.saturating_add(len);
            if end <= self.limit()
                && self.is_free(addr, end)
                && !hits_runtime(addr, end)
                && self.claim(addr, end).is_ok()
            {
                self.insert_from(addr, end, prot, name, origin);
                return Ok(addr);
            }
        }

        // Anonymous data in an M64 process goes to the high arena; code
        // stays below MMAP_TOP, where the dispatch directory reaches. A file
        // mapping stays low whatever its protection: ld.so reserves a whole
        // library read-only and then maps its text executable inside it.
        if prot & prot::EXEC == 0 && origin.is_none() {
            if let Some(start) = self.find_high(len) {
                if let Some(back) = self.back {
                    back(start, start + len).ok_or(Errno::ENOMEM)?;
                }
                self.insert_from(start, start + len, prot, name, origin);
                return Ok(start);
            }
        }

        // Bottom-up when the host's memory grows with the arena, so the highest
        // address in use stays low: a `munmap` hole is reused before the arena
        // -- and the memory behind it -- gets bigger.
        if self.take.is_some() {
            let found = self.arena.iter().find_map(|&(lo, hi)| self.first_fit(lo, hi, len));
            if let Some(candidate) = found {
                self.insert_from(candidate, candidate + len, prot, name, origin);
                return Ok(candidate);
            }
            let start = self.extend_arena(len)?;
            self.insert_from(start, start + len, prot, name, origin);
            return Ok(start);
        }

        let mut candidate = self.mmap_next;
        // The cursor only moves down; once it runs out, one more pass from
        // the top finds what `munmap` freed above it (Linux searches the whole
        // gap list each time: a program that maps and frees 1 GiB buffers in
        // turn, as expat's tests do, must get the second one).
        let mut wrapped = self.mmap_next >= self.mmap_ceiling;
        loop {
            if !wrapped && (candidate < len || candidate - len < MMAP_BOTTOM) {
                wrapped = true;
                candidate = self.mmap_ceiling;
                continue;
            }
            if candidate < len || candidate - len < MMAP_BOTTOM {
                if self.high.is_none() {
                    note_address_space_short(len);
                }
                return Err(Errno::ENOMEM);
            }
            let start = page_align_down(candidate - len);
            if let Some(&(a, _)) = self.fixed_only.iter().find(|&&(a, b)| start < b && a < start + len) {
                candidate = a;
                continue;
            }
            if self.is_free(start, start + len) {
                self.mmap_next = start;
                self.insert_from(start, start + len, prot, name, origin);
                return Ok(start);
            }
            // Skip below the mapping that got in the way.
            match self
                .vmas
                .iter()
                .rev()
                .find(|v| v.start < start + len && v.end > start)
            {
                Some(v) => candidate = v.start,
                None => return Err(Errno::ENOMEM),
            }
        }
    }

    /// Grow or shrink an existing mapping. Returns the new base -- the old one
    /// when extended in place, a fresh range otherwise; the caller moves the
    /// bytes. musl's `realloc` calls this for mmap-backed buffers, so `ENOSYS`
    /// would make it copy on every growth.
    pub fn mremap(&mut self, addr: u64, old_len: u64, new_len: u64, may_move: bool) -> Result<(u64, bool), Errno> {
        let r = self.mremap_unpublished(addr, old_len, new_len, may_move);
        self.publish();
        r
    }

    /// [`Self::mremap`] before it is published.
    fn mremap_unpublished(
        &mut self,
        addr: u64,
        old_len: u64,
        new_len: u64,
        may_move: bool,
    ) -> Result<(u64, bool), Errno> {
        self.generation += 1;
        if !addr.is_multiple_of(PAGE_SIZE) || old_len == 0 || new_len == 0 {
            return Err(Errno::EINVAL);
        }
        let old_len = page_align_up(old_len);
        let new_len = page_align_up(new_len);
        // The whole range has to be one mapping, as Linux requires.
        let Some(v) = self
            .vmas
            .iter()
            .find(|v| v.start <= addr && v.end >= addr + old_len)
            .cloned()
        else {
            return Err(Errno::EFAULT);
        };
        if new_len <= old_len {
            if new_len < old_len {
                self.unmap(addr + new_len, old_len - new_len)?;
            }
            return Ok((addr, false));
        }
        let grown = addr + new_len;
        if grown <= self.limit()
            && self.is_free(addr + old_len, grown)
            && !hits_runtime(addr + old_len, grown)
            && self.claim(addr + old_len, grown).is_ok()
        {
            self.insert(addr + old_len, grown, v.prot, &v.name);
            return Ok((addr, false));
        }
        if !may_move {
            return Err(Errno::ENOMEM);
        }
        let dst = self.mmap(0, new_len, v.prot, 0, &v.name)?;
        self.unmap(addr, old_len)?;
        Ok((dst, true))
    }

    fn is_free(&self, start: u64, end: u64) -> bool {
        // `vmas` is sorted and non-overlapping, so the ends are sorted too:
        // the first mapping ending above `start` is the only one to check.
        let i = self.vmas.partition_point(|v| v.end <= start);
        self.vmas.get(i).is_none_or(|v| v.start >= end)
    }

    #[cfg(test)]
    fn is_free_slow(&self, start: u64, end: u64) -> bool {
        !self.vmas.iter().any(|v| v.start < end && v.end > start)
    }

    /// The lowest page-aligned `len`-byte hole in `[lo, hi)`, first fit, in
    /// one pass over the sorted mappings from `lo` (the bottom-up search used
    /// to filter every mapping for each candidate: quadratic in the number of
    /// mappings, which a Wine process has thousands of, under the kernel
    /// lock, on every anonymous `mmap`).
    fn first_fit(&self, lo: u64, hi: u64, len: u64) -> Option<u64> {
        let mut candidate = page_align_up(lo);
        let mut i = self.vmas.partition_point(|v| v.end <= candidate);
        loop {
            let end = candidate.checked_add(len)?;
            if end > hi {
                return None;
            }
            if let Some(&(_, b)) = self.fixed_only.iter().find(|&&(a, b)| candidate < b && a < end) {
                candidate = page_align_up(b);
                i = self.vmas.partition_point(|v| v.end <= candidate);
                continue;
            }
            match self.vmas.get(i) {
                Some(v) if v.start < end => {
                    candidate = candidate.max(page_align_up(v.end));
                    i += 1;
                }
                _ => return Some(candidate),
            }
        }
    }

    /// Remove `[addr, addr+len)` from the map, splitting mappings as needed.
    pub fn unmap(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        let r = self.unmap_unpublished(addr, len);
        self.publish();
        r
    }

    /// [`Self::unmap`] before it is published.
    fn unmap_unpublished(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        self.generation += 1;
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let mut out = Vec::with_capacity(self.vmas.len() + 1);
        for v in std::mem::take(&mut self.vmas) {
            if v.end <= addr || v.start >= end {
                out.push(v);
                continue;
            }
            if v.start < addr {
                out.push(v.slice(v.start, addr, v.prot));
            }
            if v.end > end {
                out.push(v.slice(end, v.end, v.prot));
            }
        }
        out.sort_by_key(|v| v.start);
        self.vmas = out;
        Ok(())
    }

    /// Change protection over a range, splitting mappings as needed.
    pub fn protect(&mut self, addr: u64, len: u64, new_prot: u32) -> Result<(), Errno> {
        let r = self.protect_unpublished(addr, len, new_prot);
        self.publish();
        r
    }

    /// [`Self::protect`] before it is published.
    fn protect_unpublished(&mut self, addr: u64, len: u64, new_prot: u32) -> Result<(), Errno> {
        self.generation += 1;
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        // Code in the high arena is beyond the dispatch directory's reach
        // (docs/mem64.md §3): refused, loudly, until a sparse page table
        // covers it.
        if new_prot & prot::EXEC != 0 && self.high.is_some() && end > gex_abi::HIGH_ARENA_BASE {
            eprintln!(
                "gex: mprotect makes {addr:#x}..{end:#x} executable, above 4 GiB in a 64-bit memory: \
                 not supported yet (docs/mem64.md)"
            );
            return Err(Errno::EACCES);
        }
        if new_prot & prot::WRITE != 0 {
            self.mark_dirty(addr, end);
        }
        let mut out = Vec::with_capacity(self.vmas.len() + 2);
        let mut touched = false;
        for v in std::mem::take(&mut self.vmas) {
            if v.end <= addr || v.start >= end {
                out.push(v);
                continue;
            }
            touched = true;
            if v.start < addr {
                out.push(v.slice(v.start, addr, v.prot));
            }
            // The changed piece loses its origin: a mapping read-only *now* may
            // have been written while writable (`ld.so` relocates a RELRO region
            // then mprotects it read-only), so it cannot be rebuilt by
            // re-reading. `code` survives -- the executable bytes are still the
            // file's, and two processes relocate them identically.
            let mut piece = v.slice(v.start.max(addr), v.end.min(end), new_prot);
            piece.origin = None;
            out.push(piece);
            if v.end > end {
                out.push(v.slice(end, v.end, v.prot));
            }
        }
        out.sort_by_key(|v| v.start);
        // Neighbours the change made alike again are one mapping again, as
        // Linux merges them: a range made read-only and back is whole, so a
        // later `mremap` of it, which must name one mapping, finds one.
        // Only at the boundaries this call touched.
        let mut merged: Vec<Vma> = Vec::with_capacity(out.len());
        for v in out {
            if let Some(last) = merged.last_mut() {
                if last.end == v.start && (addr..=end).contains(&v.start) && last.joins(&v) {
                    last.end = v.end;
                    continue;
                }
            }
            merged.push(v);
        }
        self.vmas = merged;
        if touched {
            Ok(())
        } else {
            Err(Errno::ENOMEM)
        }
    }

    /// True when every page in the range is mapped with at least `need`.
    pub fn check_access(&self, addr: u64, len: u64, need: u32) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let mut cursor = addr;
        while cursor < end {
            match self
                .vmas
                .iter()
                .find(|v| v.start <= cursor && cursor < v.end)
            {
                Some(v) if v.prot & need == need => cursor = v.end,
                _ => return false,
            }
        }
        true
    }

    /// Render the map the way `/proc/self/maps` does.
    /// `/proc/self/status`'s memory sizes, in bytes: everything mapped
    /// (VmSize), what can be touched (VmRSS: gex never pages out, so every
    /// accessible page counts as resident), and the part of that no file
    /// can rebuild (RssAnon). Wine's GetProcessMemoryInfo reads them.
    pub fn vm_counters(&self) -> (u64, u64, u64) {
        let (mut size, mut rss, mut anon) = (0, 0, 0);
        for v in &self.vmas {
            let n = v.end - v.start;
            size += n;
            if v.prot != 0 {
                rss += n;
                if v.origin.is_none() {
                    anon += n;
                }
            }
        }
        (size, rss, anon)
    }

    pub fn proc_maps(&self) -> String {
        let mut s = String::new();
        for v in &self.vmas {
            s.push_str(&format!(
                "{:08x}-{:08x} {}{}{}p 00000000 00:00 0 {}{}\n",
                v.start,
                v.end,
                if v.prot & prot::READ != 0 { 'r' } else { '-' },
                if v.prot & prot::WRITE != 0 { 'w' } else { '-' },
                if v.prot & prot::EXEC != 0 { 'x' } else { '-' },
                if v.name.is_empty() {
                    ""
                } else {
                    "                    "
                },
                v.name,
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused large mapping explains what holds the space, once, without
    /// tripping over mappings that straddle `MMAP_TOP` or sit below the arena.
    #[test]
    fn a_refused_big_mapping_is_explained() {
        let mut m = MemoryMap::new(0x100_0000);
        let anon = flags::PRIVATE | flags::ANONYMOUS;
        m.mmap(MMAP_BOTTOM, 0x10_0000, prot::READ, anon | flags::FIXED, "/lib/x.so").unwrap();
        m.mmap(MMAP_TOP - 0x1000, 0x2000, prot::NONE, anon | flags::FIXED, "").unwrap();
        assert_eq!(m.mmap(GUEST_LIMIT - 0x1000, 8 << 20, 3, anon | flags::FIXED, "big"), Err(Errno::ENOMEM));
        m.report_short(0, 8 << 20);
    }

    /// /proc/self/status's sizes: a reservation counts toward VmSize only,
    /// accessible memory toward VmRSS and RssAnon.
    #[test]
    fn vm_counters_split_reserved_from_resident() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x2000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let (s0, r0, a0) = m.vm_counters();
        m.mmap(0, 0x10_0000, 0, 0, "reserved").unwrap();
        m.mmap(0, 0x4000, prot::READ | prot::WRITE, 0, "data").unwrap();
        let (s, r, a) = m.vm_counters();
        assert_eq!(s - s0, 0x10_4000);
        assert_eq!(r - r0, 0x4000);
        assert_eq!(a - a0, 0x4000);
    }

    /// glibc's heap pattern must not leak the arena a claim at a time.
    ///
    /// `new_heap` frees the head and tail of each claim; the freed tail of one
    /// and head of the next are adjacent and together fit the next request, so
    /// the second heap must land in them rather than extend the arena again.
    #[test]
    fn a_freed_tail_and_the_next_head_are_one_hole() {
        // A cursor that only goes up, which is what the browser shim has.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x1000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);

        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);

        const HEAP: u64 = 64 * 1024 * 1024;
        let mut kept = Vec::new();
        for _ in 0..4 {
            // Two heaps' worth, then trim to alignment, as glibc does.
            let got = m.mmap(0, HEAP * 2, 0, 0, "heap").unwrap() as u64;
            let aligned = (got + HEAP - 1) & !(HEAP - 1);
            if aligned > got {
                m.unmap(got, aligned - got).unwrap();
            }
            let tail = aligned + HEAP;
            if got + HEAP * 2 > tail {
                m.unmap(tail, got + HEAP * 2 - tail).unwrap();
            }
            kept.push(aligned);
        }

        // Four heaps of 64 MiB kept. Reusing the holes, the cursor should
        // have moved a little over 256 MiB; extending every time it is 512.
        let used = NEXT.load(std::sync::atomic::Ordering::SeqCst) - 0x1000_0000;
        assert!(
            used <= HEAP * 6,
            "the arena grew {} MiB for four 64 MiB heaps -- the freed head \
             and tail are not being reused",
            used / (1024 * 1024),
        );
        // And the heaps are all distinct and aligned.
        kept.sort();
        kept.dedup();
        assert_eq!(kept.len(), 4);
        for k in kept {
            assert_eq!(k % HEAP, 0);
        }
    }

    /// Bottom-up placement is first fit: with thousands of mappings and holes
    /// of every size, each `mmap` lands in the lowest hole that holds it (the
    /// one-pass search must agree with checking every page).
    #[test]
    fn bottom_up_placement_is_the_lowest_hole_that_fits() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x2000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x2000_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        // 3000 one-to-four-page mappings with alternating names (so they do
        // not merge), then every third one freed: holes of 1-4 pages.
        let mut at = Vec::new();
        for i in 0..3000u64 {
            let len = PAGE_SIZE * (1 + i % 4);
            let a = m.mmap(0, len, prot::READ, 0, if i % 2 == 0 { "a" } else { "b" }).unwrap() as u64;
            at.push((a, len));
        }
        for (i, &(a, len)) in at.iter().enumerate() {
            if i % 3 == 1 {
                m.unmap(a, len).unwrap();
            }
        }
        let lowest = |m: &MemoryMap, len: u64| {
            let mut a = page_align_up(m.arena[0].0);
            while !m.is_free_slow(a, a + len) {
                a += PAGE_SIZE;
            }
            a
        };
        for len in [PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 4 * PAGE_SIZE, 9 * PAGE_SIZE, PAGE_SIZE] {
            let want = lowest(&m, len);
            let got = m.mmap(0, len, prot::READ, 0, "c").unwrap() as u64;
            assert_eq!(got, want, "a {len:#x}-byte mapping");
        }
    }

    /// A range kept for fixed mappings (W3's exe slot and pinned-DLL band)
    /// takes no anonymous mapping once it is arena, bottom-up or top-down;
    /// a fixed mapping or a free hint still goes there.
    #[test]
    fn fixed_only_range_takes_no_placed_mapping() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x3000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        let anon = flags::PRIVATE | flags::ANONYMOUS;
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        m.adopt_arena(0x1000_0000, 0x3000_0000);
        m.keep_fixed_only(0x1000_0000, 0x2000_0000);
        let dll = m.mmap(0x1100_0000, 0x10_0000, prot::READ, anon | flags::FIXED_NOREPLACE, "dll").unwrap() as u64;
        assert_eq!(dll, 0x1100_0000);
        let heap = m.mmap(0, 64 << 20, prot::READ | prot::WRITE, anon, "heap").unwrap() as u64;
        assert!(heap >= 0x2000_0000, "an anonymous 64 MiB at {heap:#x}, in the band");
        let small = m.mmap(0, PAGE_SIZE, prot::READ, anon, "small").unwrap() as u64;
        assert!(small >= 0x2000_0000, "an anonymous page at {small:#x}, in the band");
        let hint = m.mmap(0x1200_0000, 0x10_0000, prot::READ, anon, "hinted").unwrap() as u64;
        assert_eq!(hint, 0x1200_0000);

        // Top-down (no `take`): the search steps below the range.
        let mut t = MemoryMap::new(0x40_1000);
        let top = t.mmap_next;
        t.keep_fixed_only(top - (16 << 20), top);
        let a = t.mmap(0, 64 << 20, prot::READ, anon, "low").unwrap() as u64;
        assert!(a + (64 << 20) <= top - (16 << 20), "a top-down 64 MiB at {a:#x}, in the range");
    }

    /// An exec's dead address space, donated back, is used before the arena
    /// grows again, and is never taken for zero.
    #[test]
    fn donated_arena_is_used_before_growing() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x5000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x5000_0000, std::sync::atomic::Ordering::SeqCst);

        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        m.mark_clean(0x1000_0000, 0x1800_0000);
        // Two touching spans merge; an overlapping one and an unaligned
        // sliver are ignored.
        m.donate_arena(&[
            (0x1000_0000, 0x1400_0000),
            (0x1400_0000, 0x1800_0000),
            (0x1200_0000, 0x1300_0000),
            (0x2000_0100, 0x2000_0200),
        ]);
        assert_eq!(m.arena_spans(), &[(0x1000_0000, 0x1800_0000)]);
        // Donated memory held something: none of it is known zero.
        assert!(m.clean_ranges().iter().all(|&(s, e)| e <= 0x1000_0000 || s >= 0x1800_0000));

        let got = m.mmap(0, 64 << 20, 0, 0, "heap").unwrap() as u64;
        assert!((0x1000_0000..0x1800_0000).contains(&got), "mapped at {got:#x}");
        assert_eq!(NEXT.load(std::sync::atomic::Ordering::SeqCst), 0x5000_0000);
    }

    /// `MAP_FIXED_NOREPLACE` refuses any existing mapping, reservations
    /// included, and succeeds on free space.
    #[test]
    fn fixed_noreplace_refuses_any_existing_mapping() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let base = 0x2000_0000;
        m.mmap(base, 0x4000, 0, anon | flags::FIXED, "reserved").unwrap();
        for prot in [0, prot::READ | prot::WRITE] {
            assert_eq!(
                m.mmap(base + 0x1000, 0x1000, prot, anon | flags::FIXED_NOREPLACE, "x"),
                Err(Errno::EEXIST),
                "NOREPLACE over a reservation, prot {prot}"
            );
            // And with MAP_FIXED as well: NOREPLACE wins.
            assert_eq!(
                m.mmap(base + 0x3000, 0x2000, prot, anon | flags::FIXED | flags::FIXED_NOREPLACE, "x"),
                Err(Errno::EEXIST),
            );
        }
        assert_eq!(m.mmap(base + 0x4000, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "x"), Ok(base + 0x4000));
        // Plain MAP_FIXED still replaces the guest's own mapping.
        assert_eq!(m.mmap(base, 0x1000, 3, anon | flags::FIXED, "y"), Ok(base));
    }

    /// The contexts and dispatch directory below `DISPATCH_DIR_END`, and the
    /// gap the browser shim lives in above the stack, are not free space,
    /// though no VMA records them.
    #[test]
    fn the_runtime_regions_are_not_free_space() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let gap = gex_abi::STACK_TOP + 0x30_0000;
        assert!(gap + 0x1000 <= MMAP_BOTTOM);
        for at in [0x10000, gex_abi::DISPATCH_DIR_END - 0x1000, gap] {
            assert_eq!(m.mmap(at, 0x1000, 0, anon | flags::FIXED_NOREPLACE, "r"), Err(Errno::EEXIST), "{at:#x}");
            assert_eq!(m.mmap(at, 0x1000, 3, anon | flags::FIXED, "f"), Err(Errno::ENOMEM), "{at:#x}");
            // A hint there is not honoured: the allocator places it instead.
            let got = m.mmap(at, 0x1000, 3, anon, "h").unwrap();
            assert!(!hits_runtime(got, got + 0x1000), "hint {at:#x} honoured at {got:#x}");
        }
        // A range that only straddles into the region is refused too.
        assert_eq!(
            m.mmap(gex_abi::STACK_TOP - 0x1000, 0x2000, 3, anon | flags::FIXED, "s"),
            Err(Errno::ENOMEM)
        );
        // Right above the directory and at the arena's bottom is fine.
        assert!(m.mmap(gex_abi::DISPATCH_DIR_END, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "ok").is_ok());
        assert!(m.mmap(MMAP_BOTTOM, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "ok").is_ok());
    }

    /// A MAP_FIXED above the arena moves the shared cursor past it; the
    /// bytes it stepped over are this map's, and the next mapping uses them
    /// instead of claiming more.
    #[test]
    fn memory_backed_for_a_fixed_mapping_is_reused() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let first = m.mmap(0, 0x1000, 3, anon, "a").unwrap();
        // Someone else sharing the cursor (the shim's heap) claims a span.
        let theirs = take(0x10_0000);
        let high = 0x2000_0000;
        assert_eq!(m.mmap(high, 0x1000, 3, anon | flags::FIXED, "high"), Ok(high));
        let cursor = NEXT.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(cursor, high + 0x1000);
        // The next ordinary mapping comes from below the fixed one, and not
        // from the span the other user took.
        let next = m.mmap(0, 0x10_0000, 3, anon, "b").unwrap();
        assert!(next < high && next > first, "{next:#x}");
        assert!(next + 0x10_0000 <= theirs || next >= theirs + 0x10_0000, "{next:#x} overlaps {theirs:#x}");
        assert_eq!(NEXT.load(std::sync::atomic::Ordering::SeqCst), cursor, "the arena grew");
    }

    /// A host's island (W3's pinned DLL band): a fixed mapping there is
    /// backed but never becomes arena, so once it is unmapped the next
    /// bottom-up mapping does not take its slot, and the image mapped there
    /// again gets its address back.
    #[test]
    fn an_islands_slot_is_not_reused_by_the_arena() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(start: u64, end: u64) -> Option<u64> {
            if start >= 0x1100_0000 && end <= 0x2000_0000 {
                return Some(BACKED_ISLAND);
            }
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x0800_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        // The arena's first chunk, full: a free slot anywhere else in the
        // arena would be the next bottom-up mapping's.
        m.mmap(0, 0x80_0000, 3, anon, "filler").unwrap();
        let pin = 0x1150_0000;
        let cursor = NEXT.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(m.mmap(pin, 0x10_0000, 3, anon | flags::FIXED_NOREPLACE, "dll"), Ok(pin));
        assert_eq!(NEXT.load(std::sync::atomic::Ordering::SeqCst), cursor, "the cursor moved");
        m.unmap(pin, 0x10_0000).unwrap();
        for _ in 0..4 {
            let a = m.mmap(0, 0x4_0000, 3, anon, "heap").unwrap();
            assert!(a + 0x4_0000 <= pin || a >= pin + 0x10_0000, "the slot went to {a:#x}");
        }
        assert_eq!(m.mmap(pin, 0x10_0000, 3, anon | flags::FIXED_NOREPLACE, "dll again"), Ok(pin));
    }

    #[test]
    fn clean_ranges_split_merge_and_answer() {
        let mut m = MemoryMap::new(0x10000);
        m.mark_clean(0x10000, 0x20000);
        m.mark_clean(0x30000, 0x40000);
        m.mark_clean(0x20000, 0x30000);
        assert_eq!(m.clean_ranges(), &[(0x10000, 0x40000)]);
        m.mark_dirty(0x18000, 0x19000);
        assert_eq!(m.clean_ranges(), &[(0x10000, 0x18000), (0x19000, 0x40000)]);
        assert_eq!(m.dirty_parts(0x8000, 0x50000), vec![(0x8000, 0x10000), (0x18000, 0x19000), (0x40000, 0x50000)]);
        assert_eq!(m.dirty_parts(0x11000, 0x12000), vec![]);
        // A mapping placed with contents, and a protection made writable,
        // are no longer known to be zero.
        m.insert(0x20000, 0x21000, prot::READ, "x");
        m.protect(0x30000, 0x1000, prot::READ | prot::WRITE).ok();
        assert_eq!(m.dirty_parts(0x19000, 0x40000), vec![(0x20000, 0x21000), (0x30000, 0x31000)]);
        m.mark_dirty(0, u64::MAX);
        assert!(m.clean_ranges().is_empty());
    }

    #[test]
    fn touching_anonymous_mappings_merge() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE | flags::FIXED;
        let base = 0x2000_0000;
        for i in 0..8 {
            m.mmap(base + i * 0x1000, 0x1000, 0, anon, "").unwrap();
        }
        assert_eq!(m.vmas().len(), 1, "{:?}", m.vmas());
        assert_eq!((m.vmas()[0].start, m.vmas()[0].end), (base, base + 0x8000));
        // A different permission, or a file's bytes, stay separate.
        m.mmap(base + 0x8000, 0x1000, prot::READ, anon, "").unwrap();
        m.mmap_from(base + 0x9000, 0x1000, 0, anon, "", Some(("f".into(), 0))).unwrap();
        m.mmap(base + 0xa000, 0x1000, 0, anon, "").unwrap();
        assert_eq!(m.vmas().len(), 4);
        // Unmapping from the middle of a merged mapping still splits it.
        m.unmap(base + 0x2000, 0x1000).unwrap();
        assert_eq!(m.vmas().len(), 5);
        assert!(m.check_access(base + 0x8000, 0x1000, prot::READ));
    }

    /// The arena is shared with the shim's heap through one cursor. A hint,
    /// MAP_FIXED or in-place mremap into a span the heap took must not put a
    /// guest mapping over it: musl (as zig builds it) passes mmap hints, and
    /// a benchmark's hundredth large malloc trapped inside the browser shim.
    #[test]
    fn a_placed_mapping_never_lands_in_the_runtimes_heap() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_start: u64, end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let a = m.mmap(0, 0x10_0000, 3, anon, "a").unwrap();
        let a_top = a + 0x80_0000; // the kernel claims a CHUNK at a time
        // The heap takes the next span; the kernel's next claim lands above.
        let heap = take(0x100_0000);
        assert_eq!(heap, a_top);
        let b = m.mmap(0, 0x90_0000, 3, anon, "b").unwrap();
        assert!(b >= heap + 0x100_0000);
        let inside = heap + 0x1000;
        // A hint there is ignored; the mapping goes somewhere that is ours.
        let h = m.mmap(inside, 0x2000, 3, anon, "h").unwrap();
        assert!(h + 0x2000 <= heap || h >= heap + 0x100_0000, "hint honoured in the heap: {h:#x}");
        // MAP_FIXED there is refused.
        assert_eq!(m.mmap(inside, 0x2000, 3, anon | flags::FIXED, "f"), Err(Errno::ENOMEM));
        // A mapping at the top of the kernel's first span does not grow in
        // place into the heap span above it.
        let edge = m.mmap(a_top - 0x1000, 0x1000, 3, anon | flags::FIXED, "e").unwrap();
        let (moved_to, moved) = m.mremap(edge, 0x1000, 0x3000, true).unwrap();
        assert!(moved && (moved_to + 0x3000 <= heap || moved_to >= heap + 0x100_0000), "{moved_to:#x}");
        assert_eq!(m.mremap(moved_to, 0x3000, 0x3000, false), Ok((moved_to, false)));
        // Within its own spans a hint is still honoured.
        let own = a + 0x20_0000;
        assert_eq!(m.mmap(own, 0x1000, 3, anon, "o"), Ok(own));
        // Below the arena (the placement window) is the guest's.
        let placed = gex_abi::PLACE_BOTTOM + 0x1000;
        assert_eq!(m.mmap(placed, 0x1000, 3, anon | flags::FIXED, "p"), Ok(placed));
    }

    #[test]
    fn brk_grows_and_reports() {
        let mut m = MemoryMap::new(0x40_1000);
        assert_eq!(m.brk(0), 0x40_1000);
        assert_eq!(m.brk(0x40_2000), 0x40_2000);
        assert_eq!(m.brk(0), 0x40_2000);
        // Shrinking back is allowed.
        assert_eq!(m.brk(0x40_1000), 0x40_1000);
        // Below the start is refused, reported as "no change".
        assert_eq!(m.brk(0x10), 0x40_1000);
    }

    /// The break stops at a mapping, as on Linux, but not at its own pages: a
    /// shrink leaves them recorded and growing back over them must work
    /// (glibc trims the heap top and grows it again all the time).
    #[test]
    fn brk_stops_at_a_mapping_but_not_at_its_own_heap() {
        let mut m = MemoryMap::new(0x40_1000);
        assert_eq!(m.brk(0x40_3000), 0x40_3000);
        assert_eq!(m.brk(0x40_1000), 0x40_1000);
        assert_eq!(m.brk(0x40_4000), 0x40_4000, "regrowing over the trimmed heap");
        m.mmap(0x40_6000, 0x1000, prot::READ, flags::FIXED | flags::ANONYMOUS, "reserved")
            .unwrap();
        assert_eq!(m.brk(0x40_8000), 0x40_4000, "the break must not grow over a mapping");
        assert_eq!(m.brk(0), 0x40_4000);
        assert_eq!(m.brk(0x40_6000), 0x40_6000, "up to the mapping is fine");
    }

    #[test]
    fn mmap_allocates_downwards_without_overlap() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x1000, prot::READ, flags::ANONYMOUS, "a")
            .unwrap();
        let b = m
            .mmap(0, 0x1000, prot::READ, flags::ANONYMOUS, "b")
            .unwrap();
        assert!(b < a, "expected top-down allocation, got {a:x} then {b:x}");
        assert!(a + 0x1000 <= MMAP_TOP);
        assert_eq!(a % PAGE_SIZE, 0);
    }

    #[test]
    fn mmap_reuses_a_hole_above_the_cursor() {
        // more than half the arena, mapped, a small mapping below it, then
        // freed: the next big request fits only in the hole above the cursor
        let mut m = MemoryMap::new(0x40_1000);
        let big = page_align_down((MMAP_TOP - MMAP_BOTTOM) / 5 * 3);
        let a = m.mmap(0, big, prot::READ, flags::ANONYMOUS, "a").unwrap();
        m.mmap(0, 0x2000, prot::READ, flags::ANONYMOUS, "b").unwrap();
        m.unmap(a, big).unwrap();
        let c = m.mmap(0, big, prot::READ, flags::ANONYMOUS, "c").unwrap();
        assert_eq!(c, a);
    }

    #[test]
    fn vma_at_finds_the_mapping_holding_an_address() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m.mmap(0, 0x4000, prot::READ, flags::ANONYMOUS, "x").unwrap();
        m.unmap(a + 0x1000, 0x1000).unwrap();
        for addr in [a, a + 0xfff, a + 0x2000, a + 0x3fff] {
            assert_eq!(m.vma_at(addr).map(|v| v.name.as_str()), Some("x"), "{addr:#x}");
        }
        for addr in [a + 0x1000, a + 0x1fff, a + 0x4000, 0] {
            assert!(m.vma_at(addr).is_none_or(|v| v.name != "x"), "{addr:#x}");
        }
        for addr in [0u64, 0x1000, a, a + 0x1800, a + 0x3000, MMAP_TOP - 1] {
            let want = m.vmas().iter().find(|v| v.start <= addr && addr < v.end).map(|v| v.start);
            assert_eq!(m.vma_at(addr).map(|v| v.start), want, "{addr:#x}");
        }
    }

    #[test]
    fn unmap_splits_a_mapping_in_two() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x4000, prot::READ, flags::ANONYMOUS, "x")
            .unwrap();
        m.unmap(a + 0x1000, 0x1000).unwrap();
        let covering: Vec<_> = m.vmas().iter().filter(|v| v.name == "x").collect();
        assert_eq!(covering.len(), 2);
        assert_eq!(covering[0].end, a + 0x1000);
        assert_eq!(covering[1].start, a + 0x2000);
        assert!(!m.check_access(a + 0x1000, 0x1000, prot::READ));
        assert!(m.check_access(a, 0x1000, prot::READ));
    }

    #[test]
    fn protect_narrows_to_the_requested_range() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x3000, prot::READ | prot::WRITE, flags::ANONYMOUS, "x")
            .unwrap();
        m.protect(a + 0x1000, 0x1000, prot::READ | prot::EXEC)
            .unwrap();
        assert!(m.check_access(a + 0x1000, 0x1000, prot::EXEC));
        assert!(!m.check_access(a, 0x1000, prot::EXEC));
        assert!(!m.check_access(a + 0x2000, 0x1000, prot::EXEC));
    }

    /// A range made read-only and writable again is one mapping again, as
    /// Linux merges it, so `mremap` of the whole of it works; neighbours
    /// that differ stay apart.
    #[test]
    fn protect_back_merges_and_mremap_takes_the_whole() {
        let mut m = MemoryMap::new(0x40_1000);
        let rw = prot::READ | prot::WRITE;
        let a = m.mmap(0, 0x4000, rw, flags::ANONYMOUS, "x").unwrap();
        m.protect(a + 0x1000, 0x1000, prot::READ).unwrap();
        assert_eq!(m.vmas().len(), 3);
        m.protect(a + 0x1000, 0x1000, rw).unwrap();
        assert_eq!(m.vmas().iter().filter(|v| v.start >= a && v.end <= a + 0x4000).count(), 1);
        assert!(m.mremap(a, 0x4000, 0x8000, true).is_ok());
        let b = m.mmap(0, 0x2000, rw, flags::ANONYMOUS, "y").unwrap();
        m.protect(b, 0x1000, prot::READ).unwrap();
        assert_eq!(m.vmas().iter().filter(|v| v.start >= b && v.end <= b + 0x2000).count(), 2);
    }

    #[test]
    fn fixed_mapping_replaces_whatever_was_there() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x2000, prot::READ, flags::ANONYMOUS, "x")
            .unwrap();
        let got = m
            .mmap(a, 0x1000, prot::WRITE, flags::ANONYMOUS | flags::FIXED, "y")
            .unwrap();
        assert_eq!(got, a);
        assert!(m.check_access(a, 0x1000, prot::WRITE));
    }

    #[test]
    fn check_access_spans_adjacent_mappings() {
        let mut m = MemoryMap::new(0x40_1000);
        m.insert(0x1000_0000, 0x1000_1000, prot::READ, "a");
        m.insert(0x1000_1000, 0x1000_2000, prot::READ, "b");
        assert!(m.check_access(0x1000_0800, 0x1000, prot::READ));
        assert!(!m.check_access(0x1000_1800, 0x1000, prot::READ));
    }

    /// docs/mem64.md §3: in an M64 process, data goes above 4 GiB, code stays
    /// below MMAP_TOP, and a high page cannot become code.
    #[test]
    fn a_64_bit_memory_puts_data_in_the_high_arena() {
        let mut m = MemoryMap::new(0x40_0000);
        m.set_high_arena(gex_abi::HIGH_ARENA_BASE, gex_abi::LIMIT64);
        // Unity's reservations: 512 MiB - 4 KiB with no access, twenty of them.
        let mut last = 0;
        for _ in 0..20 {
            let a = m.mmap(0, 0x1fff_f000, prot::NONE, 0, "res").unwrap();
            assert!(a >= gex_abi::HIGH_ARENA_BASE && a + 0x1fff_f000 <= gex_abi::LIMIT64);
            assert!(a > last);
            last = a;
        }
        let code = m.mmap(0, 0x10000, prot::READ | prot::WRITE | prot::EXEC, 0, "jit").unwrap();
        assert!(code < MMAP_TOP);
        assert_eq!(m.protect(last, 0x1000, prot::READ | prot::EXEC), Err(Errno::EACCES));
        assert!(m.protect(last, 0x1000, prot::READ | prot::WRITE).is_ok());
        // A hint or a fixed address up to the 64-bit limit is honoured.
        let fixed = 10u64 << 30;
        assert_eq!(m.mmap(fixed, 0x1000, prot::READ, flags::FIXED, "f"), Ok(fixed));
        // A file mapping stays low whatever its protection (ld.so reserves a
        // library read-only, then maps its text inside), and so does a
        // reservation made with `mmap_low` (the loader's, for code).
        let lib = m.mmap_from(0, 0x20000, prot::READ, 0, "libx.so", Some(("libx.so".into(), 0))).unwrap();
        assert!(lib < MMAP_TOP);
        let res = m.mmap_low(0, 0x40_0000, prot::NONE, 0, "").unwrap();
        assert!(res < MMAP_TOP);
        // ... and refuses executable pages above 4 GiB in either form.
        assert_eq!(m.mmap(11u64 << 30, 0x1000, prot::READ | prot::EXEC, flags::FIXED, "x"), Err(Errno::EACCES));
    }

    /// An M32 map is unchanged, and running out of address space is recorded
    /// for the next start (docs/mem64.md §5).
    #[test]
    fn a_32_bit_memory_records_running_out_of_address_space() {
        let mut m = MemoryMap::new(0x40_0000);
        let a = m.mmap(0, 0x1000, prot::READ, 0, "x").unwrap();
        assert!(a < MMAP_TOP);
        assert_eq!(m.mmap(1 << 33, 0x1000, prot::READ, flags::FIXED, "f"), Err(Errno::ENOMEM));
        while m.mmap(0, 0x1fff_f000, prot::NONE, 0, "res").is_ok() {}
        assert!(ADDRESS_SPACE_SHORT.load(std::sync::atomic::Ordering::Relaxed) >= 0x1fff_f000);
    }
}
