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
mod diag;
mod elf;
mod host;
mod imports;
mod jit;
mod layout;
mod overlay;
mod sys;
mod thread;
mod vm;

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

/// A host function's id, for `Intrinsic::Replace`.
fn host_fn(f: HostFn, user: u32) -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    assert!(id < layout::THUNKS, "out of host functions");
    imports::set_host_fn(id, f, user);
    id
}

/// Guest code reaching `pc` runs host function `f` instead, and returns to `x30`.
pub(crate) fn bind(pc: u64, f: HostFn, user: u32) {
    jit::add_intrinsic(pc, gex_xlate::Intrinsic::Replace(host_fn(f, user)));
}

/// A guest address that, called, runs host function `f` with `user`: `f` reads the AAPCS64 arguments from the
/// context and writes its results there, and the guest carries on at `x30`.
#[no_mangle]
pub extern "C" fn gex_embed_thunk(f: HostFn, user: u32) -> u64 {
    let id = host_fn(f, user);
    let pc = layout::thunk(id);
    jit::add_intrinsic(pc, gex_xlate::Intrinsic::Replace(id));
    pc
}

/// Start the guest on this thread: bionic's linker64 at `linker`, loading the program at `exe` (one whose only
/// purpose is to have libc loaded). This thread is the guest's main thread after. 0, or -1 with the reason logged.
#[no_mangle]
pub extern "C" fn gex_embed_boot(linker: *const core::ffi::c_char, exe: *const core::ffi::c_char) -> i32 {
    // SAFETY: the caller's strings.
    let (l, e) = unsafe { (std::ffi::CStr::from_ptr(linker), std::ffi::CStr::from_ptr(exe)) };
    match elf::boot(&l.to_string_lossy(), &e.to_string_lossy()) {
        Ok(()) => 0,
        Err(m) => {
            host::log(&format!("gex-embed: boot: {m}\n"));
            -1
        }
    }
}

fn loader() -> &'static elf::Loader {
    elf::LOADER.get().expect("gex_embed_boot first")
}

/// The guest's `dlopen(path, flags)`, in the default namespace.
#[no_mangle]
pub extern "C" fn gex_embed_dlopen(path: *const core::ffi::c_char, flags: i32) -> u64 {
    call::call_x(loader().dlopen, &[path as usize as u64, flags as u64, 0])
}

/// The guest's `dlsym(handle, name)`.
#[no_mangle]
pub extern "C" fn gex_embed_dlsym(handle: u64, name: *const core::ffi::c_char) -> u64 {
    call::call_x(loader().dlsym, &[handle, name as usize as u64, 0])
}

/// The default namespace's library search path (`android_update_LD_LIBRARY_PATH`): colon-separated directories
/// searched, after the system's, for libraries `gex_embed_dlopen` loads by name -- an app's own (`libc++_shared.so`
/// beside its libraries).
#[no_mangle]
pub extern "C" fn gex_embed_set_library_path(paths: *const core::ffi::c_char) {
    call::call_x(loader().update_library_path, &[paths as usize as u64]);
}

/// The guest's `dlerror()`: a string in guest (so the host's) memory, or null.
#[no_mangle]
pub extern "C" fn gex_embed_dlerror() -> u32 {
    call::call_x(loader().dlerror, &[]) as u32
}

/// One function of a library the host provides (`gex_embed.h`'s `gex_provided`).
#[repr(C)]
pub struct Provided {
    pub name: *const core::ffi::c_char,
    pub f: HostFn,
    pub user: u32,
}

/// When a library named `soname` is mapped executable, bind its functions to the host's: each of `fns` by name, and,
/// unless `missing` is null, every other function it exports to `missing` (which gets "soname!symbol" as its user
/// value; `gex_embed_missing` logs it once and returns 0). Before the library is loaded.
#[no_mangle]
pub extern "C" fn gex_embed_provide(soname: *const core::ffi::c_char, fns: *const Provided, n: u32, missing: Option<HostFn>) {
    // SAFETY: the caller's strings and table.
    let soname = unsafe { std::ffi::CStr::from_ptr(soname) }.to_string_lossy().into_owned();
    let table = if n == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(fns, n as usize) } };
    let fns = table
        .iter()
        .map(|p| (unsafe { std::ffi::CStr::from_ptr(p.name) }.to_string_lossy().into_owned(), p.f, p.user))
        .collect();
    overlay::provide(&soname, fns, missing);
}

/// A `missing` for `gex_embed_provide`: says which function, once, and returns 0.
#[no_mangle]
pub extern "C" fn gex_embed_missing(ctx: u32, label: u32) {
    overlay::missing(ctx, label)
}

/// Say where every guest thread is: its last system call (x8, x0-x2) and its backtrace. For a hang: a thread blocked
/// in a system call shows what it waits on.
#[no_mangle]
pub extern "C" fn gex_embed_dump_threads() {
    let all = thread::ALL.lock().unwrap().clone();
    for (tid, c, lo, hi) in all {
        let (x8, x0, x1, x2) = (cpu::x(c, 8), cpu::x(c, 0), cpu::x(c, 1), cpu::x(c, 2));
        let bt = diag::backtrace_in(c, lo, hi);
        host::log(&format!("gex-embed: thread {tid}: x8={x8} x0={x0:#x} x1={x1:#x} x2={x2:#x}\n{bt}"));
    }
}

/// Say every guest system call on stderr.
#[no_mangle]
pub extern "C" fn gex_embed_set_trace(on: i32) {
    sys::TRACE.store(on != 0, Ordering::Relaxed);
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
