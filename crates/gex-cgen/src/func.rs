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
use gex_ir::func::{BlockId, Callee, Edge, FTerm, Form, Function, RegSet, Summary};
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
/// (x86-64: 8; i386 compatibility mode: 4, see `FnUnit::ret_sp_delta`.)
fn ret_sp_delta(class32: bool) -> u64 {
    match (gex_abi::X86, class32) {
        (false, _) => 0,
        (true, false) => 8,
        (true, true) => 4,
    }
}

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

/// The `need` bits a caller passes: what the callee's expected return must
/// write back beyond every dirty non-flag register.
const NEED_DEFS: &str = "#define NEED_FLAGS 1u\n#define NEED_ALL 0xffffffffu\n\
    #define LOOP_CHUNK 2\n#define LOOP_EXIT 3\n\n";

/// The flag registers of the guest (AArch64 has no P and A: a liveness set
/// that says they are live, because an exit reads "everything", must not make
/// a caller ask for flags).
const FLAG_REGS: &[GuestReg] = if gex_abi::X86 {
    &[GuestReg::N, GuestReg::Z, GuestReg::C, GuestReg::V, GuestReg::P, GuestReg::A]
} else {
    &[GuestReg::N, GuestReg::Z, GuestReg::C, GuestReg::V]
};

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
pub(crate) fn mutate(name: &str) -> bool {
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
    /// Known calls whose callee is trap-free (liveness-limited write-back).
    pub calls_trapfree: usize,
    pub tail_known: usize,
    pub switches: usize,
    pub loops_outlined: usize,
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
        self.calls_trapfree += o.calls_trapfree;
        self.tail_known += o.tail_known;
        self.switches += o.switches;
        self.loops_outlined += o.loops_outlined;
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
    /// How far a `ret` moves SP: 8 on x86-64, 4 for an i386 program.
    pub ret_sp_delta: u64,
    /// Registers live on entry to each block of the function being emitted
    /// (`live_in`): write-backs on internal edges and at calls to functions
    /// with a summary store only these.
    live: Vec<RegSet>,
    /// A unit for `standalone::standalone`: everything `static`, and no
    /// dispatch roots (`e_<F>`), so nothing imports `gexc.dispatch`.
    standalone: bool,
}

impl Default for FnUnit {
    fn default() -> Self {
        Self::new()
    }
}

impl FnUnit {
    /// For an i386 (compatibility-mode) program.
    pub fn set_class32(&mut self, class32: bool) {
        self.ret_sp_delta = ret_sp_delta(class32);
    }

    pub fn new() -> FnUnit {
        FnUnit {
            stats: FnStats::default(),
            protos: BTreeSet::new(),
            bodies: String::new(),
            uses_call_known: false,
            weight: 0,
            ret_sp_delta: ret_sp_delta(false),
            live: Vec::new(),
            standalone: false,
        }
    }

    /// A unit for a standalone translation unit (`crate::standalone`).
    pub fn new_standalone() -> FnUnit {
        FnUnit {
            standalone: true,
            ..FnUnit::new()
        }
    }

