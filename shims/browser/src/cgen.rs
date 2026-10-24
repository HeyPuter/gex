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
//! (a large game's is ~195 MiB): JavaScript fetches and compiles it, hands over
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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) mod elfprov;
pub(crate) mod libprov;
mod wasmdll;
pub(crate) use wasmdll::skeleton_code;
pub(crate) mod wasmprog;
mod dylink;

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
/// Whether `PENDING` may hold something (set after a push, cleared before a
/// drain takes it).
static ANY_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// ---- Where each unit is: wanted, claimed, published, installed ----
//
// One byte of flags per unit (an index into `UNITS`), changed with atomic
// ors and ands: what `WANTED` (a queue), `CLAIMED` and `INSTALLED` (sets),
// each under a mutex, were. Every system call's serve asks for the oldest
// wanted unit; with none, that is two loads.

/// Placed for an image and waiting for its bytes from JS.
const U_WANTED: u8 = 1;
/// A Worker has started publishing it: the others only install.
const U_CLAIMED: u8 = 2;
/// Published (also in `PUBLISHED`, in order).
const U_PUBLISHED: u8 = 4;
/// Published and installed by some Worker: its directory entries are written.
const U_INSTALLED: u8 = 8;
/// Units with flags; one past it has none.
const U_MAX: usize = 1 << 16;
static U_STATE: [std::sync::atomic::AtomicU8; U_MAX] = [const { std::sync::atomic::AtomicU8::new(0) }; U_MAX];
/// One past the highest unit ever wanted (the scan's end).
static U_HIGH: AtomicUsize = AtomicUsize::new(0);
/// No unit below this is wanted (the scan's start). A unit is wanted once,
/// when placed, and never again after it stops being: the cursor only rises.
static U_LOW: AtomicUsize = AtomicUsize::new(0);

fn u_state(u: usize) -> u8 {
    U_STATE.get(u).map_or(0, |s| s.load(Ordering::Acquire))
}

/// Unit `u`, just placed, waits for its bytes.
fn u_want(u: usize) {
    let Some(s) = U_STATE.get(u) else {
        crate::warn_line(&format!("gex-cgen: unit {u}: past {U_MAX} units; tier-1\n"));
        return;
    };
    s.fetch_or(U_WANTED, Ordering::AcqRel);
    U_HIGH.fetch_max(u + 1, Ordering::AcqRel);
}

/// Unit `u` waits no longer (published, held, or missing).
fn u_unwant(u: usize) {
    if let Some(s) = U_STATE.get(u) {
        s.fetch_and(!U_WANTED, Ordering::AcqRel);
    }
}

/// The oldest wanted unit `skip` lets through, moving the cursor past the
/// ones no longer wanted.
fn u_oldest(skip: impl Fn(u8) -> bool) -> Option<usize> {
    let high = U_HIGH.load(Ordering::Acquire);
    let mut low = U_LOW.load(Ordering::Acquire);
    let mut found = None;
    let mut u = low;
    while u < high {
        let st = u_state(u);
        if st & U_WANTED != 0 {
            if !skip(st) {
                found = Some(u);
                break;
            }
        } else if u == low {
            // Not wanted, and nothing wanted below it: the cursor moves on.
            match U_LOW.compare_exchange(low, u + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => low = u + 1,
                Err(now) => low = now.max(low),
            }
        }
        u += 1;
    }
    found
}

/// Whether any unit is wanted.
fn wanted_any() -> bool {
    u_oldest(|_| false).is_some()
}

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
    /// The image's export providers (`gex_cgen::provider`, §9.9): each one's
    /// module file under the unit cache and the (C function, guest address)
    /// pairs it binds. Bound after the unit, whether or not it has one.
    providers: Vec<Provided>,
    /// The image's identity (`gex_key::image_identity`, without `image:`):
    /// its indirect-call profile is `<cache>/icall/<identity>.txt`, which JS
    /// reads and hands over (`gex_cgen_set_icall`) before the key is used.
    identity: String,
    /// The key before the profile's part (`key` adds it).
    key_base: String,
}

/// One export provider for a placed image.
struct Provided {
    /// `providers/<name>-<fingerprint>.wasm`, as `provider::build` names it.
    file: String,
    binds: Vec<(String, u64)>,
    /// A stateful provider (docs/eject-pe.md §5.1): every export of the
    /// image by name and address, and its entry point (DllMain), which
    /// `gexp__entry` replaces.
    stateful: Option<(Vec<(String, u64)>, u64)>,
    /// A built-in provider with data and a C stack (`stateful` on its
    /// PROVIDES line: crt_heap): bound like any built-in one, its data
    /// allocated as an ejected project's is, top down.
    data: bool,
    /// A native wasm32 Wine DLL's skeleton (`wasmdll`): Wine's PE module is
    /// its provider, installed at the first skeleton and bound at each.
    skeleton: Option<Arc<gex_ld::pe::PeImage>>,
    /// Falls back to lifted twins (`lifted`): bound only where its unit
    /// installed them.
    lifted: bool,
    /// Its groups' guest addresses (`GEX_GROUP`, `provider::group_addrs`):
    /// each binds whole or not at all.
    groups: Vec<Vec<u64>>,
}

/// Stateful providers by library (lower case) and module file under the
/// unit cache: `GEX_NT_STATEFUL=<dll>=<file>[,...]`, an ejected project's
/// (the native `libprov`).
static STATEFUL: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub(crate) fn set_stateful(spec: &str) {
    let mut v = lock(&STATEFUL);
    for item in spec.split(',') {
        if let Some((d, f)) = item.split_once('=') {
            v.push((d.trim().to_ascii_lowercase(), f.trim().to_string()));
        }
    }
}

/// `dll=file,...` (the page's `stateful`, run-node's `GEX_STATEFUL`): a
/// wine32 or wine64 process's stateful providers, before the program
/// starts.
#[no_mangle]
pub extern "C" fn gex_cgen_stateful(ptr: u32, len: u32) {
    set_stateful(&String::from_utf8_lossy(crate::shim_slice(ptr, len)));
}

/// Exports a stateful provider does not implement, by address: for the
/// message when the program calls one.
static MISSING: Mutex<Option<std::collections::HashMap<u64, String>>> = Mutex::new(None);

/// Provider modules published, by number (the module id an install names
/// is `PROVIDER | number`): the file JS loads, the bridges every Worker
/// writes and the pages the publisher numbered (as a unit's).
struct ProviderModule {
    file: String,
    regs: Vec<u32>,
    dirs: Vec<(u64, u32)>,
    slots: u64,
    /// A stateful provider's data (`__memory_base`), 0 for a stateless one.
    /// Above 4 GiB only for Wine's PE module in a 64-bit memory (its data
    /// window is the top of the 16 GiB: `crate::w64_pe_data`).
    memory_base: u64,
}

static PROVIDERS: Mutex<Vec<ProviderModule>> = Mutex::new(Vec::new());

/// Module ids at and above this are providers' (`PROVIDERS`), below
/// it units (`UNITS`).
const PROVIDER: u32 = 1 << 31;

/// An ejected build's image keys and identities, by (base, path)
/// (GEX_NT_IDENTITIES, written by `gexc eject`: `base:key:identity:path`
/// joined by `;`). Taken instead of hashing every byte of every image at
/// start-up (~1 s of the page's main thread); the mapped code is still
/// compared with the image byte for byte below, and an image not listed is
/// hashed as before.
static KNOWN: Mutex<Option<std::collections::HashMap<(u64, String), (String, String)>>> = Mutex::new(None);

pub(crate) fn set_identities(spec: &str) {
    let mut m = std::collections::HashMap::new();
    for item in spec.split(';') {
        let mut f = item.splitn(4, ':');
        let (Some(base), Some(key), Some(id), Some(path)) = (f.next(), f.next(), f.next(), f.next()) else { continue };
        let Ok(base) = u64::from_str_radix(base, 16) else { continue };
        m.insert((base, path.to_string()), (key.to_string(), id.to_string()));
    }
    *lock(&KNOWN) = Some(m);
}

/// The page's identities (`base:key:identity:path;...`, a W3 site's
/// `identities`, written by its build from the units it made), before the
/// program starts: as GEX_NT_IDENTITIES for an NT page.
#[no_mangle]
pub extern "C" fn gex_cgen_identities(ptr: u32, len: u32) {
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    set_identities(spec.trim());
}

fn known(base: u64, path: &str) -> Option<(String, String)> {
    lock(&KNOWN).as_ref()?.get(&(base, path.to_string())).cloned()
}

static UNITS: Mutex<Vec<Unit>> = Mutex::new(Vec::new());
/// Published units, in publish order: what every Worker installs.
static PUBLISHED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
// Claims ([`U_CLAIMED`]): `gex_cgen_wanted` hands the oldest wanted unit to
// every Worker that asks until it is published, so two threads of a process
// can load and publish the same unit; the second publish found every page
// numbered already and replaced the unit's `dirs` with nothing, and whichever
// install read them after that wrote no directory entry at all (a startup
// livelock: one DLL placed twice). The first claim publishes; the others
// only install.
/// Publishes refused because another Worker had claimed the unit.
pub(crate) static TWICE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether `page` is a unit's whose publish has begun ([`U_CLAIMED`]) and
/// whose install has not written its directory yet: a thread running the
/// unit's code meanwhile finds the page numbered and its entry not yet written.
pub(crate) fn in_install_window(page: u64) -> bool {
    let units = lock(&UNITS);
    units.iter().enumerate().any(|(u, x)| {
        let st = u_state(u);
        st & U_CLAIMED != 0 && st & U_INSTALLED == 0 && x.pages.iter().any(|&(p, _)| p == page)
    })
}
/// The pages installed units cover, hashed as built (`on_retire`).
static UNIT_PAGES: Mutex<Option<std::collections::HashMap<u64, u64>>> = Mutex::new(None);
/// The process's function table, host side (`gex_kernel::ftab`): one for
/// every Worker, each of which keeps its own `WebAssembly.Table` in step.
static FTAB: Mutex<Option<FuncMap>> = Mutex::new(None);

fn ftab<R>(f: impl FnOnce(&mut FuncMap) -> R) -> R {
    let mut g = lock(&FTAB);
    let m = g.get_or_insert_with(FuncMap::new);
    let r = f(m);
    FTAB_SIZE.store(m.size(), SeqCst);
    log_publish(m);
    r
}

// ---- The table's log, for every Worker to replay, read without the lock ----
//
// `FuncMap`'s log is append-only. Each change made under `FTAB` copies its
// new events here, into segments that never move (segment k holds 1024 << k
// events), and then publishes the length: a Worker asking how far the log
// goes, every system call, and reading the events it lacks takes no lock.

const LOG_SEG0: usize = 1024;
static LOG_SEGS: [std::sync::atomic::AtomicPtr<AtomicU64>; 32] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 32];
/// How many events `LOG_SEGS` holds, published after they are written.
static LOG_LEN: AtomicUsize = AtomicUsize::new(0);

/// Event `i`'s segment and the index in it.
fn log_at(i: usize) -> (usize, usize) {
    let k = (usize::BITS - 1 - (i / LOG_SEG0 + 1).leading_zeros()) as usize;
    (k, i - LOG_SEG0 * ((1 << k) - 1))
}

/// The events `m` has past the published length, copied and published.
/// Under `FTAB` (writers are serialized by it).
fn log_publish(m: &FuncMap) {
    let have = LOG_LEN.load(Ordering::Relaxed);
    let new = m.log_from(have);
    if new.is_empty() {
        return;
    }
    for (j, e) in new.iter().enumerate() {
        let (k, at) = log_at(have + j);
        let mut seg = LOG_SEGS[k].load(Ordering::Acquire);
        if seg.is_null() {
            let b: Box<[AtomicU64]> = (0..LOG_SEG0 << k).map(|_| AtomicU64::new(0)).collect();
            seg = Box::into_raw(b) as *mut AtomicU64;
            LOG_SEGS[k].store(seg, Ordering::Release);
        }
        let v = match e {
            Event::Install(n) => (1u64 << 32) | *n as u64,
            Event::Retire(idx) => (2u64 << 32) | *idx as u64,
        };
        // SAFETY: segment k holds `LOG_SEG0 << k` events and `at` is below
        // that (`log_at`); it is never freed.
        unsafe { (*seg.add(at)).store(v, Ordering::Relaxed) };
    }
    LOG_LEN.store(have + new.len(), Ordering::Release);
}

/// How many events the log has (published).
fn log_len() -> usize {
    LOG_LEN.load(Ordering::Acquire)
}

/// Event `i`, as `gex_ftab_event` gives it; 0 past the end.
fn log_event(i: usize) -> u64 {
    if i >= log_len() {
        return 0;
    }
    let (k, at) = log_at(i);
    let seg = LOG_SEGS[k].load(Ordering::Acquire);
    // SAFETY: as in `log_publish`; published before the length was.
    unsafe { (*seg.add(at)).load(Ordering::Relaxed) }
}

/// [`FuncMap::size`] as of the last change, read without the lock: what a
/// Worker's `WebAssembly.Table` must reach before its code reads an index
/// out of the probe cache.
static FTAB_SIZE: AtomicU32 = AtomicU32::new(0);

// ---- Which indices the probe cache may hold (X's B16) ----
//
// The probe cache (`gex_abi::FCACHE`) is process-wide, and compiled code
// (units, providers, winepe.wasm) calls an index it finds there straight
// through its own Worker's `WebAssembly.Table`, with no `lookup` to catch
// that Worker up first. Each Worker grows its table only when it catches up
// (`#cgenGrow`, with room over), so a thread that last caught up before
// another thread installed a large unit read the new index as a hit and
// called past its table's end ("table index is out of bounds":
// a DllMain through winepe,
// tests/w3/ftabstale.sh). So every Worker that may be running guest
// code says how long its table is, and an index goes into the cache only
// when every one of their tables holds it; otherwise the cache slot stays
// resolve and the call comes through `lookup`, which catches the caller up.
// A Worker waiting out a blocking call (`schedule` gave the stack back) is
// not counted: on its way back to guest code it is marked again, then grown
// if short (`ftab_resume`), in that order, so a publisher either counts it
// or it sees the publisher's index in `FTAB_SIZE`.

/// The registry's slots: [`SLOT_CLAIMED`] | [`SLOT_IN`] | the table length;
/// 0 is free. One per Worker with a function table, freed when its thread
/// is over (a Worker that runs another thread claims one again).
const FTAB_SLOTS: usize = 1024;
static FTAB_LENS: [AtomicU64; FTAB_SLOTS] = [const { AtomicU64::new(0) }; FTAB_SLOTS];
/// One past the highest slot ever claimed (the scan's end).
static FTAB_HIGH: AtomicUsize = AtomicUsize::new(0);
/// A Worker found no free slot: nothing is cached from then on.
static FTAB_FULL: AtomicBool = AtomicBool::new(false);
const SLOT_CLAIMED: u64 = 1 << 40;
const SLOT_IN: u64 = 1 << 32;
const NO_SLOT: usize = usize::MAX;

/// The shortest table among the Workers that may be running guest code:
/// an index below it is safe in the probe cache.
fn ftab_safe_len() -> u32 {
    if FTAB_FULL.load(SeqCst) {
        return 0;
    }
    let high = FTAB_HIGH.load(SeqCst).min(FTAB_SLOTS);
    FTAB_LENS[..high].iter().fold(u32::MAX, |m, s| {
        let v = s.load(SeqCst);
        if v & SLOT_CLAIMED != 0 && v & SLOT_IN != 0 { m.min(v as u32) } else { m }
    })
}

/// Write `addr → idx` into the probe cache if every running Worker's table
/// holds `idx`; otherwise leave the slot to resolve (clearing it if it named
/// `addr`).
fn fcache_publish(mem: &mut [u8], addr: u64, idx: u32) {
    if idx < ftab_safe_len() {
        gex_kernel::ftab::fcache_write(mem, addr, idx);
    } else {
        gex_kernel::ftab::fcache_clear(mem, addr);
    }
}

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Grow this Worker's function table past the process's size, with its
    /// room over (`#cgenGrowIfShort`), which reports it (`gex_ftab_enter`).
    fn ftab_grow();
}

