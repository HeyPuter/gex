//! `gex-lift` — AArch64 instruction semantics, expressed in GIR.
//!
//! One function per instruction family, hand-written against the Arm ARM
//! pseudocode (PLAN §5.3). The lifter's contract:
//!
//! * **Total.** Every [`Inst`] either lifts to GIR or ends the block with
//!   [`Term::Illegal`]. Nothing is silently skipped, so an instruction gex
//!   cannot execute becomes a precise SIGILL at run time rather than a wrong
//!   answer (PLAN §6.1).
//! * **Exact.** Where the architecture and wasm disagree — division by zero,
//!   `INT_MIN / -1`, shift-amount masking — the lifter inserts the guard, so
//!   the backend never has to know.
//! * **Flags on demand.** NZCV is written as four separate one-bit registers;
//!   dead-flag elimination in [`gex_ir::pass`] removes the ones nobody reads.

#![forbid(unsafe_code)]

use gex_ir::*;
use gex_isa::{
    AddOp, AddrMode, BarrierKind, BfOp, Cond, Dp1Op, Dp2Op, Dp3Op, Extend, Inst, LogOp, MemSz,
    MoveOp, Ordering as IsaOrder, RegSel, RmwOp as IsaRmw, ShiftType, Sz, SysMaintOp, SysReg,
};

mod block;
pub use block::{lift_block, BlockLimits};

/// Guest instruction width. A64 is fixed-width; the whole design leans on it.
pub const INST_BYTES: u64 = 4;

/// Lifting state for one basic block.
pub struct Lifter<'a> {
    b: &'a mut Builder,
}

/// What the lifter decided about control flow after one instruction.
pub enum Flow {
    /// Continue with the next instruction.
    Next,
    /// End the block with this terminator.
    End(Term),
}

fn ty_of(sz: Sz) -> Ty {
    match sz {
        Sz::W => Ty::I32,
        Sz::X => Ty::I64,
    }
}

fn mem_sz(sz: MemSz) -> gex_ir::MemSz {
    match sz {
        MemSz::B => gex_ir::MemSz::B1,
        MemSz::H => gex_ir::MemSz::B2,
        MemSz::W => gex_ir::MemSz::B4,
        // A 16-byte access only appears in SIMD load/store, which the decoder
        // still rejects; mapping it keeps this function total.
        MemSz::D | MemSz::Q => gex_ir::MemSz::B8,
    }
}

fn order_of(o: IsaOrder) -> Order {
    match o {
        IsaOrder::None => Order::Relaxed,
        // Acquire and release both map to seq-cst, which is stronger than
        // either and therefore always sound (PLAN §5.8).
        IsaOrder::Acquire | IsaOrder::Release => Order::SeqCst,
    }
}

/// Carry input to [`Lifter::add_with_carry`].
enum Carry {
    Zero,
    One,
    Flag(Value),
}

/// Result of an add-with-carry: the sum plus the flags it would set.
struct AddResult {
    value: Value,
    n: Value,
    z: Value,
    c: Value,
    v: Value,
}

/// A resolved addressing mode.
struct Address {
    base: Value,
    offset: i64,
    /// Post-index writeback to apply after the access.
    post: Option<(RegSel, i64)>,
}

