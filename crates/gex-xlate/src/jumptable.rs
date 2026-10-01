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
use crate::summary::Summaries;
use gex_ir::func::*;
use crate::func::to_ssa;
use gex_ir::ssa::CallEffect;
use gex_ir::{pass, BinOp, CmpOp, Expr, Stmt, Ty, Value};
use std::collections::HashMap;

/// Resolve the jump tables ending the blocks at `blocks` (each ends in
/// [`FTerm::Unresolved`]) of memory-form `f`. `sums`, when there are any,
/// stand in for its calls. `extent` is the function's extent from the
/// symbol table or its FDE, which bounds a best-effort table that has no
/// bound check (see [`resolve_unbounded`]).
pub fn resolve_all(
    f: &Function,
    blocks: &[u64],
    code: &Code,
    max: u32,
    sums: Option<&Summaries>,
    extent: Option<(u64, u64)>,
) -> Vec<(u64, Option<Table>)> {
    let call = |c: Callee| sums.map_or(CallEffect::UNKNOWN, |s| s.call_effect(c));
    let ssa = to_ssa(f, &call);
    let known = crate::summary::known_values(f, sums);
    let defs = Defs::new(&ssa, known);
    let preds = ssa.preds();
    blocks
        .iter()
        .map(|&a| {
            let t = resolve_one(&ssa, &defs, &preds, a, code, max)
                .or_else(|| extent.and_then(|x| resolve_unbounded(&ssa, &defs, a, code, max, x)));
            (a, t)
        })
        .collect()
}

/// Where each value is defined.
struct Defs<'a> {
    expr: HashMap<Value, &'a Expr>,
    /// Block parameters: the arguments every incoming edge passes.
    params: HashMap<Value, Vec<Value>>,
    /// Where each statement-defined value is: (block, statement index).
    loc: HashMap<Value, (BlockId, usize)>,
    /// Memo for [`fold`].
    folded: std::cell::RefCell<HashMap<Value, Option<u64>>>,
    /// Set when [`fold`] hit its depth limit.
    cut: std::cell::Cell<bool>,
    /// Values the abstract interpretation proves constant
    /// ([`crate::summary::known_values`]).
    known: HashMap<Value, u64>,
}

