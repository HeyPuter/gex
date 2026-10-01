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
use gex_ir::func::{Function, Summary};
use std::collections::HashMap;
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

/// Where Phase B's functions come from.
pub enum Functions {
    /// S2's recovery (`gex_xlate::func::recover`), in memory form.
    /// `intrinsics` are the entries tier-1 serves with a host builtin
    /// (`gex_xlate::intrinsic`): those functions are re-formed from their
    /// tier-1 block, so C callers get the builtin rather than the lifted loop.
    Recovered {
        functions: Vec<Function>,
        intrinsics: Vec<u64>,
        /// S2's call-graph summaries, by entry.
        summaries: HashMap<u64, Summary>,
    },
    /// The stand-in in `recover.rs`, from the tier-1 blocks and the symbol
    /// addresses (biased). Kept for comparison (`GEX_CGEN_RECOVER=standin`).
    StandIn { symbols: Vec<u64> },
}

/// `GEX_CGEN_RECOVER=standin` selects [`Functions::StandIn`]; S2's recovery
/// is the default.
pub fn use_standin() -> Result<bool> {
    match std::env::var("GEX_CGEN_RECOVER").as_deref() {
        Ok("standin") => Ok(true),
        Ok("s2") | Err(_) => Ok(false),
        Ok(other) => anyhow::bail!("GEX_CGEN_RECOVER={other}: want `s2` or `standin`"),
    }
}

/// Build the function-tier version of a tier-1 module.
///
/// `tier1` is the module `ModuleBuilder` produced from `blocks`, and each
/// block comes with the function index `ModuleBuilder::reserve` gave it.
/// The result has the tier-1 module's exact interface.
/// `class32` is an i386 program in compatibility mode (x86 builds only).
pub fn build(
    tier1: &[u8],
    blocks: &[(&Block, u32)],
    functions: Functions,
    class32: bool,
) -> Result<(Vec<u8>, SpliceStats)> {
    emit::check_environment()?;
    match tier()? {
        Tier::Blocks => build_blocks(tier1, blocks),
        Tier::Functions => build_functions(tier1, blocks, functions, class32),
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

/// Phase B: whole functions (`func.rs`).
fn build_functions(
    tier1: &[u8],
    blocks: &[(&Block, u32)],
    functions: Functions,
    class32: bool,
) -> Result<(Vec<u8>, SpliceStats)> {
    let index: std::collections::HashMap<u64, u32> =
        blocks.iter().map(|(b, f)| (b.start, *f)).collect();
    let refs: Vec<&Block> = blocks.iter().map(|(b, _)| *b).collect();
    let mut sums: HashMap<u64, Summary> = HashMap::new();
    let mut funcs = match functions {
        Functions::StandIn { symbols } => recover::functions(&refs, &symbols),
        Functions::Recovered {
            mut functions,
            intrinsics,
            summaries,
        } => {
            sums = summaries;
            let by_addr: std::collections::HashMap<u64, &Block> =
                refs.iter().map(|b| (b.start, *b)).collect();
            let intrinsics: HashSet<u64> = intrinsics.into_iter().collect();
            for f in &mut functions {
                if intrinsics.contains(&f.entry) {
                    if let Some(b) = by_addr.get(&f.entry) {
                        // One block: the builtin, then the `ret`.
                        let mut one = recover::functions(&[*b], &[f.entry]);
                        *f = one.pop().expect("an entry block forms a function");
                    }
                }
            }
            functions.retain(|f| f.form == gex_ir::func::Form::Memory);
            functions
        }
    };
    // A function sharing a page with a block that has a self-modification
    // guard stays tier-1: the C side checks no code bytes. (Pages, not
    // exact ranges: coarser, and immune to overlapping x86 blocks.)
    let pages = |lo: u64, hi: u64| (lo >> 12)..=(hi.max(lo + 1) - 1) >> 12;
    let guarded: HashSet<u64> = refs
        .iter()
        .filter(|b| b.guard.is_some())
        .flat_map(|b| pages(b.start, b.end))
        .collect();
    let overlaps = |lo: u64, hi: u64| pages(lo, hi).any(|p| guarded.contains(&p));
    funcs.retain(|f| !f.blocks.iter().any(|b| !b.synthetic && overlaps(b.start, b.end)));
    for f in &funcs {
        f.verify()
            .map_err(|e| anyhow::anyhow!("recovered function {:#x} is malformed: {e}", f.entry))?;
    }
    let known: HashSet<u64> = funcs.iter().map(|f| f.entry).collect();
    let trapfree = func::trap_free(&funcs, &sums);
    let jobs = toolchain::jobs();
    let per = funcs.len().div_ceil(jobs * 4).max(16);
    let mut units = Vec::new();
    let mut stats = func::FnStats::default();
    for (k, chunk) in funcs.chunks(per).enumerate() {
        let mut u = func::FnUnit::new();
        u.set_class32(class32);
        for f in chunk {
            u.add(f, &known, &sums, &trapfree)
                .with_context(|| format!("emitting C for the function at {:#x}", f.entry))?;
        }
        stats.merge(&u.stats);
        units.push((format!("unit{k:04}.c"), u.source()));
    }
    if std::env::var_os("GEX_CGEN_STATS").is_some() {
        let mut leaves: Vec<(u64, usize)> = stats.leaves.iter().map(|(a, n)| (*a, *n)).collect();
        leaves.sort_by_key(|(a, n)| (std::cmp::Reverse(*n), *a));
        eprintln!(
            "[cgen] {} functions ({} trap-free), {} blocks; calls: {} known ({} to trap-free), \
             {} indirect, {} leaving; tail calls: {} known; {} switches, {} loops outlined, {} unresolved jumps, {} returns",
            stats.functions,
            trapfree.len(),
            stats.blocks,
            stats.calls_known,
            stats.calls_trapfree,
            stats.calls_indirect,
            stats.leaves.values().sum::<usize>(),
            stats.tail_known,
            stats.switches,
            stats.loops_outlined,
            stats.unresolved,
            stats.returns
        );
        for (a, n) in leaves.iter().take(20) {
            eprintln!("[cgen]   leaves for {a:#x} at {n} sites");
        }
    }
    let entries: Vec<u64> = funcs.iter().map(|f| f.entry).collect();
    units.push(("call_known.c".into(), func::call_known_source(&entries)));
    let exports: Vec<String> = entries.iter().map(|a| func::entry_name(*a)).collect();
    let (cwasm, dir) = compile_and_link(units, &exports)?;
    // Registered in dispatch: the entries tier-1 also has a block for. The
    // others (tier-1 found no block there) are reached only by C calls; a
    // dispatch to them goes to the runtime's own translation.
    let pairs: Vec<(u64, u32)> = entries
        .iter()
        .filter_map(|a| index.get(a).map(|f| (*a, *f)))
        .collect();
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
