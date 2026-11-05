//! SSE/SSE2/SSE3/SSSE3/SSE4.1 semantics on the 128-bit register file.
//!
//! An xmm register is one wasm `v128`. Scalar operations extract lane 0,
//! compute in GIR's scalar FP ops (bit patterns), and put it back, which
//! preserves the upper lanes as the architecture requires. Packed operations
//! map to wasm SIMD lane ops where one exists and to shuffles otherwise.

use crate::{ty_of, Flow, Lifter, FlagSource};
use gex_ir::*;
use gex_isa_x86::{Inst, Lane, Op, Operand, Reg, Width};

fn vlane(l: Lane) -> VLane {
    match l {
        Lane::I8 => VLane::I8,
        Lane::I16 => VLane::I16,
        Lane::I32 => VLane::I32,
        Lane::I64 => VLane::I64,
        Lane::F32 => VLane::F32,
        Lane::F64 => VLane::F64,
    }
}

/// Byte-shuffle indices that interleave `a` and `b` lane by lane: the low
/// (`high == false`) or high half of each.
fn unpack_lanes(lane_bytes: usize, high: bool) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = 16 / lane_bytes; // lanes per register
    let start = if high { n / 2 } else { 0 };
    let mut k = 0;
    for l in 0..n / 2 {
        for src in [0usize, 16] {
            for byte in 0..lane_bytes {
                out[k] = (src + (start + l) * lane_bytes + byte) as u8;
                k += 1;
            }
        }
    }
    out
}

/// A shuffle selecting whole lanes: `sel[i]` is the source lane (0..2n over
/// `a` then `b`) for destination lane `i`.
fn lane_shuffle(lane_bytes: usize, sel: &[usize]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, &s) in sel.iter().enumerate() {
        for byte in 0..lane_bytes {
            out[i * lane_bytes + byte] = (s * lane_bytes + byte) as u8;
        }
    }
    out
}


/// `mm0` is `V128(MM_BASE)`: past the sixteen `xmm` registers.
const MM_BASE: u8 = 16;
/// `rdi`, `maskmovq`'s implicit destination.
const RDI_REG: u8 = 7;
/// A shuffle keeping the low 64 bits of `a` and zeroing the rest (`b` zero).
const LOW64: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 16, 16, 16, 16, 16, 16, 16, 16];

impl<'a> Lifter<'a> {
    // -- vector operand access ------------------------------------------------------

    fn zero_v(&mut self) -> Value {
        self.b.def(Expr::VConst(Box::new([0u8; 16])))
    }

    fn ones_v(&mut self) -> Value {
        self.b.def(Expr::VConst(Box::new([0xffu8; 16])))
    }

    fn vconst(&mut self, bytes: [u8; 16]) -> Value {
        self.b.def(Expr::VConst(Box::new(bytes)))
    }

    /// Read a 128-bit operand.
    fn rd_x(&mut self, o: Operand) -> Value {
        self.rd_x_n(o, 16)
    }

    /// Read `bytes` bytes of an operand into the low lanes (memory) or the
    /// whole register.
    fn rd_x_n(&mut self, o: Operand, bytes: u32) -> Value {
        match o {
            Operand::Reg(Reg::Xmm(n)) => self.b.read(GuestReg::V128(n)),
            Operand::Reg(Reg::Mm(n)) => self.b.read(GuestReg::V128(MM_BASE + (n & 7))),
            Operand::Mem(m) => {
                let (addr, offset) = self.addr(&m);
                // An MMX operand in memory is 64 bits.
                let bytes = if self.mmx { bytes.min(8) } else { bytes };
                self.b.def(Expr::VLoad {
                    addr,
                    offset,
                    bytes,
                })
            }
            _ => self.zero_v(),
        }
    }

    fn wr_x(&mut self, o: Operand, v: Value) {
        self.wr_x_n(o, v, 16);
    }

    fn wr_x_n(&mut self, o: Operand, v: Value, bytes: u32) {
        match o {
            Operand::Reg(Reg::Xmm(n)) => self.b.write(GuestReg::V128(n), v),
            Operand::Reg(Reg::Mm(n)) => {
                // Only the low 64 bits are the register; the rest stays zero
                // so a 128-bit lane op on it computes nothing spurious there.
                let z = self.zero_v();
                let lo = self.shuffle(v, z, LOW64);
                self.b.write(GuestReg::V128(MM_BASE + (n & 7)), lo)
            }
            Operand::Mem(m) => {
                let bytes = if self.mmx { bytes.min(8) } else { bytes };
                let (addr, offset) = self.addr(&m);
                self.b.stmt(Stmt::VStore {
                    addr,
                    offset,
                    val: v,
                    bytes,
                });
            }
            _ => {}
        }
    }

    fn shuffle(&mut self, a: Value, b: Value, lanes: [u8; 16]) -> Value {
        self.b.def(Expr::VShuffle {
            a,
            b,
            lanes: Box::new(lanes),
        })
    }

    /// A float lane operation; one MXCSR rounds (arithmetic, see
    /// [`Self::mx`]) under the guest's MXCSR.
    fn vbin(&mut self, op: VLaneBinOp, lane: VLane, a: Value, b: Value) -> Value {
        self.mx(Expr::VLaneBin { op, lane, a, b })
    }

    /// A lane operation, under MXCSR when it is a square root. (A round to
    /// nearest is under MXCSR only when the instruction says so:
    /// [`Self::vun_mx`].)
    fn vun(&mut self, op: VLaneUnOp, lane: VLane, a: Value) -> Value {
        if op == VLaneUnOp::Sqrt {
            return self.mx(Expr::VLaneUn { op, lane, a });
        }
        self.b.def(Expr::VLaneUn { op, lane, a })
    }

    /// A lane operation under the guest's MXCSR.
    fn vun_mx(&mut self, op: VLaneUnOp, lane: VLane, a: Value) -> Value {
        self.mx(Expr::VLaneUn { op, lane, a })
    }

    /// `e` as SSE computes it: under the guest's MXCSR rounding control
    /// when it is an operation MXCSR rounds
    /// ([`Expr::mx_able`]) and the program asked for it
    /// ([`gex_ir::mxcsr_exact`]), else as it is.
    fn mx(&mut self, e: Expr) -> Value {
        if !e.mx_able() || !gex_ir::mxcsr_exact() {
            return self.b.def(e);
        }
        let mx = self.b.read(GuestReg::Mxcsr);
        self.b.def(Expr::Mx { mx, e: Box::new(e) })
    }

    fn vbit(&mut self, op: VBinOp, a: Value, b: Value) -> Value {
        self.b.def(Expr::VBin { op, a, b })
    }

    fn vsplat(&mut self, a: Value, lane_bytes: u32) -> Value {
        self.b.def(Expr::VSplat { a, lane_bytes })
    }

    /// Lane 0 of a float vector as a bit pattern (`I32`/`I64`).
    fn scalar_of(&mut self, v: Value, lane: Lane) -> Value {
        let (lb, ty) = if lane == Lane::F64 { (8, Ty::I64) } else { (4, Ty::I32) };
        self.b.def(Expr::VExtract {
            a: v,
            lane: 0,
            lane_bytes: lb,
            signed: false,
            ty,
        })
    }

    fn with_scalar(&mut self, v: Value, lane: Lane, s: Value) -> Value {
        let lb = if lane == Lane::F64 { 8 } else { 4 };
        self.b.def(Expr::VReplace {
            a: v,
            lane: 0,
            lane_bytes: lb,
            val: s,
        })
    }

    fn extract(&mut self, v: Value, lane: u32, lane_bytes: u32, signed: bool, ty: Ty) -> Value {
        self.b.def(Expr::VExtract {
            a: v,
            lane,
            lane_bytes,
            signed,
            ty,
        })
    }

    fn replace(&mut self, v: Value, lane: u32, lane_bytes: u32, val: Value) -> Value {
        self.b.def(Expr::VReplace {
            a: v,
            lane,
            lane_bytes,
            val,
        })
    }

    fn fbin(&mut self, op: FpBinOp, double: bool, a: Value, b: Value) -> Value {
        self.mx(Expr::FpBin { op, double, a, b })
    }

