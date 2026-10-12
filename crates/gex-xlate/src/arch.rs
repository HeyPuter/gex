//! The guest architecture behind the translator: the decoder and lifter pair
//! the `x86` or `ppc` cargo feature selects. Everything else in this crate is written
//! against these few functions, so it never names an instruction set.

use gex_ir::Block;

/// Buffer a fetch fills: the longest x86 instruction is 15 bytes, and the A64
/// adapter reads one word from it.
pub type FetchBuf = [u8; 16];

#[cfg(not(any(feature = "x86", feature = "ppc")))]
mod imp {
    use super::*;
    pub use gex_lift::BlockLimits;

    /// Lift one block. `fetch(pc, buf)` fills `buf` with the executable bytes
    /// at `pc` and returns how many it could provide.
    pub fn lift_block(
        fetch: impl Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
    ) -> Block {
        gex_lift::lift_block(
            |pc| {
                if pc % 4 != 0 {
                    return None;
                }
                let mut buf = [0u8; 16];
                if fetch(pc, &mut buf) >= 4 {
                    Some(u32::from_le_bytes(buf[..4].try_into().unwrap()))
                } else {
                    None
                }
            },
            start,
            limits,
        )
    }

    /// Whether the bytes at `pc` begin an instruction gex can name, as
    /// opposed to padding, data or an encoding it does not know.
    pub fn decodable(bytes: &[u8], pc: u64) -> bool {
        if bytes.len() < 4 || pc % 4 != 0 {
            return false;
        }
        let raw = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        !matches!(
            gex_isa::decode(raw, pc),
            gex_isa::Inst::Unknown { .. } | gex_isa::Inst::Undefined { .. }
        )
    }

    /// Scan code for instructions that materialise a code address into a
    /// register, calling `seed` with each executable target. A function whose
    /// address is *taken* rather than called -- `adrp`+`add` -- is reached by
    /// no branch and lies in no data word, so neither successor-following nor
    /// the data scan finds it. Over-seeding is safe: a target that is really
    /// data is rejected by `is_exec`.
    pub fn scan_code_pointers(
        bytes: &[u8],
        vaddr: u64,
        is_exec: &dyn Fn(u64) -> bool,
        seed: &mut dyn FnMut(u64),
    ) {
        let mut page: [Option<u64>; 32] = [None; 32];
        let end = vaddr + bytes.len() as u64;
        let mut a = vaddr;
        while a + 4 <= end {
            if !is_exec(a) {
                page = [None; 32];
                a += 4;
                continue;
            }
            let off = (a - vaddr) as usize;
            let raw = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            match gex_isa::decode(raw, a) {
                gex_isa::Inst::Adrp {
                    rd: gex_isa::RegSel::Gp(r),
                    target,
                } => {
                    page[r as usize] = Some(target);
                }
                gex_isa::Inst::Adr { target, .. } => {
                    if target % 4 == 0 && is_exec(target) {
                        seed(target);
                    }
                }
                gex_isa::Inst::AddSubImm {
                    op: gex_isa::AddOp::Add,
                    rd: gex_isa::RegSel::Gp(rd),
                    rn: gex_isa::RegSel::Gp(rn),
                    imm,
                    ..
                } => {
                    if let Some(p) = page[rn as usize] {
                        let target = p.wrapping_add(imm);
                        if target % 4 == 0 && is_exec(target) {
                            seed(target);
                        }
                    }
                    page[rd as usize] = None;
                }
                _ => {}
            }
            a += 4;
        }
    }

    /// Every candidate instruction start in `[lo, hi)`: for A64, every aligned
    /// word that decodes. `anchors` (known instruction starts) are not needed
    /// on a fixed-width ISA.
    pub fn sweep(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        lo: u64,
        hi: u64,
        _anchors: &[u64],
        emit: &mut dyn FnMut(u64),
    ) {
        let mut a = lo & !3;
        while a < hi {
            let mut buf = [0u8; 16];
            let n = fetch(a, &mut buf);
            if n > 0 && decodable(&buf[..n], a) {
                emit(a);
            }
            a += 4;
        }
    }

