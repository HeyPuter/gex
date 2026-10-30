//! Dense block tables, the shim's half (`gex_emit::dense`): the slot maps in
//! this process's memory and the numbering of its blocks.
//!
//! * A runtime translation is given its dense base when it is queued for
//!   installation ([`register`]), written into the module's bytes, so every
//!   Worker instantiates the same bytes at the same place in its `blocks`.
//! * Its slots point at it once a Worker has installed it ([`publish`], in
//!   `gex_jit_installed`, with the directory). A Worker that has not
//!   instantiated it yet finds the miss stub at its index, and the miss path
//!   ([`missing`]) traps to JavaScript to catch up first.
//! * The next free index lives in the reserved low region ([`NEXT`]), so a
//!   forked child, whose low region is a copy, numbers above everything its
//!   parent's modules (which it may adopt) hold; the maps themselves are in
//!   the arena, which a child does not inherit, so a child starts with fresh
//!   ones ([`child`]) and refills them from what it adopts ([`adopt`]).

use gex_emit::dense::{self, Section};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Whether this process numbers blocks densely (`gex_set_dense`).
static ON: AtomicBool = AtomicBool::new(false);
/// `gex_set_dense(2)`: a miss on a block this Worker lacks translates it
/// again rather than trapping to catch up (for measuring).
static NO_CATCHUP: AtomicBool = AtomicBool::new(false);

/// The next free index in `blocks`, a `u32` in the reserved low region just
/// after the map words (0 reads as 1: index 0 is the miss stub).
pub(crate) const NEXT: u64 = dense::MAPS + 4 * dense::TABLES as u64;
const _: () = assert!(NEXT + 4 <= gex_abi::NEST_EXITS);

static LOCK: AtomicU32 = AtomicU32::new(0);

struct Lock;
impl Lock {
    fn take() -> Lock {
        while LOCK.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        Lock
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        LOCK.store(0, Ordering::Release);
    }
}

fn word(addr: u64) -> &'static AtomicU32 {
    // SAFETY: a word of the reserved low region or of a slot map, both in
    // this module's linear memory (the guest's addresses are linear ones);
    // aligned, and only ever accessed atomically.
    unsafe { &*(std::ptr::with_exposed_provenance::<u32>(addr as usize) as *const AtomicU32) }
}

pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Dense numbering on or off for this process, before anything is
/// translated (and again in a forked child, whose shim is new): 0 off, 1 on,
/// 2 on without catching up.
#[no_mangle]
pub extern "C" fn gex_set_dense(on: i32) {
    ON.store(on != 0, Ordering::Relaxed);
    NO_CATCHUP.store(on == 2, Ordering::Relaxed);
    dense::set(on != 0);
    // Every table's map when a directory entry first names it
    // (`note_entry`): a process that translates nothing has none.
}

#[no_mangle]
pub extern "C" fn gex_dense_on() -> i32 {
    on() as i32
}

/// How long every Worker's `blocks` must be for what this process numbered.
#[no_mangle]
pub extern "C" fn gex_dense_size() -> u32 {
    word(NEXT).load(Ordering::Acquire).max(1)
}

/// Slot-map pages (`SLOTS_PER_PAGE` slots each) a table's map can cover.
const MAP_PAGES: u64 = dense::MAP_SLOTS.div_ceil(gex_abi::SLOTS_PER_PAGE);
const _: () = assert!(MAP_PAGES <= u16::MAX as u64);

/// The pages each table's map covers, `lo | hi << 16` (`lo..hi`; 0: none).
/// The map word in the low region is the map's address less `4 * lo`'s
/// slot, so a slot's word is at `map + 4 * slot` (u32 arithmetic, as the
/// dispatch code computes it) for every slot in the window, and nothing is
/// there for slots outside it: every page a directory entry names in the
/// table is in it ([`note_entry`] grows it first). A window grows by a new
/// map (the old one stays, read by dispatch that loaded it before) with
/// the old one's words, its word in the low region stored before this.
static WIN: [AtomicU32; dense::TABLES] = [const { AtomicU32::new(0) }; dense::TABLES];

