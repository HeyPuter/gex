//! Assembling decoded x86-64 instructions into a GIR basic block.

use crate::{FlagSource, Flow, Lifter};
use gex_ir::{pass, Block, Builder, Term};
use gex_isa_x86::{decode, MAX_INST_BYTES};

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
/// `fetch(pc, buf)` copies the bytes of executable memory at `pc` into `buf`
/// and returns how many it could provide (up to 16; an x86 instruction is at
/// most 15). Zero means the address is not backed by translatable memory, and
/// the block ends there with [`Term::Illegal`].
pub fn lift_block(
    fetch: impl Fn(u64, &mut [u8; 16]) -> usize,
    start: u64,
    limits: BlockLimits,
) -> Block {
    let mut builder = Builder::new(start);
    let mut pc = start;
    let mut term = None;
    let mut flags = FlagSource::Unknown;

    for _ in 0..limits.max_insts {
        let mut buf = [0u8; 16];
        let n = fetch(pc, &mut buf).min(MAX_INST_BYTES);
        if n == 0 {
            term = Some(Term::Illegal { raw: 0, pc });
            break;
        }
        let inst = decode(&buf[..n], pc);
        let raw = u32::from_le_bytes(buf[..4].try_into().unwrap());
        let mark = builder.mark();
        let mut lifter = Lifter::new(&mut builder, flags, raw);
        let flow = lifter.lift(&inst, pc);
        flags = lifter.flags();
        if builder.has_inexpressible(mark) {
            builder.rewind(mark);
            term = Some(Term::Illegal { raw, pc });
            break;
        }
        pc += inst.len as u64;
        builder.end_at(pc);
        match flow {
            Flow::Next => {}
            Flow::End(t) => {
                term = Some(t);
                break;
            }
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
    use gex_ir::{print, Expr, GuestReg, Stmt};

    fn bytes(base: u64, code: &'static [u8]) -> impl Fn(u64, &mut [u8; 16]) -> usize {
        move |pc, buf| {
            if pc < base || pc >= base + code.len() as u64 {
                return 0;
            }
            let off = (pc - base) as usize;
            let n = (code.len() - off).min(16);
            buf[..n].copy_from_slice(&code[off..off + n]);
            n
        }
    }

    #[test]
    fn straight_line_ends_at_ret() {
        // mov eax, 1 ; mov ecx, 2 ; ret
        let b = lift_block(
            bytes(0x1000, &[0xb8, 1, 0, 0, 0, 0xb9, 2, 0, 0, 0, 0xc3]),
            0x1000,
            BlockLimits::default(),
        );
        assert_eq!(b.start, 0x1000);
        assert_eq!(b.end, 0x100b);
        assert!(matches!(b.term, Term::Indirect { is_ret: true, .. }));
    }

    #[test]
    fn compare_and_branch_does_not_read_the_flags() {
        // cmp rdi, rsi ; jb +5
        let b = lift_block(
            bytes(0x2000, &[0x48, 0x39, 0xf7, 0x72, 0x05]),
            0x2000,
            BlockLimits::default(),
        );
        match b.term {
            Term::Branch {
                taken, not_taken, ..
            } => {
                assert_eq!(taken, 0x200a);
                assert_eq!(not_taken, 0x2005);
            }
            other => panic!("{other:?}"),
        }
        let reads_flag = b.stmts.iter().any(|s| {
            matches!(
                s,
                Stmt::Def {
                    expr: Expr::ReadReg(GuestReg::N | GuestReg::Z | GuestReg::C | GuestReg::V),
                    ..
                }
            )
        });
        assert!(!reads_flag, "{}", print::block(&b));
    }

    #[test]
    fn call_writes_the_return_address_and_records_it() {
        // call +0x100 from 0x3000 (5 bytes): return 0x3005, target 0x3105
        let b = lift_block(
            bytes(0x3000, &[0xe8, 0x00, 0x01, 0x00, 0x00]),
            0x3000,
            BlockLimits::default(),
        );
        assert_eq!(b.term, Term::Goto { target: 0x3105 });
        assert_eq!(b.call_return, Some(0x3005));
        let wrote_rsp = b
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::WriteReg { reg: GuestReg::X(4), .. }));
        assert!(wrote_rsp, "call must move rsp:\n{}", print::block(&b));
    }

    #[test]
    fn syscall_terminates() {
        let b = lift_block(bytes(0x4000, &[0x0f, 0x05]), 0x4000, BlockLimits::default());
        assert_eq!(b.term, Term::Syscall { next: 0x4002 });
    }

    #[test]
    fn unknown_becomes_illegal() {
        // ud2
        let b = lift_block(bytes(0x5000, &[0x0f, 0x0b]), 0x5000, BlockLimits::default());
        assert!(matches!(b.term, Term::Illegal { .. }));
    }
}
