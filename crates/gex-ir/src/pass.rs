//! Block-local GIR optimisation passes.
//!
//! These are the passes PLAN §5.4 lists as mattering most for static binary
//! translation, restricted to what is sound inside a single basic block:
//!
//! 1. [`const_fold`] — lifted semantics generate a lot of constant
//!    arithmetic (shifted immediates, sign-extension chains, flag constants).
//! 2. [`dead_store_elim`] — kills redundant guest-register writes, which for
//!    AArch64 is overwhelmingly *dead flag elimination*: a run of `cmp`s and
//!    `adds`es where only the last one's NZCV is observed.
//! 3. [`dce`] — drops the now-unused expressions feeding those stores.
//!
//! Everything here is conservative at block boundaries: guest state is
//! assumed live-out. Cross-block flag liveness is a tier-2 concern.

use crate::*;

/// Run the standard block-local pipeline.
pub fn optimize(block: &mut Block) {
    const_fold(block);
    dead_store_elim(block);
    dce(block);
}

// ---------------------------------------------------------------------------
// Constant folding
// ---------------------------------------------------------------------------

/// Replace expressions whose operands are all constants with the constant they
/// evaluate to.
pub fn const_fold(block: &mut Block) {
    // `known[v]` is the constant value of `v`, normalised so that an `I32`
    // value has zero high bits.
    let mut known: Vec<Option<u64>> = vec![None; block.n_values as usize];

    for stmt in &mut block.stmts {
        let Stmt::Def { dst, expr } = stmt else {
            continue;
        };
        if let Some(folded) = eval(expr, &known) {
            let ty = expr.ty();
            *expr = match ty {
                Ty::I32 => Expr::ConstI32(folded as u32),
                Ty::I64 => Expr::ConstI64(folded),
            };
            known[dst.0 as usize] = Some(folded & ty.mask());
        } else if let Expr::ConstI32(c) = *expr {
            known[dst.0 as usize] = Some(c as u64);
        } else if let Expr::ConstI64(c) = *expr {
            known[dst.0 as usize] = Some(c);
        }
    }
}

fn get(known: &[Option<u64>], v: Value) -> Option<u64> {
    *known.get(v.0 as usize)?
}

fn eval(expr: &Expr, known: &[Option<u64>]) -> Option<u64> {
    match *expr {
        Expr::ConstI32(_) | Expr::ConstI64(_) => None, // already constant
        Expr::Bin { op, ty, a, b } => {
            let (a, b) = (get(known, a)?, get(known, b)?);
            eval_bin(op, ty, a, b)
        }
        Expr::Un { op, ty, a } => {
            let a = get(known, a)?;
            Some(eval_un(op, ty, a) & ty.mask())
        }
        Expr::Cmp { op, ty, a, b } => {
            let (a, b) = (get(known, a)?, get(known, b)?);
            Some(eval_cmp(op, ty, a, b) as u64)
        }
        Expr::Select { cond, a, b, ty } => {
            let c = get(known, cond)?;
            let v = if c != 0 { get(known, a)? } else { get(known, b)? };
            Some(v & ty.mask())
        }
        Expr::Wrap(a) => Some(get(known, a)? & 0xffff_ffff),
        Expr::Extend { a, signed } => {
            let a = get(known, a)? as u32;
            Some(if signed {
                a as i32 as i64 as u64
            } else {
                a as u64
            })
        }
        Expr::ExtendBits {
            a,
            ty,
            bits,
            signed,
        } => {
            let a = get(known, a)?;
            Some(extend_bits(a, bits, signed) & ty.mask())
        }
        _ => None,
    }
}

fn extend_bits(v: u64, bits: u32, signed: bool) -> u64 {
    if bits >= 64 {
        return v;
    }
    let masked = v & ((1u64 << bits) - 1);
    if signed {
        let shift = 64 - bits;
        (((masked << shift) as i64) >> shift) as u64
    } else {
        masked
    }
}

fn eval_bin(op: BinOp, ty: Ty, a: u64, b: u64) -> Option<u64> {
    let m = ty.mask();
    let bits = ty.bits();
    let (a, b) = (a & m, b & m);
    // Shifts mask their amount to the operand width, matching wasm and the
    // AArch64 variable-shift instructions.
    let sh = (b & (bits as u64 - 1)) as u32;
    let v = match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::And => a & b,
        BinOp::Or => a | b,
        BinOp::Xor => a ^ b,
        BinOp::Shl => a.wrapping_shl(sh),
        BinOp::ShrU => (a & m).wrapping_shr(sh),
        BinOp::ShrS => match ty {
            Ty::I32 => ((a as u32 as i32) >> sh) as u32 as u64,
            Ty::I64 => ((a as i64) >> sh) as u64,
        },
        BinOp::Rotr => match ty {
            Ty::I32 => (a as u32).rotate_right(sh) as u64,
            Ty::I64 => a.rotate_right(sh),
        },
        // Division is left to run time: folding it here would have to
        // reproduce wasm's trapping behaviour exactly, and the lifter has
        // already inserted guards that make the folded form unreachable.
        BinOp::DivU | BinOp::DivS | BinOp::RemU | BinOp::RemS => return None,
    };
    Some(v & m)
}

