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
/// translated (and again in a forked child, whose shim is new).
#[no_mangle]
pub extern "C" fn gex_set_dense(on: i32) {
    ON.store(on != 0, Ordering::Relaxed);
    NO_CATCHUP.store(on == 2, Ordering::Relaxed);
    dense::set(on != 0);
    // The first dense table's map now; the others when a directory entry
    // first names them (`note_entry`).
    let _ = map(dense::FIRST_TABLE);
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

/// Table `k`'s slot map, made on first need. Call before any directory
/// entry names a page in table `k` (`dir_note` does), so dispatch never
/// finds a dense page with a classic (empty) table behind it.
pub(crate) fn map(k: usize) -> Option<u64> {
    if !on() || !dense::table(k) {
        return None;
    }
    let w = word(dense::MAPS + 4 * (k - dense::FIRST_TABLE) as u64);
    let m = w.load(Ordering::Acquire);
    if m != 0 {
        return Some(m as u64);
    }
    let _l = Lock::take();
    let m = w.load(Ordering::Acquire);
    if m != 0 {
        return Some(m as u64);
    }
    // Fresh arena is zero (never written): untouched, it costs address space
    // only, and only the pages of slots written become resident.
    // Whole wasm pages: the arena's cursor must stay page-aligned, since the
    // guest's mappings come from it next (a 40,000,000-byte span left it at
    // ...a00 and `mmap` handed a thread stack that address).
    let len = (4 * dense::MAP_SLOTS).next_multiple_of(65536);
    let Some(at) = crate::arena_take(len) else {
        crate::warn_line(&format!("[gex] no arena for dispatch table {k}'s slot map ({} MiB)\n", len >> 20));
        return None;
    };
    crate::memmap::claim(crate::memmap::Kind::Heap, 0, at, at + len, "dense slot map");
    if crate::tracing() {
        crate::trace_line(&format!("[dense] table {k}'s slot map at {at:#x}..{:#x}\n", at + len));
    }
    w.store(at as u32, Ordering::Release);
    Some(at)
}

/// Make sure the map for the table a directory entry names exists.
pub(crate) fn note_entry(entry: u32) {
    if on() {
        let _ = map(gex_abi::dispatch_entry_table(entry));
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
    let n = s.entries.len() as u32;
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
    for (j, &e) in s.entries.iter().enumerate() {
        let Some(m) = map(gex_abi::dispatch_entry_table(e)) else { continue };
        let slot = gex_abi::dispatch_entry_base(e) as u64;
        if slot >= dense::MAP_SLOTS {
            continue;
        }
        let w = word(m + 4 * slot);
        let d = s.base + j as u32;
        if only_empty {
            let _ = w.compare_exchange(0, d, Ordering::AcqRel, Ordering::Relaxed);
        } else {
            w.store(d, Ordering::Release);
        }
    }
}

/// A registered module is installed: its slots point at its blocks.
pub(crate) fn publish(s: &Section) {
    write(s, false);
}

/// A module a forked child inherited (its base is the parent's, below this
/// process's `NEXT`): its slots point at it where nothing newer does.
#[no_mangle]
pub extern "C" fn gex_dense_adopt(ptr: u32, len: u32) -> i32 {
    if !on() {
        return 0;
    }
    let Some(s) = Section::decode(crate::shim_slice(ptr, len)) else { return -1 };
    let end = s.base + s.entries.len() as u32;
    word(NEXT).fetch_max(end, Ordering::AcqRel);
    write(&s, true);
    0
}

/// A forked child, before it runs: the maps its low region names are its
/// parent's (in an arena it did not inherit); start empty ones.
pub(crate) fn child() {
    for k in 0..dense::TABLES {
        let w = word(dense::MAPS + 4 * k as u64);
        if w.swap(0, Ordering::AcqRel) != 0 {
            let _ = map(dense::FIRST_TABLE + k);
        }
    }
}

/// The index in `blocks` the slot of `pc` holds in this process, or 0.
pub(crate) fn lookup(pc: u64) -> u32 {
    if !on() {
        return 0;
    }
    let page = gex_abi::dispatch_page(pc);
    if page >= gex_abi::dispatch_max_pages() {
        return 0;
    }
    let entry = word(gex_abi::dispatch_dir() + 4 * page).load(Ordering::Acquire);
    let k = gex_abi::dispatch_entry_table(entry);
    if entry == 0 || !dense::table(k) {
        return 0;
    }
    let m = word(dense::MAPS + 4 * (k - dense::FIRST_TABLE) as u64).load(Ordering::Acquire);
    if m == 0 {
        return 0;
    }
    let slot = gex_abi::dispatch_index(pc, entry);
    if slot >= dense::MAP_SLOTS {
        return 0;
    }
    word(m as u64 + 4 * slot).load(Ordering::Acquire)
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

/// Point one slot (`table << 24 | slot`) at index `d`.
pub(crate) fn set_slot(entry: u32, d: u32) {
    let Some(m) = map(gex_abi::dispatch_entry_table(entry)) else { return };
    let slot = gex_abi::dispatch_entry_base(entry) as u64;
    if slot < dense::MAP_SLOTS {
        word(m + 4 * slot).store(d, Ordering::Release);
    }
}

thread_local! {
    /// The index the last catch-up trap was for (`gex_dense_want`), or 0.
    static WANT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Note the index `pc`'s slot names, for JavaScript to instantiate.
pub(crate) fn want(pc: u64) {
    let d = lookup(pc);
    WANT.with(|w| w.set(d));
}

/// The index this Worker trapped for and must instantiate (0 for none);
/// reading it clears it.
#[no_mangle]
pub extern "C" fn gex_dense_want() -> u32 {
    WANT.with(|w| w.replace(0))
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
