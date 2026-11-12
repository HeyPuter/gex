//! The SSE4.1 and SSE4.2 instructions that are not one lane operation:
//! `phminposuw`, `mpsadbw`, `dpps`/`dppd`, the string compares
//! (`pcmpestri`, `pcmpestrm`, `pcmpistri`, `pcmpistrm`), `crc32` and
//! `pclmulqdq`. Each is expanded in GIR, specialised on its immediate
//! (known at translation), so every tier runs the same code.

use super::lane_shuffle;
use crate::{FlagSource, Flow, Lifter};
use gex_ir::*;
use gex_isa_x86::{Inst, Lane, Op, Operand, Reg, Width};

/// CRC-32C's polynomial, bit-reflected (`crc32` computes CRC-32C).
const CRC32C: u32 = 0x82f6_3b78;

/// A shuffle putting byte or word lane `k` of `a` in every lane.
fn splat_lane(k: usize, lane_bytes: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (k * lane_bytes + i % lane_bytes) as u8;
    }
    out
}

impl<'a> Lifter<'a> {
    /// The instructions this module covers; `None` for any other.
    pub(super) fn sse4_op(&mut self, i: &Inst) -> Option<Flow> {
        Some(match i.op {
            Op::Phminposuw => self.phminposuw(i),
            Op::Mpsadbw => self.mpsadbw(i),
            Op::Dpp => self.dpp(i),
            Op::Pcmpestri | Op::Pcmpestrm | Op::Pcmpistri | Op::Pcmpistrm => self.pcmpstr(i),
            Op::Pclmulqdq => self.pclmulqdq(i),
            _ => return None,
        })
    }

    /// The minimum unsigned word in word 0, its lowest index in word 1, the
    /// rest zero.
    fn phminposuw(&mut self, i: &Inst) -> Flow {
        let s = self.rd_x(i.op1());
        let mut m = s;
        for (from, n) in [(4usize, 4usize), (2, 2), (1, 1)] {
            let mut sel = [0u8; 16];
            for (k, o) in sel.iter_mut().enumerate() {
                let w = k / 2;
                *o = if w < n { ((from + w) * 2 + k % 2) as u8 } else { k as u8 };
            }
            let h = self.shuffle(m, m, sel);
            m = self.vbin(VLaneBinOp::MinU, VLane::I16, m, h);
        }
        let min = self.shuffle(m, m, splat_lane(0, 2));
        let eq = self.vbin(VLaneBinOp::Eq, VLane::I16, s, min);
        let bits = self.b.def(Expr::VBitmask { a: eq, lane: VLane::I16 });
        let idx = self.b.un(UnOp::Ctz, Ty::I32, bits);
        let w0 = self.extract(m, 0, 2, false, Ty::I32);
        let z = self.zero_v();
        let r = self.replace(z, 0, 2, w0);
        let r = self.replace(r, 1, 2, idx);
        self.wr_x(i.op0(), r);
        Flow::Next
    }

    /// Eight sums of absolute differences of unsigned bytes: a 4-byte block
    /// of the source (imm bits 0-1) against eight overlapping 4-byte runs of
    /// the destination from byte 0 or 4 (imm bit 2).
    fn mpsadbw(&mut self, i: &Inst) -> Flow {
        let Operand::Imm(imm) = i.op2() else { return self.illegal() };
        let (a, b) = self.packed_ops(i);
        let aoff = (((imm >> 2) & 1) * 4) as usize;
        let boff = ((imm & 3) * 4) as usize;
        let mut acc = self.zero_v();
        for k in 0..4 {
            let mut sel = [0u8; 16];
            for (j, o) in sel.iter_mut().enumerate() {
                *o = (aoff + k + j % 8) as u8;
            }
            let ab = self.shuffle(a, a, sel);
            let bb = self.shuffle(b, b, [(boff + k) as u8; 16]);
            let hi = self.vbin(VLaneBinOp::MaxU, VLane::I8, ab, bb);
            let lo = self.vbin(VLaneBinOp::MinU, VLane::I8, ab, bb);
            let d = self.vbin(VLaneBinOp::Sub, VLane::I8, hi, lo);
            let dw = self.b.def(Expr::VCvt { op: VCvtOp::ExtendLow { signed: false }, from: VLane::I8, a: d, b: None });
            acc = self.vbin(VLaneBinOp::Add, VLane::I16, acc, dw);
        }
        self.wr_x(i.op0(), acc);
        Flow::Next
    }