impl<'a> Defs<'a> {
    fn new(f: &'a Function, known: HashMap<Value, u64>) -> Defs<'a> {
        let mut expr = HashMap::new();
        let mut params: HashMap<Value, Vec<Value>> = HashMap::new();
        let mut loc = HashMap::new();
        for (bi, b) in f.blocks.iter().enumerate() {
            for (si, s) in b.stmts.iter().enumerate() {
                if let Stmt::Def { dst, expr: e } = s {
                    expr.insert(*dst, e);
                    loc.insert(*dst, (BlockId(bi as u32), si));
                }
            }
            for e in b.term.edges() {
                for (p, a) in f.block(e.to).params.iter().zip(&e.args) {
                    params.entry(p.val).or_default().push(*a);
                }
            }
        }
        Defs {
            expr,
            params,
            loc,
            folded: Default::default(),
            cut: Default::default(),
            known,
        }
    }
}

/// Evaluate a pure scalar expression given each operand's value from
/// `get`; `None` when an operand is unknown or the expression is not pure
/// scalar arithmetic. Operands are renumbered locally, so the folder's
/// table is as long as the operand list rather than the function.
pub(crate) fn eval_with(e: &Expr, get: &mut dyn FnMut(Value) -> Option<u64>) -> Option<u64> {
    let mut local = Stmt::Def {
        dst: Value(u32::MAX),
        expr: e.clone(),
    };
    let mut known: Vec<Option<u64>> = Vec::new();
    let mut ok = true;
    for_each_use_mut(&mut local, |u| {
        if ok {
            let x = get(*u);
            ok &= x.is_some();
            known.push(x);
        }
        *u = Value(known.len().saturating_sub(1) as u32);
    });
    if !ok {
        return None;
    }
    let Stmt::Def { expr, .. } = &local else {
        unreachable!()
    };
    pass::eval_expr(expr, &known)
}

/// The constant a value folds to with no inputs, if any.
fn fold(defs: &Defs, v: Value, depth: u32) -> Option<u64> {
    if depth > 32 {
        defs.cut.set(true);
        return None;
    }
    if let Some(&c) = defs.folded.borrow().get(&v) {
        return c;
    }
    if let Some(&c) = defs.known.get(&v) {
        return Some(c);
    }
    let e = defs.expr.get(&v)?;
    let outer = defs.cut.replace(false);
    let r = if matches!(e, Expr::Load { .. }) {
        None
    } else {
        eval_with(e, &mut |u| fold(defs, u, depth + 1))
    };
    // A failure the depth limit caused is not remembered: from nearer the
    // root it may succeed.
    let cut = defs.cut.get();
    if r.is_some() || !cut {
        defs.folded.borrow_mut().insert(v, r);
    }
    defs.cut.set(outer || cut);
    r
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
    kz(defs, v, 0)
}

fn kz(defs: &Defs, v: Value, depth: u32) -> u64 {
    if depth > 8 {
        return 0;
    }
    let Some(e) = defs.expr.get(&v) else { return 0 };
    let narrow = if e.ty() == Ty::I32 { !0xffff_ffffu64 } else { 0 };
    narrow
        | match **e {
            Expr::Extend { a, signed: false } => !0xffff_ffff | kz(defs, a, depth + 1),
            Expr::ExtendBits {
                a,
                bits,
                signed: false,
                ..
            } if bits < 64 => !((1u64 << bits) - 1) | kz(defs, a, depth + 1),
            Expr::Wrap(a) => kz(defs, a, depth + 1) & 0xffff_ffff,
            Expr::Bin {
                op: BinOp::And, a, b, ..
            } => match fold(defs, b, 0) {
                Some(c) => !c | kz(defs, a, depth + 1),
                None => kz(defs, a, depth + 1) | kz(defs, b, depth + 1),
            },
            Expr::Bin {
                op: BinOp::ShrU,
                a,
                b,
                ty,
            } => match fold(defs, b, 0) {
                Some(c) => {
                    let sh = (c & (ty.bits() as u64 - 1)) as u32;
                    !(ty.mask() >> sh) | kz(defs, a, depth + 1) >> sh
                }
                None => 0,
            },
            Expr::Load {
                sz, signed: false, ..
            } if sz.bytes() < 8 => !((1u64 << (sz.bytes() * 8)) - 1),
            _ => 0,
        }
}

/// Values in the target's expression whose definition bounds them to a
/// small unsigned range (a zero-extended byte, a masked value, `(a & 15) +
/// 15 - (b & 15)`): an index needing no compare. Nearest the target first.
fn narrow_values(defs: &Defs, target: Value, max: u32) -> Vec<(Value, u64, u64)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut ranges: HashMap<Value, Option<(u64, u64)>> = HashMap::new();
    let mut work = std::collections::VecDeque::from([target]);
    while let Some(v) = work.pop_front() {
        if !seen.insert(v) || seen.len() > 256 {
            continue;
        }
        if fold(defs, v, 0).is_some() {
            continue;
        }
        if v != target {
            if let Some((lo, hi)) = range(defs, v, &mut ranges, 0) {
                if hi - lo < max as u64 {
                    // Keep looking below it too: a byte loaded from a
                    // table is narrow, but it is not the index.
                    out.push((v, lo, hi));
                }
            }
        }
        if let Some(e) = defs.expr.get(&v) {
            e.for_each_use(|u| work.push_back(u));
        }
    }
    out
}

/// A sound unsigned interval for `v`, or `None`. Interval arithmetic that
/// gives up on any possible wrap-around.
fn range(
    defs: &Defs,
    v: Value,
    memo: &mut HashMap<Value, Option<(u64, u64)>>,
    depth: u32,
) -> Option<(u64, u64)> {
    if let Some(r) = memo.get(&v) {
        return *r;
    }
    if depth > 24 {
        return None;
    }
    // In progress: a cycle through a loop header gives up.
    memo.insert(v, None);
    let r = range_of(defs, v, memo, depth);
    memo.insert(v, r);
    r
}