fn window(k: usize) -> (u64, u64) {
    let w = WIN[k - dense::FIRST_TABLE].load(Ordering::Acquire);
    ((w & 0xffff) as u64, (w >> 16) as u64)
}

fn map_word(k: usize) -> &'static AtomicU32 {
    word(dense::MAPS + 4 * (k - dense::FIRST_TABLE) as u64)
}

/// The address of slot `slot`'s word in a map whose low-region word is `m`.
fn at(m: u32, slot: u64) -> u64 {
    m.wrapping_add(4 * slot as u32) as u64
}

/// Table `k`'s map word when its window holds `slot` (none is made here).
fn covering(k: usize, slot: u64) -> Option<u32> {
    let (lo, hi) = window(k);
    let page = slot / gex_abi::SLOTS_PER_PAGE;
    if page < lo || page >= hi {
        return None;
    }
    let m = map_word(k).load(Ordering::Acquire);
    (m != 0).then_some(m)
}

/// Table `k`'s map grown to hold slots `from..to`, with [`LOCK`] held; its
/// low-region word, or `None` (not dense, or no arena).
fn grow_locked(k: usize, from: u64, to: u64) -> Option<u32> {
    if !on() || !dense::table(k) || from >= dense::MAP_SLOTS {
        return None;
    }
    let spp = gex_abi::SLOTS_PER_PAGE;
    let (want_lo, want_hi) = (from / spp, to.min(dense::MAP_SLOTS).div_ceil(spp).max(from / spp + 1));
    let (lo, hi) = window(k);
    let m = map_word(k).load(Ordering::Acquire);
    if m != 0 && lo <= want_lo && want_hi <= hi {
        return Some(m);
    }
    // Twice the old span, so a table a process keeps adding to is copied a
    // few times, not once a page; the old maps together cost what the last one does.
    let (u_lo, u_hi) = if hi > lo { (lo.min(want_lo), hi.max(want_hi)) } else { (want_lo, want_hi) };
    let span = (u_hi - u_lo).max(2 * (hi - lo)).min(MAP_PAGES);
    let n_lo = u_lo.min(MAP_PAGES - span);
    let n_hi = n_lo + span;
    // Whole wasm pages: the arena's cursor stays page-aligned.
    let len = (4 * span * spp).next_multiple_of(65536);
    let Some(base) = crate::arena_take(len) else {
        crate::warn_line(&format!("[gex] no arena for dispatch table {k}'s slot map ({} KiB)\n", len >> 10));
        return None;
    };
    crate::memmap::claim(crate::memmap::Kind::Heap, 0, base, base + len, "dense slot map");
    // Fresh arena is zero; the words the old window held come along.
    if hi > lo && m != 0 {
        for slot in lo * spp..hi * spp {
            let v = word(at(m, slot)).load(Ordering::Acquire);
            if v != 0 {
                word(base + 4 * (slot - n_lo * spp)).store(v, Ordering::Relaxed);
            }
        }
    }
    let nm = (base as u32).wrapping_sub(4 * (n_lo * spp) as u32);
    if nm == 0 {
        // 0 reads as a classic table: never the word of a dense one.
        crate::warn_line(&format!("[gex] dispatch table {k}'s slot map lands on word 0; left classic\n"));
        return None;
    }
    if crate::tracing() {
        crate::trace_line(&format!("[dense] table {k}'s slot map pages {n_lo}..{n_hi} at {base:#x}..{:#x}\n", base + len));
    }
    map_word(k).store(nm, Ordering::Release);
    WIN[k - dense::FIRST_TABLE].store((n_lo | n_hi << 16) as u32, Ordering::Release);
    Some(nm)
}

/// Whether table `k` is numbered densely in this process (its map holds the
/// pages directory entries name in it, made as they are noted).
pub(crate) fn dense_table(k: usize) -> bool {
    on() && dense::table(k)
}

