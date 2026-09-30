//! Phase B: one C function per recovered guest function.
//!
//! Registers and flags are C locals across the whole function (pewasm's
//! scheme): loaded on entry, written back wherever control can leave, reloaded
//! after anything that can change guest state. Nothing stays in a local
//! across a call, so the host's trap-out is a correct unwind with no landing
//! pads (`docs/function-tier.md` §5). Each function `F` becomes
//!
//! * `f_<F>(ctx)`, its body: a guest `ret`, or any exit to code the function
//!   tier does not own, writes back, sets `ctx::PC` and *returns*;
//! * `e_<F>(ctx)`, its dispatch entry and nest root: `f_<F>(ctx)` and then a
//!   tail call into dispatch on `ctx::PC` (§7).
//!
//! C callers call `f_<G>` and apply the return check (§4.2 of the merge
//! plan): the context's PC must be the return address and SP back where it
//! was; anything else leaves by returning too.

use crate::emit::{self, reg_name, Emitter, FnRegs};
use anyhow::{bail, Result};
use gex_abi::ctx;
use gex_ir::func::{Callee, Edge, FTerm, Form, Function, RegSet, Summary};
use gex_ir::{Block, Expr, GuestReg, Stmt};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

pub fn body_name(entry: u64) -> String {
    format!("f_{entry:x}")
}

pub fn entry_name(entry: u64) -> String {
    format!("e_{entry:x}")
}

/// The name of the helper that calls a function-tier function by address
/// (one per module): returns 1 if it did, 0 if the address is not one.
pub const CALL_KNOWN: &str = "gex_call_known";

/// The guest's stack pointer as a GIR register.
fn sp_reg() -> GuestReg {
    if gex_abi::X86 {
        GuestReg::X(4)
    } else {
        GuestReg::Sp
    }
}

/// How far a `ret` moves SP relative to the call site: x86-64 pops the
/// return address the call pushed.
const RET_SP_DELTA: u64 = if gex_abi::X86 { 8 } else { 0 };

/// The recursion-depth guard (`docs/function-tier.md` §7). `depth` is an
/// estimate, in bytes, of the engine stack the C frames below this one in
/// the nest use; a function whose own frame would take it past this budget
/// leaves for its own entry instead of running, which unwinds the nest and
/// restarts it at zero from the dispatch root. `GEX_CGEN_STACK_BUDGET`
/// overrides it, for measurement. Engines: wasmtime's `max_wasm_stack` is
/// 512 KiB, V8's whole stack about 984 KiB.
pub const STACK_BUDGET: u32 = 384 << 10;

