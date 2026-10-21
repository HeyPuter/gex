//! A recovered function's blocks before emission: tier-1's block passes.
//!
//! Recovery hands over the lifter's statements as they are; gex-cgen leaves
//! the cleanup to clang. The JIT has no optimiser behind it but the engine's,
//! and V8's baseline tier (Liftoff) runs every dead flag computation it is
//! given, so each block gets what tier-1's blocks get (`gex_ir::pass`):
//! constant folding, dead stores, dead code. Dead flags are found across
//! blocks with gex-cgen's liveness, whose exits read everything, so only a
//! flag overwritten before any exit or read is dropped; other registers only
//! within a block, as tier-1 does (no host-visible point lies between a
//! block's statements but the traps and clobbering host calls the pass
//! already respects).
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
    let counts: Vec<u32> = f.blocks.iter_mut().map(renumber).collect();
    f.n_values = counts.iter().copied().max().unwrap_or(0);
    if !enabled() {
        return;
    }
    // Flags dead at each block's exit, by gex-cgen's liveness (flags only:
    // GEX_CGEN_LIVE=0, the exact-flags lockstep mode, keeps them all).
    let dead_out: Vec<Vec<GuestReg>> = if cf::live_writebacks() {
        let live = live_in(f, sums);
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
    for ((b, dead), &count) in f.blocks.iter_mut().zip(&dead_out).zip(&counts) {
        // The passes below see the terminator's value through a tier-1
        // `Term`, made after forwarding has substituted into it: made
        // before, they would keep the old value alive and let the one the
        // terminator now reads go (a `jno` after `test`: `0 ^ 1` forwarded
        // to the constant 1, which DCE then dropped).
        let term_of = |t: &FTerm| {
            let mut v: Option<Value> = None;
            t.for_each_use(|x| v = Some(x));
            match (t, v) {
                (FTerm::Branch { .. }, Some(c)) => Term::Branch { cond: c, taken: 0, not_taken: 0 },
                (_, Some(t)) => Term::Indirect { target: t, is_ret: false },
                (_, None) => Term::Goto { target: 0 },
            }
        };
        let mut blk = Block {
            start: b.start,
            end: b.end,
            stmts: std::mem::take(&mut b.stmts),
            term: term_of(&b.term),
            n_values: count,
            call_return: None,
            guard: None,
        };
        let before = blk.stmts.len();
        forward(&mut blk, &mut b.term);
        blk.term = term_of(&b.term);
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

/// What is live just before `term` runs, given what is live at each
/// block's entry: gex-cgen's rule, except that no callee reads the
/// arithmetic flags it is entered with (`flags_abi`).
pub fn live_out(term: &FTerm, live: &[RegSet], sums: &HashMap<u64, Summary>) -> RegSet {
    let entry = |t: Option<&u64>| match t.and_then(|t| sums.get(t)) {
        Some(sm) => sm.inputs,
        None => RegSet::ALL,
    };
    let abi = |r: RegSet| if flags_abi() { r.minus(RegSet::of(cf::FLAG_REGS)) } else { r };
    match term {
        FTerm::Jump(e) => live[e.to.index()],
        FTerm::Branch { taken, not_taken, .. } => live[taken.to.index()].union(live[not_taken.to.index()]),
        FTerm::Call { callee, ret, .. } => {
            let t = match callee {
                Callee::Direct(t) => Some(t),
                Callee::Indirect(_) => None,
            };
            let after = ret.as_ref().map_or(RegSet::EMPTY, |r| live[r.to.index()]);
            match (callee, ret) {
                // gex-cgen's own rule where it is exact.
                (Callee::Direct(t), Some(r)) if sums.contains_key(t) => sums[t].inputs.union(live[r.to.index()]),
                _ => abi(entry(t)).union(if ret.is_some() { after } else { RegSet::EMPTY }),
            }
        }
        FTerm::TailCall { .. } => abi(RegSet::ALL),
        _ => RegSet::ALL,
    }
}

/// Whether calls are taken not to read the arithmetic flags (x86: no ABI
/// passes anything in them, and compilers treat them as clobbered by every
/// call), so a flag computation whose only reader would be the callee is
/// dead. Returns, switches' defaults and unresolved jumps still read them
/// all. `GEX_FJIT_FLAGS_ABI=0` turns it off; `GEX_CGEN_LIVE=0` (every flag
/// written back) implies off.
pub fn flags_abi() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| gex_abi::X86 && cf::live_writebacks() && std::env::var("GEX_FJIT_FLAGS_ABI").as_deref() != Ok("0"))
}