/// Make sure the map for the table a directory entry names holds that
/// entry's page. Call before the entry is written.
pub(crate) fn note_entry(entry: u32) {
    let k = gex_abi::dispatch_entry_table(entry);
    if !on() || !dense::table(k) {
        return;
    }
    let base = gex_abi::dispatch_entry_base(entry) as u64;
    if covering(k, base).is_some() && covering(k, base + gex_abi::SLOTS_PER_PAGE - 1).is_some() {
        return;
    }
    let _l = Lock::take();
    let _ = grow_locked(k, base, base + gex_abi::SLOTS_PER_PAGE);
}

/// Store `d` at slot `slot` of table `k` (only over 0 with `only_empty`),
/// the window grown to hold it; [`LOCK`] held.
fn put_locked(k: usize, slot: u64, d: u32, only_empty: bool) {
    let Some(m) = grow_locked(k, slot, slot + 1) else { return };
    let w = word(at(m, slot));
    if only_empty {
        let _ = w.compare_exchange(0, d, Ordering::AcqRel, Ordering::Relaxed);
    } else {
        w.store(d, Ordering::Release);
    }
}

/// Number a module queued for installation: give it the next range of
/// `blocks` and write that into its bytes. `None` for a module with no dense
/// elements.
pub(crate) fn register(wasm: &mut [u8]) -> Option<Section> {
    if !on() {
        return None;
    }
    let Some(at) = dense::locate(wasm) else {
        if dense::has_section(wasm) {
            crate::warn_line("[gex] a translation's dense segment cannot be read; it would land on index 0\n");
        }
        return None;
    };
    let mut s = dense::section(wasm, &at)?;
    let n = s.span();
    let w = word(NEXT);
    let mut cur = w.load(Ordering::Acquire);
    let base = loop {
        let b = cur.max(1);
        match w.compare_exchange(cur, b + n, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break b,
            Err(now) => cur = now,
        }
    };
    dense::set_base(wasm, &at, base);
    s.base = base;
    Some(s)
}

fn write(s: &Section, only_empty: bool) {
    let _l = Lock::take();
    for (j, &e) in s.entries.iter().enumerate() {
        let k = gex_abi::dispatch_entry_table(e);
        if !dense_table(k) {
            continue;
        }
        let slot = gex_abi::dispatch_entry_base(e) as u64;
        if slot >= dense::MAP_SLOTS {
            continue;
        }
        put_locked(k, slot, s.word(j), only_empty);
    }
}

/// A registered module is installed: its slots point at its blocks.
pub(crate) fn publish(s: &Section) {
    write(s, false);
    // TEST(gex_test_dense_holes): the word one past this module's last
    // block, in its last entry, when that entry has arms to spare.
    if HOLES.load(Ordering::Relaxed) && s.entries.len() as u32 % dense::ARMS != 0 {
        HOLE.store(((s.base + s.span() - 1) << 8) | (dense::ARMS - 1), Ordering::Relaxed);
    }
}

/// `gex_test_dense_holes(1)` (GEX_TEST_DENSE_HOLES): a test switch, off in
/// every real run. A miss at a slot no block was
/// given first has its word pointed past the last block of the newest
/// module ([`test_hole`]), as a word left stale would be; the retried
/// dispatch must reach the miss stub through that entry's `br_table`
/// default and the address be translated, not trap.
static HOLES: AtomicBool = AtomicBool::new(false);
static HOLE: AtomicU32 = AtomicU32::new(0);
static HOLES_MADE: AtomicU32 = AtomicU32::new(0);
static HOLES_REACHED: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn gex_test_dense_holes(on: i32) {
    HOLES.store(on != 0, Ordering::Relaxed);
}