fn eval_un(op: UnOp, ty: Ty, a: u64) -> u64 {
    let m = ty.mask();
    let a = a & m;
    match (op, ty) {
        (UnOp::Clz, Ty::I32) => (a as u32).leading_zeros() as u64,
        (UnOp::Clz, Ty::I64) => a.leading_zeros() as u64,
        (UnOp::Ctz, Ty::I32) => (a as u32).trailing_zeros() as u64,
        (UnOp::Ctz, Ty::I64) => a.trailing_zeros() as u64,
        (UnOp::Popcnt, Ty::I32) => (a as u32).count_ones() as u64,
        (UnOp::Popcnt, Ty::I64) => a.count_ones() as u64,
        (UnOp::Not, _) => !a,
        (UnOp::Neg, _) => 0u64.wrapping_sub(a),
    }
}

fn eval_cmp(op: CmpOp, ty: Ty, a: u64, b: u64) -> u32 {
    let m = ty.mask();
    let (au, bu) = (a & m, b & m);
    let (as_, bs) = match ty {
        Ty::I32 => (au as u32 as i32 as i64, bu as u32 as i32 as i64),
        Ty::I64 => (au as i64, bu as i64),
    };
    let r = match op {
        CmpOp::Eq => au == bu,
        CmpOp::Ne => au != bu,
        CmpOp::LtU => au < bu,
        CmpOp::LeU => au <= bu,
        CmpOp::GtU => au > bu,
        CmpOp::GeU => au >= bu,
        CmpOp::LtS => as_ < bs,
        CmpOp::LeS => as_ <= bs,
        CmpOp::GtS => as_ > bs,
        CmpOp::GeS => as_ >= bs,
    };
    r as u32
}

// ---------------------------------------------------------------------------
// Dead guest-register store elimination
// ---------------------------------------------------------------------------

