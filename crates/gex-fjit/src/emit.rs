//! One recovered guest function → one wasm function with the `F_body` ABI
//! (docs/function-tier.md §7), plus a function per outermost loop (§7.2).
//!
//! The decisions are gex-cgen's (`func.rs`), made by the same analyses
//! (liveness, dirty-in, trap-free, outer loops); this emits wasm where that
//! emits C. Control flow is structured (`cfg`): a branch is a `br`, and
//! guest registers stay in locals across the whole body.

use crate::cfg::{self, Graph, Shape};
use gex_abi::ctx;
use gex_cgen::func::{self as cf, Loop};
use gex_emit::tier1::fmode::FnBody;
use gex_emit::tier1::Context;
use gex_emit::wasm::ValType;
use gex_ir::func::{Callee, FTerm, Function, RegSet, Summary};
use gex_ir::{GuestReg, Value};
use std::collections::HashMap;

/// `need`: the caller's continuation reads a flag.
pub const NEED_FLAGS: i32 = 1;
pub const NEED_ALL: i32 = -1;
/// A loop function's results beyond the body's 0 and 1 (§7.2).
pub const LOOP_CHUNK: i32 = 2;
pub const LOOP_EXIT: i32 = 3;

/// What a function's code refers to outside itself.
pub struct ModuleCx<'a> {
    /// tier-1's view of the module's imports.
    pub cx: &'a Context<'a>,
    /// The body of every function in this module, by entry.
    pub bodies: &'a HashMap<u64, u32>,
    /// Loop function `k` of the function at an entry.
    pub loops: &'a HashMap<(u64, usize), u32>,
    pub sums: &'a HashMap<u64, Summary>,
    /// The trap-free functions (`gex_cgen::func::trap_free`).
    pub trapfree: &'a HashMap<u64, u32>,
    /// How far a `ret` moves SP (`gex_cgen::func::ret_sp_delta`).
    pub ret_sp_delta: u64,
    /// The indirect-call helper, `(ctx, target, depth, need, ret) -> i32`:
    /// -1 when no function here starts at `target`.
    pub ic: u32,
}

/// What a function's exits did, for `GEX_FJIT_STATS`.
#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub functions: usize,
    pub blocks: usize,
    pub calls_known: usize,
    pub calls_trapfree: usize,
    pub calls_indirect: usize,
    pub calls_leaving: usize,
    pub tail_known: usize,
    pub switches: usize,
    pub loops_outlined: usize,
    pub returns: usize,
    pub irreducible: usize,
    pub code_bytes: usize,
}

impl Stats {
    pub fn merge(&mut self, o: &Stats) {
        self.functions += o.functions;
        self.blocks += o.blocks;
        self.calls_known += o.calls_known;
        self.calls_trapfree += o.calls_trapfree;
        self.calls_indirect += o.calls_indirect;
        self.calls_leaving += o.calls_leaving;
        self.tail_known += o.tail_known;
        self.switches += o.switches;
        self.loops_outlined += o.loops_outlined;
        self.returns += o.returns;
        self.irreducible += o.irreducible;
        self.code_bytes += o.code_bytes;
    }
}

/// A function's wasm: its body, then its loop functions in order.
pub struct Emitted {
    pub body: Vec<u8>,
    pub loops: Vec<Vec<u8>>,
}

/// The guest stack pointer as a GIR register.
fn sp_reg() -> GuestReg {
    if gex_abi::X86 {
        GuestReg::X(4)
    } else {
        GuestReg::Sp
    }
}

fn flags() -> RegSet {
    RegSet::of(cf::FLAG_REGS)
}

