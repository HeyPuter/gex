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

//! # Things tried that did not pay
//!
//! *Growing the natural loop* instead of walking forwards and narrowing.
//! The textbook construction -- find the latches, take everything that
//! reaches one without passing through the header -- finds seven to eleven
//! times as much: Lua goes from 495 blocks in regions to 5617, SQLite from
//! 3396 to 25769. It is also slower. nbench comes out at +1% against the +5%
//! this gets, and Lua is 11% down over three runs. A region's cost is its
//! live register set, paid on every entry and every exit, and a bigger loop
//! has more of both; the small accidental regions this finds turn out to be
//! the ones worth forming. A cost model that grows the real loop and then
//! decides is the thing to try, not more coverage.
//!
//! *Giving each region a function and turning its members into stubs* that
//! record the address and tail-call it, so a branch to any member carries on
//! inside the region rather than running a duplicate copy outside it. Works,
//! and costs: zstd 5% down over three runs, Lua mixed.
//!
//! *Spanning a call*, by following the return rather than the callee. Needs
//! the stubs, since the return lands on a member. With forward growth it
//! collapses regions (SQLite 1287 to 21) because the walk wanders off down
//! the code after each call; with natural-loop growth it works and inherits
//! that approach's losses.
//!
//! *Leaving the region to poll* rather than calling the host from inside it,
//! which makes it impossible to preempt a guest thread with its registers in
//! a region's locals. That was worth trying because three separate attempts
//! at wider regions failed only under the browser host's cooperative
//! preemption -- but the cause turned out to be an ownership bug in one of
//! those attempts rather than anything about polling, and against this
//! machine's spread the change measures somewhere between -1% and +5% with
//! the baseline itself moving 15% between runs. Not shippable on that
//! evidence either way.

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

    // Everything reachable from the entry, bounded. This is a superset of
    // the loop: it runs on past the exits into whatever follows.
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
    // without leaving the set. Anything else is code after an exit, and
    // pulling that in both duplicates it and drags in its own loops -- which
    // is what makes a region containing more than one loop, the shape this
    // deliberately does not form.
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
    // a tree of straight-line code, and folding that buys one tail call and
    // duplicates the blocks.
    //
    // Requiring the only one to reach the entry is what makes it safe to poll
    // for signals and thread switches there alone. A guest that could cycle
    // inside the region without passing through the entry would otherwise
    // spin without ever reaching a poll -- and polling at the inner back
    // edges instead is what does not work: a guest thread the browser
    // preempts there comes back through the member's own function holding
    // another thread's registers. Where that goes wrong is not yet
    // understood, so a region stays out of the situation by construction.
    let mut to_entry = false;
    for a in &order {
        let Some(b) = member(*a) else { continue };
        for s in b.successors() {
            if !seen.contains(&s) || s > *a {
                continue;
            }
            if s == entry {
                to_entry = true;
            } else {
                return None;
            }
        }
    }
    if !to_entry || order.len() < 2 {
        return None;
    }
    Some(order)
}