/// gex-cgen's `live_in` (registers live at each block's entry), with
/// [`live_out`]'s rule for calls.
pub fn live_in(f: &Function, sums: &HashMap<u64, Summary>) -> Vec<RegSet> {
    if !flags_abi() {
        return cf::live_in(f, sums);
    }
    use gex_ir::{Expr, Stmt};
    let n = f.blocks.len();
    let mut live = vec![RegSet::EMPTY; n];
    let order: Vec<usize> = f.rpo().iter().rev().map(|b| b.index()).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &i in &order {
            let b = &f.blocks[i];
            let mut l = match &b.term {
                FTerm::Switch { .. } => RegSet::ALL,
                t => live_out(t, &live, sums),
            };
            for s in b.stmts.iter().rev() {
                match s {
                    Stmt::WriteReg { reg, .. } => l.remove(*reg),
                    Stmt::Def { expr: Expr::ReadReg(r), .. } => l.insert(*r),
                    Stmt::Trap { .. } => l = RegSet::ALL,
                    _ if gex_cgen::emit::clobbers(s) => l = RegSet::ALL,
                    _ => {}
                }
            }
            if l != live[i] {
                live[i] = l;
                changed = true;
            }
        }
    }
    live
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
    let (start, term) = (b.start, format!("{:?}", b.term));
    b.term.for_each_use_mut(|v| {
        *v = Value(*map.get(&v.0).unwrap_or_else(|| panic!("memory form: a terminator value from another block ({start:#x}: {term})")))
    });
    next
}

/// Within a block: a register read after the block wrote the register is
/// the value written (until a host call that may change guest state); and
/// the algebra the lifter leaves for its operand shapes -- `x | 0`,
/// `x & all-ones`, a shift by zero, `wrap(extend(x))` -- is `x`. tier-1's
/// own cache keeps a register in a local within a block too; this keeps
/// the copies out of the code (V8's baseline tier executes every one).
fn forward(blk: &mut Block, term: &mut FTerm) {
    use gex_ir::{BinOp, Expr, Stmt, Ty};
    // By value, over this block's values only (a block is small; the
    // function's numbering is as wide as its largest block).
    let mut subst: HashMap<u32, Value> = HashMap::new();
    let mut consts: HashMap<u32, u64> = HashMap::new();
    let mut tys: HashMap<u32, Ty> = HashMap::new();
    let mut last: HashMap<GuestReg, Value> = HashMap::new();
    let get = |subst: &HashMap<u32, Value>, v: Value| subst.get(&v.0).copied().unwrap_or(v);
    let mut extends: HashMap<u32, Value> = HashMap::new();
    for st in &mut blk.stmts {
        for_each_use_mut(st, |v| *v = get(&subst, *v));
        match st {
            Stmt::WriteReg { reg, val } => {
                last.insert(*reg, *val);
            }
            Stmt::Def { dst, expr } => {
                let k = dst.0;
                tys.insert(k, expr.ty());
                let c = |v: Value| consts.get(&v.0).copied();
                let to: Option<Value> = match *expr {
                    Expr::ReadReg(r) => last.get(&r).copied().filter(|v| tys.get(&v.0) == Some(&r.ty())),
                    Expr::Bin { op: BinOp::Or | BinOp::Xor | BinOp::Add, a, b, .. } if c(a) == Some(0) => Some(b),
                    Expr::Bin { op: BinOp::Or | BinOp::Xor | BinOp::Add | BinOp::Sub, a, b, .. } if c(b) == Some(0) => Some(a),
                    Expr::Bin { op: BinOp::Shl | BinOp::ShrU | BinOp::ShrS | BinOp::Rotr, a, b, .. } if c(b) == Some(0) => Some(a),
                    Expr::Bin { op: BinOp::And, ty, a, b } if c(b) == Some(ty.mask()) => Some(a),
                    Expr::Bin { op: BinOp::And, ty, a, b } if c(a) == Some(ty.mask()) => Some(b),
                    Expr::Bin { op: BinOp::Mul, a, b, .. } if c(b) == Some(1) => Some(a),
                    _ => None,
                };
                // `wrap(extend(x))` is `x`: found through the definition of
                // the extend, kept by value.
                let to = to.or_else(|| match *expr {
                    Expr::Wrap(a) => extends.get(&a.0).copied(),
                    _ => None,
                });
                match *expr {
                    Expr::ConstI32(v) => {
                        consts.insert(k, v as u64);
                    }
                    Expr::ConstI64(v) => {
                        consts.insert(k, v);
                    }
                    _ => {}
                }
                if let Expr::Extend { a, .. } = *expr {
                    extends.insert(dst.0, a);
                }
                if let Some(t) = to {
                    if tys.get(&t.0) == Some(&expr.ty()) {
                        subst.insert(k, t);
                    }
                }
                if let Expr::HostCall { func, .. } = expr {
                    if func.clobbers_guest_state() {
                        last.clear();
                    }
                }
            }
            Stmt::HostCall { func, .. } if func.clobbers_guest_state() => last.clear(),
            _ => {}
        }
    }
    term.for_each_use_mut(|v| *v = get(&subst, *v));
}