    fn storage(&self) -> &'static str {
        if self.standalone {
            "static "
        } else {
            ""
        }
    }

    /// Add `f`. `known` is every function entry with a C body in this module;
    /// `sums` are S2's call-graph summaries, by entry (may be empty).
    ///
    /// `trapfree` maps the trap-free functions (`trap_free`) to their
    /// subtree's stack estimate.
    /// A body for `entry` that only leaves for it (PC = entry, return 0), so
    /// the nest unwinds and tier-1 runs the function: for a function whose C
    /// cannot be spliced (clang gave it a shadow stack). Its callers wrote
    /// everything back before the call, as for any function that is not
    /// trap-free, so it must not be one.
    pub fn add_leave_stub(&mut self, entry: u64) {
        let st = self.storage();
        let _ = writeln!(
            self.bodies,
            "{st}int {}(int ctx, int32_t depth, uint32_t need, uint64_t ret) {{\n  \
             (void)depth; (void)need; (void)ret;\n  GEX_C64({}u) = 0x{entry:x}ull;\n  return 0;\n}}\n",
            body_name(entry),
            ctx::PC
        );
        self.protos.insert(entry);
    }

    pub fn add(
        &mut self,
        f: &Function,
        known: &HashSet<u64>,
        sums: &HashMap<u64, Summary>,
        trapfree: &HashMap<u64, u32>,
    ) -> Result<()> {
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
        // The body's head, kept aside: its loop functions go first.
        let mut fhead = String::new();
        let _ = writeln!(fhead, "{}int {name}(int ctx, int32_t depth, uint32_t need, uint64_t ret) {{", self.storage());
        for r in &used {
            let _ = writeln!(fhead, "  {} {};", emit::cty(r.ty()), reg_name(*r));
        }
        let _ = writeln!(fhead, "  int32_t poll = {LOOP_POLL};");
        // Set when a poll expires anywhere: the next outermost back-edge
        // hands the loop over (§7.2), wherever the hot inner loop is.
        let _ = writeln!(fhead, "  int hot = 0; (void)hot;");
        // The recursion-depth guard, before anything is loaded: every caller
        // wrote everything back before calling, so leaving for this
        // function's own entry here is exactly the caller leaving for it.
        // A trap-free function has none (its caller may not have written
        // everything back, and it must not leave): its whole subtree fits in
        // the reserve every other function keeps below the budget.
        if !trapfree.contains_key(&f.entry) {
            let _ = writeln!(
                fhead,
                "  if (__builtin_expect(depth > {}, 0)) {{ GEX_C64({}u) = 0x{:x}ull; return 0; }}",
                stack_budget().saturating_sub(self.weight + TRAPFREE_RESERVE),
                ctx::PC,
                f.entry
            );
        }
        e.load_regs(&used);
        // The stack pointer on entry: a return is the one the caller
        // expects when it goes to `ret` with SP back here (plus the popped
        // return address on x86-64).
        e.line(format!("const uint64_t sp0 = {};", reg_name(sp_reg())));
        // Outermost loops become functions of their own (`outer_loops`,
        // docs/function-tier.md §7.2), emitted before this one.
        let loops = if outline_loops() { outer_loops(f) } else { Vec::new() };
        for (k, lp) in loops.iter().enumerate() {
            self.stats.loops_outlined += 1;
            let lname = loop_name(f.entry, k);
            let mut le = Emitter::new(&dummy, &owned);
            le.resize(f.n_values as usize);
            // Only the registers the loop itself touches are locals here:
            // everything else is already in the context (the body wrote
            // everything back before the call) and stays there, which
            // keeps the loop's register pressure to its own.
            let in_loop = |i: usize| lp.body.contains(&i);
            let mut lregs = registers_in(f, &in_loop);
            lregs.clean_spills = clean_spills();
            let lused: Vec<GuestReg> = lregs.used.iter().copied().collect();
            let ldirty = if lregs.clean_spills {
                dirty_in_from(f, lp.header, &in_loop)
            } else {
                Vec::new()
            };
            le.func_regs = Some(lregs);
            let mut head = String::new();
            let _ = writeln!(
                head,
                "static __attribute__((noinline)) int {lname}(int ctx, int32_t depth, uint32_t need, uint64_t ret, uint64_t sp0) {{"
            );
            for r in &lused {
                let _ = writeln!(head, "  {} {};", emit::cty(r.ty()), reg_name(*r));
            }
            let _ = writeln!(head, "  int32_t poll = {LOOP_POLL};");
            let _ = writeln!(head, "  int hot = 0; (void)hot;");
            le.load_regs(&lused);
            le.line(format!("goto L{};", lp.header));
            let scope = Scope { body: &lp.body, header: lp.header, nested: lp.nested };
            for (i, bl) in f.blocks.iter().enumerate() {
                if !lp.body.contains(&i) {
                    continue;
                }
                le.line(format!("L{i}: {{"));
                if let Some(d) = ldirty.get(i) {
                    le.set_dirty(d);
                }
                for st in &bl.stmts {
                    le.stmt(st)?;
                }
                self.term(&mut le, f, i, known, sums, trapfree, &mut calls, Some(&scope), None)?;
                le.line("}");
            }
            // Every path above ends in a goto or a return.
            le.line("__builtin_unreachable();");
            self.bodies.push_str(&head);
            self.bodies.push_str(&le.take_output());
            self.bodies.push_str("}\n\n");
        }

        self.bodies.push_str(&fhead);
        // Block 0 is the entry and comes first, so control falls into it.
        for (i, b) in f.blocks.iter().enumerate() {
            e.line(format!("L{i}: {{"));
            if let Some(d) = dirty.get(i) {
                e.set_dirty(d);
            }
            for s in &b.stmts {
                e.stmt(s)?;
            }
            let heads = Heads { loops: &loops, fname: f.entry, used: &used };
            self.term(&mut e, f, i, known, sums, trapfree, &mut calls, None, Some(&heads))?;
            e.line("}");
        }
        self.bodies.push_str(&e.take_output());
        self.bodies.push_str("}\n\n");

        // The entry and nest root. A standalone unit has none: its caller
        // dispatches on `ctx::PC` itself.
        if self.standalone {
            self.protos.insert(f.entry);
            self.protos.extend(calls);
            return Ok(());
        }
        let _ = writeln!(self.bodies, "void {}(int ctx) {{", entry_name(f.entry));
        let root_block = Block::new(f.entry);
        let mut r = Emitter::new(&root_block, &owned);
        if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
            r.target_check();
        }
        // No caller to predict: `ret` is an address no code has, so a
        // return writes everything back and sets PC; should the guest really
        // return there, PC says so and dispatch faults as tier-1's would.
        r.line(format!("if ({name}(ctx, 0, NEED_ALL, ~0ull)) GEX_C64({}u) = ~0ull;", ctx::PC));
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
        trapfree: &HashMap<u64, u32>,
        calls: &mut BTreeSet<u64>,
        scope: Option<&Scope<'_>>,
        heads: Option<&Heads<'_>>,
    ) -> Result<()> {
        let b = &f.blocks[i];
        let live = &self.live;
        let self_weight = self.weight;
        let goto = |e: &mut Emitter<'_>, edge: &Edge| {
            if !edge.args.is_empty() {
                unreachable!("memory form has no edge arguments");
            }
            let to = &f.blocks[edge.to.index()];
            if let Some(h) = heads {
                let t = edge.to.index();
                if let Some((k, lp)) = h.loops.iter().enumerate().find(|(_, lp)| lp.header == t) {
                    if lp.body.contains(&i) && !to.synthetic && to.start <= b.start {
                        // An outermost loop's back-edge in the body itself: the
                        // first 1024 trips run here; a loop that keeps going
                        // moves to its loop function, a fresh call per chunk
                        // (§7.2), and the body resumes where it leaves.
                        // A poll that expired anywhere in the loop (an inner
                        // loop's, usually) set `hot`; this edge's own counts
                        // too.
                        e.line("if (--poll <= 0) {");
                        e.line("  poll = 1024;");
                        e.flush_keep();
                        e.store_pc(&format!("0x{:x}ull", to.start));
                        e.line(format!("  gex_safepoint(ctx, 0x{:x}ll);", to.start));
                        if lp.nested {
                            e.line("  hot = 1;");
                            e.line("}");
                            e.line("if (hot) {");
                            e.flush_keep();
                        }
                        e.line("  int lr;");
                        e.line(format!(
                            "  do lr = {}(ctx, depth + {}, need, ret, sp0); while (lr == LOOP_CHUNK);",
                            loop_name(h.fname, k),
                            self_weight
                        ));
                        e.line("  if (lr <= 1) return lr;");
                        e.load_regs(h.used);
                        e.line("  switch (lr) {");
                        for x in &lp.exits {
                            e.line(format!("  case LOOP_EXIT + {x}: goto L{x};"));
                        }
                        e.line("  }");
                        e.line("  __builtin_unreachable();");
                        e.line("}");
                        e.line(format!("goto L{t};"));
                        return;
                    }
                }
            }
            if let Some(sc) = scope {
                let t = edge.to.index();
                if !sc.body.contains(&t) {
                    // Leaving the loop: everything back to the context, and
                    // the function carries on at `t`.
                    e.flush_keep();
                    e.line(format!("return LOOP_EXIT + {t};"));
                    return;
                }
                if t == sc.header && !to.synthetic && to.start <= b.start {
                    // The loop's own back-edge: a chunk ends here when the
                    // countdown expires (after the safepoint, as ever).
                    e.line("if (--poll <= 0) {");
                    e.flush_keep();
                    e.store_pc(&format!("0x{:x}ull", to.start));
                    e.line(format!("  gex_safepoint(ctx, 0x{:x}ll);", to.start));
                    e.line("  return LOOP_CHUNK;");
                    e.line("}");
                    if sc.nested {
                        // An inner loop's poll expired during this trip: end
                        // the chunk here too, so a loop whose trips are long
                        // still gets fresh calls.
                        e.line("if (hot) {");
                        e.flush_keep();
                        e.line("  return LOOP_CHUNK;");
                        e.line("}");
                    }
                    e.line(format!("goto L{t};"));
                    return;
                }
            }
            // tier-1's rule for a backward branch: poll. Here the countdown is
            // a local (tier-1's fused loops do the same) and the slow path
            // writes everything back, since the host may deliver a signal.
            // (`GEX_CGEN_EXP=NOPOLL` drops inner polls: unsound, pricing only.)
            if !to.synthetic && to.start <= b.start && !exp("NOPOLL") {
                e.line("if (--poll <= 0) {");
                e.line("  poll = 1024;");
                // Every dirty register, as tier-1 has them at a safepoint;
                // the flags only when the loop head may read one (tier-1's
                // dead-flag elimination leaves dead flags stale too).
                e.flush_keep_live(non_flags().union(live[edge.to.index()]));
                e.store_pc(&format!("0x{:x}ull", to.start));
                e.line(format!("  gex_safepoint(ctx, 0x{:x}ll);", to.start));
                // An inner loop of a nested outermost loop marks it hot.
                let in_nested = scope.is_some_and(|sc| sc.nested)
                    || heads.is_some_and(|h| h.loops.iter().any(|lp| lp.nested && lp.body.contains(&i)));
                if in_nested {
                    e.line("  hot = 1;");
                }
                e.line("}");
            }
            e.line(format!("goto L{};", edge.to.index()));
        };
        // Write back, set PC, return: the nest root dispatches from there.
        let leave = |e: &mut Emitter<'_>, pc: &str| {
            e.flush();
            e.store_pc(pc);
            e.line("return 0;");
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
                //
                // Registers other than flags are limited by liveness only for
                // a trap-free callee (`trap_free`): nothing the host observes
                // or resumes from can happen before it returns. Otherwise
                // every dirty register is written back, so the context is
                // what tier-1's would be wherever the host looks.
                let need = match (callee, ret) {
                    (Callee::Direct(t), Some(r)) if known.contains(t) => match sums.get(t) {
                        Some(sm) => {
                            let n = sm.inputs.union(live[r.to.index()]).union(RegSet::of(&[sp_reg()]));
                            if trapfree.contains_key(t) || mutate("IGNOREUNWIND") {
                                self.stats.calls_trapfree += 1;
                                n
                            } else {
                                n.union(non_flags())
                            }
                        }
                        None => RegSet::ALL,
                    },
                    _ => RegSet::ALL,
                };
                e.flush_live(need);
                // What the callee must hand back: the flags only if the
                // continuation reads one; everything when there is no
                // continuation (believed not to return).
                let (need_back, ret_arg) = match ret {
                    Some(r) if live_writebacks() => {
                        let fl = !self.live[r.to.index()].inter(RegSet::of(FLAG_REGS)).is_empty();
                        (if fl { "NEED_ALL" } else { "0" }, format!("0x{ret_pc:x}ull"))
                    }
                    Some(_) | None => ("NEED_ALL", format!("0x{ret_pc:x}ull")),
                };
                e.line(format!(
                    "{{ const uint64_t sp_want = {spn} + {}ull;", self.ret_sp_delta
                ));
                match callee {
                    Callee::Direct(t) if known.contains(t) => {
                        calls.insert(*t);
                        self.stats.calls_known += 1;
                        e.line(format!(
                            "  if (!{}(ctx, depth + {}, {need_back}, {ret_arg})) return 0;",
                            body_name(*t),
                            self.weight
                        ));
                    }
                    Callee::Direct(t) => {
                        *self.stats.leaves.entry(*t).or_default() += 1;
                        // Not ours: the rest of this function runs in tier-1.
                        e.store_pc(&format!("0x{t:x}ull"));
                        e.line("  return 0;");
                    }
                    Callee::Indirect(v) => {
                        self.uses_call_known = true;
                        self.stats.calls_indirect += 1;
                        let t = e.v(*v);
                        e.line(format!(
                            "  int rk = {CALL_KNOWN}(ctx, {t}, depth + {}, {need_back}, {ret_arg});",
                            self.weight + CALL_KNOWN_WEIGHT
                        ));
                        e.line("  if (rk < 0) {");
                        e.store_pc(&t);
                        e.line("    return 0;");
                        e.line("  }");
                        e.line("  if (!rk) return 0;");
                    }
                }
                // The return check is the callee's result: 1 when it returned
                // to `ret_pc` with SP back (it wrote back what the
                // continuation reads), 0 when control went anywhere else
                // (everything written back, PC set): then leave too.
                e.line("  (void)sp_want;");
                e.line("}");
                // A summarised callee that returned as expected left its
                // preserved registers as they were: no reload.
                match callee {
                    Callee::Direct(t) if known.contains(t) && live_writebacks() => {
                        match sums.get(t) {
                            Some(sm) => e.invalidate_except(sm.preserved.minus(RegSet::of(&[sp_reg()]))),
                            None => e.invalidate(),
                        }
                    }
                    _ => e.invalidate(),
                }
                match ret {
                    Some(edge) => goto(e, edge),
                    // Believed not to return, and it did: leave to the
                    // return address, with everything written back.
                    None => {
                        e.store_pc(&format!("0x{ret_pc:x}ull"));
                        e.line("return 0;");
                    }
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
                    // A tail call does not pass through a loop back-edge or
                    // the root, so it polls (returns no longer do).
                    e.safepoint(&format!("0x{t:x}ull"));
                    if scope.is_some() {
                        // A loop function's signature is not the bodies':
                        // an ordinary call (one more frame) instead.
                        e.line(format!("return {}(ctx, depth + {}, need, ret);", body_name(*t), self.weight));
                    } else {
                        e.line(format!("GEX_MUSTTAIL return {}(ctx, depth, need, ret);", body_name(*t)));
                    }
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
                if matches!(b.term, FTerm::Return { .. }) {
                    // The return the caller expects: write back what its
                    // continuation may read (everything but the flags, and
                    // the flags when `need` says so) and report success;
                    // the caller carries on without looking at the context.
                    // `GEX_CGEN_MUTATE=NORETCHECK` skips the check.
                    let spn = reg_name(sp_reg());
                    if mutate("NORETCHECK") {
                        e.line("if (1) {");
                    } else {
                        e.line(format!(
                            "if (__builtin_expect({t} == ret && {spn} == sp0 + {}ull, 1)) {{", self.ret_sp_delta
                        ));
                    }
                    let flags: Vec<GuestReg> = FLAG_REGS.to_vec();
                    e.flush_keep_live(RegSet::ALL.minus(RegSet::of(&flags)));
                    e.line("  if (need & NEED_FLAGS) {");
                    e.flush_keep_live(RegSet::of(&flags));
                    e.line("  }");
                    e.line("  return 1;");
                    e.line("}");
                }
                // Anywhere else: everything written back, PC set; the callers
                // unwind and the root polls and dispatches.
                e.flush();
                e.store_pc(&t);
                e.line("return 0;");
            }
            FTerm::Syscall { next_pc, next } => {
                e.flush();
                e.store_pc(&format!("0x{next_pc:x}ull"));
                e.line(format!("gex_syscall(ctx, 0x{:x}ll);", b.term_pc));
                e.invalidate();
                match next {
                    Some(edge) => goto(e, edge),
                    None => e.line("return 0;"),
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

    /// Bytes of C bodies emitted so far.
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    pub fn source(&self) -> String {
        let mut s = String::from("#include \"gex_abi.h\"\n\n");
        s.push_str(NEED_DEFS);
        let st = self.storage();
        for a in &self.protos {
            let _ = writeln!(s, "{st}int {}(int ctx, int32_t depth, uint32_t need, uint64_t ret);", body_name(*a));
        }
        let _ = writeln!(s, "{st}int {CALL_KNOWN}(int ctx, uint64_t target, int32_t depth, uint32_t need, uint64_t ret);\n");
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
        let _ = writeln!(s, "int {}(int ctx, int32_t depth, uint32_t need, uint64_t ret);", body_name(*a));
    }
    let _ = writeln!(s, "\nint {CALL_KNOWN}(int ctx, uint64_t target, int32_t depth, uint32_t need, uint64_t ret) {{");
    s.push_str("  switch (target) {\n");
    for a in entries {
        let _ = writeln!(s, "  case 0x{a:x}ull: return {}(ctx, depth, need, ret);", body_name(*a));
    }
    s.push_str("  default: return -1;\n  }\n}\n");
    s
}

/// [`call_known_source`] for many entries: clang takes minutes over one
/// `switch` of a hundred thousand cases, so the entries are cut into chunks
/// of [`CALL_KNOWN_CHUNK`], each a small function with its own `switch`, and
/// the helper picks the chunk by a balanced tree of comparisons.
pub const CALL_KNOWN_CHUNK: usize = 1024;

pub fn call_known_source_chunked(entries: &[u64]) -> String {
    if entries.len() <= CALL_KNOWN_CHUNK {
        return call_known_source(entries);
    }
    let mut sorted = entries.to_vec();
    sorted.sort_unstable();
    let chunks: Vec<&[u64]> = sorted.chunks(CALL_KNOWN_CHUNK).collect();
    let mut s = String::from("#include \"gex_abi.h\"\n\n");
    for a in &sorted {
        let _ = writeln!(s, "int {}(int ctx, int32_t depth, uint32_t need, uint64_t ret);", body_name(*a));
    }
    for (k, c) in chunks.iter().enumerate() {
        let _ = writeln!(s, "\nstatic int ck_{k}(int ctx, uint64_t target, int32_t depth, uint32_t need, uint64_t ret) {{");
        s.push_str("  switch (target) {\n");
        for a in c.iter() {
            let _ = writeln!(s, "  case 0x{a:x}ull: return {}(ctx, depth, need, ret);", body_name(*a));
        }
        s.push_str("  default: return -1;\n  }\n}\n");
    }
    // A chunk is picked by its first entry: chunk k holds [first[k], first[k+1]).
    fn tree(s: &mut String, chunks: &[&[u64]], lo: usize, hi: usize, indent: usize) {
        let pad = "  ".repeat(indent);
        if hi - lo == 1 {
            let _ = writeln!(s, "{pad}return ck_{lo}(ctx, target, depth, need, ret);");
            return;
        }
        let mid = (lo + hi) / 2;
        let _ = writeln!(s, "{pad}if (target < 0x{:x}ull) {{", chunks[mid][0]);
        tree(s, chunks, lo, mid, indent + 1);
        let _ = writeln!(s, "{pad}}}");
        tree(s, chunks, mid, hi, indent);
    }
    let _ = writeln!(s, "\nint {CALL_KNOWN}(int ctx, uint64_t target, int32_t depth, uint32_t need, uint64_t ret) {{");
    tree(&mut s, &chunks, 0, chunks.len(), 1);
    s.push_str("}\n");
    s
}

/// Every register the function reads or writes, and every one it writes.
/// The stack pointer is always used: the return check reads it.
fn registers(f: &Function) -> FnRegs {
    registers_in(f, &|_| true)
}

/// [`registers`] over the blocks `keep` admits (a loop function's).
fn registers_in(f: &Function, keep: &dyn Fn(usize) -> bool) -> FnRegs {
    let mut used = BTreeSet::new();
    let mut written = BTreeSet::new();
    used.insert(sp_reg());
    for (i, b) in f.blocks.iter().enumerate() {
        if !keep(i) {
            continue;
        }
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
    dirty_in_from(f, 0, &|_| true)
}

/// [`dirty_in`] for the blocks `keep` admits, entered (clean) at `root`:
/// a loop function, whose caller wrote everything back before calling it.
fn dirty_in_from(f: &Function, root: usize, keep: &dyn Fn(usize) -> bool) -> Vec<BTreeSet<GuestReg>> {
    let n = f.blocks.len();
    let mut inn: Vec<Option<BTreeSet<GuestReg>>> = vec![None; n];
    inn[root] = Some(BTreeSet::new());
    let order: Vec<BlockId> = f.rpo().into_iter().filter(|b| keep(b.index())).collect();
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
                if !keep(s.index()) {
                    continue;
                }
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
                    // A trap reads everything (on its faulting path).
                    Stmt::Trap { .. } => l = RegSet::ALL,
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

/// Every register but the flags.
fn non_flags() -> RegSet {
    RegSet::ALL.minus(RegSet::of(FLAG_REGS))
}

/// Stack every non-trap-free function keeps free below the budget for a
/// trap-free subtree, which has no depth guard of its own.
pub const TRAPFREE_RESERVE: u32 = 64 << 10;

/// The trap-free functions, with the stack estimate of their whole subtree.
///
/// Trap-free: once called, the function returns to its caller without
/// anything the host could observe or resume from: no syscall, no safepoint
/// (no backward edge, no tail call), no clobbering host call, no exit that
/// leaves the function tier (unresolved or incomplete-switch jumps,
/// indirect calls, calls to anything but trap-free functions), no illegal
/// instruction, and not recursive. S2's summary must also say it cannot
/// unwind, leave pending or reach unknown code. Its callers then write back
/// only what it reads and what their continuation reads: registers they
/// hold dead in locals are never seen by anyone. Its subtree must fit in
/// `TRAPFREE_RESERVE`, since it has no depth guard.
pub fn trap_free(funcs: &[Function], sums: &HashMap<u64, Summary>) -> HashMap<u64, u32> {
    let by_entry: HashMap<u64, &Function> = funcs.iter().map(|f| (f.entry, f)).collect();
    // Local conditions, and the direct callees.
    let mut cand: HashMap<u64, (u32, Vec<u64>)> = HashMap::new();
    'f: for f in funcs {
        let Some(sm) = sums.get(&f.entry) else { continue };
        if sm.may_unwind || sm.may_leave_pending || sm.unknown_callee {
            continue;
        }
        let mut callees = Vec::new();
        for b in &f.blocks {
            if b.stmts.iter().any(|s| emit::clobbers(s) || matches!(s, Stmt::Trap { .. })) {
                continue 'f;
            }
            let backward = |e: &Edge| {
                let to = &f.blocks[e.to.index()];
                !to.synthetic && to.start <= b.start
            };
            match &b.term {
                FTerm::Jump(e) if !backward(e) => {}
                FTerm::Branch {
                    taken, not_taken, ..
                } if !backward(taken) && !backward(not_taken) => {}
                FTerm::Switch { cases, table, .. }
                    if table.complete && !cases.iter().any(|(_, e)| backward(e)) => {}
                FTerm::Call {
                    callee: Callee::Direct(t),
                    ret: Some(r),
                    ..
                } if by_entry.contains_key(t) && !backward(r) => callees.push(*t),
                FTerm::Return { .. } => {}
                _ => continue 'f,
            }
        }
        cand.insert(f.entry, (frame_weight(&registers(f)), callees));
    }
    // Callees trap-free too, no recursion, bounded stack: a depth-first
    // walk that memoises the subtree's stack and treats a cycle as failure.
    fn visit(
        a: u64,
        cand: &HashMap<u64, (u32, Vec<u64>)>,
        memo: &mut HashMap<u64, Option<u32>>,
        onstack: &mut HashSet<u64>,
    ) -> Option<u32> {
        if let Some(m) = memo.get(&a) {
            return *m;
        }
        if !onstack.insert(a) {
            return None; // recursion
        }
        let r = cand.get(&a).and_then(|(w, callees)| {
            let mut deepest = 0u32;
            for c in callees {
                deepest = deepest.max(visit(*c, cand, memo, onstack)? + CALL_KNOWN_WEIGHT);
            }
            Some(w + deepest).filter(|n| *n <= TRAPFREE_RESERVE)
        });
        onstack.remove(&a);
        memo.insert(a, r);
        r
    }
    let mut memo = HashMap::new();
    let mut onstack = HashSet::new();
    let keys: Vec<u64> = cand.keys().copied().collect();
    for a in keys {
        visit(a, &cand, &mut memo, &mut onstack);
    }
    memo.into_iter().filter_map(|(a, n)| n.map(|n| (a, n))).collect()
}

/// Outlining outermost loops is on unless `GEX_CGEN_OUTLINE=0`.
fn outline_loops() -> bool {
    std::env::var("GEX_CGEN_OUTLINE").as_deref() != Ok("0")
}

fn loop_name(entry: u64, k: usize) -> String {
    format!("l_{entry:x}_{k}")
}

/// The body's view of its outlined loops.
pub(crate) struct Heads<'a> {
    loops: &'a [Loop],
    fname: u64,
    used: &'a [GuestReg],
}

/// The blocks a loop function emits.
pub(crate) struct Scope<'a> {
    body: &'a BTreeSet<usize>,
    header: usize,
    nested: bool,
}