/// Emit `f` (memory form, verified, `loops` = `outer_loops(f)` when
/// outlining). `None` when its CFG is irreducible: it stays tier-1.
pub fn emit(m: &ModuleCx<'_>, f: &Function, loops: &[Loop], stats: &mut Stats) -> Option<Emitted> {
    let n = f.blocks.len();
    // The body's graph: the blocks, then one handover node per outlined
    // loop, reached from the loop's back-edges and leading to its exits.
    let mut succs: Vec<Vec<usize>> = f.blocks.iter().map(|b| b.succs().iter().map(|s| s.index()).collect()).collect();
    for (k, lp) in loops.iter().enumerate() {
        for &i in &lp.body {
            if handover_edge(f, lp, i, lp.header) {
                succs[i].push(n + k);
            }
        }
        succs.push(lp.exits.iter().copied().collect());
    }
    let shape = cfg::shape(&Graph { succs, entry: 0 })?;
    let regs = cf::registers(f);
    let live = if cf::live_writebacks() { cf::live_in(f, m.sums) } else { vec![RegSet::ALL; n] };
    let dirty: Vec<RegSet> = if cf::clean_spills() {
        cf::dirty_in(f).iter().map(|d| RegSet::of(&d.iter().copied().collect::<Vec<_>>())).collect()
    } else {
        vec![RegSet::of(&regs.written.iter().copied().collect::<Vec<_>>()); n]
    };
    let weight = cf::frame_weight(&regs);
    let used: Vec<GuestReg> = regs.used.iter().copied().collect();

    let mut lfns = Vec::new();
    for lp in loops {
        lfns.push(emit_loop(m, f, lp, &live, stats)?);
        stats.loops_outlined += 1;
    }

    let mut b = FnBody::new(m.cx, f.entry, 4, &used, f.n_values);
    let sp0 = b.local(ValType::I64);
    let poll = b.local(ValType::I32);
    let hot = b.local(ValType::I32);
    let lr = b.local(ValType::I32);
    let rk = b.local(ValType::I32);
    // The recursion-depth guard, before anything is loaded (§7.1).
    if !m.trapfree.contains_key(&f.entry) {
        let limit = cf::stack_budget().saturating_sub(weight + cf::TRAPFREE_RESERVE);
        let fb = b.f();
        fb.local_get(1);
        fb.i32_const(limit as i32);
        fb.byte(0x4a); // i32.gt_s
        fb.if_void();
        b.store_pc(f.entry);
        b.f().i32_const(0);
        b.f().ret();
        b.f().end();
    }
    b.load(&used);
    let spl = b.reg(sp_reg());
    b.f().local_get(spl);
    b.f().local_set(sp0);
    b.f().i32_const(cf::LOOP_POLL);
    b.f().local_set(poll);
    let mut e = E {
        m,
        f,
        b,
        shape,
        labels: Vec::new(),
        live: &live,
        dirty,
        weight,
        locals: Locals { sp0, poll, hot, lr, rk },
        mode: Mode::Body { loops },
        n_blocks: n,
        stats,
    };
    e.stats.functions += 1;
    e.stats.blocks += f.blocks.iter().filter(|b| !b.synthetic).count();
    e.tree(0);
    e.b.f().unreachable();
    let body = e.b.finish();
    e.stats.code_bytes += body.len() + lfns.iter().map(|l| l.len()).sum::<usize>();
    Some(Emitted { body, loops: lfns })
}

/// Whether `i -> h` is a back-edge of outlined loop `lp` where the body
/// hands the loop over (gex-cgen's rule: a backward branch by address).
fn handover_edge(f: &Function, lp: &Loop, i: usize, t: usize) -> bool {
    t == lp.header && lp.body.contains(&i) && backward_addr(f, i, t)
}

/// tier-1's rule for a backward branch, which polls: by guest address.
fn backward_addr(f: &Function, i: usize, t: usize) -> bool {
    let to = &f.blocks[t];
    !to.synthetic && to.start <= f.blocks[i].start
}