/// JS: this Worker's function table is `len` long and its guest code may
/// run. Returns the process's table size: if it is past `len`, JS grows
/// again (another Worker allocated meanwhile).
#[no_mangle]
pub extern "C" fn gex_ftab_enter(len: u32) -> u32 {
    HAS_FTAB.with(|c| c.set(true));
    let mut slot = MY_FTAB_SLOT.with(|c| c.get());
    if slot == NO_SLOT {
        slot = FTAB_LENS.iter().position(|s| s.compare_exchange(0, SLOT_CLAIMED | SLOT_IN | len as u64, SeqCst, SeqCst).is_ok()).unwrap_or(NO_SLOT);
        if slot == NO_SLOT {
            FTAB_FULL.store(true, SeqCst);
        } else {
            FTAB_HIGH.fetch_max(slot + 1, SeqCst);
            MY_FTAB_SLOT.with(|c| c.set(slot));
        }
    }
    if slot != NO_SLOT {
        FTAB_LENS[slot].store(SLOT_CLAIMED | SLOT_IN | len as u64, SeqCst);
    }
    FTAB_SIZE.load(SeqCst)
}

/// `schedule` gave the stack back: this Worker runs no guest code until it
/// resumes (`ftab_resume`), and none at all if its thread is over.
pub(crate) fn ftab_out(thread_over: bool) {
    let slot = MY_FTAB_SLOT.with(|c| c.get());
    if slot == NO_SLOT {
        return;
    }
    if thread_over {
        FTAB_LENS[slot].store(0, SeqCst);
        MY_FTAB_SLOT.with(|c| c.set(NO_SLOT));
    } else {
        FTAB_LENS[slot].fetch_and(!SLOT_IN, SeqCst);
    }
}

/// Before this Worker's guest code runs again (`schedule` resumed it, or a
/// system call returns): counted as running, then grown if another Worker
/// allocated past its table meanwhile. On the way back from a system call
/// this also covers a thread that keeps running: each grow leaves room
/// over (`FTAB_SPARE_THREADED`).
pub(crate) fn ftab_resume() {
    if !HAS_FTAB.with(|c| c.get()) {
        // No table in this Worker (no unit, provider or native module).
        return;
    }
    let slot = MY_FTAB_SLOT.with(|c| c.get());
    let short = if slot == NO_SLOT {
        true
    } else {
        let v = FTAB_LENS[slot].fetch_or(SLOT_IN, SeqCst);
        FTAB_SIZE.load(SeqCst) > v as u32
    };
    if short {
        // SAFETY: a host import without arguments; no lock is held.
        unsafe { ftab_grow() };
    }
}

/// A fork's child or an `execve`'s new program (with the probe cache's
/// reset): the copied registry names the old process's Workers.
pub(crate) fn ftab_registry_reset() {
    for s in &FTAB_LENS {
        s.store(0, SeqCst);
    }
    FTAB_HIGH.store(0, SeqCst);
    FTAB_FULL.store(false, SeqCst);
    MY_FTAB_SLOT.with(|c| c.set(NO_SLOT));
}

thread_local! {
    /// How far through the function table's log this Worker is.
    static HAVE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// This Worker's slot in `FTAB_LENS`, or `NO_SLOT`.
    static MY_FTAB_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(NO_SLOT) };
    /// This Worker has a function table (`gex_ftab_enter` was called).
    static HAS_FTAB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether units are on.
pub(crate) fn on() -> bool {
    MODE.load(Ordering::Relaxed) != 0
}

/// ELF export providers without units (`gex_cgen_set_mode` bit 3): the
/// provider modules come from the page's dist (docs/gexcc.md §7.9), and no
/// image looks for a unit.
static PROVIDERS_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn providers_on() -> bool {
    on() || PROVIDERS_ONLY.load(Ordering::Relaxed)
}

/// Turn the function tier on (1) or require it for program images (2). Before
/// the process boots, so the exe and ntdll are seen. Bit 3 (8): ELF export
/// providers whether or not units are on. Bit 2 (4) selects the
/// unguarded flavor of i386 units (the native one, `:unguarded` keys): in
/// the browser the guarded flavor guards only at the memory's end, as the
/// unguarded one traps there by itself (docs/browser-units-plan.md).
/// The TSO flavour (`?tso=1`, for weakly ordered hosts): guest loads and stores are
/// atomic where aligned, which tier 1 and the function JIT emit and a
/// function-tier unit, compiled ahead with plain accesses, does not. So units
/// are off in it, whichever of the two switches comes first; export
/// providers (C, not translated guest code) stay.
pub(crate) static TSO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn the TSO flavour on: units off from here.
pub(crate) fn set_tso() {
    TSO.store(true, Ordering::Relaxed);
    if MODE.swap(0, Ordering::Relaxed) != 0 {
        tso_refuses_units();
    }
}

#[cold]
fn tso_refuses_units() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        crate::warn_line("[gex-note] tso: function-tier units are compiled with plain accesses; translating instead\n");
    }
}

#[no_mangle]
pub extern "C" fn gex_cgen_set_mode(mode: u32) {
    if mode & 4 != 0 {
        gex_cgen::image::set_unguarded(true);
    }
    if mode & 8 != 0 {
        PROVIDERS_ONLY.store(true, Ordering::Relaxed);
    }
    // Bit 4 (16): the counting flavor of units (`image::icall_count`), whose
    // table calls the shim tallies (`gex_icall_note`).
    if mode & 16 != 0 {
        gex_cgen::image::set_icall_count(true);
    }
    let mut mode = mode & 3;
    if mode != 0 && TSO.load(Ordering::Relaxed) {
        tso_refuses_units();
        mode = 0;
    }
    MODE.store(mode.min(2), Ordering::Relaxed);
    if mode == 0 {
        return;
    }
    static HOOK: OnceLock<()> = OnceLock::new();
    if HOOK.set(()).is_ok() {
        gex_kernel::nt::section::set_image_mapped_hook(|m| {
            lock(&PENDING).push(Pending { path: m.path.to_string(), base: m.base, pe: Arc::clone(m.pe) });
            ANY_PENDING.store(true, Ordering::Release);
        });
    }
}

/// A hash of guest page `page` as it is now (the native host's `page_hash`).
fn page_hash(memory: &[u8], page: u64) -> u64 {
    let slots = lock(&IMPORT_SLOTS);
    gex_xlate::masked_page_hash(memory, page, gex_abi::DISPATCH_PAGE, &slots)
}

/// The import slots of every PE image placed (absolute, sorted): bytes the
/// loader writes that are not code ([`page_hash`]).
static IMPORT_SLOTS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

fn note_import_slots(base: u64, pe: &gex_ld::pe::PeImage) {
    let end = base + pe.image_size as u64;
    {
        let mut v = lock(&IMPORT_SLOTS);
        v.retain(|&(a, _)| a < base || a >= end);
        v.extend(gex_xlate::pe_import_slots(pe).into_iter().map(|(a, b)| (base + a, base + b)));
        v.sort_unstable();
    }
    let mut v = lock(&PE_IMAGES);
    v.retain(|&(a, b)| b <= base || a >= end);
    v.push((base, end));
}

/// Where the PE images with units are (start, end).
static PE_IMAGES: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Whether `addr` is inside a PE image a unit was placed for: a page of one
/// made non-executable for a while (Wine's loader lifts `.text` to fill an
/// import table there, then puts it back) is not gone (the hosts'
/// `gone_code`). Its units stay; its bytes are compared when it is
/// executable again (the kernel retires it then), and a changed page reads
/// as patched.
pub(crate) fn in_pe_image(addr: u64) -> bool {
    lock(&PE_IMAGES).iter().any(|&(a, b)| a <= addr && addr < b)
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
    // Every syscall asks: an atomic load when nothing was mapped (no lock).
    if !ANY_PENDING.load(Ordering::Acquire) && !wanted_any() {
        return false;
    }
    ANY_PENDING.store(false, Ordering::Release);
    let pending = std::mem::take(&mut *lock(&PENDING));
    // The NT trampoline page with the first images (the exe says the ABI),
    // as natively: every Wine DLL's unix calls and syscalls go through it.
    static TRAMPOLINES: OnceLock<()> = OnceLock::new();
    if let Some(p) = pending.first() {
        if TRAMPOLINES.set(()).is_ok() {
            let (image, _) = gex_kernel::win::trampoline_input(!p.pe.is64);
            place(&image, "[the NT trampolines]", image.bias, true, None);
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
        place(&image, &p.path, p.base, wine, Some(&p.pe));
    }
    wanted_any()
}

/// W3 (Wine's own Unix side): an i386 image Wine's loader mapped
/// (`gex.image_mapped`, not a wasm32 skeleton), placed as the NT layer's
/// are (the native host's `pe_units::w3_image` with `GEX_W3_UNITS=all`).
/// Never fatal: an image with no unit stays on tier-1, and one whose mapped
/// code is not yet its bytes (Wine relocates a moved image after mapping it)
/// waits for its first miss ([`w3_miss`]), as natively. Returns whether a
/// unit now waits for JavaScript.
pub(crate) fn w3_mapped(path: &str, base: u64, pe: &gex_ld::pe::PeImage) -> bool {
    if !on() {
        return false;
    }
    w3_trampolines(!pe.is64);
    let Ok((image, _)) = gex_xlate::func::pe_input(pe, base) else { return false };
    if !w3_matches(&image, base, pe, true) {
        w3_defer(Deferred { path: path.to_string(), base, size: pe.image_size as u64, src: Src::Pe(Arc::new(pe.clone())) });
        return false;
    }
    w3_forget(base, pe.image_size as u64);
    place(&image, path, base, true, Some(pe));
    wanted_any()
}

/// Whether an image Wine's loader mapped at `base` can wait for its first
/// miss unread ([`w3_defer_unread`]): whenever units are on. An image that
/// never runs is then never read, hashed or placed: a resource DLL (22 MB
/// of ehres.dll mapped three times by the shell for icons, 20 tzres.dll
/// maps for time zone names), each KnownDll wineboot maps for its imports
/// (40 of them, 3.8 s of a W64 program's start), the game's exe in the
/// first process, which only hands it to start.exe (its 261 MiB unit
/// fetched, compiled and placed for nothing). One that runs is placed at
/// its first miss, as a relocated image's is.
pub(crate) fn w3_may_defer(_path: &str, _base: u64) -> bool {
    on()
}

/// Leave the image open as `file`, `size` bytes mapped at `base`, for its
/// first miss ([`w3_miss`] reads and places it then).
pub(crate) fn w3_defer_unread(path: &str, base: u64, size: u64, file: Arc<dyn gex_kernel::vfs::FileHandle>) {
    w3_defer(Deferred { path: path.to_string(), base, size, src: Src::File(file) });
}

/// Where a deferred image's PE comes from: parsed at its mapping (its code
/// was not yet its bytes), or its file, read at the first miss.
enum Src {
    Pe(Arc<gex_ld::pe::PeImage>),
    File(Arc<dyn gex_kernel::vfs::FileHandle>),
}

struct Deferred {
    path: String,
    base: u64,
    size: u64,
    src: Src,
}

/// Images [`w3_mapped`] and [`w3_defer_unread`] left for their first miss.
static W3_DEFERRED: Mutex<Vec<Deferred>> = Mutex::new(Vec::new());
static W3_ANY_DEFERRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn w3_defer(d: Deferred) {
    let mut v = lock(&W3_DEFERRED);
    // An image mapped where an unmapped one waited (ehres.dll's three maps
    // at one base): the old entry would claim the new image's misses.
    v.retain(|o| o.base + o.size <= d.base || o.base >= d.base + d.size);
    v.push(d);
    W3_ANY_DEFERRED.store(true, Ordering::Release);
}

/// Drop deferred images overlapping `[base, base + size)` (one placed there now).
fn w3_forget(base: u64, size: u64) {
    if !W3_ANY_DEFERRED.load(Ordering::Acquire) {
        return;
    }
    let mut v = lock(&W3_DEFERRED);
    v.retain(|o| o.base + o.size <= base || o.base >= base + size);
    if v.is_empty() {
        W3_ANY_DEFERRED.store(false, Ordering::Release);
    }
}

/// The trampoline page's unit (and, for i386, its unix-call provider:
/// [`place`]) for a W3 process, with the first image it places: the page is
/// in memory from the start, and Wine's unix calls and syscalls all go
/// through it. Once per address space; a page that is not the one the unit
/// was built from keeps to the other tiers ([`place`] says so).
fn w3_trampolines(class32: bool) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let (image, _) = gex_kernel::nt::boot::trampoline_input(class32);
    place(&image, "[the trampolines]", image.bias, true, None);
    if crate::tracing() {
        crate::stderr_line(&format!("gex-cgen: the trampolines' unit wanted ({}-bit)\n", if class32 { 32 } else { 64 }));
    }
}

/// Whether the mapped code is `image`'s.
/// At the mapping (`at_map`) every byte counts, the import slots too: until
/// the loader has filled them (and put back the protection it lifts to do
/// it, which retires the page and the unit's functions on it) the unit must
/// wait for the image's first miss.
fn w3_matches(image: &gex_ld::Image, base: u64, pe: &gex_ld::pe::PeImage, at_map: bool) -> bool {
    let memory = crate::guest_bytes();
    let mask = if at_map { None } else { Some(pe) };
    image.segments.iter().filter(|s| s.executable).all(|seg| {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        memory
            .get(lo..lo + n)
            .is_some_and(|live| gex_xlate::pe_code_difference(seg.vaddr, &seg.bytes, live, base, mask).is_none())
    })
}

/// A tier-1 miss at `pc`: if it is inside an image [`w3_mapped`] deferred,
/// place that image's unit now (an image whose code still differs stays on
/// tier-1). Returns whether a unit now waits for JavaScript.
pub(crate) fn w3_miss(pc: u64) -> bool {
    if !W3_ANY_DEFERRED.load(Ordering::Acquire) {
        return false;
    }
    let d = {
        let mut d = lock(&W3_DEFERRED);
        let Some(i) = d.iter().position(|p| pc >= p.base && pc < p.base + p.size) else { return false };
        let p = d.swap_remove(i);
        if d.is_empty() {
            W3_ANY_DEFERRED.store(false, Ordering::Release);
        }
        p
    };
    let pe = match d.src {
        Src::Pe(pe) => pe,
        Src::File(f) => match read_pe(&*f) {
            Some(pe) => Arc::new(pe),
            None => {
                crate::warn_line(&format!("gex-w3: {} at {:#x}: reading the image at its first miss failed; tier-1\n", d.path, d.base));
                return false;
            }
        },
    };
    w3_trampolines(!pe.is64);
    let Ok((image, _)) = gex_xlate::func::pe_input(&pe, d.base) else { return false };
    if !w3_matches(&image, d.base, &pe, false) {
        crate::warn_line(&format!("gex-w3: {} at {:#x}: its code is not the image's at its first miss; tier-1\n", d.path, d.base));
        return false;
    }
    place(&image, &d.path, d.base, true, Some(&pe));
    wanted_any()
}

