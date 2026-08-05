//! The browser host: the same kernel, in wasm.
//!
//! PLAN §9's claim is that a host is thin enough for both of them to share one
//! `gex-kernel`. The native host is a Rust program embedding wasmtime; this
//! one *is* wasm, and the guest module runs beside it.
//!
//! # Sharing the address space
//!
//! Two wasm modules cannot share a memory unless both import it, so this shim
//! imports the guest's -- the same 4 GiB the guest module gets -- and puts its
//! own heap and stack inside it. That is not a workaround, it is what makes a
//! syscall cheap: the kernel reads the guest's buffers where they lie, with no
//! copy across a host boundary and no JS in the path.
//!
//! It works because the guest's layout leaves a hole. Everything the guest
//! uses ends at `STACK_TOP`, two gigabytes in; the memory is just under four.
//! The linker is told to put this module's data and stack above that (see
//! `build.sh`), so the two never meet.
//!
//! # Status
//!
//! Runs static programs. The suite checks three of them -- including the NEON
//! and fixed-point ones -- against QEMU on every invocation.
//!
//! Not yet: threads (one Worker each, and the memory would have to be
//! declared shared), `fork`, runtime translation, and a clock, so a program
//! that calls `clock_gettime` gets whatever `gex-kernel`'s wasm32 build does
//! rather than the time.
//!
//! # What JS still does
//!
//! Creates the memory, instantiates both modules against it, wires this
//! module's exports to the guest module's `gex.*` imports, and supplies the
//! two things wasm cannot do for itself: a console and a clock.

// Only meaningful as wasm: everything below assumes it shares a linear memory
// with the guest module beside it. On any other target the crate is empty, so
// `cargo build --workspace` on the development host still works.
#![cfg(target_arch = "wasm32")]

use gex_abi::ctx;
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::file::OpenFile;
use gex_kernel::vfs::{FileHandle, Metadata};
use gex_kernel::{GuestMem, Kernel, SyscallOutcome, ThreadState};
use std::cell::RefCell;
use std::sync::Arc;

// The two things wasm cannot do for itself. Everything else about a syscall
// happens in this module.
#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Write `len` bytes at `ptr` -- an address in the shared memory -- to the
    /// host's console for `fd`.
    fn write(fd: i32, ptr: u32, len: u32);
    /// Nanoseconds since an arbitrary epoch.
    fn now() -> i64;
}

/// stdout and stderr, as the host's console.
///
/// `gex-kernel`'s own `StdStream` writes through `std::io`, which on
/// `wasm32-unknown-unknown` goes nowhere. The `FileHandle` trait is the seam:
/// the kernel does not care what is behind a descriptor.
#[derive(Debug)]
struct Console {
    fd: i32,
}

impl FileHandle for Console {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EBADF)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        // The buffer is already in the shared memory the host can see, so its
        // address is all that has to cross.
        unsafe { write(self.fd, buf.as_ptr() as u32, buf.len() as u32) };
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::regular(0))
    }

    fn is_tty(&self) -> bool {
        true
    }
}

// The shim's own heap.
//
// Rust's default allocator grows the memory when it runs out, and this memory
// cannot grow: it is the guest's, fixed at its maximum so the guest's stack
// lands where the ABI says. So the heap is a fixed region instead -- everything
// between the end of this module's data and the end of the address space,
// which is the rest of the two gigabytes the guest never touches.
#[global_allocator]
static ALLOC: Heap = Heap;

struct Heap;

/// The heap's state. Wrapped so it can be a `static`: wasm without threads is
/// single-threaded, and this module is only ever entered from JS one call at a
/// time, so there is nothing to race with.
struct SingleThreaded<T>(RefCell<T>);
// SAFETY: see above.
unsafe impl<T> Sync for SingleThreaded<T> {}

static INNER: SingleThreaded<Option<linked_list_allocator::Heap>> =
    SingleThreaded(const { RefCell::new(None) });

extern "C" {
    static __heap_base: u8;
}

unsafe impl std::alloc::GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: single-threaded; see above.
        let mut h = INNER.0.borrow_mut();
        let heap = h.get_or_insert_with(|| {
            let base = (&raw const __heap_base) as usize;
            let size = gex_abi::GUEST_MEM_BYTES as usize - base;
            // SAFETY: the linker put this module's data below `__heap_base`,
            // and the guest maps nothing above `STACK_TOP`, so the region is
            // ours alone.
            unsafe { linked_list_allocator::Heap::new(base as *mut u8, size) }
        });
        heap.allocate_first_fit(layout)
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        if let Some(heap) = INNER.0.borrow_mut().as_mut() {
            if let Some(p) = std::ptr::NonNull::new(ptr) {
                unsafe { heap.deallocate(p, layout) };
            }
        }
    }
}

