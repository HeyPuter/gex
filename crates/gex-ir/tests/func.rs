//! Function-level GIR: construction, printing and parsing, SSA.

use gex_ir::func::*;
use gex_ir::ssa::{to_ssa, CallEffect};
use gex_ir::*;

/// A tiny function builder for the tests: blocks of statements with
/// function-wide value numbers.
struct F {
    f: Function,
}

impl F {
    fn new(entry: u64) -> F {
        F {
            f: Function {
                entry,
                name: Some("test fn".into()),
                entries: Vec::new(),
                blocks: Vec::new(),
                n_values: 0,
                form: Form::Memory,
            },
        }
    }
    fn block(&mut self, start: u64) -> BlockId {
        self.f.blocks.push(FBlock {
            start,
            end: start + 16,
            synthetic: false,
            params: Vec::new(),
            stmts: Vec::new(),
            pcs: Vec::new(),
            insts: vec![start, start + 4, start + 8, start + 12],
            term: FTerm::Illegal { raw: 0, pc: start },
            term_pc: start + 12,
        });
        BlockId(self.f.blocks.len() as u32 - 1)
    }
    fn def(&mut self, b: BlockId, e: Expr) -> Value {
        let v = self.f.fresh();
        let pc = self.f.block(b).start;
        self.f.block_mut(b).push(pc, Stmt::Def { dst: v, expr: e });
        v
    }
    fn read(&mut self, b: BlockId, r: GuestReg) -> Value {
        self.def(b, Expr::ReadReg(r))
    }
    fn write(&mut self, b: BlockId, r: GuestReg, v: Value) {
        let pc = self.f.block(b).start + 4;
        self.f.block_mut(b).push(pc, Stmt::WriteReg { reg: r, val: v });
    }
    fn term(&mut self, b: BlockId, t: FTerm) {
        self.f.block_mut(b).term = t;
    }
}

fn x(n: u8) -> GuestReg {
    GuestReg::X(n)
}

/// if (x0 == 0) x1 = 1 else x1 = 2; x2 = x0 + x1; return
fn diamond() -> Function {
    let mut t = F::new(0x1000);
    let b0 = t.block(0x1000);
    let b1 = t.block(0x1010);
    let b2 = t.block(0x1020);
    let b3 = t.block(0x1030);
    let a = t.read(b0, x(0));
    let z = t.def(b0, Expr::ConstI64(0));
    let c = t.def(
        b0,
        Expr::Cmp {
            op: CmpOp::Eq,
            ty: Ty::I64,
            a,
            b: z,
        },
    );
    t.term(
        b0,
        FTerm::Branch {
            cond: c,
            taken: Edge::to(b1),
            not_taken: Edge::to(b2),
        },
    );
    let one = t.def(b1, Expr::ConstI64(1));
    t.write(b1, x(1), one);
    t.term(b1, FTerm::Jump(Edge::to(b3)));
    let two = t.def(b2, Expr::ConstI64(2));
    t.write(b2, x(1), two);
    t.term(b2, FTerm::Jump(Edge::to(b3)));
    let p = t.read(b3, x(0));
    let q = t.read(b3, x(1));
    let s = t.def(
        b3,
        Expr::Bin {
            op: BinOp::Add,
            ty: Ty::I64,
            a: p,
            b: q,
        },
    );
    t.write(b3, x(2), s);
    let lr = t.read(b3, x(30));
    t.term(b3, FTerm::Return { target: lr });
    t.f
}

/// x0 counts down to zero around a loop that calls `0x9000` each time.
fn call_loop() -> Function {
    let mut t = F::new(0x2000);
    let b0 = t.block(0x2000);
    let head = t.block(0x2010);
    let body = t.block(0x2020);
    let after = t.block(0x2030);
    let exit = t.block(0x2040);
    let n = t.def(b0, Expr::ConstI64(10));
    t.write(b0, x(19), n);
    t.term(b0, FTerm::Jump(Edge::to(head)));
    let i = t.read(head, x(19));
    let z = t.def(head, Expr::ConstI64(0));
    let c = t.def(
        head,
        Expr::Cmp {
            op: CmpOp::Eq,
            ty: Ty::I64,
            a: i,
            b: z,
        },
    );
    t.term(
        head,
        FTerm::Branch {
            cond: c,
            taken: Edge::to(exit),
            not_taken: Edge::to(body),
        },
    );
    let ret = t.def(body, Expr::ConstI64(0x2030));
    t.write(body, x(30), ret);
    t.term(
        body,
        FTerm::Call {
            callee: Callee::Direct(0x9000),
            ret_pc: 0x2030,
            ret: Some(Edge::to(after)),
        },
    );
    let i2 = t.read(after, x(19));
    let one = t.def(after, Expr::ConstI64(1));
    let d = t.def(
        after,
        Expr::Bin {
            op: BinOp::Sub,
            ty: Ty::I64,
            a: i2,
            b: one,
        },
    );
    t.write(after, x(19), d);
    t.term(after, FTerm::Jump(Edge::to(head)));
    let lr = t.read(exit, x(30));
    t.term(exit, FTerm::Return { target: lr });
    t.f
}