/// A whole file, parsed as a PE image.
pub(crate) fn read_pe(f: &dyn gex_kernel::vfs::FileHandle) -> Option<gex_ld::pe::PeImage> {
    let size = f.metadata().ok()?.size as usize;
    let mut b = vec![0u8; size];
    let mut at = 0;
    while at < size {
        match f.read_at(at as u64, &mut b[at..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => at += n as usize,
        }
    }
    b.truncate(at);
    gex_ld::pe::parse(&b).ok()
}

/// Want the unit for `image`, mapped at `base`: its key, and the check that
/// the mapped code is what the unit was built from (a mismatch ends the run,
/// as natively: a unit over different code is wrong code). `optional`: a
/// missing unit is fine even under `--cgen` (Wine's DLLs, the trampolines).
fn place(image: &gex_ld::Image, path: &str, base: u64, optional: bool, pe: Option<&gex_ld::pe::PeImage>) {
    let memory = crate::guest_bytes();
    let mut pages = Vec::new();
    // A native wasm32 Wine DLL's skeleton has no x86 code (no unit is ever
    // built for it: its `ud2` stubs are bound to Wine's PE module).
    let skeleton = pe.is_some_and(wasmdll::is_skeleton);
    if let Some(pe) = pe.filter(|_| !skeleton) {
        note_import_slots(base, pe);
    }
    for seg in image.segments.iter().filter(|s| s.executable && !skeleton) {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        let live = memory.get(lo..lo + n);
        if let Some(i) = live.and_then(|live| gex_xlate::pe_code_difference(seg.vaddr, &seg.bytes, live, base, pe)) {
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
    let known = known(base, path);
    let (key, mut providers) = unit_key(image, pe, base, known.as_ref().map(|k| k.0.clone()));
    // The trampoline page's providers (gex-cgen/providers/nt_unixcall.c),
    // bound at its entries after its unit, as an image's at its exports.
    if pe.is_none() && base == gex_kernel::win::TRAMPOLINES && image.class32 {
        providers.extend(trampoline_providers());
    }
    if let Some(pe) = pe {
        let dll = pe.exports.dll_name.to_ascii_lowercase();
        for (_, file) in lock(&STATEFUL).iter().filter(|(d, _)| *d == dll) {
            let mut exports = Vec::new();
            for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
                for n in &e.names {
                    exports.push((n.clone(), base + e.rva as u64));
                }
            }
            let entry = if pe.entry_rva != 0 { base + pe.entry_rva as u64 } else { 0 };
            providers.push(Provided { file: file.clone(), binds: Vec::new(), stateful: Some((exports, entry)), data: false, skeleton: None, lifted: false, groups: Vec::new() });
        }
    }
    // A native wasm32 Wine DLL: its exports are Wine's PE module's, not the
    // built-in providers' (as natively, `pe_units::install_providers`); the
    // key keeps them, as natively.
    if let Some(pe) = pe.filter(|pe| wasmdll::is_skeleton(pe)) {
        providers = vec![Provided {
            file: format!("wasmdll:{path}"),
            binds: Vec::new(),
            stateful: None,
            data: false,
            skeleton: Some(Arc::new(pe.clone())),
            lifted: false,
            groups: Vec::new(),
        }];
    }
    let identity = match known {
        Some((_, id)) => id,
        None => gex_key::image_identity(image).trim_start_matches("image:").to_string(),
    };
    let mut units = lock(&UNITS);
    units.push(Unit {
        identity,
        key_base: key.clone(),
        key,
        wine: optional,
        path: path.to_string(),
        base,
        pages,
        regs: Vec::new(),
        dirs: Vec::new(),
        slots: 0,
        providers,
    });
    u_want(units.len() - 1);
}

/// The key of the unit for `image` (the native host's `pe_units::unit_key`:
/// the image's key, extended by its export providers' stubs and sources),
/// and those providers. The indirect-call profile's part is added when JS
/// has read `<cache>/icall/<identity>.txt` (`gex_cgen_set_icall`).
fn unit_key(image: &gex_ld::Image, pe: Option<&gex_ld::pe::PeImage>, base: u64, known: Option<String>) -> (String, Vec<Provided>) {
    use std::hash::{Hash, Hasher};
    let mut key = known.unwrap_or_else(|| gex_cgen::image::image_key(image));
    let Some(pe) = pe else { return (key, Vec::new()) };
    // As natively (`pe_units::provided`): Wine's own build of a DLL leaves out
    // the `nowine` providers.
    let providers = gex_cgen::provider::for_image(&pe.exports.dll_name, !pe.is64, &pe.data);
    if providers.is_empty() {
        return (key, Vec::new());
    }
    let mut exports = std::collections::HashMap::new();
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        for n in &e.names {
            exports.insert(n.clone(), e.rva);
        }
    }
    let stubs = gex_cgen::provider::stubs(&providers, &exports, base);
    if !stubs.is_empty() {
        // As natively `stubs.hash(h)`: a slice's length is a `usize`, eight
        // bytes there, so it is written as a u64 here.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write_u64(stubs.len() as u64);
        for a in &stubs {
            a.hash(&mut h);
        }
        gex_cgen::provider::fingerprint(&providers).hash(&mut h);
        key = format!("{key}-p{:016x}", h.finish());
    }
    let provided = providers
        .iter()
        .map(|p| Provided {
            file: format!("providers/{}-{}.wasm", p.name, gex_cgen::provider::fingerprint(std::slice::from_ref(p))),
            binds: gex_cgen::provider::pe_binds(p, &exports).into_iter().filter_map(|(f, e)| exports.get(e).map(|&rva| (f.clone(), base + rva as u64))).collect(),
            stateful: None,
            data: p.stateful,
            skeleton: None,
            lifted: p.lifted,
            groups: gex_cgen::provider::group_addrs(p, &exports, base),
        })
        .filter(|p| !p.binds.is_empty())
        .collect();
    (key, provided)
}

/// The NT trampoline page's providers, as [`unit_key`] gives an image's.
fn trampoline_providers() -> Vec<Provided> {
    use gex_kernel::win::{tramp, TRAMPOLINES};
    let exports = gex_cgen::provider::trampoline_exports((tramp::UNIX_CALL_32 - TRAMPOLINES) as u32);
    gex_cgen::provider::for_dll(gex_cgen::provider::TRAMPOLINES_DLL, true)
        .iter()
        .map(|p| Provided {
            file: format!("providers/{}-{}.wasm", p.name, gex_cgen::provider::fingerprint(std::slice::from_ref(p))),
            binds: gex_cgen::provider::pe_binds(p, &exports).into_iter().filter_map(|(f, e)| exports.get(e).map(|&rva| (f.clone(), TRAMPOLINES + rva as u64))).collect(),
            stateful: None,
            data: p.stateful,
            skeleton: None,
            lifted: false,
            groups: gex_cgen::provider::group_addrs(p, &exports, TRAMPOLINES),
        })
        .filter(|p| !p.binds.is_empty())
        .collect()
}

/// Where unit `u`'s indirect-call profile is in the unit cache
/// (`icall/<identity>.txt`), for JS to read.
#[no_mangle]
pub extern "C" fn gex_cgen_icall_path(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).map_or(0, |x| put_str(&format!("icall/{}.txt", x.identity), buf, cap))
}

/// Unit `u`'s indirect-call profile (the `len` bytes at `ptr`, or none):
/// its key gains the profile's part, exactly as natively (`pe_units::
/// icall_hints` and `unit_key`).
#[no_mangle]
pub extern "C" fn gex_cgen_set_icall(u: u32, ptr: u32, len: u32) {
    use std::hash::{Hash, Hasher};
    let text = if len == 0 { String::new() } else { String::from_utf8_lossy(crate::shim_slice(ptr, len)).into_owned() };
    let mut units = lock(&UNITS);
    let Some(x) = units.get_mut(u as usize) else { return };
    // The counting flavor devirtualises nothing: its key has no profile.
    let hints = if gex_cgen::image::icall_count() { Default::default() } else { icall_hints(&text) };
    x.key = x.key_base.clone();
    if !hints.is_empty() {
        let mut v: Vec<(u64, u64)> = hints.into_iter().collect();
        v.sort_unstable();
        // As natively `v.hash(h)`: the length a u64 (usize there).
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write_u64(v.len() as u64);
        for p in &v {
            p.hash(&mut h);
        }
        x.key = format!("{}-i{:016x}", x.key_base, h.finish());
    }
}

/// `pe_units::icall_hints` on the profile's text: a call site whose dominant
/// target takes at least 90% of at least 64 calls, by return address.
fn icall_hints(text: &str) -> std::collections::HashMap<u64, u64> {
    use std::collections::HashMap;
    let num = |w: &str| u64::from_str_radix(w.trim_start_matches("0x"), if w.starts_with("0x") { 16 } else { 10 }).ok();
    let mut sites: HashMap<u64, HashMap<u64, u64>> = HashMap::new();
    for l in text.lines() {
        let Some(k) = l.find("icall ") else { continue };
        let w: Vec<&str> = l[k + 6..].split_whitespace().collect();
        if let [r, t, n, ..] = w[..] {
            if let (Some(r), Some(t), Some(n)) = (num(r), num(t), num(n)) {
                *sites.entry(r).or_default().entry(t).or_default() += n;
            }
        }
    }
    let mut hints = HashMap::new();
    for (ret, targets) in sites {
        let total: u64 = targets.values().sum();
        let (t, best) = targets.into_iter().max_by_key(|&(t, n)| (n, std::cmp::Reverse(t))).unwrap();
        if best >= 64 && best * 10 >= total * 9 {
            hints.insert(ret, t);
        }
    }
    hints
}

/// The oldest unit waiting for its bytes, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_wanted() -> i32 {
    drain();
    // Not one another Worker is publishing (claimed): it stops being wanted
    // when that publish ends.
    u_oldest(|st| st & U_CLAIMED != 0).map_or(-1, |u| u as i32)
}

/// `v` into a buffer of the shim's heap (above the guest's limit).
pub(crate) fn put_u32s(v: &[u32], buf: u32) {
    let out = crate::shim_slice_mut(buf, 4 * v.len() as u32);
    for (c, x) in out.chunks_exact_mut(4).zip(v) {
        c.copy_from_slice(&x.to_le_bytes());
    }
}

fn put_str(s: &str, buf: u32, cap: u32) -> u32 {
    let n = s.len().min(cap as usize);
    // A buffer from the shim's heap, above the guest's limit.
    crate::shim_slice_mut(buf, n as u32).copy_from_slice(&s.as_bytes()[..n]);
    s.len() as u32
}

/// Unit `u`'s key (the directory its files are in) into `buf`; its length.
#[no_mangle]
pub extern "C" fn gex_cgen_key(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).map_or(0, |x| put_str(&x.key, buf, cap))
}

/// Whether code a published unit's image runs translated is reported
/// (`gex_cgen_set_record`): gex-eject's W3 recorder, whose AOT-only project
/// would have no code there.
static RECORD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_cgen_set_record(on: i32) {
    RECORD.store(on != 0, Ordering::Relaxed);
}

/// A miss at `pc`, which no function holds: when it is on a code page of an
/// image whose unit is published, `[gex-record] uncovered PC KEY PATH`, once
/// per address. A run without translation needs a region entry there.
pub(crate) fn record_uncovered(pc: u64) {
    if !RECORD.load(Ordering::Relaxed) || knows(pc) {
        return;
    }
    static SEEN: Mutex<Option<std::collections::HashSet<u64>>> = Mutex::new(None);
    let page = pc / gex_abi::DISPATCH_PAGE;
    let published = lock(&PUBLISHED).clone();
    let units = lock(&UNITS);
    let Some(u) = published.iter().filter_map(|&i| units.get(i)).find(|u| u.pages.iter().any(|&(p, _)| p == page)) else { return };
    if !lock(&SEEN).get_or_insert_with(std::collections::HashSet::new).insert(pc) {
        return;
    }
    crate::warn_line(&format!("[gex-record] uncovered {pc:#x} {} {}\n", u.key, u.path));
}

/// The address unit `u`'s image is at (its key's base).
#[no_mangle]
pub extern "C" fn gex_cgen_base(u: u32) -> u64 {
    lock(&UNITS).get(u as usize).map_or(0, |x| x.base)
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
    u_unwant(u as usize);
    let units = lock(&UNITS);
    let Some(x) = units.get(u as usize) else { return 0 };
    if MODE.load(Ordering::Relaxed) == 2 && !x.wine {
        crate::warn_line(&format!(
            "gex-cgen: --cgen: {} has no function-tier unit for base {:#x} (key {}); build one natively first\n",
            x.path, x.base, x.key
        ));
        return 1;
    }
    // A unit gexc builds apart from any image (the NT trampolines: every
    // system call and Unix call goes through them): no "unitless" image
    // line names it, and without it every trampoline misses into tier-1.
    if x.path.starts_with('[') {
        crate::warn_line(&format!(
            "gex-nt: missing unit: {} (key {}) has no function-tier unit, so every call through it misses into tier-1; \
             build it with GEX_CGEN_CACHE=<the unit cache> gexc cgen-image --nt-trampolines32\n",
            x.path, x.key
        ));
        return 0;
    }
    // Loudly, as natively: a unit built at another base (or under another
    // key) is silently not used otherwise.
    crate::warn_line(&format!(
        "gex-nt: image {} at base {:#x}: no function-tier unit (key {}), tier-1; build it with \
         {}gexc cgen-image <image> --pe-base {:#x}\n",
        x.path,
        x.base,
        x.key,
        if gex_cgen::image::unguarded() { "" } else { "GEX_CGEN_GUARD=1 " },
        x.base
    ));
    0
}

/// Publish unit `u` with its `n` slots (u64s at `entries`: the guest address
/// of each slot its element segment fills, 0 for none): a range of the
/// function table, each entry mapped to its slot, the probe cache cleared for
/// them, and a bridge in tier-1's dispatch slot for each, numbering their
/// pages -- the native `ftab::publish`. 0, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_publish(u: u32, entries: u32, n: u32) -> i32 {
    let claimed = U_STATE.get(u as usize).map_or(U_CLAIMED, |s| s.fetch_or(U_CLAIMED, Ordering::AcqRel));
    if claimed & U_CLAIMED != 0 {
        TWICE.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    let raw = crate::shim_slice(entries, 8 * n);
    let slots: Vec<u64> = raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    // A lifted twin is no entry of its own address: no cache slot, no bridge.
    let addrs: Vec<u64> = slots.iter().copied().filter(|&a| a != 0 && a & gex_abi::ftab::TWIN == 0).collect();
    let pairs: Vec<(u64, u32)> = slots.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    ftab(|m| m.publish(slots.len() as u32, u, Tier::Unit, &pairs));
    {
        let mem = crate::guest_bytes();
        for &a in &addrs {
            gex_kernel::ftab::fcache_clear(mem, a);
        }
    }
    let (regs, dirs, nslots) = bridges(&addrs);
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
            let off = (gex_abi::patched_pages() + page / 8) as usize;
            if let Some(b) = mem.get_mut(off) {
                *b &= !(1 << (page % 8));
            }
        }
    }
    lock(&PUBLISHED).push(u as usize);
    if let Some(s) = U_STATE.get(u as usize) {
        s.fetch_or(U_PUBLISHED, Ordering::AcqRel);
    }
    u_unwant(u as usize);
    0
}

/// A region-entry module for placed unit `u` (docs/eject-pe.md §3.3): an
/// entry of its own whose key is `regions/<u's key>`, which the loaders find
/// at `<cache>/regions/<key>/` as natively. JS publishes it after the unit,
/// when it has one. Returns its index, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_region(u: u32) -> i32 {
    let mut units = lock(&UNITS);
    let Some(x) = units.get(u as usize) else { return -1 };
    let r = Unit {
        identity: x.identity.clone(),
        key_base: format!("regions/{}", x.key),
        key: format!("regions/{}", x.key),
        wine: true,
        path: x.path.clone(),
        base: x.base,
        pages: Vec::new(),
        regs: Vec::new(),
        dirs: Vec::new(),
        slots: 0,
        providers: Vec::new(),
    };
    units.push(r);
    (units.len() - 1) as i32
}

/// Unit `u` is a link group's member (docs/eject-pe.md §6a): not wanted on
/// its own. JS publishes the group's module once every member is placed
/// ([`gex_cgen_group`]); Wine maps an exe's static imports before any of
/// their code runs, so no member runs before it.
#[no_mangle]
pub extern "C" fn gex_cgen_hold(u: u32) {
    u_unwant(u as usize);
}

