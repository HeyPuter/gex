//! Whole-program call-graph summaries ([`Summary`]).
//!
//! Each function is analysed by abstract interpretation over its memory-form
//! CFG, with its callees' summaries standing in for the calls, and the
//! results are iterated to a fixpoint over the call graph's strongly
//! connected components, callees first. The rule: **summaries over-approximate and never guess.**
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
//! uses that register as an address or may store it
//! ([`Summary::escaping_inputs`]), or handed to a syscall). A callee's
//! writes above its entry stack pointer ([`Summary::caller_frame_writes`])
//! kill exactly the slots they cover. Nothing is kept below the stack
//! pointer less the red zone (x86-64's 128 bytes; none on AArch64), where a
//! signal frame may land at any time, nor below the stack pointer at a call
//! (the callee's frame). A reload narrower than 8 bytes gives the stored
//! value's low bytes, extended, never the entry value itself.
//!
//! Where values go is tracked as well: every entry register a value was
//! computed from is in its taint, so a value that reaches memory other code
//! can read (anything but this function's own unescaped frame), the kernel
//! (for the few syscalls that keep a pointer, or any syscall not known to
//! be harmless) or unknown code makes its taint [`Summary::escaping_inputs`],
//! and what reaches an output register at a return makes
//! [`Summary::output_sources`]. A caller uses both: a frame address in an
//! escaping input lets its frame escape, and a callee's outputs are tainted
//! by the caller's values in its output sources only.
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

use crate::arch::{SP_REG, SYSCALL_NR};
use crate::func::Program;
use gex_ir::func::*;
use gex_ir::ssa::CallEffect;
use gex_ir::{BinOp, Expr, GuestReg, HostFunc, Stmt, Ty, Value};
use std::collections::{BTreeMap, HashMap};

/// Iterations of one SCC before giving up on it (setting it unknown).
pub const MAX_ITERATIONS: usize = 64;

/// Iterations of one SCC after which a slot range still changing is widened
/// to unknown.
pub const WIDEN_AFTER: usize = 16;

/// The summaries of a whole program.
pub struct Summaries {
    pub map: HashMap<u64, Summary>,
    pub stats: SummaryStats,
    /// The guards the summaries were computed with ([`analyze_with_guards`];
    /// empty for [`analyze`]).
    pub guards: Guards,
    /// What each guarded call site was assumed to do, by (function, calling
    /// instruction): what SSA construction and the interpreter checks must
    /// assume there too.
    pub site_effects: HashMap<(u64, u64), CallEffect>,
    /// Functions that are syscall stubs (see [`Guards`]), with how far each
    /// moves the stack pointer.
    pub stubs: HashMap<u64, i64>,
}

/// Guarded targets of indirect calls, by the calling instruction's address:
/// the function tier compares the computed target with these and takes the
/// known callee's path on a match.
///
/// **Contract.** A summary computed with guards
/// ([`analyze_with_guards`]) is valid only if every guard miss leaves the
/// function tier before any instruction after the guarded call executes:
/// the miss writes guest state back, sets the PC to the actual target and
/// returns to the host loop, and every function-tier caller up the nest
/// leaves through its return check, so none continues under an assumption
/// the miss broke. A backend that cannot promise that must use [`analyze`].
///
/// At a guarded site the callee's summary is the join of its targets'
/// (unknown if any target has none). A site whose targets are all syscall
/// stubs (`int $0x80; ret`, `syscall; ret`, `svc #0; ret`: i386 musl's
/// `__vsyscall`, called through `%gs:0x10`) is the syscall itself, with the
/// syscall number the caller set ([`vsyscall_guards`]).
#[derive(Clone, Debug, Default)]
pub struct Guards {
    pub calls: HashMap<u64, Vec<u64>>,
}

#[derive(Clone, Debug, Default)]
pub struct SummaryStats {
    pub sccs: usize,
    pub recursive_sccs: usize,
    /// The most iterations any SCC took to converge.
    pub max_iterations: usize,
    /// SCCs that did not converge and were set to unknown.
    pub unconverged: usize,
    /// Their members.
    pub unconverged_members: Vec<u64>,
    /// Functions whose summary has `unknown_callee`.
    pub unknown: usize,
}

impl Summaries {
    /// The summary of the function at `a`, or [`Summary::unknown`] for code
    /// that was not recovered.
    pub fn get(&self, a: u64) -> Summary {
        self.map.get(&a).cloned().unwrap_or_else(Summary::unknown)
    }

    /// `f` in SSA form, with these summaries standing in for its calls
    /// and the guest's stack pointer ([`crate::func::to_ssa`]).
    pub fn to_ssa(&self, f: &Function) -> Function {
        let eff = self.effects_for(&[f]);
        crate::func::to_ssa(f, &eff)
    }

    /// What each call of the function `forms[0]` (in any of its `forms`:
    /// memory and SSA) does, as these summaries assumed: a guarded site's
    /// recorded effect, found by its target value, which names one call
    /// site of a form. A value two sites share (after SSA renaming) gets the
    /// meet of their effects.
    pub fn effects_for<'a>(&'a self, forms: &[&Function]) -> impl Fn(Callee) -> CallEffect + 'a {
        let entry = forms[0].entry;
        let mut by_value: HashMap<Value, Option<CallEffect>> = HashMap::new();
        if !self.site_effects.is_empty() {
            for f in forms {
                for b in &f.blocks {
                    if let FTerm::Call {
                        callee: Callee::Indirect(v),
                        ..
                    } = b.term
                    {
                        if let Some(&e) = self.site_effects.get(&(entry, b.term_pc)) {
                            let slot = by_value.entry(v).or_insert(Some(e));
                            if let Some(old) = *slot {
                                *slot = Some(CallEffect {
                                    preserved: old.preserved.inter(e.preserved),
                                    sp_delta: if old.sp_delta == e.sp_delta { e.sp_delta } else { None },
                                });
                            }
                        }
                    }
                }
            }
        }
        move |c| match c {
            Callee::Indirect(v) => by_value.get(&v).copied().flatten().unwrap_or(CallEffect::UNKNOWN),
            c => self.call_effect(c),
        }
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
        "in={:?} stack_in={:?} preserved={:?} sp_delta={:?} caller_writes={:?} addr_in={:?} escapes={:?} out_src={:?} [{}]",
        s.inputs,
        s.stack_inputs,
        s.preserved,
        s.sp_delta,
        s.caller_frame_writes,
        s.addr_inputs,
        s.escaping_inputs,
        s.output_sources,
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
    /// The low 32 bits of the entry value of register `r` plus an offset,
    /// as an `I32`: what `wrap` makes of `Entry` (a 32-bit register read,
    /// every address i386 code forms). The offset is kept in `i32` range.
    Low(u8, i64),
    /// x86: the return address, as loaded from the entry stack pointer.
    RetAddr,
    Top,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct AV {
    ex: Ex,
    /// Entry registers this value was computed from by arithmetic.
    taint: RegSet,
}