/// Loop function `k` of `f`: `(ctx, depth, need, ret, sp0) -> i32`.
fn emit_loop(m: &ModuleCx<'_>, f: &Function, lp: &Loop, live: &[RegSet], stats: &mut Stats) -> Option<Vec<u8>> {
    let n = f.blocks.len();
    let in_loop = |i: usize| lp.body.contains(&i);
    let succs: Vec<Vec<usize>> = (0..n)
        .map(|i| if in_loop(i) { f.blocks[i].succs().iter().map(|s| s.index()).filter(|&s| in_loop(s)).collect() } else { Vec::new() })
        .collect();
    let shape = cfg::shape(&Graph { succs, entry: lp.header })?;
    let regs = cf::registers_in(f, &in_loop);
    let dirty: Vec<RegSet> = if cf::clean_spills() {
        cf::dirty_in_from(f, lp.header, &in_loop)
            .iter()
            .map(|d| RegSet::of(&d.iter().copied().collect::<Vec<_>>()))
            .collect()
    } else {
        vec![RegSet::of(&regs.written.iter().copied().collect::<Vec<_>>()); n]
    };
    let weight = cf::frame_weight(&regs);
    let used: Vec<GuestReg> = regs.used.iter().copied().collect();
    let mut b = FnBody::new(m.cx, f.blocks[lp.header].start, 5, &used, f.n_values);
    let poll = b.local(ValType::I32);
    let hot = b.local(ValType::I32);
    let lr = b.local(ValType::I32);
    let rk = b.local(ValType::I32);
    b.load(&used);
    b.f().i32_const(cf::LOOP_POLL);
    b.f().local_set(poll);
    let mut e = E {
        m,
        f,
        b,
        shape,
        labels: Vec::new(),
        live,
        dirty,
        weight,
        locals: Locals { sp0: 4, poll, hot, lr, rk },
        mode: Mode::Loop { lp },
        n_blocks: n,
        stats,
    };
    e.tree(lp.header);
    e.b.f().unreachable();
    Some(e.b.finish())
}

struct Locals {
    sp0: u32,
    poll: u32,
    hot: u32,
    lr: u32,
    rk: u32,
}

enum Mode<'f> {
    Body { loops: &'f [Loop] },
    Loop { lp: &'f Loop },
}

/// An enclosing label, for branch depths.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Label {
    /// A `loop` headed by this node.
    Loop(usize),
    /// A `block` whose end is followed by this node.
    Follow(usize),
    /// Anything else (`if`, a switch's case blocks, a do-while).
    Other,
}

struct E<'m, 'a> {
    m: &'m ModuleCx<'a>,
    f: &'m Function,
    b: FnBody<'a>,
    shape: Shape,
    labels: Vec<Label>,
    live: &'m [RegSet],
    dirty: Vec<RegSet>,
    weight: u32,
    locals: Locals,
    mode: Mode<'m>,
    n_blocks: usize,
    stats: &'m mut Stats,
}

impl E<'_, '_> {
    // -- structure ---------------------------------------------------------

    fn tree(&mut self, x: usize) {
        let ys = self.shape.merge_children[x].clone();
        if self.shape.loop_header[x] {
            self.b.f().loop_void();
            self.labels.push(Label::Loop(x));
            self.within(x, &ys);
            self.labels.pop();
            self.b.f().end();
        } else {
            self.within(x, &ys);
        }
    }

    fn within(&mut self, x: usize, ys: &[usize]) {
        match ys.split_last() {
            Some((&y, rest)) => {
                self.b.f().block_void();
                self.labels.push(Label::Follow(y));
                self.within(x, rest);
                self.labels.pop();
                self.b.f().end();
                self.tree(y);
            }
            None => self.node(x),
        }
    }

    fn depth(&self, l: Label) -> u32 {
        let at = self.labels.iter().rposition(|&k| k == l).expect("structured branch to a label not in scope");
        (self.labels.len() - 1 - at) as u32
    }

    /// Transfer from node `x` to node `y`: a `br`, or `y`'s code inline.
    fn go(&mut self, x: usize, y: usize) {
        if self.shape.backward(x, y) {
            let d = self.depth(Label::Loop(y));
            self.b.f().br(d);
        } else if self.shape.merge[y] {
            let d = self.depth(Label::Follow(y));
            self.b.f().br(d);
        } else {
            self.tree(y);
        }
    }

