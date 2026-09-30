//! The function tier: GIR → C → clang → wasm, spliced into tier-1 modules.
//!
//! See `docs/function-tier.md`. Phase A (this crate today) emits every tier-1
//! block as a C function with the uniform `(ctx)` signature, compiles the
//! lot with clang, and splices the result into the tier-1 module built from
//! the same blocks, so both hosts install it unchanged.

pub mod emit;
pub mod func;
pub mod recover;
pub mod header;
pub mod splice;
pub mod toolchain;

use anyhow::{Context, Result};
use gex_ir::Block;
use std::collections::HashSet;

pub use splice::SpliceStats;

/// Blocks per translation unit, before it is worth another clang process.
const MIN_UNIT: usize = 256;

/// Which code the C side takes over: `GEX_CGEN_TIER=blocks` (Phase A, every
/// block) or `functions` (Phase B, whole functions; the default).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Tier {
    Blocks,
    Functions,
}

pub fn tier() -> Result<Tier> {
    match std::env::var("GEX_CGEN_TIER").as_deref() {
        Ok("blocks") => Ok(Tier::Blocks),
        Ok("functions") | Err(_) => Ok(Tier::Functions),
        Ok(other) => anyhow::bail!("GEX_CGEN_TIER={other}: want `blocks` or `functions`"),
    }
}

/// Build the function-tier version of a tier-1 module.
///
/// `tier1` is the module `ModuleBuilder` produced from `blocks`, and each
/// block comes with the function index `ModuleBuilder::reserve` gave it.
/// `symbols` are the image's function symbol addresses (biased), which seed
/// function recovery. The result has the tier-1 module's exact interface.
pub fn build(tier1: &[u8], blocks: &[(&Block, u32)], symbols: &[u64]) -> Result<(Vec<u8>, SpliceStats)> {
    emit::check_environment()?;
    match tier()? {
        Tier::Blocks => build_blocks(tier1, blocks),
        Tier::Functions => build_functions(tier1, blocks, symbols),
    }
}

/// Compile the C sources in `units` (name, text) and link them, exporting
/// `exports`; returns the linked module's bytes.
fn compile_and_link(units: Vec<(String, String)>, exports: &[String]) -> Result<(Vec<u8>, toolchain::WorkDir)> {
    let timing = std::env::var_os("GEX_TIMING").is_some();
    let dir = toolchain::WorkDir::new()?;
    std::fs::write(dir.path.join("gex_abi.h"), header::header())?;
    let mut sources = Vec::new();
    for (name, text) in units {
        let path = dir.path.join(name);
        std::fs::write(&path, text)?;
        sources.push(path);
    }
    let started = std::time::Instant::now();
    let objs = toolchain::compile_all(&dir.path, &sources)?;
    let linked = dir.path.join("c.wasm");
    toolchain::link(&dir.path, &objs, exports, &linked)?;
    if timing {
        eprintln!(
            "[timing]   clang + wasm-ld: {:.2}s ({} units)",
            started.elapsed().as_secs_f64(),
            sources.len()
        );
    }
    Ok((std::fs::read(&linked)?, dir))
}

/// Phase B: whole functions (`func.rs`), recovered by the stand-in in
/// `recover.rs` until S2's recovery lands.
fn build_functions(tier1: &[u8], blocks: &[(&Block, u32)], symbols: &[u64]) -> Result<(Vec<u8>, SpliceStats)> {
    let index: std::collections::HashMap<u64, u32> =
        blocks.iter().map(|(b, f)| (b.start, *f)).collect();
    let refs: Vec<&Block> = blocks.iter().map(|(b, _)| *b).collect();
    let guarded: HashSet<u64> = refs.iter().filter(|b| b.guard.is_some()).map(|b| b.start).collect();
    let mut funcs = recover::functions(&refs, symbols);
    // A function with a self-modification guard anywhere stays tier-1: the C
    // side checks no code bytes.
    funcs.retain(|f| !f.blocks.iter().any(|b| !b.synthetic && guarded.contains(&b.start)));
    for f in &funcs {
        f.verify()
            .map_err(|e| anyhow::anyhow!("recovered function {:#x} is malformed: {e}", f.entry))?;
    }
    let known: HashSet<u64> = funcs.iter().map(|f| f.entry).collect();
    let jobs = toolchain::jobs();
    let per = funcs.len().div_ceil(jobs * 4).max(16);
    let mut units = Vec::new();
    for (k, chunk) in funcs.chunks(per).enumerate() {
        let mut u = func::FnUnit::new();
        for f in chunk {
            u.add(f, &known)
                .with_context(|| format!("emitting C for the function at {:#x}", f.entry))?;
        }
        units.push((format!("unit{k:04}.c"), u.source()));
    }
    let entries: Vec<u64> = funcs.iter().map(|f| f.entry).collect();
    units.push(("call_known.c".into(), func::call_known_source(&entries)));
    let exports: Vec<String> = entries.iter().map(|a| func::entry_name(*a)).collect();
    let (cwasm, dir) = compile_and_link(units, &exports)?;
    let pairs: Vec<(u64, u32)> = entries.iter().map(|a| (*a, index[a])).collect();
    let (wasm, stats) = splice::splice(tier1, &cwasm, &pairs, &func::entry_name, false)?;
    if std::env::var_os("GEX_CGEN_KEEP").is_some() {
        std::fs::write(dir.path.join("spliced.wasm"), &wasm)?;
    }
    Ok((wasm, stats))
}

/// Phase A: every block as its own C function.
fn build_blocks(tier1: &[u8], blocks: &[(&Block, u32)]) -> Result<(Vec<u8>, SpliceStats)> {
    let timing = std::env::var_os("GEX_TIMING").is_some();
    let started = std::time::Instant::now();

    // A block with a self-modification guard stays tier-1 (§9 of the note).
    let owned: Vec<(&Block, u32)> = blocks
        .iter()
        .filter(|(b, _)| b.guard.is_none())
        .copied()
        .collect();
    let owned_set: HashSet<u64> = owned.iter().map(|(b, _)| b.start).collect();

    // Contiguous slices in address order, so a loop's blocks usually share a
    // unit and clang can turn their tail calls into a loop.
    let jobs = toolchain::jobs();
    let per = owned.len().div_ceil(jobs * 4).max(MIN_UNIT);
    let mut units = Vec::new();
    for (k, chunk) in owned.chunks(per).enumerate() {
        let mut unit = emit::Unit::new();
        for (b, _) in chunk {
            unit.add_block(b, &owned_set)
                .with_context(|| format!("emitting C for the block at {:#x}", b.start))?;
        }
        units.push((format!("unit{k:04}.c"), unit.source()));
    }
    if timing {
        eprintln!("[timing]   cgen emit: {:.2}s", started.elapsed().as_secs_f64());
    }
    let exports: Vec<String> = owned.iter().map(|(b, _)| emit::fn_name(b.start)).collect();
    let (cwasm, dir) = compile_and_link(units, &exports)?;
    let pairs: Vec<(u64, u32)> = owned.iter().map(|(b, f)| (b.start, *f)).collect();
    let (wasm, stats) = splice::splice(tier1, &cwasm, &pairs, &emit::fn_name, true)?;
    if std::env::var_os("GEX_CGEN_KEEP").is_some() {
        std::fs::write(dir.path.join("spliced.wasm"), &wasm)?;
    }
    Ok((wasm, stats))
}
