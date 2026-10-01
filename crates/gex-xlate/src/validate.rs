//! Checking recovered functions and their summaries by running them.
//!
//! Printing and parsing show a function is well formed; this shows it means
//! the same thing. Each function is run on the GIR reference interpreter
//! ([`gex_ir::interp`]) from pseudo-random states, with its calls modelled by
//! the callees' summaries, and:
//!
//! * its SSA form must end every run exactly as its memory form does (the
//!   same exit, registers and memory);
//! * its own summary must hold on every run that returns: registers it
//!   claims to preserve come back unchanged, the stack pointer moves by
//!   `sp_delta`, it returns at all only if `may_return`, and every byte it
//!   reads or writes above its entry stack pointer lies in `stack_inputs` /
//!   `caller_frame_writes`;
//! * changing the entry value of every register outside `inputs` must not
//!   change anything else the run does.
//!
//! This is evidence, not proof: runs follow the paths random data takes,
//! stop at operations the interpreter does not model (floating point,
//! vectors), and model callees rather than run them. A failure, though, is
//! always a real bug in recovery, SSA construction or the analysis.

use crate::arch::SP_REG;
use crate::summary::Summaries;
use gex_ir::func::*;
use gex_ir::interp::{self, Model, Outcome, Stop};

/// What checking found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub functions: usize,
    /// Runs made, and how many ended somewhere comparable (not stopped at an
    /// unmodelled operation).
    pub runs: usize,
    pub runs_complete: usize,
    /// Runs that returned, on which the summary was checked.
    pub returns_checked: usize,
    /// One line per failure.
    pub failures: Vec<String>,
}

/// Bytes above the entry stack pointer that count as the caller's frame.
/// Pseudo-random pointers land near a 32-bit stack pointer often enough
/// that i386 code gets a narrower window.
fn caller_frame() -> u64 {
    if crate::arch::compat() {
        1 << 12
    } else {
        1 << 20
    }
}

/// Check every function of a program. `seeds` runs per function.
pub fn check_program(
    functions: &std::collections::BTreeMap<u64, Function>,
    sums: &Summaries,
    seeds: u64,
    max_steps: usize,
) -> Report {
    let mut rep = Report::default();
    for (&a, f) in functions {
        check_function(a, f, sums, seeds, max_steps, &mut rep);
    }
    rep
}

fn slots_contain(s: Slots, lo: i64, hi: i64) -> bool {
    match s {
        Slots::Unknown => true,
        Slots::None => false,
        Slots::Range(a, b) => a <= lo && hi <= b,
    }
}

fn complete(o: &Outcome) -> bool {
    !matches!(o.stop, Stop::Unsupported(_))
}

/// Check one function; see the module documentation.
pub fn check_function(
    a: u64,
    f: &Function,
    sums: &Summaries,
    seeds: u64,
    max_steps: usize,
    rep: &mut Report,
) {
    rep.functions += 1;
    let ssa = sums.to_ssa(f);
    let call = sums.effects_for(&[f, &ssa]);
    // Syscalls as the summaries model them.
    let nr = reg_index(crate::arch::SYSCALL_NR);
    let syscall = |regs: &[u64]| crate::summary::syscall_effect(Some(regs[nr]));
    let m = Model {
        call: &call,
        sp: SP_REG,
        max_steps,
        syscall: Some(&syscall),
        narrow: crate::arch::compat(),
    };
    let own = sums.get(a);
    let spi = reg_index(SP_REG);
    let name = f.name.as_deref().unwrap_or("-");
    // A few lines per function, so one bad function hides no other.
    let mut mine = 0;
    let mut fail = |rep: &mut Report, what: String| {
        mine += 1;
        if mine <= 4 {
            rep.failures.push(format!("{a:#x} {name}: {what}"));
        } else if mine == 5 {
            rep.failures.push(format!("{a:#x} {name}: (more)"));
        }
    };
    for seed in 0..seeds {
        let seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ a;
        let init = if m.narrow {
            interp::initial_regs32(seed, SP_REG)
        } else {
            interp::initial_regs(seed, SP_REG)
        };
        let mo = interp::run_from(f, seed, init.clone(), &m);
        rep.runs += 1;
        let so = interp::run_from(&ssa, seed, init.clone(), &m);
        if mo != so {
            fail(rep, format!("seed {seed:#x}: SSA form ends {:?}, memory form {:?}", so.stop, mo.stop));
            continue;
        }
        if !complete(&mo) {
            continue;
        }
        rep.runs_complete += 1;

        // The summary, on a return.
        if let Stop::Return(_) = mo.stop {
            rep.returns_checked += 1;
            if !own.may_return {
                fail(rep, format!("seed {seed:#x}: returns, summary says it never does"));
            }
            for r in own.preserved.iter() {
                let i = reg_index(r);
                if mo.regs[i] != init[i] {
                    fail(rep, format!("seed {seed:#x}: {} not preserved", gex_ir::text::reg_name(r)));
                }
            }
            if let Some(d) = own.sp_delta {
                let got = mo.regs[spi].wrapping_sub(init[spi]) as i64;
                if got != d {
                    fail(rep, format!("seed {seed:#x}: sp moved {got}, summary says {d}"));
                }
            }
        }
        // Stack traffic above the entry stack pointer, on any exit.
        let sp0 = init[spi];
        let above = |addr: u64| -> Option<i64> {
            let o = addr.wrapping_sub(sp0);
            (o < caller_frame()).then_some(o as i64)
        };
        for &w in mo.memory.keys() {
            if let Some(o) = above(w) {
                if !slots_contain(own.caller_frame_writes, o, o + 1) {
                    fail(rep, format!("seed {seed:#x}: writes caller frame +{o}, summary says {:?}", own.caller_frame_writes));
                    break;
                }
            }
        }
        for &r in &mo.read_before_write {
            if let Some(o) = above(r) {
                if !slots_contain(own.stack_inputs, o, o + 1) {
                    fail(rep, format!("seed {seed:#x}: reads caller frame +{o}, summary says {:?}", own.stack_inputs));
                    break;
                }
            }
        }

        // Inputs: perturb every register the summary says is not read.
        let mut pert = init.clone();
        let mut changed = Vec::new();
        for i in 0..N_REGS {
            let r = reg_of_index(i);
            if !own.inputs.contains(r) && i != spi {
                pert[i] = interp::perturb_for(r, init[i], m.narrow);
                changed.push(i);
            }
        }
        if changed.is_empty() {
            continue;
        }
        let po = interp::run_from(f, seed, pert.clone(), &m);
        if !complete(&po) {
            fail(rep, format!("seed {seed:#x}: perturbing non-inputs reaches an unmodelled operation"));
            continue;
        }
        let mut diff = None;
        if po.stop != mo.stop {
            diff = Some(format!("exit {:?} vs {:?}", po.stop, mo.stop));
        } else if po.memory != mo.memory {
            diff = Some("memory".into());
        } else {
            for i in 0..N_REGS {
                if po.regs[i] == mo.regs[i] {
                    continue;
                }
                // A perturbed register left alone by both runs.
                if changed.contains(&i) && mo.regs[i] == init[i] && po.regs[i] == pert[i] {
                    continue;
                }
                diff = Some(format!("register {}", gex_ir::text::reg_name(reg_of_index(i))));
                break;
            }
        }
        if let Some(d) = diff {
            fail(rep, format!("seed {seed:#x}: depends on a register outside inputs={:?}: {d}", own.inputs));
        }
    }
}