/// A link group's module as a unit of its own: key `groups/<key>` (the
/// `len` bytes at `key`), the pages of its `n` members (unit indices, u32s
/// at `members`). JS publishes it as a unit; each member's providers and
/// region entries follow. Returns its index, or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_group(members: u32, n: u32, key: u32, len: u32) -> i32 {
    let us: Vec<usize> = crate::shim_slice(members, 4 * n).chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()) as usize).collect();
    let Ok(key) = std::str::from_utf8(crate::shim_slice(key, len)).map(|k| format!("groups/{k}")) else { return -1 };
    let mut units = lock(&UNITS);
    let mut pages = Vec::new();
    let mut paths = Vec::new();
    for &u in &us {
        let Some(x) = units.get(u) else { return -1 };
        pages.extend(x.pages.iter().copied());
        paths.push(x.path.rsplit(['/', '\\']).next().unwrap_or("").to_string());
    }
    let base = us.first().and_then(|&u| units.get(u)).map_or(0, |x| x.base);
    units.push(Unit {
        identity: String::new(),
        key_base: key.clone(),
        key,
        wine: false,
        path: format!("the link group {}", paths.join(" + ")),
        base,
        pages,
        regs: Vec::new(),
        dirs: Vec::new(),
        slots: 0,
        providers: Vec::new(),
    });
    (units.len() - 1) as i32
}

/// The bridges that make `addrs` reachable from tier-1: each address's
/// dispatch slot (numbering its page if it has none) as `[table, slot]`,
/// flattened for JS (a dense slot gets an index of its own in `blocks`,
/// `gex_emit::dense::TABLE_INDEX`), the pages it numbered, and what the
/// dispatch table must hold.
pub(crate) fn bridges(addrs: &[u64]) -> (Vec<u32>, Vec<(u64, u32)>, u64) {
    crate::with_jit_only(|j| {
        let mut seen: std::collections::BTreeSet<(u32, u32)> = Default::default();
        // Every page the entries are on, numbered here or before: the
        // install writes their directory entries. Only the fresh ones used to
        // be listed, so a second publish over the same pages (two threads
        // placing one image) listed none, and an install that read that list
        // wrote no entry at all (scratch/P/stall85: Paint's start-up spun on
        // a block whose next page had no entry).
        let mut pages: std::collections::BTreeMap<u64, u32> = Default::default();
        for &a in addrs {
            let page = a / gex_abi::DISPATCH_PAGE;
            if page >= gex_abi::dispatch_max_pages() {
                continue;
            }
            let base = match j.page_table.lookup(page) {
                Some(b) => b,
                None => crate::number_by_identity(j, page),
            };
            pages.insert(page, base);
            let t = gex_abi::dispatch_entry_table(base) as u32;
            seen.insert((t, gex_abi::dispatch_index(a, base) as u32));
        }
        let mut regs = Vec::with_capacity(2 * seen.len());
        // A dense table's slot gets an arm of an entry in `blocks`, which JS
        // writes the bridge at (as table `gex_emit::dense::TABLE_INDEX`),
        // shared with other bridged slots (`gex_emit::dense::bridge_words`).
        // One word per slot for good: bridged again (`elfprov::rebridge`, a
        // second publish over the same pages), it gets the one it had, whose
        // entry every Worker that wrote the bridge there still holds.
        let dense: Vec<(u32, u32)> = seen.iter().copied().filter(|&(t, _)| crate::dense::map(t as usize).is_some()).collect();
        let entries: Vec<u32> = dense.iter().map(|&(t, slot)| gex_abi::dispatch_entry(t as usize, slot)).collect();
        let words = gex_emit::dense::bridge_words(&mut j.bridge_dense, &mut j.bridge_group, &entries, crate::dense::alloc);
        let mut filled = std::collections::BTreeSet::new();
        for (&entry, &w) in entries.iter().zip(&words) {
            crate::dense::set_slot_word(entry, w);
            // Each entry once: its arms all run the bridge.
            if filled.insert(gex_emit::dense::index_of(w)) {
                regs.extend_from_slice(&[gex_emit::dense::TABLE_INDEX, gex_emit::dense::index_of(w)]);
            }
        }
        for (t, slot) in seen {
            if crate::dense::map(t as usize).is_none() {
                regs.extend_from_slice(&[t, slot]);
            }
        }
        j.translated.extend(addrs.iter().copied());
        (regs, pages.into_iter().collect(), j.page_table.slots())
    })
}

/// How many export providers unit `u`'s image has.
#[no_mangle]
pub extern "C" fn gex_cgen_provider_count(u: u32) -> u32 {
    lock(&UNITS).get(u as usize).map_or(0, |x| x.providers.len() as u32)
}

/// Provider `i` of unit `u`: its module's file under the unit cache.
#[no_mangle]
pub extern "C" fn gex_cgen_provider_file(u: u32, i: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).and_then(|x| x.providers.get(i as usize)).map_or(0, |p| put_str(&p.file, buf, cap))
}

/// Bind provider `i` of unit `u`, whose module is the `len` bytes at `ptr`,
/// at its exports' addresses, after the unit (as the native
/// `install_providers`): a range of the function table filled by its
/// element segment, the addresses mapped to the slots holding the C
/// functions bound to them, and bridges from tier-1. Returns the
/// provider's module number (an install names `PROVIDER | number`), or -1.
#[no_mangle]
pub extern "C" fn gex_cgen_provider_publish(u: u32, i: u32, ptr: u32, len: u32) -> i32 {
    let (file, mut binds, stateful, data, groups) = {
        let units = lock(&UNITS);
        let Some(p) = units.get(u as usize).and_then(|x| x.providers.get(i as usize)) else { return -1 };
        (p.file.clone(), p.binds.clone(), p.stateful.clone(), p.data, p.groups.clone())
    };
    if lock(&UNITS).get(u as usize).and_then(|x| x.providers.get(i as usize)).is_some_and(|p| p.lifted) {
        // Only where its unit keeps the lifted twin it falls back to, as
        // natively (`pe_units::install_providers`).
        let had = binds.len();
        binds.retain(|(_, a)| ftab(|m| m.twin(*a).is_some()));
        if binds.len() < had {
            crate::warn_line(&format!("gex-nt: provider {file}: {} of {had} exports left unbound (no lifted twin in its unit)\n", had - binds.len()));
        }
    }
    let wasm = crate::shim_slice(ptr, len);
    let names = match gex_cgen::provider::slot_names(wasm) {
        Ok(n) => n,
        Err(e) => {
            crate::warn_line(&format!("gex-cgen: provider {file}: {e}\n"));
            return -1;
        }
    };
    let mut memory_base = 0u64;
    let pairs: Vec<(u64, u32)> = match &stateful {
        None => {
            if data {
                // A built-in stateful provider's data, top down as natively
                // (`pe_units::install_providers`).
                match builtin_data(&file, wasm) {
                    Ok(b) => memory_base = b as u64,
                    Err(e) => {
                        crate::warn_line(&format!("gex-nt: provider {file}: {e}\n"));
                        return -1;
                    }
                }
            }
            let mut pairs: Vec<(u64, u32)> = binds
                .iter()
                .filter_map(|(f, a)| names.iter().position(|s| s.as_deref() == Some(f.as_str())).map(|k| (*a, k as u32)))
                .collect();
            // A group the filters above cut into (a lifted twin the unit
            // lacks, a slot the module lacks) is left to the guest whole.
            let dropped = gex_cgen::provider::whole_groups(&mut pairs, &groups);
            if dropped > 0 {
                crate::warn_line(&format!("gex-nt: provider {file}: {dropped} exports of incomplete groups left unbound\n"));
            }
            pairs
        }
        Some((exports, entry)) => {
            match stateful_pairs(&file, wasm, &names, exports, *entry) {
                Ok((pairs, base)) => {
                    memory_base = base as u64;
                    pairs
                }
                Err(e) => {
                    crate::warn_line(&format!("gex-nt: stateful provider {file}: {e}\n"));
                    crate::nt::end_process(0xc000_0135);
                }
            }
        }
    };
    if pairs.is_empty() {
        return -1;
    }
    let number = lock(&PROVIDERS).len() as u32;
    ftab(|m| m.publish(names.len() as u32, PROVIDER | number, Tier::Unit, &pairs));
    let addrs: Vec<u64> = pairs.iter().map(|&(a, _)| a).collect();
    {
        let mem = crate::guest_bytes();
        for &a in &addrs {
            gex_kernel::ftab::fcache_clear(mem, a);
        }
    }
    let (regs, dirs, slots) = bridges(&addrs);
    crate::trace_line(&format!("gex-nt: provider {file}: {} exports\n", pairs.len()));
    lock(&PROVIDERS).push(ProviderModule { file, regs, dirs, slots, memory_base });
    number as i32
}

/// A stateful provider's bindings (the native `libprov::install`): each
/// export to `gexp_<export>` or `gexp__missing`, the entry point to
/// `gexp__entry`; and its data, allocated below 2 GiB.
fn stateful_pairs(file: &str, wasm: &[u8], names: &[Option<String>], exports: &[(String, u64)], entry: u64) -> Result<(Vec<(u64, u32)>, u32), String> {
    let missing = names
        .iter()
        .position(|s| s.as_deref() == Some("gexp__missing"))
        .ok_or_else(|| "no gexp__missing in its module's table".to_string())? as u32;
    let mut provided: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for (k, s) in names.iter().enumerate() {
        if let Some(e) = s.as_deref().and_then(|s| s.strip_prefix("gexp_")).filter(|e| !e.starts_with('_') && !e.starts_with("ptr_")) {
            provided.insert(e, k as u32);
        }
    }
    let mut pairs = Vec::new();
    let mut unprovided = Vec::new();
    for (e, addr) in exports {
        match provided.get(e.as_str()) {
            Some(&k) => pairs.push((*addr, k)),
            None => {
                pairs.push((*addr, missing));
                unprovided.push((*addr, e.clone()));
            }
        }
    }
    if let (Some(k), true) = (names.iter().position(|s| s.as_deref() == Some("gexp__entry")), entry != 0) {
        pairs.push((entry, k as u32));
    }
    pairs.sort_by_key(|&(a, k)| (a, k == missing));
    pairs.dedup_by_key(|&mut (a, _)| a);
    lock(&MISSING).get_or_insert_with(Default::default).extend(unprovided);
    let (size, align) = dylink_memory(wasm)?;
    let data = provider_alloc_bytes((size as u64).max(16) + (1u64 << align), &format!("[provider {file} data]"))?;
    let data = (data + (1u64 << align) - 1) & !((1u64 << align) - 1);
    crate::warn_line(&format!("gex-nt: stateful provider {file}: {} exports, data at {data:#x}\n", pairs.len()));
    Ok((pairs, data as u32))
}

/// The `dylink.0` memory size and alignment (log2) of a PIC module.
fn dylink_memory(wasm: &[u8]) -> Result<(u32, u32), String> {
    let e = |e: wasmparser::BinaryReaderError| e.to_string();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(c) = payload.map_err(e)? {
            if c.name() != "dylink.0" {
                continue;
            }
            let mut r = wasmparser::BinaryReader::new(c.data(), 0);
            while !r.eof() {
                let kind = r.read_u8().map_err(e)?;
                let len = r.read_var_u32().map_err(e)? as usize;
                let start = r.original_position();
                if kind == 1 {
                    let size = r.read_var_u32().map_err(e)?;
                    let align = r.read_var_u32().map_err(e)?;
                    return Ok((size, align));
                }
                r.read_bytes(len - (r.original_position() - start)).map_err(e)?;
            }
        }
    }
    Err("not a position-independent module (no dylink.0 memory info)".into())
}

/// Guest pages from the NT allocator, for a stateful provider.
fn provider_alloc_bytes(size: u64, name: &str) -> Result<u64, String> {
    let Some(p) = crate::nt::process() else { return linux_alloc(size, name) };
    gex_kernel::nt::boot::alloc_private(p, crate::sigs::proc_home().mm(), size, name)
}

/// Guest pages in a Linux process (an ELF provider's data and stack): an
/// anonymous private mapping, as natively (`libprov::alloc_linux`).
fn linux_alloc(size: u64, name: &str) -> Result<u64, String> {
    use gex_kernel::mem::{flags, prot};
    let len = (size + 0xfff) & !0xfff;
    // Below 4 GiB in a 64-bit memory too: the page hands these addresses around as 32 bits.
    // The map's own lock, held across both: the clean ranges are those
    // the mapping is placed against.
    let (start, dirty) = (|| {
        let mut mm = crate::sigs::proc_home().mm().lock();
        let clean = mm.clean_ranges().to_vec();
        let start = mm.mmap_low(0, len, prot::READ | prot::WRITE, flags::PRIVATE | flags::ANONYMOUS, name)?;
        Ok::<_, gex_kernel::Errno>((start, gex_kernel::mem::dirty_parts(&clean, start, start + len)))
    })()
    .map_err(|e| format!("mapping {name}: errno {}", e.0))?;
    // Read as zero, as the kernel's anonymous `mmap` makes it: the range may
    // hold what a forked parent left there before this process's `execve`
    // (W3: Wine's PE module's data in the grandchild of a process that had
    // it at the same address, whose stale bss cached another process's
    // function-table indices).
    let mut gm = GuestMem::new(crate::guest_bytes());
    for (a, b) in dirty {
        gm.zero_dirty(a, (b - a) as usize).map_err(|e| format!("zeroing {name}: errno {}", e.0))?;
    }
    Ok(start)
}

/// [`provider_alloc_bytes`] from the top down (`boot::alloc_private_top`): a
/// built-in provider's data, heap and stack, which then do not move the DLLs
/// the program maps after them.
fn provider_alloc_top_bytes(size: u64, name: &str) -> Result<u64, String> {
    let Some(p) = crate::nt::process() else { return linux_alloc(size, name) };
    gex_kernel::nt::boot::alloc_private_top(p, crate::sigs::proc_home().mm(), size, name)
}

/// A built-in stateful provider's data (its `dylink.0` size, aligned).
fn builtin_data(file: &str, wasm: &[u8]) -> Result<u32, String> {
    let (size, align) = dylink_memory(wasm)?;
    let a = 1u64 << align;
    let data = provider_alloc_top_bytes((size as u64).max(16) + a, &format!("[provider {file} data]"))?;
    Ok(((data + a - 1) & !(a - 1)) as u32)
}

/// `gex.provider_alloc_top(size)`: fresh read-write guest pages from the top
/// down (crt_heap's heap), or 0.
#[no_mangle]
pub extern "C" fn gex_provider_alloc_top(size: u32) -> u32 {
    match provider_alloc_top_bytes(size as u64, "[provider heap]") {
        Ok(a) => a as u32,
        Err(e) => {
            crate::warn_line(&format!("gex-nt: provider_alloc_top({size:#x}): {e}\n"));
            0
        }
    }
}

/// This Worker's provider stack: top down unless an ejected project's
/// stateful providers are set (its sessions were recorded with the stack
/// bottom up, as the native `libprov::stack`).
#[no_mangle]
pub extern "C" fn gex_provider_stack(size: u32) -> u32 {
    // W3 with the split (`?w3split=1`): the guest's 2 GiB are Wine's and a
    // game may fill them (one with its units left no room for a Worker's
    // stack), while only wasm code runs on this one. It comes from the shim's
    // heap, above 2 GiB, zeroed, and lives as long as the process.
    if crate::winspace::w3_split() && crate::nt::process().is_none() {
        let layout = std::alloc::Layout::from_size_align(size as usize, 4096).expect("provider stack layout");
        // SAFETY: a non-zero size; the block is never freed (the process's stack).
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        return p as u32;
    }
    if lock(&STATEFUL).is_empty() {
        gex_provider_alloc_top(size)
    } else {
        gex_provider_alloc(size)
    }
}

/// `gex.clock_ns(clk)`: `clock_gettime(clk)` in nanoseconds, the value the
/// system call gives, with no kernel and no lock (W3's PE ntdll reads its
/// performance counter with it, docs/wine-overhead.md F2).
#[no_mangle]
pub extern "C" fn gex_clock_ns(clk: u32) -> i64 {
    gex_kernel::clock_ns(crate::browser_clock, clk as u64) as i64
}

/// `gex.nt_qpc()`: the process clock as NtQueryPerformanceCounter reads it
/// (100 ns units), for the nt_qpc provider.
#[no_mangle]
pub extern "C" fn gex_nt_qpc() -> i64 {
    crate::nt::process().map_or(0, |p| ((p.clocks.mono_ns)() / 100) as i64)
}

