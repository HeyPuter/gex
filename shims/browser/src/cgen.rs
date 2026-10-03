//! The function tier's whole-image units (format `gex-cgen-image 3`) and the
//! function table in the browser: the shim's half (docs/browser-units-plan.md,
//! docs/merge-plan.md §9.8.1).
//!
//! What stays here is small: which images were mapped, which unit each one
//! wants (its key, `gex_cgen::image::image_key`, the same function the native
//! host uses), that the mapped bytes are the ones the unit was built from,
//! and the publish -- a range of the function table, the address map, the
//! bridges in tier-1's dispatch slots, the directory -- as the native
//! `ftab::publish` does it. A unit's wasm never enters the shim's heap
//! (noita.exe's is ~195 MiB): JavaScript fetches and compiles it, hands over
//! only its slot list (`c.entries`, the `gex.funcs` section's), and each
//! Worker instantiates it into its own function table with `__table_base` at
//! the unit's range as it replays the table's log. Units are kept in a list
//! of their own, not in the module registry: the registry may be rebuilt
//! (`backfill_registry`), and a unit has no bytes to keep in it.
//!
//! The protocol with `gex.mjs`:
//!   - `gex_cgen_wanted()` names a placed unit waiting for its bytes;
//!     `gex_cgen_key`/`gex_cgen_path` say which;
//!   - JS loads `<key>/c.wasm` and `<key>/c.entries` and calls
//!     `gex_cgen_publish(u, entries, n)`, or `gex_cgen_missing(u)`;
//!   - every Worker replays the table's log (`gex_ftab_log_len`,
//!     `gex_ftab_event`): an install is its unit instantiated with
//!     `__table_base` = `gex_ftab_install_base` and its bridges written
//!     (`gex_cgen_regs`); a retirement points the index back at the
//!     runtime module's `resolve`. It reports with `gex_cgen_have(n)`; the
//!     publishing Worker then calls `gex_cgen_installed(u)`, which writes
//!     the directory.
//!   - `gex.lookup` is `gex_ftab_lookup` (JS catches up first when
//!     `gex_ftab_behind`), the table's index 1 `gex_ftab_fallback`.

use gex_kernel::ftab::{Event, FuncMap, Tier};
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
    /// Once published: the tier-1 dispatch slots (table, slot) its entries
    /// are reached at, which hold the bridge, flattened for JS.
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
/// The pages installed units cover, hashed as built (`on_retire`).
static UNIT_PAGES: Mutex<Option<std::collections::HashMap<u64, u64>>> = Mutex::new(None);
/// The process's function table, host side (`gex_kernel::ftab`): one for
/// every Worker, each of which keeps its own `WebAssembly.Table` in step.
static FTAB: Mutex<Option<FuncMap>> = Mutex::new(None);

fn ftab<R>(f: impl FnOnce(&mut FuncMap) -> R) -> R {
    f(lock(&FTAB).get_or_insert_with(FuncMap::new))
}

