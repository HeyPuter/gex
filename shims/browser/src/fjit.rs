//! The function JIT in the browser (`gex_xlate::fjit`, docs/merge-plan.md
//! §9.8 stream F): the browser half of `gexc run --fjit`, as the native
//! `ftab::lookup`/`serve` do it.
//!
//! * A `gex.lookup` miss (cgen.rs) translates the function at the address on
//!   this Worker, inside the shim, and publishes the module into the function
//!   table under module id `FJIT | n` with the page values it was made under;
//!   every Worker instantiates it as it replays the table's log (gex.mjs
//!   `#cgenInstall`), the bytes read from here.
//! * A tier-1 dispatch miss asks first (`serve`): an address a function
//!   serves is reached from tier-1 through the bridge in its slot, as units'.
//! * A function whose pages changed (their `PAGE_GEN` words) is retired at its
//!   next lookup and translated again.
//!
//! Off unless the page asks (`gex_set_fjit`). Natively the translation can
//! run on worker threads (`GEX_FJIT_BACKGROUND`); here it is always inline.

use gex_kernel::GuestMem;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Module ids at and above this are the JIT's (`MODULES`).
pub(crate) const FJIT: u32 = 1 << 30;

static ON: AtomicBool = AtomicBool::new(false);
/// `gex_set_fjit(2)`: each translation on stderr.
static TRACE: AtomicBool = AtomicBool::new(false);
/// At most this many translations, the rest declined (bisecting a bad one).
static LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
/// Translate only the function at this address (0: any).
static ONLY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Debugging: translate at most `n` functions, or (`only` != 0) just the one at `only`.
#[no_mangle]
pub extern "C" fn gex_fjit_limit(n: u32, only: i64) {
    LIMIT.store(n, Ordering::Relaxed);
    ONLY.store(only as u64, Ordering::Relaxed);
}

fn trace(s: &str) {
    if TRACE.load(Ordering::Relaxed) {
        crate::stderr_line(&format!("[fjit] {s}\n"));
    }
}
static JIT: OnceLock<gex_xlate::fjit::Jit> = OnceLock::new();

/// What translating has cost this process: shim time in `translate_root`
/// (recovery and emission, declined ones included) and the wasm it made.
static TRANSLATE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WASM_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Milliseconds this process has spent translating (`TRANSLATE_NS`).
#[no_mangle]
pub extern "C" fn gex_fjit_translate_ms() -> u32 {
    (TRANSLATE_NS.load(Ordering::Relaxed) / 1_000_000) as u32
}

/// KiB of wasm the JIT has made in this process, and how many modules.
#[no_mangle]
pub extern "C" fn gex_fjit_wasm_kib() -> u32 {
    (WASM_BYTES.load(Ordering::Relaxed) / 1024) as u32
}

#[no_mangle]
pub extern "C" fn gex_fjit_modules() -> u32 {
    lock(&MODULES).len() as u32
}

/// One translation: its bytes (named `gex-fjit` for profiles), the bridges
/// every Worker writes (cgen.rs `bridges`), the pages it numbered and what
/// the dispatch tables must hold.
pub(crate) struct Module {
    pub wasm: Vec<u8>,
    pub regs: Vec<u32>,
    pub dirs: Vec<(u64, u32)>,
    pub slots: u64,
}

pub(crate) static MODULES: Mutex<Vec<Module>> = Mutex::new(Vec::new());

/// Each JIT install's page checks: `(page, PAGE_GEN value)` it was made under.
static CHECKS: Mutex<Option<HashMap<u32, Vec<(u64, u32)>>>> = Mutex::new(None);

/// Addresses the bridge handed back to tier-1 (`cache_op::FTAB_TIER1`).
static TIER1_ONLY: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

static TRANSLATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Translations, functions and wasm bytes by object (`?fjitlibs=byobj=1`
/// prints them every 256 translations): what the JIT spends start-up on.
static BY_OBJECT: Mutex<Option<std::collections::BTreeMap<String, (u64, u64, u64)>>> = Mutex::new(None);
static BY_OBJECT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `?fjitlibs=dumpmod=ADDR`: print (base64, in 3,000-character lines: the page console cuts
/// a line at about 4,000) every JIT module
/// whose functions include guest address ADDR, with its root, to rebuild the
/// page's translation offline (scratch/unity/fjitrepro).
static DUMP_MOD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static DUMP_OBJ: Mutex<Option<(String, u64)>> = Mutex::new(None);
/// `?fjitlibs=scanconst=ADDR`: say which translations mention guest address
/// ADDR as an `i64.const` (a call or jump to it, or a pc it stores), and
/// every translation asked for at ADDR: who made a mid-function address a
/// function start.
static SCAN_CONST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scan_const(root: u64, funcs: &[u64], wasm: &[u8]) {
    let want = SCAN_CONST.load(Ordering::Relaxed);
    if want == 0 {
        return;
    }
    // `i64.const` (0x42) and its signed LEB128.
    let mut pat = vec![0x42u8];
    let mut v = want as i64;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        pat.push(if done { b } else { b | 0x80 });
        if done {
            break;
        }
    }
    let hits = wasm.windows(pat.len()).filter(|w| *w == &pat[..]).count();
    if hits > 0 {
        let list: Vec<String> = funcs.iter().take(24).map(|a| format!("{a:x}")).collect();
        crate::warn_line(&format!(
            "[fjitscan] {want:#x} x{hits} in the module of root {root:#x} ({}), {} functions: {}\n",
            crate::object_off_at(root),
            funcs.len(),
            list.join(",")
        ));
    }
}

fn dump_module(root: u64, funcs: &[u64], wasm: &[u8]) {
    let mut want = DUMP_MOD.load(Ordering::Relaxed);
    if want == 0 {
        let obj = lock(&DUMP_OBJ).clone();
        if let Some((name, off)) = obj {
            if let Some(&a) = funcs.iter().find(|&&a| crate::object_file_off(a) == (name.clone(), off)) {
                DUMP_MOD.store(a, Ordering::Relaxed);
                *lock(&DUMP_OBJ) = None;
                crate::warn_line(&format!("[fjitdump] {name}@{off:x} is {a:#x}\n"));
                want = a;
            }
        }
    }
    if want == 0 || !funcs.contains(&want) {
        return;
    }
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b = String::with_capacity(wasm.len() * 4 / 3 + 4);
    for c in wasm.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                b.push(T[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                b.push('=');
            }
        }
    }
    const W: usize = 3000;
    let lines = b.len().div_ceil(W);
    crate::warn_line(&format!("[fjitdump] root {root:#x} funcs {} bytes {} lines {lines}\n", funcs.len(), wasm.len()));
    for (i, fs) in funcs.chunks(32).enumerate() {
        let list: Vec<String> = fs.iter().map(|a| format!("{a:x}")).collect();
        crate::warn_line(&format!("[fjitdump] {root:x} funcs {i}: {}\n", list.join(",")));
    }
    for (i, ch) in b.as_bytes().chunks(W).enumerate() {
        crate::warn_line(&format!("[fjitdump] {root:x} {i} {}\n", std::str::from_utf8(ch).unwrap_or("")));
    }
}

fn note_object(addr: u64, funcs: usize, bytes: usize) {
    if !BY_OBJECT_ON.load(Ordering::Relaxed) {
        return;
    }
    let (name, _) = crate::object_file_off(addr);
    let mut g = lock(&BY_OBJECT);
    let m = g.get_or_insert_with(Default::default);
    let e = m.entry(name).or_default();
    e.0 += 1;
    e.1 += funcs as u64;
    e.2 += bytes as u64;
    let total: u64 = m.values().map(|e| e.0).sum();
    if total % 256 == 0 {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1 .2.cmp(&a.1 .2));
        let list: Vec<String> = v.iter().take(12).map(|(n, e)| format!("{n} {}/{}/{}K", e.0, e.1, e.2 / 1024)).collect();
        crate::warn_line(&format!("[fjit] {total} translations by object (translations/functions/wasm): {}\n", list.join(", ")));
    }
}
static DECLINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The function JIT on (1) or off, before the guest runs.
#[no_mangle]
pub extern "C" fn gex_set_fjit(on: i32) {
    ON.store(on != 0, Ordering::Relaxed);
    TRACE.store(on == 2, Ordering::Relaxed);
    if on != 0 {
        let _ = JIT.get_or_init(|| gex_xlate::fjit::Jit::new(Default::default()));
    }
}

#[no_mangle]
pub extern "C" fn gex_fjit_on() -> i32 {
    on() as i32
}

/// Translations made and declined (for the stats line).
pub(crate) fn counts() -> (u64, u64) {
    (TRANSLATED.load(Ordering::Relaxed), DECLINED.load(Ordering::Relaxed))
}

/// The bridge handed `addr` back to tier-1: serve it no more.
pub(crate) fn tier1_only(addr: u64) {
    lock(&TIER1_ONLY).get_or_insert_with(Default::default).insert(addr);
}