    /// `dpps`/`dppd`: the products the high immediate bits keep (the rest
    /// +0.0), summed into the lanes the low bits name, the others +0.0.
    fn dpp(&mut self, i: &Inst) -> Flow {
        let Operand::Imm(imm) = i.op2() else { return self.illegal() };
        let imm = imm as u32;
        let double = i.lane == Lane::F64;
        let (vl, lb, n) = if double { (VLane::F64, 8usize, 2usize) } else { (VLane::F32, 4, 4) };
        let (a, b) = self.packed_ops(i);
        let p = self.vbin(VLaneBinOp::Mul, vl, a, b);
        let lanes_of = |bits: u32| {
            let mut m = [0u8; 16];
            for k in 0..n {
                if (bits >> k) & 1 == 1 {
                    for byte in 0..lb {
                        m[k * lb + byte] = 0xff;
                    }
                }
            }
            m
        };
        let keep = self.vconst(lanes_of(imm >> 4));
        let p = self.vbit(VBinOp::And, p, keep);
        // Each lane gets its own sum, the operands in the order the hardware
        // adds them (which decides the NaN that comes out): dpps
        // `(p[k^1] + p[k]) + (the same for the other pair)`, dppd
        // `p[k] + p[k^1]`.
        let all = if double {
            let sw = self.shuffle(p, p, lane_shuffle(8, &[1, 0]));
            self.add_x86(vl, p, sw)
        } else {
            let sw = self.shuffle(p, p, lane_shuffle(4, &[1, 0, 3, 2]));
            let s = self.add_x86(vl, sw, p);
            let sh = self.shuffle(s, s, lane_shuffle(4, &[2, 3, 0, 1]));
            self.add_x86(vl, s, sh)
        };
        let out = self.vconst(lanes_of(imm & 15));
        let r = self.vbit(VBinOp::And, all, out);
        self.wr_x(i.op0(), r);
        Flow::Next
    }

    /// `x + y` with x86's NaN: `x`'s quieted if it is one, else `y`'s, else
    /// the sum's. Spelled out, as a compiler may swap an add's operands
    /// (the function tier's C does), which for wasm's add changes the NaN.
    fn add_x86(&mut self, vl: VLane, x: Value, y: Value) -> Value {
        let sum = self.vbin(VLaneBinOp::Add, vl, x, y);
        let q = if vl == VLane::F64 { self.k64(1 << 51) } else { self.k32(1 << 22) };
        let q = self.vsplat(q, if vl == VLane::F64 { 8 } else { 4 });
        let nx = self.vbin(VLaneBinOp::Ne, vl, x, x);
        let ny = self.vbin(VLaneBinOp::Ne, vl, y, y);
        let qx = self.vbit(VBinOp::Or, x, q);
        let qy = self.vbit(VBinOp::Or, y, q);
        let r = self.b.def(Expr::VBitSelect { mask: ny, a: qy, b: sum });
        self.b.def(Expr::VBitSelect { mask: nx, a: qx, b: r })
    }

