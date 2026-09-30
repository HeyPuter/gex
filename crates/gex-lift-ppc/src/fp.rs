//! Floating point (docs/ppc.md §8): FPRs hold f64 bit patterns, single
//! precision lives in them in double format, loads and stores convert.

use crate::{sysreg, Flow, Lifter};
use gex_ir::*;
use gex_isa_ppc::{Inst, Op};

const SIGN: u64 = 1 << 63;
const EXP: u64 = 0x7ff0_0000_0000_0000;

impl<'a> Lifter<'a> {
    fn rdf(&mut self, n: u8) -> Value {
        self.b.read(GuestReg::F(n))
    }
    fn wrf(&mut self, n: u8, v: Value) {
        self.b.write(GuestReg::F(n), v);
    }
    fn bin64c(&mut self, op: BinOp, a: Value, c: u64) -> Value {
        let k = self.k64(c);
        self.bin64(op, a, k)
    }
    fn fbin(&mut self, op: FpBinOp, a: Value, b: Value) -> Value {
        self.b.def(Expr::FpBin {
            op,
            double: true,
            a,
            b,
        })
    }
    fn fcmp(&mut self, op: FpCmpOp, a: Value, b: Value) -> Value {
        self.b.def(Expr::FpCmp {
            op,
            double: true,
            a,
            b,
        })
    }
    /// Is this f64 bit pattern a NaN?
    fn is_nan(&mut self, a: Value) -> Value {
        self.fcmp(FpCmpOp::Ne, a, a)
    }
    /// Round a double to single precision and back: `frsp`, and the
    /// single-precision arithmetic's final step.
    fn round_single(&mut self, a: Value) -> Value {
        let s = self.b.def(Expr::FpCvt {
            kind: FpConvert::FpToFp,
            src_wide: true,
            dst_wide: false,
            a,
        });
        self.b.def(Expr::FpCvt {
            kind: FpConvert::FpToFp,
            src_wide: false,
            dst_wide: true,
            a: s,
        })
    }

    /// `lfs`'s conversion of a single-format word to double format. For
    /// everything but a NaN it is `f64.promote_f32`, which is exact; a NaN's
    /// payload is carried over bit for bit, a signalling one unquieted, as
    /// the architecture's bit-field expansion does and wasm's promote need
    /// not.
    fn single_to_double(&mut self, w: Value) -> Value {
        let promoted = self.b.def(Expr::FpCvt {
            kind: FpConvert::FpToFp,
            src_wide: false,
            dst_wide: true,
            a: w,
        });
        let w64 = self.b.extend(w, false);
        let sign = self.bin64c(BinOp::And, w64, 0x8000_0000);
        let sign = self.bin64c(BinOp::Shl, sign, 32);
        let frac = self.bin64c(BinOp::And, w64, 0x7f_ffff);
        let frac = self.bin64c(BinOp::Shl, frac, 29);
        let manual = self.bin64(BinOp::Or, sign, frac);
        let manual = self.bin64c(BinOp::Or, manual, EXP);
        let e = self.binc(BinOp::And, w, 0x7f80_0000);
        let special = self.cmpc(CmpOp::Eq, e, 0x7f80_0000);
        self.b.select(special, manual, promoted, Ty::I64)
    }

    /// `stfs`'s conversion of a double to a single-format word: the
    /// architecture's bit-field extraction, not a rounding (a value `stfs`
    /// stores is meant to be single already, and the hardware truncates the
    /// rest), with its denormalisation for the single denormal range.
    fn double_to_single(&mut self, x: Value) -> Value {
        let hi = self.bin64c(BinOp::ShrU, x, 32);
        let hi = self.bin64c(BinOp::And, hi, 0xc000_0000);
        let lo = self.bin64c(BinOp::ShrU, x, 29);
        let lo = self.bin64c(BinOp::And, lo, 0x3fff_ffff);
        let ext = self.bin64(BinOp::Or, hi, lo);
        let ext = self.b.wrap(ext);
        // Denormalisation: 874 <= exponent <= 896.
        let e = self.bin64c(BinOp::ShrU, x, 52);
        let e = self.bin64c(BinOp::And, e, 0x7ff);
        let e32 = self.b.wrap(e);
        let lo_ok = self.cmpc(CmpOp::GeU, e32, 874);
        let hi_ok = self.cmpc(CmpOp::LeU, e32, 896);
        let denorm = self.bin(BinOp::And, lo_ok, hi_ok);
        let mant = self.bin64c(BinOp::And, x, 0x000f_ffff_ffff_ffff);
        let mant = self.bin64c(BinOp::Or, mant, 1 << 52);
        // shift = 29 + (897 - e), in 30..=52 on this path.
        let k = self.k32(926);
        let sh = self.bin(BinOp::Sub, k, e32);
        let sh = self.binc(BinOp::And, sh, 63);
        let sh64 = self.b.extend(sh, false);
        let d = self.bin64(BinOp::ShrU, mant, sh64);
        let d = self.b.wrap(d);
        let sgn = self.bin64c(BinOp::ShrU, x, 32);
        let sgn = self.b.wrap(sgn);
        let sgn = self.binc(BinOp::And, sgn, 0x8000_0000);
        let d = self.bin(BinOp::Or, d, sgn);
        self.sel(denorm, d, ext)
    }

