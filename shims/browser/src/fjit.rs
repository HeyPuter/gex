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
    crate::with_host(|h| match h.jit.page_table.lookup(page) {
        Some(b) => b,
        None => {
            let b = h.jit.page_table.base(page);
            fresh.push((page, b));
            b
        }
    })
}

/// Translate the function at `addr` and publish it (`gex.lookup`'s miss):
/// its index in the function table, or `None` (the fallback runs it).
pub(crate) fn translate(addr: u64) -> Option<u32> {
    if !on() || is_tier1_only(addr) || !lib_wanted(addr) {
        return None;
    }
    let only = ONLY.load(Ordering::Relaxed);
    if (only != 0 && addr != only) || lock(&MODULES).len() as u32 >= LIMIT.load(Ordering::Relaxed) {
        return None;
    }
    let jit = JIT.get()?;
    let (exec, ro, wx): (Vec<(u64, u64)>, Vec<(u64, u64)>, Vec<(u64, u64)>) = crate::with_host(|h| {
        use gex_kernel::mem::prot;
        let v = h.kernel.mem.vmas();
        (
            v.iter().filter(|v| v.prot & prot::EXEC != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::WRITE == 0 && v.prot & prot::READ != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::EXEC != 0 && v.prot & prot::WRITE != 0).map(|v| (v.start, v.end)).collect(),
        )
    });
    if !exec.iter().any(|&(s, e)| addr >= s && addr < e) {
        return None;
    }
    // An address an ELF provider holds is not code to translate (cgen::elfprov).
    let executable = |a: u64| exec.iter().any(|&(s, e)| a >= s && a < e) && !crate::cgen::elfprov::provided(a) && !crate::cgen::libprov::replaced(a);
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
    let Some(wasm) = jit.translate(crate::guest_bytes(), addr, &executable, &read_only, &volatile, &pg, class32) else {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        trace(&format!("declined {addr:#x}"));
        return None;
    };
    let funcs = gex_kernel::ftab::module_funcs(&wasm)?;
    if !funcs.contains(&addr) {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
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
    TRANSLATED.fetch_add(1, Ordering::Relaxed);
    let idx = crate::cgen::index_of(addr);
    if TRACE.load(Ordering::Relaxed) {
        let names: Vec<String> = crate::with_host(|h| funcs.iter().map(|&a| format!("{a:#x} {}", crate::object_off_at(h, a))).collect());
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
    let spec = String::from_utf8_lossy(&crate::guest_bytes()[ptr as usize..ptr as usize + len as usize]).into_owned();
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
        // the call-out and the in-place redispatch (bisection aids).
        if let Some((k, v)) = w.split_once('=') {
            let v: i32 = v.parse().unwrap_or(1);
            match k {
                "callout" => crate::cgen::gex_set_callout(v),
                "inplace" => crate::gex_set_in_place(v),
                "callouttrap" => crate::cgen::TRAP_CALLOUTS.store(v != 0, Ordering::Relaxed),
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
        let (thread, whose, vwhose) = crate::with_host(|h| (crate::cur(), crate::object_off_at(h, pc), crate::object_off_at(h, v)));
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