/// Provider module `number`'s data (`__memory_base`), 0 when stateless.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_memory_base(number: u32) -> u32 {
    lock(&PROVIDERS).get(number as usize).map_or(0, |p| p.memory_base as u32)
}

/// [`gex_cgen_pmod_memory_base`]'s upper half: 0 but for Wine's PE module
/// in a 64-bit memory, whose data is above 4 GiB.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_memory_base_hi(number: u32) -> u32 {
    lock(&PROVIDERS).get(number as usize).map_or(0, |p| (p.memory_base >> 32) as u32)
}

/// `gex.provider_alloc(size)`: fresh read-write guest pages, or 0.
#[no_mangle]
pub extern "C" fn gex_provider_alloc(size: u32) -> u32 {
    match provider_alloc_bytes(size as u64, "[provider heap]") {
        Ok(a) => a as u32,
        Err(e) => {
            crate::warn_line(&format!("gex-nt: provider_alloc({size:#x}): {e}\n"));
            0
        }
    }
}

/// `gex.provider_ctx()`: the calling thread's context.
#[no_mangle]
pub extern "C" fn gex_provider_ctx() -> u32 {
    crate::my_ctx() as u32
}

/// `gex.provider_missing(addr)`: the program called an export the provider
/// does not implement. Ends the process, saying which.
#[no_mangle]
pub extern "C" fn gex_provider_missing(addr: u32) {
    let addr = addr as u64;
    let name = lock(&MISSING).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
    crate::warn_line(&format!(
        "gex-eject: the program called {name} ({}), an export of a replaced library that its provider does not implement\n",
        crate::nt::describe(addr)
    ));
    crate::nt::end_process(0xc000_0139);
}

/// Whether the provider data at `base` still needs its relocations applied
/// (once per process): 1 the first time, then 0.
#[no_mangle]
pub extern "C" fn gex_provider_relocate(base: u32) -> i32 {
    static DONE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let mut d = lock(&DONE);
    if d.contains(&base) {
        return 0;
    }
    d.push(base);
    1
}

/// GEX_NT_ENTRY_LOG: print each pc entered inside a unit's pages without an
/// entry there, once (`gex-nt: web entry <pc>`), for an ejected project's
/// training run.
pub(crate) static ENTRY_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the function table has a function for `pc`.
pub(crate) fn knows(pc: u64) -> bool {
    ftab(|m| m.get(pc).is_some())
}

/// Whether this is an ejected project's training run (`GEX_NT_ENTRY_LOG`).
pub(crate) fn entry_log() -> bool {
    ENTRY_LOG.load(Ordering::Relaxed)
}

pub(crate) fn note_web_entry(pc: u64) {
    if !ENTRY_LOG.load(Ordering::Relaxed) {
        return;
    }
    let page = pc / gex_abi::DISPATCH_PAGE;
    if !lock(&UNIT_PAGES).as_ref().is_some_and(|m| m.contains_key(&page)) {
        return;
    }
    static SEEN: Mutex<Option<std::collections::HashSet<u64>>> = Mutex::new(None);
    if lock(&SEEN).get_or_insert_with(Default::default).insert(pc) {
        // Through the guest's stderr, which run.mjs reads: a Worker's console
        // output under Node reaches the parent only when the Worker's event
        // loop turns, and a run that never yields loses it at the end.
        let line = format!("gex-nt: web entry {pc:#x}\n");
        unsafe { crate::write_stderr(&line) };
    }
}

/// `gex.provider_try` calls and the throws they caught (GEX_NT_STATS).
pub(crate) static TRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static THROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn gex_provider_note(thrown: i32) {
    TRIES.fetch_add(1, Ordering::Relaxed);
    if thrown != 0 {
        THROWS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Provider module `number`'s file, bridges and table need, as for a unit.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_file(number: u32, buf: u32, cap: u32) -> u32 {
    lock(&PROVIDERS).get(number as usize).map_or(0, |p| put_str(&p.file, buf, cap))
}

#[no_mangle]
pub extern "C" fn gex_cgen_pmod_regs(number: u32, buf: u32, cap: u32) -> u32 {
    let ps = lock(&PROVIDERS);
    let Some(p) = ps.get(number as usize) else { return 0 };
    let n = p.regs.len().min(cap as usize);
    put_u32s(&p.regs[..n], buf);
    p.regs.len() as u32
}

#[no_mangle]
pub extern "C" fn gex_cgen_pmod_slots(number: u32) -> u32 {
    lock(&PROVIDERS).get(number as usize).map_or(0, |p| p.slots.min(u32::MAX as u64) as u32)
}

/// The publishing Worker installed provider module `number`: the
/// directory for the pages its bridges numbered.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_installed(number: u32) {
    let dirs = lock(&PROVIDERS).get(number as usize).map(|p| p.dirs.clone()).unwrap_or_default();
    write_dirs(dirs);
}

pub(crate) fn write_dirs(dirs: Vec<(u64, u32)>) {
    crate::with_jit_only(|j| {
        let mut gm = GuestMem::new(crate::guest_bytes());
        for (page, base) in dirs {
            if j.page_table.lookup(page) == Some(base) {
                crate::dense::note_entry(base);
                let _ = gm.write_u32(gex_abi::dispatch_dir() + page * 4, base);
            }
        }
    });
}

/// Publish a module of `len` slots into the function table (`pairs`: guest
/// address, slot): its install's number.
pub(crate) fn publish(len: u32, module: u32, tier: Tier, pairs: &[(u64, u32)]) -> u32 {
    ftab(|m| m.publish(len, module, tier, pairs))
}

/// `gex.ftab_twin(addr)`: the table index of the lifted twin of the export
/// at `addr` (`gex_abi::ftab::TWIN`), 0 when its unit has none -- a
/// provider's fallback (provider.h `gp_lifted`). Each address's first
/// fallback is reported, every one counted (`TWIN_CALLS`), as natively.
#[no_mangle]
pub extern "C" fn gex_ftab_twin(addr: i64) -> i32 {
    let addr = addr as u64;
    TWIN_CALLS.fetch_add(1, Ordering::Relaxed);
    let idx = ftab(|m| m.twin(addr).map_or(0, |f| f.idx));
    if lock(&TWIN_SEEN).insert(addr) {
        crate::warn_line(&if idx == 0 {
            format!("gex: provider at {addr:#x} fell back to its lifted body, but its unit has none\n")
        } else {
            format!("gex: provider at {addr:#x} fell back to its lifted body\n")
        });
    }
    idx as i32
}

/// Fallbacks to lifted twins so far ([`gex_ftab_twin`]).
pub(crate) static TWIN_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TWIN_SEEN: Mutex<std::collections::BTreeSet<u64>> = Mutex::new(std::collections::BTreeSet::new());

/// The function table index serving `addr` now, if any.
pub(crate) fn index_of(addr: u64) -> Option<u32> {
    ftab(|m| m.get(addr).map(|f| f.idx))
}

/// Whether a unit built ahead of time (or a provider) serves the function at
/// `addr`: not the JIT's own functions, whose resume points are published
/// at addresses inside functions.
pub(crate) fn unit_serves(addr: u64) -> bool {
    ftab(|m| m.get(addr).is_some_and(|f| m.install(f.install).tier == Tier::Unit))
}

/// Whether a provider's function serves `addr` (a W3 builtin's export, an
/// export provider): native code in place of the bytes there, which may be
/// no code at all (a skeleton's `ud2` stub). A retirement of its page
/// forgets the bridge and keeps the binding, so tier-1 must neither lift it
/// (its discovery follows `call [iat]`) nor translate it at a miss, but
/// bridge it again (`elfprov::rebridge`): a hooking library patching code
/// beside a bound export would otherwise run its stub (c000001d).
pub(crate) fn provider_serves(addr: u64) -> bool {
    ftab(|m| m.get(addr).is_some_and(|f| m.install(f.install).module >= PROVIDER))
}

