//! Whole-program call-graph summaries ([`Summary`]).
//!
//! Each function is analysed by abstract interpretation over its memory-form
//! CFG, with its callees' summaries standing in for the calls, and the
//! results are iterated to a fixpoint over the call graph's strongly
//! connected components, callees first. The rule is pewasm's
//! (`opt_program.cc`): **summaries over-approximate and never guess.**
//! Anything unknown (an unresolved indirect jump, an indirect call, a callee
//! that was not recovered, a syscall that is not on the known-harmless list)
//! makes the affected parts of the summary "everything".
//!
//! # The abstract domain
//!
//! A value is a constant, an *entry value plus an offset* (`x19` as it was
//! on entry, `sp` on entry minus 48), the return address (x86-64: the word
//! at the entry stack pointer), or unknown. Every value also carries the set
//! of entry registers it was computed from by arithmetic (its *taint*), which
//! is how [`Summary::addr_inputs`] and frame escape are tracked. Stack slots
//! at constant offsets from the entry stack pointer hold abstract values, so
//! a register saved in the prologue and restored in the epilogue is seen to
//! come back unchanged: that is how `preserved` is proved.
//!
//! A slot is only trusted while nothing could have written it: a store to
//! an address the analysis cannot place kills the slots of the caller's
//! frame always, and this function's own slots only once the frame has
//! *escaped* (a frame address was stored to memory, passed to a callee that
//! uses that register as an address, or handed to a syscall). A callee's
//! writes above its entry stack pointer ([`Summary::caller_frame_writes`])
//! kill exactly the slots they cover.
//!
//! # Fixpoint
//!
//! Members of a recursive SCC start at [`Summary::bottom`] (the optimistic
//! value: nothing read, everything preserved, never returns) and are
//! recomputed until nothing changes, which gives the sound result by the
//! usual induction on call depth for the "must" parts (`preserved`,
//! `sp_delta`) and least-fixpoint reasoning for the "may" parts. A component
//! that has not converged after [`MAX_ITERATIONS`] is set to
//! [`Summary::unknown`].

use crate::arch::SYSCALL_NR;
use crate::func::Program;
use gex_ir::func::*;
use gex_ir::ssa::CallEffect;
use gex_ir::{BinOp, Expr, GuestReg, HostFunc, Stmt, Ty, Value};
use std::collections::{BTreeMap, HashMap};

/// Iterations of one SCC before giving up on it (setting it unknown).
pub const MAX_ITERATIONS: usize = 64;

/// The summaries of a whole program.
pub struct Summaries {
    pub map: HashMap<u64, Summary>,
    pub stats: SummaryStats,
}

#[derive(Clone, Debug, Default)]
pub struct SummaryStats {
    pub sccs: usize,
    pub recursive_sccs: usize,
    /// The most iterations any SCC took to converge.
    pub max_iterations: usize,
    /// SCCs that did not converge and were set to unknown.
    pub unconverged: usize,
    /// Functions whose summary has `unknown_callee`.
    pub unknown: usize,
}

impl Summaries {
    /// The summary of the function at `a`, or [`Summary::unknown`] for code
    /// that was not recovered.
    pub fn get(&self, a: u64) -> Summary {
        self.map.get(&a).cloned().unwrap_or_else(Summary::unknown)
    }

    /// What a call does to registers, for [`gex_ir::ssa::to_ssa`].
    pub fn call_effect(&self, c: Callee) -> CallEffect {
        match c {
            Callee::Direct(a) => match self.map.get(&a) {
                Some(s) => CallEffect::from_summary(s),
                None => CallEffect::UNKNOWN,
            },
            Callee::Indirect(_) => CallEffect::UNKNOWN,
        }
    }
}

/// A one-line rendering, for `gexc funcs --list`.
pub fn describe(s: &Summary) -> String {
    let flags: Vec<&str> = [
        (!s.may_return, "noreturn"),
        (s.may_unwind, "unwind"),
        (s.may_leave_pending, "pending"),
        (s.unknown_callee, "unknown"),
        (s.reads_return_address, "reads-ra"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, n)| *n)
    .collect();
    format!(
        "in={:?} stack_in={:?} preserved={:?} sp_delta={:?} caller_writes={:?} addr_in={:?} [{}]",
        s.inputs,
        s.stack_inputs,
        s.preserved,
        s.sp_delta,
        s.caller_frame_writes,
        s.addr_inputs,
        flags.join(" ")
    )
}

