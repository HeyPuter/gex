//! Guest threads: any host thread that calls into guest code gets a context (`gex_abi::ctx`) and a guest stack, the
//! first time it does.
//!
//! Per pthread, in Rust thread-locals: they die with the pthread, which is what a context should do. (Per-Worker
//! state, which outlives pthreads, is the JS library's.)

use gex_abi::ctx;
use std::cell::Cell;

extern "C" {
    fn memalign(align: usize, size: usize) -> *mut u8;
}

/// The guest stack a host thread gets.
pub static STACK_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1 << 20);

/// The context's allocation: its size rounded to a cache line.
const CTX_BYTES: usize = (ctx::SIZE + 63) & !63;

pub struct Thread {
    /// The context, in linear memory.
    pub ctx: u32,
    /// One past the top of this thread's guest stack.
    pub stack_top: u64,
    /// Host-to-guest calls this thread is inside.
    pub depth: u32,
}

thread_local! {
    static ME: Cell<*mut Thread> = const { Cell::new(core::ptr::null_mut()) };
}

/// This thread's guest state, made the first time it is asked for.
pub fn current() -> &'static mut Thread {
    ME.with(|me| {
        if me.get().is_null() {
            me.set(Box::into_raw(Box::new(attach())));
        }
        // SAFETY: this thread's own, never shared, freed only with the thread.
        unsafe { &mut *me.get() }
    })
}

fn attach() -> Thread {
    let stack_size = STACK_SIZE.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: plain allocations; the context is zeroed before anything reads it.
    let (c, stack) = unsafe { (memalign(64, CTX_BYTES), memalign(16, stack_size)) };
    assert!(!c.is_null() && !stack.is_null(), "out of memory for a guest thread");
    unsafe { c.write_bytes(0, CTX_BYTES) };
    let t = Thread { ctx: c as usize as u32, stack_top: stack as usize as u64 + stack_size as u64, depth: 0 };
    crate::cpu::set_u32(t.ctx, ctx::SAFEPOINT_COUNTER, crate::imports::SAFEPOINT_INTERVAL);
    crate::cpu::set_u64(t.ctx, ctx::TID, unsafe { crate::host::gettid() } as u64);
    t
}
