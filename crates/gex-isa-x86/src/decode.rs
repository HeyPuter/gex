//! The x86-64 decoder proper: prefixes, REX, the one-, two- and three-byte
//! opcode maps, ModRM/SIB, x87, and enough VEX/EVEX to know an instruction's
//! length.

use crate::Mode;
use crate::*;

/// Decode the instruction at the start of `bytes`, which lies at guest
/// address `pc`. `bytes` may be shorter than 15; a sequence the slice cuts
/// short decodes as `Unknown` of length 1.
pub fn decode(bytes: &[u8], pc: u64) -> Inst {
    decode_in(crate::mode(), bytes, pc)
}

/// Decode in an explicit mode (see [`crate::Mode`]).
pub fn decode_in(mode: Mode, bytes: &[u8], pc: u64) -> Inst {
    let mut d = Decoder {
        b: bytes,
        pos: 0,
        pc,
        mode,
        rex: 0,
        has_rex: false,
        op16: false,
        // In compatibility mode every address is 32 bits wide; the flag is
        // what makes the lifter wrap the arithmetic, so it is on from the
        // start there and 0x67 (16-bit addressing) is refused.
        addr32: mode == Mode::Compat,
        seg: None,
        rep: Rep::None,
        lock: false,
        mand: Mand::None,
    };
    match d.run() {
        Ok(mut inst) => {
            inst.len = d.pos.min(MAX_INST_BYTES) as u8;
            // RIP-relative displacements are relative to the end of the
            // whole instruction, which was not known while parsing ModRM.
            let next = pc.wrapping_add(inst.len as u64);
            for o in inst.ops.iter_mut().flatten() {
                if let Operand::Mem(m) = o {
                    if m.rip {
                        m.disp = (next as i64).wrapping_add(m.disp);
                    }
                }
            }
            if d.pos > MAX_INST_BYTES {
                return unknown(1);
            }
            inst
        }
        Err(()) => unknown(1),
    }
}

fn unknown(len: usize) -> Inst {
    Inst {
        op: Op::Unknown,
        len: len.clamp(1, MAX_INST_BYTES) as u8,
        width: Width::D,
        lane: Lane::I8,
        scalar: false,
        mmx: false,
        rep: Rep::None,
        lock: false,
        ops: [None, None, None],
    }
}

/// The mandatory-prefix state that selects among SSE encodings.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Mand {
    None,
    P66,
    PF3,
    PF2,
}

struct Decoder<'a> {
    mode: Mode,
    b: &'a [u8],
    pos: usize,
    pc: u64,
    rex: u8,
    has_rex: bool,
    op16: bool,
    addr32: bool,
    seg: Option<Seg>,
    rep: Rep,
    lock: bool,
    mand: Mand,
}

/// A parsed ModRM (plus SIB and displacement).
#[derive(Copy, Clone)]
struct ModRm {
    /// The `reg` field with REX.R applied.
    reg: u8,
    /// True when `mod == 3`: `rm` names a register.
    is_reg: bool,
    /// The `rm` register (REX.B applied) when `is_reg`.
    rm: u8,
    /// The memory operand when not `is_reg`.
    mem: Mem,
    /// The raw ModRM byte, for x87 and group decoding.
    raw: u8,
}

type R<T> = Result<T, ()>;

impl<'a> Decoder<'a> {
    // -- byte fetching ----------------------------------------------------

