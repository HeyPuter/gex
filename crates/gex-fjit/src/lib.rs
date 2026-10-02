//! The function JIT: a recovered guest function → one wasm function,
//! emitted directly from GIR with no clang (docs/merge-plan.md §9.2, §9.8
//! stream F).
//!
//! * The ABI is `F_body` (docs/function-tier.md §7): `(ctx, depth, need,
//!   ret) -> i32`, 1 for the predicted return, 0 for "left, everything
//!   written back, `ctx::PC` set"; the depth guard (§7.1) and loop outlining
//!   (§7.2) as there.
//! * Control flow is structured (`cfg`): blocks become `block`/`loop`/`br`,
//!   guest registers live in wasm locals across the whole function and are
//!   written back only where §5.1 says.
//! * Statements are tier-1's own lowering (`gex_emit::tier1::fmode`), so
//!   memory, flags and FP are tier-1's bit for bit.
//! * The decisions (what to write back, trap-free callees, outer loops) are
//!   gex-cgen's, by the same analyses: this crate is that lowering with wasm
//!   in place of C.
//!
//! Two linkages:
//!
//! * [`runtime`]: a run-time translation, one function and the callees it
//!   brings, installed in the process's function table (merge-plan §9.8.1;
//!   `gexc run --fjit`, through `gex_xlate::fjit`).
//! * [`build`], the whole-image form: every recovered function of an
//!   image, spliced into the tier-1 module built from it, which both hosts
//!   install unchanged (`gexc build --fjit`, `gexc run --fjit-image`).

pub mod cfg;
pub mod emit;
pub mod module;
pub mod prep;

use anyhow::{Context as _, Result};
use gex_ir::func::{Function, Summary};
use gex_ir::Block;
use std::collections::{HashMap, HashSet};

pub use emit::Stats;

/// The function-JIT version of a tier-1 module: `tier1` built from
/// `blocks` (each with its tier-1 function index), with `functions` (S2's
/// recovery, memory form) and their `summaries` emitted and spliced in.
/// `intrinsics` are the entries tier-1 serves with a host builtin.
pub fn build(
    tier1: &[u8],
    blocks: &[(&Block, u32)],
    functions: Vec<Function>,
    intrinsics: &[u64],
    summaries: &HashMap<u64, Summary>,
    class32: bool,
) -> Result<(Vec<u8>, Stats)> {
    let started = std::time::Instant::now();
    let index: HashMap<u64, u32> = blocks.iter().map(|(b, f)| (b.start, *f)).collect();
    let refs: Vec<&Block> = blocks.iter().map(|(b, _)| *b).collect();
    let guarded = gex_cgen::guarded_pages(&refs);
    let intrinsics: HashSet<u64> = intrinsics.iter().copied().collect();
    let ib: HashMap<u64, &Block> = refs.iter().filter(|b| intrinsics.contains(&b.start)).map(|b| (b.start, *b)).collect();
    let mut funcs = gex_cgen::select(functions, &ib, &guarded)?;
    for f in &mut funcs {
        prep::prepare(f, summaries);
    }
    let mut stats = Stats::default();
    let (jwasm, entries) = module::build_module(&funcs, summaries, class32, &mut stats)?;
    let emitted = started.elapsed();
    // Registered in dispatch: the entries tier-1 also has a block for.
    let pairs: Vec<(u64, u32)> = entries.iter().filter_map(|a| index.get(a).map(|f| (*a, *f))).collect();
    let (wasm, _) = gex_cgen::splice::splice(tier1, &jwasm, &pairs, &module::root_name, false)
        .context("splicing the function JIT's module into the tier-1 module")?;
    if std::env::var_os("GEX_FJIT_STATS").is_some() {
        let n = stats.functions.max(1);
        eprintln!(
            "[fjit] {} functions ({} irreducible, {} left to tier-1), {} blocks, {} loops outlined; calls: {} known \
             ({} to trap-free), {} indirect, {} leaving; {} known tail calls, {} switches, {} returns",
            stats.functions,
            stats.irreducible,
            stats.dropped,
            stats.blocks,
            stats.loops_outlined,
            stats.calls_known,
            stats.calls_trapfree,
            stats.calls_indirect,
            stats.calls_leaving,
            stats.tail_known,
            stats.switches,
            stats.returns
        );
        eprintln!(
            "[fjit] emit {:.1} ms ({:.1} us/function), {} code bytes ({} bytes/function), module {} bytes, splice {:.1} ms",
            emitted.as_secs_f64() * 1e3,
            emitted.as_secs_f64() * 1e6 / n as f64,
            stats.code_bytes,
            stats.code_bytes / n,
            jwasm.len(),
            (started.elapsed() - emitted).as_secs_f64() * 1e3
        );
    }
    if let Ok(dir) = std::env::var("GEX_FJIT_KEEP") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(format!("{dir}/fjit.wasm"), &jwasm)?;
        std::fs::write(format!("{dir}/spliced.wasm"), &wasm)?;
    }
    Ok((wasm, stats))
}

/// A run-time translation of `functions` (memory form, verified; the first
/// is the one asked for) for the function table: the module, the entries it
/// holds, what the emitter did. `pages` are the `PAGE_GEN` values each
/// function checks on entry.
/// `intrinsics` are tier-1's blocks for the entries it serves with a host
/// builtin (`gex_xlate::intrinsic`): those functions are formed from them.
pub fn runtime(
    functions: Vec<Function>,
    summaries: &HashMap<u64, Summary>,
    class32: bool,
    pages: &HashMap<u64, Vec<(u64, u32)>>,
    intrinsics: &[Block],
) -> Result<(Vec<u8>, Vec<u64>, Stats)> {
    let ib: HashMap<u64, &Block> = intrinsics.iter().map(|b| (b.start, b)).collect();
    let mut functions = gex_cgen::select(functions, &ib, &HashSet::new())?;
    for f in &mut functions {
        prep::prepare(f, summaries);
    }
    let mut stats = Stats::default();
    let (wasm, entries) = module::build_runtime(&functions, summaries, class32, pages, &mut stats)?;
    Ok((wasm, entries, stats))
}
