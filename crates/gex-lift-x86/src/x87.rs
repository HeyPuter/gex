//! x87 floating point, in 80-bit extended precision.
//!
//! The eight registers live in the context (`ctx::X87`), 16 bytes each in
//! the `fxsave` layout: the 64-bit significand with its explicit integer bit,
//! then the sign and exponent word. They are addressed through
//! `GuestReg::CtxAddr` because `TOP` is dynamic: `st(i)` is physical register
//! `(top + i) & 7`.
//!
//! Moving values is done here: register-to-register copies, `fxch`,
//! `fchs`/`fabs`, `fcmov`, the environment; so are the conversions to and
//! from memory formats (`fld`, `fst`, `fild`, `fist`, the constants), in
//! `x87conv`. The arithmetic the host computes exactly
//! (`gex_kernel::x86::serve_x87` on `gex_kernel::f80`), honouring the
//! precision and rounding control of the control word and raising the
//! exception flags: the lifter hands it an [`crate::x87op`] word through
//! `sysreg_write`. A memory operand travels through the context's
//! `X87_TMP` slot as raw bits, in either direction, so the guest's own
//! loads and stores (and their faults) stay in translated code.

use crate::x87op;
use crate::{FlagSource, Flow, Lifter};
use gex_abi::ctx;
use gex_ir::*;
use gex_isa_x86::{Inst, Op, Operand, Width, X87Mem, X87Src};

/// The status word's C1 bit.
const C1: u32 = 1 << 9;

/// A register's value: (significand, sign and exponent).
type Ext = (Value, Value);

impl<'a> Lifter<'a> {
    pub(crate) fn top(&mut self) -> Value {
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
        let four = self.k32(4);
        let off = self.b.bin(BinOp::Shl, Ty::I32, idx, four);
        let off = self.b.extend(off, false);
        let k = self.k64(ctx::X87 as u64);
        let a = self.b.bin(BinOp::Add, Ty::I64, base, k);
        self.b.bin(BinOp::Add, Ty::I64, a, off)
    }

    fn ld_ext(&mut self, addr: Value, offset: i64) -> Ext {
        let sig = self.b.def(Expr::Load {
            addr,
            offset,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        });
        let se = self.b.def(Expr::Load {
            addr,
            offset: offset + 8,
            sz: MemSz::B2,
            signed: false,
            ty: Ty::I32,
            order: Order::Relaxed,
        });
        (sig, se)
    }

    fn st_ext(&mut self, addr: Value, offset: i64, (sig, se): Ext) {
        self.b.stmt(Stmt::Store {
            addr,
            offset,
            val: sig,
            sz: MemSz::B8,
            order: Order::Relaxed,
        });
        self.b.stmt(Stmt::Store {
            addr,
            offset: offset + 8,
            val: se,
            sz: MemSz::B2,
            order: Order::Relaxed,
        });
    }

    /// `st(i)`.
    pub(crate) fn ld_st(&mut self, i: u8) -> Ext {
        let top = self.top();
        let a = self.st_addr(top, i);
        self.ld_ext(a, 0)
    }

    fn st_st(&mut self, i: u8, v: Ext) {
        let top = self.top();
        let a = self.st_addr(top, i);
        self.st_ext(a, 0, v);
    }

    /// Push `v` onto the stack.
    pub(crate) fn push_st(&mut self, v: Ext) {
        let top = self.top();
        let seven = self.k32(7);
        let ntop = self.b.bin(BinOp::Add, Ty::I32, top, seven);
        let ntop = self.b.bin(BinOp::And, Ty::I32, ntop, seven);
        self.set_top(ntop);
        let a = self.st_addr(ntop, 0);
        self.st_ext(a, 0, v);
        // Mark valid.
        let tags = self.b.read(GuestReg::X87Tags);
        let one = self.k32(1);
        let bit = self.b.bin(BinOp::Shl, Ty::I32, one, ntop);
        let tags = self.b.bin(BinOp::Or, Ty::I32, tags, bit);
        self.b.write(GuestReg::X87Tags, tags);
    }

