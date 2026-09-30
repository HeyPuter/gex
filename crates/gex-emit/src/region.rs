//! Grouping blocks into one wasm function.
//!
//! Tier-1 emits a function per guest basic block and a tail call per branch,
//! which on a loop costs a register round-trip through the context on every
//! edge and gives V8 many tiny functions it never optimises. A region groups
//! a loop's blocks into one function, with the edges between them as wasm
//! branches and the guest's registers held in locals for the whole thing.
//! Only loops are worth it -- folding straight-line code just duplicates it --
//! so a region is grown from a block that some reachable block branches to.
//!
//! Wider growth (the natural loop, spanning calls, allowing nested loops) was
//! measured and did not pay: a region's cost is its live register set, paid on
//! every entry and exit, and the small accidental regions the forward walk
//! finds are the ones worth forming. `GEX_REGION_NESTED`, `GEX_REGION_RATIO`
//! and `GEX_REGION_MAXLIVE` exist to re-run those experiments; all are off by
//! default and change nothing until set.

use gex_ir::{Block, Term};
use std::collections::{HashMap, HashSet};

/// Blocks in one region, entry first.
pub type Region = Vec<u64>;

/// Grow a region from `entry`, or `None` if there is nothing there worth
/// grouping.
///
/// The rules are what keeps the result sound rather than what makes it big:
///
/// * Every member's every successor is either another member or an edge out.
///   An edge out flushes and tail-calls, exactly as an ordinary block would.
/// * A member may not call the host. A syscall can rewrite the guest's
///   registers, and the whole point is that the region holds them in locals
///   from the first instruction to the last.
/// * There must be a back edge, or the region is straight-line code and
///   grouping it only duplicates it.
pub fn grow(entry: u64, gir: &HashMap<u64, &Block>, limit: usize) -> Option<Region> {
    let member = |a: u64| -> Option<&Block> {
        let b = *gir.get(&a)?;
        // Only ordinary control flow: a syscall, an indirect branch or an
        // illegal instruction all leave through the host.
        match b.term {
            Term::Goto { .. } | Term::Branch { .. } => {}
            _ => return None,
        }
        if b.stmts.iter().any(crate::tier1::clobbers_guest_state) {
            return None;
        }
        // A guarded block checks its bytes on entry; inside a region it would
        // be entered without the check.
        if b.guard.is_some() {
            return None;
        }
        Some(b)
    };

    member(entry)?;

    // Everything reachable from the entry, bounded: a superset of the loop,
    // running on past the exits into whatever follows.
    let mut order = vec![entry];
    let mut seen: HashSet<u64> = HashSet::from([entry]);
    let mut i = 0;
    while i < order.len() {
        let Some(b) = member(order[i]) else {
            i += 1;
            continue;
        };
        for s in b.successors() {
            if seen.contains(&s) || order.len() >= limit {
                continue;
            }
            if member(s).is_some() {
                seen.insert(s);
                order.push(s);
            }
        }
        i += 1;
    }

    // Narrow it to the loop: the blocks that can get back to the entry
    // without leaving the set. Anything else is code after an exit, which
    // would only be duplicated and would drag in its own loops.
    let mut in_loop: HashSet<u64> = HashSet::new();
    loop {
        let mut grew = false;
        for a in &order {
            if in_loop.contains(a) {
                continue;
            }
            let Some(b) = member(*a) else { continue };
            if b.successors()
                .any(|s| s == entry || (in_loop.contains(&s) && s != *a))
            {
                in_loop.insert(*a);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    in_loop.insert(entry);
    order.retain(|a| in_loop.contains(a));
    let seen = in_loop;

    // Exactly one loop, and it goes through the entry.
    //
    // A back edge is what makes this worth forming at all: without one it is
    // straight-line code, and folding that just duplicates the blocks.
    //
    // A loop that does not pass through the entry is rejected unless
    // `GEX_REGION_NESTED` is set. Every back edge inside a region polls (see
    // `region_goto`), so nesting is safe; it is off by default on speed alone.
    let mut to_entry = false;
    for a in &order {
        let Some(b) = member(*a) else { continue };
        for s in b.successors() {
            if !seen.contains(&s) || s > *a {
                continue;
            }
            if s == entry {
                to_entry = true;
            } else if !allow_nested() {
                return None;
            }
        }
    }
    if !to_entry || order.len() < 2 {
        return None;
    }
    worth_forming(&order, gir).then_some(order)
}

/// Whether a loop is worth emitting as one function.
///
/// A region buys the internal edges between members (wasm branches instead of
/// tail calls) and costs the live registers it holds in locals across the
/// whole body. `GEX_REGION_RATIO` is the internal edges each live register
/// must earn; zero (the default) forms everything the growth and soundness
/// rules allow, a positive value asks a loop to be denser than that.
fn worth_forming(order: &[u64], gir: &HashMap<u64, &Block>) -> bool {
    let ratio = region_ratio();
    if ratio <= 0.0 && max_live() == usize::MAX {
        return true;
    }
    let members: HashSet<u64> = order.iter().copied().collect();
    let mut edges = 0usize;
    let mut live: HashSet<gex_ir::GuestReg> = HashSet::new();
    for a in order {
        let Some(b) = gir.get(a) else { continue };
        for s in b.successors() {
            if members.contains(&s) {
                edges += 1;
            }
        }
        let (reads, writes) = crate::tier1::registers_read_and_written(b);
        live.extend(reads);
        live.extend(writes);
    }
    // A loop with no registers to spill is free to form.
    if live.is_empty() {
        return true;
    }
    if live.len() > max_live() {
        return false;
    }
    edges as f64 >= ratio * live.len() as f64
}

/// The largest live register set a region may carry -- the cost, stated
/// directly, since every register it touches is spilled on entry and reloaded
/// at every exit. The default (`usize::MAX`) is off; set it to measure.
fn max_live() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("GEX_REGION_MAXLIVE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(usize::MAX)
    })
}

/// Whether a region may contain a loop that does not pass through its entry.
///
/// Once a safety rule -- the entry used to be the only poll site -- but
/// `region_goto` now polls on every back edge inside the region, so this is a
/// question of speed alone. Keeping it off keeps regions tiny: a wider walk
/// pulls in nested loops and every one disqualifies the whole candidate.
fn allow_nested() -> bool {
    static N: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var_os("GEX_REGION_NESTED").is_some())
}

fn region_ratio() -> f64 {
    static R: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *R.get_or_init(|| {
        std::env::var("GEX_REGION_RATIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    })
}