fn roundtrip(f: &Function) {
    let text = text::print(f);
    let back = text::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(&back, f, "round trip changed the function:\n{text}");
}

#[test]
fn memory_form_verifies_and_round_trips() {
    for f in [diamond(), call_loop()] {
        f.verify().unwrap();
        roundtrip(&f);
    }
}

#[test]
fn diamond_gets_one_parameter() {
    let f = diamond();
    let s = to_ssa(&f, &|_| CallEffect::UNKNOWN);
    s.verify().unwrap_or_else(|e| panic!("{e}\n{}", text::print(&s)));
    roundtrip(&s);
    // The join needs x1 (different per side), not x0 (the same on both).
    let join = s.block_at(0x1030).unwrap();
    let regs: Vec<GuestReg> = s.block(join).params.iter().map(|p| p.reg).collect();
    assert_eq!(regs, vec![x(1)], "{}", text::print(&s));
    // x0 is read once, at the entry, and x1 never.
    let reads: Vec<GuestReg> = s
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|st| match st {
            Stmt::Def {
                expr: Expr::ReadReg(r),
                ..
            } => Some(*r),
            _ => None,
        })
        .collect();
    assert_eq!(reads, vec![x(0), x(30)], "{}", text::print(&s));
}

#[test]
fn a_preserving_call_lets_the_counter_stay_in_ssa() {
    let f = call_loop();
    f.verify().unwrap();

    // Nothing known about the callee: x19 is re-read after every call.
    let s = to_ssa(&f, &|_| CallEffect::UNKNOWN);
    s.verify().unwrap_or_else(|e| panic!("{e}\n{}", text::print(&s)));
    roundtrip(&s);
    let after = s.block_at(0x2030).unwrap();
    assert!(
        s.block(after)
            .stmts
            .iter()
            .any(|st| matches!(st, Stmt::Def { expr: Expr::ReadReg(r), .. } if *r == x(19))),
        "{}",
        text::print(&s)
    );

    // A callee proved to preserve x19: no re-read; the loop header carries
    // it as a parameter.
    let keep = CallEffect {
        preserved: RegSet::of(&[x(19), GuestReg::Sp]),
        sp_delta: Some(0),
    };
    let s = to_ssa(&f, &|_| keep);
    s.verify().unwrap_or_else(|e| panic!("{e}\n{}", text::print(&s)));
    roundtrip(&s);
    let after = s.block_at(0x2030).unwrap();
    assert!(
        !s.block(after)
            .stmts
            .iter()
            .any(|st| matches!(st, Stmt::Def { expr: Expr::ReadReg(r), .. } if *r == x(19))),
        "{}",
        text::print(&s)
    );
    let head = s.block_at(0x2010).unwrap();
    let regs: Vec<GuestReg> = s.block(head).params.iter().map(|p| p.reg).collect();
    // x30 is not preserved, so it is re-read after the call and merged too.
    assert_eq!(regs, vec![x(19), x(30)], "{}", text::print(&s));
}

#[test]
fn effects_name_every_access() {
    let s = Stmt::Store {
        addr: Value(1),
        offset: -8,
        val: Value(2),
        sz: MemSz::B8,
        order: Order::Relaxed,
    };
    let e = func::effect(&s);
    assert_eq!(e.mem.len(), 1);
    assert_eq!(e.mem[0].access, Access::Write);
    assert_eq!(e.mem[0].offset, -8);
    assert!(!e.is_pure());
    let c = Stmt::MemCopy {
        dst: Value(1),
        src: Value(2),
        len: Value(3),
    };
    let e = func::effect(&c);
    assert_eq!(
        e.mem.iter().map(|m| m.access).collect::<Vec<_>>(),
        vec![Access::Read, Access::Write]
    );
}

#[test]
fn regset_indices_are_a_bijection() {
    for i in 0..N_REGS {
        assert_eq!(reg_index(reg_of_index(i)), i);
    }
    let s = RegSet::of(&[x(0), GuestReg::Sp, GuestReg::N]);
    assert_eq!(s.iter().collect::<Vec<_>>(), vec![x(0), GuestReg::Sp, GuestReg::N]);
    assert_eq!(RegSet::ALL.len(), N_REGS);
}