fn stack_budget() -> u32 {
    std::env::var("GEX_CGEN_STACK_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(STACK_BUDGET)
}

/// A function's frame, estimated from the registers it keeps in locals: the
/// wasm locals clang ends up with measured up to ~2.5x the register bytes
/// (coremark, x86-64), plus the engine's spill slots and frame header.
fn frame_weight(regs: &FnRegs) -> u32 {
    let bytes: u32 = regs
        .used
        .iter()
        .map(|r| if r.ty() == gex_ir::Ty::V128 { 16 } else { 8 })
        .sum();
    64 + 3 * bytes
}

/// What the indirect-call helper's own frame adds.
const CALL_KNOWN_WEIGHT: u32 = 32;

/// Pricing experiments, `GEX_CGEN_EXP=a,b`: variants that are measured
/// before they are built properly. Some are unsound (`NOFLAGWB`) and exist
/// only to price what a sound version could at most win.
fn exp(name: &str) -> bool {
    std::env::var("GEX_CGEN_EXP")
        .map(|v| v.split(',').any(|x| x == name))
        .unwrap_or(false)
}

/// Deliberate miscompilations, `GEX_CGEN_MUTATE=a,b`, each disabling one
/// safety condition so a test can show it is needed (merge plan §5.3).
fn mutate(name: &str) -> bool {
    std::env::var("GEX_CGEN_MUTATE")
        .map(|v| v.split(',').any(|x| x == name))
        .unwrap_or(false)
}

/// Local loop iterations between safepoint polls (tier-1's `loop_poll`).
const LOOP_POLL: i32 = 1024;

/// What the emitted functions do at their exits (`GEX_CGEN_STATS=1`).
#[derive(Default, Debug)]
pub struct FnStats {
    pub functions: usize,
    pub blocks: usize,
    pub calls_known: usize,
    pub calls_indirect: usize,
    pub tail_known: usize,
    pub switches: usize,
    pub unresolved: usize,
    pub returns: usize,
    /// Direct calls and tail calls that leave the function tier, by target.
    pub leaves: std::collections::BTreeMap<u64, usize>,
}

impl FnStats {
    pub fn merge(&mut self, o: &FnStats) {
        self.functions += o.functions;
        self.blocks += o.blocks;
        self.calls_known += o.calls_known;
        self.calls_indirect += o.calls_indirect;
        self.tail_known += o.tail_known;
        self.switches += o.switches;
        self.unresolved += o.unresolved;
        self.returns += o.returns;
        for (a, n) in &o.leaves {
            *self.leaves.entry(*a).or_default() += n;
        }
    }
}

/// C for one translation unit of functions.
pub struct FnUnit {
    pub stats: FnStats,
    protos: BTreeSet<u64>,
    bodies: String,
    /// Whether any function here calls through `CALL_KNOWN`.
    pub uses_call_known: bool,
    /// The frame weight of the function being emitted (`frame_weight`).
    weight: u32,
    /// Registers live on entry to each block of the function being emitted
    /// (`live_in`): write-backs on internal edges and at calls to functions
    /// with a summary store only these.
    live: Vec<RegSet>,
}

impl Default for FnUnit {
    fn default() -> Self {
        Self::new()
    }
}

impl FnUnit {
    pub fn new() -> FnUnit {
        FnUnit {
            stats: FnStats::default(),
            protos: BTreeSet::new(),
            bodies: String::new(),
            uses_call_known: false,
            weight: 0,
            live: Vec::new(),
        }
    }

    /// Add `f`. `known` is every function entry with a C body in this module;
    /// `sums` are S2's call-graph summaries, by entry (may be empty).
    pub fn add(&mut self, f: &Function, known: &HashSet<u64>, sums: &HashMap<u64, Summary>) -> Result<()> {
        if f.form != Form::Memory {
            bail!("function {:#x}: only memory form is supported", f.entry);
        }
        let dummy = Block::new(f.entry);
        let owned = HashSet::new();
        let mut e = Emitter::new(&dummy, &owned);
        e.resize(f.n_values as usize);
        let mut regs = registers(f);
        regs.clean_spills = clean_spills();
        let dirty = if regs.clean_spills { dirty_in(f) } else { Vec::new() };
        self.live = if live_writebacks() { live_in(f, sums) } else { vec![RegSet::ALL; f.blocks.len()] };
        let used: Vec<GuestReg> = regs.used.iter().copied().collect();
        self.weight = frame_weight(&regs);
        e.func_regs = Some(regs);
        let mut calls: BTreeSet<u64> = BTreeSet::new();

        self.stats.functions += 1;
        self.stats.blocks += f.blocks.iter().filter(|b| !b.synthetic).count();
        let name = body_name(f.entry);
        let _ = writeln!(self.bodies, "void {name}(int ctx, int32_t depth) {{");
        for r in &used {
            let _ = writeln!(self.bodies, "  {} {};", emit::cty(r.ty()), reg_name(*r));
        }
        let _ = writeln!(self.bodies, "  int32_t poll = {LOOP_POLL};");
        // The recursion-depth guard, before anything is loaded: every caller
        // wrote everything back before calling, so leaving for this
        // function's own entry here is exactly the caller leaving for it.
        let _ = writeln!(
            self.bodies,
            "  if (__builtin_expect(depth > {}, 0)) {{ GEX_C64({}u) = 0x{:x}ull; return; }}",
            stack_budget().saturating_sub(self.weight),
            ctx::PC,
            f.entry
        );
        e.load_regs(&used);
        // Block 0 is the entry and comes first, so control falls into it.
        for (i, b) in f.blocks.iter().enumerate() {
            e.line(format!("L{i}: {{"));
            if let Some(d) = dirty.get(i) {
                e.set_dirty(d);
            }
            for s in &b.stmts {
                e.stmt(s)?;
            }
            self.term(&mut e, f, i, known, sums, &mut calls)?;
            e.line("}");
        }
        self.bodies.push_str(&e.take_output());
        self.bodies.push_str("}\n\n");

        // The entry and nest root.
        let _ = writeln!(self.bodies, "void {}(int ctx) {{", entry_name(f.entry));
        let root_block = Block::new(f.entry);
        let mut r = Emitter::new(&root_block, &owned);
        if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
            r.target_check();
        }
        r.line(format!("{name}(ctx, 0);"));
        r.line(format!("const uint64_t tgt = GEX_C64({}u);", ctx::PC));
        r.safepoint("tgt");
        r.dispatch("tgt");
        self.bodies.push_str(&r.take_output());
        self.bodies.push_str("}\n\n");
        self.protos.insert(f.entry);
        self.protos.extend(calls);
        Ok(())
    }

    /// Emit the terminator of block `i`.
    fn term(
        &mut self,
        e: &mut Emitter<'_>,
        f: &Function,
        i: usize,
        known: &HashSet<u64>,
        sums: &HashMap<u64, Summary>,
        calls: &mut BTreeSet<u64>,
    ) -> Result<()> {
        let b = &f.blocks[i];
        let live = &self.live;
        let goto = |e: &mut Emitter<'_>, edge: &Edge| {
            if !edge.args.is_empty() {
                unreachable!("memory form has no edge arguments");
            }
            let to = &f.blocks[edge.to.index()];
            // tier-1's rule for a backward branch: poll. Here the countdown is
            // a local (tier-1's fused loops do the same) and the slow path
            // writes everything back, since the host may deliver a signal.
            if !to.synthetic && to.start <= b.start {
                e.line("if (--poll <= 0) {");
                e.line("  poll = 1024;");
                // Only what the loop head may read: the host resumes there
                // (in tier-1) if it acts at this safepoint.
                e.flush_keep_live(live[edge.to.index()]);
                e.store_pc(&format!("0x{:x}ull", to.start));
                e.line(format!("  gex_safepoint(ctx, 0x{:x}ll);", to.start));
                e.line("}");
            }
            e.line(format!("goto L{};", edge.to.index()));
        };
        // Write back, set PC, return: the nest root dispatches from there.
        let leave = |e: &mut Emitter<'_>, pc: &str| {
            e.flush();
            e.store_pc(pc);
            e.line("return;");
        };
        match &b.term {
            FTerm::Jump(edge) => goto(e, edge),
            FTerm::Branch {
                cond,
                taken,
                not_taken,
            } => {
                e.line(format!("if ({}) {{", e.v(*cond)));
                goto(e, taken);
                e.line("}");
                goto(e, not_taken);
            }
            FTerm::Switch { target, cases, .. } => {
                self.stats.switches += 1;
                let t = e.v(*target);
                e.line(format!("switch ({t}) {{"));
                for (addr, edge) in cases {
                    e.line(format!("case 0x{addr:x}ull:"));
                    goto(e, edge);
                }
                e.line("default:;");
                e.line("}");
                leave(e, &t);
            }
            FTerm::Call { callee, ret_pc, ret } => {
                let spn = reg_name(sp_reg());
                if exp("NOFLAGWB") {
                    e.forget_flags();
                }
                // A known callee with a summary needs its inputs; the
                // continuation needs what is live there. Anything else may
                // read everything.
                let need = match (callee, ret) {
                    (Callee::Direct(t), Some(r)) if known.contains(t) => match sums.get(t) {
                        Some(sm) => sm.inputs.union(live[r.to.index()]).union(RegSet::of(&[sp_reg()])),
                        None => RegSet::ALL,
                    },
                    _ => RegSet::ALL,
                };
                e.flush_live(need);
                e.line(format!(
                    "{{ const uint64_t sp_want = {spn} + {RET_SP_DELTA}ull;"
                ));
                match callee {
                    Callee::Direct(t) if known.contains(t) => {
                        calls.insert(*t);
                        self.stats.calls_known += 1;
                        e.line(format!("  {}(ctx, depth + {});", body_name(*t), self.weight));
                    }
                    Callee::Direct(t) => {
                        *self.stats.leaves.entry(*t).or_default() += 1;
                        // Not ours: the rest of this function runs in tier-1.
                        e.store_pc(&format!("0x{t:x}ull"));
                        e.line("  return;");
                    }
                    Callee::Indirect(v) => {
                        self.uses_call_known = true;
                        self.stats.calls_indirect += 1;
                        let t = e.v(*v);
                        e.line(format!(
                            "  if (!{CALL_KNOWN}(ctx, {t}, depth + {})) {{",
                            self.weight + CALL_KNOWN_WEIGHT
                        ));
                        e.store_pc(&t);
                        e.line("    return;");
                        e.line("  }");
                    }
                }
                // The return check; everything the callee left is in ctx.
                // `GEX_CGEN_MUTATE=NORETCHECK` drops it: a deliberate
                // miscompilation, for the soundness corpus's mutation check.
                if !mutate("NORETCHECK") {
                    e.line(format!(
                        "  if (GEX_C64({}u) != 0x{ret_pc:x}ull || GEX_C64({}u) != sp_want) return;",
                        ctx::PC,
                        ctx::SP
                    ));
                } else {
                    e.line("  (void)sp_want;");
                }
                e.line("}");
                e.invalidate();
                match ret {
                    Some(edge) => goto(e, edge),
                    // Believed not to return, and it did: leave to where it
                    // returned (PC is already there).
                    None => e.line("return;"),
                }
            }
            FTerm::TailCall { callee } => match callee {
                Callee::Direct(t) if known.contains(t) => {
                    calls.insert(*t);
                    self.stats.tail_known += 1;
                    if exp("NOFLAGWB") {
                        e.forget_flags();
                    }
                    e.flush();
                    if exp("NORETPOLL") {
                        e.safepoint(&format!("0x{t:x}ull"));
                    }
                    e.line(format!("GEX_MUSTTAIL return {}(ctx, depth);", body_name(*t)));
                }
                Callee::Direct(t) => {
                    *self.stats.leaves.entry(*t).or_default() += 1;
                    leave(e, &format!("0x{t:x}ull"))
                }
                Callee::Indirect(v) => {
                    let t = e.v(*v);
                    leave(e, &t)
                }
            },
            FTerm::Return { target } | FTerm::Unresolved { target } => {
                if matches!(b.term, FTerm::Return { .. }) {
                    self.stats.returns += 1;
                } else {
                    self.stats.unresolved += 1;
                }
                // tier-1 polls on every indirect branch; so does this, with
                // everything already written back and PC set.
                let t = e.v(*target);
                if exp("NOFLAGWB") && matches!(b.term, FTerm::Return { .. }) {
                    e.forget_flags();
                }
                e.flush();
                e.store_pc(&t);
                if !exp("NORETPOLL") {
                    e.safepoint(&t);
                }
                e.line("return;");
            }
            FTerm::Syscall { next_pc, next } => {
                e.flush();
                e.store_pc(&format!("0x{next_pc:x}ull"));
                e.line(format!("gex_syscall(ctx, 0x{:x}ll);", b.term_pc));
                e.invalidate();
                match next {
                    Some(edge) => goto(e, edge),
                    None => e.line("return;"),
                }
            }
            FTerm::Illegal { raw, pc } => {
                e.flush();
                e.store_pc(&format!("0x{pc:x}ull"));
                e.line(format!("gex_illegal(ctx, 0x{pc:x}ll, (int32_t)0x{raw:x}u);"));
                e.line("__builtin_trap();");
            }
        }
        Ok(())
    }

    pub fn source(&self) -> String {
        let mut s = String::from("#include \"gex_abi.h\"\n\n");
        for a in &self.protos {
            let _ = writeln!(s, "void {}(int ctx, int32_t depth);", body_name(*a));
        }
        let _ = writeln!(s, "int {CALL_KNOWN}(int ctx, uint64_t target, int32_t depth);\n");
        s.push_str(&self.bodies);
        s
    }
}

