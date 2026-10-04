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
    if !on() || is_tier1_only(addr) {
        return None;
    }
    let jit = JIT.get()?;
    let (exec, ro): (Vec<(u64, u64)>, Vec<(u64, u64)>) = crate::with_host(|h| {
        use gex_kernel::mem::prot;
        let v = h.kernel.mem.vmas();
        (
            v.iter().filter(|v| v.prot & prot::EXEC != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::WRITE == 0 && v.prot & prot::READ != 0).map(|v| (v.start, v.end)).collect(),
        )
    });
    if !exec.iter().any(|&(s, e)| addr >= s && addr < e) {
        return None;
    }
    let executable = |a: u64| exec.iter().any(|&(s, e)| a >= s && a < e);
    let read_only = |a: u64, n: u64| ro.iter().any(|&(s, e)| a >= s && a.saturating_add(n) <= e);
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
    let Some(wasm) = jit.translate(crate::guest_bytes(), addr, &executable, &read_only, &pg, class32) else {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let funcs = gex_kernel::ftab::module_funcs(&wasm)?;
    if !funcs.contains(&addr) {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let wasm = named(wasm);
    let n = lock(&MODULES).len() as u32;
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
    lock(&MODULES).push(Module { wasm, regs, dirs, slots });
    TRANSLATED.fetch_add(1, Ordering::Relaxed);
    crate::cgen::index_of(addr)
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
    if !on() || is_tier1_only(pc) {
        return false;
    }
    if crate::cgen::index_of(pc).is_some() {
        return false;
    }
    translate(pc).is_some()
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