    fn node(&mut self, x: usize) {
        if x >= self.n_blocks {
            self.handover(x - self.n_blocks);
            return;
        }
        let f = self.f;
        let blk = &f.blocks[x];
        self.b.set_dirty(self.dirty[x]);
        let mut keep: Vec<Value> = Vec::new();
        blk.term.for_each_use(|v| keep.push(v));
        self.b.block(blk.start, &blk.stmts, &keep);
        self.term(x);
        self.b.release(&keep);
    }

    // -- edges ---------------------------------------------------------------

    fn in_nested(&self, i: usize) -> bool {
        match self.mode {
            Mode::Loop { lp, .. } => lp.nested,
            Mode::Body { loops } => loops.iter().any(|lp| lp.nested && lp.body.contains(&i)),
        }
    }

    /// `if (--poll <= 0)`, opened.
    fn poll_expired(&mut self) {
        let p = self.locals.poll;
        let fb = self.b.f();
        fb.local_get(p);
        fb.i32_const(1);
        fb.byte(0x6b); // i32.sub
        fb.local_tee(p);
        fb.i32_const(0);
        fb.byte(0x4c); // i32.le_s
        fb.if_void();
        self.labels.push(Label::Other);
    }

    fn close(&mut self) {
        self.labels.pop();
        self.b.f().end();
    }

    /// The host's safepoint at `pc`, called outright (the countdown expired).
    fn call_safepoint(&mut self, pc: u64) {
        self.b.store_pc(pc);
        let sp = self.m.cx.safepoint;
        let fb = self.b.f();
        fb.local_get(0);
        fb.i64_const(pc as i64);
        fb.call(sp);
    }

    /// Whether edge `i -> t` is a bare `br` (no code on the edge).
    fn plain_br(&self, i: usize, t: usize) -> bool {
        if backward_addr(self.f, i, t) {
            return false;
        }
        match self.mode {
            Mode::Loop { lp, .. } if !lp.body.contains(&t) => false,
            _ => self.shape.branched(i, t),
        }
    }

    /// The code on edge `i -> t` (block indices), then the transfer.
    fn edge(&mut self, i: usize, t: usize) {
        let f = self.f;
        let backward = backward_addr(f, i, t);
        let to_start = f.blocks[t].start;
        match self.mode {
            Mode::Body { loops } => {
                if let Some(k) = loops.iter().position(|lp| handover_edge(f, lp, i, t)) {
                    // An outermost loop's back-edge in the body: the first
                    // 1024 trips run here, then the loop moves to its own
                    // function, a fresh call per chunk (§7.2).
                    let nested = loops[k].nested;
                    self.poll_expired();
                    let p = self.locals.poll;
                    self.b.f().i32_const(1024);
                    self.b.f().local_set(p);
                    self.b.flush(RegSet::ALL, true);
                    self.call_safepoint(to_start);
                    if nested {
                        let h = self.locals.hot;
                        self.b.f().i32_const(1);
                        self.b.f().local_set(h);
                        self.close();
                        self.b.f().local_get(h);
                        self.b.f().if_void();
                        self.labels.push(Label::Other);
                        self.b.flush(RegSet::ALL, true);
                    }
                    self.go(i, self.n_blocks + k);
                    self.close();
                    self.go(i, t);
                    return;
                }
            }
            Mode::Loop { lp, .. } => {
                if !lp.body.contains(&t) {
                    // Leaving the loop: everything back, and the body
                    // carries on at `t`.
                    self.b.flush(RegSet::ALL, true);
                    self.b.f().i32_const(LOOP_EXIT + t as i32);
                    self.b.f().ret();
                    return;
                }
                if t == lp.header && backward {
                    // The loop's own back-edge: a chunk ends here when the
                    // countdown expires.
                    self.poll_expired();
                    self.b.flush(RegSet::ALL, true);
                    self.call_safepoint(to_start);
                    self.b.f().i32_const(LOOP_CHUNK);
                    self.b.f().ret();
                    self.close();
                    if lp.nested {
                        let h = self.locals.hot;
                        self.b.f().local_get(h);
                        self.b.f().if_void();
                        self.labels.push(Label::Other);
                        self.b.flush(RegSet::ALL, true);
                        self.b.f().i32_const(LOOP_CHUNK);
                        self.b.f().ret();
                        self.close();
                    }
                    self.go(i, t);
                    return;
                }
            }
        }
        if backward && !cf::exp("NOPOLL") {
            // tier-1's rule for a backward branch: poll, with the countdown
            // in a local; the slow path writes back what the host may see.
            self.poll_expired();
            let p = self.locals.poll;
            self.b.f().i32_const(1024);
            self.b.f().local_set(p);
            self.b.flush(cf::non_flags().union(self.live[t]), true);
            self.call_safepoint(to_start);
            if self.in_nested(i) {
                let h = self.locals.hot;
                self.b.f().i32_const(1);
                self.b.f().local_set(h);
            }
            self.close();
        }
        self.go(i, t);
    }

