//! Assembling decoded PowerPC instructions into a GIR basic block.

use crate::{Flow, Lifter};
use gex_ir::{pass, Block, Builder, Term};
use gex_isa_ppc::decode;

/// Bounds on how much a single block may cover.
#[derive(Copy, Clone, Debug)]
pub struct BlockLimits {
    /// Stop and fall through after this many instructions.
    pub max_insts: usize,
}

impl Default for BlockLimits {
    fn default() -> Self {
        BlockLimits { max_insts: 256 }
    }
}

/// Lift the basic block starting at `start`.
///
/// `fetch` returns the instruction word at a guest address (already in host
/// order), or `None` when it is not backed by translatable memory, which
/// ends the block with [`Term::Illegal`].
pub fn lift_block(fetch: impl Fn(u64) -> Option<u32>, start: u64, limits: BlockLimits) -> Block {
    let mut builder = Builder::new(start);
    let mut pc = start;
    let mut term = None;
    for _ in 0..limits.max_insts {
        let Some(raw) = fetch(pc) else {
            term = Some(Term::Illegal { raw: 0, pc });
            break;
        };
        let inst = decode(raw, pc);
        let mark = builder.mark();
        let flow = Lifter::new(&mut builder, raw).lift(&inst, pc);
        if builder.has_inexpressible(mark) {
            builder.rewind(mark);
            term = Some(Term::Illegal { raw, pc });
            break;
        }
        pc = (pc + 4) & 0xffff_ffff;
        builder.end_at(pc);
        if let Flow::End(t) = flow {
            term = Some(t);
            break;
        }
    }
    let term = term.unwrap_or(Term::Goto { target: pc });
    let mut block = builder.finish(term);
    pass::optimize(&mut block);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use gex_ir::{print, Expr, GuestReg, Stmt, UnOp};

    fn words(base: u64, code: &'static [u32]) -> impl Fn(u64) -> Option<u32> {
        move |pc| {
            if pc < base {
                return None;
            }
            code.get(((pc - base) / 4) as usize).copied()
        }
    }

    #[test]
    fn a_word_load_is_a_load_and_a_swap() {
        // lwz 3, 8(4) ; blr
        let b = lift_block(words(0x1000, &[0x8064_0008, 0x4e80_0020]), 0x1000, BlockLimits::default());
        assert!(
            b.stmts.iter().any(|s| matches!(
                s,
                Stmt::Def { expr: Expr::Load { offset: 8, .. }, .. }
            )),
            "{}",
            print::block(&b)
        );
        assert!(b.stmts.iter().any(|s| matches!(
            s,
            Stmt::Def { expr: Expr::Un { op: UnOp::Bswap, .. }, .. }
        )));
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
    }

    #[test]
    fn byte_reversed_load_does_not_swap() {
        // lwbrx 8, 4, 5 ; blr
        let b = lift_block(words(0x1000, &[0x7d04_2c2c, 0x4e80_0020]), 0x1000, BlockLimits::default());
        assert!(!b.stmts.iter().any(|s| matches!(
            s,
            Stmt::Def { expr: Expr::Un { op: UnOp::Bswap, .. }, .. }
        )));
    }

    #[test]
    fn compare_and_branch() {
        // cmpwi 3, 0 ; beq +0x44 (bc 12, 2)
        let b = lift_block(words(0x2000, &[0x2c03_0000, 0x4182_0044]), 0x2000, BlockLimits::default());
        match b.term {
            Term::Branch { taken, not_taken, .. } => {
                assert_eq!(taken, 0x2048);
                assert_eq!(not_taken, 0x2008);
            }
            ref t => panic!("{t:?}"),
        }
    }

    #[test]
    fn bl_writes_lr_and_records_the_return() {
        // bl +0x100
        let b = lift_block(words(0x3000, &[0x4800_0101]), 0x3000, BlockLimits::default());
        assert_eq!(b.term, Term::Goto { target: 0x3100 });
        assert_eq!(b.call_return, Some(0x3004));
        assert!(b.stmts.iter().any(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::Lr, .. })));
    }

    #[test]
    fn sc_is_a_syscall() {
        let b = lift_block(words(0x4000, &[0x4400_0002]), 0x4000, BlockLimits::default());
        assert_eq!(b.term, Term::Syscall { next: 0x4004 });
    }

    #[test]
    fn unknown_is_illegal() {
        // ld 3, 0(4): 64-bit only.
        let b = lift_block(words(0x5000, &[0xe864_0000]), 0x5000, BlockLimits::default());
        assert!(matches!(b.term, Term::Illegal { .. }));
    }
}
