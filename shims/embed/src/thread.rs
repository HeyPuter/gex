//! Guest threads: any host thread that calls into guest code gets a context (`gex_abi::ctx`) and a guest stack, the
//! first time it does, and -- once libc is loaded -- is adopted as a bionic thread (`__aw_thread_attach`, linked into
//! the guest's libc by android-wasm's native/guest): a TCB, static TLS, errno, a stack guard.
//!
//! Per pthread, in Rust thread-locals: they die with the pthread, which is what a context should do. (Per-Worker
//! state, which outlives pthreads, is the JS library's.)

use gex_abi::ctx;
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

extern "C" {
    fn memalign(align: usize, size: usize) -> *mut u8;
}

/// The guest stack a host thread gets.
pub static STACK_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1 << 20);

/// The guest's process id, and the main thread's id (bionic takes the main thread's tid to be the pid).
pub const PID: u32 = 1000;

/// The next thread id: the main thread's is the pid.
static NEXT_TID: AtomicU32 = AtomicU32::new(PID + 1);

/// `__aw_thread_attach` in the guest's libc, once it is mapped (`overlay`).
pub static ATTACH: AtomicU64 = AtomicU64::new(0);

/// The context's allocation: its size rounded to a cache line.
const CTX_BYTES: usize = (ctx::SIZE + 63) & !63;

pub struct Thread {
    /// The context, in linear memory.
    pub ctx: u32,
    /// The guest stack.
    pub stack_base: u64,
    /// Where calls' stacks start: below the stack's top, less what bionic keeps there.
    pub stack_top: u64,
    /// Host-to-guest calls this thread is inside.
    pub depth: u32,
    pub tid: u32,
    /// Whether bionic knows this thread (it booted the linker, or was adopted).
    pub bionic: bool,
}

thread_local! {
    static ME: Cell<*mut Thread> = const { Cell::new(core::ptr::null_mut()) };
}

/// This thread's guest state, made the first time it is asked for.
pub fn current() -> &'static mut Thread {
    let t = ME.with(|me| {
        if me.get().is_null() {
            me.set(Box::into_raw(Box::new(new())));
        }
        // SAFETY: this thread's own, never shared, freed only with the thread.
        unsafe { &mut *me.get() }
    });
    if !t.bionic && t.depth == 0 {
        let attach = ATTACH.load(Ordering::Acquire);
        if attach != 0 {
            t.bionic = true;
            adopt(attach);
        }
    }
    t
}

fn new() -> Thread {
    let stack_size = STACK_SIZE.load(Ordering::Relaxed);
    // SAFETY: plain allocations; the context is zeroed before anything reads it.
    let (c, stack) = unsafe { (memalign(64, CTX_BYTES), memalign(16, stack_size)) };
    assert!(!c.is_null() && !stack.is_null(), "out of memory for a guest thread");
    // And the stack, as a kernel's are: fresh pages. Code relies on it -- linker64's start reads fields of a
    // `soinfo` on its stack that nothing has written.
    unsafe {
        c.write_bytes(0, CTX_BYTES);
        stack.write_bytes(0, stack_size);
    }
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let t = Thread {
        ctx: c as usize as u32,
        stack_base: stack as usize as u64,
        stack_top: stack as usize as u64 + stack_size as u64,
        depth: 0,
        tid,
        bionic: false,
    };
    crate::cpu::set_u32(t.ctx, ctx::SAFEPOINT_COUNTER, crate::imports::SAFEPOINT_INTERVAL);
    crate::cpu::set_u64(t.ctx, ctx::TID, tid as u64);
    t
}

/// This thread boots the guest: it is the main thread, whose tid is the pid, and the linker makes it bionic's.
pub fn become_main() {
    let t = current();
    t.tid = PID;
    t.bionic = true;
    crate::cpu::set_u64(t.ctx, ctx::TID, PID as u64);
}

/// Make this thread bionic's: `__aw_thread_attach(stack_base, stack_size, &stack_top)`, on a frame near the stack's
/// base (bionic carves the thread's `pthread_internal_t` from the stack's top).
fn adopt(attach: u64) {
    let t = current_raw();
    let size = t.stack_top - t.stack_base;
    let mut top: u64 = 0;
    let saved_top = t.stack_top;
    t.stack_top = t.stack_base + (64 << 10);
    let tp = crate::call::call_x(attach, &[t.stack_base, size, &mut top as *mut u64 as usize as u64]);
    let t = current_raw();
    if tp == 0 || top == 0 {
        t.stack_top = saved_top;
        crate::host::fatal("no memory to adopt a thread into bionic");
    }
    t.stack_top = (top - 64) & !15;
}

fn current_raw() -> &'static mut Thread {
    // SAFETY: as `current`, which made it.
    ME.with(|me| unsafe { &mut *me.get() })
}