impl<'a> Lifter<'a> {
    pub fn new(b: &'a mut Builder) -> Lifter<'a> {
        Lifter { b }
    }

    // -- register access ---------------------------------------------------

    /// Read a general-purpose register at width `sz`.
    fn rd_gp(&mut self, r: RegSel, sz: Sz) -> Value {
        let full = match r {
            RegSel::Zr => return self.b.const_ty(ty_of(sz), 0),
            RegSel::Sp => self.b.read(GuestReg::Sp),
            RegSel::Gp(n) => self.b.read(GuestReg::X(n)),
        };
        match sz {
            Sz::X => full,
            Sz::W => self.b.wrap(full),
        }
    }

    /// Write a general-purpose register at width `sz`. A 32-bit write
    /// zero-extends into the 64-bit register, as the architecture requires.
    fn wr_gp(&mut self, r: RegSel, sz: Sz, val: Value) {
        let full = match sz {
            Sz::X => val,
            Sz::W => self.b.extend(val, false),
        };
        match r {
            RegSel::Zr => {}
            RegSel::Sp => self.b.write(GuestReg::Sp, full),
            RegSel::Gp(n) => self.b.write(GuestReg::X(n), full),
        }
    }

    /// Read a 64-bit address-typed register (never narrowed).
    fn rd_addr(&mut self, r: RegSel) -> Value {
        match r {
            RegSel::Zr => self.b.const_i64(0),
            RegSel::Sp => self.b.read(GuestReg::Sp),
            RegSel::Gp(n) => self.b.read(GuestReg::X(n)),
        }
    }

    fn wr_addr(&mut self, r: RegSel, val: Value) {
        match r {
            RegSel::Zr => {}
            RegSel::Sp => self.b.write(GuestReg::Sp, val),
            RegSel::Gp(n) => self.b.write(GuestReg::X(n), val),
        }
    }

    fn konst(&mut self, ty: Ty, v: u64) -> Value {
        self.b.const_ty(ty, v)
    }

    // -- shared helpers ----------------------------------------------------

    /// Apply a shift-by-constant to a register value.
    fn shift(&mut self, v: Value, ty: Ty, kind: ShiftType, amount: u32) -> Value {
        if amount == 0 {
            return v;
        }
        let amt = self.konst(ty, amount as u64);
        let op = match kind {
            ShiftType::Lsl => BinOp::Shl,
            ShiftType::Lsr => BinOp::ShrU,
            ShiftType::Asr => BinOp::ShrS,
            ShiftType::Ror => BinOp::Rotr,
        };
        self.b.bin(op, ty, v, amt)
    }

    /// `ExtendReg`: take the low bits of `rm`, extend them, then shift left.
    /// The result is `ty`-typed, matching the enclosing operation's width.
    fn extend_reg(&mut self, rm: RegSel, ext: Extend, amount: u32, ty: Ty) -> Value {
        let raw = match ty {
            Ty::I64 => self.rd_addr(rm),
            Ty::I32 => self.rd_gp(rm, Sz::W),
        };
        let bits = ext.src_bits().min(ty.bits());
        let v = if bits >= ty.bits() {
            raw
        } else {
            self.b.def(Expr::ExtendBits {
                a: raw,
                ty,
                bits,
                signed: ext.is_signed(),
            })
        };
        if amount == 0 {
            v
        } else {
            let amt = self.konst(ty, amount as u64);
            self.b.bin(BinOp::Shl, ty, v, amt)
        }
    }

    /// `AddWithCarry` from the Arm ARM: the sum and the four flags it sets.
    fn add_with_carry(&mut self, ty: Ty, a: Value, b: Value, carry: Carry) -> AddResult {
        let s1 = self.b.bin(BinOp::Add, ty, a, b);
        let c1 = self.b.cmp(CmpOp::LtU, ty, s1, a);
        let (sum, carry_out) = match carry {
            Carry::Zero => (s1, c1),
            Carry::One => {
                let one = self.konst(ty, 1);
                let s = self.b.bin(BinOp::Add, ty, s1, one);
                let c2 = self.b.cmp(CmpOp::LtU, ty, s, s1);
                let c = self.b.bin(BinOp::Or, Ty::I32, c1, c2);
                (s, c)
            }
            Carry::Flag(cin) => {
                // The carry flag is an i32 0/1; widen it to the operand width.
                let cin_ty = match ty {
                    Ty::I32 => cin,
                    Ty::I64 => self.b.extend(cin, false),
                };
                let s = self.b.bin(BinOp::Add, ty, s1, cin_ty);
                let c2 = self.b.cmp(CmpOp::LtU, ty, s, s1);
                let c = self.b.bin(BinOp::Or, Ty::I32, c1, c2);
                (s, c)
            }
        };
        let zero = self.konst(ty, 0);
        let n = self.b.cmp(CmpOp::LtS, ty, sum, zero);
        let z = self.b.cmp(CmpOp::Eq, ty, sum, zero);
        // Signed overflow: both operands agree in sign and the result differs.
        let xa = self.b.bin(BinOp::Xor, ty, a, sum);
        let xb = self.b.bin(BinOp::Xor, ty, b, sum);
        let both = self.b.bin(BinOp::And, ty, xa, xb);
        let v = self.b.cmp(CmpOp::LtS, ty, both, zero);
        AddResult {
            value: sum,
            n,
            z,
            c: carry_out,
            v,
        }
    }

    fn set_nzcv(&mut self, r: &AddResult) {
        self.b.write(GuestReg::N, r.n);
        self.b.write(GuestReg::Z, r.z);
        self.b.write(GuestReg::C, r.c);
        self.b.write(GuestReg::V, r.v);
    }

    /// Set N and Z from a result, clearing C and V — the logical-op rule.
    fn set_nz_clear_cv(&mut self, ty: Ty, value: Value) {
        let zero = self.konst(ty, 0);
        let n = self.b.cmp(CmpOp::LtS, ty, value, zero);
        let z = self.b.cmp(CmpOp::Eq, ty, value, zero);
        let zero32 = self.b.const_i32(0);
        self.b.write(GuestReg::N, n);
        self.b.write(GuestReg::Z, z);
        self.b.write(GuestReg::C, zero32);
        self.b.write(GuestReg::V, zero32);
    }

    /// Evaluate a condition code against the current flags, as an `I32` bool.
    fn eval_cond(&mut self, cond: Cond) -> Value {
        if cond.is_always() {
            return self.b.const_i32(1);
        }
        let base = cond.0 >> 1;
        let invert = cond.0 & 1 == 1;
        let one = self.b.const_i32(1);
        let v = match base {
            0b000 => self.b.read(GuestReg::Z),
            0b001 => self.b.read(GuestReg::C),
            0b010 => self.b.read(GuestReg::N),
            0b011 => self.b.read(GuestReg::V),
            0b100 => {
                // HI: C == 1 && Z == 0
                let c = self.b.read(GuestReg::C);
                let z = self.b.read(GuestReg::Z);
                let nz = self.b.bin(BinOp::Xor, Ty::I32, z, one);
                self.b.bin(BinOp::And, Ty::I32, c, nz)
            }
            0b101 => {
                // GE: N == V
                let n = self.b.read(GuestReg::N);
                let vf = self.b.read(GuestReg::V);
                self.b.cmp(CmpOp::Eq, Ty::I32, n, vf)
            }
            _ => {
                // GT: Z == 0 && N == V
                let n = self.b.read(GuestReg::N);
                let vf = self.b.read(GuestReg::V);
                let z = self.b.read(GuestReg::Z);
                let nv = self.b.cmp(CmpOp::Eq, Ty::I32, n, vf);
                let nz = self.b.bin(BinOp::Xor, Ty::I32, z, one);
                self.b.bin(BinOp::And, Ty::I32, nv, nz)
            }
        };
        if invert {
            self.b.bin(BinOp::Xor, Ty::I32, v, one)
        } else {
            v
        }
    }

    /// Byte-reverse `v` within each `group_bytes`-sized group.
    fn byte_reverse(&mut self, v: Value, ty: Ty, group_bytes: u32) -> Value {
        let bits = ty.bits();
        let n_groups = bits / (group_bytes * 8);
        let mut acc: Option<Value> = None;
        for g in 0..n_groups {
            let base = g * group_bytes * 8;
            for byte in 0..group_bytes {
                let from = base + byte * 8;
                let to = base + (group_bytes - 1 - byte) * 8;
                let mask = self.konst(ty, 0xffu64 << from);
                let masked = self.b.bin(BinOp::And, ty, v, mask);
                let shifted = match to.cmp(&from) {
                    std::cmp::Ordering::Greater => {
                        let amt = self.konst(ty, (to - from) as u64);
                        self.b.bin(BinOp::Shl, ty, masked, amt)
                    }
                    std::cmp::Ordering::Less => {
                        let amt = self.konst(ty, (from - to) as u64);
                        self.b.bin(BinOp::ShrU, ty, masked, amt)
                    }
                    std::cmp::Ordering::Equal => masked,
                };
                acc = Some(match acc {
                    None => shifted,
                    Some(a) => self.b.bin(BinOp::Or, ty, a, shifted),
                });
            }
        }
        acc.unwrap_or(v)
    }

    /// Reverse bit order using the standard swap network.
    fn bit_reverse(&mut self, v: Value, ty: Ty) -> Value {
        const MASKS: [(u64, u32); 6] = [
            (0x5555_5555_5555_5555, 1),
            (0x3333_3333_3333_3333, 2),
            (0x0f0f_0f0f_0f0f_0f0f, 4),
            (0x00ff_00ff_00ff_00ff, 8),
            (0x0000_ffff_0000_ffff, 16),
            (0x0000_0000_ffff_ffff, 32),
        ];
        let bits = ty.bits();
        let mut cur = v;
        for (mask, shift) in MASKS {
            if shift >= bits {
                break;
            }
            let m = self.konst(ty, mask & ty.mask());
            let s = self.konst(ty, shift as u64);
            let lo = self.b.bin(BinOp::And, ty, cur, m);
            let lo_sh = self.b.bin(BinOp::Shl, ty, lo, s);
            let hi_sh = self.b.bin(BinOp::ShrU, ty, cur, s);
            let hi = self.b.bin(BinOp::And, ty, hi_sh, m);
            cur = self.b.bin(BinOp::Or, ty, lo_sh, hi);
        }
        cur
    }

    // -- addressing --------------------------------------------------------

    /// Resolve an addressing mode into a base value plus a constant offset,
    /// performing any pre-index base update. Post-index writeback is deferred
    /// to [`Self::finish_address`] so it happens *after* the access.
    fn address(&mut self, addr: &AddrMode) -> Address {
        match *addr {
            AddrMode::Offset { rn, off } => {
                let base = self.rd_addr(rn);
                Address {
                    base,
                    offset: off,
                    post: None,
                }
            }
            AddrMode::PreIndex { rn, off } => {
                let base = self.rd_addr(rn);
                let delta = self.b.const_i64(off as u64);
                let updated = self.b.bin(BinOp::Add, Ty::I64, base, delta);
                self.wr_addr(rn, updated);
                Address {
                    base: updated,
                    offset: 0,
                    post: None,
                }
            }
            AddrMode::PostIndex { rn, off } => {
                let base = self.rd_addr(rn);
                Address {
                    base,
                    offset: 0,
                    post: Some((rn, off)),
                }
            }
            AddrMode::RegOffset {
                rn,
                rm,
                ext,
                amount,
            } => {
                let base = self.rd_addr(rn);
                let index = self.extend_reg(rm, ext, amount, Ty::I64);
                let sum = self.b.bin(BinOp::Add, Ty::I64, base, index);
                Address {
                    base: sum,
                    offset: 0,
                    post: None,
                }
            }
            AddrMode::Literal { addr } => {
                let base = self.b.const_i64(addr);
                Address {
                    base,
                    offset: 0,
                    post: None,
                }
            }
        }
    }

    fn finish_address(&mut self, a: &Address) {
        if let Some((rn, off)) = a.post {
            let base = self.rd_addr(rn);
            let delta = self.b.const_i64(off as u64);
            let updated = self.b.bin(BinOp::Add, Ty::I64, base, delta);
            self.wr_addr(rn, updated);
        }
    }

    // -- the dispatcher ----------------------------------------------------

    /// Lift one instruction located at `pc`. Returns how the block continues.
    pub fn lift(&mut self, inst: &Inst, pc: u64) -> Flow {
        let next = pc + INST_BYTES;
        match *inst {
            Inst::Unknown { raw } | Inst::Undefined { raw } => Flow::End(Term::Illegal { raw, pc }),

            // Hints are architecturally no-ops for us. That includes the
            // pointer-authentication hints: with PAC unimplemented, signing
            // and authenticating are both the identity, so a program that
            // signs a pointer and later authenticates it still gets it back.
            Inst::Hint { .. } => Flow::Next,

            // Clearing the monitor makes the next store-exclusive fail, which
            // is exactly what CLREX must guarantee.
            Inst::Clrex => {
                let zero = self.b.const_i64(0);
                self.b.write(GuestReg::MonSize, zero);
                Flow::Next
            }

            Inst::Adr { rd, target } | Inst::Adrp { rd, target } => {
                let v = self.b.const_i64(target);
                self.wr_addr(rd, v);
                Flow::Next
            }

            Inst::AddSubImm {
                sz,
                op,
                set_flags,
                rd,
                rn,
                imm,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let b = self.konst(ty, imm & ty.mask());
                self.arith(ty, op, set_flags, rd, sz, a, b);
                Flow::Next
            }

            Inst::AddSubShifted {
                sz,
                op,
                set_flags,
                rd,
                rn,
                rm,
                shift,
                amount,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let raw_b = self.rd_gp(rm, sz);
                let b = self.shift(raw_b, ty, shift, amount);
                self.arith(ty, op, set_flags, rd, sz, a, b);
                Flow::Next
            }

            Inst::AddSubExtended {
                sz,
                op,
                set_flags,
                rd,
                rn,
                rm,
                ext,
                amount,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let b = self.extend_reg(rm, ext, amount, ty);
                self.arith(ty, op, set_flags, rd, sz, a, b);
                Flow::Next
            }

            Inst::AddSubCarry {
                sz,
                op,
                set_flags,
                rd,
                rn,
                rm,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let b0 = self.rd_gp(rm, sz);
                let b = if op == AddOp::Sub {
                    self.b.un(UnOp::Not, ty, b0)
                } else {
                    b0
                };
                let cf = self.b.read(GuestReg::C);
                let r = self.add_with_carry(ty, a, b, Carry::Flag(cf));
                self.wr_gp(rd, sz, r.value);
                if set_flags {
                    self.set_nzcv(&r);
                }
                Flow::Next
            }

            Inst::LogicalImm {
                sz,
                op,
                set_flags,
                rd,
                rn,
                imm,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let b = self.konst(ty, imm & ty.mask());
                self.logical(ty, op, false, set_flags, rd, sz, a, b);
                Flow::Next
            }

            Inst::LogicalShifted {
                sz,
                op,
                set_flags,
                invert,
                rd,
                rn,
                rm,
                shift,
                amount,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let raw_b = self.rd_gp(rm, sz);
                let b = self.shift(raw_b, ty, shift, amount);
                self.logical(ty, op, invert, set_flags, rd, sz, a, b);
                Flow::Next
            }

            Inst::MoveWide {
                sz,
                op,
                rd,
                imm16,
                shift,
            } => {
                let ty = ty_of(sz);
                let field = (imm16 << shift) & ty.mask();
                match op {
                    MoveOp::Movz => {
                        let v = self.konst(ty, field);
                        self.wr_gp(rd, sz, v);
                    }
                    MoveOp::Movn => {
                        let v = self.konst(ty, !field & ty.mask());
                        self.wr_gp(rd, sz, v);
                    }
                    MoveOp::Movk => {
                        let old = self.rd_gp(rd, sz);
                        let keep = self.konst(ty, !(0xffffu64 << shift) & ty.mask());
                        let cleared = self.b.bin(BinOp::And, ty, old, keep);
                        let ins = self.konst(ty, field);
                        let v = self.b.bin(BinOp::Or, ty, cleared, ins);
                        self.wr_gp(rd, sz, v);
                    }
                }
                Flow::Next
            }

            Inst::Bitfield {
                sz,
                op,
                rd,
                rn,
                immr,
                imms,
            } => {
                self.bitfield(sz, op, rd, rn, immr, imms);
                Flow::Next
            }

            Inst::Extract {
                sz,
                rd,
                rn,
                rm,
                lsb,
            } => {
                let ty = ty_of(sz);
                let bits = ty.bits();
                let low = self.rd_gp(rm, sz);
                let v = if lsb == 0 {
                    low
                } else {
                    let high = self.rd_gp(rn, sz);
                    let sh_lo = self.konst(ty, lsb as u64);
                    let lo_part = self.b.bin(BinOp::ShrU, ty, low, sh_lo);
                    let sh_hi = self.konst(ty, (bits - lsb) as u64);
                    let hi_part = self.b.bin(BinOp::Shl, ty, high, sh_hi);
                    self.b.bin(BinOp::Or, ty, lo_part, hi_part)
                };
                self.wr_gp(rd, sz, v);
                Flow::Next
            }

            Inst::CondSelect {
                sz,
                rd,
                rn,
                rm,
                cond,
                else_inc,
                else_inv,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let raw_b = self.rd_gp(rm, sz);
                let mut alt = raw_b;
                if else_inv {
                    alt = self.b.un(UnOp::Not, ty, alt);
                }
                if else_inc {
                    let one = self.konst(ty, 1);
                    alt = self.b.bin(BinOp::Add, ty, alt, one);
                }
                let c = self.eval_cond(cond);
                let v = self.b.select(c, a, alt, ty);
                self.wr_gp(rd, sz, v);
                Flow::Next
            }

            Inst::CondCompare {
                sz,
                op,
                rn,
                rm,
                imm,
                nzcv,
                cond,
            } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let operand = match rm {
                    Some(r) => self.rd_gp(r, sz),
                    None => self.konst(ty, imm),
                };
                let (b, carry) = if op == AddOp::Sub {
                    let neg = self.b.un(UnOp::Not, ty, operand);
                    (neg, Carry::One)
                } else {
                    (operand, Carry::Zero)
                };
                let r = self.add_with_carry(ty, a, b, carry);
                // When the condition fails, the immediate NZCV is installed
                // verbatim instead of the comparison's flags.
                let c = self.eval_cond(cond);
                let alt_n = self.b.const_i32((nzcv >> 3) as u32 & 1);
                let alt_z = self.b.const_i32((nzcv >> 2) as u32 & 1);
                let alt_c = self.b.const_i32((nzcv >> 1) as u32 & 1);
                let alt_v = self.b.const_i32(nzcv as u32 & 1);
                let n = self.b.select(c, r.n, alt_n, Ty::I32);
                let z = self.b.select(c, r.z, alt_z, Ty::I32);
                let cf = self.b.select(c, r.c, alt_c, Ty::I32);
                let vf = self.b.select(c, r.v, alt_v, Ty::I32);
                self.b.write(GuestReg::N, n);
                self.b.write(GuestReg::Z, z);
                self.b.write(GuestReg::C, cf);
                self.b.write(GuestReg::V, vf);
                Flow::Next
            }

            Inst::DataProc1 { sz, op, rd, rn } => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let v = match op {
                    Dp1Op::Rbit => self.bit_reverse(a, ty),
                    Dp1Op::Rev16 => self.byte_reverse(a, ty, 2),
                    Dp1Op::Rev32 => self.byte_reverse(a, ty, 4),
                    Dp1Op::Rev => self.byte_reverse(a, ty, ty.bits() / 8),
                    Dp1Op::Clz => self.b.un(UnOp::Clz, ty, a),
                    Dp1Op::Cls => {
                        // Leading sign bits: clz(x ^ (x >>s (bits-1))) - 1.
                        let shift = self.konst(ty, (ty.bits() - 1) as u64);
                        let sign = self.b.bin(BinOp::ShrS, ty, a, shift);
                        let folded = self.b.bin(BinOp::Xor, ty, a, sign);
                        let clz = self.b.un(UnOp::Clz, ty, folded);
                        let one = self.konst(ty, 1);
                        self.b.bin(BinOp::Sub, ty, clz, one)
                    }
                };
                self.wr_gp(rd, sz, v);
                Flow::Next
            }

            Inst::DataProc2 { sz, op, rd, rn, rm } => self.dataproc2(sz, op, rd, rn, rm, pc),

            Inst::DataProc3 {
                sz,
                op,
                rd,
                rn,
                rm,
                ra,
            } => {
                self.dataproc3(sz, op, rd, rn, rm, ra);
                Flow::Next
            }

            // -- control flow --------------------------------------------------
            Inst::Branch { target, link } => {
                if link {
                    let ret = self.b.const_i64(next);
                    self.b.write(GuestReg::X(30), ret);
                }
                Flow::End(Term::Goto { target })
            }

            Inst::BranchCond { cond, target } => {
                if cond.is_always() {
                    return Flow::End(Term::Goto { target });
                }
                let c = self.eval_cond(cond);
                Flow::End(Term::Branch {
                    cond: c,
                    taken: target,
                    not_taken: next,
                })
            }

            Inst::CompareBranch {
                sz,
                rt,
                target,
                nonzero,
            } => {
                let ty = ty_of(sz);
                let v = self.rd_gp(rt, sz);
                let zero = self.konst(ty, 0);
                let c = self
                    .b
                    .cmp(if nonzero { CmpOp::Ne } else { CmpOp::Eq }, ty, v, zero);
                Flow::End(Term::Branch {
                    cond: c,
                    taken: target,
                    not_taken: next,
                })
            }

            Inst::TestBranch {
                rt,
                bit,
                target,
                nonzero,
            } => {
                // The tested bit index selects the operand width: bits 32-63
                // are only reachable through the 64-bit view.
                let sz = if bit >= 32 { Sz::X } else { Sz::W };
                let ty = ty_of(sz);
                let v = self.rd_gp(rt, sz);
                let mask = self.konst(ty, 1u64 << (bit % ty.bits()));
                let masked = self.b.bin(BinOp::And, ty, v, mask);
                let zero = self.konst(ty, 0);
                let c =
                    self.b
                        .cmp(if nonzero { CmpOp::Ne } else { CmpOp::Eq }, ty, masked, zero);
                Flow::End(Term::Branch {
                    cond: c,
                    taken: target,
                    not_taken: next,
                })
            }

            Inst::BranchReg { rn, link, is_ret } => {
                // Read the target *before* clobbering x30: `blr x30` is legal
                // and not uncommon.
                let target = self.rd_addr(rn);
                if link {
                    let ret = self.b.const_i64(next);
                    self.b.write(GuestReg::X(30), ret);
                }
                Flow::End(Term::Indirect { target, is_ret })
            }

            Inst::Svc { .. } => Flow::End(Term::Syscall { next }),

            Inst::Brk { imm } => {
                let pcv = self.b.const_i64(pc);
                let immv = self.b.const_i32(imm);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::Breakpoint,
                    args: vec![pcv, immv],
                });
                Flow::End(Term::Goto { target: next })
            }

            // -- system ----------------------------------------------------
            Inst::SysRegRead { rt, sysreg } => {
                let v = self.sysreg_read(sysreg, pc);
                self.wr_addr(rt, v);
                Flow::Next
            }

            Inst::SysRegWrite { rt, sysreg } => {
                let v = self.rd_addr(rt);
                self.sysreg_write(sysreg, v, pc);
                Flow::Next
            }

            Inst::SysMaintenance { op, rt } => {
                // A data-cache clean is a no-op on a coherent host and is not
                // worth a host call.
                if op == SysMaintOp::DcClean {
                    return Flow::Next;
                }
                let addr = self.rd_addr(rt);
                let code = match op {
                    SysMaintOp::DcZva => gex_abi::cache_op::DC_ZVA,
                    SysMaintOp::IcInvalidate => gex_abi::cache_op::IC_INVALIDATE,
                    SysMaintOp::DcClean => gex_abi::cache_op::DC_CLEAN,
                    SysMaintOp::Other => gex_abi::cache_op::OTHER,
                };
                let pcv = self.b.const_i64(pc);
                let codev = self.b.const_i32(code as u32);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::CacheOp,
                    args: vec![pcv, codev, addr],
                });
                Flow::Next
            }

            Inst::Barrier { kind, .. } => {
                match kind {
                    // ISB is the architecture's promise that previously
                    // written code is now visible: our translation
                    // invalidation hook (PLAN §6.2).
                    BarrierKind::Isb => {
                        let pcv = self.b.const_i64(pc);
                        let code = self.b.const_i32(gex_abi::cache_op::ISB as u32);
                        let zero = self.b.const_i64(0);
                        self.b.stmt(Stmt::HostCall {
                            func: HostFunc::CacheOp,
                            args: vec![pcv, code, zero],
                        });
                    }
                    _ => self.b.stmt(Stmt::Fence),
                }
                Flow::Next
            }

            // -- memory ----------------------------------------------------
            Inst::Load {
                sz,
                dest,
                signed,
                rt,
                ref addr,
                order,
            } => {
                let a = self.address(addr);
                let ty = ty_of(dest);
                let v = self.b.def(Expr::Load {
                    addr: a.base,
                    offset: a.offset,
                    sz: mem_sz(sz),
                    signed,
                    ty,
                    order: order_of(order),
                });
                self.wr_gp(rt, dest, v);
                self.finish_address(&a);
                Flow::Next
            }

            Inst::Store {
                sz,
                rt,
                ref addr,
                order,
            } => {
                let width = if sz == MemSz::D { Sz::X } else { Sz::W };
                let val = self.rd_gp(rt, width);
                let a = self.address(addr);
                self.b.stmt(Stmt::Store {
                    addr: a.base,
                    offset: a.offset,
                    val,
                    sz: mem_sz(sz),
                    order: order_of(order),
                });
                self.finish_address(&a);
                Flow::Next
            }

            Inst::LoadPair {
                sz,
                signed,
                rt,
                rt2,
                ref addr,
            } => {
                let a = self.address(addr);
                let (ty, dest) = if sz == MemSz::D || signed {
                    (Ty::I64, Sz::X)
                } else {
                    (Ty::I32, Sz::W)
                };
                let step = sz.bytes() as i64;
                let v1 = self.b.def(Expr::Load {
                    addr: a.base,
                    offset: a.offset,
                    sz: mem_sz(sz),
                    signed,
                    ty,
                    order: Order::Relaxed,
                });
                let v2 = self.b.def(Expr::Load {
                    addr: a.base,
                    offset: a.offset + step,
                    sz: mem_sz(sz),
                    signed,
                    ty,
                    order: Order::Relaxed,
                });
                self.wr_gp(rt, dest, v1);
                self.wr_gp(rt2, dest, v2);
                self.finish_address(&a);
                Flow::Next
            }

            Inst::StorePair {
                sz,
                rt,
                rt2,
                ref addr,
            } => {
                let width = if sz == MemSz::D { Sz::X } else { Sz::W };
                let v1 = self.rd_gp(rt, width);
                let v2 = self.rd_gp(rt2, width);
                let a = self.address(addr);
                let step = sz.bytes() as i64;
                self.b.stmt(Stmt::Store {
                    addr: a.base,
                    offset: a.offset,
                    val: v1,
                    sz: mem_sz(sz),
                    order: Order::Relaxed,
                });
                self.b.stmt(Stmt::Store {
                    addr: a.base,
                    offset: a.offset + step,
                    val: v2,
                    sz: mem_sz(sz),
                    order: Order::Relaxed,
                });
                self.finish_address(&a);
                Flow::Next
            }

            // Prefetch is advisory and has no writeback forms, so the whole
            // instruction is a no-op.
            Inst::Prefetch { .. } => Flow::Next,

            Inst::LoadExclusive { sz, rt, rn, order } => {
                let addr = self.rd_addr(rn);
                let ty = if sz == MemSz::D { Ty::I64 } else { Ty::I32 };
                let v = self.b.def(Expr::Load {
                    addr,
                    offset: 0,
                    sz: mem_sz(sz),
                    signed: false,
                    ty,
                    order: order_of(order),
                });
                let v64 = match ty {
                    Ty::I64 => v,
                    Ty::I32 => self.b.extend(v, false),
                };
                self.b.write(GuestReg::MonAddr, addr);
                self.b.write(GuestReg::MonValue, v64);
                let size = self.b.const_i64(sz.bytes() as u64);
                self.b.write(GuestReg::MonSize, size);
                let dest = if sz == MemSz::D { Sz::X } else { Sz::W };
                self.wr_gp(rt, dest, v);
                Flow::Next
            }

            Inst::StoreExclusive { sz, rs, rt, rn, .. } => {
                self.store_exclusive(sz, rs, rt, rn);
                Flow::Next
            }

            Inst::CompareSwap { sz, rs, rt, rn, .. } => {
                let ty = if sz == MemSz::D { Ty::I64 } else { Ty::I32 };
                let width = if sz == MemSz::D { Sz::X } else { Sz::W };
                let addr = self.rd_addr(rn);
                let expected = self.rd_gp(rs, width);
                let replacement = self.rd_gp(rt, width);
                let old = self.b.def(Expr::CmpXchg {
                    addr,
                    expected,
                    replacement,
                    sz: mem_sz(sz),
                    ty,
                });
                // CAS writes the value actually read back into Rs.
                self.wr_gp(rs, width, old);
                Flow::Next
            }

            Inst::AtomicRmw {
                sz, op, rs, rt, rn, ..
            } => self.atomic_rmw(sz, op, rs, rt, rn, pc),
        }
    }

    // -- instruction families ----------------------------------------------

    /// `add`/`sub` with optional flag setting, shared by all operand forms.
    fn arith(&mut self, ty: Ty, op: AddOp, set_flags: bool, rd: RegSel, sz: Sz, a: Value, b: Value) {
        if !set_flags {
            let v = self.b.bin(
                if op == AddOp::Add {
                    BinOp::Add
                } else {
                    BinOp::Sub
                },
                ty,
                a,
                b,
            );
            self.wr_gp(rd, sz, v);
            return;
        }
        // AArch64 defines subtraction as `a + NOT(b) + 1`, which is also what
        // its carry flag follows.
        let (operand, carry) = if op == AddOp::Sub {
            let neg = self.b.un(UnOp::Not, ty, b);
            (neg, Carry::One)
        } else {
            (b, Carry::Zero)
        };
        let r = self.add_with_carry(ty, a, operand, carry);
        self.wr_gp(rd, sz, r.value);
        self.set_nzcv(&r);
    }

    #[allow(clippy::too_many_arguments)]
    fn logical(
        &mut self,
        ty: Ty,
        op: LogOp,
        invert: bool,
        set_flags: bool,
        rd: RegSel,
        sz: Sz,
        a: Value,
        b0: Value,
    ) {
        let b = if invert {
            self.b.un(UnOp::Not, ty, b0)
        } else {
            b0
        };
        let bin = match op {
            LogOp::And => BinOp::And,
            LogOp::Orr => BinOp::Or,
            LogOp::Eor => BinOp::Xor,
        };
        let v = self.b.bin(bin, ty, a, b);
        self.wr_gp(rd, sz, v);
        if set_flags {
            self.set_nz_clear_cv(ty, v);
        }
    }

    fn bitfield(&mut self, sz: Sz, op: BfOp, rd: RegSel, rn: RegSel, immr: u32, imms: u32) {
        let ty = ty_of(sz);
        let bits = ty.bits();
        // For the bitfield encodings the element size always equals the
        // register width (the decoder enforces `N == sf`), so `DecodeBitMasks`
        // reduces to two constants with no replication.
        let ones = |n: u32| -> u64 {
            if n >= 64 {
                u64::MAX
            } else {
                (1u64 << n) - 1
            }
        };
        let esize_mask = ones(bits);
        let welem = ones(imms + 1) & esize_mask;
        let wmask = if immr == 0 {
            welem
        } else {
            ((welem >> immr) | (welem << (bits - immr))) & esize_mask
        };
        let diff = imms.wrapping_sub(immr) & (bits - 1);
        let tmask = ones(diff + 1) & esize_mask;

        let src = self.rd_gp(rn, sz);
        let rotated = if immr == 0 {
            src
        } else {
            let amt = self.konst(ty, immr as u64);
            self.b.bin(BinOp::Rotr, ty, src, amt)
        };
        let wm = self.konst(ty, wmask);
        let bot = self.b.bin(BinOp::And, ty, rotated, wm);

        let result = match op {
            BfOp::Ubfm => {
                let tm = self.konst(ty, tmask);
                self.b.bin(BinOp::And, ty, bot, tm)
            }
            BfOp::Sbfm => {
                // Replicate the extracted field's top bit into the bits above.
                let shift = self.konst(ty, (bits - 1 - imms) as u64);
                let up = self.b.bin(BinOp::Shl, ty, src, shift);
                let down = self.konst(ty, (bits - 1) as u64);
                let top = self.b.bin(BinOp::ShrS, ty, up, down);
                let tm = self.konst(ty, tmask);
                let not_tm = self.konst(ty, !tmask & esize_mask);
                let hi = self.b.bin(BinOp::And, ty, top, not_tm);
                let lo = self.b.bin(BinOp::And, ty, bot, tm);
                self.b.bin(BinOp::Or, ty, hi, lo)
            }
            BfOp::Bfm => {
                let keep_mask = wmask & tmask;
                let old = self.rd_gp(rd, sz);
                let inv = self.konst(ty, !keep_mask & esize_mask);
                let kept = self.b.bin(BinOp::And, ty, old, inv);
                let km = self.konst(ty, keep_mask);
                let ins = self.b.bin(BinOp::And, ty, rotated, km);
                self.b.bin(BinOp::Or, ty, kept, ins)
            }
        };
        self.wr_gp(rd, sz, result);
    }

    fn dataproc2(&mut self, sz: Sz, op: Dp2Op, rd: RegSel, rn: RegSel, rm: RegSel, pc: u64) -> Flow {
        let ty = ty_of(sz);
        let a = self.rd_gp(rn, sz);
        let b = self.rd_gp(rm, sz);
        let v = match op {
            Dp2Op::Udiv | Dp2Op::Sdiv => {
                let signed = op == Dp2Op::Sdiv;
                let zero = self.konst(ty, 0);
                let is_zero = self.b.cmp(CmpOp::Eq, ty, b, zero);
                // Wasm traps on division by zero and on `INT_MIN / -1`;
                // AArch64 produces 0 and INT_MIN. Steering the divisor to 1
                // already yields INT_MIN for the overflow case, so only the
                // divide-by-zero result needs a select.
                let one = self.konst(ty, 1);
                let mut unsafe_divisor = is_zero;
                if signed {
                    let min = self.konst(ty, 1u64 << (ty.bits() - 1));
                    let neg1 = self.konst(ty, ty.mask());
                    let a_min = self.b.cmp(CmpOp::Eq, ty, a, min);
                    let b_neg1 = self.b.cmp(CmpOp::Eq, ty, b, neg1);
                    let ovf = self.b.bin(BinOp::And, Ty::I32, a_min, b_neg1);
                    unsafe_divisor = self.b.bin(BinOp::Or, Ty::I32, is_zero, ovf);
                }
                let divisor = self.b.select(unsafe_divisor, one, b, ty);
                let q = self.b.bin(
                    if signed { BinOp::DivS } else { BinOp::DivU },
                    ty,
                    a,
                    divisor,
                );
                self.b.select(is_zero, zero, q, ty)
            }
            Dp2Op::Lslv => self.b.bin(BinOp::Shl, ty, a, b),
            Dp2Op::Lsrv => self.b.bin(BinOp::ShrU, ty, a, b),
            Dp2Op::Asrv => self.b.bin(BinOp::ShrS, ty, a, b),
            Dp2Op::Rorv => self.b.bin(BinOp::Rotr, ty, a, b),
            // CRC32 needs a table or a carry-less multiply; not worth an
            // inline expansion until something in the corpus uses it.
            _ => return Flow::End(Term::Illegal { raw: 0, pc }),
        };
        self.wr_gp(rd, sz, v);
        Flow::Next
    }

    fn dataproc3(&mut self, sz: Sz, op: Dp3Op, rd: RegSel, rn: RegSel, rm: RegSel, ra: RegSel) {
        match op {
            Dp3Op::Madd | Dp3Op::Msub => {
                let ty = ty_of(sz);
                let a = self.rd_gp(rn, sz);
                let b = self.rd_gp(rm, sz);
                let acc = self.rd_gp(ra, sz);
                let prod = self.b.bin(BinOp::Mul, ty, a, b);
                let v = self.b.bin(
                    if op == Dp3Op::Madd {
                        BinOp::Add
                    } else {
                        BinOp::Sub
                    },
                    ty,
                    acc,
                    prod,
                );
                self.wr_gp(rd, sz, v);
            }
            Dp3Op::Smaddl | Dp3Op::Smsubl | Dp3Op::Umaddl | Dp3Op::Umsubl => {
                let signed = matches!(op, Dp3Op::Smaddl | Dp3Op::Smsubl);
                let sub = matches!(op, Dp3Op::Smsubl | Dp3Op::Umsubl);
                let a32 = self.rd_gp(rn, Sz::W);
                let b32 = self.rd_gp(rm, Sz::W);
                let a = self.b.extend(a32, signed);
                let b = self.b.extend(b32, signed);
                let acc = self.rd_gp(ra, Sz::X);
                let prod = self.b.bin(BinOp::Mul, Ty::I64, a, b);
                let v = self.b.bin(
                    if sub { BinOp::Sub } else { BinOp::Add },
                    Ty::I64,
                    acc,
                    prod,
                );
                self.wr_gp(rd, Sz::X, v);
            }
            Dp3Op::Smulh | Dp3Op::Umulh => {
                let signed = op == Dp3Op::Smulh;
                let a = self.rd_gp(rn, Sz::X);
                let b = self.rd_gp(rm, Sz::X);
                let v = self.mul_high(a, b, signed);
                self.wr_gp(rd, Sz::X, v);
            }
        }
    }

    /// High 64 bits of a 64x64 product, built from four 32-bit partial
    /// products — wasm has no widening multiply.
    fn mul_high(&mut self, a: Value, b: Value, signed: bool) -> Value {
        let ty = Ty::I64;
        let sh32 = self.konst(ty, 32);
        let lo_mask = self.konst(ty, 0xffff_ffff);

        let a_lo = self.b.bin(BinOp::And, ty, a, lo_mask);
        let b_lo = self.b.bin(BinOp::And, ty, b, lo_mask);
        // Unsigned high halves; the signed correction comes at the end.
        let a_hi = self.b.bin(BinOp::ShrU, ty, a, sh32);
        let b_hi = self.b.bin(BinOp::ShrU, ty, b, sh32);

        let ll = self.b.bin(BinOp::Mul, ty, a_lo, b_lo);
        let lh = self.b.bin(BinOp::Mul, ty, a_lo, b_hi);
        let hl = self.b.bin(BinOp::Mul, ty, a_hi, b_lo);
        let hh = self.b.bin(BinOp::Mul, ty, a_hi, b_hi);

        // mid = (ll >> 32) + (lh & 0xffffffff) + (hl & 0xffffffff)
        let ll_hi = self.b.bin(BinOp::ShrU, ty, ll, sh32);
        let lh_lo = self.b.bin(BinOp::And, ty, lh, lo_mask);
        let hl_lo = self.b.bin(BinOp::And, ty, hl, lo_mask);
        let m1 = self.b.bin(BinOp::Add, ty, ll_hi, lh_lo);
        let mid = self.b.bin(BinOp::Add, ty, m1, hl_lo);

        // high = hh + (lh >> 32) + (hl >> 32) + (mid >> 32)
        let lh_hi = self.b.bin(BinOp::ShrU, ty, lh, sh32);
        let hl_hi = self.b.bin(BinOp::ShrU, ty, hl, sh32);
        let mid_hi = self.b.bin(BinOp::ShrU, ty, mid, sh32);
        let h1 = self.b.bin(BinOp::Add, ty, hh, lh_hi);
        let h2 = self.b.bin(BinOp::Add, ty, h1, hl_hi);
        let mut high = self.b.bin(BinOp::Add, ty, h2, mid_hi);

        if signed {
            // Convert the unsigned high word to the signed one:
            // signed = unsigned - (a < 0 ? b : 0) - (b < 0 ? a : 0).
            let zero = self.konst(ty, 0);
            let a_neg = self.b.cmp(CmpOp::LtS, ty, a, zero);
            let b_neg = self.b.cmp(CmpOp::LtS, ty, b, zero);
            let ca = self.b.select(a_neg, b, zero, ty);
            let cb = self.b.select(b_neg, a, zero, ty);
            let t = self.b.bin(BinOp::Sub, ty, high, ca);
            high = self.b.bin(BinOp::Sub, ty, t, cb);
        }
        high
    }

    /// `stxr`/`stlxr`: succeed only if the local monitor still matches.
    ///
    /// Wasm has no exclusive-access primitive, so the monitor is emulated with
    /// a compare-and-swap against the value the matching load-exclusive
    /// observed. When the monitor does not match, the compare-and-swap is
    /// redirected to a scratch location so it cannot touch guest memory, and
    /// the instruction reports failure. Spurious *failure* is architecturally
    /// permitted; spurious success is not, which is why the redirect exists.
    fn store_exclusive(&mut self, sz: MemSz, rs: RegSel, rt: RegSel, rn: RegSel) {
        let ty = if sz == MemSz::D { Ty::I64 } else { Ty::I32 };
        let width = if sz == MemSz::D { Sz::X } else { Sz::W };
        let addr = self.rd_addr(rn);
        let value = self.rd_gp(rt, width);

        let mon_addr = self.b.read(GuestReg::MonAddr);
        let mon_size = self.b.read(GuestReg::MonSize);
        let want_size = self.b.const_i64(sz.bytes() as u64);
        let addr_ok = self.b.cmp(CmpOp::Eq, Ty::I64, mon_addr, addr);
        let size_ok = self.b.cmp(CmpOp::Eq, Ty::I64, mon_size, want_size);
        let armed = self.b.bin(BinOp::And, Ty::I32, addr_ok, size_ok);

        let scratch = self.b.const_i64(gex_abi::EXCLUSIVE_SCRATCH);
        let eff_addr = self.b.select(armed, addr, scratch, Ty::I64);

        let mon_value = self.b.read(GuestReg::MonValue);
        let expected = match ty {
            Ty::I64 => mon_value,
            Ty::I32 => self.b.wrap(mon_value),
        };
        let old = self.b.def(Expr::CmpXchg {
            addr: eff_addr,
            expected,
            replacement: value,
            sz: mem_sz(sz),
            ty,
        });
        let swapped = self.b.cmp(CmpOp::Eq, ty, old, expected);
        let success = self.b.bin(BinOp::And, Ty::I32, armed, swapped);

        // Rs receives 0 on success and 1 on failure.
        let one = self.b.const_i32(1);
        let status = self.b.bin(BinOp::Xor, Ty::I32, success, one);
        self.wr_gp(rs, Sz::W, status);

        // The monitor is cleared either way.
        let zero = self.b.const_i64(0);
        self.b.write(GuestReg::MonSize, zero);
    }

    fn atomic_rmw(
        &mut self,
        sz: MemSz,
        op: IsaRmw,
        rs: RegSel,
        rt: RegSel,
        rn: RegSel,
        pc: u64,
    ) -> Flow {
        let ty = if sz == MemSz::D { Ty::I64 } else { Ty::I32 };
        let width = if sz == MemSz::D { Sz::X } else { Sz::W };
        let addr = self.rd_addr(rn);
        let operand = self.rd_gp(rs, width);
        let (rmw_op, val) = match op {
            IsaRmw::Add => (RmwOp::Add, operand),
            IsaRmw::Eor => (RmwOp::Xor, operand),
            IsaRmw::Set => (RmwOp::Or, operand),
            IsaRmw::Clr => {
                // LDCLR clears exactly the bits set in the operand.
                let inv = self.b.un(UnOp::Not, ty, operand);
                (RmwOp::And, inv)
            }
            IsaRmw::Swp => (RmwOp::Xchg, operand),
            // The min/max variants need a compare-and-swap retry loop, which
            // block-local GIR cannot express. They are vanishingly rare in
            // real userspace, so trap precisely rather than guess.
            IsaRmw::Smax | IsaRmw::Smin | IsaRmw::Umax | IsaRmw::Umin => {
                return Flow::End(Term::Illegal { raw: 0, pc })
            }
        };
        let old = self.b.def(Expr::Rmw {
            op: rmw_op,
            addr,
            val,
            sz: mem_sz(sz),
            ty,
        });
        self.wr_gp(rt, width, old);
        Flow::Next
    }

    fn sysreg_read(&mut self, sysreg: SysReg, pc: u64) -> Value {
        match sysreg {
            SysReg::TpidrEl0 => self.b.read(GuestReg::Tpidr),
            SysReg::TpidrroEl0 => self.b.read(GuestReg::TpidrRo),
            SysReg::Ctr => self.b.const_i64(gex_abi::CTR_EL0),
            SysReg::Cntfrq => self.b.const_i64(gex_abi::CNTFRQ),
            SysReg::Dczid => {
                // log2 of the block size in words. The "zeroing prohibited"
                // bit stays clear because DC ZVA is implemented.
                let words = gex_abi::DCZID_BLOCK_BYTES / 4;
                self.b.const_i64(words.trailing_zeros() as u64)
            }
            SysReg::Nzcv => {
                let n = self.b.read(GuestReg::N);
                let z = self.b.read(GuestReg::Z);
                let c = self.b.read(GuestReg::C);
                let v = self.b.read(GuestReg::V);
                let mut acc: Option<Value> = None;
                for (flag, shift) in [(n, 31u64), (z, 30), (c, 29), (v, 28)] {
                    let wide = self.b.extend(flag, false);
                    let s = self.b.const_i64(shift);
                    let placed = self.b.bin(BinOp::Shl, Ty::I64, wide, s);
                    acc = Some(match acc {
                        None => placed,
                        Some(a) => self.b.bin(BinOp::Or, Ty::I64, a, placed),
                    });
                }
                acc.expect("four flags")
            }
            // Everything else — including the virtual counter, which the
            // runtime serves from the shared time page — goes to the host.
            other => {
                let pcv = self.b.const_i64(pc);
                let id = self.b.const_i32(sysreg_code(other) as u32);
                self.b.def(Expr::HostCall {
                    func: HostFunc::SysRegRead,
                    args: vec![pcv, id],
                    ty: Ty::I64,
                })
            }
        }
    }

    fn sysreg_write(&mut self, sysreg: SysReg, v: Value, pc: u64) {
        match sysreg {
            SysReg::TpidrEl0 => self.b.write(GuestReg::Tpidr, v),
            SysReg::Nzcv => {
                let one = self.b.const_i64(1);
                for (shift, reg) in [
                    (31u64, GuestReg::N),
                    (30, GuestReg::Z),
                    (29, GuestReg::C),
                    (28, GuestReg::V),
                ] {
                    let s = self.b.const_i64(shift);
                    let sh = self.b.bin(BinOp::ShrU, Ty::I64, v, s);
                    let bit = self.b.bin(BinOp::And, Ty::I64, sh, one);
                    let b32 = self.b.wrap(bit);
                    self.b.write(reg, b32);
                }
            }
            // FPCR/FPSR writes go to the host so the fidelity policy
            // (PLAN §5.7) can notice a non-default rounding mode.
            other => {
                let pcv = self.b.const_i64(pc);
                let id = self.b.const_i32(sysreg_code(other) as u32);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::SysRegWrite,
                    args: vec![pcv, id, v],
                });
            }
        }
    }
}

/// Encode a system register identifier for the host interface. Modelled
/// registers get small negative codes; everything else passes through its
/// `op0:op1:CRn:CRm:op2` packing.
pub fn sysreg_code(r: SysReg) -> i32 {
    match r {
        SysReg::Other(v) => v as i32,
        SysReg::TpidrEl0 => -1,
        SysReg::TpidrroEl0 => -2,
        SysReg::Cntvct => -3,
        SysReg::Cntfrq => -4,
        SysReg::Ctr => -5,
        SysReg::Dczid => -6,
        SysReg::Midr => -7,
        SysReg::Fpcr => -8,
        SysReg::Fpsr => -9,
        SysReg::Nzcv => -10,
    }
}