fn range_of(
    defs: &Defs,
    v: Value,
    memo: &mut HashMap<Value, Option<(u64, u64)>>,
    depth: u32,
) -> Option<(u64, u64)> {
    let Some(&e) = defs.expr.get(&v) else {
        // A block parameter: the union of what its edges pass.
        let args = defs.params.get(&v)?.clone();
        let mut acc: Option<(u64, u64)> = None;
        for a in args {
            if a == v {
                continue;
            }
            let (lo, hi) = range(defs, a, memo, depth + 1)?;
            acc = Some(match acc {
                None => (lo, hi),
                Some((l, h)) => (l.min(lo), h.max(hi)),
            });
        }
        return acc;
    };
    let full = |t: Ty| -> Option<(u64, u64)> {
        if t == Ty::I32 {
            Some((0, 0xffff_ffff))
        } else {
            None
        }
    };
    let mut r = |x: Value| range(defs, x, memo, depth + 1);
    match *e {
        Expr::ConstI32(c) => Some((c as u64, c as u64)),
        Expr::ConstI64(c) => Some((c, c)),
        Expr::Cmp { .. } | Expr::FpCmp { .. } => Some((0, 1)),
        Expr::Bin { op, ty, a, b } => {
            let m = ty.mask();
            match op {
                BinOp::And => match (r(a), r(b)) {
                    (Some((_, x)), Some((_, y))) => Some((0, x.min(y))),
                    (Some((_, x)), None) | (None, Some((_, x))) => Some((0, x)),
                    _ => full(ty),
                },
                BinOp::Add => match (r(a), r(b)) {
                    (Some((al, ah)), Some((bl, bh))) => {
                        let hi = ah.checked_add(bh).filter(|&h| h <= m);
                        match hi {
                            Some(h) => Some((al + bl, h)),
                            None => full(ty),
                        }
                    }
                    _ => full(ty),
                },
                BinOp::Sub => match (r(a), r(b)) {
                    (Some((al, ah)), Some((bl, bh))) if al >= bh => Some((al - bh, ah - bl)),
                    _ => full(ty),
                },
                BinOp::Shl => match (r(a), fold(defs, b, 0)) {
                    (Some((lo, hi)), Some(s)) => {
                        let s = (s & (ty.bits() as u64 - 1)) as u32;
                        if s < 64 && hi.leading_zeros() >= s && (hi << s) <= m {
                            Some((lo << s, hi << s))
                        } else {
                            full(ty)
                        }
                    }
                    _ => full(ty),
                },
                BinOp::ShrU => match fold(defs, b, 0) {
                    Some(s) => {
                        let s = (s & (ty.bits() as u64 - 1)) as u32;
                        match r(a) {
                            Some((lo, hi)) => Some((lo >> s, hi >> s)),
                            None => Some((0, m >> s)),
                        }
                    }
                    // A logical right shift never grows a value.
                    None => match r(a) {
                        Some((_, hi)) => Some((0, hi)),
                        None => full(ty),
                    },
                },
                BinOp::Mul => match (r(a), r(b)) {
                    (Some((al, ah)), Some((bl, bh))) => match ah.checked_mul(bh) {
                        Some(h) if h <= m => Some((al * bl, h)),
                        _ => full(ty),
                    },
                    _ => full(ty),
                },
                BinOp::RemU => match r(b) {
                    Some((bl, bh)) if bl > 0 => Some((0, bh - 1)),
                    _ => full(ty),
                },
                _ => full(ty),
            }
        }
        Expr::Wrap(a) => match r(a) {
            Some((lo, hi)) if hi <= 0xffff_ffff => Some((lo, hi)),
            _ => Some((0, 0xffff_ffff)),
        },
        Expr::Extend { a, signed } => match r(a) {
            Some((lo, hi)) if !signed || hi < 1 << 31 => Some((lo, hi)),
            _ if !signed => Some((0, 0xffff_ffff)),
            _ => None,
        },
        Expr::ExtendBits {
            a,
            bits,
            signed,
            ty,
        } => {
            let lm = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
            match r(a) {
                Some((lo, hi)) if hi <= lm >> (signed as u32) => Some((lo, hi)),
                _ if !signed => Some((0, lm)),
                _ => full(ty),
            }
        }
        Expr::Load {
            sz, signed, ty, ..
        } => {
            if !signed && sz.bytes() < 8 {
                Some((0, (1u64 << (sz.bytes() * 8)) - 1))
            } else {
                full(ty)
            }
        }
        Expr::VBitmask { lane, .. } => Some((0, (1u64 << (16 / lane.bytes())) - 1)),
        Expr::Un {
            op: gex_ir::UnOp::Clz | gex_ir::UnOp::Ctz | gex_ir::UnOp::Popcnt,
            ty,
            ..
        } => Some((0, ty.bits() as u64)),
        Expr::Select { cond, a, b, ty } => {
            // `x == 0 ? x : y` (x86 `bsf` keeps its zero source): the first
            // arm is zero whenever it is taken.
            let zero_arm = |arm: Value| match defs.expr.get(&cond).map(|e| &**e) {
                Some(&Expr::Cmp {
                    op: CmpOp::Eq, a: x, b: z, ..
                }) => x == arm && fold(defs, z, 0) == Some(0),
                _ => false,
            };
            let za = zero_arm(a);
            let ra = if za { Some((0, 0)) } else { r(a) };
            // The other arm is taken only when x != 0: ctz(x) is then below
            // x's highest possible bit.
            let rb = match defs.expr.get(&b).map(|e| &**e) {
                Some(&Expr::Un {
                    op: gex_ir::UnOp::Ctz,
                    a: x,
                    ..
                }) if za && x == a => match r(x) {
                    Some((_, hi)) if hi != 0 => Some((0, 63 - hi.leading_zeros() as u64)),
                    _ => r(b),
                },
                _ => r(b),
            };
            match (ra, rb) {
                (Some((al, ah)), Some((bl, bh))) => Some((al.min(bl), ah.max(bh))),
                _ => full(ty),
            }
        }
        ref other => full(other.ty()),
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
    /// A parameter was taken from some of its incoming edges only.
    assumed: bool,
}

impl Eval<'_, '_> {
    /// A block parameter: the constant its incoming edges pass. When only
    /// some edges pass one constant and the others something unknown (a
    /// table base in a register a callee may have changed), that constant
    /// is used but the table is marked partial: its cases are real targets
    /// on those paths, and the backend keeps the default edge for the rest.
    fn param(&mut self, v: Value, depth: u32) -> Option<u64> {
        let args = self.defs.params.get(&v)?.clone();
        // Seed the memo to cut cycles through loop headers.
        self.memo.insert(v, None);
        let mut c: Option<u64> = None;
        let mut unknown = false;
        for a in args {
            if a == v {
                continue;
            }
            match self.eval(a, depth + 1) {
                Some(x) if c.is_none() || c == Some(x) => c = Some(x),
                Some(_) => return None,
                None => unknown = true,
            }
        }
        if unknown && c.is_some() {
            self.assumed = true;
        }
        c
    }

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
        if let Some(&c) = self.defs.known.get(&v) {
            return Some(c);
        }
        let Some(&e) = self.defs.expr.get(&v) else {
            return self.param(v, depth);
        };
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
        eval_with(e, &mut |u| self.eval(u, depth + 1))
    }
}