    /// Loop `k`'s handover node in the body: run the loop function chunk by
    /// chunk, then carry on where it left, or return as it did.
    fn handover(&mut self, k: usize) {
        let entry = self.f.entry;
        let Mode::Body { loops } = self.mode else { unreachable!("a handover node outside a body") };
        let lf = self.m.loops[&(entry, k)];
        let (lr, sp0, w) = (self.locals.lr, self.locals.sp0, self.weight);
        let fb = self.b.f();
        fb.loop_void();
        fb.local_get(0);
        fb.local_get(1);
        fb.i32_const(w as i32);
        fb.byte(0x6a); // i32.add
        fb.local_get(2);
        fb.local_get(3);
        fb.local_get(sp0);
        fb.call(lf);
        fb.local_tee(lr);
        fb.i32_const(LOOP_CHUNK);
        fb.byte(0x46); // i32.eq
        fb.br_if(0);
        fb.end();
        fb.local_get(lr);
        fb.i32_const(1);
        fb.byte(0x4c); // i32.le_s
        fb.if_void();
        fb.local_get(lr);
        fb.ret();
        fb.end();
        self.b.reload_except(RegSet::EMPTY);
        let me = self.n_blocks + k;
        for &x in &loops[k].exits {
            let fb = self.b.f();
            fb.local_get(lr);
            fb.i32_const(LOOP_EXIT + x as i32);
            fb.byte(0x46); // i32.eq
            fb.if_void();
            self.labels.push(Label::Other);
            self.go(me, x);
            self.close();
        }
        self.b.f().unreachable();
    }

    // -- exits -------------------------------------------------------------

    /// Write everything back, set PC, return 0: the root takes it from here.
    fn leave_const(&mut self, pc: u64) {
        self.b.flush(RegSet::ALL, false);
        self.b.store_pc(pc);
        self.b.f().i32_const(0);
        self.b.f().ret();
    }

    fn leave_value(&mut self, v: Value) {
        self.b.flush(RegSet::ALL, false);
        self.store_pc_value(v);
        self.b.f().i32_const(0);
        self.b.f().ret();
    }

    fn store_pc_value(&mut self, v: Value) {
        self.b.f().local_get(0);
        self.b.push(v);
        self.b.f().store(0x37, 3, ctx::PC as u32);
    }

    /// Leaves the i32 "`entry`'s code was rewritten" (`GEX_PATCHED`, one
    /// byte at a constant address per page) on the stack.
    fn patched(&mut self, entry: u64) {
        let fb = self.b.f();
        if cf::mutate("NOPATCHCHECK") {
            fb.i32_const(0);
            return;
        }
        let one = |fb: &mut gex_emit::wasm::FuncBody, a: u64| {
            fb.i32_const(0);
            fb.load(0x2d, 0, (gex_abi::PATCHED_PAGES + (a >> 15)) as u32); // i32.load8_u
            fb.i32_const(((a >> 12) & 7) as i32);
            fb.byte(0x76); // i32.shr_u
            fb.i32_const(1);
            fb.byte(0x71); // i32.and
        };
        one(fb, entry);
        if entry % gex_abi::DISPATCH_PAGE > gex_abi::DISPATCH_PAGE - 5 {
            one(fb, entry + 5);
            fb.byte(0x72); // i32.or
        }
    }