thread_local! {
    /// The last (pc, word) [`test_hole`] wrote.
    static HOLE_AT: std::cell::Cell<(u64, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// TEST(gex_test_dense_holes): at a miss, before the dense catch-up. A slot
/// with no word gets the hole word; a miss at the slot it was written for,
/// the word unchanged, came back through the hole.
pub(crate) fn test_hole(pc: u64) {
    if !HOLES.load(Ordering::Relaxed) {
        return;
    }
    let Some(w) = slot_word(pc, crate::thread_alt()) else { return };
    let now = w.load(Ordering::Acquire);
    if now != 0 {
        if HOLE_AT.with(|h| h.get()) == (pc, now) {
            HOLE_AT.with(|h| h.set((0, 0)));
            let n = HOLES_REACHED.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                crate::warn_line(&format!(
                    "[gex] test: {n} of {} dense holes reached the miss stub through their entry\n",
                    HOLES_MADE.load(Ordering::Relaxed)
                ));
            }
        }
        return;
    }
    let hole = HOLE.load(Ordering::Relaxed);
    let _l = Lock::take();
    let Some(w) = slot_word(pc, crate::thread_alt()) else { return };
    if hole != 0 && w.compare_exchange(0, hole, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        HOLES_MADE.fetch_add(1, Ordering::Relaxed);
        HOLE_AT.with(|h| h.set((pc, hole)));
    }
}

/// A module a forked child inherited (its base is the parent's, below this
/// process's `NEXT`): its slots point at it where nothing newer does.
#[no_mangle]
pub extern "C" fn gex_dense_adopt(ptr: u32, len: u32) -> i32 {
    if !on() {
        return 0;
    }
    let Some(s) = Section::decode(crate::shim_slice(ptr, len)) else { return -1 };
    let end = s.base + s.span();
    word(NEXT).fetch_max(end, Ordering::AcqRel);
    write(&s, true);
    0
}

/// A forked child, before it runs: the maps its low region names are its
/// parent's (in an arena it did not inherit); start empty ones.
pub(crate) fn child() {
    let mut had = [false; dense::TABLES];
    for (k, h) in had.iter_mut().enumerate() {
        *h = word(dense::MAPS + 4 * k as u64).swap(0, Ordering::AcqRel) != 0;
        WIN[k].store(0, Ordering::Release);
    }
    if !had.iter().any(|&h| h) {
        return;
    }
    // The directory is the parent's: a window over every page it names in
    // each table that had a map.
    let mut span = [(u64::MAX, 0u64); dense::TABLES];
    let dir = gex_abi::dispatch_dir();
    for page in 0..gex_abi::dispatch_max_pages() {
        let e = word(dir + 4 * page).load(Ordering::Relaxed);
        if e == 0 {
            continue;
        }
        let k = gex_abi::dispatch_entry_table(e);
        if !dense::table(k) || !had[k - dense::FIRST_TABLE] {
            continue;
        }
        let b = gex_abi::dispatch_entry_base(e) as u64;
        let s = &mut span[k - dense::FIRST_TABLE];
        *s = (s.0.min(b), s.1.max(b + gex_abi::SLOTS_PER_PAGE));
    }
    let _l = Lock::take();
    for (i, &(from, to)) in span.iter().enumerate() {
        if had[i] && from < to {
            let _ = grow_locked(dense::FIRST_TABLE + i, from, to);
        }
    }
}

/// The `blocks` index the slot of `pc` names in this process, or 0.
pub(crate) fn index(pc: u64) -> u32 {
    dense::index_of(lookup(pc))
}

/// The slot map word of `pc` in this process, or 0: its index in `blocks`,
/// and its arm in that entry (`gex_emit::dense`).
pub(crate) fn lookup(pc: u64) -> u32 {
    lookup_in(pc, crate::thread_alt())
}

/// [`lookup`] in the directory of mode `alt` (`gex_abi::page_key`) rather
/// than the one this thread runs in.
pub(crate) fn lookup_in(pc: u64, alt: bool) -> u32 {
    slot_word(pc, alt).map_or(0, |w| w.load(Ordering::Acquire))
}

/// The slot map word of `pc` in mode `alt`'s directory, where its page is
/// in a dense table.
fn slot_word(pc: u64, alt: bool) -> Option<&'static AtomicU32> {
    if !on() {
        return None;
    }
    let page = gex_abi::dispatch_page(pc);
    if page >= gex_abi::dispatch_max_pages() {
        return None;
    }
    let dir = crate::dir_word(gex_abi::page_key(pc, alt));
    if dir == u64::MAX {
        return None;
    }
    let entry = word(dir).load(Ordering::Acquire);
    let k = gex_abi::dispatch_entry_table(entry);
    if entry == 0 || !dense::table(k) {
        return None;
    }
    let slot = gex_abi::dispatch_index(pc, entry);
    if slot >= dense::MAP_SLOTS {
        return None;
    }
    covering(k, slot).map(|m| word(at(m, slot)))
}

