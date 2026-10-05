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
//! them. When the library goes away (`munmap`, `execve`), its bindings are
//! retired with it ([`on_retire`]).
//!
//! `GEX_PROVIDERS=0` turns this off (A/B runs), `GEX_PROVIDER_SKIP` leaves
//! named exports to the guest, `GEX_PROVIDER_TRACE` lists the bindings.

use crate::HostState;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

use std::sync::{Arc, Mutex};
use wasmtime::Caller;

/// Addresses ELF providers hold, by process (the `JitShared` they are bound
/// in, by address): what the function JIT leaves alone, and what a
/// retirement unbinds.
static BOUND: Mutex<Option<HashMap<usize, HashSet<u64>>>> = Mutex::new(None);

fn key(sh: &Arc<Mutex<crate::JitShared>>) -> usize {
    Arc::as_ptr(sh) as usize
}

/// [`key`] from inside the lock (`Mutex` is the `Arc`'s pointee).
pub(crate) fn key_of(m: &Mutex<crate::JitShared>) -> usize {
    m as *const _ as usize
}

/// The addresses ELF providers hold in this process, for a translation job.
pub(crate) fn provided(sh: &Arc<Mutex<crate::JitShared>>) -> Arc<HashSet<u64>> {
    let g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
    Arc::new(g.as_ref().and_then(|m| m.get(&key(sh))).cloned().unwrap_or_default())
}

/// After a Linux syscall that mapped executable code: bind the providers of
/// each newly mapped shared object (see the module documentation).
pub(crate) fn on_elf_maps(caller: &mut Caller<'_, HostState>, mapped: &[gex_kernel::CodeMap]) -> Result<()> {
    if mapped.is_empty() || !gex_cgen::provider::enabled() {
        return Ok(());
    }
    let trace = std::env::var_os("GEX_PROVIDER_TRACE").is_some();
    for m in mapped {
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
        for p in provs.iter().filter(|p| p.class32 == image.class32) {
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
                // Already bound (a second executable mapping of the same object).
                let g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(set) = g.as_ref().and_then(|b| b.get(&key(&shared))) {
                    pairs.retain(|(a, _)| !set.contains(a));
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
            crate::ftab::publish_pairs(&shared, &memory, module, slots.len() as u32, &pairs, gex_kernel::ftab::Tier::Unit)?;
            BOUND.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry(key(&shared)).or_default().extend(pairs.iter().map(|&(a, _)| a));
            if trace {
                eprintln!("gex: provider {} for {} at {bias:#x}: {} addresses bound", p.name, m.path, pairs.len());
            }
        }
        crate::ftab::catch_up(&mut *caller)?;
    }
    Ok(())
}

/// Code in `ranges` went away: the provider bindings in it go with it (the
/// table keeps a unit's functions over a retirement; these are not the
/// unit's to keep). Returns the addresses to retire.
pub(crate) fn on_retire(process: usize, ranges: &[(u64, u64)]) -> Vec<u64> {
    let mut g = BOUND.lock().unwrap_or_else(|e| e.into_inner());
    let Some(set) = g.as_mut().and_then(|b| b.get_mut(&process)) else { return Vec::new() };
    let gone: Vec<u64> = set.iter().copied().filter(|&a| ranges.iter().any(|&(s, e)| a >= s && a < e)).collect();
    for a in &gone {
        set.remove(a);
    }
    gone
}
