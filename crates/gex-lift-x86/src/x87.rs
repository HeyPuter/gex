//! x87 floating point, computed in double precision.
//!
//! The eight registers live in the context (`ctx::X87`), addressed through
//! `GuestReg::CtxAddr` because `TOP` is dynamic: `st(i)` is physical register
//! `(top + i) & 7`. Values are IEEE doubles, not the architectural 80-bit
//! format; a guest sees `long double` with 53 bits of mantissa. Loads and
//! stores of the 80-bit memory format convert at the boundary.
//!
//! Transcendental and remainder instructions (`fsin`, `fyl2x`, `fprem`, …)
//! have no wasm form; the host computes them (see [`crate::x87op`]).

use crate::{Flow, Lifter, FlagSource};
use gex_abi::ctx;
use gex_ir::*;
use gex_isa_x86::{Inst, Op, Operand, Width, X87Mem, X87Src};

/// Status-word condition-code bits.
const C0: u32 = 1 << 8;
const C1: u32 = 1 << 9;
const C2: u32 = 1 << 10;
const C3: u32 = 1 << 14;

impl<'a> Lifter<'a> {
    fn top(&mut self) -> Value {
        self.b.read(GuestReg::X87Top)
    }

    fn set_top(&mut self, v: Value) {
        self.b.write(GuestReg::X87Top, v);
    }

    /// Guest address of physical register `(top + i) & 7`.
    fn st_addr(&mut self, top: Value, i: u8) -> Value {
        let base = self.b.read(GuestReg::CtxAddr);
        let iv = self.k32(i as u32);
        let idx = self.b.bin(BinOp::Add, Ty::I32, top, iv);
        let seven = self.k32(7);
        let idx = self.b.bin(BinOp::And, Ty::I32, idx, seven);
        let three = self.k32(3);
        let off = self.b.bin(BinOp::Shl, Ty::I32, idx, three);
        let off = self.b.extend(off, false);
        let k = self.k64(ctx::X87 as u64);
        let a = self.b.bin(BinOp::Add, Ty::I64, base, k);
        self.b.bin(BinOp::Add, Ty::I64, a, off)
    }

    /// `st(i)` as f64 bits.
    fn ld_st(&mut self, i: u8) -> Value {
        let top = self.top();
        let a = self.st_addr(top, i);
        self.b.def(Expr::Load {
            addr: a,
            offset: 0,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        })
    }

    fn st_st(&mut self, i: u8, v: Value) {
        let top = self.top();
        let a = self.st_addr(top, i);
        self.b.stmt(Stmt::Store {
            addr: a,
            offset: 0,
            val: v,
            sz: MemSz::B8,
            order: Order::Relaxed,
        });
    }

    /// Push `v` (f64 bits) onto the stack.
    fn push_st(&mut self, v: Value) {
        let top = self.top();
        let seven = self.k32(7);
        let ntop = self.b.bin(BinOp::Add, Ty::I32, top, seven);
        let ntop = self.b.bin(BinOp::And, Ty::I32, ntop, seven);
        self.set_top(ntop);
        let a = self.st_addr(ntop, 0);
        self.b.stmt(Stmt::Store {
            addr: a,
            offset: 0,
            val: v,
            sz: MemSz::B8,
            order: Order::Relaxed,
        });
        // Mark valid.
        let tags = self.b.read(GuestReg::X87Tags);
        let one = self.k32(1);
        let bit = self.b.bin(BinOp::Shl, Ty::I32, one, ntop);
        let tags = self.b.bin(BinOp::Or, Ty::I32, tags, bit);
        self.b.write(GuestReg::X87Tags, tags);
    }

    fn pop_st(&mut self) {
        let top = self.top();
        let tags = self.b.read(GuestReg::X87Tags);
        let one = self.k32(1);
        let bit = self.b.bin(BinOp::Shl, Ty::I32, one, top);
        let nbit = self.b.un(UnOp::Not, Ty::I32, bit);
        let tags = self.b.bin(BinOp::And, Ty::I32, tags, nbit);
        self.b.write(GuestReg::X87Tags, tags);
        let seven = self.k32(7);
        let ntop = self.b.bin(BinOp::Add, Ty::I32, top, one);
        let ntop = self.b.bin(BinOp::And, Ty::I32, ntop, seven);
        self.set_top(ntop);
    }

    fn fbin64(&mut self, op: FpBinOp, a: Value, b: Value) -> Value {
        self.b.def(Expr::FpBin {
            op,
            double: true,
            a,
            b,
        })
    }

    fn fun64(&mut self, op: FpUnOp, a: Value) -> Value {
        self.b.def(Expr::FpUn {
            op,
            double: true,
            a,
        })
    }