thread_local! {
    /// The last (pc, index) this Worker trapped to catch up on.
    static TRIED: std::cell::Cell<(u64, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// A miss at `pc` whose slot names a block this process has (`lookup`): this
/// Worker has not instantiated it. True the first time for a (pc, block):
/// the caller traps so JavaScript catches up, and the dispatch is retried. A
/// second miss there means nothing this Worker can install holds it (a
/// single-threaded process keeps no registry), and the ordinary miss path
/// translates it again.
pub(crate) fn missing(pc: u64) -> bool {
    if NO_CATCHUP.load(Ordering::Relaxed) {
        return false;
    }
    let d = lookup(pc);
    if d == 0 {
        return false;
    }
    TRIED.with(|t| t.replace((pc, d)) != (pc, d))
}

/// A retired page's slots (its directory entry `entry`) name nothing.
pub(crate) fn clear_page(entry: u32) {
    let k = gex_abi::dispatch_entry_table(entry);
    if !dense_table(k) {
        return;
    }
    let _l = Lock::take();
    let base = gex_abi::dispatch_entry_base(entry) as u64;
    for slot in base..(base + gex_abi::SLOTS_PER_PAGE).min(dense::MAP_SLOTS) {
        if let Some(m) = covering(k, slot) {
            word(at(m, slot)).store(0, Ordering::Release);
        }
    }
}

/// Allocate `n` indices outside any module (bridges into the function table).
pub(crate) fn alloc(n: u32) -> u32 {
    let w = word(NEXT);
    let mut cur = w.load(Ordering::Acquire);
    loop {
        let b = cur.max(1);
        match w.compare_exchange(cur, b + n, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return b,
            Err(now) => cur = now,
        }
    }
}

/// Point one slot (`table << 24 | slot`) at slot map word `w` (`index << 8
/// | arm`): a bridge's arm of an entry bridges share.
pub(crate) fn set_slot_word(entry: u32, w: u32) {
    let k = gex_abi::dispatch_entry_table(entry);
    if !dense_table(k) {
        return;
    }
    let slot = gex_abi::dispatch_entry_base(entry) as u64;
    if slot < dense::MAP_SLOTS {
        let _l = Lock::take();
        put_locked(k, slot, w, false);
    }
}

thread_local! {
    /// The index the last catch-up trap was for (`gex_dense_want`), or 0.
    static WANT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Note the index `pc`'s slot names, for JavaScript to instantiate.
pub(crate) fn want(pc: u64) {
    let d = index(pc);
    WANT.with(|w| w.set(d));
}

/// The index this Worker trapped for and must instantiate (0 for none);
/// reading it clears it.
#[no_mangle]
pub extern "C" fn gex_dense_want() -> u32 {
    WANT.with(|w| w.replace(0))
}

/// Whether `gex_dense_want` has an index to give (`gex_after_trap`).
pub(crate) fn wanted() -> bool {
    WANT.with(|w| w.get()) != 0
}

/// How many `blocks` entries registry module `i` holds (0: not a dense
/// module, or one that died). A Worker leaves such a module to the miss that
/// wants one of them (`gex_dense_want`) rather than installing it to catch up.
#[no_mangle]
pub extern "C" fn gex_registry_dense_n(i: u32) -> u32 {
    crate::with_jit_only(|j| j.registry_dense.get(i as usize).map_or(0, |&(_, n)| n))
}

/// The registry module (`gex_module_ptr`) holding index `d`, newest first,
/// or -1.
#[no_mangle]
pub extern "C" fn gex_dense_owner(d: u32) -> i32 {
    crate::with_jit_only(|j| {
        j.registry_dense
            .iter()
            .rposition(|&(b, n)| d >= b && d < b + n)
            .map_or(-1, |i| i as i32)
    })
}
