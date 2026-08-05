//! Grouping blocks into one wasm function.
//!
//! Tier-1 emits a function per guest basic block and a tail call per branch,
//! which is simple and uniform and costs two things on a loop: the guest's
//! registers go out to the context and come back on every edge, and V8 sees
//! hundreds of thousands of tiny functions and never optimises most of them.
//! Measured on SQLite, 76% of guest execution stays in the baseline compiler.
//!
//! A region is a set of blocks emitted into one function, with the edges
//! between them as wasm branches rather than tail calls and the guest's
//! registers held in locals for the whole thing. Only loops are worth it --
//! straight-line code is entered once, so folding it saves one tail call and
//! duplicates the block -- so a region is grown from a block that some block
//! reachable from it branches back to.

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
        Some(b)
    };

    member(entry)?;
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

    // A back edge into the region is what makes it a loop. Without one this
    // is a tree of straight-line code and inlining it buys a tail call.
    let looping = order.iter().any(|a| {
        member(*a)
            .map(|b| b.successors().any(|s| s <= *a && seen.contains(&s)))
            .unwrap_or(false)
    });
    if !looping || order.len() < 2 {
        return None;
    }
    Some(order)
}
