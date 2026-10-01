//! The x87's format conversions, lowered inline: `fld`/`fild` from memory
//! and the constants, `fst`/`fstp` to single, double and extended, and
//! `fist`/`fistp`/`fisttp`.
//!
//! These are most of what the x87 does in compiled code (every float or
//! double a 32-bit function returns goes through `st(0)`), so they must not
//! cost a state-flushing host call. Each is exact: the same results,
//! exception flags and C1 as `gex_f80` (whose tests check it against the
//! hardware). The loads widen with integer IR here; the stores that round
//! (`fst m32/m64`, `fist`) are a pure `Stmt::X87`, which the backend lowers
//! to a fast path for the common operands and the exact algorithm for the
//! rest. Stack overflow and underflow are handled as the host
//! handles them; an exception the control word leaves unmasked fails loudly
//! through `Stmt::Trap` (gex does not deliver x87 exceptions).

use crate::{Flow, Lifter};
use gex_ir::*;
use gex_isa_x86::{Operand, Width, X87Mem, X87Src};

/// Status-word exception bits.
const IE: u32 = 1;
const DE: u32 = 2;
const SF: u32 = 64;
/// C1's bit in the status word.
const C1_BIT: u32 = 9;

/// A binary floating-point memory format.
#[derive(Copy, Clone)]
struct Fmt {
    /// Fraction bits (the significand less its implicit integer bit).
    frac: u32,
    /// Exponent bias, which is also the largest exponent.
    bias: i64,
    /// Total width in bits.
    bits: u32,
}

const SINGLE: Fmt = Fmt {
    frac: 23,
    bias: 127,
    bits: 32,
};
const DOUBLE: Fmt = Fmt {
    frac: 52,
    bias: 1023,
    bits: 64,
};

/// The "real indefinite" QNaN, as (significand, sign/exponent).
const INDEF_SIG: u64 = 0xc000_0000_0000_0000;
const INDEF_SE: u32 = 0xffff;

impl<'a> Lifter<'a> {
    // -- small IR helpers -----------------------------------------------------

    fn b64(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        self.b.bin(op, Ty::I64, a, b)
    }
    fn b32(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        self.b.bin(op, Ty::I32, a, b)
    }
    fn c64(&mut self, op: CmpOp, a: Value, b: Value) -> Value {
        self.b.cmp(op, Ty::I64, a, b)
    }
    fn c32(&mut self, op: CmpOp, a: Value, b: Value) -> Value {
        self.b.cmp(op, Ty::I32, a, b)
    }
    fn s64(&mut self, c: Value, a: Value, b: Value) -> Value {
        self.b.select(c, a, b, Ty::I64)
    }
    fn s32(&mut self, c: Value, a: Value, b: Value) -> Value {
        self.b.select(c, a, b, Ty::I32)
    }
    fn and32(&mut self, a: Value, b: Value) -> Value {
        self.b32(BinOp::And, a, b)
    }
    fn or32(&mut self, a: Value, b: Value) -> Value {
        self.b32(BinOp::Or, a, b)
    }
    fn not1(&mut self, a: Value) -> Value {
        let one = self.k32(1);
        self.b32(BinOp::Xor, a, one)
    }
    /// `flag` (an I32 0/1) times `bits`.
    fn flag(&mut self, cond: Value, bits: u32) -> Value {
        let k = self.k32(bits);
        let z = self.k32(0);
        self.s32(cond, k, z)
    }

    /// The rounding mode, decoded once per conversion for a value of sign
    /// `sign`: (round to nearest, round away from zero -- up for a positive
    /// value, down for a negative one), as I32 0/1.
    fn x87_mode(&mut self, sign: Value) -> (Value, Value) {
        let cw = self.b.read(GuestReg::X87Cw);
        let ten = self.k32(10);
        let rc = self.b32(BinOp::ShrU, cw, ten);
        let three = self.k32(3);
        let rc = self.and32(rc, three);
        let z = self.k32(0);
        let near = self.c32(CmpOp::Eq, rc, z);
        // RC 01 rounds down (away for a negative value), 10 up (away for a
        // positive one): away from zero exactly when rc == 2 - sign... that
        // is, rc == 1 with the sign set or rc == 2 with it clear.
        let two = self.k32(2);
        let want = self.b32(BinOp::Sub, two, sign);
        let away = self.c32(CmpOp::Eq, rc, want);
        (near, away)
    }

