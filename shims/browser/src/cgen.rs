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
}

/// Provider modules published, by number (the module id an install names
/// is `PROVIDER | number`): the file JS loads, the bridges every Worker
/// writes and the pages the publisher numbered (as a unit's).
struct ProviderModule {
    file: String,
    regs: Vec<u32>,
    dirs: Vec<(u64, u32)>,
    slots: u64,
}

static PROVIDERS: Mutex<Vec<ProviderModule>> = Mutex::new(Vec::new());

/// Module ids at and above this are providers' (`PROVIDERS`), below
/// it units (`UNITS`).
const PROVIDER: u32 = 1 << 31;

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
    !lock(&WANTED).is_empty()
}

/// Want the unit for `image`, mapped at `base`: its key, and the check that
/// the mapped code is what the unit was built from (a mismatch ends the run,
/// as natively: a unit over different code is wrong code). `optional`: a
/// missing unit is fine even under `--cgen` (Wine's DLLs, the trampolines).
fn place(image: &gex_ld::Image, path: &str, base: u64, optional: bool, pe: Option<&gex_ld::pe::PeImage>) {
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
    let (key, providers) = unit_key(image, pe, base);
    let identity = gex_key::image_identity(image).trim_start_matches("image:").to_string();
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
    lock(&WANTED).push_back(units.len() - 1);
}

/// The key of the unit for `image` (the native host's `nt_cgen::unit_key`:
/// the image's key, extended by its export providers' stubs and sources),
/// and those providers. The indirect-call profile's part is added when JS
/// has read `<cache>/icall/<identity>.txt` (`gex_cgen_set_icall`).
fn unit_key(image: &gex_ld::Image, pe: Option<&gex_ld::pe::PeImage>, base: u64) -> (String, Vec<Provided>) {
    use std::hash::{Hash, Hasher};
    let mut key = gex_cgen::image::image_key(image);
    let Some(pe) = pe else { return (key, Vec::new()) };
    let providers = gex_cgen::provider::for_dll(&pe.exports.dll_name, !pe.is64);
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
            binds: p.binds.iter().filter_map(|(f, e)| exports.get(e).map(|&rva| (f.clone(), base + rva as u64))).collect(),
        })
        .filter(|p| !p.binds.is_empty())
        .collect();
    (key, provided)
}

/// Where unit `u`'s indirect-call profile is in the unit cache
/// (`icall/<identity>.txt`), for JS to read.
#[no_mangle]
pub extern "C" fn gex_cgen_icall_path(u: u32, buf: u32, cap: u32) -> u32 {
    let units = lock(&UNITS);
    units.get(u as usize).map_or(0, |x| put_str(&format!("icall/{}.txt", x.identity), buf, cap))
}

/// Unit `u`'s indirect-call profile (the `len` bytes at `ptr`, or none):
/// its key gains the profile's part, exactly as natively (`nt_cgen::
/// icall_hints` and `unit_key`).
#[no_mangle]
pub extern "C" fn gex_cgen_set_icall(u: u32, ptr: u32, len: u32) {
    use std::hash::{Hash, Hasher};
    let text = if len == 0 { String::new() } else { String::from_utf8_lossy(crate::shim_slice(ptr, len)).into_owned() };
    let mut units = lock(&UNITS);
    let Some(x) = units.get_mut(u as usize) else { return };
    let hints = icall_hints(&text);
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

/// `nt_cgen::icall_hints` on the profile's text: a call site whose dominant
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
    lock(&WANTED).front().map_or(-1, |&u| u as i32)
}