/// Remove writes to guest registers that are provably overwritten before any
/// read, within this block.
///
/// This is where dead-flag elimination happens. A sequence like
/// `cmp x0,#1 ; cmp x1,#2 ; b.eq` writes NZCV twice; only the second write is
/// observable, and dropping the first lets [`dce`] delete the whole first
/// flag computation.
///
/// Conservative rules:
/// * every register is live at the block terminator, since a successor may
///   read it;
/// * a host call that clobbers guest state is treated as reading everything,
///   because the kernel inspects the context.
pub fn dead_store_elim(block: &mut Block) {
    // Walk backwards, tracking which registers are already known to be
    // overwritten later in the block.
    let mut overwritten: Vec<GuestReg> = Vec::new();
    let mut drop_at: Vec<bool> = vec![false; block.stmts.len()];

    for (i, stmt) in block.stmts.iter().enumerate().rev() {
        match stmt {
            Stmt::WriteReg { reg, .. } => {
                if overwritten.contains(reg) {
                    drop_at[i] = true;
                } else {
                    overwritten.push(*reg);
                }
            }
            Stmt::Def { expr, .. } => {
                if let Expr::ReadReg(r) = expr {
                    overwritten.retain(|x| x != r);
                }
                if let Expr::HostCall { func, .. } = expr {
                    if func.clobbers_guest_state() {
                        overwritten.clear();
                    }
                }
            }
            Stmt::HostCall { func, .. } => {
                if func.clobbers_guest_state() {
                    overwritten.clear();
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    block.stmts.retain(|_| {
        let keep = !drop_at[i];
        i += 1;
        keep
    });
}

// ---------------------------------------------------------------------------
// Dead code elimination
// ---------------------------------------------------------------------------

/// Delete pure definitions whose values are never used.
pub fn dce(block: &mut Block) {
    let mut live = vec![false; block.n_values as usize];
    block.term_uses(|v| {
        if let Some(slot) = live.get_mut(v.0 as usize) {
            *slot = true;
        }
    });

    let mut drop_at = vec![false; block.stmts.len()];
    for (i, stmt) in block.stmts.iter().enumerate().rev() {
        let keep = match stmt {
            Stmt::Def { dst, expr } => {
                !expr.is_pure() || live.get(dst.0 as usize).copied().unwrap_or(true)
            }
            _ => true,
        };
        if keep {
            stmt.for_each_use(|v| {
                if let Some(slot) = live.get_mut(v.0 as usize) {
                    *slot = true;
                }
            });
        } else {
            drop_at[i] = true;
        }
    }

    let mut i = 0;
    block.stmts.retain(|_| {
        let keep = !drop_at[i];
        i += 1;
        keep
    });
}

/// Statistics used by the perf work in PLAN §13.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub stmts: usize,
    pub reg_writes: usize,
    pub flag_writes: usize,
    pub loads: usize,
    pub stores: usize,
}

pub fn stats(block: &Block) -> Stats {
    let mut s = Stats {
        stmts: block.stmts.len(),
        ..Default::default()
    };
    for stmt in &block.stmts {
        match stmt {
            Stmt::WriteReg { reg, .. } => {
                s.reg_writes += 1;
                if reg.is_flag() {
                    s.flag_writes += 1;
                }
            }
            Stmt::Store { .. } => s.stores += 1,
            Stmt::Def { expr, .. } => {
                if matches!(expr, Expr::Load { .. }) {
                    s.loads += 1;
                }
            }
            _ => {}
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_constant_arithmetic() {
        let mut b = Builder::new(0x1000);
        let a = b.const_i64(7);
        let c = b.const_i64(35);
        let sum = b.bin(BinOp::Mul, Ty::I64, a, c);
        b.write(GuestReg::X(0), sum);
        let mut blk = b.finish(Term::Goto { target: 0x1004 });
        const_fold(&mut blk);
        let Stmt::Def { expr, .. } = &blk.stmts[2] else {
            panic!()
        };
        assert_eq!(*expr, Expr::ConstI64(245));
    }

    #[test]
    fn folds_32bit_with_wraparound() {
        let mut b = Builder::new(0);
        let a = b.const_i32(0xffff_ffff);
        let one = b.const_i32(1);
        let sum = b.bin(BinOp::Add, Ty::I32, a, one);
        b.write(GuestReg::X(1), sum);
        let mut blk = b.finish(Term::Goto { target: 4 });
        const_fold(&mut blk);
        let Stmt::Def { expr, .. } = &blk.stmts[2] else {
            panic!()
        };
        assert_eq!(*expr, Expr::ConstI32(0));
    }

    #[test]
    fn kills_dead_flag_writes() {
        // Two flag-setting compares in a row; only the second is observable.
        let mut b = Builder::new(0);
        let zero = b.const_i32(0);
        let one = b.const_i32(1);
        b.write(GuestReg::Z, zero);
        b.write(GuestReg::N, zero);
        b.write(GuestReg::Z, one);
        b.write(GuestReg::N, one);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dead_store_elim(&mut blk);
        let writes: Vec<_> = blk
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::WriteReg { reg, val } => Some((*reg, *val)),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![(GuestReg::Z, one), (GuestReg::N, one)]);
    }

    #[test]
    fn keeps_flag_write_read_before_overwrite() {
        let mut b = Builder::new(0);
        let zero = b.const_i32(0);
        let one = b.const_i32(1);
        b.write(GuestReg::Z, zero);
        let read = b.read(GuestReg::Z);
        b.write(GuestReg::X(0), read);
        b.write(GuestReg::Z, one);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dead_store_elim(&mut blk);
        let n_z_writes = blk
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::Z, .. }))
            .count();
        assert_eq!(n_z_writes, 2);
    }

    #[test]
    fn host_call_keeps_earlier_writes_alive() {
        let mut b = Builder::new(0);
        let zero = b.const_i64(0);
        let one = b.const_i64(1);
        b.write(GuestReg::X(0), zero);
        b.stmt(Stmt::HostCall {
            func: HostFunc::Syscall,
            args: vec![],
        });
        b.write(GuestReg::X(0), one);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dead_store_elim(&mut blk);
        assert_eq!(
            blk.stmts
                .iter()
                .filter(|s| matches!(s, Stmt::WriteReg { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn dce_removes_unused_pure_defs() {
        let mut b = Builder::new(0);
        let _unused = b.bin(BinOp::Add, Ty::I64, Value(9999), Value(9999));
        let used = b.const_i64(5);
        b.write(GuestReg::X(0), used);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dce(&mut blk);
        assert_eq!(blk.stmts.len(), 2);
    }

    #[test]
    fn dce_keeps_loads() {
        let mut b = Builder::new(0);
        let addr = b.const_i64(0x1000);
        let _v = b.def(Expr::Load {
            addr,
            offset: 0,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        });
        let mut blk = b.finish(Term::Goto { target: 4 });
        dce(&mut blk);
        assert_eq!(blk.stmts.len(), 2);
    }
}
