//! A recovered function's blocks before emission: tier-1's block passes.
//!
//! Recovery hands over the lifter's statements as they are; gex-cgen leaves
//! the cleanup to clang. The JIT has no optimiser behind it but the engine's,
//! and V8's baseline tier (Liftoff) runs every dead flag computation it is
//! given, so each block gets what tier-1's blocks get (`gex_ir::pass`):
//! constant folding, dead stores, dead code. Dead flags are found across
//! blocks with gex-cgen's liveness, whose exits read everything, so only a
//! flag overwritten before any exit or read is dropped; other registers only
//! within a block, as tier-1 does (function-tier.md §5.1: no host-visible
//! point lies between a block's statements but the traps and clobbering
//! host calls the pass already respects).
//!
//! Values are renumbered per block (they are block-local in memory form), so
//! everything indexed by value is as small as the largest block.

use gex_cgen::func as cf;
use gex_ir::func::{for_each_def_mut, for_each_use_mut, Callee, FTerm, Function, RegSet, Summary};
use gex_ir::{pass, Block, GuestReg, Term, Value};
use std::collections::HashMap;

/// `GEX_FJIT_PASSES=0` leaves the blocks as recovered (pricing).
fn enabled() -> bool {
    std::env::var("GEX_FJIT_PASSES").as_deref() != Ok("0")
}

/// Optimise `f`'s blocks in place; `f.n_values` becomes the largest block's
/// count.
pub fn prepare(f: &mut Function, sums: &HashMap<u64, Summary>) {
    let mut most = 0;
    for b in &mut f.blocks {
        most = most.max(renumber(b));
    }
    f.n_values = most;
    if !enabled() {
        return;
    }
    // Flags dead at each block's exit, by gex-cgen's liveness (flags only:
    // GEX_CGEN_LIVE=0, the exact-flags lockstep mode, keeps them all).
    let dead_out: Vec<Vec<GuestReg>> = if cf::live_writebacks() {
        let live = cf::live_in(f, sums);
        f.blocks
            .iter()
            .map(|b| {
                let out = live_out(&b.term, &live, sums);
                cf::FLAG_REGS.iter().copied().filter(|r| !out.contains(*r)).collect()
            })
            .collect()
    } else {
        vec![Vec::new(); f.blocks.len()]
    };
    let mut most = 0;
    for (b, dead) in f.blocks.iter_mut().zip(&dead_out) {
        let mut v: Option<Value> = None;
        b.term.for_each_use(|x| v = Some(x));
        let term = match (&b.term, v) {
            (FTerm::Branch { .. }, Some(c)) => Term::Branch { cond: c, taken: 0, not_taken: 0 },
            (_, Some(t)) => Term::Indirect { target: t, is_ret: false },
            (_, None) => Term::Goto { target: 0 },
        };
        let mut blk = Block {
            start: b.start,
            end: b.end,
            stmts: std::mem::take(&mut b.stmts),
            term,
            n_values: f.n_values,
            call_return: None,
            guard: None,
        };
        let before = blk.stmts.len();
        pass::const_fold(&mut blk);
        pass::dead_store_elim_with(&mut blk, dead);
        pass::dce(&mut blk);
        if blk.stmts.len() != before {
            // `pcs` names each statement's instruction; what is left keeps
            // no exact correspondence, and only diagnostics read it.
            b.pcs.truncate(blk.stmts.len());
            b.pcs.resize(blk.stmts.len(), b.start);
        }
        b.stmts = blk.stmts;
        most = most.max(renumber(b));
    }
    f.n_values = most;
}

/// What gex-cgen's liveness says is live just before `term` runs (its
/// `live_in` before a block's statements).
fn live_out(term: &FTerm, live: &[RegSet], sums: &HashMap<u64, Summary>) -> RegSet {
    match term {
        FTerm::Jump(e) => live[e.to.index()],
        FTerm::Branch { taken, not_taken, .. } => live[taken.to.index()].union(live[not_taken.to.index()]),
        FTerm::Call { callee: Callee::Direct(t), ret: Some(r), .. } => match sums.get(t) {
            Some(sm) => sm.inputs.union(live[r.to.index()]),
            None => RegSet::ALL,
        },
        _ => RegSet::ALL,
    }
}

/// Number `b`'s values `0..n` in order of definition; returns `n`.
fn renumber(b: &mut gex_ir::func::FBlock) -> u32 {
    let mut map: HashMap<u32, u32> = HashMap::new();
    let mut next = 0u32;
    for s in &mut b.stmts {
        for_each_use_mut(s, |v| *v = Value(*map.get(&v.0).expect("memory form: a value used outside its block")));
        for_each_def_mut(s, |v| {
            map.insert(v.0, next);
            *v = Value(next);
            next += 1;
        });
    }
    b.term.for_each_use_mut(|v| *v = Value(*map.get(&v.0).expect("memory form: a terminator value from another block")));
    next
}
