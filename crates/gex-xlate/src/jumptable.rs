//! Jump-table recovery, on the SSA form of a function.
//!
//! Instead of matching instruction sequences per compiler and ISA, this
//! works on the GIR of the indirect jump, which already says what the
//! `adrp/add/ldrsw/add/br`, `adr/ldrb/add sxtb #2/br`, x86 `jmp
//! *tbl(,%rax,8)` or PIC `movslq (%rdx,%rax,4),%rax; add %rdx,%rax; jmp
//! *%rax` sequences compute. The recipe:
//!
//! 1. The block ending in the indirect jump has exactly one predecessor,
//!    which ends in a conditional branch on `x <u C` / `x <=u C` (or the
//!    negation) whose in-range side is the edge to the jump. In SSA form
//!    `x` is one value, so the bound holds at the jump with no reasoning
//!    about registers in between.
//! 2. The jump target is evaluated for every `x` in `[0, bound]`, reading
//!    the table from the image. Every other input of the target must fold
//!    to a constant (the table base), or resolution fails.
//! 3. The result is **complete** only if the target depends on no bit of
//!    the index the bound check leaves free (a demanded-bits check: a
//!    32-bit compare bounds only the low 32 bits of a 64-bit register), and
//!    every table entry was read from read-only memory. Otherwise the cases
//!    are kept but marked partial, and the backend keeps a default edge.
//!
//! Anything else stays [`FTerm::Unresolved`]: never guessed.

use crate::func::{Code, Table};
use gex_ir::func::*;
use gex_ir::ssa::{to_ssa, CallEffect};
use gex_ir::{pass, BinOp, CmpOp, Expr, GuestReg, Stmt, Ty, Value};
use std::collections::HashMap;

/// Resolve the jump tables ending the blocks at `blocks` (each ends in
/// [`FTerm::Unresolved`]) of memory-form `f`.
pub fn resolve_all(
    f: &Function,
    blocks: &[u64],
    code: &Code,
    max: u32,
    call: &dyn Fn(Callee) -> CallEffect,
) -> Vec<(u64, Option<Table>)> {
    let ssa = to_ssa(f, call);
    let defs = Defs::new(&ssa);
    let preds = ssa.preds();
    blocks
        .iter()
        .map(|&a| (a, resolve_one(&ssa, &defs, &preds, a, code, max)))
        .collect()
}

/// Where each value is defined.
struct Defs<'a> {
    expr: HashMap<Value, &'a Expr>,
}

impl<'a> Defs<'a> {
    fn new(f: &'a Function) -> Defs<'a> {
        let mut expr = HashMap::new();
        for b in &f.blocks {
            for s in &b.stmts {
                if let Stmt::Def { dst, expr: e } = s {
                    expr.insert(*dst, e);
                }
            }
        }
        Defs { expr }
    }
}

/// The constant a value folds to with no inputs, if any.
fn fold(defs: &Defs, v: Value, depth: u32) -> Option<u64> {
    if depth > 32 {
        return None;
    }
    let e = defs.expr.get(&v)?;
    let mut known: Vec<Option<u64>> = Vec::new();
    let mut ok = true;
    e.for_each_use(|u| {
        let c = fold(defs, u, depth + 1);
        if c.is_none() {
            ok = false;
        }
        if known.len() <= u.0 as usize {
            known.resize(u.0 as usize + 1, None);
        }
        known[u.0 as usize] = c;
    });
    if !ok {
        return None;
    }
    if matches!(e, Expr::Load { .. }) {
        return None;
    }
    pass::eval_expr(e, &known)
}

fn swap(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::LtU => CmpOp::GtU,
        CmpOp::GtU => CmpOp::LtU,
        CmpOp::LeU => CmpOp::GeU,
        CmpOp::GeU => CmpOp::LeU,
        CmpOp::LtS => CmpOp::GtS,
        CmpOp::GtS => CmpOp::LtS,
        CmpOp::LeS => CmpOp::GeS,
        CmpOp::GeS => CmpOp::LeS,
        o => o,
    }
}

/// The largest index admitted when `x op c` has truth value `holds`, for
/// unsigned bounds; `None` when it does not bound `x` above.
fn bound(op: CmpOp, c: u64, holds: bool) -> Option<u64> {
    match (op, holds) {
        (CmpOp::LtU, true) | (CmpOp::GeU, false) => c.checked_sub(1),
        (CmpOp::LeU, true) | (CmpOp::GtU, false) => Some(c),
        _ => None,
    }
}