    /// Fold an operation's exceptions into the status word, set C1 (an I32
    /// 0/1), and fail loudly on an exception the control word unmasks.
    fn x87_raise(&mut self, flags: Value, c1: Value) {
        let sw = self.b.read(GuestReg::X87Sw);
        let m = self.k32(!(1 << C1_BIT));
        let sw = self.and32(sw, m);
        let sw = self.or32(sw, flags);
        let n = self.k32(C1_BIT);
        let c1 = self.b32(BinOp::Shl, c1, n);
        let sw = self.or32(sw, c1);
        self.b.write(GuestReg::X87Sw, sw);
        let cw = self.b.read(GuestReg::X87Cw);
        let masked = self.b.un(UnOp::Not, Ty::I32, cw);
        let unmasked = self.and32(flags, masked);
        let m3f = self.k32(0x3f);
        let unmasked = self.and32(unmasked, m3f);
        let z = self.k32(0);
        let any = self.c32(CmpOp::Ne, unmasked, z);
        self.b.stmt(Stmt::Trap {
            cond: any,
            pc: self.pc,
            code: gex_abi::imports::BRK_X87_EXCEPTION,
        });
    }

    /// Whether `st(0)` is empty (its tag clear), as an I32 0/1.
    fn x87_st0_empty(&mut self) -> Value {
        let top = self.top();
        let tags = self.b.read(GuestReg::X87Tags);
        let t = self.b32(BinOp::ShrU, tags, top);
        let one = self.k32(1);
        let valid = self.and32(t, one);
        self.not1(valid)
    }

    /// Push `v` with the stack-overflow check: onto a full register the
    /// indefinite goes instead, with IE, SF and C1. Returns the flags and C1
    /// the push itself raised.
    fn x87_push_checked(&mut self, (sig, se): (Value, Value)) -> (Value, Value) {
        let top = self.top();
        let seven = self.k32(7);
        let nt = self.b32(BinOp::Add, top, seven);
        let nt = self.and32(nt, seven);
        let tags = self.b.read(GuestReg::X87Tags);
        let t = self.b32(BinOp::ShrU, tags, nt);
        let one = self.k32(1);
        let full = self.and32(t, one);
        let isig = self.k64(INDEF_SIG);
        let ise = self.k32(INDEF_SE);
        let sig = self.s64(full, isig, sig);
        let se = self.s32(full, ise, se);
        self.push_st((sig, se));
        let flags = self.flag(full, IE | SF);
        (flags, full)
    }

    // -- loads ----------------------------------------------------------------

    /// A single or double (raw bits, zero-extended in an I64) widened to
    /// extended, exactly: (value, flags). An SNaN is quieted with IE; a
    /// denormal raises DE.
    fn x87_widen(&mut self, v: Value, f: Fmt) -> ((Value, Value), Value) {
        self.x87_widen_q(v, f, true)
    }

