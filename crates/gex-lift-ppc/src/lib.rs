//! `gex-lift-ppc` — 32-bit PowerPC semantics, expressed in GIR.
//!
//! The contract is the other lifters': total (every decoded instruction lifts
//! or ends the block with a precise SIGILL), exact where wasm and the
//! architecture disagree, and condition bits written as separate one-byte
//! registers so dead-flag elimination can drop the ones nobody reads.
//!
//! Register model (docs/ppc.md §4): `r0`–`r31` are `GuestReg::R(n)`, `I32`;
//! LR and CTR are `Lr`/`Ctr`; each of the 32 CR bits is `Cr(i)`, a flag; XER
//! is `So`/`Ov`/`Ca` plus `XerBc`; FPRs are `F(n)`, f64 bit patterns; the
//! reservation of `lwarx`/`stwcx.` reuses AArch64's exclusive monitor.
//!
//! Memory is big-endian (docs/ppc.md §2): a load is a plain little-endian
//! `Load` followed by `UnOp::Bswap`, a store the reverse. Effective
//! addresses are `I32`, computed with wrapping 32-bit arithmetic, which is
//! what GIR asks of a 32-bit guest.

#![forbid(unsafe_code)]

use gex_ir::*;
use gex_isa_ppc::{Inst, Op};

mod block;
mod fp;
pub use block::{lift_block, BlockLimits};

/// Pseudo system-register codes for the `sysreg_read`/`sysreg_write` host
/// imports. Negative like the other lifters', so they never collide.
pub mod sysreg {
    /// The time base: the host's monotonic nanosecond counter (the A64
    /// `CNTVCT_EL0` code), so the time base runs at 1 GHz.
    pub const TIMEBASE: i32 = -3;
    /// An `mtfsf`/`mtfsfi`/`mtfsb*` that left FPSCR's rounding mode non-zero:
    /// the host fails loudly (docs/ppc.md §8). The value is the new FPSCR.
    pub const FPSCR_RN: i32 = -110;
    /// `mffs` under `GEX_FP_STRICT=1`: the host fails loudly, because the
    /// exception status bits are not computed.
    pub const MFFS_STRICT: i32 = -111;
}

/// What the lifter decided about control flow after one instruction.
pub enum Flow {
    Next,
    End(Term),
}

/// CR field `f`'s LT, GT, EQ and SO bits.
fn cr_bits(f: u8) -> [GuestReg; 4] {
    [
        GuestReg::Cr(4 * f),
        GuestReg::Cr(4 * f + 1),
        GuestReg::Cr(4 * f + 2),
        GuestReg::Cr(4 * f + 3),
    ]
}

pub struct Lifter<'a> {
    pub(crate) b: &'a mut Builder,
    pub(crate) raw: u32,
    pub(crate) pc: u64,
    pub(crate) next: u64,
}