/// Bits of `v` that determine the index, walking down through
/// zero-extending narrowings: `Wrap`, `ExtendBits` unsigned, `And` with a
/// low mask. Each entry is a value set to the index when evaluating, and
/// the mask of its bits the bound check fixes.
fn leaves(defs: &Defs, e: Value, ty: Ty) -> Vec<(Value, u64)> {
    let mut out = vec![(e, ty.mask())];
    let mut cur = e;
    let mut mask = ty.mask();
    for _ in 0..4 {
        let Some(d) = defs.expr.get(&cur) else { break };
        let (inner, m) = match **d {
            Expr::Wrap(x) => (x, 0xffff_ffff),
            Expr::ExtendBits {
                a,
                bits,
                signed: false,
                ..
            } if bits < 64 => (a, (1u64 << bits) - 1),
            Expr::Extend { a, signed: false } => (a, 0xffff_ffff),
            Expr::Bin {
                op: BinOp::And,
                a,
                b,
                ..
            } => match fold(defs, b, 0) {
                Some(m) if (m.wrapping_add(1)) & m == 0 => (a, m),
                _ => break,
            },
            _ => break,
        };
        mask &= m;
        // Bits the inner value's own definition makes zero are fixed too.
        out.push((inner, mask | known_zero(defs, inner)));
        cur = inner;
    }
    out
}

/// Bits of `v` its definition guarantees are zero.
fn known_zero(defs: &Defs, v: Value) -> u64 {
    match defs.expr.get(&v).map(|e| &**e) {
        Some(Expr::Extend { signed: false, .. }) => !0xffff_ffff,
        Some(Expr::ExtendBits {
            bits, signed: false, ..
        }) if *bits < 64 => !((1u64 << bits) - 1),
        Some(Expr::Bin {
            op: BinOp::And, b, ..
        }) => match fold(defs, *b, 0) {
            Some(c) => !c,
            None => 0,
        },
        Some(Expr::Load {
            sz, signed: false, ..
        }) if sz.bytes() < 8 => !((1u64 << (sz.bytes() * 8)) - 1),
        _ => 0,
    }
}

/// Which bits of each value the target depends on (demanded bits), from
/// the target down to the leaves.
fn demanded(defs: &Defs, target: Value, leaves: &[(Value, u64)]) -> Option<HashMap<Value, u64>> {
    let mut d: HashMap<Value, u64> = HashMap::new();
    let mut work: Vec<(Value, u64)> = vec![(target, u64::MAX)];
    let mut steps = 0;
    while let Some((v, m)) = work.pop() {
        steps += 1;
        if steps > 10_000 {
            return None;
        }
        let cur = d.entry(v).or_insert(0);
        if *cur | m == *cur {
            continue;
        }
        *cur |= m;
        let m = *cur;
        if leaves.iter().any(|&(l, _)| l == v) {
            continue;
        }
        let Some(e) = defs.expr.get(&v) else {
            continue; // unknown input: evaluation will fail on it
        };
        // Every bit up to the highest demanded one, for carries.
        let low = |m: u64| {
            if m == 0 {
                0
            } else {
                u64::MAX >> m.leading_zeros()
            }
        };
        match **e {
            Expr::Wrap(a) => work.push((a, m & 0xffff_ffff)),
            Expr::Extend { a, signed } => {
                let mut x = m & 0xffff_ffff;
                if signed && m >> 31 != 0 {
                    x |= 1 << 31;
                }
                work.push((a, x));
            }
            Expr::ExtendBits { a, bits, signed, .. } => {
                let lm = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
                let mut x = m & lm;
                if signed && m & !lm != 0 {
                    x |= 1u64 << (bits - 1);
                }
                work.push((a, x));
            }
            Expr::Bin { op, a, b, ty } => {
                let cb = fold(defs, b, 0);
                match (op, cb) {
                    (BinOp::And, Some(c)) => work.push((a, m & c)),
                    (BinOp::Shl, Some(c)) => {
                        let s = (c & (ty.bits() as u64 - 1)) as u32;
                        work.push((a, (m >> s) & ty.mask()));
                    }
                    (BinOp::ShrU, Some(c)) => {
                        let s = (c & (ty.bits() as u64 - 1)) as u32;
                        work.push((a, (m << s) & ty.mask()));
                    }
                    (BinOp::Add | BinOp::Sub | BinOp::Mul, _) => {
                        work.push((a, low(m)));
                        work.push((b, low(m)));
                    }
                    (BinOp::Or | BinOp::Xor, _) => {
                        work.push((a, m));
                        work.push((b, m));
                    }
                    _ => {
                        work.push((a, ty.mask()));
                        work.push((b, ty.mask()));
                    }
                }
            }
            Expr::Load { addr, .. } => work.push((addr, u64::MAX)),
            ref other => other.for_each_use(|u| work.push((u, u64::MAX))),
        }
    }
    Some(d)
}

/// Evaluate `v` with the leaves fixed, reading loads from the image.
struct Eval<'a, 'b> {
    defs: &'a Defs<'a>,
    code: &'a Code<'b>,
    memo: HashMap<Value, Option<u64>>,
    /// Loads made, as (address, bytes), in evaluation order.
    loads: Vec<(u64, u32)>,
    writable: bool,
}