    /// Lift one block for function-level GIR: like [`lift_block`], but
    /// keeping which instruction every statement came from, running no
    /// optimisation (so nothing moves between instructions), and stopping
    /// before any instruction `stop` names other than the first.
    pub fn lift_insts(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
        stop: &dyn Fn(u64) -> bool,
    ) -> Lifted {
        use gex_ir::{Builder, Term};
        use gex_lift::{FlagSource, Flow, Lifter};
        let mut b = Builder::new(start);
        let mut pcs: Vec<u64> = Vec::new();
        let mut insts: Vec<u64> = Vec::new();
        let mut pc = start;
        let mut term = None;
        let mut term_pc = start;
        let mut flags = FlagSource::Unknown;
        for _ in 0..limits.max_insts {
            if pc != start && stop(pc) {
                break;
            }
            let mut buf = [0u8; 16];
            if pc % 4 != 0 || fetch(pc, &mut buf) < 4 {
                term = Some(Term::Illegal { raw: 0, pc });
                term_pc = pc;
                break;
            }
            let raw = u32::from_le_bytes(buf[..4].try_into().unwrap());
            let inst = gex_isa::decode(raw, pc);
            let mark = b.mark();
            let mut l = Lifter::new(&mut b).with_raw(raw).with_flags(flags);
            let flow = l.lift(&inst, pc);
            flags = l.flags();
            if b.has_inexpressible(mark) {
                b.rewind(mark);
                term = Some(Term::Illegal { raw, pc });
                term_pc = pc;
                break;
            }
            pcs.resize(b.mark(), pc);
            insts.push(pc);
            term_pc = pc;
            pc += 4;
            b.end_at(pc);
            if let Flow::End(t) = flow {
                term = Some(t);
                break;
            }
        }
        let fell = term.is_none();
        let block = b.finish(term.unwrap_or(Term::Goto { target: pc }));
        pcs.truncate(block.stmts.len());
        Lifted {
            block,
            pcs,
            insts,
            term_pc: if fell { pc } else { term_pc },
        }
    }

    /// Disassemble one instruction: its text and length.
    pub fn disasm(bytes: &[u8], pc: u64) -> (String, usize) {
        if bytes.len() < 4 {
            return ("(truncated)".into(), bytes.len().max(1));
        }
        let raw = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let inst = gex_isa::decode(raw, pc);
        (
            format!("{raw:08x}\t{}", gex_isa::fmt::disasm(&inst, pc)),
            4,
        )
    }
}

#[cfg(feature = "x86")]
mod imp {
    use super::*;
    pub use gex_lift_x86::BlockLimits;

    pub fn lift_block(
        fetch: impl Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
    ) -> Block {
        gex_lift_x86::lift_block(fetch, start, limits)
    }

    pub fn decodable(bytes: &[u8], pc: u64) -> bool {
        !bytes.is_empty() && gex_isa_x86::decode(bytes, pc).is_known()
    }

    /// A linear sweep of the code: every `lea reg, [rip+disp]`, every
    /// `mov reg/mem, imm` and every `push imm` whose value lands in
    /// executable memory is an address-taken function. `push imm32` is how
    /// i386 code passes a function as an argument (a callback, a handler
    /// registered in a table: GameMaker's runner registers each builtin with
    /// `push offset fn`), and a PE without base relocations names those
    /// functions nowhere else. The sweep starts at the range's beginning and
    /// follows instruction lengths; compiler output keeps it in step, and a
    /// stretch out of step only costs spurious seeds `is_exec` rejects.
    pub fn scan_code_pointers(
        bytes: &[u8],
        vaddr: u64,
        is_exec: &dyn Fn(u64) -> bool,
        seed: &mut dyn FnMut(u64),
    ) {
        use gex_isa_x86::{Op, Operand};
        let end = vaddr + bytes.len() as u64;
        let mut a = vaddr;
        while a < end {
            if !is_exec(a) {
                a += 1;
                continue;
            }
            let off = (a - vaddr) as usize;
            let avail = (bytes.len() - off).min(16);
            let inst = gex_isa_x86::decode(&bytes[off..off + avail], a);
            match (inst.op, inst.ops[1]) {
                (Op::Lea, Some(Operand::Mem(m))) if m.rip => {
                    let t = m.disp as u64;
                    if is_exec(t) {
                        seed(t);
                    }
                }
                (Op::Mov | Op::Movabs, Some(Operand::Imm(v))) => {
                    let t = v as u64;
                    if is_exec(t) {
                        seed(t);
                    }
                }
                _ => match (inst.op, inst.ops[0]) {
                    (Op::Push, Some(Operand::Imm(v))) => {
                        // Sign-extended as decoded: an address of an i386
                        // program is its low 32 bits.
                        let t = if super::compat() { v as u32 as u64 } else { v as u64 };
                        if is_exec(t) {
                            seed(t);
                        }
                    }
                    _ => {}
                },
            }
            a += inst.len.max(1) as u64;
        }
    }