fn is_tier1_only(addr: u64) -> bool {
    lock(&TIER1_ONLY).as_ref().is_some_and(|s| s.contains(&addr))
}

/// Whether the JIT function of install `install` still matches its pages.
pub(crate) fn current(install: u32) -> bool {
    let mem = crate::guest_bytes();
    let word = |off: u64| mem.get(off as usize..off as usize + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
    lock(&CHECKS)
        .as_ref()
        .and_then(|c| c.get(&install))
        .is_none_or(|c| c.iter().all(|&(p, g)| word(gex_abi::PAGE_GEN + 4 * p) == g))
}

/// The `PAGE_GEN` value of `page`, numbering it (its directory written when
/// the translation is installed: `fresh`) if it has no translation yet.
fn page_gen(page: u64, fresh: &mut Vec<(u64, u32)>) -> u32 {
    if page >= gex_abi::DISPATCH_MAX_PAGES {
        return u32::MAX;
    }
    let have = GuestMem::new(crate::guest_bytes()).read_u32(gex_abi::PAGE_GEN + 4 * page).unwrap_or(0);
    if have != 0 {
        return have;
    }
    crate::with_jit_only(|j| match j.page_table.lookup(page) {
        Some(b) => b,
        None => {
            let b = j.page_table.base(page);
            fresh.push((page, b));
            b
        }
    })
}

/// Each object's `.eh_frame` function ranges (unbiased, sorted by start)
/// and `PT_LOAD` headers (file offset, vaddr, file size), by file path;
/// empty when it has none or cannot be read.
type ObjFdes = (Vec<(u64, u64)>, Vec<(u64, u64, u64)>);
static FDES: Mutex<Option<std::collections::HashMap<String, std::sync::Arc<ObjFdes>>>> = Mutex::new(None);
static MID_DECLINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An ELF's `PT_LOAD` headers: (file offset, vaddr, file size).
fn loads(elf: &[u8]) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    let Some(&class) = elf.get(4) else { return out };
    let wide = class == 2;
    let rd = |o: usize, n: usize| -> Option<u64> {
        let b = elf.get(o..o + n)?;
        Some(b.iter().rev().fold(0u64, |a, &x| a << 8 | x as u64))
    };
    let hdr = if wide { (rd(0x20, 8), rd(0x36, 2), rd(0x38, 2)) } else { (rd(0x1c, 4), rd(0x2a, 2), rd(0x2c, 2)) };
    let (Some(phoff), Some(phentsize), Some(phnum)) = hdr else { return out };
    for i in 0..phnum {
        let ph = (phoff + i * phentsize) as usize;
        if rd(ph, 4) != Some(1) {
            continue;
        }
        let f = if wide { (rd(ph + 8, 8), rd(ph + 16, 8), rd(ph + 32, 8)) } else { (rd(ph + 4, 4), rd(ph + 8, 4), rd(ph + 16, 4)) };
        if let (Some(o), Some(v), Some(n)) = f {
            out.push((o, v, n));
        }
    }
    out
}