struct Host {
    kernel: Kernel,
    thread: ThreadState,
    exit: Option<i32>,
    resume: Option<u64>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// How much of the address space the guest may use.
///
/// Not all of it: a Rust slice may not be longer than `isize::MAX`, which on
/// wasm32 is just under 2 GiB, and `GuestMem` is built on a slice. Claiming
/// the whole 4 GiB produces an invalid slice, and the consequences are not a
/// clean failure -- the compiler deleted stores through it, and reads above
/// 2 GiB returned `EFAULT` from one call site and succeeded from another.
///
/// So the guest gets everything below this line and the shim takes what is
/// above, which it reaches through ordinary Rust pointers rather than through
/// the slice. The guest's own layout fits comfortably: its stack is the
/// highest thing in it, and the shim places it at this address rather than at
/// `STACK_TOP`.
const GUEST_LIMIT: u64 = 0x7fff_0000;

/// The guest's memory, as a slice.
///
/// This module's memory *is* the guest's, so the address space below
/// [`GUEST_LIMIT`] is addressable directly.
#[allow(clippy::mut_from_ref)]
#[inline(never)]
fn guest_bytes() -> &'static mut [u8] {
    // The base goes through `black_box` so the optimiser cannot see that it is
    // the constant zero. Without that, LLVM concludes the pointer cannot alias
    // anything and deletes every store through it: the first version of this
    // booted the guest with a zero stack pointer, because the writes setting
    // it up had all been optimised away. Reads survived, which made it look
    // like the two modules had different memories.
    //
    // SAFETY: in wasm, address zero is an ordinary location in linear memory
    // and the guest's address space starts there. The linker places this
    // module's own data and stack above `STACK_TOP`, and the guest never maps
    // there, so the two do not overlap; the memory is imported and never
    // grows, so base and length are fixed for the life of the instance.
    // The *pointer* goes through `black_box`, not just the address it is made
    // from. Hiding only the address was not enough: LLVM still tracked the
    // pointer's provenance and miscompiled accesses through it, so the same
    // read succeeded from a small function and returned EFAULT from inside the
    // kernel's call path. Hiding the pointer leaves the optimiser nothing to
    // reason about.
    let p: *mut u8 = std::hint::black_box(std::ptr::with_exposed_provenance_mut::<u8>(0));
    // The length is hidden as well. With it a constant, LLVM pairs it against
    // what it believes the pointer's allocation to be -- nothing -- and folds
    // slice bounds checks to "always out of range": the same read succeeded
    // from a small function and returned EFAULT from inside the kernel.
    let len = std::hint::black_box(GUEST_LIMIT as usize);
    unsafe { std::slice::from_raw_parts_mut(p, len) }
}

fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    HOST.with(|h| f(h.borrow_mut().as_mut().expect("gex: not initialised")))
}

/// Reserve `len` bytes of this module's own heap and return a guest address
/// JS can write to. Used to hand the ELF across.
#[no_mangle]
pub extern "C" fn gex_alloc(len: usize) -> u32 {
    let mut v = Vec::<u8>::with_capacity(len);
    v.resize(len, 0);
    let p = v.as_mut_ptr() as u32;
    std::mem::forget(v);
    p
}

/// Load a static ELF that JS has written at `ptr`, place it and its stack, and
/// return the entry point. Zero means the image could not be loaded.
#[no_mangle]
pub extern "C" fn gex_load(ptr: u32, len: u32) -> u64 {
    // The ELF sits in the shim's own heap, above the guest's limit, so it is
    // reached through its own pointer rather than through the guest slice.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(ptr as usize)),
            len as usize,
        )
    };
    let Ok(image) = gex_ld::elf::load(bytes, gex_abi::PIE_BASE) else {
        return 0;
    };
    if image.interp.is_some() {
        // A dynamic program needs its interpreter read through a filesystem,
        // and this host has none yet.
        return 0;
    }

    let mut kernel = Kernel::new(gex_abi::ctx_addr(0), image.brk, String::new());
    kernel.vfs.mount("/", Arc::new(gex_kernel::vfs::TmpFs::new()));
    kernel.vfs.mount("/dev", Arc::new(gex_kernel::vfs::DevFs::new()));
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    for fd in [1, 2] {
        let f = OpenFile::new(
            Arc::new(Console { fd }),
            if fd == 1 { "/dev/stdout" } else { "/dev/stderr" }.to_string(),
            0,
            false,
            true,
        );
        let _ = kernel.fds.install(fd, f, false);
    }

    let data = guest_bytes();
    for seg in &image.segments {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        data[a..b].fill(0);
        data[a..a + seg.bytes.len()].copy_from_slice(&seg.bytes);
        let mut p = 0;
        if seg.readable {
            p |= gex_kernel::mem::prot::READ;
        }
        if seg.writable {
            p |= gex_kernel::mem::prot::WRITE;
        }
        if seg.executable {
            p |= gex_kernel::mem::prot::EXEC;
        }
        kernel.mem.insert(seg.vaddr, seg.vaddr + seg.mem_size, p, "[image]");
    }

    let layout = gex_ld::build_initial_stack(
        &image,
        GUEST_LIMIT,
        &["prog".to_string()],
        &["PATH=/usr/bin:/bin".to_string(), "HOME=/root".to_string()],
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: 1000,
            gid: 1000,
            interp_base: 0,
        },
    );
    let start = layout.low as usize;
    data[start..start + layout.bytes.len()].copy_from_slice(&layout.bytes);
    kernel.mem.insert(
        GUEST_LIMIT - gex_abi::STACK_SIZE,
        GUEST_LIMIT,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    let c = gex_abi::ctx_addr(0) as usize;
    write_u64(data, c + ctx::SP, layout.sp);
    write_u64(data, c + ctx::PC, image.entry);
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }

    let entry = image.entry;
    HOST.with(|h| {
        *h.borrow_mut() = Some(Host {
            kernel,
            thread: ThreadState::new(gex_abi::ctx_addr(0), 1),
            exit: None,
            resume: None,
        });
    });
    entry
}