/// [`SP_REG`]'s index ([`reg_index`]).
const SP: usize = if gex_abi::X86 { 4 } else { 31 };

/// An offset from an entry value, as the guest computes it. In i386 code
/// every register holds a zero-extended 32-bit value and 32-bit address
/// arithmetic wraps, so `Entry` values are compared modulo 2^32: an offset
/// is kept in `i32` range. (A stack pointer whose frame straddles address
/// zero or 4 GiB is the one case this would equate wrongly, and the lifter
/// itself addresses such a push differently from the CPU.)
fn norm(o: i64) -> i64 {
    if crate::arch::compat() {
        o as i32 as i64
    } else {
        o
    }
}

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
        self.taint.contains(SP_REG)
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
    /// Optimistic mode ([`known_values_trusting_frame`]): this function's
    /// own slots survive an escaped frame, as if no one wrote the frame
    /// through a pointer it was handed.
    trust_frame: bool,
}

impl St {
    fn entry() -> St {
        let mut slots = BTreeMap::new();
        if gex_abi::X86 {
            slots.insert(
                0,
                (
                    crate::arch::ptr_bytes(),
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
            trust_frame: false,
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
            trust_frame: self.trust_frame,
        }
    }

    /// The lowest stack offset a slot may be kept at: the stack pointer less
    /// the red zone (x86-64's 128 bytes the kernel's signal delivery skips;
    /// AArch64 has none). Everything when the stack pointer is not known.
    fn floor(&self) -> i64 {
        // The x86-64 psABI's red zone; i386 has none.
        let red = if gex_abi::X86 && !crate::arch::compat() { 128 } else { 0 };
        match self.regs[SP].sp_off() {
            Some(o) => o - red,
            None => i64::MAX,
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
        if self.trust_frame {
            // Optimistic: a pointer nobody derived from the stack pointer
            // does not point into a frame.
        } else if self.escaped {
            self.slots.clear();
        } else {
            self.kill(0, i64::MAX);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-function analysis
// ---------------------------------------------------------------------------

/// A syscall from state `st`: the state after it, and its number when
/// known.
fn syscall_step(st: &St, acc: &mut Acc) -> (St, Option<u64>) {
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
            escape(&mut after, acc);
        }
        // The kernel may keep what it is handed (set_tid_address,
        // set_robust_list, and whatever a syscall not known to be
        // harmless does).
        let keeps = nr.map_or(true, |nr| !benign_syscall(nr) || syscall_keeps_args(nr));
        for r in reads.iter() {
            let v = after.regs[reg_index(r)];
            acc.addr_inputs = acc.addr_inputs.union(v.taint.minus(RegSet::of(&[SP_REG])));
            if keeps {
                acc.escapes = acc.escapes.union(v.taint);
            }
        }
        after.unknown_store();
        match nr {
            Some(nr) if benign_syscall(nr) => {
                for r in clobbers.iter() {
                    after.regs[reg_index(r)] = AV::top(RegSet::EMPTY);
                }
            }
            _ => {
                // A signal handler sees every register.
                acc.may_unwind = true;
                escape_regs(&after, acc);
                for r in after.regs.iter_mut() {
                    *r = AV::top(RegSet::EMPTY);
                }
                after.slots.clear();
            }
        }
    (after, nr)
}

/// A syscall stub: a function that is a syscall instruction and a return
/// (`int $0x80; ret`, `syscall; ret`, `svc #0; ret`), touching no register
/// but the stack pointer on the way. Returns how far the return moves the
/// stack pointer.
fn syscall_stub(f: &Function) -> Option<i64> {
    let lifted: Vec<&FBlock> = f.blocks.iter().filter(|b| !b.synthetic).collect();
    if lifted.len() != 2 {
        return None;
    }
    let FTerm::Syscall { next: Some(e), .. } = &f.blocks[0].term else {
        return None;
    };
    let quiet = |b: &FBlock, allow: &[GuestReg]| {
        b.stmts.iter().all(|s| match s {
            Stmt::WriteReg { reg, .. } => *reg == GuestReg::Pc || allow.contains(reg),
            Stmt::Def { .. } => true,
            _ => false,
        })
    };
    let mut to = e.to;
    // A synthetic landing block may sit between them.
    if f.block(to).synthetic {
        match &f.block(to).term {
            FTerm::Jump(e2) => to = e2.to,
            _ => return None,
        }
    }
    let ret = f.block(to);
    if !(quiet(&f.blocks[0], &[]) && matches!(ret.term, FTerm::Return { .. }) && quiet(ret, &[SP_REG])) {
        return None;
    }
    // The stack pointer at the return: entry + k, read from the block.
    let mut st = St::entry();
    let mut acc = Acc::empty(f.blocks.len());
    run_block(ret, &mut st, &mut acc);
    st.regs[SP].sp_off()
}

/// The frame's address has escaped: from here on other code may read (and
/// write) it, so whatever its slots hold escapes too.
fn escape(st: &mut St, acc: &mut Acc) {
    st.escaped = true;
    for (_, v) in st.slots.values() {
        acc.escapes = acc.escapes.union(v.taint);
    }
}

/// Every value the registers hold may be read by other code (a clobbering
/// host call, a signal).
fn escape_regs(st: &St, acc: &mut Acc) {
    for r in &st.regs {
        acc.escapes = acc.escapes.union(r.taint);
    }
}

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
    /// Entry registers whose values may be put where other code reads them.
    escapes: RegSet,
    /// Entry registers whose values may reach an output register.
    out_src: RegSet,
    /// Per return path: (registers preserved on it, sp delta if known).
    exits: Vec<(RegSet, Option<i64>, RegSet)>,
    /// Some path leaves to code nothing is known about.
    unknown_exit: bool,
    /// Per block: ends in an unresolved jump taken with the frame torn
    /// down (stack pointer and return address as on entry).
    torn_down: Vec<bool>,
}

impl Acc {
    fn empty(n: usize) -> Acc {
        Acc {
            inputs: RegSet::EMPTY,
            stack_inputs: Slots::None,
            caller_frame_writes: Slots::None,
            addr_inputs: RegSet::EMPTY,
            may_unwind: false,
            may_leave_pending: false,
            unknown_callee: false,
            reads_ra: false,
            leaks_sp: false,
            escapes: RegSet::EMPTY,
            out_src: RegSet::EMPTY,
            torn_down: vec![false; n],
            exits: Vec::new(),
            unknown_exit: false,
        }
    }
}

/// Registers the kernel reads for a syscall, and those it may change.
fn syscall_regs() -> (RegSet, RegSet) {
    use GuestReg::X;
    if crate::arch::compat() {
        // int 0x80: eax, then ebx ecx edx esi edi ebp; clobbers eax.
        (
            RegSet::of(&[X(0), X(3), X(1), X(2), X(6), X(7), X(5)]),
            RegSet::of(&[X(0)]),
        )
    } else if gex_abi::X86 {
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
    if crate::arch::compat() {
        // The same calls by their i386 numbers (plus the 32-bit variants:
        // mmap2, _llseek, stat64, fcntl64, clock_gettime64, ...).
        matches!(
            nr,
            1 | 3..=6 | 19 | 20 | 33 | 45 | 54 | 55 | 60 | 63 | 64 | 78 | 90 | 91 | 106..=108
                | 122 | 125 | 140 | 145 | 146 | 158 | 162 | 168 | 174 | 175 | 183 | 192
                | 195..=202 | 219..=221 | 224 | 240 | 243 | 252 | 258 | 265..=267 | 295
                | 300 | 311 | 340 | 355 | 383 | 403 | 406 | 407
        )
    } else if gex_abi::X86 {
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

/// Of the syscalls [`benign_syscall`] lists, those whose kernel side keeps
/// a pointer it is handed after returning: `set_tid_address`,
/// `set_robust_list`. Their arguments escape; the others' do not (the
/// kernel copies what they point at, or writes through it before
/// returning). Every other syscall's arguments escape.
fn syscall_keeps_args(nr: u64) -> bool {
    if crate::arch::compat() {
        // set_tid_address, set_robust_list, set_thread_area (the kernel
        // keeps the TLS base the descriptor names).
        matches!(nr, 258 | 311 | 243)
    } else if gex_abi::X86 {
        matches!(nr, 218 | 273)
    } else {
        matches!(nr, 96 | 99)
    }
}

/// What the analysis assumes a syscall does to the registers, given its
/// number when known: one on the [`benign_syscall`] list changes only the
/// kernel's documented result and scratch registers and returns with the
/// stack pointer unchanged; any other may change everything.
pub fn syscall_effect(nr: Option<u64>) -> CallEffect {
    match nr {
        Some(nr) if benign_syscall(nr) => CallEffect {
            preserved: RegSet::ALL.minus(syscall_regs().1),
            sp_delta: Some(0),
        },
        _ => CallEffect::UNKNOWN,
    }
}

struct Ctx<'a> {
    sums: &'a HashMap<u64, Summary>,
    /// See [`St::trust_frame`]. Never for summaries.
    trust_frame: bool,
    /// Guarded indirect call sites, and the functions that are syscall
    /// stubs (see [`Guards`]).
    guards: Option<&'a Guards>,
    stubs: Option<&'a HashMap<u64, i64>>,
}

impl Ctx<'_> {
    /// The guard targets of the call at `site`, when it is an indirect call
    /// with guards.
    fn guarded(&self, c: Callee, site: u64) -> Option<&Vec<u64>> {
        match c {
            Callee::Indirect(_) => self.guards?.calls.get(&site).filter(|t| !t.is_empty()),
            Callee::Direct(_) => None,
        }
    }

    /// The callee's summary at `site`: a guarded call's targets joined.
    fn callee_at(&self, c: Callee, site: u64) -> Summary {
        match self.guarded(c, site) {
            Some(ts) => {
                let mut it = ts.iter().map(|t| self.callee(Callee::Direct(*t)));
                let first = it.next().unwrap();
                it.fold(first, |a, b| join_summaries(&a, &b))
            }
            None => self.callee(c),
        }
    }

    /// Whether every guard target at `site` is a syscall stub.
    fn stub_site(&self, c: Callee, site: u64) -> bool {
        match (self.guarded(c, site), self.stubs) {
            (Some(ts), Some(stubs)) => ts.iter().all(|t| stubs.contains_key(t)),
            _ => false,
        }
    }

    fn callee(&self, c: Callee) -> Summary {
        let s = match c {
            Callee::Direct(a) => self.sums.get(&a).cloned(),
            Callee::Indirect(_) => None,
        };
        match s {
            Some(s) if !(self.trust_frame && s.unknown_callee) => s,
            _ if self.trust_frame => abi_callee(),
            _ => Summary::unknown(),
        }
    }
}

/// Optimistic mode only: a callee nothing is known about, assumed to keep
/// the platform ABI (callee-saved registers survive; the stack pointer moves
/// by the return address it pops, except on i386 where stdcall callees pop
/// their arguments too) and to leave this frame alone.
fn abi_callee() -> Summary {
    use GuestReg::X;
    let (saved, delta) = if crate::arch::compat() {
        (RegSet::of(&[X(3), X(5), X(6), X(7)]), None)
    } else if gex_abi::X86 {
        (RegSet::of(&[X(3), X(5), X(12), X(13), X(14), X(15)]), Some(8))
    } else {
        let mut s = RegSet::of(&[GuestReg::Sp]);
        for r in 19..=29 {
            s.insert(X(r));
        }
        (s, Some(0))
    };
    Summary {
        preserved: saved,
        outputs: RegSet::ALL.minus(saved),
        sp_delta: delta,
        caller_frame_writes: Slots::None,
        ..Summary::unknown()
    }
}

/// Evaluate the statements of one block, updating `st`; returns the
/// abstract value of every value the block defines.
fn run_block(b: &FBlock, st: &mut St, acc: &mut Acc) -> HashMap<Value, AV> {
    // A join may have brought slots of an unescaped path into an escaped
    // frame.
    if st.escaped {
        escape(st, acc);
    }
    let mut vals: HashMap<Value, AV> = HashMap::new();
    let get = |vals: &HashMap<Value, AV>, v: Value| {
        vals.get(&v).copied().unwrap_or(AV::top(RegSet::EMPTY))
    };
    let mut at_pc = None;
    for (s, &pc) in b.stmts.iter().zip(&b.pcs) {
        // Between instructions, a signal frame may land below the stack
        // pointer (and its red zone). Within one (`push`: store, then move
        // the stack pointer) nothing can.
        if at_pc != Some(pc) {
            st.kill(i64::MIN, st.floor());
            at_pc = Some(pc);
        }
        match s {
            Stmt::Def { dst, expr } => {
                let av = eval(expr, &vals, st, acc);
                vals.insert(*dst, av);
            }
            Stmt::WriteReg { reg, val } => {
                let av = get(&vals, *val);
                if *reg == SP_REG && av.sp_off().is_none() && !av.frame() {
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
                    acc.escapes = acc.escapes.union(av.taint);
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
                // Whatever the frame holds may be copied anywhere.
                if s_.frame() && matches!(s, Stmt::MemCopy { .. }) {
                    escape_slots(st, acc);
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
                if s_.frame() && matches!(kind, gex_ir::StrKind::Movs) {
                    escape_slots(st, acc);
                }
                for o in [out_count, out_dst, out_src, out_a, out_b] {
                    vals.insert(*o, AV::top(d.taint.union(s_.taint)));
                }
            }
            Stmt::Fence => {}
            // Pure arithmetic on its arguments: no register, memory or
            // control effect; its outputs are unknown values carrying the
            // arguments' taint.
            Stmt::X87 { args, outs, .. } => {
                let mut taint = RegSet::EMPTY;
                for a in args {
                    taint = taint.union(get(&vals, *a).taint);
                }
                for o in outs {
                    vals.insert(*o, AV::top(taint));
                }
            }
            Stmt::Trap { .. } => {
                st.kill(i64::MIN, st.floor());
                // Taken, it hands the exact context to a signal handler,
                // which may leave by longjmp: every register is read and
                // may escape. Not taken, nothing happens, so the state
                // carries on unchanged. (Like an illegal instruction, a
                // handler that resumes the frame elsewhere is not modelled.)
                acc.may_unwind = true;
                acc.inputs = acc.inputs.union(RegSet::ALL.minus(st.written));
                escape_regs(st, acc);
            }
        }
    }
    // The block's last instruction is done too.
    st.kill(i64::MIN, st.floor());
    vals
}

/// The slots' values may be copied somewhere other code reads.
fn escape_slots(st: &St, acc: &mut Acc) {
    for (_, v) in st.slots.values() {
        acc.escapes = acc.escapes.union(v.taint);
    }
}

/// An address operand: its entry-register inputs become `addr_inputs`.
fn note_addr(acc: &mut Acc, a: AV) {
    acc.addr_inputs = acc.addr_inputs.union(a.taint.minus(RegSet::of(&[SP_REG])));
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
                if !st.trust_frame {
                    st.slots.clear();
                }
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
        escape(st, acc);
    }
    // The value escapes unless it goes into this function's own frame while
    // nobody else can read that.
    let private = match a.sp_off() {
        Some(o) => !st.escaped && o + offset + bytes as i64 <= 0,
        None => false,
    };
    if !private {
        acc.escapes = acc.escapes.union(v.taint);
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
            // Somewhere in the frame, not placed: anywhere in it (unless
            // optimistic: an indexed store stays inside its own array).
            if !st.trust_frame {
                st.slots.clear();
            }
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
        escape_regs(st, acc);
        escape(st, acc);
        for r in st.regs.iter_mut() {
            *r = AV::top(RegSet::EMPTY);
        }
        acc.inputs = acc.inputs.union(RegSet::ALL.minus(st.written));
        st.unknown_store();
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
            // `xor eax, eax`: no longer depends on anything.
            if a == b && matches!(op, BinOp::Xor | BinOp::Sub) {
                return AV::konst(0);
            }
            let (x, y) = (get(a), get(b));
            // The same exact value twice (two reads of an unchanged
            // register): `xor r32, r32` reads it twice.
            if matches!(op, BinOp::Xor | BinOp::Sub)
                && x.ex == y.ex
                && matches!(x.ex, Ex::Entry(..) | Ex::Low(..))
            {
                return AV::konst(0);
            }
            if x.is_ra() || y.is_ra() {
                acc.reads_ra = true;
            }
            let taint = x.taint.union(y.taint);
            if ty == Ty::I64 {
                match (op, x.ex, y.ex) {
                    (BinOp::Add, Ex::Entry(r, o), Ex::Const(c))
                    | (BinOp::Add, Ex::Const(c), Ex::Entry(r, o)) => {
                        return AV {
                            ex: Ex::Entry(r, norm(o.wrapping_add(c as i64))),
                            taint,
                        }
                    }
                    (BinOp::Sub, Ex::Entry(r, o), Ex::Const(c)) => {
                        return AV {
                            ex: Ex::Entry(r, norm(o.wrapping_sub(c as i64))),
                            taint,
                        }
                    }
                    _ => {}
                }
            } else {
                // 32-bit arithmetic on a register's low half.
                match (op, x.ex, y.ex) {
                    (BinOp::Add, Ex::Low(r, o), Ex::Const(c))
                    | (BinOp::Add, Ex::Const(c), Ex::Low(r, o)) => {
                        return AV {
                            ex: Ex::Low(r, o.wrapping_add(c as u32 as i32 as i64) as i32 as i64),
                            taint,
                        }
                    }
                    (BinOp::Sub, Ex::Low(r, o), Ex::Const(c)) => {
                        return AV {
                            ex: Ex::Low(r, o.wrapping_sub(c as u32 as i32 as i64) as i32 as i64),
                            taint,
                        }
                    }
                    _ => {}
                }
            }
            if let Some(c) = fold_consts(e, &get) {
                return AV::konst(c);
            }
            AV::top(taint)
        }
        Expr::Load {
            addr,
            offset,
            sz,
            signed,
            ty,
            ..
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
                        Some(&(m, v)) if m == n && n == 8 => v,
                        // A 32-bit reload of a register's value.
                        Some(&(4, v)) if n == 4 => match (v.ex, ty) {
                            (Ex::Entry(r, o) | Ex::Low(r, o), Ty::I32) => AV {
                                ex: Ex::Low(r, o as i32 as i64),
                                taint: v.taint,
                            },
                            (Ex::Entry(r, o) | Ex::Low(r, o), _)
                                if !signed && crate::arch::compat() =>
                            {
                                AV {
                                    ex: Ex::Entry(r, o as i32 as i64),
                                    taint: v.taint,
                                }
                            }
                            (Ex::RetAddr, _) if crate::arch::compat() && !signed => v,
                            (Ex::Const(c), _) => {
                                let x = if signed { c as u32 as i32 as i64 as u64 } else { c & 0xffff_ffff };
                                AV::konst(x & ty.mask())
                            }
                            _ => AV::top(v.taint),
                        },
                        // A narrower reload is the stored value's low bytes,
                        // extended: not the entry value it came from.
                        Some(&(m, v)) if m == n => match v.ex {
                            Ex::Const(c) => {
                                let sh = 64 - 8 * n;
                                let x = if signed {
                                    (((c << sh) as i64) >> sh) as u64
                                } else {
                                    (c << sh) >> sh
                                };
                                AV::konst(x & ty.mask())
                            }
                            _ => AV::top(v.taint),
                        },
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
        Expr::Wrap(a) => {
            let x = get(a);
            match x.ex {
                Ex::Entry(r, o) => AV {
                    ex: Ex::Low(r, o as i32 as i64),
                    taint: x.taint,
                },
                // i386: the return address is a 32-bit value already.
                Ex::RetAddr if crate::arch::compat() => x,
                Ex::Const(c) => AV::konst(c & 0xffff_ffff),
                _ => AV::top(x.taint),
            }
        }
        Expr::Extend { a, signed: false } => {
            let x = get(a);
            match x.ex {
                // i386 registers are zero-extended 32-bit values.
                Ex::Low(r, o) if crate::arch::compat() => AV {
                    ex: Ex::Entry(r, o),
                    taint: x.taint,
                },
                Ex::RetAddr if crate::arch::compat() => x,
                Ex::Const(c) => AV::konst(c & 0xffff_ffff),
                _ => AV::top(x.taint),
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
                acc.escapes = acc.escapes.union(get(*a).taint);
            }
            host(st, acc, func);
            AV::top(RegSet::EMPTY)
        }
        _ => {
            if let Some(c) = fold_consts(e, &get) {
                return AV::konst(c);
            }
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

/// A pure scalar expression whose operands are all constants, folded.
fn fold_consts(e: &Expr, get: &dyn Fn(Value) -> AV) -> Option<u64> {
    crate::jumptable::eval_with(e, &mut |u| match get(u).ex {
        Ex::Const(c) => Some(c),
        _ => None,
    })
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
        acc.addr_inputs = acc.addr_inputs.union(v.taint.minus(RegSet::of(&[SP_REG])));
        if v.frame() {
            escape(st, acc);
        }
        if v.is_ra() {
            acc.reads_ra = true;
        }
    }
    if s.leaks_sp() {
        escape(st, acc);
    }
    // Registers whose values it may store where others read them: ours
    // escape with them, and a frame address among them lets the frame
    // escape. (The callee's own stack pointer is its frame, not ours:
    // `leaks_sp` above covers that.)
    for r in s.escaping_inputs.iter() {
        if r == SP_REG {
            continue;
        }
        let v = st.regs[reg_index(r)];
        acc.escapes = acc.escapes.union(v.taint);
        if v.frame() {
            escape(st, acc);
        }
    }
    // The callee's stack accesses relative to its entry sp, which is ours
    // at the call; its own frame (and red zone) is below that.
    let sp = st.regs[SP];
    match sp.sp_off() {
        Some(o) => st.kill(i64::MIN, o),
        None if st.trust_frame => {}
        None => st.slots.clear(),
    }
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
            // Optimistic mode: a callee leaves its caller's frame alone.
            if !(st.trust_frame && w == Slots::Unknown) {
                st.kill_slots(w);
            }
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
    if st.escaped && !st.trust_frame {
        st.slots.clear();
    }
    acc.may_unwind |= s.may_unwind;
    acc.may_leave_pending |= s.may_leave_pending;
    acc.unknown_callee |= s.unknown_callee;
    if !s.may_return {
        return false;
    }
    // What the callee writes on every return path is its result from here
    // on: a later read of it is not of this function's entry value.
    st.written = st.written.union(s.always_written);
    // Outputs: derived from what flows into the callee's outputs, except
    // its own stack pointer (a callee handing back its caller's frame is
    // `leaks_sp`).
    let mut t = RegSet::EMPTY;
    for r in s.output_sources.iter() {
        if r != SP_REG {
            t = t.union(st.regs[reg_index(r)].taint);
        }
    }
    // Optimistic mode: a callee whose stack effect is not known keeps the
    // ABI's (pops its return address; not on i386, where stdcall pops more).
    let delta = s.sp_delta.or(if st.trust_frame { abi_callee().sp_delta } else { None });
    let new_sp = match (sp.sp_off(), delta) {
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
        None if s.preserved.contains(SP_REG) => sp,
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
    acc.escapes = RegSet::ALL;
    acc.out_src = RegSet::ALL;
    if st.escaped || st.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame()) {
        acc.leaks_sp = true;
    }
}

/// A return path: what the registers that changed were computed from.
fn output_flow(st: &St, acc: &mut Acc) {
    for (i, r) in st.regs.iter().enumerate() {
        if i != SP && r.ex != Ex::Entry(i as u8, 0) {
            acc.out_src = acc.out_src.union(r.taint);
        }
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
    analyze_fn_rec(f, ctx, None)
}

/// Values of memory-form `f` that are the same constant on every
/// execution, by the same abstract interpretation the summaries use: through
/// stack slots nothing else can write, and across calls whose summaries
/// (from `sums`, when given) preserve the register or slot. Value numbers
/// are those of `f`, which [`gex_ir::ssa::to_ssa`] keeps for every
/// statement it does not remove.
pub fn known_values(f: &Function, sums: Option<&Summaries>) -> HashMap<Value, u64> {
    known_values_mode(f, sums, false)
}

/// [`known_values`] assuming no code writes this function's frame through
/// a pointer to it that escaped: what the compiler assumes of a slot it
/// spilled a value to and never takes the address of. Not sound (a callee
/// handed `&local` could write past it), so a result relying on it must be
/// marked best effort: the jump-table resolver makes such a table partial.
pub fn known_values_trusting_frame(f: &Function, sums: Option<&Summaries>) -> HashMap<Value, u64> {
    known_values_mode(f, sums, true)
}

fn known_values_mode(f: &Function, sums: Option<&Summaries>, trust_frame: bool) -> HashMap<Value, u64> {
    let empty = HashMap::new();
    let ctx = Ctx {
        sums: sums.map_or(&empty, |s| &s.map),
        trust_frame,
        guards: sums.map(|s| &s.guards),
        stubs: sums.map(|s| &s.stubs),
    };
    let mut rec: Vec<HashMap<Value, AV>> = vec![HashMap::new(); f.blocks.len()];
    analyze_fn_rec(f, &ctx, Some(&mut rec));
    let mut out = HashMap::new();
    for vals in rec {
        for (v, av) in vals {
            if let Ex::Const(c) = av.ex {
                out.insert(v, c);
            }
        }
    }
    out
}

/// The unresolved indirect jumps of `f` (by the jump's address) taken with
/// the frame torn down: the stack pointer and the return address are what
/// they were on entry, as in a tail call through a pointer (a PLT stub, a
/// virtual call after the epilogue). A leaf function's unresolved jump
/// table has the same shape, so this is a description, not a verdict: the
/// jumps stay [`FTerm::Unresolved`].
pub fn frame_torn_down(f: &Function, sums: &Summaries) -> Vec<u64> {
    // A description, so the optimistic mode: a saved return address
    // reloaded after a call to unknown code still counts.
    let ctx = Ctx {
        sums: &sums.map,
        trust_frame: true,
        guards: Some(&sums.guards),
        stubs: Some(&sums.stubs),
    };
    let mut acc_out = Vec::new();
    analyze_fn_full(f, &ctx, None, Some(&mut acc_out), None);
    acc_out
}

/// [`analyze_fn`], optionally recording every value's abstract value on
/// the last (fixpoint) visit of its block.
fn analyze_fn_rec(f: &Function, ctx: &Ctx, record: Option<&mut Vec<HashMap<Value, AV>>>) -> Summary {
    analyze_fn_full(f, ctx, record, None, None)
}

fn analyze_fn_full(
    f: &Function,
    ctx: &Ctx,
    mut record: Option<&mut Vec<HashMap<Value, AV>>>,
    torn: Option<&mut Vec<u64>>,
    mut sites: Option<&mut Vec<(u64, CallEffect)>>,
) -> Summary {
    let n = f.blocks.len();
    let mut acc = Acc::empty(n);
    let rpo = f.rpo();
    let mut order = vec![usize::MAX; n];
    for (i, b) in rpo.iter().enumerate() {
        order[b.index()] = i;
    }
    let mut state_in: Vec<Option<St>> = vec![None; n];
    state_in[0] = Some(St {
        trust_frame: ctx.trust_frame,
        ..St::entry()
    });
    // Worklist by RPO position, so loops settle before what follows them.
    let mut work: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    work.insert(order[0]);
    // Values claimed for every execution must also hold when code outside
    // enters at an interior entry, in any state. (A summary describes calls
    // to the primary entry only, so it leaves them out.)
    if record.is_some() {
        for e in f.entry_blocks().into_iter().skip(1) {
            let mut any = St::entry();
            for r in any.regs.iter_mut() {
                *r = AV::top(RegSet::EMPTY);
            }
            any.slots.clear();
            any.escaped = true;
            any.trust_frame = ctx.trust_frame;
            state_in[e.index()] = Some(any);
            if order[e.index()] != usize::MAX {
                work.insert(order[e.index()]);
            }
        }
    }
    let mut visits = vec![0u32; n];
    // Exits are recorded on the final visit of each block only: collect
    // them per block and overwrite on revisit.
    let mut exits_of: Vec<Option<(RegSet, Option<i64>, RegSet)>> = vec![None; n];
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
        if let Some(r) = record.as_deref_mut() {
            r[bi] = vals.clone();
        }
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
                let site = blk.term_pc;
                if let Callee::Indirect(v) = callee {
                    note_addr(&mut acc, get(*v));
                }
                if ctx.stub_site(*callee, site) {
                    // The syscall the stub makes, with the number and
                    // arguments this caller set; the stub's `ret` pops what
                    // the call pushed.
                    // How far the stubs' `ret` moves the stack pointer (their
                    // own summaries cannot say, not knowing the number).
                    let delta = {
                        let stubs = ctx.stubs.unwrap();
                        let ds: Vec<i64> = ctx.guarded(*callee, site).unwrap().iter().map(|t| stubs[t]).collect();
                        ds.iter().all(|&d| d == ds[0]).then_some(ds[0])
                    };
                    let sp = st.regs[SP];
                    let (mut after, nr) = syscall_step(&st, &mut acc);
                    let benign = nr.is_some_and(benign_syscall);
                    after.regs[SP] = match (sp.sp_off(), delta) {
                        (Some(o), Some(d)) if benign => AV {
                            ex: Ex::Entry(SP as u8, o + d),
                            taint: sp.taint,
                        },
                        _ => AV::top(sp.taint),
                    };
                    if let Some(out) = sites.as_deref_mut() {
                        let e = syscall_effect(nr);
                        let mut preserved = e.preserved;
                        preserved.remove(SP_REG);
                        out.push((
                            site,
                            CallEffect {
                                preserved,
                                sp_delta: if benign { delta } else { None },
                            },
                        ));
                    }
                    let never = nr.is_some_and(crate::arch::syscall_never_returns);
                    if let (Some(e), false) = (ret, never) {
                        succ_state.push((e.to, after));
                    }
                } else {
                    let s = ctx.callee_at(*callee, site);
                    if ctx.guarded(*callee, site).is_some() {
                        if let Some(out) = sites.as_deref_mut() {
                            out.push((site, CallEffect::from_summary(&s)));
                        }
                    }
                    let mut after = st.clone();
                    if call(&mut after, &mut acc, &s) {
                        if let Some(e) = ret {
                            succ_state.push((e.to, after));
                        }
                    }
                }
            }
            FTerm::TailCall { callee } => {
                let s = ctx.callee_at(*callee, blk.term_pc);
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
                        exits_of[bi] = Some((before.inter(s.preserved), d, after.written));
                        if after.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame()) {
                            acc.leaks_sp = true;
                        }
                        output_flow(&after, &mut acc);
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
                exits_of[bi] = Some((unchanged(&st), d, st.written));
                output_flow(&st, &mut acc);
                if st.regs.iter().enumerate().any(|(i, r)| i != SP && r.frame() && r.sp_off().is_none_or(|o| o >= 0)) {
                    acc.leaks_sp = true;
                }
            }
            FTerm::Unresolved { target } => {
                acc.torn_down[bi] = st.regs[SP].sp_off() == Some(0)
                    && if gex_abi::X86 {
                        st.slots.get(&0).is_some_and(|&(n, v)| n == crate::arch::ptr_bytes() && v.ex == Ex::RetAddr)
                    } else {
                        st.regs[30].ex == Ex::Entry(30, 0)
                    };
                note_addr(&mut acc, get(*target));
                unknown_exit(&mut acc, &st);
                unknown_of[bi] = true;
            }
            FTerm::Syscall { next, .. } => {
                let (after, _) = syscall_step(&st, &mut acc);
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
    if let Some(t) = torn {
        for (bi, &on) in acc.torn_down.iter().enumerate() {
            if on {
                t.push(f.blocks[bi].term_pc);
            }
        }
    }
    let unknown_exit = acc.unknown_exit || unknown_of.iter().any(|&u| u);

    let may_return = !acc.exits.is_empty() || unknown_exit;
    // Written on every way back: a must-set, so nothing when a path leaves
    // to unknown code, everything (vacuously) when none returns.
    let always_written = if unknown_exit {
        RegSet::EMPTY
    } else {
        acc.exits.iter().fold(RegSet::ALL, |a, (_, _, w)| a.inter(*w))
    };
    let (preserved, sp_delta) = if unknown_exit {
        (RegSet::EMPTY, None)
    } else if acc.exits.is_empty() {
        (RegSet::ALL, None)
    } else {
        let p = acc.exits.iter().fold(RegSet::ALL, |a, (s, _, _)| a.inter(*s));
        let d0 = acc.exits[0].1;
        let d = if acc.exits.iter().all(|(_, d, _)| *d == d0) { d0 } else { None };
        (p, d)
    };
    let mut s = Summary {
        inputs: acc.inputs,
        always_written,
        stack_inputs: acc.stack_inputs,
        outputs: RegSet::ALL.minus(preserved),
        preserved,
        sp_delta,
        caller_frame_writes: acc.caller_frame_writes,
        addr_inputs: acc.addr_inputs,
        escaping_inputs: acc.escapes,
        output_sources: acc.out_src,
        may_return,
        may_unwind: acc.may_unwind,
        may_leave_pending: acc.may_leave_pending,
        unknown_callee: acc.unknown_callee,
        reads_return_address: acc.reads_ra,
    };
    if !may_return {
        s.outputs = RegSet::EMPTY;
        s.output_sources = RegSet::EMPTY;
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
        self.addr_inputs.contains(SP_REG)
    }
    fn set_leaks_sp(&mut self, on: bool) {
        if on {
            self.addr_inputs.insert(SP_REG);
        }
    }
}

// ---------------------------------------------------------------------------
// The call graph fixpoint
// ---------------------------------------------------------------------------

/// The least summary at least as general as both: what may happen in either,
/// what holds in both.
fn join_summaries(a: &Summary, b: &Summary) -> Summary {
    let preserved = a.preserved.inter(b.preserved);
    Summary {
        inputs: a.inputs.union(b.inputs),
        always_written: a.always_written.inter(b.always_written),
        stack_inputs: a.stack_inputs.join(b.stack_inputs),
        outputs: a.outputs.union(b.outputs),
        preserved,
        sp_delta: if a.sp_delta == b.sp_delta || !a.may_return {
            b.sp_delta
        } else if !b.may_return {
            a.sp_delta
        } else {
            None
        },
        caller_frame_writes: a.caller_frame_writes.join(b.caller_frame_writes),
        addr_inputs: a.addr_inputs.union(b.addr_inputs),
        escaping_inputs: a.escaping_inputs.union(b.escaping_inputs),
        output_sources: a.output_sources.union(b.output_sources),
        may_return: a.may_return || b.may_return,
        may_unwind: a.may_unwind || b.may_unwind,
        may_leave_pending: a.may_leave_pending || b.may_leave_pending,
        unknown_callee: a.unknown_callee || b.unknown_callee,
        reads_return_address: a.reads_return_address || b.reads_return_address,
    }
}

/// Guards from a profile (S6, [`crate::profile`]): every indirect call
/// site the profile saw, guarded with the targets it recorded inside the
/// same object (at most four, the profile's table size). `obj` is the
/// object's profile id (`sha256:…` of the file) and `bias` its load bias
/// (runtime address minus link-time address). The profile keys a site by
/// the tier-1 block that ends in the call; that block's straight line
/// (fall-throughs a later split made) leads to the call instruction.
pub fn profile_guards(prog: &Program, profile: &crate::profile::Profile, obj: &str, bias: u64) -> Guards {
    let mut g = Guards::default();
    let Some(o) = profile.objects.get(obj) else {
        return g;
    };
    // Every lifted block, by start: (function, block).
    let mut at: HashMap<u64, Vec<(u64, BlockId)>> = HashMap::new();
    for (&a, f) in &prog.functions {
        for id in f.ids() {
            let b = f.block(id);
            if !b.synthetic {
                at.entry(b.start).or_default().push((a, id));
            }
        }
    }
    for (&off, site) in &o.indirect {
        if !site.call {
            continue;
        }
        let targets: Vec<u64> = site
            .targets
            .keys()
            .filter(|l| l.obj == obj)
            .map(|l| l.off.wrapping_add(bias))
            .filter(|t| prog.functions.contains_key(t))
            .collect();
        // A target outside the object (a library) has no summary here:
        // guarding with the rest alone would not cover it.
        if targets.is_empty() || targets.len() != site.targets.len() {
            continue;
        }
        let start = off.wrapping_add(bias);
        for &(fa, mut id) in at.get(&start).map(|v| v.as_slice()).unwrap_or(&[]) {
            let f = &prog.functions[&fa];
            for _ in 0..64 {
                match &f.block(id).term {
                    FTerm::Jump(e) if !f.block(e.to).synthetic => id = e.to,
                    FTerm::Call {
                        callee: Callee::Indirect(_),
                        ..
                    } => {
                        g.calls.insert(f.block(id).term_pc, targets.clone());
                        break;
                    }
                    _ => break,
                }
            }
        }
    }
    g
}

/// Guards for i386 syscalls made through the vsyscall entry: every
/// `call *%gs:0x10` (musl reads its TCB's `sysinfo` word there) guarded
/// with the image's syscall stub, when it has exactly one. musl sets that
/// word to its own `__vsyscall` unless the kernel passes `AT_SYSINFO`, and
/// gex passes none, so the guard does not miss. See [`Guards`] for what a
/// backend must do when one does.
pub fn vsyscall_guards(prog: &Program) -> Guards {
    let mut g = Guards::default();
    if !crate::arch::compat() {
        return g;
    }
    let stubs: Vec<u64> = prog
        .functions
        .iter()
        .filter(|(_, f)| syscall_stub(f).is_some())
        .map(|(&a, _)| a)
        .collect();
    if stubs.len() != 1 {
        return g;
    }
    for f in prog.functions.values() {
        let defs: HashMap<Value, &Expr> = f
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .filter_map(|s| match s {
                Stmt::Def { dst, expr } => Some((*dst, expr)),
                _ => None,
            })
            .collect();
        for b in &f.blocks {
            if let FTerm::Call {
                callee: Callee::Indirect(v),
                ..
            } = b.term
            {
                if loads_gs_sysinfo(&defs, v) {
                    g.calls.insert(b.term_pc, stubs.clone());
                }
            }
        }
    }
    g
}

/// Whether `v` is the 4-byte word at `%gs:0x10`: a load (through the
/// zero-extension of an indirect target) from `TpidrRo + 0x10` (wrapped to
/// 32 bits).
fn loads_gs_sysinfo(defs: &HashMap<Value, &Expr>, v: Value) -> bool {
    let peel = |mut v: Value| {
        for _ in 0..4 {
            match defs.get(&v) {
                Some(Expr::Extend { a, signed: false }) | Some(Expr::Wrap(a)) => v = *a,
                _ => break,
            }
        }
        v
    };
    let Some(&&Expr::Load { addr, offset, sz, .. }) = defs.get(&peel(v)) else {
        return false;
    };
    if sz.bytes() != 4 {
        return false;
    }
    let base = peel(addr);
    let (reg, disp) = match defs.get(&base) {
        Some(&&Expr::ReadReg(r)) => (Some(r), 0u64),
        Some(&&Expr::Bin { op: BinOp::Add, a, b, .. }) => match (defs.get(&a), defs.get(&b)) {
            (Some(&&Expr::ReadReg(r)), Some(&&Expr::ConstI64(c))) | (Some(&&Expr::ConstI64(c)), Some(&&Expr::ReadReg(r))) => {
                (Some(r), c)
            }
            _ => (None, 0),
        },
        _ => (None, 0),
    };
    reg == Some(GuestReg::TpidrRo) && disp.wrapping_add(offset as u64) == 0x10
}

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
    analyze_functions_with_guards(functions, &Guards::default())
}

/// [`analyze`] with guarded indirect calls: see [`Guards`], and its
/// contract, which a backend must keep for these summaries to be valid.
pub fn analyze_with_guards(prog: &Program, guards: &Guards) -> Summaries {
    analyze_functions_with_guards(&prog.functions, guards)
}

/// [`analyze_with_guards`] over any set of functions.
pub fn analyze_functions_with_guards(functions: &BTreeMap<u64, Function>, guards: &Guards) -> Summaries {
    let stubs: HashMap<u64, i64> = functions
        .iter()
        .filter_map(|(&a, f)| Some((a, syscall_stub(f)?)))
        .collect();
    let mut site_effects: HashMap<(u64, u64), CallEffect> = HashMap::new();
    let nodes: Vec<u64> = functions.keys().copied().collect();
    let mut edges: HashMap<u64, Vec<u64>> = HashMap::new();
    for (&a, f) in functions {
        let mut v: Vec<u64> = f
            .call_sites()
            .iter()
            .flat_map(|c| match c.callee {
                Callee::Direct(t) => vec![t],
                Callee::Indirect(_) => guards.calls.get(&c.pc).cloned().unwrap_or_default(),
            })
            .filter(|t| functions.contains_key(t))
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
                let mut sites = Vec::new();
                let mut s = analyze_fn_full(
                    &functions[&a],
                    &Ctx {
                        sums: &map,
                        trust_frame: false,
                        guards: Some(guards),
                        stubs: Some(&stubs),
                    },
                    None,
                    None,
                    Some(&mut sites),
                );
                for (pc, eff) in sites {
                    site_effects.insert((a, pc), eff);
                }
                // Only climb: the analysis of one function is not monotone
                // in its callees' summaries (whether a callee preserves a
                // register decides whether that register's taint counts),
                // so a cycle can alternate. Joining with the last iterate
                // makes the sequence ascend, and its limit over-approximates
                // every iterate.
                if recursive {
                    if let Some(old) = map.get(&a) {
                        s = join_summaries(old, &s);
                    }
                }
                // Slot ranges have no height bound (a tail-call cycle that
                // moves the stack pointer reads one more byte each time
                // round): widen one still growing to unknown.
                if iters > WIDEN_AFTER {
                    if let Some(old) = map.get(&a) {
                        if old.stack_inputs != s.stack_inputs {
                            s.stack_inputs = Slots::Unknown;
                        }
                        if old.caller_frame_writes != s.caller_frame_writes {
                            s.caller_frame_writes = Slots::Unknown;
                        }
                    }
                }
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
                stats.unconverged_members.extend(comp.iter().copied());
                for &a in &comp {
                    map.insert(a, Summary::unknown());
                }
                break;
            }
        }
        stats.max_iterations = stats.max_iterations.max(iters);
    }
    stats.unknown = map.values().filter(|s| s.unknown_callee).count();
    Summaries {
        map,
        stats,
        guards: guards.clone(),
        site_effects,
        stubs,
    }
}