// ---------------------------------------------------------------------------
// Abstract values and state
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Ex {
    Const(u64),
    /// Entry value of register (by [`reg_index`]) plus an offset.
    Entry(u8, i64),
    /// x86-64: the return address, as loaded from the entry stack pointer.
    RetAddr,
    Top,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct AV {
    ex: Ex,
    /// Entry registers this value was computed from by arithmetic.
    taint: RegSet,
}

const SP: usize = 31;

impl AV {
    fn konst(c: u64) -> AV {
        AV {
            ex: Ex::Const(c),
            taint: RegSet::EMPTY,
        }
    }
    fn top(taint: RegSet) -> AV {
        AV { ex: Ex::Top, taint }
    }
    fn entry(r: usize, off: i64) -> AV {
        let mut t = RegSet::EMPTY;
        t.insert(reg_of_index(r));
        AV {
            ex: Ex::Entry(r as u8, off),
            taint: t,
        }
    }
    fn frame(self) -> bool {
        self.taint.contains(GuestReg::Sp)
    }
    /// Offset from the entry stack pointer, if this is exactly one.
    fn sp_off(self) -> Option<i64> {
        match self.ex {
            Ex::Entry(r, o) if r as usize == SP => Some(o),
            _ => None,
        }
    }
    fn join(self, o: AV) -> AV {
        if self == o {
            self
        } else {
            AV::top(self.taint.union(o.taint))
        }
    }
    /// Whether this is (derived from) the return address.
    fn is_ra(self) -> bool {
        match self.ex {
            Ex::RetAddr => true,
            Ex::Entry(r, _) => !gex_abi::X86 && r == 30,
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct St {
    regs: Vec<AV>,
    /// Stack slots by offset from the entry stack pointer: (bytes, value).
    slots: BTreeMap<i64, (u32, AV)>,
    /// Registers written on every path here.
    written: RegSet,
    /// The frame's address has escaped.
    escaped: bool,
}

impl St {
    fn entry() -> St {
        let mut slots = BTreeMap::new();
        if gex_abi::X86 {
            slots.insert(
                0,
                (
                    8,
                    AV {
                        ex: Ex::RetAddr,
                        taint: RegSet::EMPTY,
                    },
                ),
            );
        }
        St {
            regs: (0..N_REGS).map(|r| AV::entry(r, 0)).collect(),
            slots,
            written: RegSet::EMPTY,
            escaped: false,
        }
    }

    fn join(&self, o: &St) -> St {
        let regs = self.regs.iter().zip(&o.regs).map(|(a, b)| a.join(*b)).collect();
        let mut slots = BTreeMap::new();
        for (k, (n, v)) in &self.slots {
            if let Some((m, w)) = o.slots.get(k) {
                if n == m {
                    slots.insert(*k, (*n, v.join(*w)));
                }
            }
        }
        St {
            regs,
            slots,
            written: self.written.inter(o.written),
            escaped: self.escaped || o.escaped,
        }
    }

    /// Forget slots overlapping `[lo, hi)`.
    fn kill(&mut self, lo: i64, hi: i64) {
        let dead: Vec<i64> = self
            .slots
            .range(..hi)
            .filter(|(k, (n, _))| **k + *n as i64 > lo)
            .map(|(k, _)| *k)
            .collect();
        for k in dead {
            self.slots.remove(&k);
        }
    }

    fn kill_slots(&mut self, s: Slots) {
        match s {
            Slots::None => {}
            Slots::Range(a, b) => self.kill(a, b),
            Slots::Unknown => self.slots.clear(),
        }
    }

    /// A store nobody can place: the caller's frame may be hit always, this
    /// frame only once it has escaped.
    fn unknown_store(&mut self) {
        if self.escaped {
            self.slots.clear();
        } else {
            self.kill(0, i64::MAX);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-function analysis
// ---------------------------------------------------------------------------

/// What the walk accumulates besides the per-point states.
struct Acc {
    inputs: RegSet,
    stack_inputs: Slots,
    caller_frame_writes: Slots,
    addr_inputs: RegSet,
    may_unwind: bool,
    may_leave_pending: bool,
    unknown_callee: bool,
    reads_ra: bool,
    leaks_sp: bool,
    /// Per return path: (registers preserved on it, sp delta if known).
    exits: Vec<(RegSet, Option<i64>)>,
    /// Some path leaves to code nothing is known about.
    unknown_exit: bool,
}

/// Registers the kernel reads for a syscall, and those it may change.
fn syscall_regs() -> (RegSet, RegSet) {
    use GuestReg::X;
    if gex_abi::X86 {
        // rdi rsi rdx r10 r8 r9, rax; clobbers rax rcx r11.
        (
            RegSet::of(&[X(7), X(6), X(2), X(10), X(8), X(9), X(0)]),
            RegSet::of(&[X(0), X(1), X(11)]),
        )
    } else {
        (
            RegSet::of(&[X(0), X(1), X(2), X(3), X(4), X(5), X(8)]),
            RegSet::of(&[X(0)]),
        )
    }
}

/// Syscalls that return to the next instruction with only the documented
/// registers changed and need no unwinding: file and memory management,
/// time, futexes, signal masks. Anything else (fork, clone, execve,
/// rt_sigreturn, kill, and everything not listed) may unwind.
fn benign_syscall(nr: u64) -> bool {
    if gex_abi::X86 {
        matches!(
            nr,
            0..=14 | 16..=22 | 24 | 25 | 28 | 32 | 33 | 35 | 39 | 60 | 63 | 72 | 79 | 89 | 96
                | 102 | 104 | 107 | 108 | 158 | 186 | 202 | 217 | 218 | 228 | 230 | 231 | 257
                | 262 | 267 | 269 | 271 | 273 | 293 | 302 | 318 | 332
        )
    } else {
        matches!(
            nr,
            17 | 24 | 25 | 29 | 48 | 56 | 57 | 59 | 61..=68 | 73 | 78..=80 | 93 | 94 | 96 | 98
                | 99 | 101 | 113 | 115 | 124 | 134 | 135 | 160 | 169 | 172 | 174..=179 | 214..=216
                | 222 | 226 | 233 | 261 | 278 | 291
        )
    }
}

struct Ctx<'a> {
    sums: &'a HashMap<u64, Summary>,
}

impl Ctx<'_> {
    fn callee(&self, c: Callee) -> Summary {
        match c {
            Callee::Direct(a) => self.sums.get(&a).cloned().unwrap_or_else(Summary::unknown),
            Callee::Indirect(_) => Summary::unknown(),
        }
    }
}

/// Evaluate the statements of one block, updating `st`; returns the
/// abstract value of every value the block defines.
fn run_block(b: &FBlock, st: &mut St, acc: &mut Acc) -> HashMap<Value, AV> {
    let mut vals: HashMap<Value, AV> = HashMap::new();
    let get = |vals: &HashMap<Value, AV>, v: Value| {
        vals.get(&v).copied().unwrap_or(AV::top(RegSet::EMPTY))
    };
    for s in &b.stmts {
        match s {
            Stmt::Def { dst, expr } => {
                let av = eval(expr, &vals, st, acc);
                vals.insert(*dst, av);
            }
            Stmt::WriteReg { reg, val } => {
                let av = get(&vals, *val);
                if *reg == GuestReg::Sp && av.sp_off().is_none() && !av.frame() {
                    // The stack pointer set from something not derived from
                    // it: a stack switch (longjmp, setcontext, coroutines).
                    acc.may_unwind = true;
                }
                st.regs[reg_index(*reg)] = av;
                st.written.insert(*reg);
            }
            Stmt::Store {
                addr, offset, val, sz, ..
            } => {
                let a = get(&vals, *addr);
                let v = get(&vals, *val);
                store(st, acc, a, *offset, sz.bytes(), v);
            }
            Stmt::VStore {
                addr,
                offset,
                val,
                bytes,
            } => {
                let a = get(&vals, *addr);
                let v = get(&vals, *val);
                store(st, acc, a, *offset, *bytes, v);
            }
            Stmt::HostCall { func, args } => {
                for a in args {
                    let av = get(&vals, *a);
                    note_addr(acc, av);
                }
                host(st, acc, *func);
            }
            Stmt::MemCopy { dst, src, len } | Stmt::MemCmp { a: dst, b: src, len, .. } => {
                let (d, s_) = (get(&vals, *dst), get(&vals, *src));
                note_addr(acc, d);
                note_addr(acc, s_);
                let _ = len;
                if matches!(s, Stmt::MemCopy { .. }) {
                    bulk_write(st, acc, d);
                }
                // Reads of unknown length from the caller's frame.
                if d.frame() || s_.frame() {
                    acc.stack_inputs = Slots::Unknown;
                }
                if let Stmt::MemCmp { dst: out, .. } = s {
                    vals.insert(*out, AV::top(RegSet::EMPTY));
                }
            }
            Stmt::MemFill { dst, .. } => {
                let d = get(&vals, *dst);
                note_addr(acc, d);
                bulk_write(st, acc, d);
            }
            Stmt::StrOp {
                kind,
                dst,
                src,
                out_count,
                out_dst,
                out_src,
                out_a,
                out_b,
                ..
            } => {
                let (d, s_) = (get(&vals, *dst), get(&vals, *src));
                note_addr(acc, d);
                note_addr(acc, s_);
                if matches!(kind, gex_ir::StrKind::Movs | gex_ir::StrKind::Stos) {
                    bulk_write(st, acc, d);
                }
                if d.frame() || s_.frame() {
                    acc.stack_inputs = Slots::Unknown;
                }
                for o in [out_count, out_dst, out_src, out_a, out_b] {
                    vals.insert(*o, AV::top(d.taint.union(s_.taint)));
                }
            }
            Stmt::Fence => {}
        }
    }
    vals
}

/// An address operand: its entry-register inputs become `addr_inputs`.
fn note_addr(acc: &mut Acc, a: AV) {
    acc.addr_inputs = acc.addr_inputs.union(a.taint.minus(RegSet::of(&[GuestReg::Sp])));
    if a.is_ra() {
        acc.reads_ra = true;
    }
}

/// A write of unknown length at `d`.
fn bulk_write(st: &mut St, acc: &mut Acc, d: AV) {
    match d.sp_off() {
        Some(o) => {
            st.kill(o, i64::MAX);
            if st.escaped {
                st.slots.clear();
            }
            acc.caller_frame_writes = Slots::Unknown;
        }
        None => {
            if d.frame() {
                st.slots.clear();
                acc.caller_frame_writes = Slots::Unknown;
            } else {
                st.unknown_store();
            }
        }
    }
}

fn store(st: &mut St, acc: &mut Acc, a: AV, offset: i64, bytes: u32, v: AV) {
    note_addr(acc, a);
    if v.frame() {
        st.escaped = true;
    }
    if v.is_ra() && a.sp_off().is_none() {
        acc.reads_ra = true;
    }
    match a.sp_off() {
        Some(o) => {
            let at = o + offset;
            st.kill(at, at + bytes as i64);
            st.slots.insert(at, (bytes, v));
            if at + bytes as i64 > 0 {
                acc.caller_frame_writes = acc.caller_frame_writes.add(at.max(0), at + bytes as i64);
            }
        }
        None if a.frame() => {
            // Somewhere in the frame, not placed: anywhere in it.
            st.slots.clear();
            acc.caller_frame_writes = Slots::Unknown;
        }
        None => st.unknown_store(),
    }
}

fn host(st: &mut St, acc: &mut Acc, func: HostFunc) {
    if matches!(func, HostFunc::Illegal | HostFunc::Breakpoint) {
        // A synchronous signal: its handler may leave by longjmp.
        acc.may_unwind = true;
    }
    if func.clobbers_guest_state() {
        for r in st.regs.iter_mut() {
            *r = AV::top(RegSet::EMPTY);
        }
        acc.inputs = acc.inputs.union(RegSet::ALL.minus(st.written));
        st.unknown_store();
        st.escaped = true;
    }
}

fn eval(e: &Expr, vals: &HashMap<Value, AV>, st: &mut St, acc: &mut Acc) -> AV {
    let get = |v: Value| vals.get(&v).copied().unwrap_or(AV::top(RegSet::EMPTY));
    match *e {
        Expr::ConstI32(c) => AV::konst(c as u64),
        Expr::ConstI64(c) => AV::konst(c),
        Expr::ReadReg(r) => {
            if !st.written.contains(r) {
                acc.inputs.insert(r);
            }
            st.regs[reg_index(r)]
        }
        Expr::Bin { op, ty, a, b } => {
            let (x, y) = (get(a), get(b));
            if x.is_ra() || y.is_ra() {
                acc.reads_ra = true;
            }
            let taint = x.taint.union(y.taint);
            if ty == Ty::I64 {
                match (op, x.ex, y.ex) {
                    (BinOp::Add, Ex::Entry(r, o), Ex::Const(c))
                    | (BinOp::Add, Ex::Const(c), Ex::Entry(r, o)) => {
                        return AV {
                            ex: Ex::Entry(r, o.wrapping_add(c as i64)),
                            taint,
                        }
                    }
                    (BinOp::Sub, Ex::Entry(r, o), Ex::Const(c)) => {
                        return AV {
                            ex: Ex::Entry(r, o.wrapping_sub(c as i64)),
                            taint,
                        }
                    }
                    (_, Ex::Const(p), Ex::Const(q)) => {
                        let mut known = vec![None; (a.0.max(b.0) + 1) as usize];
                        known[a.0 as usize] = Some(p);
                        known[b.0 as usize] = Some(q);
                        if let Some(c) = gex_ir::pass::eval_expr(e, &known) {
                            return AV::konst(c);
                        }
                    }
                    _ => {}
                }
            }
            AV::top(taint)
        }
        Expr::Load {
            addr, offset, sz, ..
        } => {
            let a = get(addr);
            note_addr(acc, a);
            let n = sz.bytes();
            match a.sp_off() {
                Some(o) => {
                    let at = o + offset;
                    if at + n as i64 > 0 {
                        acc.stack_inputs = acc.stack_inputs.add(at.max(0), at + n as i64);
                    }
                    match st.slots.get(&at) {
                        Some(&(m, v)) if m == n => v,
                        _ => AV::top(RegSet::EMPTY),
                    }
                }
                None => {
                    if a.frame() {
                        acc.stack_inputs = Slots::Unknown;
                    }
                    AV::top(RegSet::EMPTY)
                }
            }
        }
        Expr::VLoad { addr, .. } => {
            let a = get(addr);
            note_addr(acc, a);
            if a.frame() {
                // Vector slots are not tracked; reading one is still an input.
                match a.sp_off() {
                    Some(o) if o >= 0 => acc.stack_inputs = acc.stack_inputs.add(o, o + 16),
                    Some(_) => {}
                    None => acc.stack_inputs = Slots::Unknown,
                }
            }
            AV::top(RegSet::EMPTY)
        }
        Expr::Rmw { addr, .. } | Expr::CmpXchg { addr, .. } => {
            let a = get(addr);
            let mut t = RegSet::EMPTY;
            e.for_each_use(|u| t = t.union(get(u).taint));
            store(st, acc, a, 0, 8, AV::top(t));
            AV::top(RegSet::EMPTY)
        }
        Expr::HostCall { func, ref args, .. } => {
            for a in args {
                note_addr(acc, get(*a));
            }
            host(st, acc, func);
            AV::top(RegSet::EMPTY)
        }
        _ => {
            let mut t = RegSet::EMPTY;
            let mut ra = false;
            e.for_each_use(|u| {
                let v = get(u);
                t = t.union(v.taint);
                ra |= v.is_ra();
            });
            if ra {
                acc.reads_ra = true;
            }
            AV::top(t)
        }
    }
}

/// Apply a call's effect to the state at the call. Returns false when the
/// callee never returns (the continuation is dead).
fn call(st: &mut St, acc: &mut Acc, s: &Summary) -> bool {
    // Inputs the callee reads that this function has not written.
    acc.inputs = acc.inputs.union(s.inputs.minus(st.written));
    // Registers it uses as addresses: our entry values in them become ours,
    // and a frame address in one lets the frame escape to it.
    for r in s.addr_inputs.iter() {
        let v = st.regs[reg_index(r)];
        acc.addr_inputs = acc.addr_inputs.union(v.taint.minus(RegSet::of(&[GuestReg::Sp])));
        if v.frame() {
            st.escaped = true;
        }
        if v.is_ra() {
            acc.reads_ra = true;
        }
    }
    if s.leaks_sp() {
        st.escaped = true;
    }
    // The callee's stack accesses relative to its entry sp, which is ours
    // at the call.
    let sp = st.regs[SP];
    match sp.sp_off() {
        Some(o) => {
            let shift_in = s.stack_inputs.shift(o);
            if let Slots::Range(a, b) = shift_in {
                if b > 0 {
                    acc.stack_inputs = acc.stack_inputs.add(a.max(0), b);
                }
            } else if shift_in == Slots::Unknown {
                acc.stack_inputs = Slots::Unknown;
            }
            let w = s.caller_frame_writes.shift(o);
            st.kill_slots(w);
            match w {
                Slots::Range(a, b) if b > 0 => {
                    acc.caller_frame_writes = acc.caller_frame_writes.add(a.max(0), b)
                }
                Slots::Unknown => acc.caller_frame_writes = Slots::Unknown,
                _ => {}
            }
        }
        None => {
            if s.stack_inputs != Slots::None {
                acc.stack_inputs = Slots::Unknown;
            }
            if s.caller_frame_writes != Slots::None {
                st.slots.clear();
                acc.caller_frame_writes = Slots::Unknown;
            }
        }
    }
    if st.escaped {
        st.slots.clear();
    }
    acc.may_unwind |= s.may_unwind;
    acc.may_leave_pending |= s.may_leave_pending;
    acc.unknown_callee |= s.unknown_callee;
    if !s.may_return {
        return false;
    }
    // Outputs: derived from anything the callee read, except its own stack
    // pointer (a callee handing back its caller's frame is `leaks_sp`).
    let mut t = RegSet::EMPTY;
    for r in s.inputs.iter() {
        if r != GuestReg::Sp {
            t = t.union(st.regs[reg_index(r)].taint);
        }
    }
    let new_sp = match (sp.sp_off(), s.sp_delta) {
        (Some(o), Some(d)) => Some(AV {
            ex: Ex::Entry(SP as u8, o + d),
            taint: sp.taint,
        }),
        _ => None,
    };
    for i in 0..N_REGS {
        if !s.preserved.contains(reg_of_index(i)) {
            st.regs[i] = AV::top(t);
        }
    }
    st.regs[SP] = match new_sp {
        Some(v) => v,
        None if s.preserved.contains(GuestReg::Sp) => sp,
        None => AV::top(sp.taint),
    };
    true
}

/// A path that leaves to code nothing is known about.
fn unknown_exit(acc: &mut Acc, st: &St) {
    acc.inputs = acc.inputs.union(RegSet::ALL.minus(st.written));
    acc.stack_inputs = Slots::Unknown;
    acc.caller_frame_writes = Slots::Unknown;
    acc.addr_inputs = RegSet::ALL;
    acc.may_unwind = true;
    acc.may_leave_pending = true;
    acc.unknown_callee = true;
    acc.unknown_exit = true;
    if st.escaped || st.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame()) {
        acc.leaks_sp = true;
    }
}

/// Registers holding their entry value in `st`.
fn unchanged(st: &St) -> RegSet {
    let mut s = RegSet::EMPTY;
    for i in 0..N_REGS {
        if st.regs[i].ex == Ex::Entry(i as u8, 0) {
            s.insert(reg_of_index(i));
        }
    }
    s
}

/// Analyse one function given the current summaries.
fn analyze_fn(f: &Function, ctx: &Ctx) -> Summary {
    let n = f.blocks.len();
    let mut acc = Acc {
        inputs: RegSet::EMPTY,
        stack_inputs: Slots::None,
        caller_frame_writes: Slots::None,
        addr_inputs: RegSet::EMPTY,
        may_unwind: false,
        may_leave_pending: false,
        unknown_callee: false,
        reads_ra: false,
        leaks_sp: false,
        exits: Vec::new(),
        unknown_exit: false,
    };
    let rpo = f.rpo();
    let mut order = vec![usize::MAX; n];
    for (i, b) in rpo.iter().enumerate() {
        order[b.index()] = i;
    }
    let mut state_in: Vec<Option<St>> = vec![None; n];
    state_in[0] = Some(St::entry());
    // Worklist by RPO position, so loops settle before what follows them.
    let mut work: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    work.insert(order[0]);
    let mut visits = vec![0u32; n];
    // Exits are recorded on the final visit of each block only: collect
    // them per block and overwrite on revisit.
    let mut exits_of: Vec<Option<(RegSet, Option<i64>)>> = vec![None; n];
    let mut unknown_of: Vec<bool> = vec![false; n];
    while let Some(pos) = work.pop_first() {
        let b = rpo[pos];
        let bi = b.index();
        visits[bi] += 1;
        let mut st = state_in[bi].clone().unwrap();
        // Joins go straight to unknown, so this terminates on its own; the
        // cap only bounds pathological inputs.
        if visits[bi] > 1000 {
            for r in st.regs.iter_mut() {
                if matches!(r.ex, Ex::Entry(_, o) if o != 0) || matches!(r.ex, Ex::Const(_)) {
                    *r = AV::top(r.taint);
                }
            }
            st.slots.clear();
        }
        let blk = f.block(b);
        let vals = run_block(blk, &mut st, &mut acc);
        let get = |v: Value| vals.get(&v).copied().unwrap_or(AV::top(RegSet::EMPTY));
        exits_of[bi] = None;
        unknown_of[bi] = false;
        let mut succ_state: Vec<(BlockId, St)> = Vec::new();
        match &blk.term {
            FTerm::Jump(e) => succ_state.push((e.to, st.clone())),
            FTerm::Branch {
                taken, not_taken, ..
            } => {
                succ_state.push((taken.to, st.clone()));
                succ_state.push((not_taken.to, st.clone()));
            }
            FTerm::Switch {
                cases,
                table,
                target,
            } => {
                note_addr(&mut acc, get(*target));
                for (_, e) in cases {
                    succ_state.push((e.to, st.clone()));
                }
                if !table.complete {
                    unknown_exit(&mut acc, &st);
                    unknown_of[bi] = true;
                }
            }
            FTerm::Call { callee, ret, .. } => {
                let s = ctx.callee(*callee);
                if let Callee::Indirect(v) = callee {
                    note_addr(&mut acc, get(*v));
                }
                let mut after = st.clone();
                if call(&mut after, &mut acc, &s) {
                    if let Some(e) = ret {
                        succ_state.push((e.to, after));
                    }
                }
            }
            FTerm::TailCall { callee } => {
                let s = ctx.callee(*callee);
                if let Callee::Indirect(v) = callee {
                    note_addr(&mut acc, get(*v));
                }
                if s.unknown_callee && matches!(callee, Callee::Indirect(_)) {
                    unknown_exit(&mut acc, &st);
                    unknown_of[bi] = true;
                } else {
                    let before = unchanged(&st);
                    let sp = st.regs[SP];
                    let mut after = st.clone();
                    if call(&mut after, &mut acc, &s) {
                        let d = match (sp.sp_off(), s.sp_delta) {
                            (Some(o), Some(d)) => Some(o + d),
                            _ => None,
                        };
                        if s.unknown_callee {
                            unknown_of[bi] = true;
                        }
                        exits_of[bi] = Some((before.inter(s.preserved), d));
                        if after.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame()) {
                            acc.leaks_sp = true;
                        }
                    }
                }
            }
            FTerm::Return { target } => {
                let t = get(*target);
                let good = if gex_abi::X86 {
                    t.ex == Ex::RetAddr
                } else {
                    t.ex == Ex::Entry(30, 0)
                };
                if !good {
                    acc.may_leave_pending = true;
                }
                let d = st.regs[SP].sp_off();
                exits_of[bi] = Some((unchanged(&st), d));
                if st.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame() && r.sp_off().is_none_or(|o| o >= 0)) {
                    acc.leaks_sp = true;
                }
            }
            FTerm::Unresolved { target } => {
                note_addr(&mut acc, get(*target));
                unknown_exit(&mut acc, &st);
                unknown_of[bi] = true;
            }
            FTerm::Syscall { next, .. } => {
                let (reads, clobbers) = syscall_regs();
                acc.inputs = acc.inputs.union(reads.minus(st.written));
                let nr = match st.regs[reg_index(SYSCALL_NR)].ex {
                    Ex::Const(c) => Some(c),
                    _ => None,
                };
                let mut after = st.clone();
                // Pointer arguments: the kernel may write through them.
                let frame_arg = reads.iter().any(|r| after.regs[reg_index(r)].frame());
                if frame_arg {
                    after.escaped = true;
                }
                for r in reads.iter() {
                    let v = after.regs[reg_index(r)];
                    acc.addr_inputs = acc.addr_inputs.union(v.taint.minus(RegSet::of(&[GuestReg::Sp])));
                }
                after.unknown_store();
                match nr {
                    Some(nr) if benign_syscall(nr) => {
                        for r in clobbers.iter() {
                            after.regs[reg_index(r)] = AV::top(RegSet::EMPTY);
                        }
                    }
                    _ => {
                        acc.may_unwind = true;
                        for r in after.regs.iter_mut() {
                            *r = AV::top(RegSet::EMPTY);
                        }
                        after.slots.clear();
                    }
                }
                if let Some(e) = next {
                    succ_state.push((e.to, after));
                }
            }
            FTerm::Illegal { .. } => acc.may_unwind = true,
        }
        for (s, ns) in succ_state {
            let si = s.index();
            let merged = match &state_in[si] {
                None => ns,
                Some(old) => old.join(&ns),
            };
            if state_in[si].as_ref() != Some(&merged) {
                state_in[si] = Some(merged);
                if order[si] != usize::MAX {
                    work.insert(order[si]);
                }
            }
        }
    }
    acc.exits = exits_of.into_iter().flatten().collect();
    let unknown_exit = acc.unknown_exit || unknown_of.iter().any(|&u| u);

    let may_return = !acc.exits.is_empty() || unknown_exit;
    let (preserved, sp_delta) = if unknown_exit {
        (RegSet::EMPTY, None)
    } else if acc.exits.is_empty() {
        (RegSet::ALL, None)
    } else {
        let p = acc.exits.iter().fold(RegSet::ALL, |a, (s, _)| a.inter(*s));
        let d0 = acc.exits[0].1;
        let d = if acc.exits.iter().all(|(_, d)| *d == d0) { d0 } else { None };
        (p, d)
    };
    let mut s = Summary {
        inputs: acc.inputs,
        stack_inputs: acc.stack_inputs,
        outputs: RegSet::ALL.minus(preserved),
        preserved,
        sp_delta,
        caller_frame_writes: acc.caller_frame_writes,
        addr_inputs: acc.addr_inputs,
        may_return,
        may_unwind: acc.may_unwind,
        may_leave_pending: acc.may_leave_pending,
        unknown_callee: acc.unknown_callee,
        reads_return_address: acc.reads_ra,
    };
    if !may_return {
        s.outputs = RegSet::EMPTY;
    }
    s.set_leaks_sp(acc.leaks_sp);
    s
}

/// Leaking the frame is folded into `addr_inputs`' Sp bit: a function whose
/// entry stack pointer (a caller's frame boundary) escapes says so there.
trait LeaksSp {
    fn leaks_sp(&self) -> bool;
    fn set_leaks_sp(&mut self, on: bool);
}

impl LeaksSp for Summary {
    fn leaks_sp(&self) -> bool {
        self.addr_inputs.contains(GuestReg::Sp)
    }
    fn set_leaks_sp(&mut self, on: bool) {
        if on {
            self.addr_inputs.insert(GuestReg::Sp);
        }
    }
}

// ---------------------------------------------------------------------------
// The call graph fixpoint
// ---------------------------------------------------------------------------

/// Tarjan's SCCs, returned callees-first (reverse topological order of the
/// call graph).
fn sccs(nodes: &[u64], edges: &HashMap<u64, Vec<u64>>) -> Vec<Vec<u64>> {
    let idx: HashMap<u64, usize> = nodes.iter().enumerate().map(|(i, &a)| (a, i)).collect();
    let n = nodes.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut out: Vec<Vec<u64>> = Vec::new();
    let mut next = 0usize;
    let succ: Vec<Vec<usize>> = nodes
        .iter()
        .map(|a| {
            edges
                .get(a)
                .map(|v| v.iter().filter_map(|b| idx.get(b).copied()).collect())
                .unwrap_or_default()
        })
        .collect();
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // Iterative Tarjan: (node, next successor position).
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&mut (v, ref mut i)) = call.last_mut() {
            if *i < succ[v].len() {
                let w = succ[v][*i];
                *i += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    call.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    low[p] = low[p].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on[w] = false;
                        comp.push(nodes[w]);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
            }
        }
    }
    out
}

