//! ELF export providers (docs/elf-providers.md): native wasm in place of
//! some of a Linux library's exports (glibc's `memcpy`...), the rest of the
//! library still translated.
//!
//! When the guest maps a shared object that a provider names
//! (`/* PROVIDES libc.so.6 x86_64`), its exports are found by versioned
//! dynamic symbol and bound in the function table before the dynamic
//! linker relocates anything:
//! * an ordinary function at its address, so calls through the PLT and GOT,
//!   `dlsym` and the library's own PLT calls reach the provider;
//! * an IFUNC (`STT_GNU_IFUNC`) by its resolver: the resolver's address runs
//!   the provider's `gexp__ifunc`, which returns the resolver's address plus
//!   one, and that address is bound to the provided function. The dynamic
//!   linker writes it in every GOT slot and IRELATIVE slot it resolves for
//!   the symbol.
//!
//! A `GEX_GROUP` binds whole or not at all. Bound addresses are not code the
//! function JIT may translate ([`provided`]), so a translation that follows
//! a GOT slot into the library cannot publish the library's own code over
//! them.
//!
//! The bindings outlive an unmapping, as PE providers' do: the function
//! table is shared by a process and the children it forks (and their
//! `execve`s), so a child replacing its program must not take them from a
//! parent still running the library, whose GOT holds resolver+1 addresses
//! that only the bindings make code. Only another object mapped over a bound
//! address takes it back ([`on_elf_maps`]).
//!
//! `GEX_PROVIDERS=0` turns this off (A/B runs), `GEX_PROVIDER_SKIP` leaves
//! named exports to the guest, `GEX_PROVIDER_TRACE` lists the bindings.

use crate::HostState;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wasmtime::Caller;

/// One library's bindings: its path, its executable mapping, the addresses.
struct Bound {
    path: String,
    start: u64,
    addrs: Vec<u64>,
}

/// What ELF providers hold, by function table (the `JitShared` they are
/// bound in, by address): what the function JIT leaves alone.
static BOUND: Mutex<Option<HashMap<usize, Vec<Bound>>>> = Mutex::new(None);

fn key(sh: &Arc<Mutex<crate::JitShared>>) -> usize {
    Arc::as_ptr(sh) as usize
}

/// The addresses ELF providers hold in this process, for a translation job.
pub(crate) fn provided(sh: &Arc<Mutex<crate::JitShared>>) -> Arc<HashSet<u64>> {
    let g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
    Arc::new(g.as_ref().and_then(|m| m.get(&key(sh))).map(|v| v.iter().flat_map(|b| b.addrs.iter().copied()).collect()).unwrap_or_default())
}

