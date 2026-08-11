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

//! # What the baseline-compiler argument is actually worth
//!
//! The reason given above for regions -- V8 sees hundreds of thousands of
//! tiny functions and never optimises most of them, and 76% of SQLite's
//! execution stays in the baseline compiler -- suggests a large prize for
//! any change that makes functions fewer and bigger.
//!
//! Measured, it is not there. Running CPython statically recompiled, the
//! same module both sides, one of them with V8 told to compile everything
//! with TurboFan up front (`--no-liftoff`), five alternating rounds,
//! minimums:
//!
//!     fannkuch   default 7.103s   TurboFan-only 6.629s   0.933x
//!     nbody      default 7.568s   TurboFan-only 8.218s   1.086x
//!     dicts      default 2.258s   TurboFan-only 2.172s   0.962x
//!
//! Two marginally faster, one marginally slower, all near the six per cent
//! this can resolve (`bench/ab-browser.mjs --calibrate`). Whatever the
//! tier-up statistic is measuring, removing it entirely buys nothing --
//! which is not an argument against regions, but is an argument against
//! expecting a large win from function count alone.
//!
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
//! *More coverage, three ways*, measured on a harness that had been
//! calibrated against two identical builds first -- which the earlier
//! attempts above did not have, and is why they could only say "somewhere
//! between -1% and +5%".
//!
//! What the regions that do form are worth, against forming none at all:
//! crunch 0.899 (+/-2.6%), interp 0.954 (+/-2.4%). Ten percent on integer
//! code, from 87 blocks out of 5945 -- 1.5% coverage. That is what made more
//! coverage look like the obvious next move.
//!
//! Allowing a region to contain a loop that does not pass through its entry
//! takes crunch from 87 blocks in regions to 1425, sixteen times as many, and
//! costs 34%: 1.343 (+/-1.4%). Correct -- both benchmarks match qemu byte for
//! byte -- and much slower. Note also that widening the forward walk without
//! allowing nesting finds *fewer* regions, not more: 33 down to 26, because a
//! wider walk pulls in inner loops and every one disqualifies the candidate.
//!
//! Selecting among the wide candidates by density -- internal edges per live
//! register -- does almost nothing, because the big regions have the most
//! internal edges and sail through. It removes the small ones, which is
//! backwards.
//!
//! Selecting by the live register set directly, which is the cost this note
//! names, does discriminate: at most 16 live registers gives 168 blocks in
//! regions, twice the default, and everything above 24 is where the harmful
//! ones live. Measured it is a wash -- crunch 0.981 (+/-1.7%), interp 1.027
//! (+/-0.4%) -- slightly better on one and slightly worse on the other.
//!
//! So the accidental heuristic is at a local optimum that two principled
//! alternatives do not beat. `GEX_REGION_NESTED`, `GEX_REGION_RATIO` and
//! `GEX_REGION_MAXLIVE` exist to run these again; all three are off by
//! default and change nothing until set.
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
/// The note above says a region's cost is its live register set, paid on
/// every entry and every exit, and that the small accidental regions the
/// forward walk found were the ones worth forming. That is a statement about
/// a ratio, and it was being enforced by a block count -- which is a proxy
/// for it and a poor one, because two loops of the same size can differ by a
/// factor of three in how many registers they touch.
///
/// So: count what a region buys and what it costs, and compare.
///
/// It buys the edges between members, which become wasm branches instead of
/// tail calls through the dispatch table. It costs the registers it holds in
/// locals, which are written out at entry and read back at every exit --
/// and, more importantly, which Cranelift must keep somewhere for the whole
/// body.
///
/// `GEX_REGION_RATIO` is the number of internal edges each live register has
/// to earn. Zero forms everything the soundness rules allow, which is the
/// natural-loop growth the note measured as a loss; the default asks a loop
/// to be denser than that.
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

/// The largest live register set a region may carry.
///
/// This is the cost, stated directly. Every register the region touches is
/// written out when it is entered and read back at every exit, and held in a
/// wasm local in between -- so a loop that touches forty registers pays for
/// forty however few blocks it has, and the block count that used to stand in
/// for this is a poor proxy: two loops of the same size can differ threefold.
///
/// The default is deliberately larger than anything the current growth rule
/// produces, so it changes nothing until asked.
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
/// The rule against it was a safety rule: the entry used to be the only place
/// a region polled, so a guest cycling inside without reaching the entry would
/// never see a signal or a thread switch. `region_goto` now polls on every
/// back edge inside the region, which costs the default growth rule nothing --
/// it has no such edge -- and leaves this a question of speed alone.
///
/// It is also what keeps regions tiny --
/// growing the walk from 8 blocks to 512 finds *fewer* regions, because a
/// wider walk pulls in nested loops and every one of them disqualifies the
/// whole candidate.
///
/// Worth measuring against, because the regions that do form are worth about
/// 10% on an integer benchmark while covering 1.5% of blocks. Off by default:
/// with it on, a long-running inner loop is unpreemptable.
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