/// Compute every function's summary.
pub fn analyze(prog: &Program) -> Summaries {
    analyze_functions(&prog.functions)
}

/// [`analyze`] over any set of functions.
pub fn analyze_functions(functions: &BTreeMap<u64, Function>) -> Summaries {
    let nodes: Vec<u64> = functions.keys().copied().collect();
    let mut edges: HashMap<u64, Vec<u64>> = HashMap::new();
    for (&a, f) in functions {
        let mut v: Vec<u64> = f
            .call_sites()
            .iter()
            .filter_map(|c| match c.callee {
                Callee::Direct(t) if functions.contains_key(&t) => Some(t),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        edges.insert(a, v);
    }
    let comps = sccs(&nodes, &edges);
    let mut map: HashMap<u64, Summary> = HashMap::new();
    let mut stats = SummaryStats {
        sccs: comps.len(),
        ..Default::default()
    };
    for comp in comps {
        let recursive = comp.len() > 1 || edges[&comp[0]].contains(&comp[0]);
        if recursive {
            stats.recursive_sccs += 1;
            for &a in &comp {
                map.insert(a, Summary::bottom());
            }
        }
        let mut iters = 0;
        loop {
            iters += 1;
            let mut changed = false;
            for &a in &comp {
                let s = analyze_fn(&functions[&a], &Ctx { sums: &map });
                if map.get(&a) != Some(&s) {
                    map.insert(a, s);
                    changed = true;
                }
            }
            if !recursive || !changed {
                break;
            }
            if iters >= MAX_ITERATIONS {
                stats.unconverged += 1;
                for &a in &comp {
                    map.insert(a, Summary::unknown());
                }
                break;
            }
        }
        stats.max_iterations = stats.max_iterations.max(iters);
    }
    stats.unknown = map.values().filter(|s| s.unknown_callee).count();
    Summaries { map, stats }
}