    fn fcmp(&mut self, op: FpCmpOp, double: bool, a: Value, b: Value) -> Value {
        self.b.def(Expr::FpCmp { op, double, a, b })
    }

    /// The source of a scalar op: memory reads only the scalar's bytes.
    fn rd_scalar_src(&mut self, o: Operand, lane: Lane) -> Value {
        let v = self.rd_x_n(o, if lane == Lane::F64 { 8 } else { 4 });
        self.scalar_of(v, lane)
    }

    /// Both operands of a packed op; a memory source is a full 16 bytes.
    fn packed_ops(&mut self, i: &Inst) -> (Value, Value) {
        let a = self.rd_x(i.op0());
        let b = self.rd_x(i.op1());
        (a, b)
    }

    /// x86 min/max: `a < b ? a : b` (min), `a > b ? a : b` (max) — the second
    /// operand wins on NaN or equality, unlike wasm's NaN-propagating forms.
    fn minmax(&mut self, i: &Inst, is_min: bool) {
        let lane = i.lane;
        let double = lane == Lane::F64;
        if i.scalar {
            let dv = self.rd_x(i.op0());
            let a = self.scalar_of(dv, lane);
            let b = self.rd_scalar_src(i.op1(), lane);
            let c = self.fcmp(if is_min { FpCmpOp::Lt } else { FpCmpOp::Gt }, double, a, b);
            let r = self.b.select(c, a, b, if double { Ty::I64 } else { Ty::I32 });
            let r = self.with_scalar(dv, lane, r);
            self.wr_x(i.op0(), r);
        } else {
            let (a, b) = self.packed_ops(i);
            let c = self.vbin(if is_min { VLaneBinOp::LtS } else { VLaneBinOp::GtS }, vlane(lane), a, b);
            let r = self.b.def(Expr::VBitSelect { mask: c, a, b });
            self.wr_x(i.op0(), r);
        }
    }

    /// A packed or scalar two-operand float op.
    fn farith(&mut self, i: &Inst, vop: VLaneBinOp, sop: FpBinOp) {
        let lane = i.lane;
        if i.scalar {
            let dv = self.rd_x(i.op0());
            let a = self.scalar_of(dv, lane);
            let b = self.rd_scalar_src(i.op1(), lane);
            let r = self.fbin(sop, lane == Lane::F64, a, b);
            let r = self.with_scalar(dv, lane, r);
            self.wr_x(i.op0(), r);
        } else {
            let (a, b) = self.packed_ops(i);
            let r = self.vbin(vop, vlane(lane), a, b);
            self.wr_x(i.op0(), r);
        }
    }

    fn illegal(&self) -> Flow {
        Flow::End(Term::Illegal {
            raw: self.raw,
            pc: self.pc,
        })
    }

    /// The truth of a compare predicate for packed lanes (all-ones/zero).
    fn packed_predicate(&mut self, pred: u8, lane: VLane, a: Value, b: Value) -> Value {
        use VLaneBinOp::*;
        match pred & 7 {
            0 => self.vbin(Eq, lane, a, b),
            1 => self.vbin(LtS, lane, a, b),
            2 => self.vbin(LeS, lane, a, b),
            3 => {
                let na = self.vbin(Ne, lane, a, a);
                let nb = self.vbin(Ne, lane, b, b);
                self.vbit(VBinOp::Or, na, nb)
            }
            4 => self.vbin(Ne, lane, a, b),
            5 => {
                let lt = self.vbin(LtS, lane, a, b);
                let ones = self.ones_v();
                self.vbit(VBinOp::Xor, lt, ones)
            }
            6 => {
                let le = self.vbin(LeS, lane, a, b);
                let ones = self.ones_v();
                self.vbit(VBinOp::Xor, le, ones)
            }
            _ => {
                let oa = self.vbin(Eq, lane, a, a);
                let ob = self.vbin(Eq, lane, b, b);
                self.vbit(VBinOp::And, oa, ob)
            }
        }
    }

    fn scalar_predicate(&mut self, pred: u8, double: bool, a: Value, b: Value) -> Value {
        use FpCmpOp::*;
        let one = self.k32(1);
        match pred & 7 {
            0 => self.fcmp(Eq, double, a, b),
            1 => self.fcmp(Lt, double, a, b),
            2 => self.fcmp(Le, double, a, b),
            3 => {
                let na = self.fcmp(Ne, double, a, a);
                let nb = self.fcmp(Ne, double, b, b);
                self.b.bin(BinOp::Or, Ty::I32, na, nb)
            }
            4 => self.fcmp(Ne, double, a, b),
            5 => {
                let lt = self.fcmp(Lt, double, a, b);
                self.b.bin(BinOp::Xor, Ty::I32, lt, one)
            }
            6 => {
                let le = self.fcmp(Le, double, a, b);
                self.b.bin(BinOp::Xor, Ty::I32, le, one)
            }
            _ => {
                let oa = self.fcmp(Eq, double, a, a);
                let ob = self.fcmp(Eq, double, b, b);
                self.b.bin(BinOp::And, Ty::I32, oa, ob)
            }
        }
    }

    /// Round a float bit pattern per an SSE4.1 rounding immediate (or the
    /// default nearest-even when it defers to MXCSR).
    fn round_op(imm: i64) -> (VLaneUnOp, FpUnOp) {
        if imm & 4 != 0 {
            return (VLaneUnOp::Nearest, FpUnOp::RoundNearest);
        }
        match imm & 3 {
            0 => (VLaneUnOp::Nearest, FpUnOp::RoundNearest),
            1 => (VLaneUnOp::Floor, FpUnOp::Floor),
            2 => (VLaneUnOp::Ceil, FpUnOp::Ceil),
            _ => (VLaneUnOp::Trunc, FpUnOp::Trunc),
        }
    }

    /// Float to integer with x86's "integer indefinite" (`INT_MIN`) for NaN
    /// and out-of-range, where wasm saturates.
    fn f2i(&mut self, x: Value, src_double: bool, dst_wide: bool, truncate: bool) -> Value {
        let x = if truncate {
            x
        } else {
            self.mx(Expr::FpUn {
                op: FpUnOp::RoundNearest,
                double: src_double,
                a: x,
            })
        };
        let r = self.b.def(Expr::FpCvt {
            kind: FpConvert::FpToInt { signed: true },
            src_wide: src_double,
            dst_wide,
            a: x,
        });
        // Out of range above: x >= 2^(bits-1). NaN: x != x.
        let limit_bits: u64 = match (src_double, dst_wide) {
            (true, true) => 0x43e0_0000_0000_0000,  // 2^63 as f64
            (true, false) => 0x41e0_0000_0000_0000, // 2^31 as f64
            (false, true) => 0x5f00_0000,           // 2^63 as f32
            (false, false) => 0x4f00_0000,          // 2^31 as f32
        };
        let limit = self.k(if src_double { Ty::I64 } else { Ty::I32 }, limit_bits);
        let over = self.fcmp(FpCmpOp::Ge, src_double, x, limit);
        let nan = self.fcmp(FpCmpOp::Ne, src_double, x, x);
        let bad = self.b.bin(BinOp::Or, Ty::I32, over, nan);
        let ty = if dst_wide { Ty::I64 } else { Ty::I32 };
        let indefinite = self.k(ty, if dst_wide { 1u64 << 63 } else { 1u64 << 31 });
        self.b.select(bad, indefinite, r, ty)
    }

    /// Widen the low lanes of `v` from `from` to `to`, one doubling at a time.
    fn widen_lanes(&mut self, v: Value, from: Lane, to: Lane, signed: bool) -> Value {
        let mut cur = vlane(from);
        let mut v = v;
        while cur.bytes() < vlane(to).bytes() {
            v = self.b.def(Expr::VCvt {
                op: VCvtOp::ExtendLow { signed },
                from: cur,
                a: v,
                b: None,
            });
            cur = match cur {
                VLane::I8 => VLane::I16,
                VLane::I16 => VLane::I32,
                _ => VLane::I64,
            };
        }
        v
    }