/// An outermost natural loop: its header, its blocks, and the blocks outside
/// it that its edges reach.
struct Loop {
    header: usize,
    body: BTreeSet<usize>,
    exits: BTreeSet<usize>,
    /// It contains an inner loop: a poll that expires there marks the
    /// outer loop hot, and its back-edge hands over (§7.2).
    nested: bool,
}

/// The outermost natural loops of `f`, for outlining (docs/function-tier.md
/// §7.2): V8 never replaces a running frame's baseline code, so a loop that
/// runs for the whole of one call of a function called once (a `main`)
/// would never reach optimised code. Outlined, each chunk of iterations is a
/// new call. None when the CFG has a backward edge that is not a natural
/// loop's (irreducible): outlining needs the header to be the only way in.
fn outer_loops(f: &Function) -> Vec<Loop> {
    let n = f.blocks.len();
    let idom = f.idoms();
    let dominates = |h: usize, mut b: usize| -> bool {
        loop {
            if b == h {
                return true;
            }
            match idom[b] {
                Some(d) => b = d.index(),
                None => return false,
            }
        }
    };
    let preds = f.preds();
    let reachable: HashSet<usize> = f.rpo().iter().map(|b| b.index()).collect();
    let mut by_header: std::collections::BTreeMap<usize, BTreeSet<usize>> = Default::default();
    for (i, b) in f.blocks.iter().enumerate() {
        if !reachable.contains(&i) {
            continue;
        }
        for s in b.succs() {
            let h = s.index();
            if !dominates(h, i) {
                // Not a natural back-edge; a retreating edge in RPO would be
                // irreducible. Checked below by containment instead.
                continue;
            }
            // Natural loop of i -> h.
            let body = by_header.entry(h).or_default();
            body.insert(h);
            // Everything that reaches the latch without passing the header.
            let mut work = vec![i];
            while let Some(x) = work.pop() {
                if x == h {
                    continue; // a self-loop's latch is its header
                }
                body.insert(x);
                for p in &preds[x] {
                    if !body.contains(&p.index()) && reachable.contains(&p.index()) {
                        work.push(p.index());
                    }
                }
            }
        }
    }
    // Irreducible control flow (a cycle with no dominating header): leave
    // the function alone.
    let order: HashMap<usize, usize> = f.rpo().iter().enumerate().map(|(k, b)| (b.index(), k)).collect();
    for (i, b) in f.blocks.iter().enumerate() {
        let Some(&oi) = order.get(&i) else { continue };
        for s in b.succs() {
            if let Some(&os) = order.get(&s.index()) {
                if os <= oi && !dominates(s.index(), i) {
                    return Vec::new();
                }
            }
        }
    }
    // Outermost: a header no other loop's body contains.
    let headers: Vec<usize> = by_header.keys().copied().collect();
    let mut out = Vec::new();
    for &h in &headers {
        if headers.iter().any(|&o| o != h && by_header[&o].contains(&h)) {
            continue;
        }
        let body = by_header[&h].clone();
        let mut exits = BTreeSet::new();
        for &bi in &body {
            for s in f.blocks[bi].succs() {
                if !body.contains(&s.index()) {
                    exits.insert(s.index());
                }
            }
        }
        let nested = body.iter().any(|&bi| {
            let b = &f.blocks[bi];
            b.succs().iter().any(|s| {
                let t = s.index();
                t != h && body.contains(&t) && !f.blocks[t].synthetic && f.blocks[t].start <= b.start
            })
        });
        // `GEX_CGEN_HOT=0`: hand over only on the loop's own back-edge, for
        // pricing.
        let nested = nested && std::env::var("GEX_CGEN_HOT").as_deref() != Ok("0");
        out.push(Loop { header: h, body, exits, nested });
    }
    let _ = n;
    out
}

/// The guest function a C function belongs to, from its name: `f_<entry>`,
/// `e_<entry>` or a loop function `l_<entry>_<k>`.
pub fn guest_entry_of(name: &str) -> Option<u64> {
    let rest = name
        .strip_prefix("f_")
        .or_else(|| name.strip_prefix("e_"))
        .or_else(|| name.strip_prefix("l_").map(|r| r.split('_').next().unwrap_or(r)))?;
    u64::from_str_radix(rest, 16).ok()
}