/// Whether a current JIT function serves `addr` (its pages unchanged since).
/// After the JIT forgets (`fjit::on_retire`) this is what still knows
/// which functions it has made.
pub(crate) fn jit_serves(addr: u64) -> bool {
    ftab(|m| m.get(addr).is_some_and(|f| m.install(f.install).tier == Tier::Jit && crate::fjit::current(f.install)))
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
    put_u32s(&x.regs[..n], buf);
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
/// at a unit's entry is then a catch-up, not a translation. Whether or not
/// units are on: a wasm32 program's module is installed through the log
/// with them off, and a bridge whose `blocks` entry a Worker already holds
/// (bridges share entries) reaches `gex.lookup` before any miss, which
/// catches up only when this says so.
pub(crate) fn behind() -> bool {
    (HAVE.with(|c| c.get()) as usize) < log_len()
}

/// [`behind`], for `gex.lookup`'s JS half.
#[no_mangle]
pub extern "C" fn gex_ftab_behind() -> i32 {
    behind() as i32
}

/// How many events the function table's log has.
#[no_mangle]
pub extern "C" fn gex_ftab_log_len() -> u32 {
    log_len() as u32
}

/// Event `i` of the log: an install as `1 << 32 | install`, a retirement as
/// `2 << 32 | index`; 0 past the end.
#[no_mangle]
pub extern "C" fn gex_ftab_event(i: u32) -> i64 {
    log_event(i as usize) as i64
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
    let stats = FCACHE_STATS.load(Ordering::Relaxed);
    if stats {
        fcache_stat(addr);
    }
    // Filler no function has (`STRAY_PC`): the fallback's.
    if addr == STRAY_PC {
        return gex_abi::ftab::FALLBACK as i32;
    }
    let mem = crate::guest_bytes();
    let hit = ftab(|m| {
        let f = m.get(addr)?;
        let stale = if m.install(f.install).tier == Tier::Jit {
            !crate::fjit::current(f.install)
        } else {
            let page = addr / gex_abi::DISPATCH_PAGE;
            let patched = mem.get((gex_abi::patched_pages() + page / 8) as usize).is_some_and(|b| b & (1 << (page % 8)) != 0);
            // A provider serves the export whatever its bytes became: a game's
            // mod sandbox may rewrite a DLL's entry points, which the Lua provider
            // replaces (as natively, ftab.rs `current`).
            patched && m.install(f.install).module < PROVIDER
        };
        if stale {
            if stats {
                fcache_why(W_STALE);
            }
            m.retire(addr);
            return None;
        }
        // Under the table's lock, as `on_retire` clears it: written after
        // the lock, a retirement on another Worker in between (its bit set,
        // the slot cleared) would be undone, and a Worker whose table still
        // holds the retired body would find itself in the slot when the body
        // sees its page patched and leaves through the cache -- calling
        // itself forever (explorer's uxtheme.dll DllMain, reloaded).
        if stats && f.idx >= ftab_safe_len() {
            fcache_why(W_REFUSED);
        }
        fcache_publish(mem, addr, f.idx);
        Some(f.idx)
    });
    // An ejected project's training run (`run.mjs --record-entries`): an
    // address of a unit image no function holds, reached through the
    // table (a return a nested dispatch rewrote, say), is a region entry
    // the web build needs as much as a tier-1 dispatch miss is.
    if hit.is_none() {
        note_web_entry(addr);
    }
    // Nothing serves it: the function JIT makes it, if it is on.
    let served = hit.is_some();
    let hit = hit.or_else(|| crate::fjit::translate(addr));
    let idx = hit.unwrap_or(gex_abi::ftab::FALLBACK);
    if stats {
        fcache_why(if served { W_SERVED } else if idx == gex_abi::ftab::FALLBACK { W_FALLBACK } else { W_JIT });
        if !served && idx >= ftab_safe_len() {
            fcache_why(W_REFUSED);
        }
    }
    if !served {
        fcache_publish(mem, addr, idx);
    }
    if libprov::any_registered() {
        LOOKUP_RING.with(|r| {
            let mut r = r.borrow_mut();
            if r.len() >= 16 {
                r.remove(0);
            }
            r.push((addr, idx));
        });
    }
    idx as i32
}

/// `?fcachestats=1` (GEX_FCACHE_STATS=1): the probe cache's misses (every
/// `gex.lookup` is one), said by each Worker at most every 10 s, with how
/// many were above 4 GiB and the addresses that missed most. A debugging
/// opt-in: nothing is counted with it off.
static FCACHE_STATS: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_set_fcache_stats(on: i32) {
    FCACHE_STATS.store(on != 0, Ordering::Relaxed);
}

// Why each miss happened, for the same line: at entry, the address's slot
// held another address (a collision) or none; then whether a function
// served it, the function JIT made one, or the fallback took it; stale
// entries retired; and indices the cache refused for a Worker's short table
// (`ftab_safe_len`: every call through such an index looks up again).
const W_COLLISION: usize = 0;
const W_EMPTY: usize = 1;
const W_SERVED: usize = 2;
const W_JIT: usize = 3;
const W_FALLBACK: usize = 4;
const W_STALE: usize = 5;
const W_REFUSED: usize = 6;
thread_local! {
    static FCACHE_WHY: std::cell::Cell<[u64; 7]> = const { std::cell::Cell::new([0; 7]) };
}

fn fcache_why(k: usize) {
    FCACHE_WHY.with(|c| {
        let mut v = c.get();
        v[k] += 1;
        c.set(v);
    });
}

thread_local! {
    /// This Worker's misses since its last line: (since ns, above 4 GiB, by address).
    static FCACHE_MISSES: std::cell::RefCell<(u64, u64, std::collections::HashMap<u64, u64>)> = Default::default();
}

fn fcache_stat(addr: u64) {
    FCACHE_MISSES.with(|m| {
        let mut m = m.borrow_mut();
        let now = crate::now_ns();
        if m.0 == 0 {
            m.0 = now;
        }
        m.1 += (addr >> 32 != 0) as u64;
        {
            let mem = crate::guest_bytes();
            let at = gex_abi::fcache_entry(addr) as usize;
            let e = mem.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
            let other = e >> gex_abi::FCACHE_TAG_BITS != gex_abi::ftab::RESOLVE as u64 && e & gex_abi::FCACHE_TAG_MASK != addr & gex_abi::FCACHE_TAG_MASK;
            fcache_why(if other { W_COLLISION } else { W_EMPTY });
        }
        *m.2.entry(addr).or_insert(0) += 1;
        let secs = now.saturating_sub(m.0) as f64 / 1e9;
        if secs < 10.0 {
            return;
        }
        let n: u64 = m.2.values().sum();
        let mut top: Vec<(u64, u64)> = m.2.iter().map(|(a, c)| (*c, *a)).collect();
        top.sort_unstable_by(|a, b| b.cmp(a));
        let top: Vec<String> = top.iter().take(5).map(|(c, a)| format!("{a:#x} {c}")).collect();
        let w = FCACHE_WHY.with(|c| c.replace([0; 7]));
        crate::warn_line(&format!(
            "[gex] fcache: pid {} tid {} {n} probe misses in {secs:.1} s ({:.0}/s), {} above 4 GiB, {} addresses; slot held another {} none {}; served {} jit {} fallback {} stale {} refused {} (safe {} size {}); most: {}\n",
            crate::gex_my_pid(),
            crate::my_tid(),
            n as f64 / secs,
            m.1,
            m.2.len(),
            w[W_COLLISION], w[W_EMPTY], w[W_SERVED], w[W_JIT], w[W_FALLBACK], w[W_STALE], w[W_REFUSED],
            ftab_safe_len(),
            FTAB_SIZE.load(Ordering::Relaxed),
            top.join(", ")
        ));
        *m = (now, 0, Default::default());
    });
}

/// The table's index 1: code no function body holds, under the `F_body`
/// contract (the call-out, docs/function-tier.md §7.3, as natively): tier-1
/// runs the target nested -- JavaScript's loop inside this call -- until the
/// guest returns to `ret` with the stack back (1, the predicted return), so
/// the frames that called here carry on in their bodies instead of leaving
/// for tier-1. With no caller waiting (`NO_RETURN`), too deep a nest, or a
/// run that cannot finish nested (the stack unwound past the caller), it
/// leaves: `ctx::PC` set, 0.
#[no_mangle]
pub extern "C" fn gex_ftab_fallback(ctx: i32, _depth: i32, _need: i32, ret: i64) -> i32 {
    LEAVES.fetch_add(1, Ordering::Relaxed);
    let mut gm = GuestMem::new(crate::guest_bytes());
    let c = ctx as u32 as u64;
    let t = gm.read_u64(c + gex_abi::ctx::LAST_TARGET as u64).unwrap_or(0);
    let _ = gm.write_u64(c + gex_abi::ctx::PC as u64, t);
    // A native caller's `run_until` after a stray return to its pad
    // (`native_return_stray`): the guest's registers as they were there,
    // and the call is done.
    if t == STRAY_PC && stray_restore(c) {
        return gex_abi::ftab::RET_PREDICTED;
    }
    // A jump out of an exception dispatched nested: back to the frames it
    // was raised under (nt::exc_escape), the dispatch's loop ends.
    if crate::nt::exc_escape(ctx, t) {
        crate::set_resume_none();
        crate::trap();
    }
    // An ejected project's run (`gex_set_aot`, docs/eject-pe.md §3.1): no
    // function body holds `t`. AOT-only (`gex_set_jit(0)`), nothing may
    // translate it either.
    if crate::aot() {
        // A branch to memory nothing executable is mapped at: the guest's
        // access violation, as natively (34c3e45b).
        let wild = !crate::sigs::executable(t);
        if wild {
            crate::nt::fault(ctx, gex_kernel::nt::Fault::NotExecutable, t);
        }
        // A window-procedure thunk made at run time (gex_kernel::win::thunk):
        // its store done here, then the call goes on to where it jumps.
        if crate::compat_abi() {
            if let Some(next) = gex_kernel::win::thunk::run(&mut gm, c, t) {
                let _ = gm.write_u64(c + gex_abi::ctx::LAST_TARGET as u64, next);
                let _ = gm.write_u64(c + gex_abi::ctx::PC as u64, next);
                if let Some(idx) = ftab(|m| m.get(next).map(|f| f.idx)) {
                    // SAFETY: a host import taking scalars; nothing here holds a lock.
                    return unsafe { ftab_call(idx as i32, ctx, _depth, _need, ret) };
                }
                return gex_ftab_fallback(ctx, _depth, _need, ret);
            }
        }
        // The first call into a W3 image left unplaced at its mapping
        // (`w3_miss`; native Wine calls a DLL's entry point straight after
        // mapping it): its unit is placed now and served below.
        let placed = w3_miss(t);
        // Published by another Worker and not yet installed here (or just
        // placed): catch up, then call it, as the lookup would have (leaving
        // would lose the frames that called here).
        if placed {
            crate::nt::serve_now();
        }
        if let Some(idx) = ftab(|m| m.get(t).map(|f| f.idx)) {
            crate::nt::serve_now();
            CAUGHT_UP.fetch_add(1, Ordering::Relaxed);
            // SAFETY: a host import taking scalars; nothing here holds a lock.
            return unsafe { ftab_call(idx as i32, ctx, _depth, _need, ret) };
        }
    }
    // The awaited return itself, reached with the stack moved (below): leave
    // before an AOT-only run would stop at it.
    if !crate::jit_on() && t == ret as u64 {
        return gex_abi::ftab::RET_LEFT;
    }
    // 64-bit native Wine's system and Unix calls (a call to its entry page):
    // its direct entry, here, in place of a call-out that runs the `ud2`.
    if let Some(r) = w64_entry(&gm, c, t, ret as u64) {
        return r;
    }
    if !crate::jit_on() {
        let path = lock(&UNITS)
            .iter()
            .filter(|x| x.base <= t)
            .max_by_key(|x| x.base)
            .map(|x| format!("{} at {:#x}, +{:#x}", x.path, x.base, t - x.base))
            .unwrap_or_else(|| "no image".into());
        // Who branched there, and the stack (i386: a call's return address
        // on top), to tell a lost return from an indirect branch.
        let from = gm.read_u64(c + gex_abi::ctx::LAST_SOURCE as u64).unwrap_or(0);
        let sp = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0) & 0xffff_ffff;
        let stack: Vec<String> = (0..8u64).map(|i| crate::nt::describe(gm.read_u32(sp + 4 * i).unwrap_or(0) as u64)).collect();
        crate::warn_line(&format!(
            "gex-eject: the guest reached {t:#x} ({path}), which no unit, region entry or provider holds, \
             and this project runs without translation: record a session that reaches it and re-eject \
             (from {}, ret {ret:#x}, sp {sp:#x} stack [{}])\n",
            crate::nt::describe(from),
            stack.join(" ")
        ));
        crate::nt::end_process(0xc000_001d);
    }
    // The awaited return itself, reached with the stack moved: a callee that
    // returns having changed `esp` (MSVC's `__chkstk`, `__SEH_prolog4`), whose
    // `ret` goes through the table. A call-out from there would wait for a
    // second return to the place it starts and never end. Leave instead: the
    // caller's return check sees its return address with the stack moved and
    // resumes there (`gex_resume`).
    if ret as u64 == gex_abi::ftab::NO_RETURN || t == ret as u64 || !CALLOUT_ON.load(Ordering::Relaxed) {
        return gex_abi::ftab::RET_LEFT;
    }
    if libprov::provider_return(ret as u64) {
        let sp = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0);
        PROV_RING.with(|r| {
            let mut r = r.borrow_mut();
            if r.len() >= 16 {
                r.remove(0);
            }
            r.push((t, ret as u64, sp));
        });
    }
    if PROV_TRACE.load(Ordering::Relaxed) && libprov::provider_return(ret as u64) {
        crate::warn_line(&format!("[gex] provider guest call to {t:#x} (call-outs open {})\n", CALLOUTS.with(|c| c.borrow().len())));
    }
    // A stateful provider's guest call (its return address in the provider's
    // code page) cannot be left: its frames would be lost. Those nest as deep
    // as the program's calls between guest and provider do (Unity: icall ->
    // runtime_invoke -> icall ...).
    let limit = if libprov::provider_return(ret as u64) { CALLOUT_MAX_DEPTH_PROVIDER } else { CALLOUT_MAX_DEPTH };
    if CALLOUTS.with(|c| c.borrow().len()) >= limit {
        if limit == CALLOUT_MAX_DEPTH_PROVIDER {
            crate::warn_line(&format!("gex: a provider's guest calls nest deeper than {limit}\n"));
        }
        return gex_abi::ftab::RET_LEFT;
    }
    // The stack once the guest has returned: the caller's, past the return
    // address (and whatever a stdcall callee pops, allowed for below).
    let sp = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0);
    let delta = if !gex_abi::X86 { 0 } else if gex_xlate::arch::compat() { 4 } else { 8 };
    let want = sp + delta;
    let span = callout_span(ret as u64, _need as u32);
    let cb = CB_DEPTH.with(|d| d.get());
    let depth = CALLOUTS.with(|c| {
        let mut c = c.borrow_mut();
        c.push((ret as u64, want, cb, span));
        c.len() - 1
    });
    OPENED_SPAN.with(|s| s.set(span as i64));
    OPENED_DEPTH.with(|s| s.set(depth as i32));
    CALLOUT_N.fetch_add(1, Ordering::Relaxed);
    // Opened (result -1): one that never closes was abandoned by a throw.
    CALLOUT_RING.with(|x| {
        let mut x = x.borrow_mut();
        if x.len() >= 24 {
            x.remove(0);
        }
        x.push((t, ret as u64, want, depth as u32, -1, sp));
    });
    cout_stat(t, ret as u64);
    // SAFETY: a host import taking scalars; nothing here holds a lock.
    let r = unsafe { call_out(ctx, t as i64, ret, want as i64) };
    CALLOUTS.with(|c| c.borrow_mut().truncate(depth));
    {
        let after = GuestMem::new(crate::guest_bytes()).read_u64(c + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0);
        CALLOUT_RING.with(|x| {
            let mut x = x.borrow_mut();
            if x.len() >= 24 {
                x.remove(0);
            }
            x.push((t, ret as u64, want, depth as u32, r, after));
        });
    }
    if r == gex_abi::ftab::RET_LEFT {
        CALLOUT_LEFT.fetch_add(1, Ordering::Relaxed);
        if libprov::provider_return(ret as u64) {
            let pc = GuestMem::new(crate::guest_bytes()).read_u64(c + gex_abi::ctx::PC as u64).unwrap_or(0);
            crate::warn_line(&format!("[gex] a provider's guest call-out to {t:#x} was left (the guest is at {pc:#x})\n"));
        }
    }
    r
}

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Run the guest from `target` in a nested loop until it returns to
    /// `ret` with the stack pointer at `sp_want` (or what a callee popped
    /// above it): 1; 0 when it cannot (`ctx::PC` says where the guest is).
    fn call_out(ctx: i32, target: i64, ret: i64, sp_want: i64) -> i32;
    /// Call function-table entry `idx` as a function body (AOT-only: one
    /// this Worker has just installed).
    fn ftab_call(idx: i32, ctx: i32, depth: i32, need: i32, ret: i64) -> i32;
    /// Call function-table entry `idx`, a `void(int)` (W3: ntdll.so's
    /// handler for a guest's Unix call, given the thread's context).
    pub(crate) fn ftab_call_vi(idx: i32, arg: i32);
    /// W64: call Wine's direct entry `idx` (a `void(int)`) for a unit's
    /// call to its system or Unix call entry, as the fault hook is called
    /// for the `ud2` there: 1; 0 when this build cannot (the `ud2` then).
    /// `sp` is the guest's stack pointer, as the call left it (the hook's
    /// C frames go below it), read here so the host need not.
    fn entry_call(idx: i32, ctx: i32, sp: i64) -> i32;
    /// Start the thread the call staged (`#startThread`), after
    /// `entry_call`: called only when one is staged.
    fn thread_start();
    /// Call the guest fault hook `idx` (`void(int ctx, unsigned code,
    /// address, unsigned kind)`) here, its C frames below the guest's stack
    /// pointer `sp`: 1; 0 when this build cannot (the fault is staged then).
    fn fault_call(idx: i32, ctx: i32, code: i32, addr: i64, kind: i32, sp: i64) -> i32;
}

/// AOT-only fallbacks that found their target published and called it.
pub(crate) static CAUGHT_UP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `?fjitlibs=coutstat=1`: say each call-out target once it has been called
/// out to 64 times on a Worker (and from where), and this Worker's totals at
/// every power of two from 1024: which code no unit holds is hot.
pub(crate) static COUT_STAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    static COUT_BY: std::cell::RefCell<std::collections::HashMap<u64, (u32, u64)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static COUT_N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn cout_stat(t: u64, ret: u64) {
    if !COUT_STAT.load(Ordering::Relaxed) {
        return;
    }
    let n = COUT_N.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    if n >= 1024 && n.is_power_of_two() {
        let distinct = COUT_BY.with(|m| m.borrow().len());
        crate::warn_line(&format!("[cout] pid {} {n} call-outs on this Worker, {distinct} targets\n", crate::gex_my_pid()));
    }
    let k = COUT_BY.with(|m| {
        let mut m = m.borrow_mut();
        let e = m.entry(t).or_insert((0, ret));
        e.0 += 1;
        e.0
    });
    if k == 64 {
        crate::warn_line(&format!(
            "[cout] pid {} hot target {t:#x} ({}), from ret {ret:#x} ({}), 64 call-outs; served by fjit {}\n",
            crate::gex_my_pid(),
            crate::nt::describe(t),
            crate::nt::describe(ret),
            ftab(|m| m.get(t).is_some()),
        ));
    }
}

/// `gex_set_callout(0)` turns the call-out off (the fallback always leaves,
/// as before it existed), for measuring.
static CALLOUT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[no_mangle]
pub extern "C" fn gex_set_callout(on: i32) {
    CALLOUT_ON.store(on != 0, Ordering::Relaxed);
    PROV_TRACE.store(on == 2, Ordering::Relaxed);
}

/// Call-outs nested deeper than this leave instead: each holds a shim frame
/// and a JS one.
const CALLOUT_MAX_DEPTH: usize = 32;
/// `gex_set_callout(2)`: say every stateful provider's guest call.
static PROV_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The same for a stateful provider's guest calls, which cannot leave.
const CALLOUT_MAX_DEPTH_PROVIDER: usize = 400;

/// How far above a call-out's `sp_want` its return may come back (what a
/// stdcall callee pops, or a function frame resumed inside), and past which
/// the stack has unwound beyond its caller: as natively.
pub(crate) const CALLOUT_UNWOUND: u64 = 64 << 10;

/// How far above `sp_want` the return of a call-out to `ret` made with
/// `need` may come back: for a guest call from native code (`ret` the
/// native return pad) that names its callee's argument bytes
/// (`ftab::need_args`), exactly those, so that a return to the pad for an
/// outer native caller is not taken as this one's; otherwise
/// [`CALLOUT_UNWOUND`].
fn callout_span(ret: u64, need: u32) -> u64 {
    match gex_abi::ftab::need_args(need) {
        Some(args) if ret == gex_kernel::win::tramp::NATIVE_RETURN_PAD => args,
        _ => CALLOUT_UNWOUND,
    }
}

/// The span of the call-out `gex_ftab_fallback` just opened, for JS's loop
/// that runs it (`gex_callout_span`); -1 once read, or when the call-out
/// came from elsewhere.
#[no_mangle]
pub extern "C" fn gex_callout_span() -> i64 {
    OPENED_SPAN.with(|s| s.replace(-1))
}

/// The index in [`CALLOUTS`] of the call-out `gex_ftab_fallback` just
/// opened, for JS's loop that runs it to speak of it in this list's terms
/// (`gex_callout_hit`, `gex_callout_leave_ok`); -1 once read, or when the
/// call-out came from elsewhere (it is in JS's list alone).
#[no_mangle]
pub extern "C" fn gex_callout_opened() -> i32 {
    OPENED_DEPTH.with(|s| s.replace(-1))
}

