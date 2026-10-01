//! The x87's format conversions, lowered inline: `fld`/`fild` from memory
//! and the constants, `fst`/`fstp` to single, double and extended, and
//! `fist`/`fistp`/`fisttp`.
//!
//! These are most of what the x87 does in compiled code (every float or
//! double a 32-bit function returns goes through `st(0)`), so they must not
//! cost a host call. Each is exact: the same results, exception flags and
//! C1 as `gex_f80` (whose tests check it against the hardware),
//! computed with integer operations on the register's significand and
//! sign/exponent word. Stack overflow and underflow are handled as the host
//! handles them; an exception the control word leaves unmasked fails loudly
//! through `Stmt::Trap` (gex does not deliver x87 exceptions).

use crate::{Flow, Lifter};
use gex_ir::*;
use gex_isa_x86::{Operand, X87Mem};

/// Status-word exception bits.
const IE: u32 = 1;
const DE: u32 = 2;
const OE: u32 = 8;
const UE: u32 = 16;
const PE: u32 = 32;
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

    /// Whether to round a magnitude up: to nearest when the dropped bits are
    /// over half an ulp, or exactly half with the kept part odd; away from
    /// zero whenever any dropped bit is set.
    fn x87_round_up(&mut self, (near, away): (Value, Value), inexact: Value, gt: Value, eq: Value, odd: Value) -> Value {
        let tie_odd = self.and32(eq, odd);
        let n = self.or32(gt, tie_odd);
        let n = self.and32(near, n);
        let a = self.and32(away, inexact);
        self.or32(n, a)
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
        let top2 = self.k64(3 << 62);
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
    pub(crate) fn x87_fld_mem(&mut self, o: Operand, m: X87Mem) -> Flow {
        let Operand::Mem(mem) = o else {
            return self.illegal_x87();
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
        let (v, flags) = match m {
            X87Mem::F32 => {
                let raw = ld(self, offset, MemSz::B4, false);
                self.x87_widen(raw, SINGLE)
            }
            X87Mem::F64 => {
                let raw = ld(self, offset, MemSz::B8, false);
                self.x87_widen(raw, DOUBLE)
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

    /// The fields of an extended value for the stores: (sign, unbiased
    /// exponent as an I64 -- a denormal's taken as the minimum, -16382 --,
    /// whether it is a finite number the formats can represent (J set, or a
    /// denormal), a NaN, an SNaN, infinity, zero, unsupported).
    fn x87_classify(&mut self, sig: Value, se: Value) -> Class80 {
        let m = self.k32(0x7fff);
        let e15 = self.and32(se, m);
        let fifteen = self.k32(15);
        let sign = self.b32(BinOp::ShrU, se, fifteen);
        let one = self.k32(1);
        let sign = self.and32(sign, one);
        let z32 = self.k32(0);
        let z64 = self.k64(0);
        let is_max = self.c32(CmpOp::Eq, e15, m);
        let is_e0 = self.c32(CmpOp::Eq, e15, z32);
        let sixty3 = self.k64(63);
        let j = self.b64(BinOp::ShrU, sig, sixty3);
        let j = self.b.wrap(j);
        let nj = self.not1(j);
        let sig0 = self.c64(CmpOp::Eq, sig, z64);
        let zero = self.and32(is_e0, sig0);
        let shl1 = self.k64(1);
        let low = self.b64(BinOp::Shl, sig, shl1);
        let low0 = self.c64(CmpOp::Eq, low, z64);
        let inf = self.and32(is_max, j);
        let inf = self.and32(inf, low0);
        let nan = self.and32(is_max, j);
        let lown = self.not1(low0);
        let nan = self.and32(nan, lown);
        let qb = self.k64(1 << 62);
        let qbit = self.b64(BinOp::And, sig, qb);
        let qclear = self.c64(CmpOp::Eq, qbit, z64);
        let snan = self.and32(nan, qclear);
        // Unsupported: J clear with a nonzero exponent (unnormals,
        // pseudo-NaNs and pseudo-infinities).
        let ne0 = self.not1(is_e0);
        let unsupported = self.and32(ne0, nj);
        // Unbiased exponent, a denormal's as -16382.
        let e15_64 = self.b.extend(e15, false);
        let bias = self.k64(16383);
        let e = self.b64(BinOp::Sub, e15_64, bias);
        let dmin = self.k64((-16382i64) as u64);
        let e = self.s64(is_e0, dmin, e);
        let special = self.or32(is_max, unsupported);
        let special = self.or32(special, zero);
        let finite = self.not1(special);
        Class80 {
            sign,
            e,
            finite,
            nan,
            snan,
            inf,
            zero,
            unsupported,
        }
    }

    /// Round an extended value to single or double: (raw bits in an I64,
    /// flags, C1), as `gex_f80::store` does.
    fn x87_narrow(&mut self, sig: Value, se: Value, f: Fmt) -> (Value, Value, Value) {
        let c = self.x87_classify(sig, se);
        let mode = self.x87_mode(c.sign);
        let exp_bits = f.bits - 1 - f.frac;
        let max_field = (1i64 << exp_bits) - 1; // all ones
        let emin = 1 - f.bias;
        let z64 = self.k64(0);
        let z32 = self.k32(0);
        let one64 = self.k64(1);

        // Bits dropped: the extra significand bits, and for a result below
        // the normal range as many more as it falls short.
        let base_drop = (63 - f.frac) as i64;
        let eminv = self.k64(emin as u64);
        let short = self.b64(BinOp::Sub, eminv, c.e); // > 0: denormal
        let is_den = self.c64(CmpOp::GtS, short, z64);
        let extra = self.s64(is_den, short, z64);
        let bd = self.k64(base_drop as u64);
        let drop = self.b64(BinOp::Add, bd, extra); // >= base_drop
        let sixty4 = self.k64(64);
        let beyond = self.c64(CmpOp::GtS, drop, sixty4);
        let ge64 = self.c64(CmpOp::GeS, drop, sixty4);
        let s = self.s64(ge64, sixty4, drop); // 1..=64
        // kept = sig >> s (0 at 64); mask = (1 << s) - 1 (all ones at 64).
        let kept = self.b64(BinOp::ShrU, sig, s);
        let kept = self.s64(ge64, z64, kept);
        let pw = self.b64(BinOp::Shl, one64, s);
        let mask = self.b64(BinOp::Sub, pw, one64);
        let all = self.k64(u64::MAX);
        let mask = self.s64(ge64, all, mask);
        let rem = self.b64(BinOp::And, sig, mask);
        let sm1 = self.b64(BinOp::Sub, s, one64);
        let half = self.b64(BinOp::Shl, one64, sm1);
        let inexact = self.c64(CmpOp::Ne, rem, z64);
        let gt = self.c64(CmpOp::GtU, rem, half);
        let eq = self.c64(CmpOp::Eq, rem, half);
        let nb = self.not1(beyond);
        let gt = self.and32(gt, nb);
        let eq = self.and32(eq, nb);
        let odd = self.b64(BinOp::And, kept, one64);
        let odd = self.b.wrap(odd);
        let up = self.x87_round_up(mode, inexact, gt, eq, odd);
        let up64 = self.b.extend(up, false);
        let kept2 = self.b64(BinOp::Add, kept, up64);

        // Normal result: ((e + bias - 1) << frac) + kept2 carries a rounding
        // overflow into the exponent by itself.
        let bm1 = self.k64((f.bias - 1) as u64);
        let ef = self.b64(BinOp::Add, c.e, bm1);
        let fr = self.k64(f.frac as u64);
        let efs = self.b64(BinOp::Shl, ef, fr);
        let normal_bits = self.b64(BinOp::Add, efs, kept2);
        let mag = self.s64(is_den, kept2, normal_bits);
        // Overflow: the exponent field reaches all ones.
        let carry = self.b64(BinOp::ShrU, kept2, fr); // 1, or 2 on a carry
        let field = self.b64(BinOp::Add, ef, carry);
        let mf = self.k64(max_field as u64);
        let ovf = self.c64(CmpOp::GeS, field, mf);
        let nden = self.not1(is_den);
        let ovf = self.and32(ovf, nden);
        let ovf = self.and32(ovf, c.finite);
        // To infinity, or to the largest finite number, by the mode.
        let to_inf = self.or32(mode.0, mode.1);
        let one32 = self.k32(1);
        let inf_bits = self.k64((max_field as u64) << f.frac);
        let max_bits = self.k64(((max_field as u64) << f.frac) - 1);
        let ovf_bits = self.s64(to_inf, inf_bits, max_bits);
        let mag = self.s64(ovf, ovf_bits, mag);

        // Tininess after rounding: tiny unless the value, rounded to the
        // format's precision with an unbounded exponent, reaches the
        // smallest normal -- which only a value just below it can.
        let emin_m1 = self.k64((emin - 1) as u64);
        let is_below = self.c64(CmpOp::LtS, c.e, eminv);
        let just_below = self.c64(CmpOp::Eq, c.e, emin_m1);
        let bdk = self.k64(base_drop as u64);
        let k1 = self.b64(BinOp::ShrU, sig, bdk);
        let ones = self.k64((1u64 << (f.frac + 1)) - 1);
        let all_ones = self.c64(CmpOp::Eq, k1, ones);
        let m1 = self.k64((1u64 << base_drop) - 1);
        let r1 = self.b64(BinOp::And, sig, m1);
        let h1 = self.k64(1u64 << (base_drop - 1));
        let in1 = self.c64(CmpOp::Ne, r1, z64);
        let gt1 = self.c64(CmpOp::GtU, r1, h1);
        let eq1 = self.c64(CmpOp::Eq, r1, h1);
        let up1 = self.x87_round_up(mode, in1, gt1, eq1, one32);
        let carries = self.and32(all_ones, up1);
        let carries = self.and32(carries, just_below);
        let nc = self.not1(carries);
        let tiny = self.and32(is_below, nc);

        // Flags for a finite value.
        let pe = self.or32(inexact, ovf);
        let pe_f = self.flag(pe, PE);
        let ue = self.and32(tiny, inexact);
        let ue_f = self.flag(ue, UE);
        let oe_f = self.flag(ovf, OE);
        let ff = self.or32(pe_f, ue_f);
        let ff = self.or32(ff, oe_f);
        let c1f = self.s32(ovf, to_inf, up);

        // The special values.
        let smax = self.k64((max_field as u64) << f.frac);
        let qn = self.k64(1 << 62);
        let qsig = self.b64(BinOp::Or, sig, qn);
        let qsig = self.b64(BinOp::Shl, qsig, one64);
        let tail = self.k64((64 - f.frac) as u64);
        let nfrac = self.b64(BinOp::ShrU, qsig, tail);
        let nan_bits = self.b64(BinOp::Or, smax, nfrac);
        let indef = self.k64(((max_field as u64) << f.frac) | 1 << (f.frac - 1));
        let mag = self.s64(c.zero, z64, mag);
        let mag = self.s64(c.inf, smax, mag);
        let mag = self.s64(c.nan, nan_bits, mag);
        let mag = self.s64(c.unsupported, indef, mag);
        let sign64 = self.b.extend(c.sign, false);
        let sgn = self.k64((f.bits - 1) as u64);
        let sbit = self.b64(BinOp::Shl, sign64, sgn);
        let bits = self.b64(BinOp::Or, mag, sbit);
        let ind_sign = self.k64(1 << (f.bits - 1));
        let ib = self.b64(BinOp::Or, indef, ind_sign);
        let bits = self.s64(c.unsupported, ib, bits);

        let flags = self.s32(c.finite, ff, z32);
        let bad = self.or32(c.snan, c.unsupported);
        let ie = self.flag(bad, IE);
        let flags = self.or32(flags, ie);
        let c1 = self.s32(c.finite, c1f, z32);
        (bits, flags, c1)
    }

    /// Round an extended value to an integer of `bits` bits: (the integer in
    /// an I64, flags, C1). Out of range, a NaN, an infinity or an
    /// unsupported value give the integer indefinite with IE.
    fn x87_to_int(&mut self, sig: Value, se: Value, bits: u32, trunc: bool) -> (Value, Value, Value) {
        let c = self.x87_classify(sig, se);
        let z64 = self.k64(0);
        let z32 = self.k32(0);
        let one64 = self.k64(1);
        // Bits below the binary point: 63 - e, at least 1 here.
        let sixty3 = self.k64(63);
        let drop = self.b64(BinOp::Sub, sixty3, c.e);
        let whole = self.c64(CmpOp::LeS, drop, z64); // e >= 63
        let sixty4 = self.k64(64);
        let beyond = self.c64(CmpOp::GtS, drop, sixty4);
        let ge64 = self.c64(CmpOp::GeS, drop, sixty4);
        let d1 = self.s64(whole, one64, drop); // keep the shifts in range
        let s = self.s64(ge64, sixty4, d1);
        let kept = self.b64(BinOp::ShrU, sig, s);
        let kept = self.s64(ge64, z64, kept);
        let pw = self.b64(BinOp::Shl, one64, s);
        let mask = self.b64(BinOp::Sub, pw, one64);
        let all = self.k64(u64::MAX);
        let mask = self.s64(ge64, all, mask);
        let rem = self.b64(BinOp::And, sig, mask);
        let sm1 = self.b64(BinOp::Sub, s, one64);
        let half = self.b64(BinOp::Shl, one64, sm1);
        let inexact = self.c64(CmpOp::Ne, rem, z64);
        let up = if trunc {
            z32
        } else {
            let gt = self.c64(CmpOp::GtU, rem, half);
            let eq = self.c64(CmpOp::Eq, rem, half);
            let nb = self.not1(beyond);
            let gt = self.and32(gt, nb);
            let eq = self.and32(eq, nb);
            let odd = self.b64(BinOp::And, kept, one64);
            let odd = self.b.wrap(odd);
            let mode = self.x87_mode(c.sign);
            self.x87_round_up(mode, inexact, gt, eq, odd)
        };
        let up64 = self.b.extend(up, false);
        let k = self.b64(BinOp::Add, kept, up64);
        // e >= 63: the significand is the integer (e >= 64 is out of range).
        let k = self.s64(whole, sig, k);
        let inexact = self.s32(whole, z32, inexact);
        let up = self.s32(whole, z32, up);
        let e64 = self.c64(CmpOp::GeS, c.e, sixty4);
        // In range: k <= 2^(bits-1) - 1, or 2^(bits-1) when negative. k is
        // unsigned here (up to 2^64 - 1).
        let lim = self.k64((1u64 << (bits - 1)) - 1);
        let sign64 = self.b.extend(c.sign, false);
        let lim = self.b64(BinOp::Add, lim, sign64);
        let over = self.c64(CmpOp::GtU, k, lim);
        let over = self.or32(over, e64);
        let neg = self.b64(BinOp::Sub, z64, k);
        let signed = self.s64(c.sign, neg, k);
        let zero_or_fin = self.or32(c.finite, c.zero);
        let signed = self.s64(c.zero, z64, signed);
        let ok = self.not1(over);
        let ok = self.and32(zero_or_fin, ok);
        let fin_ok = self.and32(ok, c.finite);
        let indef = self.k64(1u64 << (bits - 1));
        let v = self.s64(ok, signed, indef);
        let pe = self.and32(fin_ok, inexact);
        let pe_f = self.flag(pe, PE);
        let bad = self.not1(ok);
        let ie_f = self.flag(bad, IE);
        let flags = self.or32(pe_f, ie_f);
        let c1 = self.and32(fin_ok, up);
        (v, flags, c1)
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
            let (v, flags, c1) = self.x87_to_int(sig, se, bits, trunc);
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
                    let (v, flags, c1) = self.x87_narrow(sig, se, SINGLE);
                    let v = self.b.wrap(v);
                    st(self, offset, v, MemSz::B4);
                    (flags, c1)
                }
                _ => {
                    let (v, flags, c1) = self.x87_narrow(sig, se, DOUBLE);
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

/// What `x87_classify` finds (I32 0/1 flags, the exponent an I64).
struct Class80 {
    sign: Value,
    e: Value,
    finite: Value,
    nan: Value,
    snan: Value,
    inf: Value,
    zero: Value,
    unsupported: Value,
}
