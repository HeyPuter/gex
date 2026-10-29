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

/// Run the guest function at `pc` on this thread, with `c`'s arguments, until it returns; fill in `c`'s results.
pub fn call(pc: u64, c: &mut Call) {
    let t = crate::thread::current();
    let cx = t.ctx;
    // Nothing of the caller's guest state survives the call but what the callee returns: an outer guest frame
    // (this call made from a host function the guest called) sees its own registers back.
    let saved = cpu::save(cx);
    t.depth += 1;
    let depth = t.depth;
    // Below whatever the guest is already using, or the stack's top for the first call in.
    let below = if depth == 1 { t.stack_top } else { cpu::x(cx, 31) };
    let args = c.nstack as u64 * 8;
    let sp = (below - args) & !15;
    if c.nstack > 0 {
        // SAFETY: the caller's array of `nstack` words, and this thread's stack.
        unsafe { core::ptr::copy_nonoverlapping(c.stack as usize as *const u64, sp as usize as *mut u64, c.nstack as usize) };
    }
    for i in 0..8 {
        cpu::set_x(cx, i as u8, c.x[i]);
        cpu::set_v(cx, i as u8, c.v[i]);
    }
    cpu::set_x(cx, 8, c.x8);
    cpu::set_x(cx, 30, crate::layout::pad(depth));
    cpu::set_u64(cx, ctx::SP, sp);
    cpu::set_u64(cx, ctx::PC, pc);
    // SAFETY: the JS library's; returns when the guest reaches the pad.
    unsafe { crate::host::js::gexjs_enter(crate::desc(), cx, pc) };
    c.rx = [cpu::x(cx, 0), cpu::x(cx, 1)];
    for i in 0..4 {
        c.rv[i] = cpu::v(cx, i as u8);
    }
    let t = crate::thread::current();
    t.depth -= 1;
    cpu::restore(cx, &saved);
}