    /// Every candidate instruction start in `[lo, hi)`, by linear sweep: from
    /// each anchor (a known instruction start -- a symbol, a block discovery
    /// reached, a data pointer) decode forward, following instruction
    /// lengths, until the next anchor. Seeding *every byte* instead, as the
    /// A64 sweep does with every word, is wrong twice over on x86-64: most
    /// byte offsets fall inside an instruction and decode as garbage (libc came
    /// out at 1.2 million blocks, ten times its instruction count), and the
    /// garbage exercises lifter paths no real program has.
    pub fn sweep(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        lo: u64,
        hi: u64,
        anchors: &[u64],
        emit: &mut dyn FnMut(u64),
    ) {
        let mut starts: Vec<u64> = anchors.iter().copied().filter(|&a| a >= lo && a < hi).collect();
        starts.sort_unstable();
        starts.dedup();
        if starts.first().map_or(true, |&f| f > lo) {
            starts.insert(0, lo);
        }
        for (i, &s) in starts.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(hi);
            let mut a = s;
            while a < next {
                let mut buf = [0u8; 16];
                let n = fetch(a, &mut buf);
                if n == 0 {
                    // Not executable here: step to the next byte and try to
                    // resynchronise.
                    a += 1;
                    continue;
                }
                let inst = gex_isa_x86::decode(&buf[..n], a);
                if inst.is_known() {
                    emit(a);
                }
                a += inst.len.max(1) as u64;
            }
        }
    }

    /// See the A64 version.
    pub fn lift_insts(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
        stop: &dyn Fn(u64) -> bool,
    ) -> Lifted {
        use gex_ir::{Builder, Term};
        use gex_lift_x86::{FlagSource, Flow, Lifter};
        let mut b = Builder::new(start);
        let mut pcs: Vec<u64> = Vec::new();
        let mut insts: Vec<u64> = Vec::new();
        let mut pc = start;
        let mut term = None;
        let mut term_pc = start;
        let mut flags = FlagSource::Unknown;
        for _ in 0..limits.max_insts {
            if pc != start && stop(pc) {
                break;
            }
            let mut buf = [0u8; 16];
            let n = fetch(pc, &mut buf).min(gex_isa_x86::MAX_INST_BYTES);
            if n == 0 {
                term = Some(Term::Illegal { raw: 0, pc });
                term_pc = pc;
                break;
            }
            let inst = gex_isa_x86::decode(&buf[..n], pc);
            let raw = u32::from_le_bytes(buf[..4].try_into().unwrap());
            let mark = b.mark();
            let mut l = Lifter::new(&mut b, flags, raw);
            let flow = match (pc == start).then(|| gex_lift_x86::copy_loop(fetch, start)).flatten() {
                Some(r) => l.lift_copy_loop(r),
                None => l.lift(&inst, pc),
            };
            flags = l.flags();
            if b.has_inexpressible(mark) {
                b.rewind(mark);
                term = Some(Term::Illegal { raw, pc });
                term_pc = pc;
                break;
            }
            pcs.resize(b.mark(), pc);
            insts.push(pc);
            term_pc = pc;
            pc += inst.len as u64;
            b.end_at(pc);
            if let Flow::End(t) = flow {
                term = Some(t);
                break;
            }
        }
        let fell = term.is_none();
        let block = b.finish(term.unwrap_or(Term::Goto { target: pc }));
        pcs.truncate(block.stmts.len());
        Lifted {
            block,
            pcs,
            insts,
            term_pc: if fell { pc } else { term_pc },
        }
    }

    pub fn disasm(bytes: &[u8], pc: u64) -> (String, usize) {
        let inst = gex_isa_x86::decode(bytes, pc);
        let n = inst.len as usize;
        let hex: String = bytes[..n.min(bytes.len())]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        (
            format!("{hex:<30}\t{}", gex_isa_x86::fmt::disasm(&inst, pc)),
            n.max(1),
        )
    }
}