    /// A string's length in elements: explicit, `|reg|` saturated at `n`
    /// (the 64-bit register under REX.W); implicit, the index of its first
    /// zero element, or `n`.
    fn str_len(&mut self, s: Value, explicit: Option<(u8, Width)>, n: u32, lane: VLane) -> Value {
        match explicit {
            Some((reg, w)) => {
                let wide = w == Width::Q;
                let ty = if wide { Ty::I64 } else { Ty::I32 };
                let v = self.rd_reg(Reg::Gp(reg, w));
                let z = self.k(ty, 0);
                let neg = self.b.bin(BinOp::Sub, ty, z, v);
                let lt = self.b.cmp(CmpOp::LtS, ty, v, z);
                let abs = self.b.select(lt, neg, v, ty);
                let nn = self.k(ty, n as u64);
                let big = self.b.cmp(CmpOp::GtU, ty, abs, nn);
                let abs = self.b.select(big, nn, abs, ty);
                if wide { self.b.wrap(abs) } else { abs }
            }
            None => {
                let z = self.zero_v();
                let eq = self.vbin(VLaneBinOp::Eq, lane, s, z);
                let bits = self.b.def(Expr::VBitmask { a: eq, lane });
                let end = self.k32(1 << n);
                let bits = self.b.bin(BinOp::Or, Ty::I32, bits, end);
                self.b.un(UnOp::Ctz, Ty::I32, bits)
            }
        }
    }

    /// The lanes below `len`, all-ones, the rest zero.
    fn lanes_below(&mut self, len: Value, n: u32, lane: VLane) -> Value {
        let lb = 16 / n;
        let mut iota = [0u8; 16];
        for k in 0..n as usize {
            iota[k * lb as usize] = k as u8;
        }
        let iota = self.vconst(iota);
        let l = self.vsplat(len, lb);
        self.vbin(VLaneBinOp::LtU, lane, iota, l)
    }