/// Whether `addr` lies strictly inside a function of its object, by the
/// object's `.eh_frame`: a miss there is a resume into the middle of a
/// function (a signal's return, a block boundary of the fallback's), not an
/// entry. Translated as a root, `recover_one` records it as a function start
/// for good, and every later translation of the function around it ends at
/// it as at a tail call, with the flags dead across it: libxcb's
/// `_xcb_in_read_block` lost its `cmp` before the `cmovle` at +0x30 and took
/// `min(queue_len, len)` from stale flags (hp8, hu16: memmove of -8 bytes),
/// and SDL's X11 connect failed the same way (hb26, hb45). The fallback runs
/// it instead; the function's real entry still gets translated.
fn mid_function(addr: u64) -> bool {
    let home = crate::sigs::proc_home();
    let found = home.mm().lock().vmas().iter().find(|v| addr >= v.start && addr < v.end).and_then(|v| v.code.clone().map(|(p, o)| (p, o, v.start)));
    let Some((path, off, start)) = found else {
        return false;
    };
    let cached = lock(&FDES).get_or_insert_with(Default::default).get(&path).cloned();
    let obj = match cached {
        Some(o) => o,
        None => {
            let bytes = home.read_file(&path).unwrap_or_default();
            // Not the PLT's: one FDE covers all of `.plt`, whose stubs are
            // each an entry (hp9 left them to the fallback, and died).
            let plt: Vec<(u64, u64)> = gex_xlate::ehframe::sections(&bytes)
                .into_iter()
                .filter(|s| s.name.starts_with(".plt"))
                .map(|s| (s.addr, s.addr + s.size))
                .collect();
            let mut f: Vec<(u64, u64)> = gex_xlate::ehframe::fdes(&bytes, 0)
                .into_iter()
                .filter(|f| !plt.iter().any(|&(lo, hi)| f.start >= lo && f.start < hi))
                .map(|f| (f.start, f.end))
                .collect();
            f.sort_unstable();
            let o = std::sync::Arc::new((f, loads(&bytes)));
            lock(&FDES).get_or_insert_with(Default::default).insert(path.clone(), o.clone());
            o
        }
    };
    let (fdes, loads) = &*obj;
    // The vma's file offset gives its vaddr in the file, hence the bias.
    let Some(base) = loads.iter().find(|&&(o, _, n)| off >= o && off < o + n.max(1)).map(|&(o, v, _)| v + (off - o)) else {
        return false;
    };
    let x = addr - start + base;
    let i = fdes.partition_point(|&(s, _)| s <= x);
    let mid = i > 0 && fdes[i - 1].0 < x && x < fdes[i - 1].1;
    if mid {
        let n = MID_DECLINED.fetch_add(1, Ordering::Relaxed);
        if n < 32 || (n + 1).is_power_of_two() {
            let name = path.rsplit('/').next().unwrap_or(&path);
            crate::warn_line(&format!("[fjit] {addr:#x} ({name} +{x:#x}) is inside a function: left to the fallback ({} so far)\n", n + 1));
        }
    }
    mid
}

/// Code in `ranges` was unmapped or reprotected (`retire_unmapped_code`):
/// what the JIT learned there goes (`Jit::retire`). HK's SDL loads libdbus,
/// unloads it, and maps libxcb over the same range; with libdbus's starts
/// kept, libxcb's `_xcb_in_read_block` ended at a libdbus function's address
/// (+0x30, the `cmovle` of `min(queue_len, len)`) as at a tail call, flags
/// dead, and read the setup reply out of an empty queue: X11 video init
/// failed, the black screen (hb26, hb45; `scanconst=` in hs5 showed the
/// start was libdbus's), and with the unit, a memmove of -8 bytes (hp8).
pub(crate) fn on_retire(ranges: &[(u64, u64)]) {
    if let Some(jit) = JIT.get() {
        jit.retire(0, ranges);
    }
    lock(&SEEDED).retain(|&(_, lo, hi)| !ranges.iter().any(|&(s, e)| s < hi && lo < e));
}

/// The objects whose function starts the JIT has (`seed_mapped`): path and
/// the address range of their `PT_LOAD`s, dropped when code there retires.
static SEEDED: Mutex<Vec<(String, u64, u64)>> = Mutex::new(Vec::new());

/// Give the JIT the function starts of every mapped object it lacks (its
/// symbols, `.eh_frame`, init arrays and relocations, `Jit::add_image`),
/// as natively ahead of time. Without them recovery knows only the starts
/// calls name, and follows a tail jump into the function it reaches: each
/// of libasound's exported wrappers carried a copy of the function it
/// jumps to. Offline, translating each of libasound's 1,698 exports in
/// turn made 57.2 MiB of wasm in 16.4 s without its starts and 9.7 MiB in
/// 7.1 s with them (one function per module: 49.7 MiB and 14.1 s against
/// 3.2 MiB and 1.7 s); HK's menu had 20 MiB of libasound wasm (hp13).
fn seed_mapped(jit: &gex_xlate::fjit::Jit) {
    let home = crate::sigs::proc_home();
    let maps: Vec<(u64, String, u64)> = {
        use gex_kernel::mem::prot;
        home.mm().lock().vmas().iter().filter(|v| v.prot & prot::EXEC != 0).filter_map(|v| v.code.clone().map(|(p, o)| (v.start, p, o))).collect()
    };
    for (start, path, off) in maps {
        if lock(&SEEDED).iter().any(|(p, lo, hi)| *p == path && start >= *lo && start < *hi) {
            continue;
        }
        let bytes = home.read_file(&path).unwrap_or_default();
        let lds = loads(&bytes);
        // The vma's file offset gives its vaddr in the file, hence the bias.
        let Some(bias) = lds.iter().find(|&&(o, _, n)| off >= o && off < o + n.max(1)).map(|&(o, v, _)| start.wrapping_sub(v + (off - o))) else {
            lock(&SEEDED).push((path, start, start + 1));
            continue;
        };
        let lo = lds.iter().map(|l| l.1).min().unwrap_or(0).wrapping_add(bias) & !0xfff;
        let hi = lds.iter().map(|l| l.1 + l.2).max().unwrap_or(0).wrapping_add(bias);
        let n = match gex_ld::elf::load(&bytes, bias) {
            Ok(mut image) => {
                gex_ld::elf::apply_own_relative(&mut image);
                jit.add_image(&image, Some(&bytes))
            }
            Err(_) => 0,
        };
        if BY_OBJECT_ON.load(Ordering::Relaxed) {
            let name = path.rsplit('/').next().unwrap_or(&path);
            crate::warn_line(&format!("[fjit] starts of {name} at {bias:#x}: {n}\n"));
        }
        lock(&SEEDED).push((path, lo, hi.max(lo + 1)));
    }
}