/// `svc #0`.
///
/// Returns normally when the guest should carry straight on, which is almost
/// every syscall. Anything else -- a redirect, an exit -- traps out to JS
/// instead, the same way the native host unwinds out of wasm: there is no
/// other way for a callee to say "abandon this stack", and it keeps JS out of
/// the common path entirely.
#[no_mangle]
pub extern "C" fn gex_syscall(_ctx: i32, pc: i64) {
    let stop = with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        h.kernel.enter_thread(&h.thread);
        let outcome = h.kernel.syscall(&mut gm, pc as u64);
        h.kernel.leave_thread(&mut h.thread);
        match outcome {
            SyscallOutcome::Continue => 0,
            SyscallOutcome::Resume(target) => {
                h.resume = Some(target);
                1
            }
            SyscallOutcome::Exit(code) => {
                h.exit = Some(code);
                2
            }
            SyscallOutcome::Killed { signal, .. } => {
                h.exit = Some(128 + signal as i32);
                2
            }
            // One thread, no scheduler: a call that wants to block, fork or
            // exec cannot be served. Saying so beats hanging.
            _ => {
                h.exit = Some(128 + 31);
                2
            }
        }
    });
    if stop != 0 {
        trap();
    }
}

/// Abandon the guest's wasm stack and return control to JS.
fn trap() -> ! {
    core::arch::wasm32::unreachable()
}

#[no_mangle]
pub extern "C" fn gex_resume_pc() -> i64 {
    with_host(|h| h.resume.take().unwrap_or(0) as i64)
}

#[no_mangle]
pub extern "C" fn gex_exit_code() -> i32 {
    with_host(|h| h.exit.unwrap_or(0))
}

fn write_u64(data: &mut [u8], off: usize, v: u64) {
    data[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fused multiply-add, which wasm cannot express. Rust's `mul_add` is the
/// hardware instruction where there is one and libm's exact software fallback
/// where there is not, which is what the guest needs either way.
#[no_mangle]
pub extern "C" fn gex_fma(_ctx: i32, a: i64, b: i64, c: i64, double: i32) -> i64 {
    if double != 0 {
        f64::from_bits(a as u64)
            .mul_add(f64::from_bits(b as u64), f64::from_bits(c as u64))
            .to_bits() as i64
    } else {
        f32::from_bits(a as u32)
            .mul_add(f32::from_bits(b as u32), f32::from_bits(c as u32))
            .to_bits() as i64
    }
}

#[no_mangle]
pub extern "C" fn gex_illegal(_ctx: i32, _pc: i64, _raw: i32) {
    with_host(|h| h.exit = Some(128 + 4));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_breakpoint(_ctx: i32, _pc: i64, _raw: i32) {
    with_host(|h| h.exit = Some(128 + 5));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_sysreg_read(_ctx: i32, _pc: i64, _reg: i32) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn gex_sysreg_write(_ctx: i32, _pc: i64, _reg: i32, _val: i64) {}
#[no_mangle]
pub extern "C" fn gex_cache_op(_ctx: i32, _pc: i64, _op: i32, _addr: i64) {}
#[no_mangle]
pub extern "C" fn gex_safepoint(_ctx: i32, _pc: i64) {}

/// A dispatch miss, with no runtime translator behind it. A module built with
/// `--exhaustive` should never reach one.
#[no_mangle]
pub extern "C" fn gex_dispatch_miss(_ctx: i32, _pc: i64) -> i32 {
    with_host(|h| h.exit = Some(128 + 11));
    trap();
}