    fn u8(&mut self) -> R<u8> {
        let v = *self.b.get(self.pos).ok_or(())?;
        self.pos += 1;
        Ok(v)
    }
    fn i8(&mut self) -> R<i64> {
        Ok(self.u8()? as i8 as i64)
    }
    fn u16(&mut self) -> R<u64> {
        let lo = self.u8()? as u64;
        let hi = self.u8()? as u64;
        Ok(lo | (hi << 8))
    }
    fn i16(&mut self) -> R<i64> {
        Ok(self.u16()? as u16 as i16 as i64)
    }
    fn i32(&mut self) -> R<i64> {
        let mut v = 0u32;
        for i in 0..4 {
            v |= (self.u8()? as u32) << (8 * i);
        }
        Ok(v as i32 as i64)
    }
    fn u64(&mut self) -> R<i64> {
        let mut v = 0u64;
        for i in 0..8 {
            v |= (self.u8()? as u64) << (8 * i);
        }
        Ok(v as i64)
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    // -- REX bits -----------------------------------------------------------

    fn rex_w(&self) -> bool {
        self.rex & 8 != 0
    }
    fn rex_r(&self) -> u8 {
        (self.rex >> 2) & 1
    }
    fn rex_x(&self) -> u8 {
        (self.rex >> 1) & 1
    }
    fn rex_b(&self) -> u8 {
        self.rex & 1
    }

    /// Operand width for the ordinary `v`-sized operations.
    fn wv(&self) -> Width {
        if self.mode == Mode::Long && self.rex_w() {
            Width::Q
        } else if self.op16 {
            Width::W
        } else {
            Width::D
        }
    }
    /// Width for operations that default to 64 bits (`push`, `pop`, near
    /// `call`/`jmp`), where only the `0x66` prefix shrinks them.
    fn w64(&self) -> Width {
        if self.op16 {
            Width::W
        } else {
            self.wa()
        }
    }
    /// The address width: the size of a pushed return address, of `ret`'s
    /// pop, and of an indirect branch target.
    fn wa(&self) -> Width {
        if self.mode == Mode::Compat {
            Width::D
        } else {
            Width::Q
        }
    }
    /// A branch target computed from a relative displacement, wrapped to the
    /// address width.
    fn rel_target(&self, rel: i64) -> i64 {
        let t = (self.pc as i64).wrapping_add(self.pos as i64).wrapping_add(rel);
        if self.mode == Mode::Compat {
            t & 0xffff_ffff
        } else {
            t
        }
    }
    /// The `z`-sized immediate: 16 or 32 bits, sign-extended.
    fn imm_z(&mut self, w: Width) -> R<i64> {
        match w {
            Width::W => self.i16(),
            _ => self.i32(),
        }
    }
    /// An immediate of exactly the width (`mov r8, imm8`, `mov r32, imm32`,
    /// `mov r64, imm64`).
    fn imm_full(&mut self, w: Width) -> R<i64> {
        match w {
            Width::B => self.i8(),
            Width::W => self.i16(),
            Width::D => self.i32(),
            Width::Q => self.u64(),
        }
    }

    // -- operands -------------------------------------------------------------

    /// General-purpose register `n` at width `w`, with the legacy high-byte
    /// rule: without any REX prefix, 8-bit registers 4–7 are ah/ch/dh/bh.
    fn gp(&self, n: u8, w: Width) -> Operand {
        if w == Width::B && !self.has_rex && (4..8).contains(&n) {
            Operand::Reg(Reg::High(n - 4))
        } else {
            Operand::Reg(Reg::Gp(n, w))
        }
    }
    fn xmm(n: u8) -> Operand {
        Operand::Reg(Reg::Xmm(n))
    }
    fn mm(n: u8) -> Operand {
        Operand::Reg(Reg::Mm(n & 7))
    }

    fn modrm(&mut self) -> R<ModRm> {
        let raw = self.u8()?;
        let md = raw >> 6;
        let reg = ((raw >> 3) & 7) | (self.rex_r() << 3);
        let rm3 = raw & 7;
        let mut mem = Mem {
            base: None,
            index: None,
            scale: 1,
            disp: 0,
            rip: false,
            seg: match self.seg {
                Some(Seg::Fs) | Some(Seg::Gs) => self.seg,
                _ => None,
            },
            addr32: self.addr32,
        };
        if md == 3 {
            return Ok(ModRm {
                reg,
                is_reg: true,
                rm: rm3 | (self.rex_b() << 3),
                mem,
                raw,
            });
        }
        if rm3 == 4 {
            let sib = self.u8()?;
            let idx = ((sib >> 3) & 7) | (self.rex_x() << 3);
            let base3 = sib & 7;
            if idx != 4 {
                mem.index = Some(idx);
                mem.scale = 1 << (sib >> 6);
            }
            if base3 == 5 && md == 0 {
                mem.disp = self.i32()?;
            } else {
                mem.base = Some(base3 | (self.rex_b() << 3));
            }
        } else if rm3 == 5 && md == 0 {
            // RIP-relative in long mode; a plain absolute disp32 in 32-bit code.
            mem.rip = self.mode == Mode::Long;
            mem.disp = self.i32()?;
        } else {
            mem.base = Some(rm3 | (self.rex_b() << 3));
        }
        match md {
            1 => mem.disp = self.i8()?,
            2 => mem.disp = self.i32()?,
            _ => {}
        }
        Ok(ModRm {
            reg,
            is_reg: false,
            rm: 0,
            mem,
            raw,
        })
    }

    /// The `r/m` operand as a GPR of width `w` or memory.
    fn e(&self, m: &ModRm, w: Width) -> Operand {
        if m.is_reg {
            self.gp(m.rm, w)
        } else {
            Operand::Mem(m.mem)
        }
    }
    /// The `r/m` operand as an xmm register or memory.
    fn ex(m: &ModRm) -> Operand {
        if m.is_reg {
            Self::xmm(m.rm)
        } else {
            Operand::Mem(m.mem)
        }
    }
    fn em(m: &ModRm) -> Operand {
        if m.is_reg {
            Self::mm(m.rm)
        } else {
            Operand::Mem(m.mem)
        }
    }
    /// The `reg` operand as a GPR.
    fn g(&self, m: &ModRm, w: Width) -> Operand {
        self.gp(m.reg, w)
    }

    // -- instruction assembly ---------------------------------------------------

    fn inst(&self, op: Op, w: Width, ops: [Option<Operand>; 3]) -> Inst {
        Inst {
            op,
            len: 0,
            width: w,
            lane: Lane::I8,
            scalar: false,
            mmx: false,
            rep: self.rep,
            lock: self.lock,
            ops,
        }
    }
    fn i0(&self, op: Op, w: Width) -> Inst {
        self.inst(op, w, [None, None, None])
    }
    fn i1(&self, op: Op, w: Width, a: Operand) -> Inst {
        self.inst(op, w, [Some(a), None, None])
    }
    fn i2(&self, op: Op, w: Width, a: Operand, b: Operand) -> Inst {
        self.inst(op, w, [Some(a), Some(b), None])
    }
    fn i3(&self, op: Op, w: Width, a: Operand, b: Operand, c: Operand) -> Inst {
        self.inst(op, w, [Some(a), Some(b), Some(c)])
    }
    /// An SSE instruction with lane type and scalar flag.
    fn sse(&self, op: Op, lane: Lane, scalar: bool, a: Operand, b: Operand) -> Inst {
        let mut i = self.i2(op, Width::Q, a, b);
        i.lane = lane;
        i.scalar = scalar;
        i
    }
    fn sse3(&self, op: Op, lane: Lane, scalar: bool, a: Operand, b: Operand, c: Operand) -> Inst {
        let mut i = self.i3(op, Width::Q, a, b, c);
        i.lane = lane;
        i.scalar = scalar;
        i
    }

    /// An `Unknown` that has consumed its bytes, so the length is right.
    fn unk(&self) -> Inst {
        unknown(self.pos)
    }

    // -- prefixes -------------------------------------------------------------

    fn run(&mut self) -> R<Inst> {
        // Legacy prefixes in any order, then an optional REX, then the opcode.
        // A REX not immediately before the opcode is ignored.
        loop {
            let b = self.u8()?;
            match b {
                0xf0 => self.lock = true,
                0xf2 => {
                    self.rep = Rep::RepNe;
                    self.mand = Mand::PF2;
                }
                0xf3 => {
                    self.rep = Rep::Rep;
                    self.mand = Mand::PF3;
                }
                0x66 => {
                    self.op16 = true;
                    if self.mand == Mand::None {
                        self.mand = Mand::P66;
                    }
                }
                0x67 => {
                    if self.mode == Mode::Compat {
                        // 16-bit addressing: nothing gex runs uses it.
                        return Err(());
                    }
                    self.addr32 = true;
                }
                0x26 => self.seg = Some(Seg::Es),
                0x2e => self.seg = Some(Seg::Cs),
                0x36 => self.seg = Some(Seg::Ss),
                0x3e => self.seg = Some(Seg::Ds),
                0x64 => self.seg = Some(Seg::Fs),
                0x65 => self.seg = Some(Seg::Gs),
                0x40..=0x4f if self.mode == Mode::Long => {
                    // REX must be the last prefix; if another prefix follows
                    // it, the REX is dropped and prefix parsing continues.
                    match self.peek() {
                        Some(0xf0 | 0xf2 | 0xf3 | 0x66 | 0x67 | 0x26 | 0x2e | 0x36 | 0x3e | 0x64
                        | 0x65 | 0x40..=0x4f) => {}
                        _ => {
                            self.rex = b & 0xf;
                            self.has_rex = true;
                            let op = self.u8()?;
                            return self.opcode(op);
                        }
                    }
                }
                _ => return self.opcode(b),
            }
            if self.pos > MAX_INST_BYTES {
                return Err(());
            }
        }
    }

    // -- one-byte map -----------------------------------------------------------

    fn opcode(&mut self, op: u8) -> R<Inst> {
        const ALU: [Op; 8] = [
            Op::Add,
            Op::Or,
            Op::Adc,
            Op::Sbb,
            Op::And,
            Op::Sub,
            Op::Xor,
            Op::Cmp,
        ];
        match op {
            0x0f => return self.map_0f(),
            // ALU block: 00-3f less the escapes and (invalid in 64-bit) BCD ops.
            0x00..=0x3f if op & 7 < 6 => {
                let alu = ALU[(op >> 3) as usize & 7];
                return Ok(match op & 7 {
                    0 => {
                        let m = self.modrm()?;
                        self.i2(alu, Width::B, self.e(&m, Width::B), self.g(&m, Width::B))
                    }
                    1 => {
                        let w = self.wv();
                        let m = self.modrm()?;
                        self.i2(alu, w, self.e(&m, w), self.g(&m, w))
                    }
                    2 => {
                        let m = self.modrm()?;
                        self.i2(alu, Width::B, self.g(&m, Width::B), self.e(&m, Width::B))
                    }
                    3 => {
                        let w = self.wv();
                        let m = self.modrm()?;
                        self.i2(alu, w, self.g(&m, w), self.e(&m, w))
                    }
                    4 => {
                        let imm = self.i8()?;
                        self.i2(alu, Width::B, self.gp(0, Width::B), Operand::Imm(imm))
                    }
                    _ => {
                        let w = self.wv();
                        let imm = self.imm_z(w)?;
                        self.i2(alu, w, self.gp(0, w), Operand::Imm(imm))
                    }
                });
            }
            // push/pop of a segment register: 32-bit code only.
            0x06 | 0x07 | 0x0e | 0x16 | 0x17 | 0x1e | 0x1f if self.mode == Mode::Compat => {
                let seg = match op >> 3 {
                    0 => Seg::Es,
                    1 => Seg::Cs,
                    2 => Seg::Ss,
                    _ => Seg::Ds,
                };
                let o = if op & 1 == 0 { Op::Push } else { Op::Pop };
                return Ok(self.i1(o, Width::D, Operand::Reg(Reg::Seg(seg))));
            }
            0x06 | 0x07 | 0x0e | 0x16 | 0x17 | 0x1e | 0x1f | 0x27 | 0x2f | 0x37 | 0x3f => {
                return Ok(self.i0(Op::Undefined, Width::D))
            }
            // inc/dec r32 -- in long mode these bytes are REX and never reach
            // here.
            0x40..=0x4f => {
                let w = self.wv();
                let o = if op < 0x48 { Op::Inc } else { Op::Dec };
                return Ok(self.i1(o, w, self.gp(op & 7, w)));
            }
            0x50..=0x57 => {
                let w = self.w64();
                return Ok(self.i1(Op::Push, w, self.gp((op & 7) | (self.rex_b() << 3), w)));
            }
            0x58..=0x5f => {
                let w = self.w64();
                return Ok(self.i1(Op::Pop, w, self.gp((op & 7) | (self.rex_b() << 3), w)));
            }
            0x63 => {
                if self.mode == Mode::Compat {
                    return Ok(self.unk()); // arpl
                }
                // movsxd r64, r/m32 (with REX.W); without it a plain 32-bit mov.
                let w = self.wv();
                let m = self.modrm()?;
                let src_w = if w == Width::Q { Width::D } else { w };
                let o = if w == Width::Q { Op::Movsxd } else { Op::Mov };
                return Ok(self.i2(o, w, self.g(&m, w), self.e(&m, src_w)));
            }
            0x68 => {
                let w = self.w64();
                let imm = self.imm_z(w)?;
                return Ok(self.i1(Op::Push, w, Operand::Imm(imm)));
            }
            0x69 | 0x6b => {
                let w = self.wv();
                let m = self.modrm()?;
                let imm = if op == 0x6b { self.i8()? } else { self.imm_z(w)? };
                return Ok(self.i3(Op::Imul, w, self.g(&m, w), self.e(&m, w), Operand::Imm(imm)));
            }
            0x6a => {
                let w = self.w64();
                let imm = self.i8()?;
                return Ok(self.i1(Op::Push, w, Operand::Imm(imm)));
            }
            // ins/outs: #GP in user mode.
            0x6c..=0x6f => return Ok(self.i0(Op::Privileged(op), Width::D)),
            0x70..=0x7f => {
                let rel = self.i8()?;
                let target = self.rel_target(rel);
                return Ok(self.i1(Op::Jcc(Cond(op & 15)), self.wa(), Operand::Imm(target)));
            }
            0x80 | 0x81 | 0x83 => {
                let w = if op == 0x80 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                let alu = ALU[((m.raw >> 3) & 7) as usize];
                let imm = if op == 0x81 { self.imm_z(w)? } else { self.i8()? };
                let imm = if w == Width::B { imm & 0xff } else { imm };
                return Ok(self.i2(alu, w, self.e(&m, w), Operand::Imm(imm)));
            }
            0x84 | 0x85 => {
                let w = if op == 0x84 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Test, w, self.e(&m, w), self.g(&m, w)));
            }
            0x86 | 0x87 => {
                let w = if op == 0x86 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Xchg, w, self.e(&m, w), self.g(&m, w)));
            }
            0x88 | 0x89 => {
                let w = if op == 0x88 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Mov, w, self.e(&m, w), self.g(&m, w)));
            }
            0x8a | 0x8b => {
                let w = if op == 0x8a { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Mov, w, self.g(&m, w), self.e(&m, w)));
            }
            0x8c | 0x8e => {
                let m = self.modrm()?;
                let Some(seg) = Seg::from_num(m.reg & 7) else {
                    return Ok(self.unk());
                };
                let w = if m.is_reg { self.wv() } else { Width::W };
                let s = Operand::Reg(Reg::Seg(seg));
                return Ok(if op == 0x8c {
                    self.i2(Op::Mov, w, self.e(&m, w), s)
                } else {
                    self.i2(Op::Mov, w, s, self.e(&m, w))
                });
            }
            0x8d => {
                let w = self.wv();
                let m = self.modrm()?;
                if m.is_reg {
                    return Ok(self.unk());
                }
                return Ok(self.i2(Op::Lea, w, self.g(&m, w), Operand::Mem(m.mem)));
            }
            0x8f => {
                let w = self.w64();
                let m = self.modrm()?;
                if (m.raw >> 3) & 7 != 0 {
                    return Ok(self.unk());
                }
                return Ok(self.i1(Op::Pop, w, self.e(&m, w)));
            }
            0x90 => {
                if self.rex_b() != 0 {
                    let w = self.wv();
                    return Ok(self.i2(Op::Xchg, w, self.gp(8, w), self.gp(0, w)));
                }
                return Ok(if self.mand == Mand::PF3 {
                    self.i0(Op::Pause, Width::D)
                } else {
                    self.i0(Op::Nop, Width::D)
                });
            }
            0x91..=0x97 => {
                let w = self.wv();
                return Ok(self.i2(Op::Xchg, w, self.gp((op & 7) | (self.rex_b() << 3), w), self.gp(0, w)));
            }
            0x98 => return Ok(self.i0(Op::Cbw, self.wv())),
            0x99 => return Ok(self.i0(Op::Cwd, self.wv())),
            0x9b => {
                // `fwait` fused with the x87 control instruction it precedes
                // (`fstsw`, `fstcw`, `fclex`, `finit`, `fstenv`, `fsave`), as
                // binutils spells them. Semantically the wait is a no-op, so
                // the fused form is just the second instruction, one byte
                // longer.
                if let Some(next @ (0xd9 | 0xdb | 0xdd | 0xdf)) = self.peek() {
                    self.pos += 1;
                    return self.x87(next);
                }
                return Ok(self.i0(Op::Fwait, Width::D));
            }
            0x9c => return Ok(self.i0(Op::Pushf, self.w64())),
            0x9d => return Ok(self.i0(Op::Popf, self.w64())),
            0x9e => return Ok(self.i0(Op::Sahf, Width::B)),
            0x9f => return Ok(self.i0(Op::Lahf, Width::B)),
            0xa0..=0xa3 => {
                // mov al/eax <-> moffs64: a full 64-bit absolute address.
                let w = if op & 1 == 0 { Width::B } else { self.wv() };
                let addr = if self.addr32 { self.i32()? & 0xffff_ffff } else { self.u64()? };
                let mut mem = Mem::abs(addr);
                // A 32-bit moffs with a segment is `seg_base + disp32` wrapped
                // at 32 bits: TLS lives *below* %gs on i386, so the offset is
                // usually negative.
                mem.addr32 = self.addr32;
                mem.seg = match self.seg {
                    Some(Seg::Fs) | Some(Seg::Gs) => self.seg,
                    _ => None,
                };
                return Ok(if op < 0xa2 {
                    self.i2(Op::Mov, w, self.gp(0, w), Operand::Mem(mem))
                } else {
                    self.i2(Op::Mov, w, Operand::Mem(mem), self.gp(0, w))
                });
            }
            0xa4 | 0xa5 => return Ok(self.i0(Op::Movs, if op == 0xa4 { Width::B } else { self.wv() })),
            0xa6 | 0xa7 => return Ok(self.i0(Op::Cmps, if op == 0xa6 { Width::B } else { self.wv() })),
            0xa8 | 0xa9 => {
                let w = if op == 0xa8 { Width::B } else { self.wv() };
                let imm = if w == Width::B { self.i8()? & 0xff } else { self.imm_z(w)? };
                return Ok(self.i2(Op::Test, w, self.gp(0, w), Operand::Imm(imm)));
            }
            0xaa | 0xab => return Ok(self.i0(Op::Stos, if op == 0xaa { Width::B } else { self.wv() })),
            0xac | 0xad => return Ok(self.i0(Op::Lods, if op == 0xac { Width::B } else { self.wv() })),
            0xae | 0xaf => return Ok(self.i0(Op::Scas, if op == 0xae { Width::B } else { self.wv() })),
            0xb0..=0xb7 => {
                let imm = self.i8()? & 0xff;
                return Ok(self.i2(
                    Op::Mov,
                    Width::B,
                    self.gp((op & 7) | (self.rex_b() << 3), Width::B),
                    Operand::Imm(imm),
                ));
            }
            0xb8..=0xbf => {
                let w = self.wv();
                let imm = self.imm_full(w)?;
                let o = if w == Width::Q { Op::Movabs } else { Op::Mov };
                let imm = if w == Width::D { imm & 0xffff_ffff } else { imm };
                return Ok(self.i2(o, w, self.gp((op & 7) | (self.rex_b() << 3), w), Operand::Imm(imm)));
            }
            0xc0 | 0xc1 | 0xd0..=0xd3 => {
                const SH: [Op; 8] = [Op::Rol, Op::Ror, Op::Rcl, Op::Rcr, Op::Shl, Op::Shr, Op::Shl, Op::Sar];
                let w = if op & 1 == 0 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                let sh = SH[((m.raw >> 3) & 7) as usize];
                let amount = match op {
                    0xc0 | 0xc1 => Operand::Imm(self.i8()? & 0xff),
                    0xd0 | 0xd1 => Operand::Imm(1),
                    _ => Operand::Reg(Reg::Gp(1, Width::B)),
                };
                return Ok(self.i2(sh, w, self.e(&m, w), amount));
            }
            0xc2 => {
                let imm = self.u16()? as i64;
                return Ok(self.i1(Op::Ret, self.wa(), Operand::Imm(imm)));
            }
            0xc3 => return Ok(self.i0(Op::Ret, self.wa())),
            0xc6 | 0xc7 => {
                let w = if op == 0xc6 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                if (m.raw >> 3) & 7 != 0 {
                    return Ok(self.unk());
                }
                let imm = if w == Width::B { self.i8()? & 0xff } else { self.imm_z(w)? };
                return Ok(self.i2(Op::Mov, w, self.e(&m, w), Operand::Imm(imm)));
            }
            0xc8 => {
                let size = self.u16()? as i64;
                let nest = self.u8()? as i64;
                return Ok(self.i2(Op::Enter, self.wa(), Operand::Imm(size), Operand::Imm(nest)));
            }
            0xc9 => return Ok(self.i0(Op::Leave, self.w64())),
            0xcc => return Ok(self.i0(Op::Int3, Width::D)),
            0xcd => {
                let n = self.u8()? as i64;
                return Ok(self.i1(Op::Int, Width::D, Operand::Imm(n)));
            }
            0xd7 => {
                // xlat: al = [seg:(e/r)bx + zero-extended al]. The operand
                // carries the segment override and the address size; the
                // lifter adds al.
                let mut mem = Mem::abs(0);
                mem.base = Some(3);
                mem.addr32 = self.addr32;
                mem.seg = match self.seg {
                    Some(Seg::Fs) | Some(Seg::Gs) => self.seg,
                    _ => None,
                };
                return Ok(self.i1(Op::Xlat, Width::B, Operand::Mem(mem)));
            }
            0xd8..=0xdf => return self.x87(op),
            0xe0..=0xe3 => {
                let rel = self.i8()?;
                let target = self.rel_target(rel);
                let o = match op {
                    0xe0 => Op::Loopne,
                    0xe1 => Op::Loope,
                    0xe2 => Op::Loop,
                    _ => Op::Jrcxz,
                };
                // The counter is ecx with 0x67, rcx otherwise.
                let w = if self.addr32 { Width::D } else { Width::Q };
                return Ok(self.i1(o, w, Operand::Imm(target)));
            }
            0xe8 | 0xe9 => {
                let rel = self.i32()?;
                let target = self.rel_target(rel);
                let o = if op == 0xe8 { Op::Call } else { Op::Jmp };
                return Ok(self.i1(o, self.wa(), Operand::Imm(target)));
            }
            0xeb => {
                let rel = self.i8()?;
                let target = self.rel_target(rel);
                return Ok(self.i1(Op::Jmp, self.wa(), Operand::Imm(target)));
            }
            0xf1 => return Ok(self.i0(Op::Int, Width::D)),
            0xf4 => return Ok(self.i0(Op::Hlt, Width::D)),
            0xf5 => return Ok(self.i0(Op::Cmc, Width::D)),
            0xf6 | 0xf7 => {
                let w = if op == 0xf6 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                let e = self.e(&m, w);
                return Ok(match (m.raw >> 3) & 7 {
                    0 | 1 => {
                        let imm = if w == Width::B { self.i8()? & 0xff } else { self.imm_z(w)? };
                        self.i2(Op::Test, w, e, Operand::Imm(imm))
                    }
                    2 => self.i1(Op::Not, w, e),
                    3 => self.i1(Op::Neg, w, e),
                    4 => self.i1(Op::Mul, w, e),
                    5 => self.i1(Op::Imul1, w, e),
                    6 => self.i1(Op::Div, w, e),
                    _ => self.i1(Op::Idiv, w, e),
                });
            }
            0xf8 => return Ok(self.i0(Op::Clc, Width::D)),
            0xf9 => return Ok(self.i0(Op::Stc, Width::D)),
            // cli/sti, and in/out with an immediate port or dx: #GP in
            // user mode.
            0xfa | 0xfb | 0xec..=0xef => return Ok(self.i0(Op::Privileged(op), Width::D)),
            0xe4..=0xe7 => {
                let _port = self.u8()?;
                return Ok(self.i0(Op::Privileged(op), Width::D));
            }
            0xfc => return Ok(self.i0(Op::Cld, Width::D)),
            0xfd => return Ok(self.i0(Op::Std, Width::D)),
            0xfe => {
                let m = self.modrm()?;
                let e = self.e(&m, Width::B);
                return Ok(match (m.raw >> 3) & 7 {
                    0 => self.i1(Op::Inc, Width::B, e),
                    1 => self.i1(Op::Dec, Width::B, e),
                    _ => self.unk(),
                });
            }
            0xff => {
                let m = self.modrm()?;
                return Ok(match (m.raw >> 3) & 7 {
                    0 => {
                        let w = self.wv();
                        self.i1(Op::Inc, w, self.e(&m, w))
                    }
                    1 => {
                        let w = self.wv();
                        self.i1(Op::Dec, w, self.e(&m, w))
                    }
                    2 => self.i1(Op::Call, self.wa(), self.e(&m, self.wa())),
                    4 => self.i1(Op::Jmp, self.wa(), self.e(&m, self.wa())),
                    6 => {
                        let w = self.w64();
                        self.i1(Op::Push, w, self.e(&m, w))
                    }
                    // far call/jmp
                    _ => self.unk(),
                });
            }
            // In 32-bit code c4/c5 are VEX only when the next byte has mod=11;
            // otherwise they are les/lds, and 62 is bound -- none of which
            // compiled code uses.
            0xc4 | 0xc5 if self.mode == Mode::Compat => {
                return match self.peek() {
                    Some(b) if b >= 0xc0 => self.vex(op),
                    _ => Ok(self.unk()),
                };
            }
            0x62 if self.mode == Mode::Compat => return Ok(self.unk()),
            0xc4 | 0xc5 => return self.vex(op),
            0x62 => return self.evex(),
            _ => {}
        }
        Ok(self.unk())
    }

    // -- two-byte map -----------------------------------------------------------

    fn map_0f(&mut self) -> R<Inst> {
        let op = self.u8()?;
        match op {
            0x38 => return self.map_0f38(),
            0x3a => return self.map_0f3a(),
            0x05 => return Ok(self.i0(Op::Syscall, Width::D)),
            0x0b => return Ok(self.i0(Op::Undefined, Width::D)),
            0x0d => {
                let m = self.modrm()?;
                return Ok(if m.is_reg { self.unk() } else { self.i1(Op::Prefetch, Width::B, Operand::Mem(m.mem)) });
            }
            0x18 => {
                let m = self.modrm()?;
                return Ok(if m.is_reg || (m.raw >> 3) & 7 > 3 {
                    self.i0(Op::Nop, Width::D)
                } else {
                    self.i1(Op::Prefetch, Width::B, Operand::Mem(m.mem))
                });
            }
            0x19..=0x1d => {
                let _m = self.modrm()?;
                return Ok(self.i0(Op::Nop, Width::D));
            }
            0x1e => {
                let m = self.modrm()?;
                if self.mand == Mand::PF3 && (m.raw == 0xfa || m.raw == 0xfb) {
                    return Ok(self.i0(Op::Endbr, Width::D));
                }
                // rdsspd/q and the reserved-nop forms: no-ops without CET.
                return Ok(self.i0(Op::Nop, Width::D));
            }
            0x1f => {
                let _m = self.modrm()?;
                return Ok(self.i0(Op::Nop, Width::D));
            }
            0x31 => return Ok(self.i0(Op::Rdtsc, Width::D)),
            0x01 => {
                let m = self.modrm()?;
                return Ok(match m.raw {
                    0xd0 => self.i0(Op::Xgetbv, Width::D),
                    0xf9 => self.i0(Op::Rdtscp, Width::D),
                    _ => self.unk(),
                });
            }
            0x00 => {
                let _m = self.modrm()?;
                return Ok(self.unk());
            }
            0x40..=0x4f => {
                let w = self.wv();
                let m = self.modrm()?;
                return Ok(self.i2(Op::Cmovcc(Cond(op & 15)), w, self.g(&m, w), self.e(&m, w)));
            }
            0x80..=0x8f => {
                let rel = self.i32()?;
                let target = self.rel_target(rel);
                return Ok(self.i1(Op::Jcc(Cond(op & 15)), self.wa(), Operand::Imm(target)));
            }
            0x90..=0x9f => {
                let m = self.modrm()?;
                return Ok(self.i1(Op::Setcc(Cond(op & 15)), Width::B, self.e(&m, Width::B)));
            }
            0xa0 => return Ok(self.i1(Op::Push, Width::Q, Operand::Reg(Reg::Seg(Seg::Fs)))),
            0xa1 => return Ok(self.i1(Op::Pop, Width::Q, Operand::Reg(Reg::Seg(Seg::Fs)))),
            0xa8 => return Ok(self.i1(Op::Push, Width::Q, Operand::Reg(Reg::Seg(Seg::Gs)))),
            0xa9 => return Ok(self.i1(Op::Pop, Width::Q, Operand::Reg(Reg::Seg(Seg::Gs)))),
            0xa2 => return Ok(self.i0(Op::Cpuid, Width::D)),
            0xa3 | 0xab | 0xb3 | 0xbb => {
                let w = self.wv();
                let m = self.modrm()?;
                let o = match op {
                    0xa3 => Op::Bt,
                    0xab => Op::Bts,
                    0xb3 => Op::Btr,
                    _ => Op::Btc,
                };
                return Ok(self.i2(o, w, self.e(&m, w), self.g(&m, w)));
            }
            0xa4 | 0xa5 | 0xac | 0xad => {
                let w = self.wv();
                let m = self.modrm()?;
                let o = if op < 0xa8 { Op::Shld } else { Op::Shrd };
                let amount = if op & 1 == 0 {
                    Operand::Imm(self.i8()? & 0xff)
                } else {
                    Operand::Reg(Reg::Gp(1, Width::B))
                };
                return Ok(self.i3(o, w, self.e(&m, w), self.g(&m, w), amount));
            }
            0xae => return self.grp15(),
            0xaf => {
                let w = self.wv();
                let m = self.modrm()?;
                return Ok(self.i2(Op::Imul, w, self.g(&m, w), self.e(&m, w)));
            }
            0xb0 | 0xb1 => {
                let w = if op == 0xb0 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Cmpxchg, w, self.e(&m, w), self.g(&m, w)));
            }
            0xb6 | 0xb7 | 0xbe | 0xbf => {
                let w = self.wv();
                let m = self.modrm()?;
                let src_w = if op & 1 == 0 { Width::B } else { Width::W };
                let o = if op < 0xb8 { Op::Movzx(src_w) } else { Op::Movsx(src_w) };
                return Ok(self.i2(o, w, self.g(&m, w), self.e(&m, src_w)));
            }
            0xb8 => {
                let w = self.wv();
                let m = self.modrm()?;
                if self.mand != Mand::PF3 {
                    return Ok(self.unk());
                }
                return Ok(self.i2(Op::Popcnt, w, self.g(&m, w), self.e(&m, w)));
            }
            0xba => {
                let w = self.wv();
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                let o = match (m.raw >> 3) & 7 {
                    4 => Op::Bt,
                    5 => Op::Bts,
                    6 => Op::Btr,
                    7 => Op::Btc,
                    _ => return Ok(self.unk()),
                };
                return Ok(self.i2(o, w, self.e(&m, w), Operand::Imm(imm)));
            }
            0xbc | 0xbd => {
                let w = self.wv();
                let m = self.modrm()?;
                let o = match (op, self.mand) {
                    (0xbc, Mand::PF3) => Op::Tzcnt,
                    (0xbc, _) => Op::Bsf,
                    (_, Mand::PF3) => Op::Lzcnt,
                    _ => Op::Bsr,
                };
                return Ok(self.i2(o, w, self.g(&m, w), self.e(&m, w)));
            }
            0xc0 | 0xc1 => {
                let w = if op == 0xc0 { Width::B } else { self.wv() };
                let m = self.modrm()?;
                return Ok(self.i2(Op::Xadd, w, self.e(&m, w), self.g(&m, w)));
            }
            0xc7 => {
                let m = self.modrm()?;
                return Ok(match ((m.raw >> 3) & 7, m.is_reg) {
                    (1, false) => {
                        let w = if self.rex_w() { Width::Q } else { Width::D };
                        self.i1(Op::CmpxchgNb, w, Operand::Mem(m.mem))
                    }
                    (6, true) => {
                        let w = self.wv();
                        self.i1(Op::Rdrand, w, self.gp(m.rm, w))
                    }
                    (7, true) => {
                        let w = self.wv();
                        self.i1(Op::Rdseed, w, self.gp(m.rm, w))
                    }
                    _ => self.unk(),
                });
            }
            0xc8..=0xcf => {
                let w = self.wv();
                return Ok(self.i1(Op::Bswap, w, self.gp((op & 7) | (self.rex_b() << 3), w)));
            }
            0xff => {
                let _m = self.modrm()?;
                return Ok(self.i0(Op::Undefined, Width::D)); // ud0
            }
            0xb9 => {
                let _m = self.modrm()?;
                return Ok(self.i0(Op::Undefined, Width::D)); // ud1
            }
            0x77 => return Ok(self.i0(Op::Emms, Width::D)),
            _ => {}
        }
        self.sse_0f(op)
    }

    /// Group 15: fences, MXCSR, fxsave/fxrstor, xsave.
    fn grp15(&mut self) -> R<Inst> {
        let m = self.modrm()?;
        let r = (m.raw >> 3) & 7;
        if m.is_reg {
            return Ok(match r {
                5 | 6 | 7 => self.i0(Op::Fence, Width::D),
                // rdfsbase/rdgsbase/wrfsbase/wrgsbase (F3): not advertised.
                _ => self.unk(),
            });
        }
        let mem = Operand::Mem(m.mem);
        Ok(match r {
            0 => self.i1(Op::Fxsave, Width::Q, mem),
            1 => self.i1(Op::Fxrstor, Width::Q, mem),
            2 => self.i1(Op::Ldmxcsr, Width::D, mem),
            3 => self.i1(Op::Stmxcsr, Width::D, mem),
            4 => self.i1(Op::Xsave, Width::Q, mem),
            5 => self.i1(Op::Xrstor, Width::Q, mem),
            6 => self.i1(Op::Xsave, Width::Q, mem), // xsaveopt
            _ => self.i1(Op::Clflush, Width::B, mem),
        })
    }

    // -- SSE in the 0F map ------------------------------------------------------

    fn sse_0f(&mut self, op: u8) -> R<Inst> {
        use Lane::*;
        let mand = self.mand;
        // The lane type that np/66/F3/F2 select on the packed-float family.
        let fl = |mand: Mand| -> Option<(Lane, bool)> {
            Some(match mand {
                Mand::None => (F32, false),
                Mand::P66 => (F64, false),
                Mand::PF3 => (F32, true),
                Mand::PF2 => (F64, true),
            })
        };
        // Integer ops: np means MMX, 66 means xmm; F3/F2 are not defined.
        let int_form = |d: &Decoder, m: &ModRm, op: Op, lane: Lane| -> Inst {
            match mand {
                Mand::P66 => d.sse(op, lane, false, Self::xmm(m.reg), Self::ex(m)),
                _ => {
                    let mut i = d.sse(op, lane, false, Self::mm(m.reg), Self::em(m));
                    i.mmx = true;
                    i
                }
            }
        };
        match op {
            0x10 | 0x11 => {
                let m = self.modrm()?;
                let (o, lane, scalar) = match mand {
                    Mand::None => (Op::Movups, F32, false),
                    Mand::P66 => (Op::Movups, F64, false),
                    Mand::PF3 => (Op::Movss, F32, true),
                    Mand::PF2 => (Op::Movsd, F64, true),
                };
                let (dst, src) = if op == 0x10 {
                    (Self::xmm(m.reg), Self::ex(&m))
                } else {
                    (Self::ex(&m), Self::xmm(m.reg))
                };
                return Ok(self.sse(o, lane, scalar, dst, src));
            }
            0x12 => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::None if m.is_reg => self.sse(Op::Movhlps, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::None => self.sse(Op::Movlps, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::P66 => self.sse(Op::Movlps, F64, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::PF3 => self.sse(Op::Movsldup, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::PF2 => self.sse(Op::Movddup, F64, false, Self::xmm(m.reg), Self::ex(&m)),
                });
            }
            0x13 => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                return Ok(self.sse(Op::Movlps, lane, false, Self::ex(&m), Self::xmm(m.reg)));
            }
            0x14 | 0x15 => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                let o = if op == 0x14 { Op::Unpcklp } else { Op::Unpckhp };
                return Ok(self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x16 => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::None if m.is_reg => self.sse(Op::Movlhps, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::None => self.sse(Op::Movhps, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::P66 => self.sse(Op::Movhps, F64, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::PF3 => self.sse(Op::Movshdup, F32, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::PF2 => self.unk(),
                });
            }
            0x17 => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                return Ok(self.sse(Op::Movhps, lane, false, Self::ex(&m), Self::xmm(m.reg)));
            }
            0x28 | 0x29 => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                let (dst, src) = if op == 0x28 {
                    (Self::xmm(m.reg), Self::ex(&m))
                } else {
                    (Self::ex(&m), Self::xmm(m.reg))
                };
                return Ok(self.sse(Op::Movaps, lane, false, dst, src));
            }
            0x2a => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::PF3 | Mand::PF2 => {
                        let w = if self.rex_w() { Width::Q } else { Width::D };
                        let lane = if mand == Mand::PF3 { F32 } else { F64 };
                        let mut i = self.sse(Op::Cvtsi2s, lane, true, Self::xmm(m.reg), self.e(&m, w));
                        i.width = w;
                        i
                    }
                    _ => {
                        let lane = if mand == Mand::P66 { F64 } else { F32 };
                        let mut i = self.sse(Op::CvtMmx, lane, false, Self::xmm(m.reg), Self::em(&m));
                        i.mmx = true;
                        i
                    }
                });
            }
            0x2b => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                return Ok(self.sse(Op::Movaps, lane, false, Self::ex(&m), Self::xmm(m.reg)));
            }
            0x2c | 0x2d => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::PF3 | Mand::PF2 => {
                        let w = if self.rex_w() { Width::Q } else { Width::D };
                        let lane = if mand == Mand::PF3 { F32 } else { F64 };
                        let mut i = self.sse(
                            Op::Cvts2si { truncate: op == 0x2c },
                            lane,
                            true,
                            self.g(&m, w),
                            Self::ex(&m),
                        );
                        i.width = w;
                        i
                    }
                    _ => {
                        let lane = if mand == Mand::P66 { F64 } else { F32 };
                        let mut i = self.sse(Op::CvtMmx, lane, false, Self::mm(m.reg), Self::ex(&m));
                        i.mmx = true;
                        i
                    }
                });
            }
            0x2e | 0x2f => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                let o = if op == 0x2e { Op::Ucomis } else { Op::Comis };
                return Ok(self.sse(o, lane, true, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x50 => {
                let m = self.modrm()?;
                let lane = if mand == Mand::P66 { F64 } else { F32 };
                let mut i = self.sse(Op::Movmsk, lane, false, self.g(&m, Width::D), Self::ex(&m));
                i.width = Width::D;
                return Ok(i);
            }
            0x51..=0x53 | 0x58 | 0x59 | 0x5c..=0x5f => {
                let m = self.modrm()?;
                let Some((lane, scalar)) = fl(mand) else { return Ok(self.unk()) };
                let o = match op {
                    0x51 => Op::Sqrtp,
                    0x52 => Op::Rsqrtp,
                    0x53 => Op::Rcpp,
                    0x58 => Op::Addp,
                    0x59 => Op::Mulp,
                    0x5c => Op::Subp,
                    0x5d => Op::Minp,
                    0x5e => Op::Divp,
                    _ => Op::Maxp,
                };
                if matches!(op, 0x52 | 0x53) && lane == F64 {
                    return Ok(self.unk());
                }
                return Ok(self.sse(o, lane, scalar, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x54..=0x57 => {
                let m = self.modrm()?;
                let lane = match mand {
                    Mand::None => F32,
                    Mand::P66 => F64,
                    _ => return Ok(self.unk()),
                };
                let o = match op {
                    0x54 => Op::Andp,
                    0x55 => Op::Andnp,
                    0x56 => Op::Orp,
                    _ => Op::Xorp,
                };
                return Ok(self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x5a => {
                let m = self.modrm()?;
                // cvtps2pd / cvtpd2ps / cvtss2sd / cvtsd2ss: lane is the SOURCE.
                let (o, lane, scalar) = match mand {
                    Mand::None => (Op::Cvtp2p, F32, false),
                    Mand::P66 => (Op::Cvtp2p, F64, false),
                    Mand::PF3 => (Op::Cvts2s, F32, true),
                    Mand::PF2 => (Op::Cvts2s, F64, true),
                };
                return Ok(self.sse(o, lane, scalar, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x5b => {
                let m = self.modrm()?;
                // cvtdq2ps / cvtps2dq / cvttps2dq. Lane is the float type.
                let (o, lane) = match mand {
                    Mand::None => (Op::Cvtdq2p, F32),
                    Mand::P66 => (Op::Cvtp2dq { truncate: false }, F32),
                    Mand::PF3 => (Op::Cvtp2dq { truncate: true }, F32),
                    Mand::PF2 => return Ok(self.unk()),
                };
                return Ok(self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x60..=0x6b | 0x6c | 0x6d => {
                let m = self.modrm()?;
                if matches!(op, 0x6c | 0x6d) && mand != Mand::P66 {
                    return Ok(self.unk());
                }
                let (o, lane) = match op {
                    0x60 => (Op::Punpckl, I8),
                    0x61 => (Op::Punpckl, I16),
                    0x62 => (Op::Punpckl, I32),
                    0x63 => (Op::Packss, I16),
                    0x64 => (Op::Pcmpgt, I8),
                    0x65 => (Op::Pcmpgt, I16),
                    0x66 => (Op::Pcmpgt, I32),
                    0x67 => (Op::Packus, I16),
                    0x68 => (Op::Punpckh, I8),
                    0x69 => (Op::Punpckh, I16),
                    0x6a => (Op::Punpckh, I32),
                    0x6b => (Op::Packss, I32),
                    0x6c => (Op::Punpckl, I64),
                    _ => (Op::Punpckh, I64),
                };
                return Ok(int_form(self, &m, o, lane));
            }
            0x6e => {
                // movd/movq xmm|mm, r/m32|64
                let m = self.modrm()?;
                let w = if self.rex_w() { Width::Q } else { Width::D };
                let dst = if mand == Mand::P66 { Self::xmm(m.reg) } else { Self::mm(m.reg) };
                let mut i = self.sse(Op::Movd, if w == Width::Q { I64 } else { I32 }, false, dst, self.e(&m, w));
                i.width = w;
                i.mmx = mand != Mand::P66;
                return Ok(i);
            }
            0x6f => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::P66 => self.sse(Op::Movaps, I64, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::PF3 => self.sse(Op::Movups, I64, false, Self::xmm(m.reg), Self::ex(&m)),
                    Mand::None => {
                        let mut i = self.sse(Op::Movq, I64, false, Self::mm(m.reg), Self::em(&m));
                        i.mmx = true;
                        i
                    }
                    Mand::PF2 => self.unk(),
                });
            }
            0x70 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                return Ok(match mand {
                    Mand::P66 => self.sse3(Op::Pshufd, I32, false, Self::xmm(m.reg), Self::ex(&m), Operand::Imm(imm)),
                    Mand::PF3 => self.sse3(Op::Pshufhw, I16, false, Self::xmm(m.reg), Self::ex(&m), Operand::Imm(imm)),
                    Mand::PF2 => self.sse3(Op::Pshuflw, I16, false, Self::xmm(m.reg), Self::ex(&m), Operand::Imm(imm)),
                    Mand::None => {
                        let mut i = self.sse3(Op::Pshufw, I16, false, Self::mm(m.reg), Self::em(&m), Operand::Imm(imm));
                        i.mmx = true;
                        i
                    }
                });
            }
            0x71..=0x73 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                if !m.is_reg {
                    return Ok(self.unk());
                }
                let lane = match op {
                    0x71 => I16,
                    0x72 => I32,
                    _ => I64,
                };
                let o = match ((m.raw >> 3) & 7, op) {
                    (2, _) => Op::Psrl,
                    (4, 0x71 | 0x72) => Op::Psra,
                    (6, _) => Op::Psll,
                    (3, 0x73) if mand == Mand::P66 => Op::Psrldq,
                    (7, 0x73) if mand == Mand::P66 => Op::Pslldq,
                    _ => return Ok(self.unk()),
                };
                let dst = if mand == Mand::P66 { Self::xmm(m.rm) } else { Self::mm(m.rm) };
                let mut i = self.sse(o, lane, false, dst, Operand::Imm(imm));
                i.mmx = mand != Mand::P66;
                return Ok(i);
            }
            0x74..=0x76 => {
                let m = self.modrm()?;
                let lane = match op {
                    0x74 => I8,
                    0x75 => I16,
                    _ => I32,
                };
                return Ok(int_form(self, &m, Op::Pcmpeq, lane));
            }
            0x7c | 0x7d => {
                let m = self.modrm()?;
                let lane = match mand {
                    Mand::P66 => F64,
                    Mand::PF2 => F32,
                    _ => return Ok(self.unk()),
                };
                let o = if op == 0x7c { Op::Haddp } else { Op::Hsubp };
                return Ok(self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0x7e => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::PF3 => self.sse(Op::Movq, I64, false, Self::xmm(m.reg), Self::ex(&m)),
                    _ => {
                        let w = if self.rex_w() { Width::Q } else { Width::D };
                        let src = if mand == Mand::P66 { Self::xmm(m.reg) } else { Self::mm(m.reg) };
                        let mut i = self.sse(Op::Movd, if w == Width::Q { I64 } else { I32 }, false, self.e(&m, w), src);
                        i.width = w;
                        i.mmx = mand != Mand::P66;
                        i
                    }
                });
            }
            0x7f => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::P66 => self.sse(Op::Movaps, I64, false, Self::ex(&m), Self::xmm(m.reg)),
                    Mand::PF3 => self.sse(Op::Movups, I64, false, Self::ex(&m), Self::xmm(m.reg)),
                    Mand::None => {
                        let mut i = self.sse(Op::Movq, I64, false, Self::em(&m), Self::mm(m.reg));
                        i.mmx = true;
                        i
                    }
                    Mand::PF2 => self.unk(),
                });
            }
            0xc2 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                let Some((lane, scalar)) = fl(mand) else { return Ok(self.unk()) };
                return Ok(self.sse3(Op::Cmpp, lane, scalar, Self::xmm(m.reg), Self::ex(&m), Operand::Imm(imm)));
            }
            0xc3 => {
                let m = self.modrm()?;
                if m.is_reg || mand != Mand::None {
                    return Ok(self.unk());
                }
                let w = if self.rex_w() { Width::Q } else { Width::D };
                return Ok(self.i2(Op::Movnti, w, Operand::Mem(m.mem), self.g(&m, w)));
            }
            0xc4 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                let dst = if mand == Mand::P66 { Self::xmm(m.reg) } else { Self::mm(m.reg) };
                let mut i = self.sse3(Op::Pinsr, I16, false, dst, self.e(&m, Width::D), Operand::Imm(imm));
                i.width = Width::W;
                i.mmx = mand != Mand::P66;
                return Ok(i);
            }
            0xc5 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                if !m.is_reg {
                    return Ok(self.unk());
                }
                let src = if mand == Mand::P66 { Self::xmm(m.rm) } else { Self::mm(m.rm) };
                let mut i = self.sse3(Op::Pextr, I16, false, self.g(&m, Width::D), src, Operand::Imm(imm));
                i.width = Width::W;
                i.mmx = mand != Mand::P66;
                return Ok(i);
            }
            0xc6 => {
                let m = self.modrm()?;
                let imm = self.i8()? & 0xff;
                let lane = match mand {
                    Mand::None => F32,
                    Mand::P66 => F64,
                    _ => return Ok(self.unk()),
                };
                return Ok(self.sse3(Op::Shufp, lane, false, Self::xmm(m.reg), Self::ex(&m), Operand::Imm(imm)));
            }
            0xd0 => {
                let m = self.modrm()?;
                let lane = match mand {
                    Mand::P66 => F64,
                    Mand::PF2 => F32,
                    _ => return Ok(self.unk()),
                };
                return Ok(self.sse(Op::Addsubp, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0xd6 => {
                let m = self.modrm()?;
                return Ok(match mand {
                    Mand::P66 => self.sse(Op::Movq, I64, false, Self::ex(&m), Self::xmm(m.reg)),
                    // movq2dq / movdq2q
                    Mand::PF3 if m.is_reg => {
                        let mut i = self.sse(Op::Movq, I64, false, Self::xmm(m.reg), Self::mm(m.rm));
                        i.mmx = true;
                        i
                    }
                    Mand::PF2 if m.is_reg => {
                        let mut i = self.sse(Op::Movq, I64, false, Self::mm(m.reg), Self::xmm(m.rm));
                        i.mmx = true;
                        i
                    }
                    _ => self.unk(),
                });
            }
            0xd7 => {
                let m = self.modrm()?;
                if !m.is_reg {
                    return Ok(self.unk());
                }
                let src = if mand == Mand::P66 { Self::xmm(m.rm) } else { Self::mm(m.rm) };
                let mut i = self.sse(Op::Pmovmskb, I8, false, self.g(&m, Width::D), src);
                i.width = Width::D;
                i.mmx = mand != Mand::P66;
                return Ok(i);
            }
            0xe6 => {
                let m = self.modrm()?;
                let (o, lane) = match mand {
                    Mand::P66 => (Op::Cvtp2dq { truncate: true }, F64),
                    Mand::PF3 => (Op::Cvtdq2p, F64),
                    Mand::PF2 => (Op::Cvtp2dq { truncate: false }, F64),
                    Mand::None => return Ok(self.unk()),
                };
                return Ok(self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)));
            }
            0xe7 => {
                let m = self.modrm()?;
                if m.is_reg {
                    return Ok(self.unk());
                }
                return Ok(match mand {
                    Mand::P66 => self.sse(Op::Movaps, I64, false, Operand::Mem(m.mem), Self::xmm(m.reg)),
                    Mand::None => {
                        let mut i = self.sse(Op::Movq, I64, false, Operand::Mem(m.mem), Self::mm(m.reg));
                        i.mmx = true;
                        i
                    }
                    _ => self.unk(),
                });
            }
            0xf0 => {
                let m = self.modrm()?;
                if mand != Mand::PF2 || m.is_reg {
                    return Ok(self.unk());
                }
                return Ok(self.sse(Op::Movups, I64, false, Self::xmm(m.reg), Operand::Mem(m.mem)));
            }
            0xf7 => {
                let m = self.modrm()?;
                if !m.is_reg {
                    return Ok(self.unk());
                }
                let mut i = match mand {
                    Mand::P66 => self.sse(Op::Maskmov, I8, false, Self::xmm(m.reg), Self::xmm(m.rm)),
                    _ => {
                        let mut i = self.sse(Op::Maskmov, I8, false, Self::mm(m.reg), Self::mm(m.rm));
                        i.mmx = true;
                        i
                    }
                };
                i.width = Width::Q;
                return Ok(i);
            }
            // The regular packed-integer block.
            0xd1..=0xd5 | 0xd8..=0xdf | 0xe0..=0xe5 | 0xe8..=0xef | 0xf1..=0xf6 | 0xf8..=0xfe => {
                let m = self.modrm()?;
                let (o, lane) = match op {
                    0xd1 => (Op::Psrl, I16),
                    0xd2 => (Op::Psrl, I32),
                    0xd3 => (Op::Psrl, I64),
                    0xd4 => (Op::Padd, I64),
                    0xd5 => (Op::Pmullw, I16),
                    0xd8 => (Op::Psubus, I8),
                    0xd9 => (Op::Psubus, I16),
                    0xda => (Op::Pminu, I8),
                    0xdb => (Op::Pand, I64),
                    0xdc => (Op::Paddus, I8),
                    0xdd => (Op::Paddus, I16),
                    0xde => (Op::Pmaxu, I8),
                    0xdf => (Op::Pandn, I64),
                    0xe0 => (Op::Pavg, I8),
                    0xe1 => (Op::Psra, I16),
                    0xe2 => (Op::Psra, I32),
                    0xe3 => (Op::Pavg, I16),
                    0xe4 => (Op::Pmulhuw, I16),
                    0xe5 => (Op::Pmulhw, I16),
                    0xe8 => (Op::Psubs, I8),
                    0xe9 => (Op::Psubs, I16),
                    0xea => (Op::Pmins, I16),
                    0xeb => (Op::Por, I64),
                    0xec => (Op::Padds, I8),
                    0xed => (Op::Padds, I16),
                    0xee => (Op::Pmaxs, I16),
                    0xef => (Op::Pxor, I64),
                    0xf1 => (Op::Psll, I16),
                    0xf2 => (Op::Psll, I32),
                    0xf3 => (Op::Psll, I64),
                    0xf4 => (Op::Pmuludq, I32),
                    0xf5 => (Op::Pmaddwd, I16),
                    0xf6 => (Op::Psadbw, I8),
                    0xf8 => (Op::Psub, I8),
                    0xf9 => (Op::Psub, I16),
                    0xfa => (Op::Psub, I32),
                    0xfb => (Op::Psub, I64),
                    0xfc => (Op::Padd, I8),
                    0xfd => (Op::Padd, I16),
                    _ => (Op::Padd, I32),
                };
                return Ok(int_form(self, &m, o, lane));
            }
            _ => {}
        }
        Ok(self.unk())
    }

    // -- three-byte maps ----------------------------------------------------------

    fn map_0f38(&mut self) -> R<Inst> {
        use Lane::*;
        let op = self.u8()?;
        let mand = self.mand;
        // movbe / crc32 are the non-SIMD residents of this map.
        if op == 0xf0 || op == 0xf1 {
            let m = self.modrm()?;
            return Ok(match mand {
                Mand::None | Mand::P66 => {
                    let w = self.wv();
                    if m.is_reg {
                        return Ok(self.unk());
                    }
                    if op == 0xf0 {
                        self.i2(Op::Movbe, w, self.g(&m, w), Operand::Mem(m.mem))
                    } else {
                        self.i2(Op::Movbe, w, Operand::Mem(m.mem), self.g(&m, w))
                    }
                }
                Mand::PF2 => {
                    let src_w = if op == 0xf0 { Width::B } else { self.wv() };
                    let dst_w = if self.rex_w() { Width::Q } else { Width::D };
                    self.i2(Op::Crc32, src_w, self.g(&m, dst_w), self.e(&m, src_w))
                }
                Mand::PF3 => self.unk(),
            });
        }
        let m = self.modrm()?;
        let (o, lane) = match op {
            0x00 => (Op::Pshufb, I8),
            0x01 => (Op::Phadd, I16),
            0x02 => (Op::Phadd, I32),
            0x03 => (Op::Phadds, I16),
            0x04 => (Op::Pmaddubsw, I8),
            0x05 => (Op::Phsub, I16),
            0x06 => (Op::Phsub, I32),
            0x07 => (Op::Phsubs, I16),
            0x08 => (Op::Psign, I8),
            0x09 => (Op::Psign, I16),
            0x0a => (Op::Psign, I32),
            0x0b => (Op::Pmulhrsw, I16),
            0x10 => (Op::Pblendvb, I8),
            0x14 => (Op::Blendvp, F32),
            0x15 => (Op::Blendvp, F64),
            0x17 => (Op::Ptest, I64),
            0x1c => (Op::Pabs, I8),
            0x1d => (Op::Pabs, I16),
            0x1e => (Op::Pabs, I32),
            0x20 => (Op::Pmovsx(I8, I16), I16),
            0x21 => (Op::Pmovsx(I8, I32), I32),
            0x22 => (Op::Pmovsx(I8, I64), I64),
            0x23 => (Op::Pmovsx(I16, I32), I32),
            0x24 => (Op::Pmovsx(I16, I64), I64),
            0x25 => (Op::Pmovsx(I32, I64), I64),
            0x28 => (Op::Pmuldq, I32),
            0x29 => (Op::Pcmpeq, I64),
            0x2a => (Op::Movups, I64), // movntdqa
            0x2b => (Op::Packus, I32),
            0x30 => (Op::Pmovzx(I8, I16), I16),
            0x31 => (Op::Pmovzx(I8, I32), I32),
            0x32 => (Op::Pmovzx(I8, I64), I64),
            0x33 => (Op::Pmovzx(I16, I32), I32),
            0x34 => (Op::Pmovzx(I16, I64), I64),
            0x35 => (Op::Pmovzx(I32, I64), I64),
            0x37 => (Op::Pcmpgt, I64),
            0x38 => (Op::Pmins, I8),
            0x39 => (Op::Pmins, I32),
            0x3a => (Op::Pminu, I16),
            0x3b => (Op::Pminu, I32),
            0x3c => (Op::Pmaxs, I8),
            0x3d => (Op::Pmaxs, I32),
            0x3e => (Op::Pmaxu, I16),
            0x3f => (Op::Pmaxu, I32),
            0x40 => (Op::Pmulld, I32),
            0x41 => (Op::Phminposuw, I16),
            _ => return Ok(self.unk()),
        };
        // SSSE3 ops (00-0b, 1c-1e) exist in MMX form without 66; the rest
        // need 66.
        let ssse3 = op <= 0x0b || (0x1c..=0x1e).contains(&op);
        Ok(match mand {
            Mand::P66 => self.sse(o, lane, false, Self::xmm(m.reg), Self::ex(&m)),
            Mand::None if ssse3 => {
                let mut i = self.sse(o, lane, false, Self::mm(m.reg), Self::em(&m));
                i.mmx = true;
                i
            }
            _ => self.unk(),
        })
    }

    fn map_0f3a(&mut self) -> R<Inst> {
        use Lane::*;
        let op = self.u8()?;
        let mand = self.mand;
        let m = self.modrm()?;
        let imm = self.i8()? & 0xff;
        let immo = Operand::Imm(imm);
        if op == 0x0f && mand == Mand::None {
            let mut i = self.sse3(Op::Palignr, I8, false, Self::mm(m.reg), Self::em(&m), immo);
            i.mmx = true;
            return Ok(i);
        }
        if mand != Mand::P66 {
            return Ok(self.unk());
        }
        let v = Self::xmm(m.reg);
        Ok(match op {
            0x08 => self.sse3(Op::Roundp, F32, false, v, Self::ex(&m), immo),
            0x09 => self.sse3(Op::Roundp, F64, false, v, Self::ex(&m), immo),
            0x0a => self.sse3(Op::Roundp, F32, true, v, Self::ex(&m), immo),
            0x0b => self.sse3(Op::Roundp, F64, true, v, Self::ex(&m), immo),
            0x0c => self.sse3(Op::Blendp, F32, false, v, Self::ex(&m), immo),
            0x0d => self.sse3(Op::Blendp, F64, false, v, Self::ex(&m), immo),
            0x0e => self.sse3(Op::Pblendw, I16, false, v, Self::ex(&m), immo),
            0x0f => self.sse3(Op::Palignr, I8, false, v, Self::ex(&m), immo),
            0x14 => {
                let mut i = self.sse3(Op::Pextr, I8, false, self.e(&m, Width::D), v, immo);
                i.width = Width::B;
                i
            }
            0x15 => {
                let mut i = self.sse3(Op::Pextr, I16, false, self.e(&m, Width::D), v, immo);
                i.width = Width::W;
                i
            }
            0x16 => {
                let w = if self.rex_w() { Width::Q } else { Width::D };
                let mut i = self.sse3(Op::Pextr, if w == Width::Q { I64 } else { I32 }, false, self.e(&m, w), v, immo);
                i.width = w;
                i
            }
            0x17 => {
                let mut i = self.sse3(Op::Extractps, F32, false, self.e(&m, Width::D), v, immo);
                i.width = Width::D;
                i
            }
            0x20 => {
                let mut i = self.sse3(Op::Pinsr, I8, false, v, self.e(&m, Width::D), immo);
                i.width = Width::B;
                i
            }
            0x21 => self.sse3(Op::Insertps, F32, false, v, Self::ex(&m), immo),
            0x22 => {
                let w = if self.rex_w() { Width::Q } else { Width::D };
                let mut i = self.sse3(Op::Pinsr, if w == Width::Q { I64 } else { I32 }, false, v, self.e(&m, w), immo);
                i.width = w;
                i
            }
            0x40 => self.sse3(Op::Dpp, F32, false, v, Self::ex(&m), immo),
            0x41 => self.sse3(Op::Dpp, F64, false, v, Self::ex(&m), immo),
            0x42 => self.sse3(Op::Mpsadbw, I8, false, v, Self::ex(&m), immo),
            0x44 => self.sse3(Op::Pclmulqdq, I64, false, v, Self::ex(&m), immo),
            0x60 => self.sse3(Op::Pcmpestrm, I8, false, v, Self::ex(&m), immo),
            0x61 => self.sse3(Op::Pcmpestri, I8, false, v, Self::ex(&m), immo),
            0x62 => self.sse3(Op::Pcmpistrm, I8, false, v, Self::ex(&m), immo),
            0x63 => self.sse3(Op::Pcmpistri, I8, false, v, Self::ex(&m), immo),
            _ => self.unk(),
        })
    }

    // -- x87 --------------------------------------------------------------------

    fn x87(&mut self, op: u8) -> R<Inst> {
        use X87Mem::*;
        let m = self.modrm()?;
        let r = (m.raw >> 3) & 7;
        let i = m.raw & 7;
        let mem = Operand::Mem(m.mem);
        let f = |d: &Decoder, o: Op| d.i0(o, Width::Q);
        let fm = |d: &Decoder, o: Op| d.i1(o, Width::Q, mem);
        if !m.is_reg {
            let o = match (op, r) {
                (0xd8, 0) => Op::Fadd(X87Src::MemF(Width::D)),
                (0xd8, 1) => Op::Fmul(X87Src::MemF(Width::D)),
                (0xd8, 2) => Op::Fcom { src: X87Src::MemF(Width::D), pop: 0 },
                (0xd8, 3) => Op::Fcom { src: X87Src::MemF(Width::D), pop: 1 },
                (0xd8, 4) => Op::Fsub(X87Src::MemF(Width::D)),
                (0xd8, 5) => Op::Fsubr(X87Src::MemF(Width::D)),
                (0xd8, 6) => Op::Fdiv(X87Src::MemF(Width::D)),
                (0xd8, _) => Op::Fdivr(X87Src::MemF(Width::D)),
                (0xd9, 0) => Op::Fld(F32),
                (0xd9, 2) => Op::Fst { mem: F32, pop: false },
                (0xd9, 3) => Op::Fst { mem: F32, pop: true },
                (0xd9, 4) => Op::Fldenv,
                (0xd9, 5) => Op::Fldcw,
                (0xd9, 6) => Op::Fnstenv,
                (0xd9, 7) => Op::Fnstcw,
                (0xda, 0) => Op::Fadd(X87Src::MemI(Width::D)),
                (0xda, 1) => Op::Fmul(X87Src::MemI(Width::D)),
                (0xda, 2) => Op::Ficom { mem: I32, pop: false },
                (0xda, 3) => Op::Ficom { mem: I32, pop: true },
                (0xda, 4) => Op::Fsub(X87Src::MemI(Width::D)),
                (0xda, 5) => Op::Fsubr(X87Src::MemI(Width::D)),
                (0xda, 6) => Op::Fdiv(X87Src::MemI(Width::D)),
                (0xda, _) => Op::Fdivr(X87Src::MemI(Width::D)),
                (0xdb, 0) => Op::Fild(I32),
                (0xdb, 1) => Op::Fist { mem: I32, pop: true, truncate: true },
                (0xdb, 2) => Op::Fist { mem: I32, pop: false, truncate: false },
                (0xdb, 3) => Op::Fist { mem: I32, pop: true, truncate: false },
                (0xdb, 5) => Op::Fld(F80),
                (0xdb, 7) => Op::Fst { mem: F80, pop: true },
                (0xdc, 0) => Op::Fadd(X87Src::MemF(Width::Q)),
                (0xdc, 1) => Op::Fmul(X87Src::MemF(Width::Q)),
                (0xdc, 2) => Op::Fcom { src: X87Src::MemF(Width::Q), pop: 0 },
                (0xdc, 3) => Op::Fcom { src: X87Src::MemF(Width::Q), pop: 1 },
                (0xdc, 4) => Op::Fsub(X87Src::MemF(Width::Q)),
                (0xdc, 5) => Op::Fsubr(X87Src::MemF(Width::Q)),
                (0xdc, 6) => Op::Fdiv(X87Src::MemF(Width::Q)),
                (0xdc, _) => Op::Fdivr(X87Src::MemF(Width::Q)),
                (0xdd, 0) => Op::Fld(F64),
                (0xdd, 1) => Op::Fist { mem: I64, pop: true, truncate: true },
                (0xdd, 2) => Op::Fst { mem: F64, pop: false },
                (0xdd, 3) => Op::Fst { mem: F64, pop: true },
                (0xdd, 4) => Op::Frstor,
                (0xdd, 6) => Op::Fnsave,
                (0xdd, 7) => Op::Fnstsw,
                (0xde, 0) => Op::Fadd(X87Src::MemI(Width::W)),
                (0xde, 1) => Op::Fmul(X87Src::MemI(Width::W)),
                (0xde, 2) => Op::Ficom { mem: I16, pop: false },
                (0xde, 3) => Op::Ficom { mem: I16, pop: true },
                (0xde, 4) => Op::Fsub(X87Src::MemI(Width::W)),
                (0xde, 5) => Op::Fsubr(X87Src::MemI(Width::W)),
                (0xde, 6) => Op::Fdiv(X87Src::MemI(Width::W)),
                (0xde, _) => Op::Fdivr(X87Src::MemI(Width::W)),
                (0xdf, 0) => Op::Fild(I16),
                (0xdf, 1) => Op::Fist { mem: I16, pop: true, truncate: true },
                (0xdf, 2) => Op::Fist { mem: I16, pop: false, truncate: false },
                (0xdf, 3) => Op::Fist { mem: I16, pop: true, truncate: false },
                (0xdf, 5) => Op::Fild(I64),
                (0xdf, 7) => Op::Fist { mem: I64, pop: true, truncate: false },
                // fbld / fbstp (packed BCD) and the reserved slots.
                _ => return Ok(self.unk()),
            };
            // The environment's image has a 16-bit form (14 bytes, 94 for
            // fnsave), which the operand-size prefix selects.
            if matches!(o, Op::Fldenv | Op::Fnstenv | Op::Frstor | Op::Fnsave) && self.op16 {
                return Ok(self.i1(o, Width::W, mem));
            }
            return Ok(fm(self, o));
        }
        let o = match (op, r) {
            (0xd8, 0) => Op::Fadd(X87Src::St0Sti(i)),
            (0xd8, 1) => Op::Fmul(X87Src::St0Sti(i)),
            (0xd8, 2) => Op::Fcom { src: X87Src::St0Sti(i), pop: 0 },
            (0xd8, 3) => Op::Fcom { src: X87Src::St0Sti(i), pop: 1 },
            (0xd8, 4) => Op::Fsub(X87Src::St0Sti(i)),
            (0xd8, 5) => Op::Fsubr(X87Src::St0Sti(i)),
            (0xd8, 6) => Op::Fdiv(X87Src::St0Sti(i)),
            (0xd8, _) => Op::Fdivr(X87Src::St0Sti(i)),
            (0xd9, 0) => Op::FldSt(i),
            (0xd9, 1) => Op::Fxch(i),
            (0xd9, 2) if i == 0 => Op::Fnop,
            (0xd9, 3) => Op::FstSt { i, pop: true }, // fstp1, an alias
            (0xd9, 4) => match i {
                0 => Op::Fchs,
                1 => Op::Fabs,
                4 => Op::Ftst,
                5 => Op::Fxam,
                _ => return Ok(self.unk()),
            },
            (0xd9, 5) => match i {
                0 => Op::Fld1,
                1 => Op::Fldl2t,
                2 => Op::Fldl2e,
                3 => Op::Fldpi,
                4 => Op::Fldlg2,
                5 => Op::Fldln2,
                6 => Op::Fldz,
                _ => return Ok(self.unk()),
            },
            (0xd9, 6) => match i {
                0 => Op::F2xm1,
                1 => Op::Fyl2x,
                2 => Op::Fptan,
                3 => Op::Fpatan,
                4 => Op::Fxtract,
                5 => Op::Fprem1,
                6 => Op::Fdecstp,
                _ => Op::Fincstp,
            },
            (0xd9, _) => match i {
                0 => Op::Fprem,
                1 => Op::Fyl2xp1,
                2 => Op::Fsqrt,
                3 => Op::Fsincos,
                4 => Op::Frndint,
                5 => Op::Fscale,
                6 => Op::Fsin,
                _ => Op::Fcos,
            },
            (0xda, 0) => Op::Fcmovcc { cond: Cond(2), i },  // fcmovb
            (0xda, 1) => Op::Fcmovcc { cond: Cond(4), i },  // fcmove
            (0xda, 2) => Op::Fcmovcc { cond: Cond(6), i },  // fcmovbe
            (0xda, 3) => Op::Fcmovcc { cond: Cond(10), i }, // fcmovu
            (0xda, 5) if i == 1 => Op::Fucom { i: 1, pop: 2 }, // fucompp
            (0xdb, 0) => Op::Fcmovcc { cond: Cond(3), i },  // fcmovnb
            (0xdb, 1) => Op::Fcmovcc { cond: Cond(5), i },  // fcmovne
            (0xdb, 2) => Op::Fcmovcc { cond: Cond(7), i },  // fcmovnbe
            (0xdb, 3) => Op::Fcmovcc { cond: Cond(11), i }, // fcmovnu
            (0xdb, 4) => match i {
                2 => Op::Fnclex,
                3 => Op::Fninit,
                _ => return Ok(self.unk()),
            },
            (0xdb, 5) => Op::Fcomi { i, pop: false, unordered: true },
            (0xdb, 6) => Op::Fcomi { i, pop: false, unordered: false },
            (0xdc, 0) => Op::Fadd(X87Src::StiSt0 { i, pop: false }),
            (0xdc, 1) => Op::Fmul(X87Src::StiSt0 { i, pop: false }),
            (0xdc, 2) => Op::Fcom { src: X87Src::St0Sti(i), pop: 0 }, // fcom2 alias
            (0xdc, 3) => Op::Fcom { src: X87Src::St0Sti(i), pop: 1 }, // fcomp3 alias
            (0xdc, 4) => Op::Fsubr(X87Src::StiSt0 { i, pop: false }),
            (0xdc, 5) => Op::Fsub(X87Src::StiSt0 { i, pop: false }),
            (0xdc, 6) => Op::Fdivr(X87Src::StiSt0 { i, pop: false }),
            (0xdc, _) => Op::Fdiv(X87Src::StiSt0 { i, pop: false }),
            (0xdd, 0) => Op::Ffree(i),
            (0xdd, 1) => Op::Fxch(i), // fxch4 alias
            (0xdd, 2) => Op::FstSt { i, pop: false },
            (0xdd, 3) => Op::FstSt { i, pop: true },
            (0xdd, 4) => Op::Fucom { i, pop: 0 },
            (0xdd, 5) => Op::Fucom { i, pop: 1 },
            (0xde, 0) => Op::Fadd(X87Src::StiSt0 { i, pop: true }),
            (0xde, 1) => Op::Fmul(X87Src::StiSt0 { i, pop: true }),
            (0xde, 2) => Op::Fcom { src: X87Src::St0Sti(i), pop: 1 }, // fcomp5 alias
            (0xde, 3) if i == 1 => Op::Fcom { src: X87Src::St0Sti(1), pop: 2 }, // fcompp
            (0xde, 4) => Op::Fsubr(X87Src::StiSt0 { i, pop: true }),
            (0xde, 5) => Op::Fsub(X87Src::StiSt0 { i, pop: true }),
            (0xde, 6) => Op::Fdivr(X87Src::StiSt0 { i, pop: true }),
            (0xde, 7) => Op::Fdiv(X87Src::StiSt0 { i, pop: true }),
            (0xdf, 0) => Op::Ffreep(i),
            (0xdf, 1) => Op::Fxch(i), // fxch7 alias
            (0xdf, 2) | (0xdf, 3) => Op::FstSt { i, pop: true }, // fstp8/fstp9 aliases
            (0xdf, 4) if i == 0 => Op::FnstswAx,
            (0xdf, 5) => Op::Fcomi { i, pop: true, unordered: true },
            (0xdf, 6) => Op::Fcomi { i, pop: true, unordered: false },
            _ => return Ok(self.unk()),
        };
        Ok(f(self, o))
    }

    // -- VEX / EVEX: length only ----------------------------------------------------

    /// Consume a VEX-encoded instruction far enough to know its length. The
    /// result is `Unknown`: gex advertises no AVX.
    fn vex(&mut self, first: u8) -> R<Inst> {
        let map;
        let pp;
        if first == 0xc5 {
            let b1 = self.u8()?;
            map = 1;
            pp = b1 & 3;
        } else {
            let b1 = self.u8()?;
            let b2 = self.u8()?;
            map = b1 & 0x1f;
            pp = b2 & 3;
        }
        let _ = pp;
        let op = self.u8()?;
        // Every VEX instruction in maps 1-3 has a ModRM except vzeroupper
        // (map 1, 0x77) and a few others without operands.
        if map == 1 && op == 0x77 {
            return Ok(self.unk());
        }
        let _m = self.modrm()?;
        let has_imm = match map {
            3 => true,
            1 => matches!(op, 0x70..=0x73 | 0xc2 | 0xc4 | 0xc5 | 0xc6),
            2 => false,
            _ => false,
        };
        if has_imm {
            self.u8()?;
        }
        Ok(self.unk())
    }

    /// The same for EVEX (`62` is always EVEX in 64-bit mode).
    fn evex(&mut self) -> R<Inst> {
        let p0 = self.u8()?;
        let _p1 = self.u8()?;
        let _p2 = self.u8()?;
        let map = p0 & 7;
        let op = self.u8()?;
        let _m = self.modrm()?;
        let has_imm = match map {
            3 => true,
            1 => matches!(op, 0x70..=0x73 | 0xc2 | 0xc4 | 0xc5 | 0xc6),
            _ => false,
        };
        if has_imm {
            self.u8()?;
        }
        Ok(self.unk())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(bytes: &[u8]) -> Inst {
        decode(bytes, 0x1000)
    }

    #[test]
    fn simple_moves() {
        let i = d(&[0x48, 0x89, 0xe5]); // mov rbp, rsp
        assert_eq!(i.op, Op::Mov);
        assert_eq!(i.len, 3);
        assert_eq!(i.width, Width::Q);
        assert_eq!(i.op0(), Operand::Reg(Reg::Gp(5, Width::Q)));
        assert_eq!(i.op1(), Operand::Reg(Reg::Gp(4, Width::Q)));
    }

    #[test]
    fn rip_relative_is_absolute() {
        // lea rdi, [rip+0x10] at 0x1000, 7 bytes: target 0x1017
        let i = d(&[0x48, 0x8d, 0x3d, 0x10, 0x00, 0x00, 0x00]);
        assert_eq!(i.op, Op::Lea);
        match i.op1() {
            Operand::Mem(m) => {
                assert!(m.rip);
                assert_eq!(m.disp, 0x1017);
                assert_eq!(m.base, None);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn high_byte_registers_without_rex() {
        let i = d(&[0x88, 0xe0]); // mov al, ah
        assert_eq!(i.op0(), Operand::Reg(Reg::Gp(0, Width::B)));
        assert_eq!(i.op1(), Operand::Reg(Reg::High(0)));
        let i = d(&[0x40, 0x88, 0xe0]); // mov al, spl
        assert_eq!(i.op1(), Operand::Reg(Reg::Gp(4, Width::B)));
    }

    #[test]
    fn sib_and_scaled_index() {
        // mov rax, [rbx+rcx*8+0x10]
        let i = d(&[0x48, 0x8b, 0x44, 0xcb, 0x10]);
        match i.op1() {
            Operand::Mem(m) => {
                assert_eq!(m.base, Some(3));
                assert_eq!(m.index, Some(1));
                assert_eq!(m.scale, 8);
                assert_eq!(m.disp, 0x10);
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(i.len, 5);
    }

    #[test]
    fn branches_have_absolute_targets() {
        let i = d(&[0x74, 0x05]); // je +5
        assert_eq!(i.op, Op::Jcc(Cond(4)));
        assert_eq!(i.op0(), Operand::Imm(0x1007));
        let i = d(&[0xe8, 0x00, 0x01, 0x00, 0x00]); // call +0x100
        assert_eq!(i.op0(), Operand::Imm(0x1105));
    }

    #[test]
    fn syscall_and_ret() {
        assert_eq!(d(&[0x0f, 0x05]).op, Op::Syscall);
        assert_eq!(d(&[0x0f, 0x05]).len, 2);
        assert_eq!(d(&[0xc3]).op, Op::Ret);
    }

    #[test]
    fn sse_forms() {
        let i = d(&[0x66, 0x0f, 0xef, 0xc0]); // pxor xmm0, xmm0
        assert_eq!(i.op, Op::Pxor);
        assert!(!i.mmx);
        let i = d(&[0xf2, 0x0f, 0x10, 0x05, 0, 0, 0, 0]); // movsd xmm0, [rip]
        assert_eq!(i.op, Op::Movsd);
        assert!(i.scalar);
        let i = d(&[0x66, 0x48, 0x0f, 0x7e, 0xc0]); // movq rax, xmm0
        assert_eq!(i.op, Op::Movd);
        assert_eq!(i.width, Width::Q);
        assert_eq!(i.op0(), Operand::Reg(Reg::Gp(0, Width::Q)));
    }

    #[test]
    fn truncated_is_unknown() {
        let i = d(&[0x48, 0x8b]);
        assert_eq!(i.op, Op::Unknown);
        assert_eq!(i.len, 1);
    }

    #[test]
    fn x87_forms() {
        let i = d(&[0xdd, 0xd8]); // fstp st(0)
        assert_eq!(i.op, Op::FstSt { i: 0, pop: true });
        let i = d(&[0xde, 0xc1]); // faddp st(1), st
        assert_eq!(i.op, Op::Fadd(X87Src::StiSt0 { i: 1, pop: true }));
        let i = d(&[0xdd, 0x45, 0xf8]); // fld qword [rbp-8]
        assert_eq!(i.op, Op::Fld(X87Mem::F64));
    }
}