    /// Load an x87 memory operand as f64 bits.
    fn x87_load(&mut self, o: Operand, m: X87Mem) -> Value {
        let Operand::Mem(mem) = o else {
            return self.k64(0);
        };
        let (addr, offset) = self.addr(&mem);
        let ld = |l: &mut Self, sz: MemSz, ty: Ty, off: i64| {
            l.b.def(Expr::Load {
                addr,
                offset: off,
                sz,
                signed: false,
                ty,
                order: Order::Relaxed,
            })
        };
        match m {
            X87Mem::F64 => ld(self, MemSz::B8, Ty::I64, offset),
            X87Mem::F32 => {
                let v = ld(self, MemSz::B4, Ty::I32, offset);
                self.b.def(Expr::FpCvt {
                    kind: FpConvert::FpToFp,
                    src_wide: false,
                    dst_wide: true,
                    a: v,
                })
            }
            X87Mem::I16 | X87Mem::I32 => {
                let v = ld(self, if m == X87Mem::I16 { MemSz::B2 } else { MemSz::B4 }, Ty::I32, offset);
                let v = if m == X87Mem::I16 {
                    self.b.def(Expr::ExtendBits {
                        a: v,
                        ty: Ty::I32,
                        bits: 16,
                        signed: true,
                    })
                } else {
                    v
                };
                self.b.def(Expr::FpCvt {
                    kind: FpConvert::IntToFp { signed: true },
                    src_wide: false,
                    dst_wide: true,
                    a: v,
                })
            }
            X87Mem::I64 => {
                let v = ld(self, MemSz::B8, Ty::I64, offset);
                self.b.def(Expr::FpCvt {
                    kind: FpConvert::IntToFp { signed: true },
                    src_wide: true,
                    dst_wide: true,
                    a: v,
                })
            }
            X87Mem::F80 => {
                let mant = ld(self, MemSz::B8, Ty::I64, offset);
                let se = ld(self, MemSz::B2, Ty::I32, offset + 8);
                self.f80_to_f64(mant, se)
            }
        }
    }

    /// Store f64 bits to an x87 memory operand.
    fn x87_store(&mut self, o: Operand, m: X87Mem, v: Value) {
        let Operand::Mem(mem) = o else {
            return;
        };
        let (addr, offset) = self.addr(&mem);
        let st = |l: &mut Self, sz: MemSz, val: Value, off: i64| {
            l.b.stmt(Stmt::Store {
                addr,
                offset: off,
                val,
                sz,
                order: Order::Relaxed,
            })
        };
        match m {
            X87Mem::F64 => st(self, MemSz::B8, v, offset),
            X87Mem::F32 => {
                let s = self.b.def(Expr::FpCvt {
                    kind: FpConvert::FpToFp,
                    src_wide: true,
                    dst_wide: false,
                    a: v,
                });
                st(self, MemSz::B4, s, offset);
            }
            X87Mem::F80 => {
                let (mant, se) = self.f64_to_f80(v);
                st(self, MemSz::B8, mant, offset);
                st(self, MemSz::B2, se, offset + 8);
            }
            // Integer stores go through `fist`, not here.
            X87Mem::I16 | X87Mem::I32 | X87Mem::I64 => {}
        }
    }

    /// 80-bit extended (mantissa with explicit integer bit, sign+exponent
    /// word) to an f64 bit pattern, rounding to nearest.
    fn f80_to_f64(&mut self, mant: Value, se: Value) -> Value {
        let sign = {
            let fifteen = self.k32(15);
            let s = self.b.bin(BinOp::ShrU, Ty::I32, se, fifteen);
            let one = self.k32(1);
            self.b.bin(BinOp::And, Ty::I32, s, one)
        };
        let exp = {
            let m = self.k32(0x7fff);
            self.b.bin(BinOp::And, Ty::I32, se, m)
        };
        let sign64 = self.b.extend(sign, false);
        let sh63 = self.k64(63);
        let signbit = self.b.bin(BinOp::Shl, Ty::I64, sign64, sh63);

        // Normal: e = exp - 16383 + 1023; m53 = (mant >> 11) + round bit.
        let eleven = self.k64(11);
        let m53 = self.b.bin(BinOp::ShrU, Ty::I64, mant, eleven);
        let ten = self.k64(10);
        let rb = self.b.bin(BinOp::ShrU, Ty::I64, mant, ten);
        let one64 = self.k64(1);
        let rb = self.b.bin(BinOp::And, Ty::I64, rb, one64);
        let m53 = self.b.bin(BinOp::Add, Ty::I64, m53, rb);
        let bias = self.k32(16383 - 1023 + 1); // e - 1 = exp - 15361
        let em1 = self.b.bin(BinOp::Sub, Ty::I32, exp, bias);
        let em1_64 = self.b.extend(em1, true);
        let sh52 = self.k64(52);
        let ebits = self.b.bin(BinOp::Shl, Ty::I64, em1_64, sh52);
        let normal = self.b.bin(BinOp::Add, Ty::I64, ebits, m53);

        // Special cases.
        let zero32 = self.k32(0);
        let exp_zero = self.b.cmp(CmpOp::Eq, Ty::I32, exp, zero32);
        let maxe = self.k32(0x7fff);
        let exp_max = self.b.cmp(CmpOp::Eq, Ty::I32, exp, maxe);
        // Denormals and zero: zero.
        let zero64 = self.k64(0);
        // Inf/NaN: exponent all ones, mantissa from the high bits sans the
        // integer bit.
        let one = self.k64(1);
        let m = self.b.bin(BinOp::Shl, Ty::I64, mant, one);
        let twelve = self.k64(12);
        let m = self.b.bin(BinOp::ShrU, Ty::I64, m, twelve);
        let infexp = self.k64(0x7ffu64 << 52);
        let special = self.b.bin(BinOp::Or, Ty::I64, infexp, m);
        // Overflow of the exponent into inf: e >= 0x7ff.
        let lim = self.k32(0x7fe + 16383 - 1023 + 1);
        let too_big = self.b.cmp(CmpOp::GeS, Ty::I32, exp, lim);
        let inf = self.k64(0x7ffu64 << 52);
        let v = self.b.select(too_big, inf, normal, Ty::I64);
        // Underflow: e <= 0 -> zero (denormal results are flushed).
        let low = self.k32(16383 - 1023);
        let too_small = self.b.cmp(CmpOp::LeS, Ty::I32, exp, low);
        let v = self.b.select(too_small, zero64, v, Ty::I64);
        let v = self.b.select(exp_max, special, v, Ty::I64);
        let v = self.b.select(exp_zero, zero64, v, Ty::I64);
        self.b.bin(BinOp::Or, Ty::I64, v, signbit)
    }

