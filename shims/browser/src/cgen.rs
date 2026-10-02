//! The function tier's whole-image units (format `gex-cgen-image 3`) in the
//! browser: the shim's half (docs/browser-units-plan.md, step 1).
//!
//! What stays here is small: which images were mapped, which unit each one
//! wants (its key, `gex_cgen::image::image_key`, the same function the native
//! host uses), that the mapped bytes are the ones the unit was built from,
//! and the publish -- page bases, the entries' slots, the directory -- as the
//! native `publish_cgen_unit` does it. A unit's wasm never enters the shim's
//! heap (noita.exe's is ~195 MiB): JavaScript fetches and compiles it, hands
//! over only its entry list, and fills each Worker's dispatch and body tables
//! from the unit's own function table (`gex_fnref`). Units are kept in a list
//! of their own, not in the module registry: the registry may be rebuilt
//! (`backfill_registry`), and a unit has no bytes to keep in it.
//!
//! The protocol with `gex.mjs`:
//!   - `gex_cgen_wanted()` names a placed unit waiting for its bytes;
//!     `gex_cgen_key`/`gex_cgen_path` say which;
//!   - JS loads `<key>/c.wasm` and `<key>/c.entries` and calls
//!     `gex_cgen_publish(u, entries, n)`, or `gex_cgen_missing(u)`;
//!   - every Worker installs published units in order (`gex_cgen_count`,
//!     `gex_cgen_regs`), reports with `gex_cgen_have(n)` and its body tables'
//!     sizes with `gex_cgen_body_size` (one body table per dispatch table,
//!     `gexc.bodies0..7`); the publishing Worker then calls
//!     `gex_cgen_installed(u)`, which writes the directory.
//!
//! The body table's default entry is `gex_cgen_leave`: -1, as natively. What
//! C code does when it leaves is the units' business, not this file's.

use gex_kernel::GuestMem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 0 off, 1 use a unit where one exists, 2 also refuse a program image
/// without one (`--cgen`, as natively).
static MODE: AtomicU32 = AtomicU32::new(0);