    /// `x87_widen`, quieting an SNaN only when `quiet`: an arithmetic or
    /// compare operand keeps it signalling, so the operation raises IE and
    /// picks between NaNs as the x87 does (a QNaN beats an SNaN).
    fn x87_widen_q(&mut self, v: Value, f: Fmt, quiet: bool) -> ((Value, Value), Value) {
        let exp_bits = f.bits - 1 - f.frac;
        let emax_field = (1u64 << exp_bits) - 1;
        let sh = self.k64((f.bits - 1) as u64);
        let sign = self.b64(BinOp::ShrU, v, sh);
        let sign = self.b.wrap(sign); // the value is zero-extended: 0 or 1
        let fr = self.k64(f.frac as u64);
        let exp = self.b64(BinOp::ShrU, v, fr);
        let em = self.k64(emax_field);
        let exp = self.b64(BinOp::And, exp, em);
        let fmask = self.k64((1u64 << f.frac) - 1);
        let frac = self.b64(BinOp::And, v, fmask);
        let z64 = self.k64(0);
        let exp0 = self.c64(CmpOp::Eq, exp, z64);
        let expmax = self.c64(CmpOp::Eq, exp, em);
        let frac0 = self.c64(CmpOp::Eq, frac, z64);
        let nonzero = self.not1(frac0);

        // The significand with its integer bit (absent for a denormal),
        // normalised by its leading zeros: one path for normals and
        // denormals. value = m * 2^(max(exp, 1) - bias - frac).
        let n = self.not1(exp0);
        let n64 = self.b.extend(n, false);
        let ib = self.b64(BinOp::Shl, n64, fr);
        let m = self.b64(BinOp::Or, frac, ib);
        let lz = self.b.un(UnOp::Clz, Ty::I64, m);
        let sig = self.b64(BinOp::Shl, m, lz); // 0 for a zero (shift by 64 is by 0 of 0)
        let one64 = self.k64(1);
        let e1 = self.s64(exp0, one64, exp);
        let k = self.k64((16383 - f.bias - f.frac as i64 + 63) as u64);
        let e = self.b64(BinOp::Add, e1, k);
        let e = self.b64(BinOp::Sub, e, lz);
        let zero = self.and32(exp0, frac0);
        let e = self.s64(zero, z64, e);
        // Infinity and NaN: the fraction moves up, a NaN comes back quiet.
        let up = self.k64((63 - f.frac) as u64);
        let fs = self.b64(BinOp::Shl, frac, up);
        let top2 = self.k64(if quiet { 3 << 62 } else { 1 << 63 });
        let ibit = self.k64(1 << 63);
        let q = self.s64(nonzero, top2, ibit);
        let sig_s = self.b64(BinOp::Or, fs, q);
        let e_s = self.k64(0x7fff);
        let sig = self.s64(expmax, sig_s, sig);
        let e = self.s64(expmax, e_s, e);
        let e = self.b.wrap(e);
        let fifteen = self.k32(15);
        let sbit = self.b32(BinOp::Shl, sign, fifteen);
        let se = self.or32(e, sbit);

        // IE for an SNaN (quiet bit clear), DE for a denormal.
        let qb = self.k64(1u64 << (f.frac - 1));
        let qbit = self.b64(BinOp::And, frac, qb);
        let quiet_clear = self.c64(CmpOp::Eq, qbit, z64);
        let snan = self.and32(expmax, nonzero);
        let snan = self.and32(snan, quiet_clear);
        let den = self.and32(exp0, nonzero);
        let ie = self.flag(snan, IE);
        let de = self.flag(den, DE);
        let flags = self.or32(ie, de);
        ((sig, se), flags)
    }

    /// A signed integer (in an I64) to extended; always exact.
    fn x87_int_to_ext(&mut self, v: Value) -> (Value, Value) {
        let z = self.k64(0);
        let neg = self.c64(CmpOp::LtS, v, z);
        let nv = self.b64(BinOp::Sub, z, v);
        let mag = self.s64(neg, nv, v);
        let lz = self.b.un(UnOp::Clz, Ty::I64, mag);
        let sig = self.b64(BinOp::Shl, mag, lz);
        let k = self.k64(16383 + 63);
        let e = self.b64(BinOp::Sub, k, lz);
        let is0 = self.c64(CmpOp::Eq, mag, z);
        let e = self.s64(is0, z, e);
        let e = self.b.wrap(e);
        let fifteen = self.k32(15);
        let sb = self.b32(BinOp::Shl, neg, fifteen);
        let se = self.or32(e, sb);
        (sig, se)
    }

