//! Calling guest code from the host, by the AAPCS64.

use crate::cpu;
use gex_abi::ctx;

/// A call's arguments and results (`gex_embed.h`'s `gex_call`).
#[repr(C)]
pub struct Call {
    /// `x0`-`x7`, `v0`-`v7` (low, high), `x8` (an indirect result's address).
    pub x: [u64; 8],
    pub v: [[u64; 2]; 8],
    pub x8: u64,
    /// Arguments past the registers, eight bytes each, the first at the lowest address.
    pub stack: u32,
    pub nstack: u32,
    /// What the callee returned: `x0`, `x1`, `v0`-`v3`.
    pub rx: [u64; 2],
    pub rv: [[u64; 2]; 4],
}

/// Run guest code from `pc` on this thread until it returns to the pad `x30` holds. `setup` gets the context, the
/// pad and the highest address the call's stack may use, writes what it passes, and returns the stack pointer;
/// `finish` reads the results.
///
/// Nothing of the caller's guest state survives the call but what `finish` takes and the thread pointer: an outer
/// guest frame (this call made from a host function the guest called) sees its registers back, and a callee that
/// set TPIDR_EL0 (the linker, `__aw_thread_attach`) leaves it set.
pub fn run(pc: u64, setup: impl FnOnce(u32, u64, u64) -> u64, finish: impl FnOnce(u32)) {
    let t = crate::thread::current();
    let cx = t.ctx;
    let saved = cpu::save(cx);
    t.depth += 1;
    let depth = t.depth;
    // Below whatever the guest is already using, or the stack's top for the first call in.
    let below = if depth == 1 { t.stack_top } else { cpu::x(cx, 31) };
    let pad = crate::layout::pad(depth);
    let sp = setup(cx, pad, below);
    cpu::set_x(cx, 30, pad);
    cpu::set_u64(cx, ctx::SP, sp);
    cpu::set_u64(cx, ctx::PC, pc);
    // SAFETY: the JS library's; returns when the guest reaches the pad.
    unsafe { crate::host::js::gexjs_enter(crate::desc(), cx, pc) };
    finish(cx);
    let (tp, tpro) = (cpu::get_u64(cx, ctx::TPIDR), cpu::get_u64(cx, ctx::TPIDRRO));
    cpu::restore(cx, &saved);
    cpu::set_u64(cx, ctx::TPIDR, tp);
    cpu::set_u64(cx, ctx::TPIDRRO, tpro);
    crate::thread::current().depth -= 1;
}

/// Run the guest function at `pc` with `c`'s arguments; fill in its results.
pub fn call(pc: u64, c: &mut Call) {
    run(
        pc,
        |cx, _, below| {
            let sp = (below - c.nstack as u64 * 8) & !15;
            if c.nstack > 0 {
                // SAFETY: the caller's array of `nstack` words, and this thread's stack.
                unsafe { core::ptr::copy_nonoverlapping(c.stack as usize as *const u64, sp as usize as *mut u64, c.nstack as usize) };
            }
            for i in 0..8 {
                cpu::set_x(cx, i as u8, c.x[i]);
                cpu::set_v(cx, i as u8, c.v[i]);
            }
            cpu::set_x(cx, 8, c.x8);
            sp
        },
        |cx| {
            c.rx = [cpu::x(cx, 0), cpu::x(cx, 1)];
            for i in 0..4 {
                c.rv[i] = cpu::v(cx, i as u8);
            }
        },
    );
}

/// `f(args...)` for integer and pointer arguments: x0.
pub fn call_x(pc: u64, args: &[u64]) -> u64 {
    let mut c = Call { x: [0; 8], v: [[0; 2]; 8], x8: 0, stack: 0, nstack: 0, rx: [0; 2], rv: [[0; 2]; 4] };
    c.x[..args.len()].copy_from_slice(args);
    call(pc, &mut c);
    c.rx[0]
}