/// An image the NT layer mapped, waiting to be placed.
struct Pending {
    path: String,
    base: u64,
    pe: Arc<gex_ld::pe::PeImage>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

/// A unit placed for an image: wanted until JS brings its bytes, then
/// published.
struct Unit {
    key: String,
    path: String,
    base: u64,
    /// A Wine DLL: tier-1 is fine without a unit even under `--cgen`.
    wine: bool,
    /// Each executable page as the unit was built from it, hashed.
    pages: Vec<(u64, u64)>,
    /// Once published: (entry's index in the unit, its dispatch table, its
    /// slot there, alone in the slot), flattened for JS.
    regs: Vec<u32>,
    /// Pages this unit was the first to number.
    dirs: Vec<(u64, u32)>,
    /// What the dispatch table must hold.
    slots: u64,
}

static UNITS: Mutex<Vec<Unit>> = Mutex::new(Vec::new());
/// Units waiting for their bytes, oldest first (indices into `UNITS`).
static WANTED: Mutex<std::collections::VecDeque<usize>> = Mutex::new(std::collections::VecDeque::new());
/// Published units, in publish order: what every Worker installs.
static PUBLISHED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Each Worker's body-table size, by Worker; `gex_abi::BODY_LIMIT` is the least.
static BODY_SIZES: Mutex<std::collections::BTreeMap<u32, [u32; gex_abi::DISPATCH_TABLES]>> =
    Mutex::new(std::collections::BTreeMap::new());
/// The pages installed units cover, hashed as built (`on_retire`).
static UNIT_PAGES: Mutex<Option<std::collections::HashMap<u64, u64>>> = Mutex::new(None);

thread_local! {
    /// How many published units this Worker has installed.
    static HAVE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether units are on.
pub(crate) fn on() -> bool {
    MODE.load(Ordering::Relaxed) != 0
}

/// Turn the function tier on (1) or require it for program images (2). Before
/// the process boots, so the exe and ntdll are seen.
#[no_mangle]
pub extern "C" fn gex_cgen_set_mode(mode: u32) {
    MODE.store(mode.min(2), Ordering::Relaxed);
    if mode == 0 {
        return;
    }
    static HOOK: OnceLock<()> = OnceLock::new();
    if HOOK.set(()).is_ok() {
        gex_kernel::nt::section::set_image_mapped_hook(|m| {
            lock(&PENDING).push(Pending { path: m.path.to_string(), base: m.base, pe: Arc::clone(m.pe) });
        });
    }
}

/// A hash of guest page `page` as it is now (the native host's `page_hash`).
fn page_hash(memory: &[u8], page: u64) -> u64 {
    let lo = (page * gex_abi::DISPATCH_PAGE) as usize;
    let hi = (lo + gex_abi::DISPATCH_PAGE as usize).min(memory.len());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in memory.get(lo..hi).unwrap_or(&[]).chunks(8) {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn is_wine(path: &str) -> bool {
    path.to_ascii_lowercase().contains("/drive_c/windows/")
}

/// Place what was mapped since the last call: the key, and the check that
/// the mapped code is what the unit was built from. Returns whether a unit
/// now waits for JavaScript (the caller traps to it). A mismatch ends the
/// run, as natively: running a unit over different code is wrong code.
pub(crate) fn drain() -> bool {
    if !on() {
        return false;
    }
    let pending = std::mem::take(&mut *lock(&PENDING));
    // The NT trampoline page with the first images (the exe says the ABI),
    // as natively: every Wine DLL's unix calls and syscalls go through it.
    static TRAMPOLINES: OnceLock<()> = OnceLock::new();
    if let Some(p) = pending.first() {
        if TRAMPOLINES.set(()).is_ok() {
            let (image, _) = gex_kernel::nt::boot::trampoline_input(!p.pe.is64);
            place(&image, "[the NT trampolines]", image.bias, true);
        }
    }
    for p in pending {
        let (image, _) = match gex_xlate::func::pe_input(&p.pe, p.base) {
            Ok(x) => x,
            Err(e) => {
                crate::warn_line(&format!("gex-cgen: {}: placing the image at {:#x}: {e}; tier-1\n", p.path, p.base));
                continue;
            }
        };
        let wine = is_wine(&p.path);
        place(&image, &p.path, p.base, wine);
    }
    !lock(&WANTED).is_empty()
}

/// Want the unit for `image`, mapped at `base`: its key, and the check that
/// the mapped code is what the unit was built from (a mismatch ends the run,
/// as natively: a unit over different code is wrong code). `optional`: a
/// missing unit is fine even under `--cgen` (Wine's DLLs, the trampolines).
fn place(image: &gex_ld::Image, path: &str, base: u64, optional: bool) {
    let memory = crate::guest_bytes();
    let mut pages = Vec::new();
    for seg in image.segments.iter().filter(|s| s.executable) {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        let live = memory.get(lo..lo + n);
        if let Some(i) = live.and_then(|live| (0..n).find(|&i| live[i] != seg.bytes[i])) {
            crate::warn_line(&format!(
                "gex-cgen: {path}: the mapped code at {:#x} differs from the image (guest {:#04x}, image {:#04x}); refusing its unit\n",
                seg.vaddr + i as u64,
                memory[lo + i],
                seg.bytes[i]
            ));
            crate::nt::end_process(0xc000_0005);
        }
        let first = seg.vaddr / gex_abi::DISPATCH_PAGE;
        let last = (seg.vaddr + n as u64 - 1) / gex_abi::DISPATCH_PAGE;
        for page in first..=last {
            pages.push((page, page_hash(memory, page)));
        }
    }
    let key = gex_cgen::image::image_key(image);
    let mut units = lock(&UNITS);
    units.push(Unit {
        key,
        wine: optional,
        path: path.to_string(),
        base,
        pages,
        regs: Vec::new(),
        dirs: Vec::new(),
        slots: 0,
    });
    lock(&WANTED).push_back(units.len() - 1);
}

/// The oldest unit waiting for its bytes, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_wanted() -> i32 {
    drain();
    lock(&WANTED).front().map_or(-1, |&u| u as i32)
}

fn put_str(s: &str, buf: u32, cap: u32) -> u32 {
    let n = s.len().min(cap as usize);
    let gm = &mut crate::guest_bytes()[buf as usize..buf as usize + n];
    gm.copy_from_slice(&s.as_bytes()[..n]);
    s.len() as u32
}

/// Unit `u`'s key (the directory its files are in) into `buf`; its length.
#[no_mangle]
pub extern "C" fn gex_cgen_key(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).map_or(0, |x| put_str(&x.key, buf, cap))
}

/// The path of the image unit `u` is for, for messages.
#[no_mangle]
pub extern "C" fn gex_cgen_path(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).map_or(0, |x| put_str(&x.path, buf, cap))
}

/// JS has no unit for `u`: the image runs on tier-1. Under `--cgen` a program
/// image (not a Wine DLL) without one is refused: returns 1, and JS stops.
#[no_mangle]
pub extern "C" fn gex_cgen_missing(u: u32) -> i32 {
    let mut wanted = lock(&WANTED);
    wanted.retain(|&w| w != u as usize);
    let units = lock(&UNITS);
    let Some(x) = units.get(u as usize) else { return 0 };
    if MODE.load(Ordering::Relaxed) == 2 && !x.wine {
        crate::warn_line(&format!(
            "gex-cgen: --cgen: {} has no function-tier unit for base {:#x} (key {}); build one natively first\n",
            x.path, x.base, x.key
        ));
        return 1;
    }
    0
}

/// Publish unit `u` with its `n` entries (u64s at `entries`): give every page
/// holding one a base, mark them translated, and number their slots, as the
/// native `publish_cgen_unit` does. 0, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_publish(u: u32, entries: u32, n: u32) -> i32 {
    let gm = GuestMem::new(crate::guest_bytes());
    let addrs: Vec<u64> = (0..n as u64).filter_map(|i| gm.read_u64(entries as u64 + 8 * i).ok()).collect();
    let (regs, dirs, slots) = crate::with_host(|h| {
        // By (table, slot): the browser's runtime slots span several dispatch
        // tables (they start above the statically recompiled images'), and
        // each has a body table of its own.
        let mut by_slot: std::collections::BTreeMap<(u32, u32), Vec<(u64, u32)>> = Default::default();
        let mut new_pages = Vec::new();
        for (k, &a) in addrs.iter().enumerate() {
            let page = a / gex_abi::DISPATCH_PAGE;
            let base = match h.jit.page_table.lookup(page) {
                Some(b) => b,
                None => {
                    let b = h.jit.page_table.base(page);
                    new_pages.push((page, b));
                    b
                }
            };
            let t = gex_abi::dispatch_entry_table(base) as u32;
            by_slot.entry((t, gex_abi::dispatch_index(a, base) as u32)).or_default().push((a, k as u32));
        }
        let mut regs = Vec::with_capacity(4 * by_slot.len());
        for ((t, slot), mut v) in by_slot {
            // A shared slot: the first gets the dispatch slot (its target
            // check hands the others to tier-1's trampoline); none gets a body.
            v.sort_unstable();
            regs.extend_from_slice(&[v[0].1, t, slot, (v.len() == 1) as u32]);
        }
        h.jit.translated.extend(addrs.iter().copied());
        (regs, new_pages, h.jit.page_table.slots())
    });
    {
        let mut units = lock(&UNITS);
        let Some(x) = units.get_mut(u as usize) else {
            crate::warn_line(&format!("gex-cgen: no unit {u} to publish\n"));
            return -1;
        };
        x.regs = regs;
        x.dirs = dirs;
        x.slots = slots;
        let mut pages = lock(&UNIT_PAGES);
        let map = pages.get_or_insert_with(Default::default);
        let mem = crate::guest_bytes();
        for &(page, hash) in &x.pages {
            map.insert(page, hash);
            let off = (gex_abi::PATCHED_PAGES + page / 8) as usize;
            if let Some(b) = mem.get_mut(off) {
                *b &= !(1 << (page % 8));
            }
        }
    }
    lock(&WANTED).retain(|&w| w != u as usize);
    lock(&PUBLISHED).push(u as usize);
    0
}