    /// `fld`/`fild` from memory, inline.
    /// A memory operand converted to extended, exactly: (value, the flags
    /// the conversion raises -- IE for an SNaN, DE for a denormal).
    pub(crate) fn x87_mem_value(&mut self, o: Operand, m: X87Mem) -> Option<((Value, Value), Value)> {
        self.x87_mem_value_q(o, m, true)
    }

    /// `x87_mem_value`, quieting an SNaN only when `quiet` (see
    /// `x87_widen_q`).
    pub(crate) fn x87_mem_value_q(&mut self, o: Operand, m: X87Mem, quiet: bool) -> Option<((Value, Value), Value)> {
        let Operand::Mem(mem) = o else {
            return None;
        };
        let (addr, offset) = self.addr(&mem);
        let ld = |this: &mut Self, off: i64, sz: MemSz, signed: bool| {
            this.b.def(Expr::Load {
                addr,
                offset: off,
                sz,
                signed,
                ty: Ty::I64,
                order: Order::Relaxed,
            })
        };
        Some(match m {
            X87Mem::F32 => {
                let raw = ld(self, offset, MemSz::B4, false);
                self.x87_widen_q(raw, SINGLE, quiet)
            }
            X87Mem::F64 => {
                let raw = ld(self, offset, MemSz::B8, false);
                self.x87_widen_q(raw, DOUBLE, quiet)
            }
            X87Mem::F80 => {
                // Loaded as it is: fld m80 raises nothing.
                let sig = ld(self, offset, MemSz::B8, false);
                let se = self.b.def(Expr::Load {
                    addr,
                    offset: offset + 8,
                    sz: MemSz::B2,
                    signed: false,
                    ty: Ty::I32,
                    order: Order::Relaxed,
                });
                let z = self.k32(0);
                ((sig, se), z)
            }
            X87Mem::I16 | X87Mem::I32 | X87Mem::I64 => {
                let sz = match m {
                    X87Mem::I16 => MemSz::B2,
                    X87Mem::I32 => MemSz::B4,
                    _ => MemSz::B8,
                };
                let raw = ld(self, offset, sz, true);
                let z = self.k32(0);
                (self.x87_int_to_ext(raw), z)
            }
        })
    }

    /// Whether `st(i)` is empty (its tag clear), as an I32 0/1.
    pub(crate) fn x87_st_empty(&mut self, i: u8) -> Value {
        let top = self.top();
        let iv = self.k32(i as u32);
        let idx = self.b32(BinOp::Add, top, iv);
        let seven = self.k32(7);
        let idx = self.and32(idx, seven);
        let tags = self.b.read(GuestReg::X87Tags);
        let t = self.b32(BinOp::ShrU, tags, idx);
        let one = self.k32(1);
        let valid = self.and32(t, one);
        self.not1(valid)
    }

    /// Combine an operation's flags `f` with a memory operand's conversion
    /// flags `extra`: its IE always, its DE when `f` has neither IE nor ZE
    /// and the result (if given) is not a NaN.
    fn x87_mem_flags(&mut self, f: Value, extra: Value, result: Option<(Value, Value)>) -> Value {
        let ize = self.k32(IE | 4);
        let bad = self.and32(f, ize);
        let z = self.k32(0);
        let mut ok = self.c32(CmpOp::Eq, bad, z);
        if let Some((sig, se)) = result {
            let m = self.k32(0x7fff);
            let e = self.and32(se, m);
            let emax = self.c32(CmpOp::Eq, e, m);
            let one = self.k64(1);
            let low = self.b64(BinOp::Shl, sig, one);
            let z64 = self.k64(0);
            let frac = self.c64(CmpOp::Ne, low, z64);
            let nan = self.and32(emax, frac);
            let notnan = self.not1(nan);
            ok = self.and32(ok, notnan);
        }
        self.x87_mem_flags_if(f, extra, ok)
    }