thread_local! {
    /// How far through the function table's log this Worker is.
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

/// Publish unit `u` with its `n` slots (u64s at `entries`: the guest address
/// of each slot its element segment fills, 0 for none): a range of the
/// function table, each entry mapped to its slot, the probe cache cleared for
/// them, and a bridge in tier-1's dispatch slot for each, numbering their
/// pages -- the native `ftab::publish`. 0, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_publish(u: u32, entries: u32, n: u32) -> i32 {
    let gm = GuestMem::new(crate::guest_bytes());
    let slots: Vec<u64> = (0..n as u64).filter_map(|i| gm.read_u64(entries as u64 + 8 * i).ok()).collect();
    let addrs: Vec<u64> = slots.iter().copied().filter(|&a| a != 0).collect();
    let pairs: Vec<(u64, u32)> = slots.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    ftab(|m| m.publish(slots.len() as u32, u, Tier::Unit, &pairs));
    {
        let mem = crate::guest_bytes();
        for &a in &addrs {
            gex_kernel::ftab::fcache_clear(mem, a);
        }
    }
    let (regs, dirs, nslots) = crate::with_host(|h| {
        let mut seen: std::collections::BTreeSet<(u32, u32)> = Default::default();
        let mut new_pages = Vec::new();
        for &a in &addrs {
            let page = a / gex_abi::DISPATCH_PAGE;
            if page >= gex_abi::DISPATCH_MAX_PAGES {
                continue;
            }
            let base = match h.jit.page_table.lookup(page) {
                Some(b) => b,
                None => {
                    let b = h.jit.page_table.base(page);
                    new_pages.push((page, b));
                    b
                }
            };
            let t = gex_abi::dispatch_entry_table(base) as u32;
            seen.insert((t, gex_abi::dispatch_index(a, base) as u32));
        }
        let mut regs = Vec::with_capacity(2 * seen.len());
        // A dense table's slot gets an index in `blocks` of its own, which
        // JS writes the bridge at (as table `gex_emit::dense::TABLE_INDEX`).
        let dense: Vec<(u32, u32)> = seen.iter().copied().filter(|&(t, _)| crate::dense::map(t as usize).is_some()).collect();
        let first = if dense.is_empty() { 0 } else { crate::dense::alloc(dense.len() as u32) };
        for (j, &(t, slot)) in dense.iter().enumerate() {
            crate::dense::set_slot(gex_abi::dispatch_entry(t as usize, slot), first + j as u32);
            regs.extend_from_slice(&[gex_emit::dense::TABLE_INDEX, first + j as u32]);
        }
        for (t, slot) in seen {
            if crate::dense::map(t as usize).is_none() {
                regs.extend_from_slice(&[t, slot]);
            }
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
        x.slots = nslots;
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

/// Unit `u`'s bridges, `[table, slot]` per dispatch slot, copied to `buf`
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

/// This Worker has replayed the first `n` events of the function table's log.
#[no_mangle]
pub extern "C" fn gex_cgen_have(n: u32) {
    HAVE.with(|c| c.set(n));
}

/// Whether this Worker has the function table's log still to replay: a miss
/// at a unit's entry is then a catch-up, not a translation.
pub(crate) fn behind() -> bool {
    on() && (HAVE.with(|c| c.get()) as usize) < ftab(|m| m.log_len())
}

/// [`behind`], for `gex.lookup`'s JS half.
#[no_mangle]
pub extern "C" fn gex_ftab_behind() -> i32 {
    behind() as i32
}

/// How many events the function table's log has.
#[no_mangle]
pub extern "C" fn gex_ftab_log_len() -> u32 {
    ftab(|m| m.log_len() as u32)
}

/// Event `i` of the log: an install as `1 << 32 | install`, a retirement as
/// `2 << 32 | index`; 0 past the end.
#[no_mangle]
pub extern "C" fn gex_ftab_event(i: u32) -> i64 {
    ftab(|m| match m.log_from(i as usize).first() {
        Some(Event::Install(n)) => (1i64 << 32) | *n as i64,
        Some(Event::Retire(idx)) => (2i64 << 32) | *idx as i64,
        None => 0,
    })
}

/// Install `n`'s table range starts here.
#[no_mangle]
pub extern "C" fn gex_ftab_install_base(n: u32) -> u32 {
    ftab(|m| m.installs().get(n as usize).map_or(0, |i| i.base))
}

/// And holds this unit (the index the other `gex_cgen_*` calls take).
#[no_mangle]
pub extern "C" fn gex_ftab_install_unit(n: u32) -> u32 {
    ftab(|m| m.installs().get(n as usize).map_or(u32::MAX, |i| i.module))
}

/// How large every Worker's table must be.
#[no_mangle]
pub extern "C" fn gex_ftab_size() -> u32 {
    ftab(|m| m.size())
}

/// `gex.lookup(ctx, addr)`: the index of the function for `addr` -- a
/// unit's, if its entry page was not rewritten since -- else the fallback,
/// written into the probe cache either way. JS has caught this Worker up.
#[no_mangle]
pub extern "C" fn gex_ftab_lookup(_ctx: i32, addr: i64) -> i32 {
    let addr = addr as u64;
    let mem = crate::guest_bytes();
    let hit = ftab(|m| {
        let f = m.get(addr)?;
        let page = addr / gex_abi::DISPATCH_PAGE;
        let patched = mem.get((gex_abi::PATCHED_PAGES + page / 8) as usize).is_some_and(|b| b & (1 << (page % 8)) != 0);
        if patched {
            m.retire(addr);
            return None;
        }
        Some(f.idx)
    });
    let idx = hit.unwrap_or(gex_abi::ftab::FALLBACK);
    gex_kernel::ftab::fcache_write(mem, addr, idx);
    idx as i32
}

/// The table's index 1: code no unit holds. The browser has no call-out
/// yet (a nested guest run inside a wasm call), so this leaves for it:
/// `ctx::PC` = the target, 0; the frame that owns the code it returns into
/// catches it, else tier-1 runs it.
#[no_mangle]
pub extern "C" fn gex_ftab_fallback(ctx: i32, _depth: i32, _need: i32, _ret: i64) -> i32 {
    LEAVES.fetch_add(1, Ordering::Relaxed);
    let mut gm = GuestMem::new(crate::guest_bytes());
    let c = ctx as u32 as u64;
    let t = gm.read_u64(c + gex_abi::ctx::LAST_TARGET as u64).unwrap_or(0);
    let _ = gm.write_u64(c + gex_abi::ctx::PC as u64, t);
    gex_abi::ftab::RET_LEFT
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

static LEAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The function table's runtime module (`gex_cgen::runtime`): where its
/// bytes are, and how long they are.
static RUNTIME: OnceLock<Vec<u8>> = OnceLock::new();

fn runtime() -> &'static Vec<u8> {
    RUNTIME.get_or_init(|| {
        let m = gex_cgen::runtime::module();
        if !crate::dense::on() {
            return m;
        }
        dense_runtime(&m).unwrap_or_else(|| {
            crate::warn_line("gex-cgen: the function table's runtime module is not the shape expected; its bridge dispatches classically\n");
            m
        })
    })
}

#[no_mangle]
pub extern "C" fn gex_ftab_runtime_ptr() -> u32 {
    runtime().as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn gex_ftab_runtime_len() -> u32 {
    runtime().len() as u32
}

fn leb(b: &[u8], p: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        v |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
}

fn put_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v != 0 { b | 0x80 } else { b });
        if v == 0 {
            return;
        }
    }
}

/// The runtime module (`gex_cgen::runtime`) with dense dispatch (the
/// bridge carries on in tier-1 through its last function, `dispatch`): an
/// import of `gex.blocks` after its tables, and that function replaced by
/// `gex_emit::dense::dispatch_body`. `None` if the module is not the shape
/// that was written for (memory, `ftab` and the dispatch tables imported, four
/// functions defined, `dispatch` last, of type 3).
fn dense_runtime(m: &[u8]) -> Option<Vec<u8>> {
    if m.get(..8)? != b"\0asm\x01\0\0\0" {
        return None;
    }
    let mut out = m[..8].to_vec();
    let mut p = 8;
    let mut tables = 0u32;
    while p < m.len() {
        let id = m[p];
        p += 1;
        let len = leb(m, &mut p)? as usize;
        let body = m.get(p..p + len)?;
        p += len;
        let mut sec = Vec::new();
        match id {
            2 => {
                let mut q = 0;
                let n = leb(body, &mut q)?;
                let start = q;
                for _ in 0..n {
                    for _ in 0..2 {
                        let l = leb(body, &mut q)? as usize;
                        q += l;
                    }
                    match *body.get(q)? {
                        0 => {
                            q += 1;
                            leb(body, &mut q)?;
                        }
                        1 => {
                            tables += 1;
                            q += 2; // the kind, funcref
                            let flag = *body.get(q)?;
                            q += 1;
                            leb(body, &mut q)?;
                            if flag & 1 != 0 {
                                leb(body, &mut q)?;
                            }
                        }
                        2 => {
                            q += 1;
                            let flag = *body.get(q)?;
                            q += 1;
                            leb(body, &mut q)?;
                            if flag & 1 != 0 {
                                leb(body, &mut q)?;
                            }
                        }
                        _ => return None,
                    }
                }
                if tables != 1 + gex_abi::DISPATCH_TABLES as u32 {
                    return None;
                }
                put_leb(&mut sec, n + 1);
                sec.extend_from_slice(&body[start..]);
                for name in [gex_abi::IMPORT_MODULE, gex_emit::dense::TABLE_NAME] {
                    put_leb(&mut sec, name.len() as u32);
                    sec.extend_from_slice(name.as_bytes());
                }
                sec.extend_from_slice(&[0x01, 0x70, 0x00, 0x00]);
            }
            10 => {
                let mut q = 0;
                let n = leb(body, &mut q)?;
                if n != 4 {
                    return None;
                }
                put_leb(&mut sec, n);
                for k in 0..n {
                    let at = q;
                    let l = leb(body, &mut q)? as usize;
                    q += l;
                    if k + 1 < n {
                        sec.extend_from_slice(body.get(at..q)?);
                    } else {
                        sec.extend_from_slice(&gex_emit::dense::dispatch_body(3, 1, tables));
                    }
                }
            }
            _ => sec.extend_from_slice(body),
        }
        out.push(id);
        put_leb(&mut out, sec.len() as u32);
        out.extend_from_slice(&sec);
    }
    Some(out)
}

/// Calls the fallback answered (it leaves), for the stats line.
pub(crate) fn leaves() -> u64 {
    LEAVES.load(Ordering::Relaxed)
}

/// Pages that retired: any an installed unit covers whose bytes are no longer
/// the ones it was built from gets its bit in `gex_abi::PATCHED_PAGES`, and
/// compiled code stops calling into it (the native `on_retire`).
pub(crate) fn on_retire(ranges: &[(u64, u64)]) {
    on_retire_pages(ranges);
    // After the bits: a lookup once the probe cache is cleared sees them.
    let memory = crate::guest_bytes();
    ftab(|m| {
        for &(start, end) in ranges {
            for (a, _) in m.range(start, end).collect::<Vec<_>>() {
                gex_kernel::ftab::fcache_clear(memory, a);
            }
        }
    });
}

fn on_retire_pages(ranges: &[(u64, u64)]) {
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