    /// Sign- or zero-extend the even (or odd) 16-bit lanes into 32-bit lanes.
    fn words_to_dwords(&mut self, v: Value, odd: bool, signed: bool) -> Value {
        let zero = self.zero_v();
        let mut lanes = [16u8; 16]; // zero by default
        for k in 0..4 {
            let word = 2 * k + if odd { 1 } else { 0 };
            // Place the word in the high half of dword k; the low half is 0.
            lanes[k * 4 + 2] = (word * 2) as u8;
            lanes[k * 4 + 3] = (word * 2 + 1) as u8;
        }
        let placed = self.shuffle(v, zero, lanes);
        let sixteen = self.k32(16);
        self.b.def(Expr::VShift {
            op: if signed { VShiftOp::ShrS } else { VShiftOp::ShrU },
            lane: VLane::I32,
            a: placed,
            amount: sixteen,
        })
    }

    /// Truncate 32-bit lanes of two vectors to 16 bits (`a` low, `b` high).
    fn dwords_to_words(&mut self, a: Value, b: Value) -> Value {
        let mut lanes = [0u8; 16];
        for k in 0..4 {
            lanes[k * 2] = (k * 4) as u8;
            lanes[k * 2 + 1] = (k * 4 + 1) as u8;
            lanes[8 + k * 2] = (16 + k * 4) as u8;
            lanes[8 + k * 2 + 1] = (16 + k * 4 + 1) as u8;
        }
        self.shuffle(a, b, lanes)
    }

    /// The high 16 bits of each 32-bit lane of two vectors, packed.
    fn dwords_high_words(&mut self, a: Value, b: Value) -> Value {
        let mut lanes = [0u8; 16];
        for k in 0..4 {
            lanes[k * 2] = (k * 4 + 2) as u8;
            lanes[k * 2 + 1] = (k * 4 + 3) as u8;
            lanes[8 + k * 2] = (16 + k * 4 + 2) as u8;
            lanes[8 + k * 2 + 1] = (16 + k * 4 + 3) as u8;
        }
        self.shuffle(a, b, lanes)
    }