/// How many units are published: a Worker installs up to this many.
#[no_mangle]
pub extern "C" fn gex_cgen_count() -> u32 {
    lock(&PUBLISHED).len() as u32
}

/// The unit published `i`th (an index for the other calls).
#[no_mangle]
pub extern "C" fn gex_cgen_published(i: u32) -> i32 {
    lock(&PUBLISHED).get(i as usize).map_or(-1, |&u| u as i32)
}

/// Unit `u`'s registrations, `[k, table, slot, alone]` per slot, copied to `buf`
/// (`cap` u32s); returns how many u32s there are.
#[no_mangle]
pub extern "C" fn gex_cgen_regs(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    let Some(x) = units.get(u as usize) else { return 0 };
    let n = x.regs.len().min(cap as usize);
    let mut gm = GuestMem::new(crate::guest_bytes());
    for (i, v) in x.regs[..n].iter().enumerate() {
        let _ = gm.write_u32(buf as u64 + 4 * i as u64, *v);
    }
    x.regs.len() as u32
}

/// The dispatch-table slots unit `u` needs.
#[no_mangle]
pub extern "C" fn gex_cgen_slots(u: u32) -> u32 {
    lock(&UNITS).get(u as usize).map_or(0, |x| x.slots.min(u32::MAX as u64) as u32)
}

