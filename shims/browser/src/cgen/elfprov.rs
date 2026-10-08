//! ELF export providers in the browser (docs/elf-providers.md; the native
//! host is shims/native/src/elfprov.rs): a library a provider names
//! (`/* PROVIDES libc.so.6 x86_64`) gets its exports bound in the function
//! table when it maps, by `gex_cgen::provider::elf_bindings`, before the
//! guest goes on.
//!
//! Each library's providers become a provider-only unit (no unit module of
//! its own) that JS serves as it serves an image's providers: it loads the
//! module from the unit cache (`providers/<name>-<fingerprint>.wasm`, built
//! natively) and calls `gex_cgen_provider_publish`. So this needs the unit
//! cache (`--cgen-prefer` under run-node, `cgen=` on a page).
//!
//! A forked child is a fresh shim whose table starts empty, while the GOT it
//! inherited holds resolver+1 addresses that only the bindings make code:
//! before the child runs, JS asks (`gex_elfprov_wanted`), and the libraries
//! its kernel says are mapped are bound again. Bound addresses are not code
//! for the function JIT ([`provided`]). As natively, the bindings outlive an
//! unmapping; another object mapped over them takes them back.

use super::{lock, Provided, Unit, UNITS};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

/// One library's bindings: its path, its executable mapping, the addresses.
struct Bound {
    path: String,
    start: u64,
    addrs: Vec<u64>,
}

static BOUND: Mutex<Vec<Bound>> = Mutex::new(Vec::new());
/// Provider-only units JS has still to serve.
static QUEUE: Mutex<VecDeque<usize>> = Mutex::new(VecDeque::new());
/// The process the state above is for: a forked child's shim may start
/// with its parent's copy, which describes the parent's table.
static PID: AtomicI32 = AtomicI32::new(0);

/// `gex_elfprov_off`: the page asked for none (`GEX_PROVIDERS=0` natively).
static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn on() -> bool {
    super::on() && gex_cgen::provider::enabled() && !OFF.load(Ordering::Relaxed)
}

/// No ELF export providers in this process (A/B runs): before it maps any.
#[no_mangle]
pub extern "C" fn gex_elfprov_off() {
    OFF.store(true, Ordering::Relaxed);
}

/// Whether an ELF provider holds `addr` (the function JIT leaves it alone).
pub(crate) fn provided(addr: u64) -> bool {
    lock(&BOUND).iter().any(|b| b.addrs.contains(&addr))
}

/// A new process (a forked child): start over from what its kernel has
/// mapped. Returns whether something was queued.
fn rescan_if_new() -> bool {
    let pid = crate::with_host(|h| h.kernel.pid);
    let was = PID.swap(pid, Ordering::AcqRel);
    if was == pid {
        return false;
    }
    lock(&BOUND).clear();
    lock(&QUEUE).clear();
    let mapped: Vec<gex_kernel::CodeMap> = crate::with_host(|h| {
        use gex_kernel::mem::prot;
        h.kernel
            .mem
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0)
            .filter_map(|v| v.code.as_ref().map(|(p, off)| gex_kernel::CodeMap { start: v.start, end: v.end, path: p.clone(), offset: *off }))
            .collect()
    });
    maps(&mapped)
}

/// After a syscall: bind the providers of each newly mapped object. Returns
/// whether JS has providers to serve before the guest goes on.
pub(crate) fn maps(mapped: &[gex_kernel::CodeMap]) -> bool {
    // A replaced library (a stateful Linux provider, libprov.rs) is served the same way.
    if let Some(u) = super::libprov::maps(mapped) {
        lock(&QUEUE).push_back(u);
    }
    if !on() {
        return !lock(&QUEUE).is_empty();
    }
    if PID.load(Ordering::Acquire) == 0 {
        PID.store(crate::with_host(|h| h.kernel.pid), Ordering::Release);
    }
    let trace = std::env::var_os("GEX_PROVIDER_TRACE").is_some();
    for m in mapped {
        unbind_overlapped(m, trace);
        let file = m.path.rsplit('/').next().unwrap_or(&m.path);
        let provs: Vec<gex_cgen::provider::Provider> = [false, true].iter().flat_map(|&c| gex_cgen::provider::for_dll(file, c)).collect();
        if provs.is_empty() {
            continue;
        }
        let Some(bytes) = crate::with_host(|h| h.kernel.read_file(&m.path)) else { continue };
        let Ok(image) = gex_ld::elf::load(&bytes, 0) else { continue };
        let Some(text) = image.segments.iter().find(|s| s.executable) else { continue };
        let bias = m.start.wrapping_sub(text.vaddr & !0xfff);
        let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
        let exports = image.exports_by_version(lo, hi);
        let id = gex_ld::elf::build_id(&bytes);
        let mut provided = Vec::new();
        let mut addrs = Vec::new();
        for p in provs.iter().filter(|p| p.class32 == image.class32 && !(p.stateful && gex_abi::mem64())) {
            if !p.build_ids.is_empty() && !id.as_ref().is_some_and(|i| p.build_ids.contains(i)) {
                continue;
            }
            let mut binds: Vec<(String, u64)> = gex_cgen::provider::elf_bindings(p, &exports, bias, trace).into_iter().map(|(a, f)| (f, a)).collect();
            addrs.extend(binds.iter().map(|&(_, a)| a));
            // Already bound here (the same object where it was, or a child's
            // copy of its parent's table).
            binds.retain(|(_, a)| !super::ftab(|t| t.get(*a)).is_some_and(|f| t_is_provider(f.install)));
            if binds.is_empty() {
                continue;
            }
            crate::warn_line(&format!("gex: provider {} for {} at {bias:#x}: {} addresses bound\n", p.name, m.path, binds.len()));
            provided.push(Provided {
                file: format!("providers/{}-{}.wasm", p.name, gex_cgen::provider::fingerprint(std::slice::from_ref(p))),
                binds,
                stateful: None,
                data: p.stateful,
                skeleton: None,
            });
        }
        if !addrs.is_empty() && !lock(&BOUND).iter().any(|b| b.path == m.path && b.start == m.start) {
            lock(&BOUND).push(Bound { path: m.path.clone(), start: m.start, addrs });
        }
        if provided.is_empty() {
            continue;
        }
        let u = {
            let mut units = lock(&UNITS);
            units.push(Unit {
                key: String::new(),
                path: m.path.clone(),
                base: m.start,
                wine: true,
                pages: Vec::new(),
                regs: Vec::new(),
                dirs: Vec::new(),
                slots: 0,
                providers: provided,
                identity: String::new(),
                key_base: String::new(),
            });
            units.len() - 1
        };
        lock(&QUEUE).push_back(u);
    }
    !lock(&QUEUE).is_empty()
}

