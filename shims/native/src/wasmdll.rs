//! Native Wine DLLs on wasm32 (stream W, docs/wine-native.md §3.2): a DLL
//! whose code is a wasm module, behind a PE skeleton that winebuild
//! (`CPU_WASM32`) wrote. The skeleton maps like any PE; gex binds the
//! module's entry adapters at its export addresses. Its sections:
//!
//! - `.text`: the entry point and a 4-byte `ud2; ud2` stub per exported
//!   function, never run;
//! - `.wasmdat`: the module's data (`__memory_base`), so data exports
//!   point into it;
//! - `.edata`: the exports;
//! - `.wasm`: the module.
//!
//! The module is installed like a stateful provider (`libprov`), with its
//! data at `.wasmdat` rather than allocated: `gexp_<export>` binds at each
//! export, `gexp__entry` at the entry point, `gexp__missing` at exports it
//! has no adapter for (varargs, Wine's system calls, until they have one).

use super::*;

/// If the PE just mapped at `base` is a wasm32 skeleton, install its
/// module. Returns how many exports were bound, or `None` for an ordinary
/// PE.
pub(crate) fn install(
    pe: &gex_ld::pe::PeImage,
    path: &str,
    base: u64,
    state: &mut HostState,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    memory: &gex_mem::GuestMemory,
) -> Result<Option<usize>> {
    let Some(wasm_sec) = pe.sections.iter().find(|s| s.name == ".wasm") else { return Ok(None) };
    let data_sec = pe
        .sections
        .iter()
        .find(|s| s.name == ".wasmdat")
        .ok_or_else(|| anyhow!("{path}: a .wasm section but no .wasmdat"))?;
    let at = (base + wasm_sec.rva as u64) as usize;
    let wasm = memory
        .bytes()
        .get(at..at + wasm_sec.vsize as usize)
        .ok_or_else(|| anyhow!("{path}: .wasm outside memory"))?
        .to_vec();
    let (size, align) = crate::libprov::dylink_memory(&wasm)?;
    let data = base + data_sec.rva as u64;
    if data & ((1u64 << align) - 1) != 0 || (size as u64) > data_sec.vsize as u64 {
        bail!("{path}: .wasmdat ({:#x}, {:#x} bytes) does not hold the module's data ({size:#x} bytes, align {align})", data, data_sec.vsize);
    }
    let slots = gex_cgen::provider::slot_names(&wasm)?;
    let slot = |name: &str| slots.iter().position(|s| s.as_deref() == Some(name)).map(|k| k as u32);
    let missing = slot("gexp__missing").ok_or_else(|| anyhow!("{path}: no gexp__missing in the module's table"))?;
    let mut pairs = Vec::new();
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        // Data exports point into .wasmdat: nothing to bind.
        if e.rva >= data_sec.rva && e.rva < data_sec.rva + data_sec.vsize {
            continue;
        }
        let k = e
            .names
            .iter()
            .find_map(|n| slot(&format!("gexp_{n}")))
            .or_else(|| slot(&format!("gexp_#{}", e.ordinal)))
            .unwrap_or(missing);
        pairs.push((base + e.rva as u64, k));
    }
    if let (Some(k), true) = (slot("gexp__entry"), pe.entry_rva != 0) {
        pairs.push((base + pe.entry_rva as u64, k));
    }
    pairs.sort_unstable();
    pairs.dedup_by_key(|&mut (a, _)| a);
    let n = pairs.len();
    let module = Module::new(engine, &wasm).with_context(|| format!("compiling {path}'s wasm module"))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    let _ = state;
    crate::ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data as u32 })?;
    Ok(Some(n))
}