/// 32-bit big-endian PowerPC (docs/ppc.md): fixed 4-byte words, stored
/// big-endian in guest memory.
#[cfg(feature = "ppc")]
mod imp {
    use super::*;
    pub use gex_lift_ppc::BlockLimits;

    fn word(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(bytes[..4].try_into().unwrap())
    }

    pub fn lift_block(
        fetch: impl Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
    ) -> Block {
        gex_lift_ppc::lift_block(
            |pc| {
                if pc % 4 != 0 {
                    return None;
                }
                let mut buf = [0u8; 16];
                if fetch(pc, &mut buf) >= 4 {
                    Some(word(&buf))
                } else {
                    None
                }
            },
            start,
            limits,
        )
    }

    pub fn decodable(bytes: &[u8], pc: u64) -> bool {
        bytes.len() >= 4
            && pc % 4 == 0
            && gex_isa_ppc::decode(word(bytes), pc).op != gex_isa_ppc::Op::Unknown
    }

    /// `lis rX, hi` then `addi rY, rX, lo` (or `ori`): the absolute-address
    /// idiom of 32-bit code, which is how a function whose address is taken
    /// shows up. Over-seeding is safe; `is_exec` rejects data.
    pub fn scan_code_pointers(
        bytes: &[u8],
        vaddr: u64,
        is_exec: &dyn Fn(u64) -> bool,
        seed: &mut dyn FnMut(u64),
    ) {
        use gex_isa_ppc::Op;
        let mut hi: [Option<u32>; 32] = [None; 32];
        let end = vaddr + bytes.len() as u64;
        let mut a = vaddr;
        while a + 4 <= end {
            if !is_exec(a) {
                hi = [None; 32];
                a += 4;
                continue;
            }
            let off = (a - vaddr) as usize;
            let i = gex_isa_ppc::decode(word(&bytes[off..]), a);
            match i.op {
                Op::Addis if i.ra() == 0 => hi[i.rt() as usize] = Some((i.simm() as u32) << 16),
                Op::Addi | Op::Ori if i.ra() != 0 || i.op == Op::Ori => {
                    let (src, dst) = if i.op == Op::Ori { (i.rt(), i.ra()) } else { (i.ra(), i.rt()) };
                    if let Some(h) = hi[src as usize] {
                        let t = if i.op == Op::Ori {
                            h | i.uimm()
                        } else {
                            h.wrapping_add(i.simm() as u32)
                        } as u64;
                        if t % 4 == 0 && is_exec(t) {
                            seed(t);
                        }
                    }
                    hi[dst as usize] = None;
                }
                _ => {}
            }
            a += 4;
        }
    }

    /// Every candidate instruction start in `[lo, hi)`: every aligned word
    /// that decodes.
    pub fn sweep(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        lo: u64,
        hi: u64,
        _anchors: &[u64],
        emit: &mut dyn FnMut(u64),
    ) {
        let mut a = lo & !3;
        while a < hi {
            let mut buf = [0u8; 16];
            let n = fetch(a, &mut buf);
            if n > 0 && decodable(&buf[..n], a) {
                emit(a);
            }
            a += 4;
        }
    }