    /// `depth + w` on the stack.
    fn depth_plus(&mut self, w: u32) {
        let fb = self.b.f();
        fb.local_get(1);
        fb.i32_const(w as i32);
        fb.byte(0x6a); // i32.add
    }

    fn term(&mut self, i: usize) {
        let f = self.f;
        let blk = &f.blocks[i];
        match &blk.term {
            FTerm::Jump(e) => self.edge(i, e.to.index()),
            FTerm::Branch { cond, taken, not_taken } => {
                let (t, nt) = (taken.to.index(), not_taken.to.index());
                if self.plain_br(i, t) {
                    self.b.push(*cond);
                    let d = self.br_depth(i, t);
                    self.b.f().br_if(d);
                    self.edge(i, nt);
                } else if self.plain_br(i, nt) {
                    self.b.push(*cond);
                    self.b.f().byte(0x45); // i32.eqz
                    let d = self.br_depth(i, nt);
                    self.b.f().br_if(d);
                    self.edge(i, t);
                } else {
                    self.b.push(*cond);
                    self.b.f().if_void();
                    self.labels.push(Label::Other);
                    self.edge(i, t);
                    self.b.f().else_();
                    self.edge(i, nt);
                    self.close();
                }
            }
            FTerm::Switch { target, cases, .. } => {
                self.stats.switches += 1;
                self.switch(i, *target, cases.iter().map(|(a, e)| (*a, e.to.index())).collect());
                self.leave_value(*target);
            }
            FTerm::Call { callee, ret_pc, ret } => self.call(i, *callee, *ret_pc, ret.as_ref().map(|e| e.to.index())),
            FTerm::TailCall { callee } => match callee {
                Callee::Direct(t) if self.m.bodies.contains_key(t) => {
                    self.stats.tail_known += 1;
                    self.b.flush(RegSet::ALL, false);
                    // A tail call passes through no back-edge or root, so it
                    // polls.
                    self.b.safepoint(*t);
                    self.patched(*t);
                    self.b.f().if_void();
                    self.b.store_pc(*t);
                    self.b.f().i32_const(0);
                    self.b.f().ret();
                    self.b.f().end();
                    // The frame is replaced (a loop function's included:
                    // its result is the body's), so the depth stays.
                    let body = self.m.bodies[t];
                    let fb = self.b.f();
                    fb.local_get(0);
                    fb.local_get(1);
                    fb.local_get(2);
                    fb.local_get(3);
                    fb.return_call(body);
                }
                Callee::Direct(t) => {
                    self.stats.calls_leaving += 1;
                    self.leave_const(*t)
                }
                Callee::Indirect(v) => self.leave_value(*v),
            },
            FTerm::Return { target } => {
                self.stats.returns += 1;
                // The return the caller expects: write back what its
                // continuation may read and report success; anything else
                // leaves with everything written back.
                let (sp, sp0, delta) = (self.b.reg(sp_reg()), self.locals.sp0, self.m.ret_sp_delta);
                if cf::mutate("NORETCHECK") {
                    self.b.f().i32_const(1);
                } else {
                    self.b.push(*target);
                    let fb = self.b.f();
                    fb.local_get(3);
                    fb.byte(0x51); // i64.eq
                    fb.local_get(sp);
                    fb.local_get(sp0);
                    fb.i64_const(delta as i64);
                    fb.byte(0x7c); // i64.add
                    fb.byte(0x51); // i64.eq
                    fb.byte(0x71); // i32.and
                }
                self.b.f().if_void();
                self.b.flush(RegSet::ALL.minus(flags()), true);
                let fb = self.b.f();
                fb.local_get(2);
                fb.i32_const(NEED_FLAGS);
                fb.byte(0x71); // i32.and
                fb.if_void();
                self.b.flush(flags(), true);
                self.b.f().end();
                self.b.f().i32_const(1);
                self.b.f().ret();
                self.b.f().end();
                self.leave_value(*target);
            }
            FTerm::Unresolved { target } => self.leave_value(*target),
            FTerm::Syscall { next_pc, next } => {
                self.b.flush(RegSet::ALL, false);
                self.b.store_pc(*next_pc);
                let sc = self.m.cx.syscall;
                let fb = self.b.f();
                fb.local_get(0);
                fb.i64_const(blk.term_pc as i64);
                fb.call(sc);
                self.b.reload_except(RegSet::EMPTY);
                match next {
                    Some(e) => self.edge(i, e.to.index()),
                    None => {
                        self.b.f().i32_const(0);
                        self.b.f().ret();
                    }
                }
            }
            FTerm::Illegal { raw, pc } => {
                self.b.flush(RegSet::ALL, false);
                self.b.store_pc(*pc);
                let il = self.m.cx.illegal;
                let fb = self.b.f();
                fb.local_get(0);
                fb.i64_const(*pc as i64);
                fb.i32_const(*raw as i32);
                fb.call(il);
                fb.unreachable();
            }
        }
    }