/// Whether install `i` is an export provider's module.
fn t_is_provider(i: u32) -> bool {
    super::ftab(|t| t.install(i).module) & super::PROVIDER != 0
}

/// Another object mapped over addresses a library's providers hold: its code
/// now, so the bindings there go.
fn unbind_overlapped(m: &gex_kernel::CodeMap, trace: bool) {
    let mut gone = Vec::new();
    lock(&BOUND).retain_mut(|b| {
        if b.path == m.path && b.start == m.start {
            return true;
        }
        let before = b.addrs.len();
        b.addrs.retain(|&a| {
            let hit = a >= m.start && a < m.end;
            if hit {
                gone.push(a);
            }
            !hit
        });
        if trace && b.addrs.len() != before {
            crate::warn_line(&format!("gex: provider bindings of {} taken back by {} mapped over them\n", b.path, m.path));
        }
        !b.addrs.is_empty()
    });
    let mem = crate::guest_bytes();
    for a in gone {
        gex_kernel::ftab::fcache_clear(mem, a);
        super::ftab(|t| t.retire(a));
    }
}

/// The next provider-only unit for JS to serve (`#cgenProviders`), or -1.
/// Asked before a Worker's guest first runs, so a forked child binds what
/// it inherited.
#[no_mangle]
pub extern "C" fn gex_elfprov_wanted() -> i32 {
    if !on() {
        return lock(&QUEUE).pop_front().map_or(-1, |u| u as i32);
    }
    rescan_if_new();
    lock(&QUEUE).pop_front().map_or(-1, |u| u as i32)
}

/// Dispatch slots (table, slot pairs, as `bridges` gives them) JS must point
/// at the bridge again (`gex_elfprov_rebridge`).
static REBRIDGE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// How many times each address was re-bridged (a loop guard).
static REBRIDGED: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());

/// A dispatch miss at an address a provider holds: its slot lost the bridge
/// (a forked child instantiating a module it inherited, which fills the
/// whole page's slots), and tier-1 must not translate it -- an IFUNC's
/// resolver+1 is the middle of an instruction. The slot gets the bridge
/// back (JS writes it before the guest goes on, `#cgenServe`) and the
/// dispatch is retried. False when it is not a provided address.
pub(crate) fn rebridge(pc: u64) -> bool {
    if !provided(pc) || super::index_of(pc).is_none() {
        return false;
    }
    {
        let mut seen = lock(&REBRIDGED);
        let n = match seen.iter_mut().find(|(a, _)| *a == pc) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                seen.push((pc, 1));
                1
            }
        };
        if n > 64 {
            crate::warn_line(&format!("gex: provider address {pc:#x} keeps losing its bridge; leaving it to tier-1\n"));
            return false;
        }
    }
    let (regs, _, slots) = super::bridges(&[pc]);
    // The page's directory entry too, numbered before or not: a forked
    // child's directory is rebuilt from what it can honour, which leaves the
    // page its parent numbered for these bridges at the miss stub.
    let page = pc / gex_abi::DISPATCH_PAGE;
    if let Some(base) = crate::with_host(|h| h.jit.page_table.lookup(page)) {
        super::write_dirs(vec![(page, base)]);
    }
    lock(&REBRIDGE).extend(regs);
    NEED.fetch_max(slots.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    true
}

/// What the dispatch tables must hold for the re-bridged slots.
static NEED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn gex_elfprov_rebridge_need() -> u32 {
    NEED.load(Ordering::Relaxed)
}

/// How many u32s of re-bridge pairs wait (`gex_elfprov_rebridge`).
#[no_mangle]
pub extern "C" fn gex_elfprov_rebridge_len() -> u32 {
    lock(&REBRIDGE).len() as u32
}

/// The re-bridge pairs into `buf` (at most `cap` u32s), taken.
#[no_mangle]
pub extern "C" fn gex_elfprov_rebridge(buf: u32, cap: u32) -> u32 {
    let mut v = lock(&REBRIDGE);
    let k = v.len().min(cap as usize) & !1;
    super::put_u32s(&v[..k], buf);
    v.drain(..k);
    k as u32
}
