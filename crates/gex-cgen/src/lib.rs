//! The function tier: GIR → C → clang → wasm, spliced into tier-1 modules.
//!
//! See `docs/function-tier.md`. Phase A (this crate today) emits every tier-1
//! block as a C function with the uniform `(ctx)` signature, compiles the
//! lot with clang, and splices the result into the tier-1 module built from
//! the same blocks, so both hosts install it unchanged.

pub mod emit;
pub mod header;
pub mod splice;
pub mod toolchain;

use anyhow::{Context, Result};
use gex_ir::Block;
use std::collections::HashSet;

pub use splice::SpliceStats;

/// Blocks per translation unit, before it is worth another clang process.
const MIN_UNIT: usize = 256;

/// Build the function-tier version of a tier-1 module.
///
/// `tier1` is the module `ModuleBuilder` produced from `blocks`, and each
/// block comes with the function index `ModuleBuilder::reserve` gave it. The
/// result has the tier-1 module's exact interface.
pub fn build(tier1: &[u8], blocks: &[(&Block, u32)]) -> Result<(Vec<u8>, SpliceStats)> {
    emit::check_environment()?;
    let timing = std::env::var_os("GEX_TIMING").is_some();
    let started = std::time::Instant::now();

    // A block with a self-modification guard stays tier-1 (§9 of the note).
    let owned: Vec<(&Block, u32)> = blocks
        .iter()
        .filter(|(b, _)| b.guard.is_none())
        .copied()
        .collect();
    let owned_set: HashSet<u64> = owned.iter().map(|(b, _)| b.start).collect();

    let dir = toolchain::WorkDir::new()?;
    std::fs::write(dir.path.join("gex_abi.h"), header::header())?;

    // Contiguous slices in address order, so a loop's blocks usually share a
    // unit and clang can turn their tail calls into a loop.
    let jobs = toolchain::jobs();
    let per = owned.len().div_ceil(jobs * 4).max(MIN_UNIT);
    let mut sources = Vec::new();
    for (k, chunk) in owned.chunks(per).enumerate() {
        let mut unit = emit::Unit::new();
        for (b, _) in chunk {
            unit.add_block(b, &owned_set)
                .with_context(|| format!("emitting C for the block at {:#x}", b.start))?;
        }
        let path = dir.path.join(format!("unit{k:04}.c"));
        std::fs::write(&path, unit.source())?;
        sources.push(path);
    }
    if timing {
        eprintln!("[timing]   cgen emit: {:.2}s ({} units)", started.elapsed().as_secs_f64(), sources.len());
    }
    let started = std::time::Instant::now();
    let objs = toolchain::compile_all(&dir.path, &sources)?;
    let exports: Vec<String> = owned.iter().map(|(b, _)| emit::fn_name(b.start)).collect();
    let linked = dir.path.join("c.wasm");
    toolchain::link(&dir.path, &objs, &exports, &linked)?;
    if timing {
        eprintln!("[timing]   clang + wasm-ld: {:.2}s", started.elapsed().as_secs_f64());
    }
    let cwasm = std::fs::read(&linked)?;
    let pairs: Vec<(u64, u32)> = owned.iter().map(|(b, f)| (b.start, *f)).collect();
    let (wasm, stats) = splice::splice(tier1, &cwasm, &pairs, &emit::fn_name, true)?;
    if std::env::var_os("GEX_CGEN_KEEP").is_some() {
        std::fs::write(dir.path.join("spliced.wasm"), &wasm)?;
    }
    Ok((wasm, stats))
}