    /// See the A64 version. Each instruction is lifted with fresh lifter
    /// state, so none borrows a value another one computed. (Function
    /// recovery refuses 32-bit images today, so nothing calls this yet.)
    pub fn lift_insts(
        fetch: &dyn Fn(u64, &mut FetchBuf) -> usize,
        start: u64,
        limits: BlockLimits,
        stop: &dyn Fn(u64) -> bool,
    ) -> Lifted {
        use gex_ir::{Builder, Term};
        use gex_lift_ppc::{BlockState, Flow, Lifter};
        let mut b = Builder::new(start);
        let mut pcs: Vec<u64> = Vec::new();
        let mut insts: Vec<u64> = Vec::new();
        let mut pc = start;
        let mut term = None;
        let mut term_pc = start;
        for _ in 0..limits.max_insts {
            if pc != start && stop(pc) {
                break;
            }
            let mut buf = [0u8; 16];
            if pc % 4 != 0 || fetch(pc, &mut buf) < 4 {
                term = Some(Term::Illegal { raw: 0, pc });
                term_pc = pc;
                break;
            }
            let raw = word(&buf);
            let inst = gex_isa_ppc::decode(raw, pc);
            let mark = b.mark();
            let mut st = BlockState::default();
            let flow = Lifter::new(&mut b, &mut st, raw).lift(&inst, pc);
            if b.has_inexpressible(mark) {
                b.rewind(mark);
                term = Some(Term::Illegal { raw, pc });
                term_pc = pc;
                break;
            }
            pcs.resize(b.mark(), pc);
            insts.push(pc);
            term_pc = pc;
            pc = (pc + 4) & 0xffff_ffff;
            b.end_at(pc);
            if let Flow::End(t) = flow {
                term = Some(t);
                break;
            }
        }
        let fell = term.is_none();
        let block = b.finish(term.unwrap_or(Term::Goto { target: pc }));
        pcs.truncate(block.stmts.len());
        Lifted {
            block,
            pcs,
            insts,
            term_pc: if fell { pc } else { term_pc },
        }
    }

    pub fn disasm(bytes: &[u8], pc: u64) -> (String, usize) {
        if bytes.len() < 4 {
            return ("(truncated)".into(), bytes.len().max(1));
        }
        let raw = word(bytes);
        let inst = gex_isa_ppc::decode(raw, pc);
        (format!("{raw:08x}\t{}", gex_isa_ppc::fmt::disasm(&inst, pc)), 4)
    }
}

pub use imp::*;

/// A block lifted for function-level GIR by `lift_insts`.
#[derive(Clone)]
pub struct Lifted {
    /// The lifted block, unoptimised.
    pub block: Block,
    /// The instruction each statement of `block` came from.
    pub pcs: Vec<u64>,
    /// Every instruction lifted, in order.
    pub insts: Vec<u64>,
    /// The instruction that ended the block, or the fall-through address
    /// when the block was cut short.
    pub term_pc: u64,
}

/// The guest's stack pointer: `sp` on AArch64, `rsp` (`X(4)`) on x86-64,
/// whose lifter leaves [`gex_ir::GuestReg::Sp`] unused.
pub const SP_REG: gex_ir::GuestReg = if gex_abi::X86 {
    gex_ir::GuestReg::X(4)
} else if gex_abi::PPC {
    gex_ir::GuestReg::R(1)
} else {
    gex_ir::GuestReg::Sp
};

/// The register holding the syscall number.
pub const SYSCALL_NR: gex_ir::GuestReg = if gex_abi::PPC {
    gex_ir::GuestReg::R(0)
} else {
    gex_ir::GuestReg::X(if gex_abi::X86 { 0 } else { 8 })
};

/// Whether the code being recovered is i386 (x86 compatibility mode, set
/// with [`crate::set_compat`]): 4-byte pointers and return addresses, every
/// register a zero-extended 32-bit value, the `int 0x80` syscall ABI.
pub fn compat() -> bool {
    #[cfg(feature = "x86")]
    {
        gex_isa_x86::mode() == gex_isa_x86::Mode::Compat
    }
    #[cfg(not(feature = "x86"))]
    false
}

/// Bytes in a guest pointer (and a return address on the stack).
pub fn ptr_bytes() -> u32 {
    if compat() {
        4
    } else {
        8
    }
}

/// Syscalls that never return to the next instruction: `exit`,
/// `exit_group`, `rt_sigreturn` (and i386's `sigreturn`).
pub fn syscall_never_returns(nr: u64) -> bool {
    if compat() {
        matches!(nr, 1 | 252 | 119 | 173)
    } else if gex_abi::X86 {
        matches!(nr, 60 | 231 | 15)
    } else if gex_abi::PPC {
        matches!(nr, 1 | 234 | 172 | 119)
    } else {
        matches!(nr, 93 | 94 | 139)
    }
}