impl Eval<'_, '_> {
    fn eval(&mut self, v: Value, depth: u32) -> Option<u64> {
        if let Some(&r) = self.memo.get(&v) {
            return r;
        }
        if depth > 64 {
            return None;
        }
        let r = self.compute(v, depth);
        self.memo.insert(v, r);
        r
    }
    fn compute(&mut self, v: Value, depth: u32) -> Option<u64> {
        let e = *self.defs.expr.get(&v)?;
        if let Expr::Load {
            addr,
            offset,
            sz,
            signed,
            ty,
            ..
        } = *e
        {
            let a = self.eval(addr, depth + 1)?.wrapping_add(offset as u64);
            let (raw, w) = self.code.read(a, sz.bytes())?;
            self.writable |= w;
            self.loads.push((a, sz.bytes()));
            let bits = sz.bytes() * 8;
            let x = if bits < 64 && signed {
                let sh = 64 - bits;
                (((raw << sh) as i64) >> sh) as u64
            } else {
                raw
            };
            return Some(x & ty.mask());
        }
        let mut known: Vec<Option<u64>> = Vec::new();
        let mut ops: Vec<Value> = Vec::new();
        e.for_each_use(|u| ops.push(u));
        for u in ops {
            let c = self.eval(u, depth + 1)?;
            if known.len() <= u.0 as usize {
                known.resize(u.0 as usize + 1, None);
            }
            known[u.0 as usize] = Some(c);
        }
        pass::eval_expr(e, &known)
    }
}

fn resolve_one(
    f: &Function,
    defs: &Defs,
    preds: &[Vec<BlockId>],
    at: u64,
    code: &Code,
    max: u32,
) -> Option<Table> {
    let b = f.block_at(at)?;
    let FTerm::Unresolved { target } = f.block(b).term else {
        return None;
    };
    // 1. The bound check.
    let mut ps = preds[b.index()].clone();
    ps.dedup();
    if ps.len() != 1 || f.entry_blocks().contains(&b) {
        return None;
    }
    let p = ps[0];
    let FTerm::Branch {
        cond,
        ref taken,
        ref not_taken,
    } = f.block(p).term
    else {
        return None;
    };
    let holds = match (taken.to == b, not_taken.to == b) {
        (true, false) => true,
        (false, true) => false,
        _ => return None,
    };
    let Some(&&Expr::Cmp { op, ty, a, b: rhs }) = defs.expr.get(&cond) else {
        return None;
    };
    let (op, x, c) = match (fold(defs, a, 0), fold(defs, rhs, 0)) {
        (None, Some(c)) => (op, a, c),
        (Some(c), None) => (swap(op), rhs, c),
        _ => return None,
    };
    let hi = bound(op, c & ty.mask(), holds)?;
    if hi >= max as u64 {
        return None;
    }
    let leaves = leaves(defs, x, ty);

    // 3a. Demanded bits: the target may depend on no index bit the check
    // leaves free.
    let exact = match demanded(defs, target, &leaves) {
        Some(d) => leaves
            .iter()
            .all(|&(l, m)| d.get(&l).copied().unwrap_or(0) & !m == 0),
        None => false,
    };

    // 2. Evaluate every case.
    let mut cases = Vec::with_capacity(hi as usize + 1);
    let mut writable = false;
    let mut first_loads: Vec<(u64, u32)> = Vec::new();
    let mut second_loads: Vec<(u64, u32)> = Vec::new();
    for k in 0..=hi {
        let mut ev = Eval {
            defs,
            code,
            memo: HashMap::new(),
            loads: Vec::new(),
            writable: false,
        };
        for &(l, m) in &leaves {
            ev.memo.insert(l, Some(k & m));
        }
        let t = ev.eval(target, 0)?;
        writable |= ev.writable;
        if k == 0 {
            first_loads = ev.loads.clone();
        } else if k == 1 {
            second_loads = ev.loads.clone();
        }
        // Never guess: a target that is not code fails the whole table.
        if !code.is_code(t) {
            return None;
        }
        cases.push(t);
    }
    // The table: the first load whose address moves with the index.
    let (base, entry_bytes) = first_loads
        .iter()
        .zip(second_loads.iter())
        .find(|(a, b)| a.0 != b.0)
        .map(|(a, b)| (a.0, b.0.wrapping_sub(a.0) as u32))
        .or_else(|| first_loads.first().copied())
        .unwrap_or((cases[0], 0));
    if std::env::var_os("GEX_JT_DEBUG").is_some() {
        let d = demanded(defs, target, &leaves);
        eprintln!(
            "[jt] {at:#x}: leaves {:?} demanded {:?} writable {writable} exact {exact}",
            leaves,
            d.map(|d| leaves.iter().map(|(l, _)| (*l, d.get(l).copied())).collect::<Vec<_>>())
        );
    }
    let index = leaves.iter().rev().find_map(|&(l, _)| match defs.expr.get(&l) {
        Some(Expr::ReadReg(r)) => Some(*r),
        _ => None,
    });
    let _ = GuestReg::Sp;
    Some(Table {
        cases,
        table: JumpTable {
            base,
            entry_bytes,
            count: hi as u32 + 1,
            index,
            complete: exact && !writable,
        },
    })
}
