//! Block-local GIR optimisation passes: [`const_fold`], [`dead_store_elim`]
//! (mostly dead-flag elimination), and [`dce`].
//!
//! Conservative at block boundaries: guest state is assumed live-out.

use crate::*;

/// Run the standard block-local pipeline.
pub fn optimize(block: &mut Block) {
    const_fold(block);
    dead_store_elim(block);
    dce(block);
}

/// The constant a value holds, if the block computed one for it.
pub fn const_value(block: &Block, v: Value) -> Option<u64> {
    block.stmts.iter().find_map(|s| match s {
        Stmt::Def {
            dst,
            expr: Expr::ConstI64(c),
        } if *dst == v => Some(*c),
        Stmt::Def {
            dst,
            expr: Expr::ConstI32(c),
        } if *dst == v => Some(*c as u64),
        _ => None,
    })
}

/// Constant-fold with an oracle that can read initialised guest memory.
///
/// Makes dynamically linked code discoverable: a PLT `br x17` becomes a
/// constant once the GOT slot it loads is read. Reading a slot that later
/// changes only wastes a translation; the dispatch table decides the real
/// target (PLAN §6.1).
pub fn const_fold_with_memory(block: &mut Block, read: &dyn Fn(u64, u32) -> Option<u64>) {
    let mut known: Vec<Option<u64>> = vec![None; block.n_values as usize];
    for stmt in &mut block.stmts {
        let Stmt::Def { dst, expr } = stmt else {
            continue;
        };
        // A plain unsigned load from a known address: the one case `eval`
        // cannot handle on its own.
        let folded = if let Expr::Load {
            addr,
            offset,
            sz,
            signed: false,
            order: Order::Relaxed,
            ..
        } = expr
        {
            get(&known, *addr).and_then(|base| read(base.wrapping_add(*offset as u64), sz.bytes()))
        } else {
            eval(expr, &known)
        };
        if let Some(v) = folded {
            let ty = expr.ty();
            *expr = match ty {
                Ty::I32 => Expr::ConstI32(v as u32),
                _ => Expr::ConstI64(v),
            };
            known[dst.0 as usize] = Some(v & ty.mask());
        } else if let Expr::ConstI32(c) = *expr {
            known[dst.0 as usize] = Some(c as u64);
        } else if let Expr::ConstI64(c) = *expr {
            known[dst.0 as usize] = Some(c);
        }
    }
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
                // Vector expressions never fold to a scalar, so `eval` cannot
                // return one for them.
                Ty::V128 => unreachable!("vector expression folded to a scalar"),
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

/// Evaluate a pure scalar expression given the constant value of each
/// operand (`known[v]`), for analyses outside this module. `None` for an
/// expression that is not pure scalar arithmetic or has an unknown operand.
/// Constants themselves evaluate to their value.
pub fn eval_expr(expr: &Expr, known: &[Option<u64>]) -> Option<u64> {
    match *expr {
        Expr::ConstI32(c) => Some(c as u64),
        Expr::ConstI64(c) => Some(c),
        _ => eval(expr, known),
    }
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
            let v = if c != 0 {
                get(known, a)?
            } else {
                get(known, b)?
            };
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
            Ty::I64 | Ty::V128 => ((a as i64) >> sh) as u64,
        },
        BinOp::Rotr => match ty {
            Ty::I32 => (a as u32).rotate_right(sh) as u64,
            Ty::I64 | Ty::V128 => a.rotate_right(sh),
        },
        // Left to run time: reproducing wasm's trap behaviour isn't worth it,
        // and the lifter's guards make the folded form unreachable anyway.
        BinOp::DivU | BinOp::DivS | BinOp::RemU | BinOp::RemS => return None,
    };
    Some(v & m)
}

fn eval_un(op: UnOp, ty: Ty, a: u64) -> u64 {
    let m = ty.mask();
    let a = a & m;
    match (op, ty) {
        (UnOp::Clz, Ty::I32) => (a as u32).leading_zeros() as u64,
        (UnOp::Clz, _) => a.leading_zeros() as u64,
        (UnOp::Ctz, Ty::I32) => (a as u32).trailing_zeros() as u64,
        (UnOp::Ctz, _) => a.trailing_zeros() as u64,
        (UnOp::Popcnt, Ty::I32) => (a as u32).count_ones() as u64,
        (UnOp::Popcnt, _) => a.count_ones() as u64,
        (UnOp::Not, _) => !a,
        (UnOp::Neg, _) => 0u64.wrapping_sub(a),
        (UnOp::Bswap, Ty::I32) => (a as u32).swap_bytes() as u64,
        (UnOp::Bswap, _) => a.swap_bytes(),
        (UnOp::Bswap16, _) => (a as u16).swap_bytes() as u64,
    }
}