    fn set_compare_flags(&mut self, lt: Value, eq: Value, unord: Value) {
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

    // -- the SSE entry point --------------------------------------------------------

    pub(crate) fn sse(&mut self, i: &Inst) -> Flow {
        if i.mmx {
            self.mmx = true;
            let f = self.mmx_op(i);
            self.mmx = false;
            return f;
        }
        self.sse_op(i)
    }

    /// An MMX instruction: an `mm` register is a vector whose upper half is
    /// zero and an `m64` is an 8-byte load or store, so every lane-wise op is
    /// the SSE one. The ops whose result depends on the vector's width are
    /// here: their operands are rearranged so the 128-bit form computes the
    /// 64-bit one in its low half.
    ///
    /// Not modelled: the x87 registers' aliasing of `mm0`–`mm7` (an MMX
    /// instruction does not empty or fill the x87 stack, and FSAVE does not
    /// store `mm`), which code that uses the two as separate units, with
    /// `emms` between, cannot observe.
    fn mmx_op(&mut self, i: &Inst) -> Flow {
        let vl = vlane(i.lane);
        match i.op {
            Op::Movd => match i.op0() {
                Operand::Reg(Reg::Mm(_)) => {
                    let w = i.width;
                    let v = self.rd(i.op1(), w);
                    let z = self.zero_v();
                    let r = self.replace(z, 0, w.bytes(), v);
                    self.wr_x(i.op0(), r);
                }
                d => {
                    let s = self.rd_x(i.op1());
                    let w = i.width;
                    let v = self.extract(s, 0, w.bytes(), false, ty_of(w));
                    self.wr(d, w, v);
                }
            },
            Op::Pshufw => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let s = self.rd_x(i.op1());
                let mut sel: Vec<usize> = (0..8).collect();
                for k in 0..4 {
                    sel[k] = ((imm >> (2 * k)) & 3) as usize;
                }
                let r = self.shuffle(s, s, lane_shuffle(2, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Punpckl => {
                // The low halves: an m32 source.
                let a = self.rd_x(i.op0());
                let b = self.rd_x_n(i.op1(), 4);
                let r = self.shuffle(a, b, unpack_lanes(vl.bytes() as usize, false));
                self.wr_x(i.op0(), r);
            }
            Op::Punpckh => {
                // The high halves of two 64-bit values, interleaved: the low
                // interleave of each moved down by four bytes.
                let (a, b) = self.packed_ops(i);
                let z = self.zero_v();
                let hi = [4, 5, 6, 7, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16];
                let a = self.shuffle(a, z, hi);
                let b = self.shuffle(b, z, hi);
                let r = self.shuffle(a, b, unpack_lanes(vl.bytes() as usize, false));
                self.wr_x(i.op0(), r);
            }
            Op::Packss | Op::Packus | Op::Phadd | Op::Phsub | Op::Phadds | Op::Phsubs => {
                // Both 64-bit operands side by side as one vector, used for
                // both: the 128-bit form's low half is then the 64-bit result.
                let (a, b) = self.packed_ops(i);
                let c = self.shuffle(a, b, [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23]);
                let r = match i.op {
                    Op::Packss | Op::Packus => self.b.def(Expr::VCvt {
                        op: VCvtOp::NarrowSat {
                            signed: i.op == Op::Packss,
                        },
                        from: vl,
                        a: c,
                        b: Some(c),
                    }),
                    _ => {
                        let lb = vl.bytes() as usize;
                        let n = 16 / lb;
                        let even: Vec<usize> = (0..n).map(|k| 2 * k).collect();
                        let odd: Vec<usize> = (0..n).map(|k| 2 * k + 1).collect();
                        let e = self.shuffle(c, c, lane_shuffle(lb, &even));
                        let o = self.shuffle(c, c, lane_shuffle(lb, &odd));
                        let op = match i.op {
                            Op::Phadd => VLaneBinOp::Add,
                            Op::Phsub => VLaneBinOp::Sub,
                            Op::Phadds => VLaneBinOp::AddSatS,
                            _ => VLaneBinOp::SubSatS,
                        };
                        self.vbin(op, vl, e, o)
                    }
                };
                self.wr_x(i.op0(), r);
            }
            Op::Pshufb => {
                // Indices select among eight bytes; bit 7 still zeroes.
                let (a, idx) = self.packed_ops(i);
                let m = self.k32(0x87);
                let mv = self.vsplat(m, 1);
                let idx = self.vbit(VBinOp::And, idx, mv);
                let r = self.b.def(Expr::VSwizzle { a, idx });
                self.wr_x(i.op0(), r);
            }
            Op::Palignr => {
                // (dst:src) >> 8n, the low 64 bits.
                let Operand::Imm(n) = i.op2() else { return self.illegal() };
                let (d, s) = self.packed_ops(i);
                let n = n as usize;
                let t = self.shuffle(s, d, [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23]);
                let z = self.zero_v();
                let mut lanes = [16u8; 16];
                for k in 0..8 {
                    if k + n < 16 {
                        lanes[k] = (k + n) as u8;
                    }
                }
                let r = self.shuffle(t, z, lanes);
                self.wr_x(i.op0(), r);
            }
            Op::Pextr | Op::Pinsr => {
                // Four words, not eight.
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let mut j = i.clone();
                j.ops[2] = Some(Operand::Imm(imm & 3));
                return self.sse_op(&j);
            }
            Op::Maskmov => {
                // Bytes of mm1 whose mask byte (mm2) has its top bit set, to
                // [ds:(e/r)di]: read, merge and write the eight bytes.
                let v = self.rd_x(i.op0());
                let m = self.rd_x(i.op1());
                let z = self.zero_v();
                let sel = self.vbin(VLaneBinOp::LtS, VLane::I8, m, z);
                let mut at = gex_isa_x86::Mem::abs(0);
                at.base = Some(RDI_REG);
                let at = Operand::Mem(at);
                let old = self.rd_x(at);
                let r = self.b.def(Expr::VBitSelect { mask: sel, a: v, b: old });
                self.wr_x(at, r);
            }
            Op::CvtMmx { truncate } => return self.cvt_mmx(i, truncate),
            _ => return self.sse_op(i),
        }
        Flow::Next
    }

    /// The conversions between `mm` integer pairs and `xmm` floats.
    fn cvt_mmx(&mut self, i: &Inst, truncate: bool) -> Flow {
        if let Operand::Reg(Reg::Mm(_)) = i.op0() {
            // cvt[t]ps2pi / cvt[t]pd2pi: cvt[t]p?2dq's low two lanes. The
            // source is m64 for floats and m128 for doubles.
            let mut j = i.clone();
            j.op = Op::Cvtp2dq { truncate };
            j.mmx = false;
            self.mmx = i.lane != Lane::F64;
            return self.sse_op(&j);
        }
        let s = self.rd_x_n(i.op1(), 8);
        if i.lane == Lane::F64 {
            // cvtpi2pd: both doubles.
            let r = self.b.def(Expr::VCvt {
                op: VCvtOp::IntToFp { signed: true },
                from: VLane::F64,
                a: s,
                b: None,
            });
            self.wr_x(i.op0(), r);
        } else {
            // cvtpi2ps: the low two floats; the upper two are kept.
            let c = self.mx(Expr::VCvt {
                op: VCvtOp::IntToFp { signed: true },
                from: VLane::I32,
                a: s,
                b: None,
            });
            let d = self.rd_x(i.op0());
            let r = self.shuffle(c, d, [0, 1, 2, 3, 4, 5, 6, 7, 24, 25, 26, 27, 28, 29, 30, 31]);
            self.wr_x(i.op0(), r);
        }
        Flow::Next
    }

    fn sse_op(&mut self, i: &Inst) -> Flow {
        let lane = i.lane;
        let vl = vlane(lane);
        let double = lane == Lane::F64;
        match i.op {
            Op::Movaps | Op::Movups => {
                let v = self.rd_x(i.op1());
                self.wr_x(i.op0(), v);
            }
            Op::Movss | Op::Movsd => {
                let bytes = if i.op == Op::Movsd { 8 } else { 4 };
                match (i.op0(), i.op1()) {
                    (Operand::Reg(Reg::Xmm(_)), Operand::Mem(_)) => {
                        // Load: zero the upper lanes.
                        let v = self.rd_x_n(i.op1(), bytes);
                        self.wr_x(i.op0(), v);
                    }
                    (Operand::Mem(_), _) => {
                        let v = self.rd_x(i.op1());
                        self.wr_x_n(i.op0(), v, bytes);
                    }
                    _ => {
                        // Register to register merges the low lane.
                        let d = self.rd_x(i.op0());
                        let s = self.rd_x(i.op1());
                        let l = if bytes == 8 { Lane::F64 } else { Lane::F32 };
                        let sv = self.scalar_of(s, l);
                        let r = self.with_scalar(d, l, sv);
                        self.wr_x(i.op0(), r);
                    }
                }
            }
            Op::Movd => {
                let w = i.width;
                let lb = w.bytes();
                match i.op0() {
                    Operand::Reg(Reg::Xmm(_)) => {
                        let v = self.rd(i.op1(), w);
                        let z = self.zero_v();
                        let r = self.replace(z, 0, lb, v);
                        self.wr_x(i.op0(), r);
                    }
                    d => {
                        let s = self.rd_x(i.op1());
                        let v = self.extract(s, 0, lb, false, ty_of(w));
                        self.wr(d, w, v);
                    }
                }
            }
            Op::Movq => match i.op0() {
                Operand::Reg(Reg::Xmm(_)) => {
                    let s = self.rd_x_n(i.op1(), 8);
                    let lo = self.extract(s, 0, 8, false, Ty::I64);
                    let z = self.zero_v();
                    let r = self.replace(z, 0, 8, lo);
                    self.wr_x(i.op0(), r);
                }
                d => {
                    let s = self.rd_x(i.op1());
                    self.wr_x_n(d, s, 8);
                }
            },
            Op::Movlps => match i.op0() {
                Operand::Reg(Reg::Xmm(_)) => {
                    let d = self.rd_x(i.op0());
                    let s = self.rd_x_n(i.op1(), 8);
                    let mut lanes = [0u8; 16];
                    for k in 0..8 {
                        lanes[k] = 16 + k as u8; // low from src
                        lanes[8 + k] = 8 + k as u8; // high from dst
                    }
                    let r = self.shuffle(d, s, lanes);
                    self.wr_x(i.op0(), r);
                }
                d => {
                    let s = self.rd_x(i.op1());
                    self.wr_x_n(d, s, 8);
                }
            },
            Op::Movhps => match i.op0() {
                Operand::Reg(Reg::Xmm(_)) => {
                    let d = self.rd_x(i.op0());
                    let s = self.rd_x_n(i.op1(), 8);
                    let mut lanes = [0u8; 16];
                    for k in 0..8 {
                        lanes[k] = k as u8; // low from dst
                        lanes[8 + k] = 16 + k as u8; // high from src low
                    }
                    let r = self.shuffle(d, s, lanes);
                    self.wr_x(i.op0(), r);
                }
                d => {
                    let s = self.rd_x(i.op1());
                    let z = self.zero_v();
                    let mut lanes = [16u8; 16];
                    for k in 0..8 {
                        lanes[k] = 8 + k as u8;
                    }
                    let hi = self.shuffle(s, z, lanes);
                    self.wr_x_n(d, hi, 8);
                }
            },
            Op::Movhlps => {
                let d = self.rd_x(i.op0());
                let s = self.rd_x(i.op1());
                let mut lanes = [0u8; 16];
                for k in 0..8 {
                    lanes[k] = 16 + 8 + k as u8;
                    lanes[8 + k] = 8 + k as u8;
                }
                let r = self.shuffle(d, s, lanes);
                self.wr_x(i.op0(), r);
            }
            Op::Movlhps => {
                let d = self.rd_x(i.op0());
                let s = self.rd_x(i.op1());
                let mut lanes = [0u8; 16];
                for k in 0..8 {
                    lanes[k] = k as u8;
                    lanes[8 + k] = 16 + k as u8;
                }
                let r = self.shuffle(d, s, lanes);
                self.wr_x(i.op0(), r);
            }
            Op::Movmsk => {
                let s = self.rd_x(i.op1());
                let m = self.b.def(Expr::VBitmask {
                    a: s,
                    lane: if double { VLane::I64 } else { VLane::I32 },
                });
                self.wr(i.op0(), Width::D, m);
            }
            Op::Pmovmskb => {
                let s = self.rd_x(i.op1());
                let m = self.b.def(Expr::VBitmask {
                    a: s,
                    lane: VLane::I8,
                });
                self.wr(i.op0(), Width::D, m);
            }
            Op::Movnti => {
                let v = self.rd(i.op1(), i.width);
                self.wr(i.op0(), i.width, v);
            }
            Op::Movsldup | Op::Movshdup => {
                let s = self.rd_x(i.op1());
                let sel: [usize; 4] = if i.op == Op::Movsldup { [0, 0, 2, 2] } else { [1, 1, 3, 3] };
                let r = self.shuffle(s, s, lane_shuffle(4, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Movddup => {
                let s = self.rd_x_n(i.op1(), 8);
                let r = self.shuffle(s, s, lane_shuffle(8, &[0, 0]));
                self.wr_x(i.op0(), r);
            }
            Op::Maskmov => {
                // maskmovdqu: bytes of xmm1 whose mask byte (xmm2) has its
                // top bit set, to [ds:(e/r)di]; read, merge and write the
                // sixteen bytes, as maskmovq does its eight.
                let v = self.rd_x(i.op0());
                let m = self.rd_x(i.op1());
                let z = self.zero_v();
                let sel = self.vbin(VLaneBinOp::LtS, VLane::I8, m, z);
                let mut at = gex_isa_x86::Mem::abs(0);
                at.base = Some(RDI_REG);
                let at = Operand::Mem(at);
                let old = self.rd_x(at);
                let r = self.b.def(Expr::VBitSelect { mask: sel, a: v, b: old });
                self.wr_x(at, r);
            }

            // -- float arithmetic -------------------------------------------------------
            Op::Addp => self.farith(i, VLaneBinOp::Add, FpBinOp::Add),
            Op::Subp => self.farith(i, VLaneBinOp::Sub, FpBinOp::Sub),
            Op::Mulp => self.farith(i, VLaneBinOp::Mul, FpBinOp::Mul),
            Op::Divp => self.farith(i, VLaneBinOp::Div, FpBinOp::Div),
            Op::Minp => self.minmax(i, true),
            Op::Maxp => self.minmax(i, false),
            Op::Sqrtp | Op::Rsqrtp | Op::Rcpp => {
                if i.scalar {
                    let dv = self.rd_x(i.op0());
                    let a = self.rd_scalar_src(i.op1(), lane);
                    let r = match i.op {
                        Op::Sqrtp => self.mx(Expr::FpUn {
                            op: FpUnOp::Sqrt,
                            double,
                            a,
                        }),
                        Op::Rsqrtp => {
                            let s = self.b.def(Expr::FpUn {
                                op: FpUnOp::Sqrt,
                                double,
                                a,
                            });
                            let one = self.k32(0x3f80_0000);
                            self.fbin(FpBinOp::Div, false, one, s)
                        }
                        _ => {
                            let one = self.k32(0x3f80_0000);
                            self.fbin(FpBinOp::Div, false, one, a)
                        }
                    };
                    let r = self.with_scalar(dv, lane, r);
                    self.wr_x(i.op0(), r);
                } else {
                    let a = self.rd_x(i.op1());
                    let r = match i.op {
                        Op::Sqrtp => self.vun(VLaneUnOp::Sqrt, vl, a),
                        Op::Rsqrtp => {
                            let s = self.vun(VLaneUnOp::Sqrt, vl, a);
                            let one = self.k32(0x3f80_0000);
                            let ones = self.vsplat(one, 4);
                            self.vbin(VLaneBinOp::Div, vl, ones, s)
                        }
                        _ => {
                            let one = self.k32(0x3f80_0000);
                            let ones = self.vsplat(one, 4);
                            self.vbin(VLaneBinOp::Div, vl, ones, a)
                        }
                    };
                    self.wr_x(i.op0(), r);
                }
            }
            Op::Andp | Op::Pand => {
                let (a, b) = self.packed_ops(i);
                let r = self.vbit(VBinOp::And, a, b);
                self.wr_x(i.op0(), r);
            }
            Op::Andnp | Op::Pandn => {
                // dst = ~dst & src
                let (a, b) = self.packed_ops(i);
                let r = self.vbit(VBinOp::AndNot, b, a);
                self.wr_x(i.op0(), r);
            }
            Op::Orp | Op::Por => {
                let (a, b) = self.packed_ops(i);
                let r = self.vbit(VBinOp::Or, a, b);
                self.wr_x(i.op0(), r);
            }
            Op::Xorp | Op::Pxor => {
                let (a, b) = self.packed_ops(i);
                let r = self.vbit(VBinOp::Xor, a, b);
                self.wr_x(i.op0(), r);
            }
            Op::Cmpp => {
                let Operand::Imm(pred) = i.op2() else { return self.illegal() };
                if i.scalar {
                    let dv = self.rd_x(i.op0());
                    let a = self.scalar_of(dv, lane);
                    let b = self.rd_scalar_src(i.op1(), lane);
                    let c = self.scalar_predicate(pred as u8, double, a, b);
                    let ty = if double { Ty::I64 } else { Ty::I32 };
                    let ones = self.k(ty, u64::MAX);
                    let zero = self.k(ty, 0);
                    let m = self.b.select(c, ones, zero, ty);
                    let r = self.with_scalar(dv, lane, m);
                    self.wr_x(i.op0(), r);
                } else {
                    let (a, b) = self.packed_ops(i);
                    let r = self.packed_predicate(pred as u8, vl, a, b);
                    self.wr_x(i.op0(), r);
                }
            }
            Op::Comis | Op::Ucomis => {
                let dv = self.rd_x(i.op0());
                let a = self.scalar_of(dv, lane);
                let b = self.rd_scalar_src(i.op1(), lane);
                let lt = self.fcmp(FpCmpOp::Lt, double, a, b);
                let eq = self.fcmp(FpCmpOp::Eq, double, a, b);
                let na = self.fcmp(FpCmpOp::Ne, double, a, a);
                let nb = self.fcmp(FpCmpOp::Ne, double, b, b);
                let unord = self.b.bin(BinOp::Or, Ty::I32, na, nb);
                self.set_compare_flags(lt, eq, unord);
            }
            Op::Haddp | Op::Hsubp => {
                let (a, b) = self.packed_ops(i);
                let lb = vl.bytes() as usize;
                let n = 16 / lb;
                let even: Vec<usize> = (0..n).map(|k| 2 * k).collect();
                let odd: Vec<usize> = (0..n).map(|k| 2 * k + 1).collect();
                let e = self.shuffle(a, b, lane_shuffle(lb, &even));
                let o = self.shuffle(a, b, lane_shuffle(lb, &odd));
                let op = if i.op == Op::Haddp { VLaneBinOp::Add } else { VLaneBinOp::Sub };
                let r = self.vbin(op, vl, e, o);
                self.wr_x(i.op0(), r);
            }
            Op::Addsubp => {
                let (a, b) = self.packed_ops(i);
                let add = self.vbin(VLaneBinOp::Add, vl, a, b);
                let sub = self.vbin(VLaneBinOp::Sub, vl, a, b);
                // Even lanes subtract, odd lanes add.
                let lb = vl.bytes() as usize;
                let mut mask = [0u8; 16];
                for k in 0..16 {
                    if (k / lb) % 2 == 1 {
                        mask[k] = 0xff;
                    }
                }
                let m = self.vconst(mask);
                let r = self.b.def(Expr::VBitSelect { mask: m, a: add, b: sub });
                self.wr_x(i.op0(), r);
            }
            Op::Roundp => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let (vop, sop) = Self::round_op(imm);
                // Bit 2: MXCSR's rounding control decides.
                let by_mx = imm & 4 != 0;
                if i.scalar {
                    let dv = self.rd_x(i.op0());
                    let a = self.rd_scalar_src(i.op1(), lane);
                    let e = Expr::FpUn { op: sop, double, a };
                    let r = if by_mx { self.mx(e) } else { self.b.def(e) };
                    let r = self.with_scalar(dv, lane, r);
                    self.wr_x(i.op0(), r);
                } else {
                    let a = self.rd_x(i.op1());
                    let r = if by_mx { self.vun_mx(vop, vl, a) } else { self.vun(vop, vl, a) };
                    self.wr_x(i.op0(), r);
                }
            }
            Op::Blendp | Op::Pblendw => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let (a, b) = self.packed_ops(i);
                let lb = vl.bytes() as usize;
                let n = 16 / lb;
                let sel: Vec<usize> = (0..n)
                    .map(|k| if (imm >> k) & 1 == 1 { n + k } else { k })
                    .collect();
                let r = self.shuffle(a, b, lane_shuffle(lb, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Blendvp | Op::Pblendvb => {
                // Select by the sign bit of each lane of xmm0.
                let (a, b) = self.packed_ops(i);
                let m = self.b.read(GuestReg::V128(0));
                let sel_lane = match i.op {
                    Op::Pblendvb => VLane::I8,
                    _ => if double { VLane::I64 } else { VLane::I32 },
                };
                let z = self.zero_v();
                let mask = self.vbin(VLaneBinOp::LtS, sel_lane, m, z);
                let r = self.b.def(Expr::VBitSelect { mask, a: b, b: a });
                self.wr_x(i.op0(), r);
            }
            Op::Shufp => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let (a, b) = self.packed_ops(i);
                let sel: Vec<usize> = if double {
                    vec![(imm & 1) as usize, 2 + ((imm >> 1) & 1) as usize]
                } else {
                    vec![
                        (imm & 3) as usize,
                        ((imm >> 2) & 3) as usize,
                        4 + ((imm >> 4) & 3) as usize,
                        4 + ((imm >> 6) & 3) as usize,
                    ]
                };
                let r = self.shuffle(a, b, lane_shuffle(vl.bytes() as usize, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Unpcklp | Op::Punpckl => {
                let (a, b) = self.packed_ops(i);
                let r = self.shuffle(a, b, unpack_lanes(vl.bytes() as usize, false));
                self.wr_x(i.op0(), r);
            }
            Op::Unpckhp | Op::Punpckh => {
                let (a, b) = self.packed_ops(i);
                let r = self.shuffle(a, b, unpack_lanes(vl.bytes() as usize, true));
                self.wr_x(i.op0(), r);
            }

            // -- conversions ---------------------------------------------------------------
            Op::Cvtsi2s => {
                let w = i.width;
                let v = self.rd(i.op1(), w);
                let f = self.mx(Expr::FpCvt {
                    kind: FpConvert::IntToFp { signed: true },
                    src_wide: w == Width::Q,
                    dst_wide: double,
                    a: v,
                });
                let dv = self.rd_x(i.op0());
                let r = self.with_scalar(dv, lane, f);
                self.wr_x(i.op0(), r);
            }
            Op::Cvts2si { truncate } => {
                let w = i.width;
                let x = self.rd_scalar_src(i.op1(), lane);
                let r = self.f2i(x, double, w == Width::Q, truncate);
                self.wr(i.op0(), w, r);
            }
            Op::Cvts2s => {
                // lane is the SOURCE type.
                let x = self.rd_scalar_src(i.op1(), lane);
                let r = self.mx(Expr::FpCvt {
                    kind: FpConvert::FpToFp,
                    src_wide: double,
                    dst_wide: !double,
                    a: x,
                });
                let dv = self.rd_x(i.op0());
                let dst_lane = if double { Lane::F32 } else { Lane::F64 };
                let r = self.with_scalar(dv, dst_lane, r);
                self.wr_x(i.op0(), r);
            }
            Op::Cvtp2p => {
                let s = self.rd_x_n(i.op1(), if double { 16 } else { 8 });
                let r = self.mx(Expr::VCvt {
                    op: if double { VCvtOp::DemoteF64 } else { VCvtOp::PromoteF32 },
                    from: vl,
                    a: s,
                    b: None,
                });
                self.wr_x(i.op0(), r);
            }
            Op::Cvtdq2p => {
                if double {
                    let s = self.rd_x_n(i.op1(), 8);
                    let r = self.b.def(Expr::VCvt {
                        op: VCvtOp::IntToFp { signed: true },
                        // The emitter's table keys the `f64x2.convert_low_i32x4_s`
                        // form on a non-integer source lane.
                        from: VLane::F64,
                        a: s,
                        b: None,
                    });
                    self.wr_x(i.op0(), r);
                } else {
                    let s = self.rd_x(i.op1());
                    let r = self.mx(Expr::VCvt {
                        op: VCvtOp::IntToFp { signed: true },
                        from: VLane::I32,
                        a: s,
                        b: None,
                    });
                    self.wr_x(i.op0(), r);
                }
            }
            Op::Cvtp2dq { truncate } => {
                let s = self.rd_x(i.op1());
                let s = if truncate {
                    s
                } else {
                    self.vun_mx(VLaneUnOp::Nearest, vl, s)
                };
                let r = self.b.def(Expr::VCvt {
                    op: VCvtOp::FpToIntSat { signed: true },
                    from: vl,
                    a: s,
                    b: None,
                });
                // wasm saturates; x86 answers the "integer indefinite"
                // (INT_MIN) for a NaN or an out-of-range lane, in both
                // directions. Positive overflow is the one case the two
                // disagree on (saturation gives INT_MAX), plus NaN (0).
                let r = if vl == VLane::F32 {
                    let limit = self.vconst(0x4f00_0000u32.to_le_bytes().repeat(4).try_into().unwrap());
                    let over = self.vbin(VLaneBinOp::GeS, VLane::F32, s, limit);
                    let nan = self.vbin(VLaneBinOp::Ne, VLane::F32, s, s);
                    let bad = self.vbit(VBinOp::Or, over, nan);
                    let indefinite = self.vconst(0x8000_0000u32.to_le_bytes().repeat(4).try_into().unwrap());
                    let keep = self.vbit(VBinOp::AndNot, r, bad);
                    let fix = self.vbit(VBinOp::And, indefinite, bad);
                    self.vbit(VBinOp::Or, keep, fix)
                } else {
                    // Doubles: the two results are the low two i32 lanes,
                    // each 64-bit lane's verdict moved down to its i32 lane.
                    let limit = self.vconst(0x41e0_0000_0000_0000u64.to_le_bytes().repeat(2).try_into().unwrap());
                    let over = self.vbin(VLaneBinOp::GeS, VLane::F64, s, limit);
                    let nan = self.vbin(VLaneBinOp::Ne, VLane::F64, s, s);
                    let bad = self.vbit(VBinOp::Or, over, nan);
                    let z = self.zero_v();
                    let bad = self.shuffle(bad, z, [0, 1, 2, 3, 8, 9, 10, 11, 16, 16, 16, 16, 16, 16, 16, 16]);
                    let indefinite = self.vconst(0x8000_0000u32.to_le_bytes().repeat(4).try_into().unwrap());
                    let keep = self.vbit(VBinOp::AndNot, r, bad);
                    let fix = self.vbit(VBinOp::And, indefinite, bad);
                    self.vbit(VBinOp::Or, keep, fix)
                };
                self.wr_x(i.op0(), r);
            }
            Op::CvtMmx { .. } => return self.illegal(),

            // -- packed integer ----------------------------------------------------------------
            Op::Padd => self.pint(i, VLaneBinOp::Add),
            Op::Psub => self.pint(i, VLaneBinOp::Sub),
            Op::Padds => self.pint(i, VLaneBinOp::AddSatS),
            Op::Paddus => self.pint(i, VLaneBinOp::AddSatU),
            Op::Psubs => self.pint(i, VLaneBinOp::SubSatS),
            Op::Psubus => self.pint(i, VLaneBinOp::SubSatU),
            Op::Pmullw | Op::Pmulld => self.pint(i, VLaneBinOp::Mul),
            Op::Pavg => self.pint(i, VLaneBinOp::AvgrU),
            Op::Pmaxu => self.pint(i, VLaneBinOp::MaxU),
            Op::Pminu => self.pint(i, VLaneBinOp::MinU),
            Op::Pmaxs => self.pint(i, VLaneBinOp::MaxS),
            Op::Pmins => self.pint(i, VLaneBinOp::MinS),
            Op::Pcmpeq => self.pint(i, VLaneBinOp::Eq),
            Op::Pcmpgt => self.pint(i, VLaneBinOp::GtS),
            Op::Pmulhw | Op::Pmulhuw => {
                let (a, b) = self.packed_ops(i);
                let signed = i.op == Op::Pmulhw;
                let al = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendLow { signed },
                    from: VLane::I16,
                    a,
                    b: None,
                });
                let bl = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendLow { signed },
                    from: VLane::I16,
                    a: b,
                    b: None,
                });
                let ah = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendHigh { signed },
                    from: VLane::I16,
                    a,
                    b: None,
                });
                let bh = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendHigh { signed },
                    from: VLane::I16,
                    a: b,
                    b: None,
                });
                let lo = self.vbin(VLaneBinOp::Mul, VLane::I32, al, bl);
                let hi = self.vbin(VLaneBinOp::Mul, VLane::I32, ah, bh);
                let r = self.dwords_high_words(lo, hi);
                self.wr_x(i.op0(), r);
            }
            Op::Pmulhrsw => {
                let (a, b) = self.packed_ops(i);
                let mut halves = Vec::new();
                for high in [false, true] {
                    let op = if high {
                        VCvtOp::ExtendHigh { signed: true }
                    } else {
                        VCvtOp::ExtendLow { signed: true }
                    };
                    let ae = self.b.def(Expr::VCvt { op, from: VLane::I16, a, b: None });
                    let be = self.b.def(Expr::VCvt { op, from: VLane::I16, a: b, b: None });
                    let p = self.vbin(VLaneBinOp::Mul, VLane::I32, ae, be);
                    let s14 = self.k32(14);
                    let p = self.b.def(Expr::VShift {
                        op: VShiftOp::ShrS,
                        lane: VLane::I32,
                        a: p,
                        amount: s14,
                    });
                    let one = self.k32(1);
                    let ones = self.vsplat(one, 4);
                    let p = self.vbin(VLaneBinOp::Add, VLane::I32, p, ones);
                    let p = self.b.def(Expr::VShift {
                        op: VShiftOp::ShrS,
                        lane: VLane::I32,
                        a: p,
                        amount: one,
                    });
                    halves.push(p);
                }
                let r = self.dwords_to_words(halves[0], halves[1]);
                self.wr_x(i.op0(), r);
            }
            Op::Pmuludq | Op::Pmuldq => {
                let (a, b) = self.packed_ops(i);
                let signed = i.op == Op::Pmuldq;
                // Bring dword lanes 0 and 2 to 0 and 1, then widen and multiply.
                let sel = lane_shuffle(4, &[0, 2, 0, 2]);
                let a2 = self.shuffle(a, a, sel);
                let b2 = self.shuffle(b, b, sel);
                let ae = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendLow { signed },
                    from: VLane::I32,
                    a: a2,
                    b: None,
                });
                let be = self.b.def(Expr::VCvt {
                    op: VCvtOp::ExtendLow { signed },
                    from: VLane::I32,
                    a: b2,
                    b: None,
                });
                let r = self.vbin(VLaneBinOp::Mul, VLane::I64, ae, be);
                self.wr_x(i.op0(), r);
            }
            Op::Pmaddwd => {
                let (a, b) = self.packed_ops(i);
                let ae = self.words_to_dwords(a, false, true);
                let be = self.words_to_dwords(b, false, true);
                let ao = self.words_to_dwords(a, true, true);
                let bo = self.words_to_dwords(b, true, true);
                let pe = self.vbin(VLaneBinOp::Mul, VLane::I32, ae, be);
                let po = self.vbin(VLaneBinOp::Mul, VLane::I32, ao, bo);
                let r = self.vbin(VLaneBinOp::Add, VLane::I32, pe, po);
                self.wr_x(i.op0(), r);
            }
            Op::Pmaddubsw => {
                // u8 (dst) * s8 (src), adjacent pairs summed with saturation.
                let (a, b) = self.packed_ops(i);
                let mut prods = Vec::new();
                for high in [false, true] {
                    let (ua, sb) = if high {
                        (VCvtOp::ExtendHigh { signed: false }, VCvtOp::ExtendHigh { signed: true })
                    } else {
                        (VCvtOp::ExtendLow { signed: false }, VCvtOp::ExtendLow { signed: true })
                    };
                    let ae = self.b.def(Expr::VCvt { op: ua, from: VLane::I8, a, b: None });
                    let be = self.b.def(Expr::VCvt { op: sb, from: VLane::I8, a: b, b: None });
                    prods.push(self.vbin(VLaneBinOp::Mul, VLane::I16, ae, be));
                }
                // Pairwise saturating add of adjacent words: even + odd.
                let mut halves = Vec::new();
                for p in prods {
                    let e = self.words_to_dwords(p, false, true);
                    let o = self.words_to_dwords(p, true, true);
                    halves.push(self.vbin(VLaneBinOp::Add, VLane::I32, e, o));
                }
                let r = self.b.def(Expr::VCvt {
                    op: VCvtOp::NarrowSat { signed: true },
                    from: VLane::I32,
                    a: halves[0],
                    b: Some(halves[1]),
                });
                self.wr_x(i.op0(), r);
            }
            Op::Psadbw => {
                let (a, b) = self.packed_ops(i);
                let mx = self.vbin(VLaneBinOp::MaxU, VLane::I8, a, b);
                let mn = self.vbin(VLaneBinOp::MinU, VLane::I8, a, b);
                let d = self.vbin(VLaneBinOp::Sub, VLane::I8, mx, mn);
                let s16 = self.b.def(Expr::VUn {
                    op: VUnOp::ExtAddPairwiseU8,
                    a: d,
                });
                let s32 = self.b.def(Expr::VUn {
                    op: VUnOp::ExtAddPairwiseU16,
                    a: s16,
                });
                let z = self.zero_v();
                let e = self.shuffle(s32, z, [0, 1, 2, 3, 16, 16, 16, 16, 8, 9, 10, 11, 16, 16, 16, 16]);
                let o = self.shuffle(s32, z, [4, 5, 6, 7, 16, 16, 16, 16, 12, 13, 14, 15, 16, 16, 16, 16]);
                let r = self.vbin(VLaneBinOp::Add, VLane::I64, e, o);
                self.wr_x(i.op0(), r);
            }
            Op::Psll | Op::Psrl | Op::Psra => {
                let a = self.rd_x(i.op0());
                let bits = vl.bytes() * 8;
                let shift_op = match i.op {
                    Op::Psll => VShiftOp::Shl,
                    Op::Psrl => VShiftOp::ShrU,
                    _ => VShiftOp::ShrS,
                };
                let r = match i.op1() {
                    Operand::Imm(n) => {
                        let n = n as u32;
                        if n >= bits {
                            if i.op == Op::Psra {
                                let amt = self.k32(bits - 1);
                                self.b.def(Expr::VShift {
                                    op: shift_op,
                                    lane: vl,
                                    a,
                                    amount: amt,
                                })
                            } else {
                                self.zero_v()
                            }
                        } else {
                            let amt = self.k32(n);
                            self.b.def(Expr::VShift {
                                op: shift_op,
                                lane: vl,
                                a,
                                amount: amt,
                            })
                        }
                    }
                    src => {
                        let s = self.rd_x_n(src, 8);
                        let cnt = self.extract(s, 0, 8, false, Ty::I64);
                        let big = self.k64(bits as u64);
                        let over = self.b.cmp(CmpOp::GeU, Ty::I64, cnt, big);
                        let cap = self.k64((bits - 1) as u64);
                        let amt = self.b.select(over, cap, cnt, Ty::I64);
                        let amt = self.b.wrap(amt);
                        let shifted = self.b.def(Expr::VShift {
                            op: shift_op,
                            lane: vl,
                            a,
                            amount: amt,
                        });
                        if i.op == Op::Psra {
                            shifted
                        } else {
                            // Shift counts at or past the width clear the lane.
                            let zero = self.k32(0);
                            let allones = self.k32(u32::MAX);
                            let keep = self.b.select(over, zero, allones, Ty::I32);
                            let keepv = self.vsplat(keep, 4);
                            self.vbit(VBinOp::And, shifted, keepv)
                        }
                    }
                };
                self.wr_x(i.op0(), r);
            }
            Op::Pslldq | Op::Psrldq => {
                let Operand::Imm(n) = i.op1() else { return self.illegal() };
                let a = self.rd_x(i.op0());
                let z = self.zero_v();
                let n = n as usize;
                let r = if n >= 16 {
                    z
                } else {
                    let mut lanes = [16u8; 16];
                    for k in 0..16 {
                        if i.op == Op::Pslldq {
                            if k >= n {
                                lanes[k] = (k - n) as u8;
                            }
                        } else if k + n < 16 {
                            lanes[k] = (k + n) as u8;
                        }
                    }
                    self.shuffle(a, z, lanes)
                };
                self.wr_x(i.op0(), r);
            }
            Op::Packss | Op::Packus => {
                let (a, b) = self.packed_ops(i);
                let r = self.b.def(Expr::VCvt {
                    op: VCvtOp::NarrowSat {
                        signed: i.op == Op::Packss,
                    },
                    from: vl,
                    a,
                    b: Some(b),
                });
                self.wr_x(i.op0(), r);
            }
            Op::Pshufd => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let s = self.rd_x(i.op1());
                let sel: Vec<usize> = (0..4).map(|k| ((imm >> (2 * k)) & 3) as usize).collect();
                let r = self.shuffle(s, s, lane_shuffle(4, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Pshufhw | Op::Pshuflw => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let s = self.rd_x(i.op1());
                let base = if i.op == Op::Pshufhw { 4 } else { 0 };
                let mut sel: Vec<usize> = (0..8).collect();
                for k in 0..4 {
                    sel[base + k] = base + ((imm >> (2 * k)) & 3) as usize;
                }
                let r = self.shuffle(s, s, lane_shuffle(2, &sel));
                self.wr_x(i.op0(), r);
            }
            Op::Pshufb => {
                let (a, idx) = self.packed_ops(i);
                // Bit 7 set selects zero: keep it so the index is out of range.
                let m = self.k32(0x8f);
                let mv = self.vsplat(m, 1);
                let idx = self.vbit(VBinOp::And, idx, mv);
                let r = self.b.def(Expr::VSwizzle { a, idx });
                self.wr_x(i.op0(), r);
            }
            Op::Palignr => {
                let Operand::Imm(n) = i.op2() else { return self.illegal() };
                let (d, s) = self.packed_ops(i);
                let n = n as usize;
                let r = if n >= 32 {
                    self.zero_v()
                } else if n >= 16 {
                    let z = self.zero_v();
                    let mut lanes = [16u8; 16];
                    for k in 0..16 {
                        if k + n - 16 < 16 {
                            lanes[k] = (k + n - 16) as u8;
                        }
                    }
                    self.shuffle(d, z, lanes)
                } else {
                    let mut lanes = [0u8; 16];
                    for k in 0..16 {
                        lanes[k] = (k + n) as u8; // src is a (0..16), dst is b (16..32)
                    }
                    self.shuffle(s, d, lanes)
                };
                self.wr_x(i.op0(), r);
            }
            Op::Pabs => {
                let s = self.rd_x(i.op1());
                let r = self.vun(VLaneUnOp::Abs, vl, s);
                self.wr_x(i.op0(), r);
            }
            Op::Phadd | Op::Phsub | Op::Phadds | Op::Phsubs => {
                let (a, b) = self.packed_ops(i);
                let lb = vl.bytes() as usize;
                let n = 16 / lb;
                let even: Vec<usize> = (0..n).map(|k| 2 * k).collect();
                let odd: Vec<usize> = (0..n).map(|k| 2 * k + 1).collect();
                let e = self.shuffle(a, b, lane_shuffle(lb, &even));
                let o = self.shuffle(a, b, lane_shuffle(lb, &odd));
                let op = match i.op {
                    Op::Phadd => VLaneBinOp::Add,
                    Op::Phsub => VLaneBinOp::Sub,
                    Op::Phadds => VLaneBinOp::AddSatS,
                    _ => VLaneBinOp::SubSatS,
                };
                let r = self.vbin(op, vl, e, o);
                self.wr_x(i.op0(), r);
            }
            Op::Psign => {
                // dst lanes negated where src < 0, zeroed where src == 0.
                let (a, b) = self.packed_ops(i);
                let z = self.zero_v();
                let neg = self.vun(VLaneUnOp::Neg, vl, a);
                let is_neg = self.vbin(VLaneBinOp::LtS, vl, b, z);
                let r = self.b.def(Expr::VBitSelect { mask: is_neg, a: neg, b: a });
                let is_zero = self.vbin(VLaneBinOp::Eq, vl, b, z);
                let r = self.vbit(VBinOp::AndNot, r, is_zero);
                self.wr_x(i.op0(), r);
            }
            Op::Ptest => {
                let (a, b) = self.packed_ops(i);
                let and = self.vbit(VBinOp::And, a, b);
                let andn = self.vbit(VBinOp::AndNot, b, a);
                let z = self.zero_v();
                let full = self.k32(0xffff);
                let ea = self.vbin(VLaneBinOp::Eq, VLane::I8, and, z);
                let ma = self.b.def(Expr::VBitmask { a: ea, lane: VLane::I8 });
                let zf = self.b.cmp(CmpOp::Eq, Ty::I32, ma, full);
                let en = self.vbin(VLaneBinOp::Eq, VLane::I8, andn, z);
                let mn = self.b.def(Expr::VBitmask { a: en, lane: VLane::I8 });
                let cf = self.b.cmp(CmpOp::Eq, Ty::I32, mn, full);
                let zero = self.k32(0);
                self.b.write(GuestReg::Z, zf);
                self.b.write(GuestReg::C, cf);
                for f in [GuestReg::N, GuestReg::V, GuestReg::P, GuestReg::A] {
                    self.b.write(f, zero);
                }
                self.flags = FlagSource::Unknown;
            }
            Op::Pextr => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let s = self.rd_x(i.op1());
                let lb = vl.bytes();
                let n = 16 / lb;
                let w = i.width;
                let v = self.extract(s, (imm as u32) % n, lb, false, ty_of(if lb == 8 { Width::Q } else { Width::D }));
                // A register destination is zero-extended to 32 bits.
                let dw = match i.op0() {
                    Operand::Reg(_) => if lb == 8 { Width::Q } else { Width::D },
                    _ => w,
                };
                self.wr(i.op0(), dw, v);
            }
            Op::Pinsr => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let d = self.rd_x(i.op0());
                let lb = vl.bytes();
                let n = 16 / lb;
                // A memory source is read at the lane's width; a register
                // source's low bits are what count.
                let src_w = match i.op1() {
                    Operand::Mem(_) => i.width,
                    _ => if lb == 8 { Width::Q } else { Width::D },
                };
                let v = self.rd(i.op1(), src_w);
                let r = self.replace(d, (imm as u32) % n, lb, v);
                self.wr_x(i.op0(), r);
            }
            Op::Extractps => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let s = self.rd_x(i.op1());
                let v = self.extract(s, (imm as u32) & 3, 4, false, Ty::I32);
                self.wr(i.op0(), Width::D, v);
            }
            Op::Insertps => {
                let Operand::Imm(imm) = i.op2() else { return self.illegal() };
                let d = self.rd_x(i.op0());
                let src_lane = match i.op1() {
                    Operand::Mem(_) => 0,
                    _ => ((imm >> 6) & 3) as u32,
                };
                let s = self.rd_x_n(i.op1(), 4);
                let v = self.extract(s, src_lane, 4, false, Ty::I32);
                let dst_lane = ((imm >> 4) & 3) as u32;
                let r = self.replace(d, dst_lane, 4, v);
                // Zero the lanes named by the low nibble.
                let mut mask = [0xffu8; 16];
                for k in 0..4 {
                    if (imm >> k) & 1 == 1 {
                        for byte in 0..4 {
                            mask[k * 4 + byte] = 0;
                        }
                    }
                }
                let r = if imm & 15 != 0 {
                    let m = self.vconst(mask);
                    self.vbit(VBinOp::And, r, m)
                } else {
                    r
                };
                self.wr_x(i.op0(), r);
            }
            Op::Pmovsx(from, to) | Op::Pmovzx(from, to) => {
                let ratio = to.bytes() / from.bytes();
                let s = self.rd_x_n(i.op1(), 16 / ratio);
                let signed = matches!(i.op, Op::Pmovsx(..));
                let r = self.widen_lanes(s, from, to, signed);
                self.wr_x(i.op0(), r);
            }
            Op::Phminposuw
            | Op::Pcmpistri
            | Op::Pcmpistrm
            | Op::Pcmpestri
            | Op::Pcmpestrm
            | Op::Pclmulqdq
            | Op::Mpsadbw
            | Op::Dpp => return self.illegal(),
            // Every x87 register empty. The mm registers are separate from
            // the x87 stack in gex (see `mmx_op`), so that is all emms does.
            Op::Emms => {
                let z = self.k32(0);
                self.b.write(GuestReg::X87Tags, z);
            }
            _ => return self.illegal(),
        }
        Flow::Next
    }

    /// A lane-wise binary op on packed integers.
    fn pint(&mut self, i: &Inst, op: VLaneBinOp) {
        let (a, b) = self.packed_ops(i);
        let r = self.vbin(op, vlane(i.lane), a, b);
        self.wr_x(i.op0(), r);
    }
}