    pub(crate) fn lift_fp_mem(&mut self, i: &Inst) -> Flow {
        use Op::*;
        let (rt, ra, rb) = (i.rt(), i.ra(), i.rb());
        let indexed = matches!(
            i.op,
            Lfsx | Lfsux | Lfdx | Lfdux | Stfsx | Stfsux | Stfdx | Stfdux | Stfiwx
        );
        let update = matches!(
            i.op,
            Lfsu | Lfdu | Stfsu | Stfdu | Lfsux | Lfdux | Stfsux | Stfdux
        );
        if update && ra == 0 {
            return Flow::End(Term::Illegal {
                raw: self.raw,
                pc: self.pc,
            });
        }
        // The address, and whether it may carry a static offset.
        let (addr, off) = if indexed {
            let b = self.rd(rb);
            if ra == 0 {
                (b, 0)
            } else {
                let a = self.rd(ra);
                (self.bin(BinOp::Add, a, b), 0)
            }
        } else if update {
            let base = self.rd0(ra);
            (self.binc(BinOp::Add, base, i.simm() as u32), 0)
        } else {
            let base = self.rd0(ra);
            let d = i.simm();
            if d >= 0 {
                (base, d as i64)
            } else {
                (self.binc(BinOp::Add, base, d as u32), 0)
            }
        };
        match i.op {
            Lfs | Lfsu | Lfsx | Lfsux => {
                let w = self.load(addr, off, MemSz::B4, false, false);
                let v = self.single_to_double(w);
                self.wrf(rt, v);
            }
            Lfd | Lfdu | Lfdx | Lfdux => {
                let v = self.load64(addr, off);
                self.wrf(rt, v);
            }
            Stfs | Stfsu | Stfsx | Stfsux => {
                let x = self.rdf(rt);
                let w = self.double_to_single(x);
                self.store(addr, off, MemSz::B4, w, false);
            }
            Stfd | Stfdu | Stfdx | Stfdux => {
                let x = self.rdf(rt);
                self.store64(addr, off, x);
            }
            _ => {
                // stfiwx: the low word as it stands.
                let x = self.rdf(rt);
                let w = self.b.wrap(x);
                self.store(addr, off, MemSz::B4, w, false);
            }
        }
        if update {
            self.wr(ra, addr);
        }
        Flow::Next
    }

    /// CR1 for an FP record form: FPSCR's FX, FEX, VX and OX. gex does not
    /// compute those (docs/ppc.md §8), so this copies what FPSCR holds.
    fn record_fp(&mut self) {
        let f = self.b.read(GuestReg::Fpscr);
        for k in 0..4u8 {
            let s = self.binc(BinOp::ShrU, f, 31 - k as u32);
            let bit = self.binc(BinOp::And, s, 1);
            self.b.write(GuestReg::Cr(4 + k), bit);
        }
    }

    /// Single-precision fused multiply-add, `a*c + b` rounded once to
    /// single: exact product (single operands), TwoSum for the addition's
    /// error, the sum rounded to odd in double, then demoted. Round-to-odd
    /// with 53 >= 24 + 2 bits rounds correctly to single.
    fn fma_single(&mut self, a: Value, c: Value, b: Value) -> Value {
        let p = self.fbin(FpBinOp::Mul, a, c);
        let s = self.fbin(FpBinOp::Add, p, b);
        // TwoSum: e = (p - (s - bb)) + (b - bb), bb = s - p.
        let bb = self.fbin(FpBinOp::Sub, s, p);
        let t = self.fbin(FpBinOp::Sub, s, bb);
        let e1 = self.fbin(FpBinOp::Sub, p, t);
        let e2 = self.fbin(FpBinOp::Sub, b, bb);
        let e = self.fbin(FpBinOp::Add, e1, e2);
        let zero = self.k64(0);
        let inexact = self.fcmp(FpCmpOp::Ne, e, zero);
        let exp = self.bin64c(BinOp::And, s, EXP);
        let expk = self.k64(EXP);
        let finite = self.b.cmp(CmpOp::Ne, Ty::I64, exp, expk);
        let adjust = self.bin(BinOp::And, inexact, finite);
        // Truncate toward zero when the error points the other way, then
        // make it odd.
        let x = self.bin64(BinOp::Xor, s, e);
        let differ = self.bin64c(BinOp::ShrU, x, 63);
        let dec = self.bin64(BinOp::Sub, s, differ);
        let odd = self.bin64c(BinOp::Or, dec, 1);
        let r = self.b.select(adjust, odd, s, Ty::I64);
        self.round_single(r)
    }