fn eval_cmp(op: CmpOp, ty: Ty, a: u64, b: u64) -> u32 {
    let m = ty.mask();
    let (au, bu) = (a & m, b & m);
    let (as_, bs) = match ty {
        Ty::I32 => (au as u32 as i32 as i64, bu as u32 as i32 as i64),
        Ty::I64 | Ty::V128 => (au as i64, bu as i64),
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
// Flag liveness
// ---------------------------------------------------------------------------

/// AArch64's and x86-64's condition flags, as a bitmask.
pub const ALL_FLAGS: u64 = 0b111_1111;

/// PowerPC's flags in the same mask: the 32 CR bits (bits 8..40) and XER's
/// SO, OV and CA (bits 40..43). Kept apart from [`ALL_FLAGS`] so the other
/// guests' masks are what they always were.
pub const PPC_FLAGS: u64 = (0xffff_ffff << 8) | (0b111 << 40);

/// Bit position of a flag in an [`ALL_FLAGS`]/[`PPC_FLAGS`] mask.
fn flag_bit(r: GuestReg) -> Option<u64> {
    match r {
        GuestReg::N => Some(1),
        GuestReg::Z => Some(2),
        GuestReg::C => Some(4),
        GuestReg::V => Some(8),
        GuestReg::P => Some(16),
        GuestReg::A => Some(32),
        GuestReg::D => Some(64),
        GuestReg::Cr(i) => Some(1 << (8 + i as u64)),
        GuestReg::So => Some(1 << 40),
        GuestReg::Ov => Some(1 << 41),
        GuestReg::Ca => Some(1 << 42),
        _ => None,
    }
}

/// Turn a mask back into the registers it names.
pub fn flags_of_mask(mask: u64) -> Vec<GuestReg> {
    [
        GuestReg::N,
        GuestReg::Z,
        GuestReg::C,
        GuestReg::V,
        GuestReg::P,
        GuestReg::A,
        GuestReg::D,
    ]
    .into_iter()
    .chain((0..32).map(GuestReg::Cr))
    .chain([GuestReg::So, GuestReg::Ov, GuestReg::Ca])
    .filter(|r| mask & flag_bit(*r).unwrap() != 0)
    .collect()
}

/// A block's effect on the flags: which it reads before writing (`use`), and
/// which it writes (`def`), the sets of a backwards liveness analysis. A
/// straight-line block always performs every write, so all count toward `def`.
pub fn flag_effects(block: &Block) -> (u64, u64) {
    let (mut uses, mut defs) = (0u64, 0u64);
    for s in &block.stmts {
        match s {
            Stmt::Def {
                expr: Expr::ReadReg(r),
                ..
            } => {
                if let Some(b) = flag_bit(*r) {
                    // Only a read that reaches past the block's top is a use.
                    if defs & b == 0 {
                        uses |= b;
                    }
                }
            }
            Stmt::WriteReg { reg, .. } => {
                if let Some(b) = flag_bit(*reg) {
                    defs |= b;
                }
            }
            // The kernel reads the whole context, flags included.
            Stmt::HostCall { func, .. }
            | Stmt::Def {
                expr: Expr::HostCall { func, .. },
                ..
            } if func.clobbers_guest_state() => {
                uses = ALL_FLAGS | PPC_FLAGS;
            }
            // A fault hands the whole state to the host: every flag not yet
            // written in this block is read from the block's entry.
            Stmt::Trap { .. } => {
                uses |= (ALL_FLAGS | PPC_FLAGS) & !defs;
            }
            _ => {}
        }
    }
    (uses, defs)
}

/// Whether a block's terminator can reach code this analysis cannot see, so
/// everything must be assumed live across it.
pub fn escapes(block: &Block) -> bool {
    matches!(
        block.term,
        Term::Indirect { .. } | Term::Illegal { .. } | Term::Syscall { .. }
    )
}

// ---------------------------------------------------------------------------
// Dead guest-register store elimination
// ---------------------------------------------------------------------------

/// Remove writes to guest registers provably overwritten before any read,
/// within this block. Mostly dead-flag elimination.
///
/// Conservative: every register is live at the terminator (a successor may
/// read it), and a host call that clobbers guest state reads everything.
pub fn dead_store_elim(block: &mut Block) {
    dead_store_elim_with(block, &[]);
}

/// `dead_store_elim`, told which registers are already dead at the block's
/// exit — which cross-block analysis can supply once the CFG is known (for
/// the flags, usually "nothing reads them"). See [`flag_effects`].
pub fn dead_store_elim_with(block: &mut Block, dead_out: &[GuestReg]) {
    // Walk backwards tracking registers known to be overwritten later. A
    // register dead at exit behaves like one overwritten past the terminator.
    let mut overwritten: Vec<GuestReg> = dead_out.to_vec();
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
            Stmt::HostCall { func, .. } if func.clobbers_guest_state() => {
                overwritten.clear();
            }
            // A register written before a fault is observable in it.
            Stmt::Trap { .. } => overwritten.clear(),
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

    /// A register written before a `Trap` is part of the state the fault
    /// reports, so it survives even when a later write would kill it; and
    /// the flags the block has not yet written count as read there.
    #[test]
    fn a_trap_reads_the_whole_state() {
        let mut b = Builder::new(0x1000);
        let k = b.const_i64(7);
        b.write(GuestReg::X(2), k);
        let c = b.const_i32(1);
        b.stmt(Stmt::Trap { cond: c, pc: 0x1000, code: -4 });
        let k2 = b.const_i64(8);
        b.write(GuestReg::X(2), k2);
        let mut block = b.finish(Term::Goto { target: 0x1004 });
        dead_store_elim(&mut block);
        let writes = block
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::X(2), .. }))
            .count();
        assert_eq!(writes, 2, "{}", crate::print::block(&block));
        let (uses, _) = flag_effects(&block);
        assert_eq!(uses & ALL_FLAGS, ALL_FLAGS);
    }

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
    fn dead_out_flags_are_dropped_but_live_ones_are_not() {
        // The caller has established that N is dead past this block and Z is
        // not; only N's write may go.
        let mut b = Builder::new(0);
        let one = b.const_i32(1);
        b.write(GuestReg::Z, one);
        b.write(GuestReg::N, one);
        b.write(GuestReg::C, one);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dead_store_elim_with(&mut blk, &[GuestReg::N, GuestReg::C]);
        let writes: Vec<_> = blk
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::WriteReg { reg, .. } => Some(*reg),
                _ => None,
            })
            .collect();
        assert_eq!(writes, vec![GuestReg::Z]);
    }

    #[test]
    fn a_dead_out_flag_read_earlier_in_the_block_still_survives() {
        // `adcs` reads C then writes it. With C dead on exit the write is
        // dead, but the read of the incoming C must survive.
        let mut b = Builder::new(0);
        let c = b.read(GuestReg::C);
        b.write(GuestReg::X(0), c);
        b.write(GuestReg::C, c);
        let mut blk = b.finish(Term::Goto { target: 4 });
        dead_store_elim_with(&mut blk, &[GuestReg::C]);
        // The write is genuinely dead; the read is not, and must remain.
        assert!(blk.stmts.iter().any(|s| matches!(
            s,
            Stmt::Def {
                expr: Expr::ReadReg(GuestReg::C),
                ..
            }
        )));
        assert!(!blk.stmts.iter().any(|s| matches!(
            s,
            Stmt::WriteReg {
                reg: GuestReg::C,
                ..
            }
        )));
    }

    #[test]
    fn flag_effects_separates_use_from_def() {
        // Reads Z before writing it (a use), writes N without reading it
        // (a def only), and reads C only after writing C (not a use).
        let mut b = Builder::new(0);
        let z = b.read(GuestReg::Z);
        b.write(GuestReg::X(0), z);
        b.write(GuestReg::N, z);
        b.write(GuestReg::C, z);
        let c = b.read(GuestReg::C);
        b.write(GuestReg::X(1), c);
        b.write(GuestReg::Z, z);
        let blk = b.finish(Term::Goto { target: 4 });
        let (uses, defs) = flag_effects(&blk);
        assert_eq!(flags_of_mask(uses), vec![GuestReg::Z]);
        assert_eq!(
            flags_of_mask(defs),
            vec![GuestReg::N, GuestReg::Z, GuestReg::C]
        );
    }

    #[test]
    fn a_block_that_leaves_the_graph_is_marked_escaping() {
        let mut b = Builder::new(0);
        let t = b.const_i64(0x1000);
        let blk = b.finish(Term::Indirect {
            target: t,
            is_ret: true,
        });
        assert!(escapes(&blk));

        let b2 = Builder::new(0);
        assert!(!escapes(&b2.finish(Term::Goto { target: 4 })));
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
            .filter(|s| {
                matches!(
                    s,
                    Stmt::WriteReg {
                        reg: GuestReg::Z,
                        ..
                    }
                )
            })
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