static CALLOUT_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CALLOUT_LEFT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// This Worker's call-outs in progress: `(ret, sp_want, NT callbacks in
    /// progress when it began, span)`, innermost last. The return is the
    /// call-out's with the stack from `sp_want` to `sp_want + span`
    /// (`callout_span`).
    static CALLOUTS: std::cell::RefCell<Vec<(u64, u64, u32, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// [`gex_callout_span`].
    static OPENED_SPAN: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
    /// [`gex_callout_opened`].
    static OPENED_DEPTH: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
    /// NT user-mode callbacks in progress on this Worker (nt.rs).
    static CB_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The call-out whose return a dispatch miss found (`returned`), or -1.
    static HIT: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
    /// 1 + the call-out whose loop called `enter` last with nothing nested
    /// since (`gex_callout_leave_ok`), or 0.
    static LEAVE_OK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// JS's call-out loop `depth` is about to call `enter` (`depth + 1`), or
/// another loop is (0): whether a miss at that call-out's return may leave
/// by returning (`DISPATCH_MISS_LEAVE`) instead of trapping.
#[no_mangle]
pub extern "C" fn gex_callout_leave_ok(v: u32) {
    LEAVE_OK.with(|c| c.set(v));
}

/// Whether JS's call-out loop `v - 1` is the last to have called `enter`
/// with no other loop since (`gex_callout_leave_ok(v)`): a watched return
/// of that call-out, reached with only tail calls since, may leave its
/// `enter` by returning instead of trapping (`#watchHit`), as a miss does
/// (`may_leave`).
#[no_mangle]
pub extern "C" fn gex_callout_may_return(v: u32) -> i32 {
    (v != 0 && LEAVE_OK.with(|c| c.get()) == v && !exp_trap_callouts()) as i32
}

/// After `returned`: the call-out found is the innermost and its loop's
/// `enter` is the innermost JS frame, so only tail calls lie between it and
/// the miss and a return reaches it.
pub(crate) fn may_leave() -> bool {
    let hit = HIT.with(|h| h.get());
    hit >= 0
        && CALLOUTS.with(|c| c.borrow().len()) == hit as usize + 1
        && LEAVE_OK.with(|c| c.get()) == hit as u32 + 1
        && !exp_trap_callouts()
}

/// `GEX_NT_CALLOUT_TRAP=1`: call-outs leave by trapping, as before (A/B).
fn exp_trap_callouts() -> bool {
    TRAP_CALLOUTS.load(Ordering::Relaxed)
}
pub(crate) static TRAP_CALLOUTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn sp_index() -> u8 {
    if gex_abi::X86 {
        4
    } else {
        31
    }
}

/// A dispatch missed at `pc` with the stack at `sp`: whether it is the
/// return a call-out of this Worker waits for (the caller then traps; JS's
/// nested loop ends that call-out). An untranslated return address is
/// caught here; a translated one by the watched table entry (JS).
pub(crate) fn returned(pc: u64, ctx: u64) -> bool {
    let sp = GuestMem::new(crate::guest_bytes()).read_u64(ctx + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0);
    // Only a call-out with no NT callback begun inside it (as `#watchHit`).
    let cb = CB_DEPTH.with(|d| d.get());
    let hit = CALLOUTS.with(|c| {
        let v = c.borrow();
        v.iter().rposition(|&(r, w, d, span)| d == cb && r == pc && sp >= w && sp <= w + span)
    });
    match hit {
        Some(i) => {
            HIT.with(|h| h.set(i as i32));
            true
        }
        None => false,
    }
}

thread_local! {
    /// The last function-table lookups of this Worker (address, index), in a provider's process.
    static LOOKUP_RING: std::cell::RefCell<Vec<(u64, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The last stateful-provider guest calls that reached the fallback: (target, return, sp).
    static PROV_RING: std::cell::RefCell<Vec<(u64, u64, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// This Worker's last finished call-outs: (target, return, sp wanted,
    /// depth, result, sp after), for the wild-branch diagnostic.
    static CALLOUT_RING: std::cell::RefCell<Vec<(u64, u64, u64, u32, i32, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A guest return to native Wine's return pad (gex-kernel's
/// `tramp::NATIVE_RETURN_PAD`, int3 filler) that no call-out of this Worker
/// waits for (`returned` said no), with call-outs open: the guest went on
/// past them to a native caller's return. That caller called a function
/// body directly (gexrt `__gexw_guest_call`), and the body is still inside
/// one of these call-outs: an exception the callee raised was handled in
/// the body's own function, whose code then ran on tier-1 and returned. The
/// call-outs leave (JS's nested loops, `#callOutSteps`), the bodies leave
/// with them, and the native caller's `run_until` finds the guest at its
/// return. Without call-outs open nothing is waiting there: it stays a
/// breakpoint.
///
/// The bodies leaving write back what they held in locals, which is stale
/// now: the guest's registers at the pad (the callee's EAX and EDX, the
/// callee-saved ones it put back) are kept here (`stray_keep`), and the
/// native caller's `run_until` is sent to [`STRAY_PC`], whose fallback puts
/// them back and ends there as the predicted return (`gex_ftab_fallback`).
///
/// A body between two call-outs that leaves sends tier-1 on at
/// [`STRAY_PC`] inside the outer one: that is the same stray, still on its
/// way to the native caller.
///
/// Keeps the registers (for a return to the pad) and says whether the
/// caller is to trap to JS's loop with the guest resuming at `pc`.
pub(crate) fn native_return_stray(pc: u64, ctx: u64) -> bool {
    if STRAY_TRAP.load(Ordering::Relaxed) || CALLOUTS.with(|c| c.borrow().is_empty()) {
        return false;
    }
    if pc == gex_kernel::win::tramp::NATIVE_RETURN_PAD {
        stray_keep(ctx);
        return true;
    }
    pc == STRAY_PC && STRAY.with(|s| s.borrow().is_some())
}

/// `?straytrap=1` (`gex_set_stray_trap`): a return to the pad past open
/// call-outs stays the "breakpoint" it was before [`native_return_stray`],
/// for a bisect that looks for the run that reaches it.
static STRAY_TRAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_set_stray_trap(on: i32) {
    STRAY_TRAP.store(on != 0, Ordering::Relaxed);
}

/// Where a native caller's `run_until` goes after a stray return to its pad
/// ([`native_return_stray`]): the pad's int3 filler, which no function or
/// block has. Its fallback restores the registers kept at the pad and
/// returns as the pad.
pub const STRAY_PC: u64 = gex_kernel::win::tramp::NATIVE_RETURN_PAD + 1;

/// The context's words kept at a stray return to the pad: the registers
/// (`ctx` from 0 to `ctx::PC`) and the flags word.
const STRAY_WORDS: usize = gex_abi::ctx::PC / 8;

thread_local! {
    /// [`native_return_stray`]'s kept registers, until the native caller's
    /// `run_until` reaches [`STRAY_PC`].
    static STRAY: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

/// Keep the guest's registers at a stray return to the pad (thread
/// context `ctx`).
fn stray_keep(ctx: u64) {
    let gm = GuestMem::new(crate::guest_bytes());
    let mut v: Vec<u64> = (0..STRAY_WORDS as u64).map(|i| gm.read_u64(ctx + 8 * i).unwrap_or(0)).collect();
    v.push(gm.read_u64(ctx + gex_abi::ctx::NZCV as u64).unwrap_or(0));
    STRAY.with(|s| *s.borrow_mut() = Some(v));
}

/// Whether a stray return to the pad was just kept (`native_return_stray`):
/// what tells JS's call-out loop that its resume at the pad is one (a resume
/// there for any other reason, a catch-up, misses again and comes here).
/// `take` 1: forget it as well (the call-out waiting for the pad itself
/// took the return).
#[no_mangle]
pub extern "C" fn gex_callout_stray(take: i32) -> i32 {
    STRAY.with(|s| {
        let mut s = s.borrow_mut();
        let had = s.is_some();
        if take != 0 {
            *s = None;
        }
        had as i32
    })
}

/// The kept registers back into `ctx` (a native caller's `run_until` at
/// [`STRAY_PC`]); whether there were any.
fn stray_restore(ctx: u64) -> bool {
    // (One thread per Worker: the context it was kept from is this one.)
    let Some(v) = STRAY.with(|s| s.borrow_mut().take()) else { return false };
    let mut gm = GuestMem::new(crate::guest_bytes());
    for (i, w) in v[..STRAY_WORDS].iter().enumerate() {
        let _ = gm.write_u64(ctx + 8 * i as u64, *w);
    }
    let _ = gm.write_u64(ctx + gex_abi::ctx::NZCV as u64, v[STRAY_WORDS]);
    let _ = gm.write_u64(ctx + gex_abi::ctx::PC as u64, gex_kernel::win::tramp::NATIVE_RETURN_PAD);
    true
}

/// [`CALLOUT_RING`], for a diagnostic.
pub(crate) fn callouts_recent() -> Vec<(u64, u64, u64, u32, i32, u64)> {
    CALLOUT_RING.with(|r| r.borrow().clone())
}

/// Whether a stateful provider's guest call is open on this Worker: the guest
/// code running now has the provider's wasm frames below it, which a trap to
/// JS's run loop would abandon.
pub(crate) fn provider_callout_open() -> bool {
    libprov::guest_call_open() || CALLOUTS.with(|c| c.borrow().iter().any(|&(r, _, _, _)| libprov::provider_return(r)))
}

/// [`LOOKUP_RING`], for a diagnostic.
pub(crate) fn lookups_recent() -> Vec<(u64, u32)> {
    LOOKUP_RING.with(|r| r.borrow().clone())
}

/// [`PROV_RING`], for a diagnostic.
pub(crate) fn provider_calls_recent() -> Vec<(u64, u64, u64)> {
    PROV_RING.with(|r| r.borrow().clone())
}

/// The call-outs this Worker has open, for a diagnostic: (return, sp wanted, callback depth).
pub(crate) fn callouts_open() -> Vec<(u64, u64, u32)> {
    CALLOUTS.with(|c| c.borrow().iter().map(|&(r, w, d, _)| (r, w, d)).collect())
}

/// A call-out at depth `depth` (and every deeper one) was left by an
/// exception (JS's loop unwound through its frames): its entries go.
#[no_mangle]
pub extern "C" fn gex_callout_unwind(depth: u32) {
    CALLOUTS.with(|c| c.borrow_mut().truncate(depth as usize));
}

/// NT user-mode callbacks in progress on this Worker.
pub(crate) fn callback_depth() -> u32 {
    CB_DEPTH.with(|d| d.get())
}

/// An NT user-mode callback begins (or ends) on this Worker.
pub(crate) fn callback(enter: bool) {
    CB_DEPTH.with(|d| d.set(if enter { d.get() + 1 } else { d.get().saturating_sub(1) }));
}

/// The call-out a miss found returned (its depth, 0 outermost), or -1;
/// reading it clears it.
#[no_mangle]
pub extern "C" fn gex_callout_hit() -> i32 {
    HIT.with(|h| h.replace(-1))
}

/// Call-outs made and left (for the stats line).
pub(crate) fn callouts() -> (u64, u64) {
    (CALLOUT_N.load(Ordering::Relaxed), CALLOUT_LEFT.load(Ordering::Relaxed))
}

/// How dispatch reaches `pc` in this Worker: `table << 32 | index`, the
/// table being a dispatch table or `gex_emit::dense::TABLE_INDEX`
/// (`blocks`); -1 when nothing is there yet (a miss). What a call-out
/// watches its return through.
#[no_mangle]
pub extern "C" fn gex_dispatch_ref(pc: i64) -> i64 {
    let pc = pc as u64;
    let page = gex_abi::dispatch_page(pc);
    if page >= gex_abi::dispatch_max_pages() {
        return -1;
    }
    let entry = GuestMem::new(crate::guest_bytes()).read_u32(gex_abi::dispatch_dir() + page * 4).unwrap_or(0);
    if entry == 0 {
        return -1;
    }
    let k = gex_abi::dispatch_entry_table(entry);
    if crate::dense::map(k).is_some() {
        let d = crate::dense::lookup(pc);
        return if d == 0 { -1 } else { ((gex_emit::dense::TABLE_INDEX as i64) << 32) | d as i64 };
    }
    ((k as i64) << 32) | gex_abi::dispatch_index(pc, entry) as i64
}

/// The publishing Worker has installed unit `u`: point the directory at the
/// pages it numbered (those still numbered so).
#[no_mangle]
pub extern "C" fn gex_cgen_installed(u: u32) {
    let dirs = lock(&UNITS).get(u as usize).map(|x| x.dirs.clone()).unwrap_or_default();
    write_dirs(dirs);
    // A Worker that lost the claim installs too, possibly before the
    // claimant's publish has set `dirs`: only after the publish is it done.
    if let Some(s) = U_STATE.get(u as usize) {
        if s.load(Ordering::Acquire) & U_PUBLISHED != 0 {
            s.fetch_or(U_INSTALLED, Ordering::AcqRel);
        }
    }
}

static LEAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The function table's runtime module (`gex_cgen::runtime`): where its
/// bytes are, and how long they are.
static RUNTIME: OnceLock<Vec<u8>> = OnceLock::new();

fn runtime() -> &'static Vec<u8> {
    RUNTIME.get_or_init(|| {
        let m = gex_cgen::runtime::module();
        let mut m = if !crate::dense::on() {
            m
        } else {
            dense_runtime(&m).unwrap_or_else(|| {
                crate::warn_line("gex-cgen: the function table's runtime module is not the shape expected; its bridge dispatches classically\n");
                m
            })
        };
        // Named, so a CPU profile tells it from tier-1 (`wasm://wasm/
        // gex-ftab-runtime-...`; tests/nt/aot-share.mjs).
        let name = b"gex-ftab-runtime";
        let mut sub = vec![name.len() as u8];
        sub.extend_from_slice(name);
        let mut sec = vec![4u8];
        sec.extend_from_slice(b"name");
        sec.push(0); // the module-name subsection
        put_leb(&mut sec, sub.len() as u32);
        sec.extend_from_slice(&sub);
        m.push(0);
        put_leb(&mut m, sec.len() as u32);
        m.extend_from_slice(&sec);
        m
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
    // `(ctx, arm) -> ()`, the type of `blocks`' entries, added after the
    // module's own types.
    let mut entry_ty = None;
    while p < m.len() {
        let id = m[p];
        p += 1;
        let len = leb(m, &mut p)? as usize;
        let body = m.get(p..p + len)?;
        p += len;
        let mut sec = Vec::new();
        match id {
            1 => {
                let mut q = 0;
                let n = leb(body, &mut q)?;
                entry_ty = Some(n);
                put_leb(&mut sec, n + 1);
                sec.extend_from_slice(&body[q..]);
                sec.extend_from_slice(&[0x60, 0x02, 0x7f, 0x7f, 0x00]);
            }
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
                        sec.extend_from_slice(&gex_emit::dense::dispatch_body(3, entry_ty?, 1, tables));
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
///
/// `gone` (within `ranges`): pages no longer mapped executable. A unit there
/// is dead whatever the bytes still hash to -- an unmapped object's bytes
/// stay until the next mapping overwrites them, and that mapping retires
/// nothing. Its entries leave the table now: otherwise the object that next
/// lands there (HK: libxcb where libdbus had been, both with units) clears
/// the bits for its own unit and every address it does not define runs the
/// old object's functions.
pub(crate) fn on_retire(ranges: &[(u64, u64)], gone: &[(u64, u64)]) {
    on_retire_pages(ranges, gone);
    libprov::forget_gone(gone);
    // W64 only: on W3 it hung a desktop shell's boot (explorer
    // went quiet after loading a DLL).
    if wasmprog::guest_x86_64() {
        wasmdll::forget_gone(gone);
    }
    // After the bits: a lookup once the probe cache is cleared sees them.
    // Every entry's probe cache slot first, the units' that retire here
    // included (as natively, ftab.rs `on_retire`): cleared after the
    // retirement, they were no longer in the map, and their slots kept
    // naming the retired body. A Worker that had not yet replayed the
    // retirement (its table still holding that body) then entered it from
    // the cache; the body found its page patched and left through the same
    // slot, to itself, forever: explorer's uxtheme.dll, unloaded and loaded
    // again at its base, hung its DllMain with the loader lock held.
    let memory = crate::guest_bytes();
    ftab(|m| {
        for &(start, end) in ranges {
            for (a, _) in m.range(start, end).collect::<Vec<_>>() {
                gex_kernel::ftab::fcache_clear(memory, a);
            }
        }
        for &(start, end) in gone {
            for (a, f) in m.range(start, end).collect::<Vec<_>>() {
                let i = m.install(f.install);
                if i.tier == Tier::Unit && i.module < PROVIDER {
                    m.retire(a);
                }
            }
        }
    });
}

/// `execve`: the old program's code is all gone, whatever reuses its
/// addresses next. Before exec reused the dead arena (`do_exec`), nothing new
/// could land there and stale entries were unreachable; now the old code
/// ranges are retired as an unmap retires them (every tier: a provider's
/// address in the old program is nothing in the new one), the providers' own
/// bookkeeping is cleared, and the replaced libraries' provider installs are
/// killed (`FuncMap::kill_module`) so a Worker started later does not
/// instantiate them.
pub(crate) fn on_exec(old_code: &[(u64, u64)]) {
    on_retire_pages(old_code, old_code);
    let dead = libprov::on_exec(old_code);
    elfprov::on_exec();
    let memory = crate::guest_bytes();
    ftab(|m| {
        for &(start, end) in old_code {
            for (a, _) in m.retire_range(start, end) {
                gex_kernel::ftab::fcache_clear(memory, a);
            }
        }
        for n in dead {
            m.kill_module(PROVIDER | n);
        }
    });
}

fn on_retire_pages(ranges: &[(u64, u64)], gone: &[(u64, u64)]) {
    let guard = lock(&UNIT_PAGES);
    let Some(pages) = guard.as_ref() else { return };
    let memory = crate::guest_bytes();
    for &(start, end) in ranges {
        for page in start / gex_abi::DISPATCH_PAGE..end.div_ceil(gex_abi::DISPATCH_PAGE) {
            let Some(&built) = pages.get(&page) else { continue };
            let at = page * gex_abi::DISPATCH_PAGE;
            let unmapped = gone.iter().any(|&(s, e)| s <= at && at < e);
            if unmapped || page_hash(memory, page) != built {
                let off = (gex_abi::patched_pages() + page / 8) as usize;
                if let Some(b) = memory.get_mut(off) {
                    *b |= 1 << (page % 8);
                }
            }
        }
    }
}

/// Wine's Unix-side entry points (W3.1, as the native
/// `crate::unix_dispatcher`), function-table indices of the process's
/// ntdll.so, each set by an even op and read by the odd one after it (0 for
/// none): 0/1 its system-call switch, 2/3 its handler for a guest's Unix
/// call (the trampoline page's `UNIX_CALL_32`), 4/5 `__wine_unix_call_
/// dispatcher` itself, which Wine's PE module calls directly. Per process:
/// statics of this instance, set again by a new program at its start.
static UNIX_DISPATCHERS: [std::sync::atomic::AtomicU32; 3] = [const { std::sync::atomic::AtomicU32::new(0) }; 3];

#[no_mangle]
pub extern "C" fn gex_unix_dispatcher(op: i32, idx: u32) -> u32 {
    let Some(slot) = UNIX_DISPATCHERS.get((op.max(0) / 2) as usize) else { return 0 };
    if op % 2 == 0 {
        slot.store(idx, Ordering::Relaxed);
        return 0;
    }
    slot.load(Ordering::Relaxed)
}

/// A call to 64-bit native Wine's system or Unix call entry
/// (`crate::W64_SYSCALL_ENTRY`, `W64_UNIX_CALL_ENTRY`) from code no
/// nested dispatch need run: Wine's direct entry for it
/// (`gex.unix_dispatcher` op 0 or 4) takes the registers as the call left
/// them, its return address at `[sp]`, and returns as the `ud2` route does,
/// PC at that address and the address popped. That is the predicted return;
/// a call that ends elsewhere (NtContinue, a raised exception, the thread's
/// end) leaves with PC where Wine put it. None, and the `ud2` route as
/// before, unless the target is one of the two, Wine has registered that
/// entry, the entry page holds Wine's pointer, and the call's return
/// address is `ret`.
fn w64_entry(gm: &GuestMem, c: u64, t: u64, ret: u64) -> Option<i32> {
    let slot = match t {
        crate::W64_SYSCALL_ENTRY => 0,
        crate::W64_UNIX_CALL_ENTRY => 2,
        _ => return None,
    };
    let idx = UNIX_DISPATCHERS[slot].load(Ordering::Relaxed);
    if idx == 0 || gm.read_u64(crate::W64_ENTRY_PAGE).ok()? != crate::W64_SYSCALL_ENTRY {
        return None;
    }
    let sp_at = c + gex_abi::ctx::x(sp_index()) as u64;
    let sp = gm.read_u64(sp_at).ok()?;
    if gm.read_u64(sp).ok()? != ret {
        return None;
    }
    // SAFETY: a host import taking scalars; nothing here holds a lock.
    if unsafe { entry_call(idx as i32, c as i32, sp as i64) } == 0 {
        return None;
    }
    // A thread the call made (NtCreateThreadEx's clone): started as the
    // fault hook's return would, before anything below may trap out.
    if crate::thread_staged() {
        // SAFETY: a host import without arguments; nothing here holds a lock.
        unsafe { thread_start() };
    }
    // Where the Unix side's own calls left this Worker to resume (a user
    // callback's loop, a wait): the call's return is in the context, as the
    // `ud2`'s fault hook leaves it, and a later trap must not resume there.
    // Another thread's context to resume with is the run loop's, as from
    // the hook: the resume point and context stay as the Unix side set them
    // and this thread traps out, as the `ud2`'s fault hook would leave
    // (from a unit's call as from translated code's).
    if crate::resume_ctx().is_some_and(|v| v != c) {
        crate::trap();
    }
    crate::set_resume_none();
    crate::set_resume_ctx_none();
    let gm = GuestMem::new(crate::guest_bytes());
    let pc = gm.read_u64(c + gex_abi::ctx::PC as u64).unwrap_or(0);
    let back = gm.read_u64(sp_at).unwrap_or(0);
    Some(if pc == ret && back == sp + 8 { gex_abi::ftab::RET_PREDICTED } else { gex_abi::ftab::RET_LEFT })
}

/// The `ud2` at 64-bit native Wine's system or Unix call entry reached by
/// translated code (a call from code no unit holds, opengl32's and win32u's
/// stubs among them): Wine's direct entry runs the call here, as
/// [`w64_entry`] does for a unit's, in place of a trap out to the fault
/// hook. True when it ran and the thread goes on at the context's PC (the
/// caller dispatches there); false, and the `ud2` route as before, when it
/// could not. A call that left another thread's context to resume with
/// traps out ([`w64_entry`]), the way the fault hook's return would leave.
pub(crate) fn w64_entry_illegal(c: u64, pc: u64) -> bool {
    let gm = GuestMem::new(crate::guest_bytes());
    let Ok(sp) = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64) else { return false };
    let Ok(ret) = gm.read_u64(sp) else { return false };
    w64_entry(&gm, c, pc, ret).is_some()
}

/// The process's handler for a guest's Unix call (W3), or 0.
pub(crate) fn guest_unix_call_hook() -> u32 {
    UNIX_DISPATCHERS[1].load(Ordering::Relaxed)
}

/// A native wasm32 program's guest fault hook (W3.1). The page delivers a
/// branch to non-executable memory to it (`gex_fault_take`), as natively.
static FAULT_HOOK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn gex_fault_hook_set(idx: u32) {
    FAULT_HOOK.store(idx, Ordering::Relaxed);
}

/// The fault hook's function-table index, or 0 (none).
pub(crate) fn fault_hook() -> u32 {
    FAULT_HOOK.load(Ordering::Relaxed)
}

thread_local! {
    /// A guest fault staged for the hook: (NTSTATUS code, address, kind:
    /// Windows' ExceptionInformation[0], 0 read, 1 write, 8 execute).
    static FAULT_PENDING: std::cell::Cell<Option<(u32, u64, u32)>> = const { std::cell::Cell::new(None) };
}

/// How many faults may be delivered in place one inside another's
/// dispatch on one thread (a handler that faults, its handler faulting
/// again): each holds the hook's and the dispatch's wasm frames. Past this
/// the fault is staged and the thread traps out, which drops them.
const FAULT_NEST_MAX: usize = 64;

thread_local! {
    /// The guest stack pointer of each in-place delivery whose dispatch may
    /// still be running on this thread, outermost first. One the guest
    /// stack has since come back above (its dispatch left, by a handler's
    /// jump, a longjmp or NtContinue) is over.
    static FAULT_NEST: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Deliver a guest fault to the hook in place, from code that goes on
/// after the import returns (lib.rs `hook_fault`): true when the hook ran
/// and the context's PC is where the thread goes on (the dispatcher). The
/// hook may also leave by unwinding (a native `__EXCEPT`'s longjmp) through
/// this frame, which holds nothing that needs dropping. False when it
/// cannot here; the caller stages the fault and traps.
pub(crate) fn fault_in_place(c: u64, code: u32, addr: u64, kind: u32) -> bool {
    let idx = fault_hook();
    if idx == 0 {
        return false;
    }
    let gm = GuestMem::new(crate::guest_bytes());
    let Ok(sp) = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64) else { return false };
    let deep = FAULT_NEST.with(|n| {
        let mut n = n.borrow_mut();
        while n.last().is_some_and(|&s| s < sp) {
            n.pop();
        }
        if n.len() >= FAULT_NEST_MAX {
            n.clear();
            return true;
        }
        n.push(sp);
        false
    });
    if deep {
        crate::warn_line(&format!(
            "gex: faults nested {FAULT_NEST_MAX} deep in their own dispatch (code {code:#010x}); this one is delivered from the run loop\n"
        ));
        return false;
    }
    // SAFETY: a host import taking scalars; nothing here holds a lock or a
    // borrow, since the hook's dispatch may unwind through this frame.
    unsafe { fault_call(idx as i32, c as i32, code as i32, addr as i64, kind as i32, sp as i64) != 0 }
}

/// Stage a fault for the host to give the hook (`gex_fault_take`); the
/// caller writes the faulting PC into the context and traps.
pub(crate) fn fault_stage(code: u32, addr: u64, kind: u32) {
    FAULT_PENDING.with(|f| f.set(Some((code, addr, kind))));
}

/// A longjmp or unwind that reached this Worker's outermost run loop
/// (gex.mjs): the frame that was to catch it is gone, so the exception it
/// carried cannot be handled where the program said. `jmp` is a longjmp's
/// jmp_buf (0 for another tag). When it is a native `__TRY`'s (Wine's
/// `__WINE_FRAME`, the jmp_buf 16 bytes in, the exception code after its
/// 64: a frame at the head of the thread's SEH chain, or just taken off it,
/// its `Prev` the head), the exception is
/// raised again with no handler left (the chain emptied), as an exception
/// no frame handles is on Windows: Wine ends the process with its code. 1,
/// and the run loop delivers the staged fault; 0 when it is not one
/// (the caller reports a crash). Said, as a critical error.
#[no_mangle]
pub extern "C" fn gex_handler_lost(ctx_addr: u32, jmp: u32) -> u32 {
    static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if fault_hook() == 0 || jmp < 16 || gex_abi::mem64() {
        return 0;
    }
    let c = ctx_addr as u64;
    let mut gm = GuestMem::new(crate::guest_bytes());
    let teb = gm.read_u64(c + gex_abi::ctx::FS_BASE as u64).unwrap_or(0) & 0xffff_ffff;
    let frame = jmp as u64 - 16;
    let (Ok(head), Ok(prev), Ok(code)) = (gm.read_u32(teb), gm.read_u32(frame), gm.read_u32(jmp as u64 + 64)) else { return 0 };
    if teb == 0 || code == 0 || (head as u64 != frame && head != prev) {
        return 0;
    }
    // A second loss on this Worker while the first is being raised: give up.
    if ONCE.swap(true, Ordering::Relaxed) {
        return 0;
    }
    let pc = gm.read_u64(c + gex_abi::ctx::PC as u64).unwrap_or(0);
    crate::warn_line(&format!(
        "gex: exception {code:#010x}: the native handler frame that caught it is gone (pc {pc:#x}); raised again with no handler\n"
    ));
    let _ = gm.write_u32(teb, 0xffff_ffff);
    fault_stage(code, pc, 0);
    crate::set_resume(pc);
    crate::set_resume_ctx(c);
    1
}

/// The staged fault, taken: the hook's table index (0: none staged), with
/// its code, address and kind at `out` (u32, u64, u32, packed little-endian
/// as 4 + 8 + 4 bytes).
#[no_mangle]
pub extern "C" fn gex_fault_take(out: u32) -> u32 {
    let Some((code, addr, kind)) = FAULT_PENDING.with(|f| f.take()) else { return 0 };
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&code.to_le_bytes());
    b[4..12].copy_from_slice(&addr.to_le_bytes());
    b[12..].copy_from_slice(&kind.to_le_bytes());
    // SAFETY: `out` is 16 bytes the host allocated in this module's memory.
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), std::ptr::with_exposed_provenance_mut::<u8>(out as usize), 16) };
    fault_hook()
}

/// A native wasm32 program's signal hook (W3.1). Recorded, as the fault
/// hook is; the page does not run signal frames through it yet (W3.7).
static SIGNAL_HOOK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn gex_signal_hook_set(idx: u32) {
    SIGNAL_HOOK.store(idx, Ordering::Relaxed);
}

/// The counting flavor's tally (`gex_cgen::image::icall_count`): every
/// call through the function table, by (return address, target). An
/// open-addressed table of `ICALL_SLOTS`, claimed by CAS on a tag mixed from
/// both, so a note takes no lock; a full table drops new pairs (counted).
const ICALL_SLOTS: usize = 1 << 16;
#[allow(clippy::declare_interior_mutable_const)]
const ICALL_ZERO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ICALL_TAG: [std::sync::atomic::AtomicU64; ICALL_SLOTS] = [ICALL_ZERO; ICALL_SLOTS];
static ICALL_RET: [std::sync::atomic::AtomicU64; ICALL_SLOTS] = [ICALL_ZERO; ICALL_SLOTS];
static ICALL_TARGET: [std::sync::atomic::AtomicU64; ICALL_SLOTS] = [ICALL_ZERO; ICALL_SLOTS];
static ICALL_N: [std::sync::atomic::AtomicU64; ICALL_SLOTS] = [ICALL_ZERO; ICALL_SLOTS];
static ICALL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ICALL_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// When the tally is next printed (ns of `now_ns`; 0 before the first note).
static ICALL_NEXT_DUMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Every this many seconds of a counting run, the whole tally so far.
const ICALL_DUMP_SECS: u64 = 150;

/// A table call of a counting unit (`gex_abi::imports::ICALL_NOTE`).
#[no_mangle]
pub extern "C" fn gex_icall_note(_ctx: i32, ret: i64, target: i64) {
    let (ret, target) = (ret as u64, target as u64);
    let tag = (ret.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ target.rotate_left(29).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)) | 1;
    let mut i = (tag >> 20) as usize & (ICALL_SLOTS - 1);
    let mut noted = false;
    for _ in 0..64 {
        let mut t = ICALL_TAG[i].load(Ordering::Relaxed);
        if t == 0 {
            match ICALL_TAG[i].compare_exchange(0, tag, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    ICALL_RET[i].store(ret, Ordering::Relaxed);
                    ICALL_TARGET[i].store(target, Ordering::Release);
                    t = tag;
                }
                Err(now) => t = now,
            }
        }
        if t == tag {
            ICALL_N[i].fetch_add(1, Ordering::Relaxed);
            noted = true;
            break;
        }
        i = (i + 1) & (ICALL_SLOTS - 1);
    }
    if !noted {
        ICALL_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    let calls = ICALL_CALLS.fetch_add(1, Ordering::Relaxed);
    if calls & 0xfffff == 0 {
        let now = crate::now_ns();
        let next = ICALL_NEXT_DUMP.load(Ordering::Relaxed);
        if next == 0 {
            let _ = ICALL_NEXT_DUMP.compare_exchange(0, now + ICALL_DUMP_SECS * 1_000_000_000, Ordering::Relaxed, Ordering::Relaxed);
        } else if now >= next
            && ICALL_NEXT_DUMP.compare_exchange(next, now + ICALL_DUMP_SECS * 1_000_000_000, Ordering::Relaxed, Ordering::Relaxed).is_ok()
        {
            gex_icall_dump();
        }
    }
}

/// Print the tally as an indirect-call profile (`icall RET TARGET N`, the
/// lines `pe_units::icall_hints` reads), between `icall-dump` markers: the
/// last complete dump of a run is its profile.
#[no_mangle]
pub extern "C" fn gex_icall_dump() {
    let pid = crate::gex_my_pid();
    let mut lines = Vec::new();
    for i in 0..ICALL_SLOTS {
        if ICALL_TAG[i].load(Ordering::Acquire) == 0 {
            continue;
        }
        let t = ICALL_TARGET[i].load(Ordering::Acquire);
        let c = ICALL_N[i].load(Ordering::Relaxed);
        if c != 0 {
            lines.push(format!("icall {:#x} {t:#x} {c}\n", ICALL_RET[i].load(Ordering::Relaxed)));
        }
    }
    crate::warn_line(&format!(
        "icall-dump begin pid {pid}: {} pairs, {} calls, {} dropped\n",
        lines.len(),
        ICALL_CALLS.load(Ordering::Relaxed),
        ICALL_DROPPED.load(Ordering::Relaxed)
    ));
    for l in &lines {
        crate::warn_line(l);
    }
    crate::warn_line(&format!("icall-dump end pid {pid}\n"));
}