    pub(crate) fn pop_st(&mut self) {
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

    /// Guest address of the context's `X87_TMP` slot.
    fn x87_tmp(&mut self) -> Value {
        let base = self.b.read(GuestReg::CtxAddr);
        let k = self.k64(ctx::X87_TMP as u64);
        self.b.bin(BinOp::Add, Ty::I64, base, k)
    }

    /// Have the host compute an x87 operation (see [`x87op`]).
    fn x87_host(&mut self, op: u64) {
        let pcv = self.k64(self.pc);
        let id = self.k32(crate::sysreg::X87 as u32);
        let v = self.k64(op);
        self.b.stmt(Stmt::HostCall {
            func: HostFunc::SysRegWrite,
            args: vec![pcv, id, v],
        });
    }

    fn mem_code(m: X87Mem) -> u64 {
        (match m {
            X87Mem::F32 => x87op::MEM_F32,
            X87Mem::F64 => x87op::MEM_F64,
            X87Mem::F80 => x87op::MEM_F80,
            X87Mem::I16 => x87op::MEM_I16,
            X87Mem::I32 => x87op::MEM_I32,
            X87Mem::I64 => x87op::MEM_I64,
        }) << x87op::MEM_SHIFT
    }

    /// Copy the memory operand into `X87_TMP` as raw bits, for the host.
    fn x87_mem_in(&mut self, o: Operand, m: X87Mem) -> Option<()> {
        let Operand::Mem(mem) = o else {
            return None;
        };
        let (addr, offset) = self.addr(&mem);
        let tmp = self.x87_tmp();
        let (sz, ty) = match m {
            X87Mem::F32 | X87Mem::I32 => (MemSz::B4, Ty::I32),
            X87Mem::I16 => (MemSz::B2, Ty::I32),
            X87Mem::F64 | X87Mem::I64 | X87Mem::F80 => (MemSz::B8, Ty::I64),
        };
        let v = self.b.def(Expr::Load {
            addr,
            offset,
            sz,
            signed: false,
            ty,
            order: Order::Relaxed,
        });
        let v = if ty == Ty::I32 { self.b.extend(v, false) } else { v };
        self.b.stmt(Stmt::Store {
            addr: tmp,
            offset: 0,
            val: v,
            sz: MemSz::B8,
            order: Order::Relaxed,
        });
        if m == X87Mem::F80 {
            let se = self.b.def(Expr::Load {
                addr,
                offset: offset + 8,
                sz: MemSz::B2,
                signed: false,
                ty: Ty::I32,
                order: Order::Relaxed,
            });
            self.b.stmt(Stmt::Store {
                addr: tmp,
                offset: 8,
                val: se,
                sz: MemSz::B2,
                order: Order::Relaxed,
            });
        }
        Some(())
    }

    /// An arithmetic instruction: `x87op::FADD` .. `FDIV`, `rev` for the
    /// reversed forms.
    fn x87_arith(&mut self, op: u64, src: X87Src, rev: bool) -> Flow {
        let word = match src {
            X87Src::MemF(w) => {
                let m = if w == Width::Q { X87Mem::F64 } else { X87Mem::F32 };
                if self.x87_mem_in(self.cur_mem, m).is_none() {
                    return self.illegal_x87();
                }
                x87op::FORM_MEM << x87op::FORM_SHIFT | Self::mem_code(m)
            }
            X87Src::MemI(w) => {
                let m = if w == Width::W { X87Mem::I16 } else { X87Mem::I32 };
                if self.x87_mem_in(self.cur_mem, m).is_none() {
                    return self.illegal_x87();
                }
                x87op::FORM_MEM << x87op::FORM_SHIFT | Self::mem_code(m)
            }
            X87Src::St0Sti(i) => {
                x87op::FORM_ST0_STI << x87op::FORM_SHIFT | (i as u64) << x87op::I_SHIFT
            }
            X87Src::StiSt0 { i, pop } => {
                x87op::FORM_STI_ST0 << x87op::FORM_SHIFT
                    | (i as u64) << x87op::I_SHIFT
                    | (pop as u64) << x87op::POP_SHIFT
            }
        };
        self.x87_host(op | word | if rev { x87op::REV } else { 0 });
        Flow::Next
    }

    /// A comparison against `st(i)` or the memory operand, `pops` times
    /// popping afterwards.
    fn x87_compare(&mut self, src: Option<X87Mem>, i: u8, pops: u8, quiet: bool, eflags: bool) -> Flow {
        let mut word = x87op::FCOM | (pops as u64) << x87op::POP_SHIFT;
        match src {
            Some(m) => {
                if self.x87_mem_in(self.cur_mem, m).is_none() {
                    return self.illegal_x87();
                }
                word |= x87op::FORM_MEM << x87op::FORM_SHIFT | Self::mem_code(m);
            }
            None => word |= x87op::FORM_ST0_STI << x87op::FORM_SHIFT | (i as u64) << x87op::I_SHIFT,
        }
        if quiet {
            word |= x87op::QUIET;
        }
        if eflags {
            word |= x87op::TO_EFLAGS;
        }
        self.x87_host(word);
        if eflags {
            // The host leaves ZF, PF, CF in bits 0-2 of the scratch slot;
            // `fcomi` clears OF, SF and AF.
            let tmp = self.x87_tmp();
            let r = self.b.def(Expr::Load {
                addr: tmp,
                offset: 0,
                sz: MemSz::B4,
                signed: false,
                ty: Ty::I32,
                order: Order::Relaxed,
            });
            let one = self.k32(1);
            let z = self.b.bin(BinOp::And, Ty::I32, r, one);
            let p = self.b.bin(BinOp::ShrU, Ty::I32, r, one);
            let p = self.b.bin(BinOp::And, Ty::I32, p, one);
            let two = self.k32(2);
            let c = self.b.bin(BinOp::ShrU, Ty::I32, r, two);
            let c = self.b.bin(BinOp::And, Ty::I32, c, one);
            let zero = self.k32(0);
            self.b.write(GuestReg::Z, z);
            self.b.write(GuestReg::C, c);
            self.b.write(GuestReg::P, p);
            self.b.write(GuestReg::N, zero);
            self.b.write(GuestReg::V, zero);
            self.b.write(GuestReg::A, zero);
            self.flags = FlagSource::Unknown;
        }
        Flow::Next
    }

    /// Clear C1, as the sign and move instructions do.
    fn clear_c1(&mut self) {
        let sw = self.b.read(GuestReg::X87Sw);
        let m = self.k32(!C1);
        let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
        self.b.write(GuestReg::X87Sw, sw);
    }

    pub(crate) fn x87(&mut self, i: &Inst) -> Flow {
        self.cur_mem = i.ops[0].unwrap_or(Operand::Imm(0));
        match i.op {
            // The conversions are inline (see `x87conv`): they are most of
            // what compiled code does with the x87.
            Op::Fld(m) | Op::Fild(m) => return self.x87_fld_mem(i.op0(), m),
            Op::Fst { mem, pop } => return self.x87_fst_mem(i.op0(), mem, pop, false, false),
            Op::Fist { mem, pop, truncate } => return self.x87_fst_mem(i.op0(), mem, pop, truncate, true),
            Op::FldSt(n) => {
                let v = self.ld_st(n);
                self.push_st(v);
                self.clear_c1();
            }
            Op::FstSt { i: n, pop } => {
                let v = self.ld_st(0);
                self.st_st(n, v);
                if pop {
                    self.pop_st();
                }
                self.clear_c1();
            }
            Op::Fxch(n) => {
                let a = self.ld_st(0);
                let b = self.ld_st(n);
                self.st_st(0, b);
                self.st_st(n, a);
                self.clear_c1();
            }
            Op::Fadd(s) => return self.x87_arith(x87op::FADD, s, false),
            Op::Fmul(s) => return self.x87_arith(x87op::FMUL, s, false),
            Op::Fsub(s) => return self.x87_arith(x87op::FSUB, s, false),
            Op::Fsubr(s) => return self.x87_arith(x87op::FSUB, s, true),
            Op::Fdiv(s) => return self.x87_arith(x87op::FDIV, s, false),
            Op::Fdivr(s) => return self.x87_arith(x87op::FDIV, s, true),
            Op::Fcom { src, pop } => {
                return match src {
                    X87Src::MemF(w) => {
                        let m = if w == Width::Q { X87Mem::F64 } else { X87Mem::F32 };
                        self.x87_compare(Some(m), 0, pop, false, false)
                    }
                    X87Src::St0Sti(n) => self.x87_compare(None, n, pop, false, false),
                    _ => self.illegal_x87(),
                };
            }
            Op::Fucom { i: n, pop } => return self.x87_compare(None, n, pop, true, false),
            Op::Fcomi { i: n, pop, unordered } => {
                return self.x87_compare(None, n, pop as u8, unordered, true);
            }
            Op::Ficom { mem, pop } => return self.x87_compare(Some(mem), 0, pop as u8, false, false),
            Op::Fchs | Op::Fabs => {
                let (sig, se) = self.ld_st(0);
                let se = if i.op == Op::Fchs {
                    let m = self.k32(0x8000);
                    self.b.bin(BinOp::Xor, Ty::I32, se, m)
                } else {
                    let m = self.k32(0x7fff);
                    self.b.bin(BinOp::And, Ty::I32, se, m)
                };
                self.st_st(0, (sig, se));
                self.clear_c1();
            }
            Op::Fsqrt => self.x87_host(x87op::FSQRT),
            Op::Frndint => self.x87_host(x87op::FRNDINT),
            Op::Fscale => self.x87_host(x87op::FSCALE),
            Op::Ftst => self.x87_host(x87op::FTST),
            Op::Fxam => self.x87_host(x87op::FXAM),
            Op::Fld1 | Op::Fldl2t | Op::Fldl2e | Op::Fldpi | Op::Fldlg2 | Op::Fldln2 | Op::Fldz => {
                let k = match i.op {
                    Op::Fld1 => 0,
                    Op::Fldl2t => 1,
                    Op::Fldl2e => 2,
                    Op::Fldpi => 3,
                    Op::Fldlg2 => 4,
                    Op::Fldln2 => 5,
                    _ => 6,
                };
                return self.x87_fld_const(k);
            }
            Op::Fdecstp => {
                let top = self.top();
                let seven = self.k32(7);
                let t = self.b.bin(BinOp::Add, Ty::I32, top, seven);
                let t = self.b.bin(BinOp::And, Ty::I32, t, seven);
                self.set_top(t);
                self.clear_c1();
            }
            Op::Fincstp => {
                let top = self.top();
                let one = self.k32(1);
                let seven = self.k32(7);
                let t = self.b.bin(BinOp::Add, Ty::I32, top, one);
                let t = self.b.bin(BinOp::And, Ty::I32, t, seven);
                self.set_top(t);
                self.clear_c1();
            }
            Op::Fnop | Op::Fwait => {}
            Op::Fninit => self.x87_init(),
            Op::Fnclex => {
                let sw = self.b.read(GuestReg::X87Sw);
                let m = self.k32(!0x80ffu32);
                let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
                self.b.write(GuestReg::X87Sw, sw);
            }
            Op::Fldcw => {
                let v = self.rd(i.op0(), Width::W);
                self.x87_set_cw(v);
            }
            Op::Fnstcw => {
                let v = self.b.read(GuestReg::X87Cw);
                self.wr(i.op0(), Width::W, v);
            }
            Op::Fnstsw | Op::FnstswAx => {
                let v = self.x87_status_word();
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
                let (asig, ase) = self.ld_st(0);
                let (bsig, bse) = self.ld_st(n);
                let sig = self.b.select(c, bsig, asig, Ty::I64);
                let se = self.b.select(c, bse, ase, Ty::I32);
                self.st_st(0, (sig, se));
                self.clear_c1();
            }
            // The transcendental and remainder instructions: the host
            // computes them on the register stack in the context
            // (`gex_kernel::x86::serve_x87`), pushes and pops included.
            // i386 musl's libm is built on them (`atan2`, `log`, `exp`,
            // `fmod`, `remquo` are x87 assembly there).
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
                    Op::F2xm1 => x87op::F2XM1,
                    Op::Fyl2x => x87op::FYL2X,
                    Op::Fptan => x87op::FPTAN,
                    Op::Fpatan => x87op::FPATAN,
                    Op::Fxtract => x87op::FXTRACT,
                    Op::Fprem1 => x87op::FPREM1,
                    Op::Fprem => x87op::FPREM,
                    Op::Fyl2xp1 => x87op::FYL2XP1,
                    Op::Fsincos => x87op::FSINCOS,
                    Op::Fsin => x87op::FSIN,
                    _ => x87op::FCOS,
                };
                self.x87_host(op);
            }
            Op::Fnstenv | Op::Fldenv | Op::Fnsave | Op::Frstor => {
                let Operand::Mem(m) = i.op0() else { return self.illegal_x87() };
                let (addr, off) = self.addr(&m);
                let addr = self.offset_addr(addr, off);
                let store = matches!(i.op, Op::Fnstenv | Op::Fnsave);
                // A 16-bit operand size selects the 14-byte environment.
                let wide = i.width != Width::W;
                self.x87_env(addr, store, wide);
                // `fnsave`/`frstor`: the registers follow the environment
                // (108- or 94-byte image), 10 bytes each, in stack order.
                if matches!(i.op, Op::Fnsave | Op::Frstor) {
                    let env = if wide { 28 } else { 14 };
                    for n in 0..8u8 {
                        let off = env + 10 * n as i64;
                        if i.op == Op::Fnsave {
                            let v = self.ld_st(n);
                            self.st_ext(addr, off, v);
                        } else {
                            let v = self.ld_ext(addr, off);
                            self.st_st(n, v);
                        }
                    }
                    if i.op == Op::Fnsave {
                        self.x87_init();
                    }
                }
            }
            _ => return self.illegal_x87(),
        }
        Flow::Next
    }

    /// Load the control word as the hardware keeps it: bits 7 and 13-15
    /// read back as zero, reserved bit 6 as one (`fldcw 0x23f` stores
    /// 0x27f).
    pub(crate) fn x87_set_cw(&mut self, v: Value) {
        let keep = self.k32(0x1f3f);
        let v = self.b.bin(BinOp::And, Ty::I32, v, keep);
        let one = self.k32(0x40);
        let v = self.b.bin(BinOp::Or, Ty::I32, v, one);
        self.b.write(GuestReg::X87Cw, v);
    }

    /// `fninit`'s state: control word 0x37f, status and tags clear, TOP 0.
    fn x87_init(&mut self) {
        let cw = self.k32(0x37f);
        self.b.write(GuestReg::X87Cw, cw);
        let z = self.k32(0);
        self.b.write(GuestReg::X87Sw, z);
        self.set_top(z);
        self.b.write(GuestReg::X87Tags, z);
    }

    /// The x87 part of an `fxsave` area after the control word: the status
    /// word at 2, the abridged tag byte at 4 (one bit per physical register,
    /// set when valid, which is how gex keeps them), zeroed instruction and
    /// operand pointers, and `st(0)`..`st(7)` at 32 + 16i.
    pub(crate) fn x87_fxsave(&mut self, addr: Value, save: bool) {
        let st = |this: &mut Self, offset: i64, val: Value, sz: MemSz| {
            this.b.stmt(Stmt::Store { addr, offset, val, sz, order: Order::Relaxed });
        };
        if save {
            let sw = self.x87_status_word();
            st(self, 2, sw, MemSz::B2);
            let tags = self.b.read(GuestReg::X87Tags);
            st(self, 4, tags, MemSz::B2);
            let zero = self.k64(0);
            st(self, 8, zero, MemSz::B8);
            st(self, 16, zero, MemSz::B8);
            let zero32 = self.k32(0);
            st(self, 6, zero32, MemSz::B2);
            for n in 0..8u8 {
                let off = 32 + 16 * n as i64;
                let v = self.ld_st(n);
                self.st_ext(addr, off, v);
                st(self, off + 10, zero32, MemSz::B2);
                st(self, off + 12, zero32, MemSz::B4);
            }
        } else {
            let ld = |this: &mut Self, offset: i64, sz: MemSz| {
                this.b.def(Expr::Load { addr, offset, sz, signed: false, ty: Ty::I32, order: Order::Relaxed })
            };
            let sw = ld(self, 2, MemSz::B2);
            let eleven = self.k32(11);
            let t = self.b.bin(BinOp::ShrU, Ty::I32, sw, eleven);
            let seven = self.k32(7);
            let top = self.b.bin(BinOp::And, Ty::I32, t, seven);
            self.set_top(top);
            let m = self.k32(!0x3800u32);
            let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
            self.b.write(GuestReg::X87Sw, sw);
            let tags = ld(self, 4, MemSz::B1);
            self.b.write(GuestReg::X87Tags, tags);
            for n in 0..8u8 {
                let v = self.ld_ext(addr, 32 + 16 * n as i64);
                self.st_st(n, v);
            }
        }
    }

    /// The status word as `fnstsw` stores it, with `TOP` in bits 11-13.
    pub(crate) fn x87_status_word(&mut self) -> Value {
        let sw = self.b.read(GuestReg::X87Sw);
        let m = self.k32(!0x3800u32);
        let sw = self.b.bin(BinOp::And, Ty::I32, sw, m);
        let top = self.top();
        let eleven = self.k32(11);
        let t = self.b.bin(BinOp::Shl, Ty::I32, top, eleven);
        self.b.bin(BinOp::Or, Ty::I32, sw, t)
    }

    /// The x87 environment, 28 bytes in the 32-bit protected-mode
    /// layout glibc's fenv.h expects: control word, status word, tag
    /// word, then the instruction/operand pointers (stored as zero, as
    /// a real CPU stores nothing meaningful after `fninit`). `fnstenv`
    /// also masks every exception in the live control word, which
    /// `feholdexcept` relies on. Tags here are one valid bit per
    /// register; the tag word is two bits per register, 11 = empty.
    ///
    /// With a 16-bit operand size (`wide` false) the image is the 14-byte
    /// one: the same fields, two bytes each.
    fn x87_env(&mut self, addr: Value, store: bool, wide: bool) {
        let stride: i64 = if wide { 4 } else { 2 };
        // Field `n` of the image; the 32-bit layout's upper halves read as
        // ones (they are reserved).
        let st32 = |this: &mut Self, offset: i64, val: Value| {
            let n = offset / 4;
            if wide {
                this.b.stmt(Stmt::Store { addr, offset, val, sz: MemSz::B4, order: Order::Relaxed });
            } else {
                this.b.stmt(Stmt::Store { addr, offset: n * 2, val, sz: MemSz::B2, order: Order::Relaxed });
            }
        };
        if store {
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
            // The last instruction's and operand's addresses and opcode:
            // gex does not track them and stores zero. The data selector's
            // reserved upper half reads as ones, like the others'.
            let zero = self.k32(0);
            st32(self, 12, zero);
            st32(self, 16, zero);
            st32(self, 20, zero);
            st32(self, 24, hi);
            // Then mask all exceptions.
            let m3f = self.k32(0x3f);
            let cw2 = self.b.bin(BinOp::Or, Ty::I32, cw, m3f);
            self.b.write(GuestReg::X87Cw, cw2);
        } else {
            let ld16 = |this: &mut Self, offset: i64| {
                this.b.def(Expr::Load { addr, offset, sz: MemSz::B2, signed: false, ty: Ty::I32, order: Order::Relaxed })
            };
            let cw = ld16(self, 0);
            self.x87_set_cw(cw);
            let sw = ld16(self, stride);
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
            let tw = ld16(self, 2 * stride);
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

    pub(crate) fn illegal_x87(&self) -> Flow {
        Flow::End(Term::Illegal {
            raw: self.raw,
            pc: self.pc,
        })
    }
}
