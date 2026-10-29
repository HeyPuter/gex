//! gex hosted in another program's WebAssembly module.
//!
//! Everywhere else gex is the operating system: it owns the guest's memory from address zero, lays out its tables at
//! fixed addresses (`gex_abi`), and runs its own kernel and allocator. Here it is a library inside an emscripten
//! program (android-wasm's `dotnet.native.wasm`, which runs Android apps on mono and IKVM). That program owns the
//! memory: its data starts at 0x400, dlmalloc owns the heap, and its pthreads are Workers. gex lives inside it:
//! * its tables sit in a block the host's link placed (`gex_abi::hosted`);
//! * guest mappings come from the host's `memalign`;
//! * guest syscalls go to the host's libc;
//! * guest code runs on whichever host thread calls into it.
//!
//! The point is the shared memory. A JNI array, a direct buffer, a GL client array or an asset buffer is the same
//! bytes on both sides, with no copy.
//!
//! The crate is a `staticlib` for `wasm32-unknown-unknown`, built with the host's exact wasm features (`build.sh`) and
//! linked by emcc beside the host's own objects, with `js/gex_embed.js` as a `--js-library`. Nothing in it may assume
//! a Rust runtime: there is no `main`, no `std::thread`, and Rust thread-locals are only for state that dies with the
//! pthread. Workers outlive their pthreads, so anything per-Worker lives in the JS library.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

mod alloc;
mod call;
mod cpu;
mod host;
mod imports;
mod jit;
mod layout;
mod sys;
mod thread;

pub use call::Call;
pub use imports::HostFn;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

/// What the JS library reads to set a Worker up (`js/gex_embed.js`, `gexInit`).
#[repr(C)]
struct Desc {
    /// The seed module: the entry and miss stubs and the dispatch tables, one instance per Worker.
    seed: u32,
    seed_len: u32,
    /// The module log (`jit::Row`s) and the address of its length.
    rows: u32,
    log_len: u32,
    row_size: u32,
    /// The imports' function-table indices (`imports::table`).
    imports: [u32; 10],
}

static DESC: OnceLock<Desc> = OnceLock::new();

/// The descriptor's address, for the JS library.
pub(crate) fn desc() -> u32 {
    DESC.get().expect("gex_embed_init first") as *const Desc as usize as u32
}

/// `gex_embed_init`'s settings.
#[repr(C)]
pub struct Config {
    /// The lowest address the guest may name: the host's `GLOBAL_BASE`.
    pub floor: u32,
    /// Each thread's guest stack, in bytes (0: 1 MiB).
    pub stack_size: u32,
}

/// Set gex up in this program. Once, before anything else here, on any thread.
#[no_mangle]
pub extern "C" fn gex_embed_init(cfg: *const Config) -> i32 {
    // SAFETY: the caller's settings, or none.
    let cfg = unsafe { cfg.as_ref() };
    DESC.get_or_init(|| {
        host::install_panic_hook();
        layout::init(cfg.map_or(0x400, |c| c.floor as u64));
        if let Some(c) = cfg.filter(|c| c.stack_size != 0) {
            thread::STACK_SIZE.store(c.stack_size as usize, Ordering::Relaxed);
        }
        jit::init();
        let seed = gex_xlate::seed_module(true).wasm.leak();
        Desc {
            seed: seed.as_ptr() as usize as u32,
            seed_len: seed.len() as u32,
            rows: jit::rows(),
            log_len: &jit::LOG_LEN as *const AtomicU32 as usize as u32,
            row_size: core::mem::size_of::<jit::Row>() as u32,
            imports: imports::table(),
        }
    });
    0
}

/// Mark `[addr, addr + len)` executable: guest code may run there.
#[no_mangle]
pub extern "C" fn gex_embed_map_exec(addr: u64, len: u64) {
    jit::map_exec(addr, addr + len);
}

/// A guest address that, called, runs host function `f` with `user`: `f` reads the AAPCS64 arguments from the
/// context and writes its results there, and the guest carries on at `x30`.
#[no_mangle]
pub extern "C" fn gex_embed_thunk(f: HostFn, user: u32) -> u64 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    assert!(id < layout::THUNKS, "out of host-function thunks");
    imports::set_host_fn(id, f, user);
    let pc = layout::thunk(id);
    jit::add_intrinsic(pc, gex_xlate::Intrinsic::Replace(id));
    pc
}

/// Call the guest function at `pc` on this thread with `c`'s arguments; its results are left in `c`.
#[no_mangle]
pub extern "C" fn gex_embed_call(pc: u64, c: *mut Call) -> i32 {
    // SAFETY: the caller's.
    call::call(pc, unsafe { &mut *c });
    0
}

/// Register `n` of a context (31 is SP).
#[no_mangle]
pub extern "C" fn gex_ctx_x(ctx: u32, n: u32) -> u64 {
    cpu::x(ctx, n as u8)
}

#[no_mangle]
pub extern "C" fn gex_ctx_set_x(ctx: u32, n: u32, v: u64) {
    cpu::set_x(ctx, n as u8, v)
}

/// The low 64 bits of `v<n>` (`d<n>`).
#[no_mangle]
pub extern "C" fn gex_ctx_d(ctx: u32, n: u32) -> u64 {
    cpu::v(ctx, n as u8)[0]
}

#[no_mangle]
pub extern "C" fn gex_ctx_set_d(ctx: u32, n: u32, v: u64) {
    cpu::set_v(ctx, n as u8, [v, 0])
}