/// Translate the function at `addr` and publish it (`gex.lookup`'s miss):
/// its index in the function table, or `None` (the fallback runs it).
pub(crate) fn translate(addr: u64) -> Option<u32> {
    let r = translate_root(addr);
    if addr != 0 && addr == SCAN_CONST.load(Ordering::Relaxed) {
        crate::warn_line(&format!("[fjitscan] translation asked at {addr:#x}: {r:?}\n"));
    }
    r
}

fn translate_root(addr: u64) -> Option<u32> {
    if !on() || is_tier1_only(addr) || !lib_wanted(addr) {
        return None;
    }
    let only = ONLY.load(Ordering::Relaxed);
    if (only != 0 && addr != only) || lock(&MODULES).len() as u32 >= LIMIT.load(Ordering::Relaxed) {
        return None;
    }
    let jit = JIT.get()?;
    seed_mapped(jit);
    let (exec, ro, wx): (Vec<(u64, u64)>, Vec<(u64, u64)>, Vec<(u64, u64)>) = {
        use gex_kernel::mem::prot;
        let mm = crate::sigs::proc_home().mm().lock();
        let v = mm.vmas();
        (
            v.iter().filter(|v| v.prot & prot::EXEC != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::WRITE == 0 && v.prot & prot::READ != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::EXEC != 0 && v.prot & prot::WRITE != 0).map(|v| (v.start, v.end)).collect(),
        )
    };
    if !exec.iter().any(|&(s, e)| addr >= s && addr < e) {
        return None;
    }
    if mid_function(addr) {
        return None;
    }
    // An address an ELF provider holds is not code to translate (cgen::elfprov).
    // With `?fjitlibs=` rules, callees outside them are not brought in either.
    let rules = !lock(&LIBS).is_empty();
    // A callee a unit or the JIT already serves is not brought into the
    // batch: the call reaches it through the table. Without this, a miss in
    // UnityPlayer (a function its unit lacks) re-translated up to 63 of the
    // unit's own functions with it -- 3,667 functions and 36 MiB of wasm by
    // HK's menu (ht3), JIT code shadowing clang's -- and libasound's served
    // callees went into every batch (28 MiB of wasm by the menu, hp12).
    // `skip` bounds the batch only. Asked of instruction fetch (as part of
    // `executable`), an entry inside the function being recovered -- a
    // resume point, or a root the JIT took inside a function and, since
    // misses no longer make starts, does not stop recovery at -- read as
    // non-code and decoded as illegal: `cmovle` in libxcb's
    // `_xcb_in_read_block` (hu15), and SIGILL at HK's start-up without the
    // unit (hb52).
    let served_memo = std::cell::RefCell::new(std::collections::HashMap::<u64, bool>::new());
    let served = |a: u64| *served_memo.borrow_mut().entry(a).or_insert_with(|| crate::cgen::unit_serves(a) || crate::cgen::jit_serves(a));
    // Asked of every byte fetched: the providers' addresses and replaced
    // ranges taken once, and the range checks kept by 4 KiB page (exec,
    // replaced and object ranges are pages; `?fjitlibs=` ranges are not).
    let provided = crate::cgen::elfprov::provided_set();
    // Native wasm32 Wine DLLs' skeletons as replaced: no x86 code there.
    let mut replaced = crate::cgen::libprov::replaced_ranges();
    replaced.extend(crate::cgen::skeleton_code());
    let page_memo = std::cell::RefCell::new(std::collections::HashMap::<u64, bool>::new());
    let ranged = rules && lock(&LIBS).iter().any(|r| r.range.is_some());
    let executable = |a: u64| {
        let page_ok = |a: u64| exec.iter().any(|&(s, e)| a >= s && a < e) && !replaced.iter().any(|&(lo, hi)| a >= lo && a < hi) && (!rules || lib_wanted(a));
        let ok = if ranged { page_ok(a) } else { *page_memo.borrow_mut().entry(a >> 12).or_insert_with(|| page_ok(a)) };
        ok && !provided.contains(&a)
    };
    let read_only = |a: u64, n: u64| ro.iter().any(|&(s, e)| a >= s && a.saturating_add(n) <= e);
    // Writable-and-executable code (a JIT's) stays tier-1, as natively.
    let volatile = |a: u64| wx.iter().any(|&(s, e)| a >= s && a < e);
    let fresh = std::cell::RefCell::new(Vec::new());
    let checks = std::cell::RefCell::new(Vec::<(u64, u32)>::new());
    let pg = |page: u64| {
        let g = page_gen(page, &mut fresh.borrow_mut());
        let mut c = checks.borrow_mut();
        if !c.iter().any(|&(q, _)| q == page) {
            c.push((page, g));
        }
        g
    };
    let class32 = gex_abi::X86 && gex_xlate::arch::compat();
    trace(&format!("translate {addr:#x}"));
    // Declined (or not holding `addr`): the pages numbered for it get their
    // directory entries anyway, as an install would give them. A page with a
    // base and no entry strands tier-1's blocks there (they go into that
    // base, which is not fresh, so their unit writes no entry for it) --
    // natively a livelock once the emitter left a function over V8's size
    // limit to tier-1 (gex-fjit `max_body_bytes`); here the miss path's
    // reconcile covers the faulting page, not the others.
    let declined = |fresh: Vec<(u64, u32)>| {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        let mut gm = GuestMem::new(crate::guest_bytes());
        for (p, b) in fresh {
            let at = gex_abi::DISPATCH_DIR + p * 4;
            if gm.read_u32(at).unwrap_or(0) == 0 {
                let _ = gm.write_u32(at, b);
            }
        }
    };
    let t0 = crate::now_ns();
    let wasm = jit.translate_skipping(0, crate::guest_bytes(), addr, &executable, &served, &read_only, &volatile, &pg, class32);
    TRANSLATE_NS.fetch_add(crate::now_ns().saturating_sub(t0), Ordering::Relaxed);
    if let Some(w) = &wasm {
        WASM_BYTES.fetch_add(w.len() as u64, Ordering::Relaxed);
    }
    let Some(wasm) = wasm else {
        trace(&format!("declined {addr:#x}"));
        declined(fresh.into_inner());
        return None;
    };
    let Some(funcs) = gex_kernel::ftab::module_funcs(&wasm) else {
        declined(fresh.into_inner());
        return None;
    };
    if !funcs.contains(&addr) {
        declined(fresh.into_inner());
        return None;
    }
    note_object(addr, funcs.len(), wasm.len());
    dump_module(addr, &funcs, &wasm);
    scan_const(addr, &funcs, &wasm);
    let wasm = named(wasm);
    // Held until the module is in the list: a Worker replaying the
    // function table's log (`gex_fjit_ptr`, `gex_fjit_regs`) waits for it
    // rather than reading nothing, and two Workers translating at once get
    // numbers of their own.
    let mut modules = lock(&MODULES);
    let n = modules.len() as u32;
    let pairs: Vec<(u64, u32)> = funcs.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    let install = crate::cgen::publish(funcs.len() as u32, FJIT | n, gex_kernel::ftab::Tier::Jit, &pairs);
    lock(&CHECKS).get_or_insert_with(Default::default).insert(install, checks.into_inner());
    let entries: Vec<u64> = funcs.iter().copied().filter(|&a| a != 0).collect();
    {
        let mem = crate::guest_bytes();
        for &a in &entries {
            gex_kernel::ftab::fcache_clear(mem, a);
        }
    }
    let (regs, mut dirs, slots) = crate::cgen::bridges(&entries);
    for d in fresh.into_inner() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    modules.push(Module { wasm, regs, dirs, slots });
    drop(modules);
    let n = TRANSLATED.fetch_add(1, Ordering::Relaxed) + 1;
    // The translator's line at 1024, 2048, ... translations: a process that
    // never exits (a game) reports too.
    if n >= 1024 && n.is_power_of_two() {
        gex_fjit_report();
    }
    let idx = crate::cgen::index_of(addr);
    if TRACE.load(Ordering::Relaxed) {
        let names: Vec<String> = funcs.iter().map(|&a| format!("{a:#x} {}", crate::object_off_at(a))).collect();
        trace(&format!("module {n}: {addr:#x} {} funcs, install {install}, index {idx:?}: {}", funcs.len(), names.join(", ")));
    }
    idx
}

