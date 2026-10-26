//! Assembling decoded instructions into a GIR basic block.

use crate::{FlagSource, Flow, Lifter, INST_BYTES};
use gex_ir::{pass, Block, Builder, Term};
use gex_isa::decode;

/// Bounds on how much a single block may cover.
#[derive(Copy, Clone, Debug)]
pub struct BlockLimits {
    /// Stop and fall through after this many instructions, so no single wasm
    /// function grows pathologically large.
    pub max_insts: usize,
}

impl Default for BlockLimits {
    fn default() -> Self {
        BlockLimits { max_insts: 256 }
    }
}

/// Lift the basic block starting at `start`.
///
/// `fetch` returns the instruction word at a guest address, or `None` when it
/// is not backed by translatable memory; an unfetchable address ends the block
/// with [`Term::Illegal`] rather than failing the build.
pub fn lift_block(fetch: impl Fn(u64) -> Option<u32>, start: u64, limits: BlockLimits) -> Block {
    let mut builder = Builder::new(start);
    let mut pc = start;
    let mut term = None;
    // A block's predecessor is unknown here, so the flags start unknown.
    let mut flags = FlagSource::Unknown;

    for _ in 0..limits.max_insts {
        let Some(raw) = fetch(pc) else {
            term = Some(Term::Illegal { raw: 0, pc });
            break;
        };
        let inst = decode(raw, pc);
        let mark = builder.mark();
        let mut lifter = Lifter::new(&mut builder).with_raw(raw).with_flags(flags);
        let flow = lifter.lift(&inst, pc);
        flags = lifter.flags();
        // An instruction with no wasm form is undone and becomes a SIGILL,
        // rather than panicking the backend: exhaustive discovery lifts
        // padding and literal pools a program would never execute.
        if builder.has_inexpressible(mark) {
            builder.rewind(mark);
            term = Some(Term::Illegal { raw, pc });
            break;
        }
        pc += INST_BYTES;
        builder.end_at(pc);
        match flow {
            Flow::Next => {}
            Flow::End(t) => {
                term = Some(t);
                break;
            }
        }
    }

    // Running out of budget is not an error: fall through to the next
    // instruction, which becomes its own block.
    let term = term.unwrap_or(Term::Goto { target: pc });
    let mut block = builder.finish(term);
    pass::optimize(&mut block);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use gex_ir::{print, GuestReg, Stmt};

    /// `cmp x0, x1` then `b.<cc>`: nothing should read N/Z/C/V afterwards,
    /// since the branch asks about x0 and x1 directly. MI, PL, VS and VC are
    /// absent on purpose -- they have no direct form (see `cond_direct`).
    #[test]
    fn compare_and_branch_does_not_read_the_flags() {
        for cc in [
            0b0000u32, 0b0001, 0b0010, 0b0011, 0b1000, 0b1001, 0b1010, 0b1011, 0b1100, 0b1101,
        ] {
            let code = vec![0xeb01001f_u32, 0x54000040 | cc];
            let base = 0x1000u64;
            let fetch = |pc: u64| -> Option<u32> {
                if pc < base {
                    return None;
                }
                code.get(((pc - base) / 4) as usize).copied()
            };
            let block = lift_block(fetch, base, BlockLimits::default());
            let reads_flag = block.stmts.iter().any(|s| {
                matches!(
                    s,
                    Stmt::Def {
                        expr: gex_ir::Expr::ReadReg(
                            GuestReg::N | GuestReg::Z | GuestReg::C | GuestReg::V
                        ),
                        ..
                    }
                )
            });
            assert!(!reads_flag, "condition {cc:#06b} still reads a flag");
        }
    }

    /// The same, for the logical-op rule: `ands x0, x0, x1` sets N and Z from
    /// the result and clears C and V, so `b.eq` is `result == 0`.
    #[test]
    fn logical_and_branch_does_not_read_the_flags() {
        let code = vec![0xea010000_u32, 0x54000040];
        let base = 0x1000u64;
        let fetch = |pc: u64| -> Option<u32> {
            if pc < base {
                return None;
            }
            code.get(((pc - base) / 4) as usize).copied()
        };
        let block = lift_block(fetch, base, BlockLimits::default());
        assert!(!block.stmts.iter().any(|s| {
            matches!(
                s,
                Stmt::Def {
                    expr: gex_ir::Expr::ReadReg(
                        GuestReg::N | GuestReg::Z | GuestReg::C | GuestReg::V
                    ),
                    ..
                }
            )
        }));
    }

    /// Build a fetch closure over a slice of instruction words at `base`.
    fn words(base: u64, code: &'static [u32]) -> impl Fn(u64) -> Option<u32> {
        move |pc| {
            if pc < base {
                return None;
            }
            let idx = ((pc - base) / 4) as usize;
            code.get(idx).copied()
        }
    }

    #[test]
    fn straight_line_ends_at_branch() {
        // mov x0, #1 ; mov x1, #2 ; ret
        let b = lift_block(
            words(0x1000, &[0xd2800020, 0xd2800041, 0xd65f03c0]),
            0x1000,
            BlockLimits::default(),
        );
        assert_eq!(b.start, 0x1000);
        assert_eq!(b.end, 0x100c);
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
    }

    #[test]
    fn conditional_branch_records_both_targets() {
        // cmp x0, #0 ; b.eq .+8
        let b = lift_block(
            words(0x2000, &[0xf100001f, 0x54000040]),
            0x2000,
            BlockLimits::default(),
        );
        match b.term {
            Term::Branch {
                taken, not_taken, ..
            } => {
                // The branch is the second instruction, at 0x2004.
                assert_eq!(taken, 0x200c);
                assert_eq!(not_taken, 0x2008);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bl_writes_the_return_address() {
        // bl .+16 from 0x3000
        let b = lift_block(words(0x3000, &[0x94000004]), 0x3000, BlockLimits::default());
        assert_eq!(b.term, Term::Goto { target: 0x3010 });
        let wrote_lr = b.stmts.iter().any(|s| {
            matches!(
                s,
                Stmt::WriteReg {
                    reg: GuestReg::X(30),
                    ..
                }
            )
        });
        assert!(wrote_lr, "bl must set x30:\n{}", print::block(&b));
    }

    #[test]
    fn unknown_instruction_becomes_illegal() {
        // A SIMD encoding: decoded but not yet lifted.
        let b = lift_block(words(0x4000, &[0x4ea11c20]), 0x4000, BlockLimits::default());
        assert!(matches!(b.term, Term::Illegal { .. }));
    }

    #[test]
    fn unfetchable_address_becomes_illegal() {
        let b = lift_block(|_| None, 0x5000, BlockLimits::default());
        assert_eq!(b.term, Term::Illegal { raw: 0, pc: 0x5000 });
    }

    #[test]
    fn block_splits_at_the_instruction_budget() {
        // 300 nops: longer than the default budget, no terminator in sight.
        static NOPS: [u32; 300] = [0xd503201f; 300];
        let b = lift_block(
            |pc| {
                let idx = ((pc - 0x6000) / 4) as usize;
                NOPS.get(idx).copied()
            },
            0x6000,
            BlockLimits { max_insts: 64 },
        );
        assert_eq!(b.end, 0x6000 + 64 * 4);
        assert_eq!(
            b.term,
            Term::Goto {
                target: 0x6000 + 64 * 4
            }
        );
    }

    #[test]
    fn dead_flags_are_eliminated_across_compares() {
        // Two compares in a row, then a conditional branch: only the second
        // compare's flags can be observed.
        // cmp x0,#1 ; cmp x1,#2 ; b.eq .+8
        let b = lift_block(
            words(0x7000, &[0xf100041f, 0xf100085f, 0x54000040]),
            0x7000,
            BlockLimits::default(),
        );
        let flag_writes = b
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::WriteReg { reg, .. } if reg.is_flag()))
            .count();
        // Four flags written once, not twice.
        assert_eq!(flag_writes, 4, "{}", print::block(&b));
    }
}