/// The helper indirect calls go through: a `switch` over every function
/// entry with a C body. The slow path pewasm's dispatch-cache probe and
/// devirtualization later speed up; priced before it is optimised.
pub fn call_known_source(entries: &[u64]) -> String {
    let mut s = String::from("#include \"gex_abi.h\"\n\n");
    for a in entries {
        let _ = writeln!(s, "void {}(int ctx, int32_t depth);", body_name(*a));
    }
    let _ = writeln!(s, "\nint {CALL_KNOWN}(int ctx, uint64_t target, int32_t depth) {{");
    s.push_str("  switch (target) {\n");
    for a in entries {
        let _ = writeln!(s, "  case 0x{a:x}ull: {}(ctx, depth); return 1;", body_name(*a));
    }
    s.push_str("  default: return 0;\n  }\n}\n");
    s
}

/// Every register the function reads or writes, and every one it writes.
/// The stack pointer is always used: the return check reads it.
fn registers(f: &Function) -> FnRegs {
    let mut used = BTreeSet::new();
    let mut written = BTreeSet::new();
    used.insert(sp_reg());
    for b in &f.blocks {
        for s in &b.stmts {
            match s {
                Stmt::Def {
                    expr: Expr::ReadReg(r),
                    ..
                } => {
                    used.insert(*r);
                }
                Stmt::WriteReg { reg, .. } => {
                    used.insert(*reg);
                    written.insert(*reg);
                }
                _ => {}
            }
        }
    }
    FnRegs {
        used,
        written,
        clean_spills: false,
    }
}