    /// f64 bits to the 80-bit format: `(mantissa, sign|exponent)`.
    fn f64_to_f80(&mut self, v: Value) -> (Value, Value) {
        let sh63 = self.k64(63);
        let sign = self.b.bin(BinOp::ShrU, Ty::I64, v, sh63);
        let sign = self.b.wrap(sign);
        let fifteen = self.k32(15);
        let sign15 = self.b.bin(BinOp::Shl, Ty::I32, sign, fifteen);
        let sh52 = self.k64(52);
        let exp = self.b.bin(BinOp::ShrU, Ty::I64, v, sh52);
        let exp = self.b.wrap(exp);
        let m7ff = self.k32(0x7ff);
        let exp = self.b.bin(BinOp::And, Ty::I32, exp, m7ff);
        let fmask = self.k64((1u64 << 52) - 1);
        let frac = self.b.bin(BinOp::And, Ty::I64, v, fmask);

        // Normal: mant = 1<<63 | frac << 11; e80 = exp - 1023 + 16383.
        let eleven = self.k64(11);
        let fr = self.b.bin(BinOp::Shl, Ty::I64, frac, eleven);
        let ibit = self.k64(1u64 << 63);
        let mant_n = self.b.bin(BinOp::Or, Ty::I64, fr, ibit);
        let bias = self.k32(16383 - 1023);
        let e_n = self.b.bin(BinOp::Add, Ty::I32, exp, bias);

        // Denormal: mant = frac << clz(frac); e80 = 63 - clz - 1074 + 16383.
        let lz = self.b.un(UnOp::Clz, Ty::I64, frac);
        let mant_d = self.b.bin(BinOp::Shl, Ty::I64, frac, lz);
        let lz32 = self.b.wrap(lz);
        let k = self.k32((63i32 - 1074 + 16383) as u32);
        let e_d = self.b.bin(BinOp::Sub, Ty::I32, k, lz32);

        // Inf/NaN.
        let e_s = self.k32(0x7fff);
        let zero32 = self.k32(0);
        let zero64 = self.k64(0);
        let exp_zero = self.b.cmp(CmpOp::Eq, Ty::I32, exp, zero32);
        let frac_zero = self.b.cmp(CmpOp::Eq, Ty::I64, frac, zero64);
        let is_zero = self.b.bin(BinOp::And, Ty::I32, exp_zero, frac_zero);
        let exp_max = self.b.cmp(CmpOp::Eq, Ty::I32, exp, m7ff);

        let mant = self.b.select(exp_zero, mant_d, mant_n, Ty::I64);
        let mant = self.b.select(is_zero, zero64, mant, Ty::I64);
        let e = self.b.select(exp_zero, e_d, e_n, Ty::I32);
        let e = self.b.select(is_zero, zero32, e, Ty::I32);
        let e = self.b.select(exp_max, e_s, e, Ty::I32);
        let se = self.b.bin(BinOp::Or, Ty::I32, e, sign15);
        (mant, se)
    }