    fn br_depth(&self, i: usize, t: usize) -> u32 {
        if self.shape.backward(i, t) {
            self.depth(Label::Loop(t))
        } else {
            self.depth(Label::Follow(t))
        }
    }

    /// A jump table's cases; the default (falling out) is the caller's.
    /// Dense tables become a `br_table`, others a chain of compares.
    fn switch(&mut self, i: usize, target: Value, cases: Vec<(u64, usize)>) {
        let shift = gex_abi::INST_ALIGN_SHIFT;
        let (lo, hi) = (cases.first().map(|c| c.0).unwrap_or(0), cases.last().map(|c| c.0).unwrap_or(0));
        let span = ((hi - lo) >> shift) + 1;
        if cases.len() < 4 || span > 4 * cases.len() as u64 + 16 {
            for (a, t) in cases {
                self.b.push(target);
                let fb = self.b.f();
                fb.i64_const(a as i64);
                fb.byte(0x51); // i64.eq
                fb.if_void();
                self.labels.push(Label::Other);
                self.edge(i, t);
                self.close();
            }
            return;
        }
        // Distinct targets, each a block; slot -> its depth inside them.
        let mut groups: Vec<usize> = Vec::new();
        for &(_, t) in &cases {
            if !groups.contains(&t) {
                groups.push(t);
            }
        }
        let g = groups.len() as u32;
        let tmp = self.b.local(ValType::I64);
        self.b.f().block_void(); // the default
        self.labels.push(Label::Other);
        for _ in 0..g {
            self.b.f().block_void();
            self.labels.push(Label::Other);
        }
        // Inside the innermost block group k is at depth k, the default at g.
        self.b.push(target);
        let fb = self.b.f();
        fb.i64_const(lo as i64);
        fb.byte(0x7d); // i64.sub
        fb.local_tee(tmp);
        fb.i64_const(((span << shift) - 1) as i64 | ((1i64 << shift) - 1));
        fb.byte(0x56); // i64.gt_u
        fb.br_if(g);
        if shift != 0 {
            fb.local_get(tmp);
            fb.i64_const((1i64 << shift) - 1);
            fb.byte(0x83); // i64.and
            fb.byte(0xa7); // i32.wrap_i64
            fb.br_if(g);
        }
        let mut slots = vec![g; span as usize];
        for &(a, t) in &cases {
            slots[((a - lo) >> shift) as usize] = groups.iter().position(|&x| x == t).unwrap() as u32;
        }
        fb.local_get(tmp);
        if shift != 0 {
            fb.i64_const(shift as i64);
            fb.byte(0x88); // i64.shr_u
        }
        fb.byte(0xa7); // i32.wrap_i64
        fb.br_table(&slots, g);
        for &t in &groups {
            self.close();
            self.edge(i, t);
        }
        self.close();
    }

