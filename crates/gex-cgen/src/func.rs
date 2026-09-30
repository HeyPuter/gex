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
use gex_ir::func::{Callee, Edge, FTerm, Form, Function};
use gex_ir::{Block, Expr, GuestReg, Stmt};
use std::collections::{BTreeSet, HashSet};
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

/// Local loop iterations between safepoint polls (tier-1's `loop_poll`).
const LOOP_POLL: i32 = 1024;

/// C for one translation unit of functions.
pub struct FnUnit {
    protos: BTreeSet<u64>,
    bodies: String,
    /// Whether any function here calls through `CALL_KNOWN`.
    pub uses_call_known: bool,
}

impl Default for FnUnit {
    fn default() -> Self {
        Self::new()
    }
}

impl FnUnit {
    pub fn new() -> FnUnit {
        FnUnit {
            protos: BTreeSet::new(),
            bodies: String::new(),
            uses_call_known: false,
        }
    }

    /// Add `f`. `known` is every function entry with a C body in this module.
    pub fn add(&mut self, f: &Function, known: &HashSet<u64>) -> Result<()> {
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
        let used: Vec<GuestReg> = regs.used.iter().copied().collect();
        e.func_regs = Some(regs);
        let mut calls: BTreeSet<u64> = BTreeSet::new();

        let name = body_name(f.entry);
        let _ = writeln!(self.bodies, "void {name}(int ctx) {{");
        for r in &used {
            let _ = writeln!(self.bodies, "  {} {};", emit::cty(r.ty()), reg_name(*r));
        }
        let _ = writeln!(self.bodies, "  int32_t poll = {LOOP_POLL};");
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
            self.term(&mut e, f, i, known, &mut calls)?;
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
        r.line(format!("{name}(ctx);"));
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
        calls: &mut BTreeSet<u64>,
    ) -> Result<()> {
        let b = &f.blocks[i];
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
                e.flush_keep();
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
                e.flush();
                e.line(format!(
                    "{{ const uint64_t sp_want = {spn} + {RET_SP_DELTA}ull;"
                ));
                match callee {
                    Callee::Direct(t) if known.contains(t) => {
                        calls.insert(*t);
                        e.line(format!("  {}(ctx);", body_name(*t)));
                    }
                    Callee::Direct(t) => {
                        // Not ours: the rest of this function runs in tier-1.
                        e.store_pc(&format!("0x{t:x}ull"));
                        e.line("  return;");
                    }
                    Callee::Indirect(v) => {
                        self.uses_call_known = true;
                        let t = e.v(*v);
                        e.line(format!("  if (!{CALL_KNOWN}(ctx, {t})) {{"));
                        e.store_pc(&t);
                        e.line("    return;");
                        e.line("  }");
                    }
                }
                // The return check; everything the callee left is in ctx.
                e.line(format!(
                    "  if (GEX_C64({}u) != 0x{ret_pc:x}ull || GEX_C64({}u) != sp_want) return;",
                    ctx::PC,
                    ctx::SP
                ));
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
                    e.flush();
                    e.line(format!("GEX_MUSTTAIL return {}(ctx);", body_name(*t)));
                }
                Callee::Direct(t) => leave(e, &format!("0x{t:x}ull")),
                Callee::Indirect(v) => {
                    let t = e.v(*v);
                    leave(e, &t)
                }
            },
            FTerm::Return { target } | FTerm::Unresolved { target } => {
                // tier-1 polls on every indirect branch; so does this, with
                // everything already written back and PC set.
                let t = e.v(*target);
                e.flush();
                e.store_pc(&t);
                e.safepoint(&t);
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
            let _ = writeln!(s, "void {}(int ctx);", body_name(*a));
        }
        let _ = writeln!(s, "int {CALL_KNOWN}(int ctx, uint64_t target);\n");
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
        let _ = writeln!(s, "void {}(int ctx);", body_name(*a));
    }
    let _ = writeln!(s, "\nint {CALL_KNOWN}(int ctx, uint64_t target) {{");
    s.push_str("  switch (target) {\n");
    for a in entries {
        let _ = writeln!(s, "  case 0x{a:x}ull: {}(ctx); return 1;", body_name(*a));
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