/// `wasm` with a module name, `gex-fjit` (tests/nt/aot-share.mjs: jit).
fn named(mut wasm: Vec<u8>) -> Vec<u8> {
    let name = b"gex-fjit";
    let mut sec = vec![4u8];
    sec.extend_from_slice(b"name");
    sec.push(0);
    sec.push(1 + name.len() as u8);
    sec.push(name.len() as u8);
    sec.extend_from_slice(name);
    wasm.push(0);
    wasm.push(sec.len() as u8);
    wasm.extend_from_slice(&sec);
    wasm
}

/// A tier-1 dispatch miss at `pc`: does a function serve it (or can the JIT
/// make one)? Then its bridge reaches it once this Worker has installed
/// what the table's log holds (the caller traps or installs in place).
pub(crate) fn serve(pc: u64) -> bool {
    if !on() || is_tier1_only(pc) || !lib_wanted(pc) {
        return false;
    }
    if crate::cgen::index_of(pc).is_some() {
        return false;
    }
    translate(pc).is_some()
}

/// `+name,-name` (`?fjitlibs=`): the objects the JIT is limited to (any `+`)
/// and kept from (`-`), by file name. `name@lo-hi` (hex) narrows a rule to
/// the functions whose entry lies at those offsets into their mapping (the
/// `name +0x..` a trap prints), for bisecting within an object. Checked at
/// every translation, the table lookups' (`gex.lookup`) as well as tier-1's
/// misses: JIT code calling through a pointer must not pull in objects the
/// rules keep out.
struct Rule {
    include: bool,
    name: String,
    range: Option<(u64, u64)>,
}