/// Bounds that hold at block `b`: for each dominator `d` of `b` (itself
/// included) entered only by one edge from a conditional branch, the
/// unsigned bound that edge implies. In SSA form the bounded value cannot
/// change between `d` and `b`: `d` dominates `b`, so every path to `b`
/// passes `d` after the value's last definition.
fn bounds(f: &Function, defs: &Defs, preds: &[Vec<BlockId>], b: BlockId) -> Vec<(Value, Ty, u64)> {
    let idom = f.idoms();
    let entries = f.entry_blocks();
    let mut out = Vec::new();
    let mut d = Some(b);
    let mut depth = 0;
    while let Some(db) = d {
        depth += 1;
        if depth > 16 || entries.contains(&db) {
            break;
        }
        let mut ps = preds[db.index()].clone();
        ps.dedup();
        if ps.len() == 1 {
            if let FTerm::Branch {
                cond,
                ref taken,
                ref not_taken,
            } = f.block(ps[0]).term
            {
                let holds = match (taken.to == db, not_taken.to == db) {
                    (true, false) => Some(true),
                    (false, true) => Some(false),
                    _ => None,
                };
                // Look through negations: `ja` is `!(CF | ZF)`, lifted as
                // `(a <=u b) ^ 1`.
                let (cond, holds) = strip_not(defs, cond, holds);
                if let (Some(holds), Some(&&Expr::Cmp { op, ty, a, b: rhs })) =
                    (holds, defs.expr.get(&cond))
                {
                    let c = match (fold(defs, a, 0), fold(defs, rhs, 0)) {
                        (None, Some(c)) => Some((op, a, c)),
                        (Some(c), None) => Some((swap(op), rhs, c)),
                        _ => None,
                    };
                    if let Some((op, x, c)) = c {
                        if let Some(hi) = bound(op, c & ty.mask(), holds) {
                            out.push((x, ty, hi));
                        }
                    }
                }
            }
        }
        d = idom[db.index()];
    }
    out
}