/// This Worker has installed the first `n` published units.
#[no_mangle]
pub extern "C" fn gex_cgen_have(n: u32) {
    HAVE.with(|c| c.set(n));
}

/// Whether this Worker has published units still to install: a miss at a
/// unit's entry is then a catch-up, not a translation.
pub(crate) fn behind() -> bool {
    on() && HAVE.with(|c| c.get()) < gex_cgen_count()
}

/// The publishing Worker has installed unit `u`: point the directory at the
/// pages it numbered (those still numbered so).
#[no_mangle]
pub extern "C" fn gex_cgen_installed(u: u32) {
    let dirs = lock(&UNITS).get(u as usize).map(|x| x.dirs.clone()).unwrap_or_default();
    crate::with_host(|h| {
        let mut gm = GuestMem::new(crate::guest_bytes());
        for (page, base) in dirs {
            if h.jit.page_table.lookup(page) == Some(base) {
                let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
            }
        }
    });
}

/// Worker `worker`'s body table for dispatch table `table` holds `size`
/// entries. Each table's limit (`gex_abi::BODY_LIMITS[table]`) is the least
/// of every Worker's, so the dispatcher's `call_known` goes straight to a
/// body only for a slot every Worker has.
#[no_mangle]
pub extern "C" fn gex_cgen_body_size(worker: u32, table: u32, size: u32) {
    let mut sizes = lock(&BODY_SIZES);
    if (table as usize) < gex_abi::DISPATCH_TABLES {
        sizes.entry(worker).or_insert([0; gex_abi::DISPATCH_TABLES])[table as usize] = size;
    }
    publish_limits(&sizes);
}

/// A Worker has gone: its body tables no longer limit anyone.
#[no_mangle]
pub extern "C" fn gex_cgen_body_gone(worker: u32) {
    let mut sizes = lock(&BODY_SIZES);
    sizes.remove(&worker);
    publish_limits(&sizes);
}

fn publish_limits(sizes: &std::collections::BTreeMap<u32, [u32; gex_abi::DISPATCH_TABLES]>) {
    let mut gm = GuestMem::new(crate::guest_bytes());
    for t in 0..gex_abi::DISPATCH_TABLES {
        let min = sizes.values().map(|v| v[t]).min().unwrap_or(0);
        let _ = gm.write_u32(gex_abi::BODY_LIMITS + 4 * t as u64, min);
    }
}

static DISPATCHER: OnceLock<Vec<u8>> = OnceLock::new();

/// The per-store dispatcher module (`gex_cgen::splice::dispatcher_module`):
/// where its bytes are.
#[no_mangle]
pub extern "C" fn gex_cgen_dispatcher_ptr() -> u32 {
    DISPATCHER.get_or_init(gex_cgen::splice::dispatcher_module).as_ptr() as u32
}

/// And how long they are.
#[no_mangle]
pub extern "C" fn gex_cgen_dispatcher_len() -> u32 {
    DISPATCHER.get_or_init(gex_cgen::splice::dispatcher_module).len() as u32
}

static LEAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The body table's default entry: an address no installed unit holds a body
/// for. -1, leave -- as the native `cgen_leave`.
#[no_mangle]
pub extern "C" fn gex_cgen_leave(_ctx: i32, _depth: i32, _need: i32, _ret: i64) -> i32 {
    LEAVES.fetch_add(1, Ordering::Relaxed);
    -1
}

/// Leaves so far, for the stats line.
pub(crate) fn leaves() -> u64 {
    LEAVES.load(Ordering::Relaxed)
}

/// Pages that retired: any an installed unit covers whose bytes are no longer
/// the ones it was built from gets its bit in `gex_abi::PATCHED_PAGES`, and
/// compiled code stops calling into it (the native `on_retire`).
pub(crate) fn on_retire(ranges: &[(u64, u64)]) {
    let guard = lock(&UNIT_PAGES);
    let Some(pages) = guard.as_ref() else { return };
    let memory = crate::guest_bytes();
    for &(start, end) in ranges {
        for page in start / gex_abi::DISPATCH_PAGE..end.div_ceil(gex_abi::DISPATCH_PAGE) {
            let Some(&built) = pages.get(&page) else { continue };
            if page_hash(memory, page) != built {
                let off = (gex_abi::PATCHED_PAGES + page / 8) as usize;
                if let Some(b) = memory.get_mut(off) {
                    *b |= 1 << (page % 8);
                }
            }
        }
    }
}