/// After a Linux syscall that mapped executable code: bind the providers of
/// each newly mapped shared object (see the module documentation).
pub(crate) fn on_elf_maps(caller: &mut Caller<'_, HostState>, mapped: &[gex_kernel::CodeMap]) -> Result<()> {
    if mapped.is_empty() || !gex_cgen::provider::enabled() {
        return Ok(());
    }
    let trace = std::env::var_os("GEX_PROVIDER_TRACE").is_some();
    for m in mapped {
        unbind_overlapped(caller, m, trace);
        let file = m.path.rsplit('/').next().unwrap_or(&m.path);
        // Cheap first: a name no provider has.
        let provs: Vec<gex_cgen::provider::Provider> = [false, true].iter().flat_map(|&c| gex_cgen::provider::for_dll(file, c)).collect();
        if provs.is_empty() {
            continue;
        }
        let Some(jit) = caller.data().jit.as_ref() else { return Ok(()) };
        let (shared, engine) = (Arc::clone(&jit.shared), jit.engine.clone());
        let Some(bytes) = caller.data_mut().with_kernel(|k| k.read_file(&m.path)) else {
            continue;
        };
        let Ok(image) = gex_ld::elf::load(&bytes, 0) else { continue };
        let Some(text) = image.segments.iter().find(|s| s.executable) else { continue };
        let bias = m.start.wrapping_sub(text.vaddr & !0xfff);
        let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
        let exports = image.exports_by_version(lo, hi);
        let id = gex_ld::elf::build_id(&bytes);
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        // A stateful provider is a wasm32 module with its data in the guest's
        // memory: not under a 64-bit guest memory (docs/mem64.md).
        for p in provs.iter().filter(|p| p.class32 == image.class32 && !(p.stateful && gex_abi::mem64())) {
            if !p.build_ids.is_empty() && !id.as_ref().is_some_and(|i| p.build_ids.contains(i)) {
                if trace {
                    eprintln!("[provider] {}: not for this build of {} (build-id {id:?})", p.name, m.path);
                }
                continue;
            }
            let (path, slots) = crate::nt_cgen::provider_module(p)?;
            let slot = |f: &str| slots.iter().position(|s| s.as_deref() == Some(f)).map(|k| k as u32);
            let mut pairs: Vec<(u64, u32)> = gex_cgen::provider::elf_bindings(p, &exports, bias, trace)
                .into_iter()
                .filter_map(|(a, f)| slot(&f).map(|k| (a, k)))
                .collect();
            {
                // Already bound: the same object again where it was (a child's
                // `execve` of a program with the same libc, at the same place).
                let g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(v) = g.as_ref().and_then(|b| b.get(&key(&shared))) {
                    pairs.retain(|(a, _)| !v.iter().any(|b| b.addrs.contains(a)));
                }
            }
            if pairs.is_empty() {
                continue;
            }
            let module = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| gex_cache::compile(&engine, &b))
                .map_err(|e| anyhow!("compiling provider {}: {e:#}", p.name))?;
            crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
            if p.stateful {
                // Its data in guest pages (the stack comes per thread), as a
                // stateful PE provider's (`nt_cgen::install_providers`).
                let wasm = std::fs::read(&path)?;
                let (size, align) = crate::libprov::dylink_memory(&wasm)?;
                let a = 1u64 << align;
                let data = crate::libprov::alloc_top(caller.data_mut(), (size as u64).max(16) + a, &format!("[provider {} data]", p.name))?;
                let data = (data + a - 1) & !(a - 1);
                crate::ftab::publish_stateful(&shared, &memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data })?;
            } else {
                crate::ftab::publish_pairs(&shared, &memory, module, slots.len() as u32, &pairs, gex_kernel::ftab::Tier::Unit)?;
            }
            BOUND.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry(key(&shared)).or_default().push(Bound {
                path: m.path.clone(),
                start: m.start,
                addrs: pairs.iter().map(|&(a, _)| a).collect(),
            });
            if trace {
                eprintln!("gex: provider {} for {} at {bias:#x}: {} addresses bound", p.name, m.path, pairs.len());
            }
        }
        crate::ftab::catch_up(&mut *caller)?;
    }
    Ok(())
}

/// Another object mapped over addresses a library's providers hold: they are
/// its code now, so the bindings there go (in every process sharing the
/// table: two processes with different code at one address are beyond a
/// shared function table, for PE units as for these).
fn unbind_overlapped(caller: &mut Caller<'_, HostState>, m: &gex_kernel::CodeMap, trace: bool) {
    let Some(jit) = caller.data().jit.as_ref() else { return };
    let shared = Arc::clone(&jit.shared);
    let gone: Vec<u64> = {
        let mut g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = g.as_mut().and_then(|b| b.get_mut(&key(&shared))) else { return };
        let mut gone = Vec::new();
        v.retain_mut(|b| {
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
                eprintln!("gex: provider bindings of {} at {:#x}: {} taken back by {} mapped over them", b.path, b.start, before - b.addrs.len(), m.path);
            }
            !b.addrs.is_empty()
        });
        gone
    };
    if gone.is_empty() {
        return;
    }
    let Some(memory) = caller.data().memory.clone() else { return };
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    for a in gone {
        gex_kernel::ftab::fcache_clear(memory.bytes(), a);
        sh.ftab.map.retire(a);
    }
}