    /// The string compares. The immediate gives the element (unsigned or
    /// signed bytes or words), the aggregation (equal any, ranges, equal
    /// each, equal ordered), the polarity and the output's form; the
    /// result's bits set the flags (CF any, ZF the second string ends, SF
    /// the first ends, OF bit 0) and go to ECX as an index or to XMM0 as a
    /// mask.
    fn pcmpstr(&mut self, i: &Inst) -> Flow {
        let Operand::Imm(imm) = i.op2() else { return self.illegal() };
        let imm = imm as u32;
        let words = imm & 1 == 1;
        let signed = imm & 2 == 2;
        let agg = (imm >> 2) & 3;
        let pol = (imm >> 4) & 3;
        let most = (imm >> 6) & 1 == 1;
        let (lane, lb, n) = if words { (VLane::I16, 2usize, 8u32) } else { (VLane::I8, 1usize, 16u32) };
        let explicit = matches!(i.op, Op::Pcmpestri | Op::Pcmpestrm);
        let w = if i.width == Width::Q { Width::Q } else { Width::D };
        let a = self.rd_x(i.op0());
        let b = self.rd_x(i.op1());
        let la = self.str_len(a, explicit.then_some((0, w)), n, lane);
        let lbv = self.str_len(b, explicit.then_some((2, w)), n, lane);
        let va = self.lanes_below(la, n, lane);
        let vb = self.lanes_below(lbv, n, lane);
        let ones = self.ones_v();
        let res = match agg {
            // equal any: b[j] is some valid a[i]
            0 => {
                let mut acc = self.zero_v();
                for k in 0..n as usize {
                    let ak = self.shuffle(a, a, splat_lane(k, lb));
                    let vk = self.shuffle(va, va, splat_lane(k, lb));
                    let eq = self.vbin(VLaneBinOp::Eq, lane, b, ak);
                    let eq = self.vbit(VBinOp::And, eq, vk);
                    acc = self.vbit(VBinOp::Or, acc, eq);
                }
                self.vbit(VBinOp::And, acc, vb)
            }
            // ranges: a[2k] <= b[j] <= a[2k+1] for a valid pair
            1 => {
                let (ge, le) = if signed { (VLaneBinOp::GeS, VLaneBinOp::LeS) } else { (VLaneBinOp::GeU, VLaneBinOp::LeU) };
                let mut acc = self.zero_v();
                for k in 0..(n / 2) as usize {
                    let lo = self.shuffle(a, a, splat_lane(2 * k, lb));
                    let hi = self.shuffle(a, a, splat_lane(2 * k + 1, lb));
                    let vhi = self.shuffle(va, va, splat_lane(2 * k + 1, lb));
                    let g = self.vbin(ge, lane, b, lo);
                    let l = self.vbin(le, lane, b, hi);
                    let m = self.vbit(VBinOp::And, g, l);
                    // a[2k+1] valid implies a[2k] valid
                    let m = self.vbit(VBinOp::And, m, vhi);
                    acc = self.vbit(VBinOp::Or, acc, m);
                }
                self.vbit(VBinOp::And, acc, vb)
            }
            // equal each: a[j] == b[j]; past both ends true, past one false
            2 => {
                let eq = self.vbin(VLaneBinOp::Eq, lane, a, b);
                let both = self.vbit(VBinOp::And, va, vb);
                let eq = self.vbit(VBinOp::And, eq, both);
                let either = self.vbit(VBinOp::Or, va, vb);
                let neither = self.vbit(VBinOp::Xor, either, ones);
                self.vbit(VBinOp::Or, eq, neither)
            }
            // equal ordered: a occurs in b at j (a ending first matches)
            _ => {
                let mut acc = ones;
                let z = self.zero_v();
                for k in 0..n as usize {
                    // b and its validity moved down k elements, zeros above
                    let mut sel = [0u8; 16];
                    for (j, o) in sel.iter_mut().enumerate() {
                        let from = j + k * lb;
                        *o = if from < 16 { from as u8 } else { 16 };
                    }
                    let bk = self.shuffle(b, z, sel);
                    let vbk = self.shuffle(vb, z, sel);
                    let ak = self.shuffle(a, a, splat_lane(k, lb));
                    let vak = self.shuffle(va, va, splat_lane(k, lb));
                    let eq = self.vbin(VLaneBinOp::Eq, lane, bk, ak);
                    let eq = self.vbit(VBinOp::And, eq, vbk);
                    let not_va = self.vbit(VBinOp::Xor, vak, ones);
                    let mut term = self.vbit(VBinOp::Or, eq, not_va);
                    if k > 0 {
                        // lanes j >= n - k are past the comparison's end
                        let mut past = [0u8; 16];
                        for (j, o) in past.iter_mut().enumerate() {
                            if j / lb >= n as usize - k {
                                *o = 0xff;
                            }
                        }
                        let past = self.vconst(past);
                        term = self.vbit(VBinOp::Or, term, past);
                    }
                    acc = self.vbit(VBinOp::And, acc, term);
                }
                acc
            }
        };
        let res = match pol {
            1 => self.vbit(VBinOp::Xor, res, ones),
            3 => self.vbit(VBinOp::Xor, res, vb),
            _ => res,
        };
        let bits = self.b.def(Expr::VBitmask { a: res, lane });
        let zero = self.k32(0);
        let nn = self.k32(n);
        let cf = self.b.cmp(CmpOp::Ne, Ty::I32, bits, zero);
        let zf = self.b.cmp(CmpOp::LtU, Ty::I32, lbv, nn);
        let sf = self.b.cmp(CmpOp::LtU, Ty::I32, la, nn);
        let one = self.k32(1);
        let of = self.b.bin(BinOp::And, Ty::I32, bits, one);
        match i.op {
            Op::Pcmpestri | Op::Pcmpistri => {
                let idx = if most {
                    let c = self.b.un(UnOp::Clz, Ty::I32, bits);
                    let k31 = self.k32(31);
                    self.b.bin(BinOp::Sub, Ty::I32, k31, c)
                } else {
                    self.b.un(UnOp::Ctz, Ty::I32, bits)
                };
                let idx = self.b.select(cf, idx, nn, Ty::I32);
                self.wr_reg(Reg::Gp(1, Width::D), idx);
            }
            _ => {
                let m = if most {
                    res
                } else {
                    let z = self.zero_v();
                    self.replace(z, 0, 4, bits)
                };
                self.b.write(GuestReg::V128(0), m);
            }
        }
        self.b.write(GuestReg::C, cf);
        self.b.write(GuestReg::Z, zf);
        self.b.write(GuestReg::N, sf);
        self.b.write(GuestReg::V, of);
        self.b.write(GuestReg::P, zero);
        self.b.write(GuestReg::A, zero);
        self.flags = FlagSource::Unknown;
        Flow::Next
    }