static LIBS: Mutex<Vec<Rule>> = Mutex::new(Vec::new());

#[no_mangle]
pub extern "C" fn gex_fjit_libs(ptr: u32, len: u32) {
    // The string is in the shim's heap (`gex_alloc`), above the guest's
    // slice (`GUEST_LIMIT`): read it directly, not through `guest_bytes`.
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    let mut v = lock(&LIBS);
    v.clear();
    for w in spec.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        // `watch=0xADDR` (a debugging aid riding on the same query): say when
        // the u64 there changes, at syscalls and safepoints (`watch_check`).
        if let Some(a) = w.strip_prefix("watch=") {
            if let Ok(a) = u64::from_str_radix(a.trim_start_matches("0x"), 16) {
                WATCH.store(a, Ordering::Relaxed);
            }
            continue;
        }
        // `callout=0|2`, `inplace=0`, `callouttrap=1`: the A/B switches of
        // the call-out and the in-place redispatch (bisection aids);
        // `provcheck=1`: check each provider guest call ends at its return;
        // `byobj=1`: count translations by object (`note_object`).
        if let Some(a) = w.strip_prefix("scanconst=") {
            if let Ok(a) = u64::from_str_radix(a.trim_start_matches("0x"), 16) {
                SCAN_CONST.store(a, Ordering::Relaxed);
            }
            continue;
        }
        if let Some(a) = w.strip_prefix("dumpmod=") {
            // `dumpmod=OBJ@OFF`: by the object's name and the hex offset from
            // its text mapping (as `object_off_at` prints), for a layout
            // whose addresses are not known ahead.
            if let Some((name, off)) = a.split_once('@') {
                if let Ok(off) = u64::from_str_radix(off.trim_start_matches("0x"), 16) {
                    *lock(&DUMP_OBJ) = Some((name.to_string(), off));
                }
            } else if let Ok(a) = u64::from_str_radix(a.trim_start_matches("0x"), 16) {
                DUMP_MOD.store(a, Ordering::Relaxed);
            }
            continue;
        }
        if let Some((k, v)) = w.split_once('=') {
            let v: i32 = v.parse().unwrap_or(1);
            match k {
                "callout" => crate::cgen::gex_set_callout(v),
                "inplace" => crate::gex_set_in_place(v),
                "callouttrap" => crate::cgen::TRAP_CALLOUTS.store(v != 0, Ordering::Relaxed),
                "provcheck" => crate::cgen::libprov::PROV_CHECK.store(v != 0, Ordering::Relaxed),
                "byobj" => BY_OBJECT_ON.store(v != 0, Ordering::Relaxed),
                "coutstat" => crate::cgen::COUT_STAT.store(v != 0, Ordering::Relaxed),
                _ => crate::warn_line(&format!("gex: ?fjitlibs: no switch {k}\n")),
            }
            continue;
        }
        let (include, w) = match w.strip_prefix('-') {
            Some(n) => (false, n),
            None => (true, w.trim_start_matches('+')),
        };
        let hex = |x: &str| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok();
        let (name, range) = match w.split_once('@') {
            Some((n, r)) => (n, r.split_once('-').and_then(|(lo, hi)| Some((hex(lo)?, hex(hi)?)))),
            None => (w, None),
        };
        v.push(Rule { include, name: name.to_string(), range });
    }
}

static WATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WATCH_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
static WATCH_SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `?fjitlibs=watch=0xADDR`: the u64 at ADDR changed since the last look
/// (any thread's): say where this thread is (`at`: syscall or safepoint).
pub(crate) fn watch_check(pc: u64, at: &str) {
    let a = WATCH.load(Ordering::Relaxed);
    if a == 0 {
        return;
    }
    // Not before the memory reaches it (the slice spans the maximum).
    if a + 8 > crate::warch::memory_size(0) as u64 * 65536 {
        return;
    }
    let mem = crate::guest_bytes();
    let Some(b) = mem.get(a as usize..a as usize + 8) else { return };
    let v = u64::from_le_bytes(b.try_into().unwrap());
    let last = WATCH_LAST.swap(v, Ordering::Relaxed);
    if last != v && WATCH_SAID.fetch_add(1, Ordering::Relaxed) < 64 {
        let (thread, whose, vwhose) = (crate::cur(), crate::object_off_at(pc), crate::object_off_at(v));
        crate::warn_line(&format!(
            "[watch] {a:#x}: {last:#x} -> {v:#x} ({vwhose}), seen by thread {thread} at {at} pc {pc:#x} ({whose}), {} JIT modules\n",
            lock(&MODULES).len()
        ));
    }
}

fn lib_wanted(pc: u64) -> bool {
    let rules = lock(&LIBS);
    if rules.is_empty() {
        return true;
    }
    let (name, off) = crate::object_file_off(pc);
    let hit = |r: &Rule| r.name == name && r.range.is_none_or(|(lo, hi)| off >= lo && off < hi);
    if rules.iter().any(|r| !r.include && hit(r)) {
        return false;
    }
    !rules.iter().any(|r| r.include) || rules.iter().any(|r| r.include && hit(r))
}

/// JIT module `n`'s bytes.
#[no_mangle]
pub extern "C" fn gex_fjit_ptr(n: u32) -> u32 {
    lock(&MODULES).get(n as usize).map_or(0, |m| m.wasm.as_ptr() as u32)
}

#[no_mangle]
pub extern "C" fn gex_fjit_len(n: u32) -> u32 {
    lock(&MODULES).get(n as usize).map_or(0, |m| m.wasm.len() as u32)
}

/// JIT module `n`'s bridges (as `gex_cgen_regs`).
#[no_mangle]
pub extern "C" fn gex_fjit_regs(n: u32, buf: u32, cap: u32) -> u32 {
    let ms = lock(&MODULES);
    let Some(m) = ms.get(n as usize) else { return 0 };
    let k = m.regs.len().min(cap as usize);
    crate::cgen::put_u32s(&m.regs[..k], buf);
    m.regs.len() as u32
}

#[no_mangle]
pub extern "C" fn gex_fjit_slots(n: u32) -> u32 {
    lock(&MODULES).get(n as usize).map_or(0, |m| m.slots.min(u32::MAX as u64) as u32)
}

/// A Worker installed JIT module `n`: the directory for the pages it numbered.
#[no_mangle]
pub extern "C" fn gex_fjit_installed(n: u32) {
    let dirs = lock(&MODULES).get(n as usize).map(|m| m.dirs.clone()).unwrap_or_default();
    crate::cgen::write_dirs(dirs);
}

/// The translator's line (`gex_xlate::fjit::Jit::stats`: modules, time,
/// and how often its lock was found held), for the host to print as the
/// process ends.
#[no_mangle]
pub extern "C" fn gex_fjit_report() {
    if let (true, Some(jit)) = (on(), JIT.get()) {
        crate::warn_line(&format!("[gex] fjit: {} (translated {}, declined {})\n", jit.stats(), TRANSLATED.load(Ordering::Relaxed), DECLINED.load(Ordering::Relaxed)));
    }
}