    /// Set C0/C2/C3 in the status word from a comparison of `a` with `b`.
    fn x87_compare_sw(&mut self, a: Value, b: Value) {
        let lt = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Lt,
            double: true,
            a,
            b,
        });
        let eq = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Eq,
            double: true,
            a,
            b,
        });
        let na = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Ne,
            double: true,
            a,
            b: a,
        });
        let nb = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Ne,
            double: true,
            a: b,
            b,
        });
        let unord = self.b.bin(BinOp::Or, Ty::I32, na, nb);
        let sw = self.b.read(GuestReg::X87Sw);
        let clear = self.k32(!(C0 | C1 | C2 | C3));
        let sw = self.b.bin(BinOp::And, Ty::I32, sw, clear);
        // C0 = lt | unord; C2 = unord; C3 = eq | unord
        let c0 = self.b.bin(BinOp::Or, Ty::I32, lt, unord);
        let c3 = self.b.bin(BinOp::Or, Ty::I32, eq, unord);
        let k8 = self.k32(8);
        let c0 = self.b.bin(BinOp::Shl, Ty::I32, c0, k8);
        let k10 = self.k32(10);
        let c2 = self.b.bin(BinOp::Shl, Ty::I32, unord, k10);
        let k14 = self.k32(14);
        let c3 = self.b.bin(BinOp::Shl, Ty::I32, c3, k14);
        let sw = self.b.bin(BinOp::Or, Ty::I32, sw, c0);
        let sw = self.b.bin(BinOp::Or, Ty::I32, sw, c2);
        let sw = self.b.bin(BinOp::Or, Ty::I32, sw, c3);
        self.b.write(GuestReg::X87Sw, sw);
    }

    /// Set ZF/PF/CF from a comparison (`fcomi`).
    fn x87_compare_flags(&mut self, a: Value, b: Value) {
        let lt = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Lt,
            double: true,
            a,
            b,
        });
        let eq = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Eq,
            double: true,
            a,
            b,
        });
        let na = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Ne,
            double: true,
            a,
            b: a,
        });
        let nb = self.b.def(Expr::FpCmp {
            op: FpCmpOp::Ne,
            double: true,
            a: b,
            b,
        });
        let unord = self.b.bin(BinOp::Or, Ty::I32, na, nb);
        let z = self.b.bin(BinOp::Or, Ty::I32, eq, unord);
        let c = self.b.bin(BinOp::Or, Ty::I32, lt, unord);
        let zero = self.k32(0);
        self.b.write(GuestReg::Z, z);
        self.b.write(GuestReg::C, c);
        self.b.write(GuestReg::P, unord);
        self.b.write(GuestReg::N, zero);
        self.b.write(GuestReg::V, zero);
        self.b.write(GuestReg::A, zero);
        self.flags = FlagSource::Unknown;
    }

    /// Round per the control word's RC field (bits 10-11).
    fn round_by_cw(&mut self, v: Value) -> Value {
        let cw = self.b.read(GuestReg::X87Cw);
        let ten = self.k32(10);
        let rc = self.b.bin(BinOp::ShrU, Ty::I32, cw, ten);
        let three = self.k32(3);
        let rc = self.b.bin(BinOp::And, Ty::I32, rc, three);
        let n = self.fun64(FpUnOp::RoundNearest, v);
        let d = self.fun64(FpUnOp::Floor, v);
        let u = self.fun64(FpUnOp::Ceil, v);
        let t = self.fun64(FpUnOp::Trunc, v);
        let zero = self.k32(0);
        let one = self.k32(1);
        let two = self.k32(2);
        let is0 = self.b.cmp(CmpOp::Eq, Ty::I32, rc, zero);
        let is1 = self.b.cmp(CmpOp::Eq, Ty::I32, rc, one);
        let is2 = self.b.cmp(CmpOp::Eq, Ty::I32, rc, two);
        let r = self.b.select(is2, u, t, Ty::I64);
        let r = self.b.select(is1, d, r, Ty::I64);
        self.b.select(is0, n, r, Ty::I64)
    }

    fn x87_binop(&mut self, op: FpBinOp, src: X87Src, reversed: bool) {
        let (a, b) = match src {
            X87Src::MemF(w) => {
                let st0 = self.ld_st(0);
                let m = self.x87_load(self.mem_operand(), if w == Width::Q { X87Mem::F64 } else { X87Mem::F32 });
                (st0, m)
            }
            X87Src::MemI(w) => {
                let st0 = self.ld_st(0);
                let m = self.x87_load(self.mem_operand(), if w == Width::W { X87Mem::I16 } else { X87Mem::I32 });
                (st0, m)
            }
            X87Src::St0Sti(i) => (self.ld_st(0), self.ld_st(i)),
            X87Src::StiSt0 { i, .. } => (self.ld_st(i), self.ld_st(0)),
        };
        let (a, b) = if reversed { (b, a) } else { (a, b) };
        let r = self.fbin64(op, a, b);
        match src {
            X87Src::StiSt0 { i, pop } => {
                self.st_st(i, r);
                if pop {
                    self.pop_st();
                }
            }
            _ => self.st_st(0, r),
        }
    }

    fn mem_operand(&self) -> Operand {
        self.cur_mem
    }

    pub(crate) fn x87(&mut self, i: &Inst) -> Flow {
        self.cur_mem = i.ops[0].unwrap_or(Operand::Imm(0));
        match i.op {
            Op::Fld(m) => {
                let v = self.x87_load(i.op0(), m);
                self.push_st(v);
            }
            Op::Fild(m) => {
                let v = self.x87_load(i.op0(), m);
                self.push_st(v);
            }
            Op::Fst { mem, pop } => {
                let v = self.ld_st(0);
                self.x87_store(i.op0(), mem, v);
                if pop {
                    self.pop_st();
                }
            }
            Op::Fist { mem, pop, truncate } => {
                let v = self.ld_st(0);
                let r = if truncate {
                    self.fun64(FpUnOp::Trunc, v)
                } else {
                    self.round_by_cw(v)
                };
                let wide = mem == X87Mem::I64;
                let iv = self.b.def(Expr::FpCvt {
                    kind: FpConvert::FpToInt { signed: true },
                    src_wide: true,
                    dst_wide: wide,
                    a: r,
                });
                // Out of range -> integer indefinite.
                let ty = if wide { Ty::I64 } else { Ty::I32 };
                let (lim_bits, indef) = match mem {
                    X87Mem::I16 => (0x40e0_0000_0000_0000u64, 0x8000u64),
                    X87Mem::I32 => (0x41e0_0000_0000_0000, 0x8000_0000),
                    _ => (0x43e0_0000_0000_0000, 1u64 << 63),
                };
                let lim = self.k64(lim_bits);
                let over = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Ge,
                    double: true,
                    a: r,
                    b: lim,
                });
                let nlim = self.k64(lim_bits | (1 << 63));
                let under = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Lt,
                    double: true,
                    a: r,
                    b: nlim,
                });
                let nan = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Ne,
                    double: true,
                    a: r,
                    b: r,
                });
                let bad = self.b.bin(BinOp::Or, Ty::I32, over, under);
                let bad = self.b.bin(BinOp::Or, Ty::I32, bad, nan);
                let ind = self.k(ty, indef);
                let iv = self.b.select(bad, ind, iv, ty);
                let Operand::Mem(mm) = i.op0() else {
                    return self.illegal_x87();
                };
                let (addr, offset) = self.addr(&mm);
                let sz = match mem {
                    X87Mem::I16 => MemSz::B2,
                    X87Mem::I32 => MemSz::B4,
                    _ => MemSz::B8,
                };
                self.b.stmt(Stmt::Store {
                    addr,
                    offset,
                    val: iv,
                    sz,
                    order: Order::Relaxed,
                });
                if pop {
                    self.pop_st();
                }
            }
            Op::FldSt(n) => {
                let v = self.ld_st(n);
                self.push_st(v);
            }
            Op::FstSt { i: n, pop } => {
                let v = self.ld_st(0);
                self.st_st(n, v);
                if pop {
                    self.pop_st();
                }
            }
            Op::Fxch(n) => {
                let a = self.ld_st(0);
                let b = self.ld_st(n);
                self.st_st(0, b);
                self.st_st(n, a);
            }
            Op::Fadd(s) => self.x87_binop(FpBinOp::Add, s, false),
            Op::Fmul(s) => self.x87_binop(FpBinOp::Mul, s, false),
            Op::Fsub(s) => self.x87_binop(FpBinOp::Sub, s, false),
            Op::Fsubr(s) => self.x87_binop(FpBinOp::Sub, s, true),
            Op::Fdiv(s) => self.x87_binop(FpBinOp::Div, s, false),
            Op::Fdivr(s) => self.x87_binop(FpBinOp::Div, s, true),
            Op::Fcom { src, pop } => {
                let (a, b) = match src {
                    X87Src::MemF(w) => {
                        let m = self.x87_load(i.op0(), if w == Width::Q { X87Mem::F64 } else { X87Mem::F32 });
                        (self.ld_st(0), m)
                    }
                    X87Src::St0Sti(n) => (self.ld_st(0), self.ld_st(n)),
                    _ => return self.illegal_x87(),
                };
                self.x87_compare_sw(a, b);
                for _ in 0..pop {
                    self.pop_st();
                }
            }
            Op::Fucom { i: n, pop } => {
                let a = self.ld_st(0);
                let b = self.ld_st(n);
                self.x87_compare_sw(a, b);
                for _ in 0..pop {
                    self.pop_st();
                }
            }
            Op::Fcomi { i: n, pop, .. } => {
                let a = self.ld_st(0);
                let b = self.ld_st(n);
                self.x87_compare_flags(a, b);
                if pop {
                    self.pop_st();
                }
            }
            Op::Ficom { mem, pop } => {
                let a = self.ld_st(0);
                let b = self.x87_load(i.op0(), mem);
                self.x87_compare_sw(a, b);
                if pop {
                    self.pop_st();
                }
            }
            Op::Fchs => {
                let v = self.ld_st(0);
                let r = self.fun64(FpUnOp::Neg, v);
                self.st_st(0, r);
            }
            Op::Fabs => {
                let v = self.ld_st(0);
                let r = self.fun64(FpUnOp::Abs, v);
                self.st_st(0, r);
            }
            Op::Fsqrt => {
                let v = self.ld_st(0);
                let r = self.fun64(FpUnOp::Sqrt, v);
                self.st_st(0, r);
            }
            Op::Frndint => {
                let v = self.ld_st(0);
                let r = self.round_by_cw(v);
                self.st_st(0, r);
            }
            Op::Fscale => {
                // st0 *= 2^trunc(st1), by building the power of two's bits.
                let v = self.ld_st(0);
                let e = self.ld_st(1);
                let t = self.fun64(FpUnOp::Trunc, e);
                let n = self.b.def(Expr::FpCvt {
                    kind: FpConvert::FpToInt { signed: true },
                    src_wide: true,
                    dst_wide: false,
                    a: t,
                });
                // Clamp to a range that stays a normal double.
                let lo = self.k32((-1022i32) as u32);
                let hi = self.k32(1023);
                let lt = self.b.cmp(CmpOp::LtS, Ty::I32, n, lo);
                let n = self.b.select(lt, lo, n, Ty::I32);
                let gt = self.b.cmp(CmpOp::GtS, Ty::I32, n, hi);
                let n = self.b.select(gt, hi, n, Ty::I32);
                let bias = self.k32(1023);
                let eb = self.b.bin(BinOp::Add, Ty::I32, n, bias);
                let eb = self.b.extend(eb, false);
                let sh = self.k64(52);
                let p2 = self.b.bin(BinOp::Shl, Ty::I64, eb, sh);
                let r = self.fbin64(FpBinOp::Mul, v, p2);
                self.st_st(0, r);
            }
            Op::Ftst => {
                let a = self.ld_st(0);
                let z = self.k64(0);
                self.x87_compare_sw(a, z);
            }
            Op::Fxam => {
                let v = self.ld_st(0);
                let sh63 = self.k64(63);
                let sign = self.b.bin(BinOp::ShrU, Ty::I64, v, sh63);
                let sign = self.b.wrap(sign);
                let abs = self.fun64(FpUnOp::Abs, v);
                let zero = self.k64(0);
                let is_zero = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Eq,
                    double: true,
                    a: abs,
                    b: zero,
                });
                let is_nan = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Ne,
                    double: true,
                    a: v,
                    b: v,
                });
                let inf = self.k64(0x7ffu64 << 52);
                let is_inf = self.b.def(Expr::FpCmp {
                    op: FpCmpOp::Eq,
                    double: true,
                    a: abs,
                    b: inf,
                });
                // Also "empty" if the tag says so.
                let top = self.top();
                let tags = self.b.read(GuestReg::X87Tags);
                let t = self.b.bin(BinOp::ShrU, Ty::I32, tags, top);
                let one = self.k32(1);
                let valid = self.b.bin(BinOp::And, Ty::I32, t, one);
                let empty = self.b.bin(BinOp::Xor, Ty::I32, valid, one);
                // Classes: zero: C3; normal: C2; NaN: C0; inf: C2|C0; empty: C3|C0.
                let c3 = self.k32(C3);
                let c2 = self.k32(C2);
                let c0 = self.k32(C0);
                let c20 = self.k32(C2 | C0);
                let c30 = self.k32(C3 | C0);
                let r = self.b.select(is_zero, c3, c2, Ty::I32);
                let r = self.b.select(is_inf, c20, r, Ty::I32);
                let r = self.b.select(is_nan, c0, r, Ty::I32);
                let r = self.b.select(empty, c30, r, Ty::I32);
                let nine = self.k32(9);
                let c1 = self.b.bin(BinOp::Shl, Ty::I32, sign, nine);
                let r = self.b.bin(BinOp::Or, Ty::I32, r, c1);
                let sw = self.b.read(GuestReg::X87Sw);
                let clear = self.k32(!(C0 | C1 | C2 | C3));
                let sw = self.b.bin(BinOp::And, Ty::I32, sw, clear);
                let sw = self.b.bin(BinOp::Or, Ty::I32, sw, r);
                self.b.write(GuestReg::X87Sw, sw);
            }
            Op::Fld1 | Op::Fldl2t | Op::Fldl2e | Op::Fldpi | Op::Fldlg2 | Op::Fldln2 | Op::Fldz => {
                let bits: u64 = match i.op {
                    Op::Fld1 => 1.0f64.to_bits(),
                    Op::Fldl2t => std::f64::consts::LOG2_10.to_bits(),
                    Op::Fldl2e => std::f64::consts::LOG2_E.to_bits(),
                    Op::Fldpi => std::f64::consts::PI.to_bits(),
                    Op::Fldlg2 => std::f64::consts::LOG10_2.to_bits(),
                    Op::Fldln2 => std::f64::consts::LN_2.to_bits(),
                    _ => 0,
                };
                let v = self.k64(bits);
                self.push_st(v);
            }
            Op::Fdecstp => {
                let top = self.top();
                let seven = self.k32(7);
                let t = self.b.bin(BinOp::Add, Ty::I32, top, seven);
                let t = self.b.bin(BinOp::And, Ty::I32, t, seven);
                self.set_top(t);
            }
            Op::Fincstp => {
                let top = self.top();
                let one = self.k32(1);
                let seven = self.k32(7);
                let t = self.b.bin(BinOp::Add, Ty::I32, top, one);
                let t = self.b.bin(BinOp::And, Ty::I32, t, seven);
                self.set_top(t);
            }
            Op::Fnop | Op::Fwait => {}
            Op::Fninit => {
                let cw = self.k32(0x37f);
                self.b.write(GuestReg::X87Cw, cw);
                let z = self.k32(0);
                self.b.write(GuestReg::X87Sw, z);
                self.set_top(z);
                self.b.write(GuestReg::X87Tags, z);
            }
            Op::Fnclex => {
                let sw = self.b.read(GuestReg::X87Sw);
                let m = self.k32(!0x80ffu32);
                let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
                self.b.write(GuestReg::X87Sw, sw);
            }
            Op::Fldcw => {
                let v = self.rd(i.op0(), Width::W);
                self.b.write(GuestReg::X87Cw, v);
            }
            Op::Fnstcw => {
                let v = self.b.read(GuestReg::X87Cw);
                self.wr(i.op0(), Width::W, v);
            }
            Op::Fnstsw | Op::FnstswAx => {
                let sw = self.b.read(GuestReg::X87Sw);
                let m = self.k32(!0x3800u32);
                let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
                let top = self.top();
                let eleven = self.k32(11);
                let t = self.b.bin(BinOp::Shl, Ty::I32, top, eleven);
                let v = self.b.bin(BinOp::Or, Ty::I32, sw, t);
                if i.op == Op::FnstswAx {
                    self.wr_reg(gex_isa_x86::Reg::Gp(0, Width::W), v);
                } else {
                    self.wr(i.op0(), Width::W, v);
                }
            }
            Op::Ffree(n) | Op::Ffreep(n) => {
                let top = self.top();
                let nv = self.k32(n as u32);
                let idx = self.b.bin(BinOp::Add, Ty::I32, top, nv);
                let seven = self.k32(7);
                let idx = self.b.bin(BinOp::And, Ty::I32, idx, seven);
                let one = self.k32(1);
                let bit = self.b.bin(BinOp::Shl, Ty::I32, one, idx);
                let nbit = self.b.un(UnOp::Not, Ty::I32, bit);
                let tags = self.b.read(GuestReg::X87Tags);
                let tags = self.b.bin(BinOp::And, Ty::I32, tags, nbit);
                self.b.write(GuestReg::X87Tags, tags);
                if matches!(i.op, Op::Ffreep(_)) {
                    self.pop_st();
                }
            }
            Op::Fcmovcc { cond, i: n } => {
                let c = self.cond(cond);
                let a = self.ld_st(0);
                let b = self.ld_st(n);
                let r = self.b.select(c, b, a, Ty::I64);
                self.st_st(0, r);
            }
            // The transcendental and remainder instructions have no wasm
            // form: the host computes them on the register stack in the
            // context (`gex_kernel::x86::serve_x87`), pushes and pops
            // included. i386 musl's libm is built on them (`atan2`, `log`,
            // `exp`, `fmod`, `remquo` are x87 assembly there).
            Op::F2xm1
            | Op::Fyl2x
            | Op::Fptan
            | Op::Fpatan
            | Op::Fxtract
            | Op::Fprem1
            | Op::Fprem
            | Op::Fyl2xp1
            | Op::Fsincos
            | Op::Fsin
            | Op::Fcos => {
                let op = match i.op {
                    Op::F2xm1 => crate::x87op::F2XM1,
                    Op::Fyl2x => crate::x87op::FYL2X,
                    Op::Fptan => crate::x87op::FPTAN,
                    Op::Fpatan => crate::x87op::FPATAN,
                    Op::Fxtract => crate::x87op::FXTRACT,
                    Op::Fprem1 => crate::x87op::FPREM1,
                    Op::Fprem => crate::x87op::FPREM,
                    Op::Fyl2xp1 => crate::x87op::FYL2XP1,
                    Op::Fsincos => crate::x87op::FSINCOS,
                    Op::Fsin => crate::x87op::FSIN,
                    _ => crate::x87op::FCOS,
                };
                let pcv = self.k64(self.pc);
                let id = self.k32(crate::sysreg::X87 as u32);
                let v = self.k64(op);
                self.b.stmt(Stmt::HostCall {
                    func: HostFunc::SysRegWrite,
                    args: vec![pcv, id, v],
                });
            }
            Op::Frstor | Op::Fnsave => return self.illegal_x87(),
            // The x87 environment, 28 bytes in the 32-bit protected-mode
            // layout glibc's fenv.h expects: control word, status word, tag
            // word, then the instruction/operand pointers (stored as zero, as
            // a real CPU stores nothing meaningful after `fninit`). `fnstenv`
            // also masks every exception in the live control word, which
            // `feholdexcept` relies on. Tags here are one valid bit per
            // register; the tag word is two bits per register, 11 = empty.
            Op::Fnstenv | Op::Fldenv => {
                let Operand::Mem(m) = i.op0() else { return self.illegal_x87() };
                let (addr, off) = self.addr(&m);
                let addr = self.offset_addr(addr, off);
                let st32 = |this: &mut Self, offset: i64, val: Value| {
                    this.b.stmt(Stmt::Store { addr, offset, val, sz: MemSz::B4, order: Order::Relaxed });
                };
                if i.op == Op::Fnstenv {
                    let cw = self.b.read(GuestReg::X87Cw);
                    let hi = self.k32(0xffff_0000);
                    let cwv = self.b.bin(BinOp::Or, Ty::I32, cw, hi);
                    st32(self, 0, cwv);
                    let sw = self.b.read(GuestReg::X87Sw);
                    let m = self.k32(!0x3800u32);
                    let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
                    let top = self.top();
                    let eleven = self.k32(11);
                    let t = self.b.bin(BinOp::Shl, Ty::I32, top, eleven);
                    let sw = self.b.bin(BinOp::Or, Ty::I32, sw, t);
                    let swv = self.b.bin(BinOp::Or, Ty::I32, sw, hi);
                    st32(self, 4, swv);
                    // Spread the 8 valid bits to 16 (each bit doubled), then
                    // invert: valid -> 00, empty -> 11.
                    let tags = self.b.read(GuestReg::X87Tags);
                    let m8 = self.k32(0xff);
                    let x = self.b.bin(BinOp::And, Ty::I32, tags, m8);
                    let four = self.k32(4);
                    let s4 = self.b.bin(BinOp::Shl, Ty::I32, x, four);
                    let x = self.b.bin(BinOp::Or, Ty::I32, x, s4);
                    let m0f = self.k32(0x0f0f);
                    let x = self.b.bin(BinOp::And, Ty::I32, x, m0f);
                    let two = self.k32(2);
                    let s2 = self.b.bin(BinOp::Shl, Ty::I32, x, two);
                    let x = self.b.bin(BinOp::Or, Ty::I32, x, s2);
                    let m33 = self.k32(0x3333);
                    let x = self.b.bin(BinOp::And, Ty::I32, x, m33);
                    let one = self.k32(1);
                    let s1 = self.b.bin(BinOp::Shl, Ty::I32, x, one);
                    let x = self.b.bin(BinOp::Or, Ty::I32, x, s1);
                    let m55 = self.k32(0x5555);
                    let x = self.b.bin(BinOp::And, Ty::I32, x, m55);
                    let s1 = self.b.bin(BinOp::Shl, Ty::I32, x, one);
                    let x = self.b.bin(BinOp::Or, Ty::I32, x, s1);
                    let all = self.k32(0xffff);
                    let tw = self.b.bin(BinOp::Xor, Ty::I32, x, all);
                    let twv = self.b.bin(BinOp::Or, Ty::I32, tw, hi);
                    st32(self, 8, twv);
                    let zero = self.k32(0);
                    st32(self, 12, zero);
                    st32(self, 16, zero);
                    st32(self, 20, zero);
                    st32(self, 24, zero);
                    // Then mask all exceptions.
                    let m3f = self.k32(0x3f);
                    let cw2 = self.b.bin(BinOp::Or, Ty::I32, cw, m3f);
                    self.b.write(GuestReg::X87Cw, cw2);
                } else {
                    let ld16 = |this: &mut Self, offset: i64| {
                        this.b.def(Expr::Load { addr, offset, sz: MemSz::B2, signed: false, ty: Ty::I32, order: Order::Relaxed })
                    };
                    let cw = ld16(self, 0);
                    self.b.write(GuestReg::X87Cw, cw);
                    let sw = ld16(self, 4);
                    let eleven = self.k32(11);
                    let t = self.b.bin(BinOp::ShrU, Ty::I32, sw, eleven);
                    let seven = self.k32(7);
                    let top = self.b.bin(BinOp::And, Ty::I32, t, seven);
                    self.set_top(top);
                    let m = self.k32(!0x3800u32);
                    let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
                    self.b.write(GuestReg::X87Sw, sw);
                    // Tag word -> valid bits: a register is valid when its two
                    // tag bits are not both set. Compress the even bits to 8.
                    let tw = ld16(self, 8);
                    let one = self.k32(1);
                    let hi1 = self.b.bin(BinOp::ShrU, Ty::I32, tw, one);
                    let both = self.b.bin(BinOp::And, Ty::I32, tw, hi1);
                    let nb = self.b.un(UnOp::Not, Ty::I32, both);
                    let m55 = self.k32(0x5555);
                    let v = self.b.bin(BinOp::And, Ty::I32, nb, m55);
                    let s1 = self.b.bin(BinOp::ShrU, Ty::I32, v, one);
                    let v = self.b.bin(BinOp::Or, Ty::I32, v, s1);
                    let m33 = self.k32(0x3333);
                    let v = self.b.bin(BinOp::And, Ty::I32, v, m33);
                    let two = self.k32(2);
                    let s2 = self.b.bin(BinOp::ShrU, Ty::I32, v, two);
                    let v = self.b.bin(BinOp::Or, Ty::I32, v, s2);
                    let m0f = self.k32(0x0f0f);
                    let v = self.b.bin(BinOp::And, Ty::I32, v, m0f);
                    let four = self.k32(4);
                    let s4 = self.b.bin(BinOp::ShrU, Ty::I32, v, four);
                    let v = self.b.bin(BinOp::Or, Ty::I32, v, s4);
                    let m8 = self.k32(0xff);
                    let v = self.b.bin(BinOp::And, Ty::I32, v, m8);
                    self.b.write(GuestReg::X87Tags, v);
                }
            }
            _ => return self.illegal_x87(),
        }
        Flow::Next
    }

    fn illegal_x87(&self) -> Flow {
        Flow::End(Term::Illegal {
            raw: self.raw,
            pc: self.pc,
        })
    }
}