/// Clean-spill elision (pewasm's `opt_spill`, optimization-plan §16) is on
/// unless `GEX_CGEN_CLEAN=0`.
fn clean_spills() -> bool {
    std::env::var("GEX_CGEN_CLEAN").as_deref() != Ok("0")
}

/// For each block, the registers that may differ from the context on entry:
/// a forward may-analysis. A write makes a register dirty; a write-back
/// followed by a reload (a call, a syscall, a clobbering host call) makes
/// everything clean; the entry starts clean, having just loaded everything.
fn dirty_in(f: &Function) -> Vec<BTreeSet<GuestReg>> {
    let n = f.blocks.len();
    let mut inn: Vec<Option<BTreeSet<GuestReg>>> = vec![None; n];
    inn[0] = Some(BTreeSet::new());
    let order = f.rpo();
    let mut changed = true;
    while changed {
        changed = false;
        for &id in &order {
            let Some(mut d) = inn[id.index()].clone() else { continue };
            let b = &f.blocks[id.index()];
            for s in &b.stmts {
                match s {
                    Stmt::WriteReg { reg, .. } => {
                        d.insert(*reg);
                    }
                    _ if emit::clobbers(s) => d.clear(),
                    _ => {}
                }
            }
            if matches!(b.term, FTerm::Call { .. } | FTerm::Syscall { .. }) {
                d.clear();
            }
            for s in b.succs() {
                let slot = &mut inn[s.index()];
                let merged = match slot {
                    Some(old) => {
                        let u: BTreeSet<GuestReg> = old.union(&d).copied().collect();
                        if u.len() == old.len() {
                            continue;
                        }
                        u
                    }
                    None => d.clone(),
                };
                *slot = Some(merged);
                changed = true;
            }
        }
    }
    inn.into_iter().map(|d| d.unwrap_or_default()).collect()
}