    /// `f | IE of extra | (DE of extra if ok)`.
    fn x87_mem_flags_if(&mut self, f: Value, extra: Value, ok: Value) -> Value {
        let iem = self.k32(IE);
        let ie = self.and32(extra, iem);
        let dem = self.k32(DE);
        let de = self.and32(extra, dem);
        let z = self.k32(0);
        let de = self.s32(ok, de, z);
        let r = self.or32(f, ie);
        self.or32(r, de)
    }

    /// `fadd`/`fsub`/`fmul`/`fdiv` (with `rev` for `fsubr`/`fdivr`) on the
    /// form's operands, through a pure `Stmt::X87`. An empty operand
    /// register is a stack underflow: the indefinite with IE and SF.
    pub(crate) fn x87_arith_op(&mut self, k: X87Arith, src: X87Src, rev: bool) -> Flow {
        let mut extra = self.k32(0);
        let (a, b, dst, empty, pop) = match src {
            X87Src::MemF(_) | X87Src::MemI(_) => {
                let m = match src {
                    X87Src::MemF(w) if w == Width::Q => X87Mem::F64,
                    X87Src::MemF(_) => X87Mem::F32,
                    X87Src::MemI(w) if w == Width::W => X87Mem::I16,
                    _ => X87Mem::I32,
                };
                let Some((v, f)) = self.x87_mem_value_q(self.cur_mem, m, false) else {
                    return self.illegal_x87();
                };
                extra = f;
                let st0 = self.ld_st(0);
                let e = self.x87_st_empty(0);
                (st0, v, 0, e, false)
            }
            X87Src::St0Sti(i) => {
                let st0 = self.ld_st(0);
                let sti = self.ld_st(i);
                let e0 = self.x87_st_empty(0);
                let ei = self.x87_st_empty(i);
                let e = self.or32(e0, ei);
                (st0, sti, 0, e, false)
            }
            X87Src::StiSt0 { i, pop } => {
                let st0 = self.ld_st(0);
                let sti = self.ld_st(i);
                let e0 = self.x87_st_empty(0);
                let ei = self.x87_st_empty(i);
                let e = self.or32(e0, ei);
                (sti, st0, i, e, pop)
            }
        };
        let (a, b) = if rev { (b, a) } else { (a, b) };
        let cw = self.b.read(GuestReg::X87Cw);
        let outs = self.b.x87(X87Op::Arith(k), vec![a.0, a.1, b.0, b.1, cw]);
        let isig = self.k64(INDEF_SIG);
        let ise = self.k32(INDEF_SE);
        let sig = self.s64(empty, isig, outs[0]);
        let se = self.s32(empty, ise, outs[1]);
        self.st_st(dst, (sig, se));
        let m = self.k32(0x7f);
        let f = self.and32(outs[2], m);
        let sf = self.k32(IE | SF);
        let f = self.s32(empty, sf, f);
        // The memory operand's conversion flags: IE for an SNaN always; DE
        // for a denormal only when the operation itself would report a
        // denormal operand -- not beside IE or ZE, nor with a NaN result
        // (the x87 checks NaNs and invalid operations first).
        let flags = self.x87_mem_flags(f, extra, Some((sig, se)));
        let n = self.k32(C1_BIT);
        let c1 = self.b32(BinOp::ShrU, outs[2], n);
        let one = self.k32(1);
        let c1 = self.and32(c1, one);
        let ne = self.not1(empty);
        let c1 = self.and32(c1, ne);
        self.x87_raise(flags, c1);
        if pop {
            self.pop_st();
        }
        Flow::Next
    }