/// Loads in the target's expression that reload the memory a bounded leaf
/// was loaded from (same address value, offset, size and signedness), with
/// no memory write on the straight-line path between, each paired with the
/// leaf's mask.
fn reloads(f: &Function, defs: &Defs, target: Value, lv: &[(Value, u64)]) -> Vec<(Value, u64)> {
    let key = |v: Value| match defs.expr.get(&v).map(|e| &**e) {
        Some(&Expr::Load {
            addr,
            offset,
            sz,
            signed,
            ..
        }) => Some((addr, offset, sz, signed)),
        _ => None,
    };
    // Every load feeding the target.
    let mut loads = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut work = vec![target];
    while let Some(v) = work.pop() {
        if !seen.insert(v) || seen.len() > 256 {
            continue;
        }
        if key(v).is_some() {
            loads.push(v);
        }
        if let Some(e) = defs.expr.get(&v) {
            e.for_each_use(|u| work.push(u));
        }
    }
    let mut out = Vec::new();
    for &(l, m) in lv {
        let Some(k) = key(l) else { continue };
        for &y in &loads {
            if y != l && key(y) == Some(k) && no_writes_between(f, defs, l, y) {
                out.push((y, m));
            }
        }
    }
    out
}

fn writes_memory(s: &Stmt) -> bool {
    match s {
        Stmt::Def { expr, .. } => match expr {
            Expr::Rmw { .. } | Expr::CmpXchg { .. } => true,
            Expr::HostCall { func, .. } => func.clobbers_guest_state(),
            _ => false,
        },
        Stmt::Store { .. }
        | Stmt::VStore { .. }
        | Stmt::MemCopy { .. }
        | Stmt::MemFill { .. }
        | Stmt::StrOp { .. }
        | Stmt::Fence => true,
        Stmt::HostCall { func, .. } => func.clobbers_guest_state(),
        _ => false,
    }
}