/// `v` into a buffer of the shim's heap (above the guest's limit).
fn put_u32s(v: &[u32], buf: u32) {
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
    // Loudly, as natively: a unit built at another base (or under another
    // key) is silently not used otherwise.
    crate::warn_line(&format!(
        "gex-nt: image {} at base {:#x}: no function-tier unit (key {}), tier-1; build it with \
         GEX_CGEN_GUARD=1 gexc cgen-image <image> --pe-base {:#x}\n",
        x.path, x.base, x.key, x.base
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
    let raw = crate::shim_slice(entries, 8 * n);
    let slots: Vec<u64> = raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    let addrs: Vec<u64> = slots.iter().copied().filter(|&a| a != 0).collect();
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

/// The bridges that make `addrs` reachable from tier-1: each address's
/// dispatch slot (numbering its page if it has none) as `[table, slot]`,
/// flattened for JS (a dense slot gets an index of its own in `blocks`,
/// `gex_emit::dense::TABLE_INDEX`), the pages it numbered, and what the
/// dispatch table must hold.
fn bridges(addrs: &[u64]) -> (Vec<u32>, Vec<(u64, u32)>, u64) {
    crate::with_host(|h| {
        let mut seen: std::collections::BTreeSet<(u32, u32)> = Default::default();
        let mut new_pages = Vec::new();
        for &a in addrs {
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
    let (file, binds) = {
        let units = lock(&UNITS);
        let Some(p) = units.get(u as usize).and_then(|x| x.providers.get(i as usize)) else { return -1 };
        (p.file.clone(), p.binds.clone())
    };
    let wasm = crate::shim_slice(ptr, len);
    let names = match gex_cgen::provider::slot_names(wasm) {
        Ok(n) => n,
        Err(e) => {
            crate::warn_line(&format!("gex-cgen: provider {file}: {e}\n"));
            return -1;
        }
    };
    let pairs: Vec<(u64, u32)> = binds
        .iter()
        .filter_map(|(f, a)| names.iter().position(|s| s.as_deref() == Some(f.as_str())).map(|k| (*a, k as u32)))
        .collect();
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
    crate::warn_line(&format!("gex-nt: provider {file}: {} exports\n", pairs.len()));
    lock(&PROVIDERS).push(ProviderModule { file, regs, dirs, slots });
    number as i32
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

fn write_dirs(dirs: Vec<(u64, u32)>) {
    crate::with_host(|h| {
        let mut gm = GuestMem::new(crate::guest_bytes());
        for (page, base) in dirs {
            if h.jit.page_table.lookup(page) == Some(base) {
                crate::dense::note_entry(base);
                let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
            }
        }
    });
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
    if ret as u64 == gex_abi::ftab::NO_RETURN || !CALLOUT_ON.load(Ordering::Relaxed) {
        return gex_abi::ftab::RET_LEFT;
    }
    if CALLOUTS.with(|c| c.borrow().len()) >= CALLOUT_MAX_DEPTH {
        return gex_abi::ftab::RET_LEFT;
    }
    // The stack once the guest has returned: the caller's, past the return
    // address (and whatever a stdcall callee pops, allowed for below).
    let sp = gm.read_u64(c + gex_abi::ctx::x(sp_index()) as u64).unwrap_or(0);
    let delta = if !gex_abi::X86 { 0 } else if gex_xlate::arch::compat() { 4 } else { 8 };
    let want = sp + delta;
    let cb = CB_DEPTH.with(|d| d.get());
    let depth = CALLOUTS.with(|c| {
        let mut c = c.borrow_mut();
        c.push((ret as u64, want, cb));
        c.len() - 1
    });
    CALLOUT_N.fetch_add(1, Ordering::Relaxed);
    // SAFETY: a host import taking scalars; nothing here holds a lock.
    let r = unsafe { call_out(ctx, t as i64, ret, want as i64) };
    CALLOUTS.with(|c| c.borrow_mut().truncate(depth));
    if r == gex_abi::ftab::RET_LEFT {
        CALLOUT_LEFT.fetch_add(1, Ordering::Relaxed);
    }
    r
}

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Run the guest from `target` in a nested loop until it returns to
    /// `ret` with the stack pointer at `sp_want` (or what a callee popped
    /// above it): 1; 0 when it cannot (`ctx::PC` says where the guest is).
    fn call_out(ctx: i32, target: i64, ret: i64, sp_want: i64) -> i32;
}

/// `gex_set_callout(0)` turns the call-out off (the fallback always leaves,
/// as before it existed), for measuring.
static CALLOUT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[no_mangle]
pub extern "C" fn gex_set_callout(on: i32) {
    CALLOUT_ON.store(on != 0, Ordering::Relaxed);
}

/// Call-outs nested deeper than this leave instead: each holds a shim frame
/// and a JS one.
const CALLOUT_MAX_DEPTH: usize = 32;

/// How far above a call-out's `sp_want` its return may come back (what a
/// stdcall callee pops, or a function frame resumed inside), and past which
/// the stack has unwound beyond its caller: as natively.
pub(crate) const CALLOUT_UNWOUND: u64 = 64 << 10;

static CALLOUT_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CALLOUT_LEFT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// This Worker's call-outs in progress: `(ret, sp_want, NT callbacks in
    /// progress when it began)`, innermost last.
    static CALLOUTS: std::cell::RefCell<Vec<(u64, u64, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// NT user-mode callbacks in progress on this Worker (nt.rs).
    static CB_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The call-out whose return a dispatch miss found (`returned`), or -1.
    static HIT: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
}

fn sp_index() -> u8 {
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
        v.iter().rposition(|&(r, w, d)| d == cb && r == pc && sp >= w && sp <= w + CALLOUT_UNWOUND)
    });
    match hit {
        Some(i) => {
            HIT.with(|h| h.set(i as i32));
            true
        }
        None => false,
    }
}

/// A call-out at depth `depth` (and every deeper one) was left by an
/// exception (JS's loop unwound through its frames): its entries go.
#[no_mangle]
pub extern "C" fn gex_callout_unwind(depth: u32) {
    CALLOUTS.with(|c| c.borrow_mut().truncate(depth as usize));
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
    if page >= gex_abi::DISPATCH_MAX_PAGES {
        return -1;
    }
    let entry = GuestMem::new(crate::guest_bytes()).read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(0);
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