    /// `fcom`/`fucom`/`fcomi`/`fucomi`/`ficom`/`ftst`: st(0) against st(i),
    /// a memory operand, or (`ftst`) zero; the condition codes or (`eflags`)
    /// ZF/PF/CF, and `pops` pops. An empty register compares unordered with
    /// IE and SF.
    pub(crate) fn x87_compare_op(&mut self, src: Option<X87Mem>, i: u8, pops: u8, quiet: bool, eflags: bool, test: bool) -> Flow {
        let mut extra = self.k32(0);
        let st0 = self.ld_st(0);
        let mut empty = self.x87_st_empty(0);
        let b = if test {
            let z64 = self.k64(0);
            let z32 = self.k32(0);
            (z64, z32)
        } else if let Some(m) = src {
            let Some((v, f)) = self.x87_mem_value_q(self.cur_mem, m, false) else {
                return self.illegal_x87();
            };
            extra = f;
            v
        } else {
            let ei = self.x87_st_empty(i);
            empty = self.or32(empty, ei);
            self.ld_st(i)
        };
        let outs = self.b.x87(X87Op::Compare { quiet }, vec![st0.0, st0.1, b.0, b.1]);
        let unord = self.k32(1 << 8 | 1 << 10 | 1 << 14);
        let cc = self.s32(empty, unord, outs[0]);
        let m = self.k32(0x7f);
        let f = self.and32(outs[1], m);
        let sf = self.k32(IE | SF);
        let f = self.s32(empty, sf, f);
        // As for the arithmetic: a denormal memory operand reports DE only
        // when the compare is ordered.
        let ordered = {
            let u = self.k32(1 << 10);
            let c2 = self.and32(cc, u);
            let z = self.k32(0);
            self.c32(CmpOp::Eq, c2, z)
        };
        let flags = self.x87_mem_flags_if(f, extra, ordered);
        let z = self.k32(0);
        if eflags {
            // C3 -> ZF, C2 -> PF, C0 -> CF; OF, SF and AF cleared.
            let bit = |this: &mut Self, n: u32| {
                let k = this.k32(n);
                let v = this.b32(BinOp::ShrU, cc, k);
                let one = this.k32(1);
                this.and32(v, one)
            };
            let zf = bit(self, 14);
            let pf = bit(self, 10);
            let cf = bit(self, 8);
            self.b.write(GuestReg::Z, zf);
            self.b.write(GuestReg::P, pf);
            self.b.write(GuestReg::C, cf);
            self.b.write(GuestReg::N, z);
            self.b.write(GuestReg::V, z);
            self.b.write(GuestReg::A, z);
            self.flags = crate::FlagSource::Unknown;
        } else {
            let sw = self.b.read(GuestReg::X87Sw);
            let keep = self.k32(!(1u32 << 8 | 1 << 10 | 1 << 14));
            let sw = self.and32(sw, keep);
            let sw = self.or32(sw, cc);
            self.b.write(GuestReg::X87Sw, sw);
        }
        self.x87_raise(flags, z);
        for _ in 0..pops {
            self.pop_st();
        }
        Flow::Next
    }

    pub(crate) fn x87_fld_mem(&mut self, o: Operand, m: X87Mem) -> Flow {
        let Some((v, flags)) = self.x87_mem_value(o, m) else {
            return self.illegal_x87();
        };
        let (pflags, c1) = self.x87_push_checked(v);
        let flags = self.or32(flags, pflags);
        self.x87_raise(flags, c1);
        Flow::Next
    }

    /// `fld1`, `fldl2t`, `fldl2e`, `fldpi`, `fldlg2`, `fldln2`, `fldz`: the
    /// x87 holds each to 66 bits and rounds to 64 by the rounding mode.
    pub(crate) fn x87_fld_const(&mut self, which: u32) -> Flow {
        let (se, lo, near_up): (u32, u64, bool) = match which {
            0 => (0x3fff, 1 << 63, false),
            1 => (0x4000, 0xd49a_784b_cd1b_8afe, false),
            2 => (0x3fff, 0xb8aa_3b29_5c17_f0bb, true),
            3 => (0x4000, 0xc90f_daa2_2168_c234, true),
            4 => (0x3ffd, 0x9a20_9a84_fbcf_f798, true),
            5 => (0x3ffe, 0xb172_17f7_d1cf_79ab, true),
            _ => (0, 0, false),
        };
        let sig = if which == 0 || which == 6 {
            self.k64(lo)
        } else {
            // Up for RC=up, and for nearest when the dropped bits are over
            // half; down and toward zero truncate.
            let z = self.k32(0);
            let (is_near, is_up) = self.x87_mode(z);
            let up = if near_up { self.or32(is_up, is_near) } else { is_up };
            let up = self.b.extend(up, false);
            let base = self.k64(lo);
            self.b64(BinOp::Add, base, up)
        };
        let sev = self.k32(se);
        let (flags, c1) = self.x87_push_checked((sig, sev));
        self.x87_raise(flags, c1);
        Flow::Next
    }