/// Whether nothing can write memory between the definitions of `a` and
/// `b`: the same block with no write between, or a straight line of
/// single-predecessor blocks joined by branches from `a`'s block down to
/// `b`'s.
fn no_writes_between(f: &Function, defs: &Defs, a: Value, b: Value) -> bool {
    let (Some(&(ba, ia)), Some(&(bb, ib))) = (defs.loc.get(&a), defs.loc.get(&b)) else {
        return false;
    };
    let clean = |blk: BlockId, from: usize, to: usize| {
        f.block(blk).stmts[from..to].iter().all(|s| !writes_memory(s))
    };
    if ba == bb {
        return ia < ib && clean(ba, ia + 1, ib);
    }
    let preds = f.preds();
    let mut cur = bb;
    let mut hops = 0;
    loop {
        let mut ps = preds[cur.index()].clone();
        ps.dedup();
        if ps.len() != 1 || hops > 8 {
            return false;
        }
        let p = ps[0];
        if !matches!(f.block(p).term, FTerm::Jump(_) | FTerm::Branch { .. }) {
            return false;
        }
        hops += 1;
        if p == ba {
            break;
        }
        if !clean(p, 0, f.block(p).stmts.len()) {
            return false;
        }
        cur = p;
    }
    clean(ba, ia + 1, f.block(ba).stmts.len()) && clean(bb, 0, ib)
}

/// Peel boolean negations off a branch condition: `c ^ 1`, `c == 0`,
/// `c != 0` of a comparison result.
fn strip_not(defs: &Defs, mut c: Value, mut holds: Option<bool>) -> (Value, Option<bool>) {
    for _ in 0..4 {
        let is_bool = |v: Value| {
            matches!(
                defs.expr.get(&v).map(|e| &**e),
                Some(Expr::Cmp { .. })
            )
        };
        match defs.expr.get(&c).map(|e| &**e) {
            Some(&Expr::Bin {
                op: BinOp::Xor, a, b, ..
            }) if fold(defs, b, 0) == Some(1) && is_bool(a) => {
                c = a;
                holds = holds.map(|h| !h);
            }
            Some(&Expr::Cmp {
                op: CmpOp::Eq, a, b, ..
            }) if fold(defs, b, 0) == Some(0) && is_bool(a) => {
                c = a;
                holds = holds.map(|h| !h);
            }
            Some(&Expr::Cmp {
                op: CmpOp::Ne, a, b, ..
            }) if fold(defs, b, 0) == Some(0) && is_bool(a) => {
                c = a;
            }
            _ => break,
        }
    }
    (c, holds)
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
    // 1. The bound check: the first dominating bound the target depends on.
    let bs = bounds(f, defs, preds, b);
    if std::env::var_os("GEX_JT_DEBUG").is_some() {
        eprintln!("[jt] {at:#x}: target {target:?} bounds {bs:?}");
    }
    for (x, ty, hi) in bs {
        if hi >= max as u64 {
            continue;
        }
        let mut lv = leaves(defs, x, ty);
        let depends = |lv: &[(Value, u64)]| match demanded(defs, target, lv) {
            Some(d) => lv.iter().any(|(l, _)| d.get(l).copied().unwrap_or(0) != 0),
            None => false,
        };
        let mut partial = false;
        if !depends(&lv) {
            // The compiler may compare one load and index with a reload of
            // the same memory. Equal only if nothing writes memory in
            // between, and another thread could still: a data race, so
            // such a table is kept partial.
            let reloads = reloads(f, defs, target, &lv);
            if reloads.is_empty() {
                continue;
            }
            lv.extend(reloads);
            partial = true;
            if !depends(&lv) {
                continue;
            }
        }
        if let Some(mut t) = with_bound(defs, target, at, code, lv, hi) {
            t.table.complete &= !partial;
            return Some(t);
        }
    }
    // 1b. An index its own definition bounds (`ldrb` into a byte table).
    for (x, lo, hi) in narrow_values(defs, target, max) {
        if let Some(t) = with_bound_from(defs, target, at, code, vec![(x, u64::MAX)], lo, hi) {
            return Some(t);
        }
    }
    None
}

fn with_bound(
    defs: &Defs,
    target: Value,
    at: u64,
    code: &Code,
    leaves: Vec<(Value, u64)>,
    hi: u64,
) -> Option<Table> {
    with_bound_from(defs, target, at, code, leaves, 0, hi)
}