    fn call(&mut self, i: usize, callee: Callee, ret_pc: u64, ret: Option<usize>) {
        let m = self.m;
        let live = self.live;
        // A known callee with a summary needs its inputs, the continuation
        // what is live there; non-flag registers are limited only for a
        // trap-free callee (function-tier.md §5.1). Anything else may read
        // everything.
        let need = match (callee, ret) {
            (Callee::Direct(t), Some(r)) if m.bodies.contains_key(&t) => match m.sums.get(&t) {
                Some(sm) => {
                    let n = sm.inputs.union(live[r]).union(RegSet::of(&[sp_reg()]));
                    if m.trapfree.contains_key(&t) || cf::mutate("IGNOREUNWIND") {
                        self.stats.calls_trapfree += 1;
                        n
                    } else {
                        n.union(cf::non_flags())
                    }
                }
                None => RegSet::ALL,
            },
            _ => RegSet::ALL,
        };
        self.b.flush(need, false);
        // What the callee must hand back: the flags only if the
        // continuation reads one.
        let need_back = match ret {
            Some(r) if cf::live_writebacks() => {
                if live[r].inter(flags()).is_empty() {
                    0
                } else {
                    NEED_ALL
                }
            }
            _ => NEED_ALL,
        };
        let w = self.weight;
        match callee {
            Callee::Direct(t) if m.bodies.contains_key(&t) => {
                self.stats.calls_known += 1;
                // Its code rewritten since (Detours): leave for it.
                self.patched(t);
                self.b.f().if_void();
                self.b.store_pc(t);
                self.b.f().i32_const(0);
                self.b.f().ret();
                self.b.f().end();
                let body = m.bodies[&t];
                self.b.f().local_get(0);
                self.depth_plus(w);
                let fb = self.b.f();
                fb.i32_const(need_back);
                fb.i64_const(ret_pc as i64);
                fb.call(body);
                fb.byte(0x45); // i32.eqz
                fb.if_void();
                fb.i32_const(0);
                fb.ret();
                fb.end();
            }
            Callee::Direct(t) => {
                // Not ours: the root runs it, and the rest of this function
                // runs where the guest returns to.
                self.stats.calls_leaving += 1;
                self.b.store_pc(t);
                self.b.f().i32_const(0);
                self.b.f().ret();
                return;
            }
            Callee::Indirect(v) => {
                self.stats.calls_indirect += 1;
                let rk = self.locals.rk;
                self.b.f().local_get(0);
                self.b.push(v);
                self.depth_plus(w + cf::CALL_KNOWN_WEIGHT);
                let fb = self.b.f();
                fb.i32_const(need_back);
                fb.i64_const(ret_pc as i64);
                fb.call(m.ic);
                fb.local_tee(rk);
                fb.i32_const(0);
                fb.byte(0x48); // i32.lt_s
                fb.if_void();
                self.store_pc_value(v);
                self.b.f().i32_const(0);
                self.b.f().ret();
                self.b.f().end();
                let fb = self.b.f();
                fb.local_get(rk);
                fb.byte(0x45); // i32.eqz
                fb.if_void();
                fb.i32_const(0);
                fb.ret();
                fb.end();
            }
        }
        // A summarised callee that returned as expected left its preserved
        // registers as they were: no reload.
        let keep = match callee {
            Callee::Direct(t) if m.bodies.contains_key(&t) && cf::live_writebacks() => match m.sums.get(&t) {
                Some(sm) => sm.preserved.minus(RegSet::of(&[sp_reg()])),
                None => RegSet::EMPTY,
            },
            _ => RegSet::EMPTY,
        };
        self.b.reload_except(keep);
        match ret {
            Some(r) => self.edge(i, r),
            None => {
                // Believed not to return, and it did.
                self.b.store_pc(ret_pc);
                self.b.f().i32_const(0);
                self.b.f().ret();
            }
        }
    }
}