    // -- stores ---------------------------------------------------------------

    /// A pure [`Stmt::X87`] conversion of `(sig, se)` under the current
    /// control word: (result `I64`, exception flags `I32`, C1 as an `I32`
    /// 0/1). The backend computes it -- inline for the common operands, the
    /// exact host algorithm otherwise.
    fn x87_op(&mut self, op: X87Op, sig: Value, se: Value) -> (Value, Value, Value) {
        let cw = self.b.read(GuestReg::X87Cw);
        let outs = self.b.x87(op, vec![sig, se, cw]);
        let (r, sw) = (outs[0], outs[1]);
        let m = self.k32(0x7f);
        let flags = self.and32(sw, m);
        let n = self.k32(C1_BIT);
        let c1 = self.b32(BinOp::ShrU, sw, n);
        let one = self.k32(1);
        let c1 = self.and32(c1, one);
        (r, flags, c1)
    }

    /// `fst`/`fstp`/`fist`/`fistp`/`fisttp` to memory, inline. An empty
    /// `st(0)` stores the indefinite with IE and SF.
    pub(crate) fn x87_fst_mem(&mut self, o: Operand, m: X87Mem, pop: bool, trunc: bool, int: bool) -> Flow {
        let Operand::Mem(mem) = o else {
            return self.illegal_x87();
        };
        let (addr, offset) = self.addr(&mem);
        let (sig, se) = self.ld_st(0);
        let empty = self.x87_st0_empty();
        let isig = self.k64(INDEF_SIG);
        let ise = self.k32(INDEF_SE);
        let sig = self.s64(empty, isig, sig);
        let se = self.s32(empty, ise, se);
        let st = |this: &mut Self, off: i64, val: Value, sz: MemSz| {
            this.b.stmt(Stmt::Store {
                addr,
                offset: off,
                val,
                sz,
                order: Order::Relaxed,
            });
        };
        let (flags, c1) = if int {
            let (bits, sz) = match m {
                X87Mem::I16 => (16, MemSz::B2),
                X87Mem::I32 => (32, MemSz::B4),
                _ => (64, MemSz::B8),
            };
            let (v, flags, c1) = self.x87_op(X87Op::ToInt { bits: bits as u8, trunc }, sig, se);
            let v = if sz == MemSz::B8 { v } else { self.b.wrap(v) };
            st(self, offset, v, sz);
            (flags, c1)
        } else {
            match m {
                X87Mem::F80 => {
                    st(self, offset, sig, MemSz::B8);
                    st(self, offset + 8, se, MemSz::B2);
                    let z = self.k32(0);
                    (z, z)
                }
                X87Mem::F32 => {
                    let (v, flags, c1) = self.x87_op(X87Op::Narrow { single: true }, sig, se);
                    let v = self.b.wrap(v);
                    st(self, offset, v, MemSz::B4);
                    (flags, c1)
                }
                _ => {
                    let (v, flags, c1) = self.x87_op(X87Op::Narrow { single: false }, sig, se);
                    st(self, offset, v, MemSz::B8);
                    (flags, c1)
                }
            }
        };
        // An empty st(0): IE and SF, C1 clear; the conversion's own flags
        // are those of the indefinite.
        let sfl = self.flag(empty, IE | SF);
        let flags = self.or32(flags, sfl);
        let ne = self.not1(empty);
        let c1 = self.and32(c1, ne);
        self.x87_raise(flags, c1);
        if pop {
            self.pop_st();
        }
        Flow::Next
    }
}