fn same_run(a: &Function, b: &Function, call: &dyn Fn(Callee) -> CallEffect, sp: GuestReg) {
    for seed in 0..32u64 {
        let m = interp::Model {
            call,
            sp,
            max_steps: 200,
        };
        let x = interp::run(a, seed, &m);
        let y = interp::run(b, seed, &m);
        assert!(!matches!(x.stop, interp::Stop::Unsupported(_)), "{:?}", x.stop);
        assert_eq!(x, y, "seed {seed}:\n{}\n{}", text::print(a), text::print(b));
    }
}

#[test]
fn ssa_runs_like_memory_form() {
    let keep = CallEffect {
        preserved: RegSet::of(&[x(19), GuestReg::Sp]),
        sp_delta: Some(0),
    };
    for f in [diamond(), call_loop()] {
        for eff in [CallEffect::UNKNOWN, keep] {
            let s = to_ssa(&f, &|_| eff);
            same_run(&f, &s, &|_| eff, GuestReg::Sp);
        }
    }
}

/// x86-64 shape: `rsp` is X(4); a call pushes the return address and the
/// callee pops it (`sp_delta` +8). The value spilled below the stack pointer
/// is reloaded after the call through the computed stack pointer.
fn x86_spill_around_call() -> Function {
    let mut t = F::new(0x3000);
    let b0 = t.block(0x3000);
    let after = t.block(0x3010);
    let sp = t.read(b0, x(4));
    let m16 = t.def(b0, Expr::ConstI64(-16i64 as u64));
    let frame = t.def(
        b0,
        Expr::Bin {
            op: BinOp::Add,
            ty: Ty::I64,
            a: sp,
            b: m16,
        },
    );
    t.write(b0, x(4), frame);
    let v = t.read(b0, x(3));
    let pc = t.f.block(b0).start;
    t.f.block_mut(b0).push(
        pc,
        Stmt::Store {
            addr: frame,
            offset: 8,
            val: v,
            sz: MemSz::B8,
            order: Order::Relaxed,
        },
    );
    let m8 = t.def(b0, Expr::ConstI64(-8i64 as u64));
    let pushed = t.def(
        b0,
        Expr::Bin {
            op: BinOp::Add,
            ty: Ty::I64,
            a: frame,
            b: m8,
        },
    );
    let ra = t.def(b0, Expr::ConstI64(0x3010));
    t.f.block_mut(b0).push(
        pc,
        Stmt::Store {
            addr: pushed,
            offset: 0,
            val: ra,
            sz: MemSz::B8,
            order: Order::Relaxed,
        },
    );
    t.write(b0, x(4), pushed);
    t.term(
        b0,
        FTerm::Call {
            callee: Callee::Direct(0x9000),
            ret_pc: 0x3010,
            ret: Some(Edge::to(after)),
        },
    );
    let sp2 = t.read(after, x(4));
    let back = t.def(
        after,
        Expr::Load {
            addr: sp2,
            offset: 8,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        },
    );
    t.write(after, x(0), back);
    let r = t.def(
        after,
        Expr::Load {
            addr: sp2,
            offset: 16,
            sz: MemSz::B8,
            signed: false,
            ty: Ty::I64,
            order: Order::Relaxed,
        },
    );
    t.term(after, FTerm::Return { target: r });
    t.f
}

#[test]
fn ssa_carries_a_non_default_stack_pointer_across_calls() {
    let f = x86_spill_around_call();
    f.verify().unwrap();
    let eff = CallEffect {
        preserved: RegSet::EMPTY,
        sp_delta: Some(8),
    };
    let s = ssa::to_ssa_sp(&f, &|_| eff, x(4));
    s.verify().unwrap_or_else(|e| panic!("{e}\n{}", text::print(&s)));
    roundtrip(&s);
    // rsp is computed after the call, not re-read.
    let after = s.block_at(0x3010).unwrap();
    assert!(
        !s.block(after)
            .stmts
            .iter()
            .any(|st| matches!(st, Stmt::Def { expr: Expr::ReadReg(r), .. } if *r == x(4))),
        "{}",
        text::print(&s)
    );
    same_run(&f, &s, &|_| eff, x(4));
    // With the AArch64 default, rsp is an ordinary register: re-read, and
    // still equivalent.
    let d = to_ssa(&f, &|_| eff);
    d.verify().unwrap();
    same_run(&f, &d, &|_| eff, x(4));
}