    /// `crc32`: CRC-32C of the source's bytes, low byte first, onto the
    /// destination's low 32 bits (a 64-bit destination is zero-extended).
    pub(crate) fn crc32(&mut self, i: &Inst) -> Flow {
        let sw = i.width;
        let dst = i.op0();
        let dw = match dst {
            Operand::Reg(Reg::Gp(_, w)) => w,
            _ => Width::D,
        };
        let crc = self.rd(dst, dw);
        let mut crc = if dw == Width::Q { self.b.wrap(crc) } else { crc };
        let src = self.rd(i.op1(), sw);
        // the source in 32-bit pieces, low first, with their bit counts
        let pieces: Vec<(Value, u32)> = if sw == Width::Q {
            let lo = self.b.wrap(src);
            let k32 = self.k64(32);
            let hi = self.b.bin(BinOp::ShrU, Ty::I64, src, k32);
            let hi = self.b.wrap(hi);
            vec![(lo, 32), (hi, 32)]
        } else {
            let v = self.narrow(src, sw);
            vec![(v, sw.bits())]
        };
        let poly = self.k32(CRC32C);
        let one = self.k32(1);
        let zero = self.k32(0);
        for (v, nbits) in pieces {
            crc = self.b.bin(BinOp::Xor, Ty::I32, crc, v);
            for _ in 0..nbits {
                let lsb = self.b.bin(BinOp::And, Ty::I32, crc, one);
                let m = self.b.bin(BinOp::Sub, Ty::I32, zero, lsb);
                let p = self.b.bin(BinOp::And, Ty::I32, poly, m);
                let sh = self.b.bin(BinOp::ShrU, Ty::I32, crc, one);
                crc = self.b.bin(BinOp::Xor, Ty::I32, sh, p);
            }
        }
        match dst {
            Operand::Reg(r) => {
                let r = match r {
                    Reg::Gp(n, Width::Q) => Reg::Gp(n, Width::D),
                    r => r,
                };
                self.wr_reg(r, crc);
            }
            _ => return self.illegal(),
        }
        Flow::Next
    }

    /// `pclmulqdq`: the carry-less product of the quadwords imm bit 0 (of the
    /// destination) and bit 4 (of the source) select, 128 bits.
    fn pclmulqdq(&mut self, i: &Inst) -> Flow {
        let Operand::Imm(imm) = i.op2() else { return self.illegal() };
        let (a, b) = self.packed_ops(i);
        let x = self.extract(a, (imm & 1) as u32, 8, false, Ty::I64);
        let y = self.extract(b, ((imm >> 4) & 1) as u32, 8, false, Ty::I64);
        let zero = self.k64(0);
        let one = self.k64(1);
        let mut lo = zero;
        let mut hi = zero;
        for k in 0..64u64 {
            let kk = self.k64(k);
            let bit = self.b.bin(BinOp::ShrU, Ty::I64, y, kk);
            let bit = self.b.bin(BinOp::And, Ty::I64, bit, one);
            let m = self.b.bin(BinOp::Sub, Ty::I64, zero, bit);
            let l = self.b.bin(BinOp::Shl, Ty::I64, x, kk);
            let l = self.b.bin(BinOp::And, Ty::I64, l, m);
            lo = self.b.bin(BinOp::Xor, Ty::I64, lo, l);
            if k > 0 {
                let r = self.k64(64 - k);
                let h = self.b.bin(BinOp::ShrU, Ty::I64, x, r);
                let h = self.b.bin(BinOp::And, Ty::I64, h, m);
                hi = self.b.bin(BinOp::Xor, Ty::I64, hi, h);
            }
        }
        let z = self.zero_v();
        let r = self.replace(z, 0, 8, lo);
        let r = self.replace(r, 1, 8, hi);
        self.wr_x(i.op0(), r);
        Flow::Next
    }
}
