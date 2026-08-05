//! Assembling decoded instructions into a GIR basic block.

use crate::{Flow, Lifter, INST_BYTES};
use gex_ir::{pass, Block, Builder, Term};
use gex_isa::decode;

/// Bounds on how much a single block may cover.
#[derive(Copy, Clone, Debug)]
pub struct BlockLimits {
    /// Stop and fall through after this many instructions.
    ///
    /// Long straight-line runs are split so that no single wasm function
    /// becomes pathologically large, which matters both for engine
    /// compilation time and for the per-module function-count budget
    /// (PLAN §9).
    pub max_insts: usize,
}

impl Default for BlockLimits {
    fn default() -> Self {
        BlockLimits { max_insts: 256 }
    }
}

/// Lift the basic block starting at `start`.
///
/// `fetch` returns the instruction word at a guest address, or `None` when the
/// address is not backed by translatable memory. An unfetchable address ends
/// the block with [`Term::Illegal`] rather than failing the build: correctness
/// never depends on translation succeeding ahead of time (PLAN §6.1).
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
        let mut lifter = Lifter::new(&mut builder);
        let flow = lifter.lift(&inst, pc);
        // An instruction the backend has no wasm form for is undone and
        // becomes a SIGILL here, where the preceding instructions have
        // already run. Exhaustive discovery decodes literal pools and
        // padding, so `sqadd v0.2d` -- which the architecture defines and
        // wasm SIMD has no instruction for -- turns up in programs that would
        // never execute it, and a panic in the backend would refuse to build
        // them at all.
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