/// Liveness-limited write-backs are on unless `GEX_CGEN_LIVE=0`.
fn live_writebacks() -> bool {
    std::env::var("GEX_CGEN_LIVE").as_deref() != Ok("0")
}

/// For each block, the registers (flags included) that may be read before
/// being written on some path from its entry, where every exit the function
/// does not see past reads everything: returns, tail calls, unresolved and
/// switch-default jumps, syscalls, calls with no summary or no
/// continuation, and clobbering host calls. A call to a summarised
/// function reads its inputs and passes on what its continuation reads.
///
/// Write-backs guarded by this are exact for anything that resumes at the
/// point the liveness describes (a safepoint's loop head, a callee trapping
/// out and tier-1 returning into the continuation). Registers the ABI does
/// not preserve across calls can be stale in the context when a callee
/// `longjmp`s to an earlier call's continuation in the same frame; C and
/// the ABIs leave them undefined there (docs/function-tier.md §5).
fn live_in(f: &Function, sums: &HashMap<u64, Summary>) -> Vec<RegSet> {
    let n = f.blocks.len();
    let mut live = vec![RegSet::EMPTY; n];
    let order: Vec<usize> = f.rpo().iter().rev().map(|b| b.index()).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &i in &order {
            let b = &f.blocks[i];
            let mut l = match &b.term {
                FTerm::Jump(e) => live[e.to.index()],
                FTerm::Branch {
                    taken, not_taken, ..
                } => live[taken.to.index()].union(live[not_taken.to.index()]),
                // The default case leaves.
                FTerm::Switch { .. } => RegSet::ALL,
                FTerm::Call {
                    callee: Callee::Direct(t),
                    ret: Some(r),
                    ..
                } => match sums.get(t) {
                    Some(sm) => sm.inputs.union(live[r.to.index()]),
                    None => RegSet::ALL,
                },
                _ => RegSet::ALL,
            };
            for s in b.stmts.iter().rev() {
                match s {
                    Stmt::WriteReg { reg, .. } => l.remove(*reg),
                    Stmt::Def {
                        expr: Expr::ReadReg(r),
                        ..
                    } => l.insert(*r),
                    _ if emit::clobbers(s) => l = RegSet::ALL,
                    _ => {}
                }
            }
            if l != live[i] {
                live[i] = l;
                changed = true;
            }
        }
    }
    live
}