fn with_bound_from(
    defs: &Defs,
    target: Value,
    at: u64,
    code: &Code,
    leaves: Vec<(Value, u64)>,
    lo: u64,
    hi: u64,
) -> Option<Table> {


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
    for k in lo..=hi {
        let mut ev = Eval {
            defs,
            code,
            memo: HashMap::new(),
            loads: Vec::new(),
            writable: false,
            assumed: false,
        };
        for &(l, m) in &leaves {
            ev.memo.insert(l, Some(k & m));
        }
        let t = ev.eval(target, 0)?;
        writable |= ev.writable || ev.assumed;
        if k == lo {
            first_loads = ev.loads.clone();
        } else if k == lo + 1 {
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
    // Index `lo` was the first evaluated; the table starts at index 0.
    let base = base.wrapping_sub(lo.wrapping_mul(entry_bytes as u64));
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
    Some(Table {
        cases,
        table: JumpTable {
            base,
            entry_bytes,
            count: (hi - lo) as u32 + 1,
            index,
            complete: exact && !writable,
        },
    })
}

/// A jump through a table with no bound check anywhere the resolver can
/// see: the compiler knew the index's range from elsewhere (an enum held in
/// a global, a value range proved in another function). The cases are
/// read best effort and the table is always **partial**, so the backend
/// keeps the default edge: entries are read from index 0 while the table
/// stays in read-only memory, moves by a fixed stride, and every target is
/// code inside the function's `extent` (from the symbol table or its FDE).
/// At least two entries must qualify.
fn resolve_unbounded(
    f: &Function,
    defs: &Defs,
    at: u64,
    code: &Code,
    max: u32,
    extent: (u64, u64),
) -> Option<Table> {
    let b = f.block_at(at)?;
    let FTerm::Unresolved { target } = f.block(b).term else {
        return None;
    };
    let inside = |t: u64| t >= extent.0 && t < extent.1 && code.is_code(t);
    let eval_at = |x: Value, k: u64| -> Option<(u64, Vec<(u64, u32)>, bool)> {
        let mut ev = Eval {
            defs,
            code,
            memo: HashMap::new(),
            loads: Vec::new(),
            writable: false,
            assumed: false,
        };
        ev.memo.insert(x, Some(k));
        let t = ev.eval(target, 0)?;
        Some((t, ev.loads, ev.writable))
    };
    // The index: the nearest value feeding the target that is not a
    // constant and makes the target a table read at indexes 0 and 1.
    let mut seen = std::collections::HashSet::new();
    let mut work = std::collections::VecDeque::from([target]);
    let mut index = None;
    while let Some(v) = work.pop_front() {
        if !seen.insert(v) || seen.len() > 64 {
            continue;
        }
        if v != target && fold(defs, v, 0).is_none() {
            if let (Some((t0, l0, w0)), Some((t1, l1, w1))) = (eval_at(v, 0), eval_at(v, 1)) {
                let moved = l0.iter().zip(&l1).find(|(a, b)| a.0 != b.0).map(|(a, b)| (a.0, b.0.wrapping_sub(a.0)));
                if let Some((base, stride @ (1 | 2 | 4 | 8))) = moved {
                    if inside(t0) && inside(t1) && !w0 && !w1 {
                        index = Some((v, base, stride));
                        break;
                    }
                }
            }
        }
        if let Some(e) = defs.expr.get(&v) {
            e.for_each_use(|u| work.push_back(u));
        }
    }
    let (x, base, stride) = index?;
    let mut cases = Vec::new();
    for k in 0..max as u64 {
        let Some((t, loads, writable)) = eval_at(x, k) else { break };
        // Still the same table: its entry moves by the stride.
        let entry = base.wrapping_add(k * stride);
        if writable || !inside(t) || !loads.iter().any(|l| l.0 == entry) {
            break;
        }
        cases.push(t);
    }
    if cases.len() < 2 {
        return None;
    }
    let index_reg = match defs.expr.get(&x) {
        Some(Expr::ReadReg(r)) => Some(*r),
        _ => None,
    };
    let count = cases.len() as u32;
    Some(Table {
        cases,
        table: JumpTable {
            base,
            entry_bytes: stride as u32,
            count,
            index: index_reg,
            complete: false,
        },
    })
}