    pub(crate) fn lift_fp(&mut self, i: &Inst) -> Flow {
        use Op::*;
        let (rt, ra, rb, rc) = (i.rt(), i.ra(), i.rb(), i.rc_field());
        let r = match i.op {
            Fadd | Fsub | Fmul | Fdiv | Fadds | Fsubs | Fmuls | Fdivs => {
                let a = self.rdf(ra);
                let b = if matches!(i.op, Fmul | Fmuls) {
                    self.rdf(rc)
                } else {
                    self.rdf(rb)
                };
                let op = match i.op {
                    Fadd | Fadds => FpBinOp::Add,
                    Fsub | Fsubs => FpBinOp::Sub,
                    Fmul | Fmuls => FpBinOp::Mul,
                    _ => FpBinOp::Div,
                };
                let v = self.fbin(op, a, b);
                // Single forms: the double result rounded to single, which
                // is exact for the single operands the ISA requires.
                if matches!(i.op, Fadds | Fsubs | Fmuls | Fdivs) {
                    self.round_single(v)
                } else {
                    v
                }
            }
            Fsqrt | Fsqrts => {
                let b = self.rdf(rb);
                let v = self.b.def(Expr::FpUn {
                    op: FpUnOp::Sqrt,
                    double: true,
                    a: b,
                });
                if i.op == Fsqrts {
                    self.round_single(v)
                } else {
                    v
                }
            }
            Fmadd | Fmsub | Fnmadd | Fnmsub => {
                let a = self.rdf(ra);
                let c = self.rdf(rc);
                let b = self.rdf(rb);
                let b = if matches!(i.op, Fmsub | Fnmsub) {
                    self.bin64c(BinOp::Xor, b, SIGN)
                } else {
                    b
                };
                let one = self.k32(1);
                let v = self.b.def(Expr::HostCall {
                    func: HostFunc::Fma,
                    args: vec![a, c, b, one],
                    ty: Ty::I64,
                });
                if matches!(i.op, Fnmadd | Fnmsub) {
                    self.negate_unless_nan(v)
                } else {
                    v
                }
            }
            Fmadds | Fmsubs | Fnmadds | Fnmsubs => {
                let a = self.rdf(ra);
                let c = self.rdf(rc);
                let b = self.rdf(rb);
                let b = if matches!(i.op, Fmsubs | Fnmsubs) {
                    self.bin64c(BinOp::Xor, b, SIGN)
                } else {
                    b
                };
                let v = self.fma_single(a, c, b);
                if matches!(i.op, Fnmadds | Fnmsubs) {
                    self.negate_unless_nan(v)
                } else {
                    v
                }
            }
            Fsel => {
                let a = self.rdf(ra);
                let c = self.rdf(rc);
                let b = self.rdf(rb);
                let zero = self.k64(0);
                let ge = self.fcmp(FpCmpOp::Ge, a, zero);
                self.b.select(ge, c, b, Ty::I64)
            }
            Fmr => self.rdf(rb),
            Fneg => {
                let b = self.rdf(rb);
                self.bin64c(BinOp::Xor, b, SIGN)
            }
            Fabs => {
                let b = self.rdf(rb);
                self.bin64c(BinOp::And, b, !SIGN)
            }
            Fnabs => {
                let b = self.rdf(rb);
                self.bin64c(BinOp::Or, b, SIGN)
            }
            Frsp => {
                let b = self.rdf(rb);
                self.round_single(b)
            }
            Fctiw | Fctiwz => {
                let b = self.rdf(rb);
                let x = if i.op == Fctiw {
                    // FPSCR[RN] is round-to-nearest-even (a non-default mode
                    // fails loudly when set, docs/ppc.md §8).
                    self.b.def(Expr::FpUn {
                        op: FpUnOp::RoundNearest,
                        double: true,
                        a: b,
                    })
                } else {
                    b
                };
                // Saturating, as the architecture's; a NaN converts to
                // 0x80000000, where wasm's saturating conversion gives 0.
                let w = self.b.def(Expr::FpCvt {
                    kind: FpConvert::FpToInt { signed: true },
                    src_wide: true,
                    dst_wide: false,
                    a: x,
                });
                // The high word is undefined; QEMU's (the oracle's) is the
                // result's sign, and zero for a NaN.
                let wide = self.b.extend(w, true);
                let nan = self.is_nan(b);
                let nanv = self.k64(0x8000_0000);
                self.b.select(nan, nanv, wide, Ty::I64)
            }
            Fcmpu | Fcmpo => {
                let a = self.rdf(ra);
                let b = self.rdf(rb);
                let lt = self.fcmp(FpCmpOp::Lt, a, b);
                let gt = self.fcmp(FpCmpOp::Gt, a, b);
                let eq = self.fcmp(FpCmpOp::Eq, a, b);
                let na = self.is_nan(a);
                let nb = self.is_nan(b);
                let un = self.bin(BinOp::Or, na, nb);
                let f = i.crfd();
                for (k, v) in [lt, gt, eq, un].into_iter().enumerate() {
                    self.b.write(GuestReg::Cr(4 * f + k as u8), v);
                }
                return Flow::Next;
            }
            Mffs => {
                if std::env::var_os("GEX_FP_STRICT").is_some() {
                    let pcv = self.k64(self.pc);
                    let id = self.k32(sysreg::MFFS_STRICT as u32);
                    let z = self.k64(0);
                    self.b.stmt(Stmt::HostCall {
                        func: HostFunc::SysRegWrite,
                        args: vec![pcv, id, z],
                    });
                }
                let f = self.b.read(GuestReg::Fpscr);
                self.b.extend(f, false)
            }
            Mtfsf | Mtfsfi | Mtfsb0 | Mtfsb1 => {
                let old = self.b.read(GuestReg::Fpscr);
                let (val, m) = match i.op {
                    Mtfsf => {
                        let b = self.rdf(rb);
                        let v = self.b.wrap(b);
                        let mut m = 0u32;
                        for f in 0..8 {
                            if i.flm() & (0x80 >> f) != 0 {
                                m |= 0xf000_0000 >> (4 * f);
                            }
                        }
                        (v, m)
                    }
                    Mtfsfi => {
                        let f = i.crfd() as u32;
                        let v = self.k32((i.mtfsfi_imm() as u32) << (28 - 4 * f));
                        (v, 0xf000_0000 >> (4 * f))
                    }
                    Mtfsb0 => (self.k32(0), 0x8000_0000 >> rt),
                    _ => (self.k32(u32::MAX), 0x8000_0000 >> rt),
                };
                // FEX and VX are summaries the hardware recomputes; gex keeps
                // what was written.
                let keep = self.binc(BinOp::And, old, !m);
                let set = self.binc(BinOp::And, val, m);
                let new = self.bin(BinOp::Or, keep, set);
                self.b.write(GuestReg::Fpscr, new);
                // A rounding mode other than nearest is refused by the host.
                let pcv = self.k64(self.pc);
                let id = self.k32(sysreg::FPSCR_RN as u32);
                let rn = self.binc(BinOp::And, new, 3);
                let nz = self.cmpc(CmpOp::Ne, rn, 0);
                // Only call out when the mode is non-default: the call's
                // argument carries the FPSCR, and a zero means "fine".
                let z = self.k32(0);
                let arg = self.sel(nz, new, z);
                let arg64 = self.b.extend(arg, false);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::SysRegWrite,
                    args: vec![pcv, id, arg64],
                });
                if i.rc() {
                    self.record_fp();
                }
                return Flow::Next;
            }
            Mcrfs | Fre | Fres | Frsqrte | Frsqrtes => {
                // The estimates are implementation-specific (docs/ppc.md §8),
                // and mcrfs clears exception bits gex does not track.
                return Flow::End(Term::Illegal {
                    raw: self.raw,
                    pc: self.pc,
                });
            }
            _ => unreachable!("{:?} is not an FP arithmetic instruction", i.op),
        };
        self.wrf(rt, r);
        if i.rc() {
            self.record_fp();
        }
        Flow::Next
    }

    /// The negated forms flip the sign of a numeric result but leave a NaN's
    /// alone.
    fn negate_unless_nan(&mut self, v: Value) -> Value {
        let n = self.bin64c(BinOp::Xor, v, SIGN);
        let nan = self.is_nan(v);
        self.b.select(nan, v, n, Ty::I64)
    }
}