impl<'a> Lifter<'a> {
    pub fn new(b: &'a mut Builder, raw: u32) -> Lifter<'a> {
        Lifter {
            b,
            raw,
            pc: 0,
            next: 0,
        }
    }

    // -- values ----------------------------------------------------------------

    pub(crate) fn k32(&mut self, v: u32) -> Value {
        self.b.const_i32(v)
    }
    pub(crate) fn k64(&mut self, v: u64) -> Value {
        self.b.const_i64(v)
    }
    pub(crate) fn bin(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        self.b.bin(op, Ty::I32, a, b)
    }
    pub(crate) fn bin64(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        self.b.bin(op, Ty::I64, a, b)
    }
    pub(crate) fn binc(&mut self, op: BinOp, a: Value, c: u32) -> Value {
        let k = self.k32(c);
        self.bin(op, a, k)
    }
    pub(crate) fn cmp(&mut self, op: CmpOp, a: Value, b: Value) -> Value {
        self.b.cmp(op, Ty::I32, a, b)
    }
    pub(crate) fn cmpc(&mut self, op: CmpOp, a: Value, c: u32) -> Value {
        let k = self.k32(c);
        self.cmp(op, a, k)
    }
    pub(crate) fn not(&mut self, a: Value) -> Value {
        self.b.un(UnOp::Not, Ty::I32, a)
    }
    pub(crate) fn sel(&mut self, c: Value, a: Value, b: Value) -> Value {
        self.b.select(c, a, b, Ty::I32)
    }

    // -- registers ------------------------------------------------------------

    pub(crate) fn rd(&mut self, n: u8) -> Value {
        self.b.read(GuestReg::R(n))
    }
    /// `(rA|0)`: register 0 reads as zero in an address.
    pub(crate) fn rd0(&mut self, n: u8) -> Value {
        if n == 0 {
            self.k32(0)
        } else {
            self.rd(n)
        }
    }
    pub(crate) fn wr(&mut self, n: u8, v: Value) {
        self.b.write(GuestReg::R(n), v);
    }

    // -- memory ---------------------------------------------------------------

    /// `(rA|0) + d` as an `I32` address and a static offset. A non-negative
    /// displacement rides as the offset; a negative one is added, wrapping.
    fn ea_d(&mut self, ra: u8, d: i32) -> (Value, i64) {
        let base = self.rd0(ra);
        if d >= 0 {
            (base, d as i64)
        } else {
            (self.binc(BinOp::Add, base, d as u32), 0)
        }
    }
    /// `(rA|0) + d` as one wrapped `I32`, for the update forms.
    fn ea_d_full(&mut self, ra: u8, d: i32) -> Value {
        let base = self.rd0(ra);
        if d == 0 {
            base
        } else {
            self.binc(BinOp::Add, base, d as u32)
        }
    }
    /// `(rA|0) + rB`.
    fn ea_x(&mut self, ra: u8, rb: u8) -> Value {
        let b = self.rd(rb);
        if ra == 0 {
            b
        } else {
            let a = self.rd(ra);
            self.bin(BinOp::Add, a, b)
        }
    }

    /// Load `sz` bytes at `addr + off` in guest (big-endian) order, as an
    /// `I32` (zero- or sign-extended). `reverse` is the byte-reversed forms.
    pub(crate) fn load(&mut self, addr: Value, off: i64, sz: MemSz, signed: bool, reverse: bool) -> Value {
        let v = self.b.def(Expr::Load {
            addr,
            offset: off,
            sz,
            signed: signed && (sz == MemSz::B1 || reverse),
            ty: Ty::I32,
            order: Order::Relaxed,
        });
        if reverse || sz == MemSz::B1 {
            return v;
        }
        match sz {
            MemSz::B2 => {
                let s = self.b.un(UnOp::Bswap16, Ty::I32, v);
                if signed {
                    self.b.def(Expr::ExtendBits {
                        a: s,
                        ty: Ty::I32,
                        bits: 16,
                        signed: true,
                    })
                } else {
                    s
                }
            }
            _ => self.b.un(UnOp::Bswap, Ty::I32, v),
        }
    }

    pub(crate) fn store(&mut self, addr: Value, off: i64, sz: MemSz, v: Value, reverse: bool) {
        let val = if reverse || sz == MemSz::B1 {
            v
        } else if sz == MemSz::B2 {
            self.b.un(UnOp::Bswap16, Ty::I32, v)
        } else {
            self.b.un(UnOp::Bswap, Ty::I32, v)
        };
        self.b.stmt(Stmt::Store {
            addr,
            offset: off,
            val,
            sz,
            order: Order::Relaxed,
        });
    }

    pub(crate) fn load64(&mut self, addr: Value, off: i64) -> Value {
        let v = self.b.def(Expr::Load {
            addr,
            offset: off,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        });
        self.b.un(UnOp::Bswap, Ty::I64, v)
    }

    pub(crate) fn store64(&mut self, addr: Value, off: i64, v: Value) {
        let val = self.b.un(UnOp::Bswap, Ty::I64, v);
        self.b.stmt(Stmt::Store {
            addr,
            offset: off,
            val,
            sz: MemSz::B8,
            order: Order::Relaxed,
        });
    }

    // -- condition register -----------------------------------------------------

    /// Set CR field `f` from a signed or unsigned comparison of `a` and `b`,
    /// with SO copied from XER.
    fn set_cr_cmp(&mut self, f: u8, a: Value, b: Value, signed: bool) {
        let (lt, gt) = if signed {
            (CmpOp::LtS, CmpOp::GtS)
        } else {
            (CmpOp::LtU, CmpOp::GtU)
        };
        let l = self.cmp(lt, a, b);
        let g = self.cmp(gt, a, b);
        let e = self.cmp(CmpOp::Eq, a, b);
        let so = self.b.read(GuestReg::So);
        let [rl, rg, re, rs] = cr_bits(f);
        self.b.write(rl, l);
        self.b.write(rg, g);
        self.b.write(re, e);
        self.b.write(rs, so);
    }

    /// The record form's CR0: the result compared with zero as signed, and
    /// SO from XER.
    pub(crate) fn record(&mut self, r: Value) {
        let z = self.k32(0);
        self.set_cr_cmp(0, r, z, true);
    }

    /// XER[OV] from an overflow bit, and SO accumulating it.
    fn set_ov(&mut self, ov: Value) {
        let so = self.b.read(GuestReg::So);
        let so = self.bin(BinOp::Or, so, ov);
        self.b.write(GuestReg::Ov, ov);
        self.b.write(GuestReg::So, so);
    }

    /// Finish an XO-form arithmetic result: write rD, then OV/SO when OE,
    /// then CR0 when Rc (after OE, so CR0.SO sees the new SO).
    fn finish_xo(&mut self, i: &Inst, r: Value, ov: impl FnOnce(&mut Self) -> Value) {
        self.wr(i.rt(), r);
        if i.oe() {
            let o = ov(self);
            self.set_ov(o);
        }
        if i.rc() {
            self.record(r);
        }
    }

    /// Signed overflow of `x + y (+ carry) = r`: both operands' signs agree
    /// and the result's differs.
    fn add_ov(&mut self, x: Value, y: Value, r: Value) -> Value {
        let a = self.bin(BinOp::Xor, x, r);
        let b = self.bin(BinOp::Xor, y, r);
        let c = self.bin(BinOp::And, a, b);
        self.binc(BinOp::ShrU, c, 31)
    }

    /// `x + y + c` with its carry out, via 64-bit arithmetic.
    fn add_carry(&mut self, x: Value, y: Value, c: Value) -> (Value, Value) {
        let x64 = self.b.extend(x, false);
        let y64 = self.b.extend(y, false);
        let c64 = self.b.extend(c, false);
        let s = self.bin64(BinOp::Add, x64, y64);
        let s = self.bin64(BinOp::Add, s, c64);
        let r = self.b.wrap(s);
        let k = self.k64(32);
        let hi = self.bin64(BinOp::ShrU, s, k);
        let ca = self.b.wrap(hi);
        (r, ca)
    }

    /// `rotl32(x, n)`; wasm and GIR only rotate right.
    pub(crate) fn rotl(&mut self, x: Value, n: Value) -> Value {
        let k = self.k32(32);
        let m = self.bin(BinOp::Sub, k, n);
        let m = self.binc(BinOp::And, m, 31);
        self.bin(BinOp::Rotr, x, m)
    }

    fn illegal(&self) -> Flow {
        Flow::End(Term::Illegal {
            raw: self.raw,
            pc: self.pc,
        })
    }

    // -- the instruction set ----------------------------------------------------

    pub fn lift(&mut self, i: &Inst, pc: u64) -> Flow {
        self.pc = pc;
        self.next = pc.wrapping_add(4) & 0xffff_ffff;
        use Op::*;
        let (rt, ra, rb) = (i.rt(), i.ra(), i.rb());
        match i.op {
            Unknown => self.illegal(),

            // -- branches and system ---------------------------------------------
            B => {
                let target = i.target(pc);
                if i.lk() {
                    let n = self.k32(self.next as u32);
                    self.b.write(GuestReg::Lr, n);
                    self.b.set_call_return(self.next);
                }
                Flow::End(Term::Goto { target })
            }
            Bc => self.lift_bc(i),
            Bclr | Bcctr => self.lift_bc_reg(i),
            Sc => {
                if i.lev() != 0 {
                    return self.illegal();
                }
                Flow::End(Term::Syscall { next: self.next })
            }
            Tw | Twi => self.lift_trap(i),

            // -- condition register ------------------------------------------------
            Mcrf => {
                let src = cr_bits(i.crfs());
                let dst = cr_bits(i.crfd());
                let vals: Vec<Value> = src.iter().map(|r| self.b.read(*r)).collect();
                for (r, v) in dst.iter().zip(vals) {
                    self.b.write(*r, v);
                }
                Flow::Next
            }
            Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => {
                let a = self.b.read(GuestReg::Cr(ra));
                let b = self.b.read(GuestReg::Cr(rb));
                let one = self.k32(1);
                let nb = self.bin(BinOp::Xor, b, one);
                let r = match i.op {
                    Crand => self.bin(BinOp::And, a, b),
                    Crandc => self.bin(BinOp::And, a, nb),
                    Cror => self.bin(BinOp::Or, a, b),
                    Crorc => self.bin(BinOp::Or, a, nb),
                    Crxor => self.bin(BinOp::Xor, a, b),
                    Creqv => {
                        let x = self.bin(BinOp::Xor, a, b);
                        self.bin(BinOp::Xor, x, one)
                    }
                    Crnand => {
                        let x = self.bin(BinOp::And, a, b);
                        self.bin(BinOp::Xor, x, one)
                    }
                    _ => {
                        let x = self.bin(BinOp::Or, a, b);
                        self.bin(BinOp::Xor, x, one)
                    }
                };
                self.b.write(GuestReg::Cr(rt), r);
                Flow::Next
            }
            Mcrxr => {
                let so = self.b.read(GuestReg::So);
                let ov = self.b.read(GuestReg::Ov);
                let ca = self.b.read(GuestReg::Ca);
                let zero = self.k32(0);
                let [l, g, e, s] = cr_bits(i.crfd());
                self.b.write(l, so);
                self.b.write(g, ov);
                self.b.write(e, ca);
                self.b.write(s, zero);
                self.b.write(GuestReg::So, zero);
                self.b.write(GuestReg::Ov, zero);
                self.b.write(GuestReg::Ca, zero);
                Flow::Next
            }
            Mfcr => {
                let v = self.read_cr();
                // mfocrf leaves the fields it does not name undefined; QEMU
                // (the oracle) zeroes them.
                let v = if i.raw & (1 << 20) != 0 {
                    let f = i.fxm().leading_zeros();
                    self.binc(BinOp::And, v, 0xf000_0000 >> (4 * f))
                } else {
                    v
                };
                self.wr(rt, v);
                Flow::Next
            }
            Mtcrf => {
                let s = self.rd(rt);
                let fxm = i.fxm();
                for f in 0..8u8 {
                    if fxm & (0x80 >> f) == 0 {
                        continue;
                    }
                    for k in 0..4u8 {
                        let bit = 4 * f + k;
                        let sh = self.binc(BinOp::ShrU, s, 31 - bit as u32);
                        let v = self.binc(BinOp::And, sh, 1);
                        self.b.write(GuestReg::Cr(bit), v);
                    }
                }
                Flow::Next
            }

            // -- integer arithmetic -----------------------------------------------
            Addi | Addis => {
                let imm = if i.op == Addis {
                    (i.simm() as u32) << 16
                } else {
                    i.simm() as u32
                };
                let r = if ra == 0 {
                    self.k32(imm)
                } else {
                    let a = self.rd(ra);
                    self.binc(BinOp::Add, a, imm)
                };
                self.wr(rt, r);
                Flow::Next
            }
            Addic | AddicRc => {
                let a = self.rd(ra);
                let r = self.binc(BinOp::Add, a, i.simm() as u32);
                let ca = self.cmp(CmpOp::LtU, r, a);
                self.wr(rt, r);
                self.b.write(GuestReg::Ca, ca);
                if i.op == AddicRc {
                    self.record(r);
                }
                Flow::Next
            }
            Subfic => {
                // imm - rA = ~rA + imm + 1; the carry is imm >= rA unsigned.
                let a = self.rd(ra);
                let imm = self.k32(i.simm() as u32);
                let r = self.bin(BinOp::Sub, imm, a);
                let ca = self.cmp(CmpOp::GeU, imm, a);
                self.wr(rt, r);
                self.b.write(GuestReg::Ca, ca);
                Flow::Next
            }
            Mulli => {
                let a = self.rd(ra);
                let r = self.binc(BinOp::Mul, a, i.simm() as u32);
                self.wr(rt, r);
                Flow::Next
            }
            Add => {
                let (a, b) = (self.rd(ra), self.rd(rb));
                let r = self.bin(BinOp::Add, a, b);
                self.finish_xo(i, r, |l| l.add_ov(a, b, r));
                Flow::Next
            }
            Subf => {
                // rB - rA = ~rA + rB + 1.
                let (a, b) = (self.rd(ra), self.rd(rb));
                let r = self.bin(BinOp::Sub, b, a);
                self.finish_xo(i, r, |l| {
                    let na = l.not(a);
                    l.add_ov(na, b, r)
                });
                Flow::Next
            }
            Addc | Adde | Addme | Addze | Subfc | Subfe | Subfme | Subfze => {
                let a = self.rd(ra);
                // x + y + c, per form.
                let (x, y, c) = match i.op {
                    Addc => (a, self.rd(rb), self.k32(0)),
                    Adde => (a, self.rd(rb), self.b.read(GuestReg::Ca)),
                    Addme => (a, self.k32(u32::MAX), self.b.read(GuestReg::Ca)),
                    Addze => (a, self.k32(0), self.b.read(GuestReg::Ca)),
                    Subfc => (self.not(a), self.rd(rb), self.k32(1)),
                    Subfe => (self.not(a), self.rd(rb), self.b.read(GuestReg::Ca)),
                    Subfme => (self.not(a), self.k32(u32::MAX), self.b.read(GuestReg::Ca)),
                    _ => (self.not(a), self.k32(0), self.b.read(GuestReg::Ca)),
                };
                let (r, ca) = self.add_carry(x, y, c);
                self.b.write(GuestReg::Ca, ca);
                self.finish_xo(i, r, |l| l.add_ov(x, y, r));
                Flow::Next
            }
            Neg => {
                let a = self.rd(ra);
                let z = self.k32(0);
                let r = self.bin(BinOp::Sub, z, a);
                self.finish_xo(i, r, |l| l.cmpc(CmpOp::Eq, a, 0x8000_0000));
                Flow::Next
            }
            Mullw => {
                let (a, b) = (self.rd(ra), self.rd(rb));
                let r = self.bin(BinOp::Mul, a, b);
                self.finish_xo(i, r, |l| {
                    let a64 = l.b.extend(a, true);
                    let b64 = l.b.extend(b, true);
                    let p = l.bin64(BinOp::Mul, a64, b64);
                    let r64 = l.b.extend(r, true);
                    l.b.cmp(CmpOp::Ne, Ty::I64, p, r64)
                });
                Flow::Next
            }
            Mulhw | Mulhwu => {
                let signed = i.op == Mulhw;
                let (a, b) = (self.rd(ra), self.rd(rb));
                let a64 = self.b.extend(a, signed);
                let b64 = self.b.extend(b, signed);
                let p = self.bin64(BinOp::Mul, a64, b64);
                let k = self.k64(32);
                let hi = self.bin64(BinOp::ShrU, p, k);
                let r = self.b.wrap(hi);
                self.wr(rt, r);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }
            Divw | Divwu => {
                let signed = i.op == Divw;
                let (a, b) = (self.rd(ra), self.rd(rb));
                // Division by zero, and INT_MIN / -1, leave rD undefined and
                // set OV; wasm traps on them. The divisor is steered to 1, so
                // rD gets the dividend, which is what QEMU (the oracle) gives.
                let zero = self.cmpc(CmpOp::Eq, b, 0);
                let bad = if signed {
                    let min = self.cmpc(CmpOp::Eq, a, 0x8000_0000);
                    let m1 = self.cmpc(CmpOp::Eq, b, u32::MAX);
                    let o = self.bin(BinOp::And, min, m1);
                    self.bin(BinOp::Or, zero, o)
                } else {
                    zero
                };
                let one = self.k32(1);
                let d = self.sel(bad, one, b);
                let r = self.bin(if signed { BinOp::DivS } else { BinOp::DivU }, a, d);
                self.finish_xo(i, r, |_| bad);
                Flow::Next
            }

            // -- compares -------------------------------------------------------------
            Cmp | Cmpl => {
                let (a, b) = (self.rd(ra), self.rd(rb));
                self.set_cr_cmp(i.crfd(), a, b, i.op == Cmp);
                Flow::Next
            }
            Cmpi | Cmpli => {
                let a = self.rd(ra);
                let imm = if i.op == Cmpi { i.simm() as u32 } else { i.uimm() };
                let b = self.k32(imm);
                self.set_cr_cmp(i.crfd(), a, b, i.op == Cmpi);
                Flow::Next
            }

            // -- logical ------------------------------------------------------------
            AndiRc | AndisRc | Ori | Oris | Xori | Xoris => {
                let s = self.rd(rt);
                let imm = match i.op {
                    AndisRc | Oris | Xoris => i.uimm() << 16,
                    _ => i.uimm(),
                };
                let op = match i.op {
                    AndiRc | AndisRc => BinOp::And,
                    Ori | Oris => BinOp::Or,
                    _ => BinOp::Xor,
                };
                let r = self.binc(op, s, imm);
                self.wr(ra, r);
                if matches!(i.op, AndiRc | AndisRc) {
                    self.record(r);
                }
                Flow::Next
            }
            And | Andc | Or | Orc | Xor | Nand | Nor | Eqv => {
                let s = self.rd(rt);
                let b = self.rd(rb);
                let r = match i.op {
                    // `mr` (or rA, rS, rS) and its `and` twin are a copy.
                    And | Or if rt == rb => s,
                    And => self.bin(BinOp::And, s, b),
                    Or => self.bin(BinOp::Or, s, b),
                    Xor => self.bin(BinOp::Xor, s, b),
                    Andc => {
                        let nb = self.not(b);
                        self.bin(BinOp::And, s, nb)
                    }
                    Orc => {
                        let nb = self.not(b);
                        self.bin(BinOp::Or, s, nb)
                    }
                    Nand => {
                        let x = self.bin(BinOp::And, s, b);
                        self.not(x)
                    }
                    Nor => {
                        let x = self.bin(BinOp::Or, s, b);
                        self.not(x)
                    }
                    _ => {
                        let x = self.bin(BinOp::Xor, s, b);
                        self.not(x)
                    }
                };
                self.wr(ra, r);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }
            Extsb | Extsh | Cntlzw => {
                let s = self.rd(rt);
                let r = match i.op {
                    Cntlzw => self.b.un(UnOp::Clz, Ty::I32, s),
                    op => self.b.def(Expr::ExtendBits {
                        a: s,
                        ty: Ty::I32,
                        bits: if op == Extsb { 8 } else { 16 },
                        signed: true,
                    }),
                };
                self.wr(ra, r);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }

            // -- rotates and shifts -------------------------------------------------
            Rlwinm | Rlwnm | Rlwimi => {
                let s = self.rd(rt);
                let rot = if i.op == Rlwnm {
                    let n = self.rd(rb);
                    let n = self.binc(BinOp::And, n, 31);
                    self.rotl(s, n)
                } else if rb == 0 {
                    s
                } else {
                    let n = self.k32(rb as u32);
                    self.rotl(s, n)
                };
                let m = mask(i.mb(), i.me());
                let r = if i.op == Rlwimi {
                    let a = self.rd(ra);
                    let x = self.binc(BinOp::And, rot, m);
                    let y = self.binc(BinOp::And, a, !m);
                    self.bin(BinOp::Or, x, y)
                } else if m == u32::MAX {
                    rot
                } else {
                    self.binc(BinOp::And, rot, m)
                };
                self.wr(ra, r);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }
            Slw | Srw => {
                // The amount is rB's low six bits: 32..63 shift everything out.
                let s = self.rd(rt);
                let n = self.rd(rb);
                let n = self.binc(BinOp::And, n, 63);
                let op = if i.op == Slw { BinOp::Shl } else { BinOp::ShrU };
                let sh = self.bin(op, s, n);
                let big = self.cmpc(CmpOp::GeU, n, 32);
                let z = self.k32(0);
                let r = self.sel(big, z, sh);
                self.wr(ra, r);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }
            Sraw | Srawi => {
                let s = self.rd(rt);
                let n = if i.op == Srawi {
                    self.k32(rb as u32)
                } else {
                    let n = self.rd(rb);
                    self.binc(BinOp::And, n, 63)
                };
                // 32..63 shift in only sign bits, which is a shift by 31.
                let big = self.cmpc(CmpOp::GeU, n, 32);
                let k31 = self.k32(31);
                let eff = self.sel(big, k31, n);
                let r = self.bin(BinOp::ShrS, s, eff);
                // CA: the source is negative and a one bit was shifted out.
                let neg = self.cmpc(CmpOp::LtS, s, 0);
                let one = self.k32(1);
                let lowmask = {
                    let sh = self.bin(BinOp::Shl, one, eff);
                    let m = self.binc(BinOp::Sub, sh, 1);
                    // For n >= 32 every bit is shifted out.
                    let all = self.k32(u32::MAX);
                    self.sel(big, all, m)
                };
                let lost = self.bin(BinOp::And, s, lowmask);
                let lost = self.cmpc(CmpOp::Ne, lost, 0);
                let ca = self.bin(BinOp::And, neg, lost);
                self.wr(ra, r);
                self.b.write(GuestReg::Ca, ca);
                if i.rc() {
                    self.record(r);
                }
                Flow::Next
            }

            // -- loads and stores ----------------------------------------------------
            Lbz | Lbzu | Lhz | Lhzu | Lha | Lhau | Lwz | Lwzu => {
                let (sz, signed) = match i.op {
                    Lbz | Lbzu => (MemSz::B1, false),
                    Lhz | Lhzu => (MemSz::B2, false),
                    Lha | Lhau => (MemSz::B2, true),
                    _ => (MemSz::B4, false),
                };
                let update = matches!(i.op, Lbzu | Lhzu | Lhau | Lwzu);
                if update && (ra == 0 || ra == rt) {
                    return self.illegal();
                }
                if update {
                    let ea = self.ea_d_full(ra, i.simm());
                    let v = self.load(ea, 0, sz, signed, false);
                    self.wr(rt, v);
                    self.wr(ra, ea);
                } else {
                    let (a, off) = self.ea_d(ra, i.simm());
                    let v = self.load(a, off, sz, signed, false);
                    self.wr(rt, v);
                }
                Flow::Next
            }
            Lbzx | Lbzux | Lhzx | Lhzux | Lhax | Lhaux | Lwzx | Lwzux | Lhbrx | Lwbrx => {
                let (sz, signed) = match i.op {
                    Lbzx | Lbzux => (MemSz::B1, false),
                    Lhzx | Lhzux | Lhbrx => (MemSz::B2, false),
                    Lhax | Lhaux => (MemSz::B2, true),
                    _ => (MemSz::B4, false),
                };
                let update = matches!(i.op, Lbzux | Lhzux | Lhaux | Lwzux);
                if update && (ra == 0 || ra == rt) {
                    return self.illegal();
                }
                let reverse = matches!(i.op, Lhbrx | Lwbrx);
                let ea = self.ea_x(ra, rb);
                let v = self.load(ea, 0, sz, signed, reverse);
                self.wr(rt, v);
                if update {
                    self.wr(ra, ea);
                }
                Flow::Next
            }
            Stb | Stbu | Sth | Sthu | Stw | Stwu => {
                let sz = match i.op {
                    Stb | Stbu => MemSz::B1,
                    Sth | Sthu => MemSz::B2,
                    _ => MemSz::B4,
                };
                let update = matches!(i.op, Stbu | Sthu | Stwu);
                if update && ra == 0 {
                    return self.illegal();
                }
                // Read rS before rA is updated: `stwu 1, -16(1)` stores the
                // old stack pointer.
                let s = self.rd(rt);
                if update {
                    let ea = self.ea_d_full(ra, i.simm());
                    self.store(ea, 0, sz, s, false);
                    self.wr(ra, ea);
                } else {
                    let (a, off) = self.ea_d(ra, i.simm());
                    self.store(a, off, sz, s, false);
                }
                Flow::Next
            }
            Stbx | Stbux | Sthx | Sthux | Stwx | Stwux | Sthbrx | Stwbrx => {
                let sz = match i.op {
                    Stbx | Stbux => MemSz::B1,
                    Sthx | Sthux | Sthbrx => MemSz::B2,
                    _ => MemSz::B4,
                };
                let update = matches!(i.op, Stbux | Sthux | Stwux);
                if update && ra == 0 {
                    return self.illegal();
                }
                let reverse = matches!(i.op, Sthbrx | Stwbrx);
                let s = self.rd(rt);
                let ea = self.ea_x(ra, rb);
                self.store(ea, 0, sz, s, reverse);
                if update {
                    self.wr(ra, ea);
                }
                Flow::Next
            }
            Lmw => {
                // rA in the range loaded is an invalid form.
                if ra >= rt && ra != 0 {
                    return self.illegal();
                }
                let (base, off) = self.ea_d(ra, i.simm());
                let vals: Vec<Value> = (rt..32)
                    .map(|n| self.load(base, off + 4 * (n - rt) as i64, MemSz::B4, false, false))
                    .collect();
                for (n, v) in (rt..32).zip(vals) {
                    self.wr(n, v);
                }
                Flow::Next
            }
            Stmw => {
                let (base, off) = self.ea_d(ra, i.simm());
                for n in rt..32 {
                    let v = self.rd(n);
                    self.store(base, off + 4 * (n - rt) as i64, MemSz::B4, v, false);
                }
                Flow::Next
            }
            Lswi | Stswi => self.lift_string_imm(i),
            // The XER byte-count forms: rare enough that no compiler emits
            // them; refused rather than guessed at.
            Lswx | Stswx => self.illegal(),
            Lwarx => {
                let ea = self.ea_x(ra, rb);
                let raw = self.b.def(Expr::Load {
                    addr: ea,
                    offset: 0,
                    sz: MemSz::B4,
                    signed: false,
                    ty: Ty::I32,
                    order: Order::SeqCst,
                });
                let v = self.b.un(UnOp::Bswap, Ty::I32, raw);
                let ea64 = self.b.extend(ea, false);
                let raw64 = self.b.extend(raw, false);
                let four = self.k64(4);
                self.b.write(GuestReg::MonAddr, ea64);
                self.b.write(GuestReg::MonValue, raw64);
                self.b.write(GuestReg::MonSize, four);
                self.wr(rt, v);
                Flow::Next
            }
            Stwcx => {
                // AArch64's STXR lowering (docs/ppc.md §9): a CAS against the
                // value the reservation saw, at the reserved address if the
                // reservation is for this address and size, and otherwise at
                // a scratch word, so the failure costs no branch.
                let ea = self.ea_x(ra, rb);
                let s = self.rd(rt);
                let ea64 = self.b.extend(ea, false);
                let mon_addr = self.b.read(GuestReg::MonAddr);
                let mon_size = self.b.read(GuestReg::MonSize);
                let four = self.k64(4);
                let addr_ok = self.b.cmp(CmpOp::Eq, Ty::I64, mon_addr, ea64);
                let size_ok = self.b.cmp(CmpOp::Eq, Ty::I64, mon_size, four);
                let armed = self.bin(BinOp::And, addr_ok, size_ok);
                let scratch = self.k32(gex_abi::EXCLUSIVE_SCRATCH as u32);
                let addr = self.sel(armed, ea, scratch);
                let mon_value = self.b.read(GuestReg::MonValue);
                let expected = self.b.wrap(mon_value);
                let repl = self.b.un(UnOp::Bswap, Ty::I32, s);
                let old = self.b.def(Expr::CmpXchg {
                    addr,
                    expected,
                    replacement: repl,
                    sz: MemSz::B4,
                    ty: Ty::I32,
                });
                let swapped = self.cmp(CmpOp::Eq, old, expected);
                let ok = self.bin(BinOp::And, armed, swapped);
                let zero = self.k32(0);
                let zero64 = self.k64(0);
                let so = self.b.read(GuestReg::So);
                let [l, g, e, sob] = cr_bits(0);
                self.b.write(l, zero);
                self.b.write(g, zero);
                self.b.write(e, ok);
                self.b.write(sob, so);
                self.b.write(GuestReg::MonSize, zero64);
                Flow::Next
            }

            // -- floating point --------------------------------------------------------
            Lfs | Lfsu | Lfd | Lfdu | Stfs | Stfsu | Stfd | Stfdu | Lfsx | Lfsux | Lfdx
            | Lfdux | Stfsx | Stfsux | Stfdx | Stfdux | Stfiwx => self.lift_fp_mem(i),
            Fadd | Fsub | Fmul | Fdiv | Fsqrt | Fsel | Fre | Frsqrte | Fmadd | Fmsub | Fnmadd
            | Fnmsub | Fadds | Fsubs | Fmuls | Fdivs | Fsqrts | Fres | Frsqrtes | Fmadds
            | Fmsubs | Fnmadds | Fnmsubs | Fmr | Fneg | Fabs | Fnabs | Frsp | Fctiw | Fctiwz
            | Fcmpu | Fcmpo | Mffs | Mtfsf | Mtfsfi | Mtfsb0 | Mtfsb1 | Mcrfs => self.lift_fp(i),

            // -- SPRs, caches, barriers -----------------------------------------------
            Mfspr => {
                let v = match i.spr() {
                    1 => self.read_xer(),
                    8 => self.b.read(GuestReg::Lr),
                    9 => self.b.read(GuestReg::Ctr),
                    // VRSAVE reads as zero without AltiVec.
                    256 => self.k32(0),
                    // Time base via mfspr 268/269 is the Power ISA 2.x spelling.
                    268 | 269 => self.timebase(i.spr() == 269),
                    _ => return self.illegal(),
                };
                self.wr(rt, v);
                Flow::Next
            }
            Mtspr => {
                let v = self.rd(rt);
                match i.spr() {
                    1 => self.write_xer(v),
                    8 => self.b.write(GuestReg::Lr, v),
                    9 => self.b.write(GuestReg::Ctr, v),
                    256 => {}
                    _ => return self.illegal(),
                }
                Flow::Next
            }
            Mftb => {
                let v = match i.spr() {
                    268 => self.timebase(false),
                    269 => self.timebase(true),
                    _ => return self.illegal(),
                };
                self.wr(rt, v);
                Flow::Next
            }
            Sync | Eieio => {
                self.b.stmt(Stmt::Fence);
                Flow::Next
            }
            // A translated block is already the synchronisation point.
            Isync => Flow::Next,
            Icbi => {
                let ea = self.ea_x(ra, rb);
                let ea64 = self.b.extend(ea, false);
                let pcv = self.k64(pc);
                let code = self.k32(gex_abi::cache_op::IC_INVALIDATE as u32);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::CacheOp,
                    args: vec![pcv, code, ea64],
                });
                Flow::Next
            }
            Dcbz => {
                // The 32-byte block containing EA (AT_DCACHEBSIZE = 32).
                let ea = self.ea_x(ra, rb);
                let dst = self.binc(BinOp::And, ea, !31u32);
                // `MemFill` takes its value and length as `I64`s.
                let val = self.k64(0);
                let len = self.k64(32);
                self.b.stmt(Stmt::MemFill { dst, val, len });
                Flow::Next
            }
            Dcbf | Dcbst | Dcbt | Dcbtst => Flow::Next,

            // -- supervisor ---------------------------------------------------------
            Dcbi | Mfmsr | Mtmsr | Mfsr | Mfsrin | Mtsr | Mtsrin | Rfi | Tlbie | Tlbsync => {
                self.illegal()
            }
        }
    }

    /// The 32-bit CR, bit 0 (CR0.LT) most significant.
    pub(crate) fn read_cr(&mut self) -> Value {
        let mut acc: Option<Value> = None;
        for bit in 0..32u8 {
            let v = self.b.read(GuestReg::Cr(bit));
            let placed = if bit == 31 {
                v
            } else {
                self.binc(BinOp::Shl, v, 31 - bit as u32)
            };
            acc = Some(match acc {
                None => placed,
                Some(a) => self.bin(BinOp::Or, a, placed),
            });
        }
        acc.unwrap()
    }

    fn read_xer(&mut self) -> Value {
        let so = self.b.read(GuestReg::So);
        let ov = self.b.read(GuestReg::Ov);
        let ca = self.b.read(GuestReg::Ca);
        let bc = self.b.read(GuestReg::XerBc);
        let so = self.binc(BinOp::Shl, so, 31);
        let ov = self.binc(BinOp::Shl, ov, 30);
        let ca = self.binc(BinOp::Shl, ca, 29);
        let x = self.bin(BinOp::Or, so, ov);
        let x = self.bin(BinOp::Or, x, ca);
        self.bin(BinOp::Or, x, bc)
    }

    fn write_xer(&mut self, v: Value) {
        for (sh, reg) in [(31u32, GuestReg::So), (30, GuestReg::Ov), (29, GuestReg::Ca)] {
            let s = self.binc(BinOp::ShrU, v, sh);
            let bit = self.binc(BinOp::And, s, 1);
            self.b.write(reg, bit);
        }
        let bc = self.binc(BinOp::And, v, 0x7f);
        self.b.write(GuestReg::XerBc, bc);
    }

    /// TBL or TBU from the host's nanosecond counter.
    fn timebase(&mut self, upper: bool) -> Value {
        let pcv = self.k64(self.pc);
        let id = self.k32(sysreg::TIMEBASE as u32);
        let t = self.b.def(Expr::HostCall {
            func: HostFunc::SysRegRead,
            args: vec![pcv, id],
            ty: Ty::I64,
        });
        let t = if upper {
            let k = self.k64(32);
            self.bin64(BinOp::ShrU, t, k)
        } else {
            t
        };
        self.b.wrap(t)
    }

    /// `bc`: decrement CTR if asked, test it and a CR bit if asked, branch.
    fn branch_cond(&mut self, bo: u8, bi: u8) -> Option<Value> {
        let mut conds = Vec::new();
        if bo & 0b00100 == 0 {
            let ctr = self.b.read(GuestReg::Ctr);
            let ctr = self.binc(BinOp::Sub, ctr, 1);
            self.b.write(GuestReg::Ctr, ctr);
            // BO[3]: branch when CTR reached zero, else when it did not.
            let op = if bo & 0b00010 != 0 { CmpOp::Eq } else { CmpOp::Ne };
            conds.push(self.cmpc(op, ctr, 0));
        }
        if bo & 0b10000 == 0 {
            let bit = self.b.read(GuestReg::Cr(bi));
            // BO[1]: branch when the bit is set, else when clear.
            let c = if bo & 0b01000 != 0 {
                bit
            } else {
                self.binc(BinOp::Xor, bit, 1)
            };
            conds.push(c);
        }
        match conds.len() {
            0 => None,
            1 => Some(conds[0]),
            _ => Some(self.bin(BinOp::And, conds[0], conds[1])),
        }
    }

    fn lift_bc(&mut self, i: &Inst) -> Flow {
        let target = i.target(self.pc);
        let cond = self.branch_cond(i.bo(), i.bi());
        if i.lk() {
            let n = self.k32(self.next as u32);
            self.b.write(GuestReg::Lr, n);
            // `bcl 20, 31, .+4` reads the PC into LR and is no call.
            if target != self.next {
                self.b.set_call_return(self.next);
            }
        }
        Flow::End(match cond {
            None => Term::Goto { target },
            Some(cond) => Term::Branch {
                cond,
                taken: target,
                not_taken: self.next,
            },
        })
    }

    /// `bclr` and `bcctr`. The target is read before LR is written, so
    /// `blrl` calls the old LR.
    fn lift_bc_reg(&mut self, i: &Inst) -> Flow {
        let reg = if i.op == Op::Bclr { GuestReg::Lr } else { GuestReg::Ctr };
        let t = self.b.read(reg);
        let t = self.binc(BinOp::And, t, !3u32);
        let cond = self.branch_cond(i.bo(), i.bi());
        if i.lk() {
            let n = self.k32(self.next as u32);
            self.b.write(GuestReg::Lr, n);
            self.b.set_call_return(self.next);
        }
        let (t, is_ret) = match cond {
            None => (t, i.op == Op::Bclr && !i.lk()),
            Some(c) => {
                let n = self.k32(self.next as u32);
                (self.sel(c, t, n), false)
            }
        };
        let target = self.b.extend(t, false);
        Flow::End(Term::Indirect { target, is_ret })
    }

    /// `tw`/`twi`. An unconditional trap (`trap`, TO = 31 against itself)
    /// raises SIGTRAP; a trap that can never fire is nothing; a conditional
    /// one is refused (docs/ppc.md §7: no C compiler emits one).
    fn lift_trap(&mut self, i: &Inst) -> Flow {
        let to = i.rt();
        let same = i.op == Op::Tw && i.ra() == i.rb();
        // Against itself only the equal/le/ge conditions can hold.
        let always = to & 0b00100 != 0 && same;
        if to == 0 || (same && to & 0b00100 == 0) {
            return Flow::Next;
        }
        if always || to == 31 {
            let pcv = self.k64(self.pc);
            let imm = self.k32(0);
            self.b.stmt(Stmt::HostCall {
                func: HostFunc::Breakpoint,
                args: vec![pcv, imm],
            });
            return Flow::End(Term::Goto { target: self.next });
        }
        self.illegal()
    }

    /// `lswi`/`stswi`: NB bytes (32 when NB is 0) to or from consecutive
    /// registers from rT, four to a register, big-endian, wrapping r31 to r0;
    /// a partial last register is left-justified and zero-filled.
    fn lift_string_imm(&mut self, i: &Inst) -> Flow {
        let nb = if i.rb() == 0 { 32 } else { i.rb() as u32 };
        let nregs = nb.div_ceil(4) as u8;
        let rt = i.rt();
        let ra = i.ra();
        if i.op == Op::Lswi {
            // rA (as a base) inside the range loaded is invalid.
            if ra != 0 && (0..nregs).any(|k| (rt + k) % 32 == ra) {
                return self.illegal();
            }
        }
        let base = self.rd0(ra);
        let mut done = 0u32;
        let mut r = rt;
        while done < nb {
            let n = (nb - done).min(4);
            if i.op == Op::Lswi {
                let mut acc = self.k32(0);
                for k in 0..n {
                    let byte = self.load(base, (done + k) as i64, MemSz::B1, false, false);
                    let sh = self.binc(BinOp::Shl, byte, 24 - 8 * k);
                    acc = self.bin(BinOp::Or, acc, sh);
                }
                self.wr(r, acc);
            } else {
                let v = self.rd(r);
                for k in 0..n {
                    let sh = self.binc(BinOp::ShrU, v, 24 - 8 * k);
                    self.store(base, (done + k) as i64, MemSz::B1, sh, false);
                }
            }
            done += n;
            r = (r + 1) % 32;
        }
        Flow::Next
    }
}

/// The rotate mask from `MB` to `ME` in Power ISA bit order (bit 0 the most
/// significant), wrapping when `MB > ME`.
pub fn mask(mb: u8, me: u8) -> u32 {
    let from_mb = u32::MAX >> mb;
    let to_me = u32::MAX << (31 - me);
    if mb <= me {
        from_mb & to_me
    } else {
        from_mb | to_me
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_follow_the_isa_bit_order() {
        assert_eq!(mask(0, 31), u32::MAX);
        assert_eq!(mask(0, 0), 0x8000_0000);
        assert_eq!(mask(31, 31), 1);
        assert_eq!(mask(16, 31), 0xffff);
        // Wrapping: bits 30, 31, 0, 1.
        assert_eq!(mask(30, 1), 0xc000_0003);
    }
}
