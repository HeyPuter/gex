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
//! Runs static programs, including real ones: Lua 5.4 passes all sixteen of
//! its own conformance files here byte-for-byte against QEMU. The suite
//! checks twelve programs on every invocation -- the whole conformance suite,
//! the VFS, signals, NEON and C++ exceptions.
//!
//! Not yet: threads (one Worker each, and the memory would have to be
//! declared shared), `fork`, and runtime translation, so a module must be
//! built with `--exhaustive`.
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

/// The host's clock, in the shape `gex-kernel` wants.
fn browser_clock() -> (u64, u32) {
    let ns = unsafe { now() } as u64;
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

/// Standard input, as bytes the embedder supplied before the guest started.
///
/// A page has nothing to block on -- no terminal, no pipe -- so stdin is
/// whatever was handed over up front, and reads past the end return zero the
/// way a closed pipe does. That is enough for the programs that read stdin at
/// all: `sqlite3` taking a script, a filter reading a document.
#[derive(Debug)]
struct StdinBytes(Vec<u8>);

impl FileHandle for StdinBytes {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EBADF)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::Fifo,
            size: self.0.len() as u64,
            mode: 0o400,
            ino: 2,
            mtime: 0,
        })
    }
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
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 1,
            mtime: 0,
        })
    }

    /// A console is not seekable, so writes must not advance an offset.
    fn is_stream(&self) -> bool {
        true
    }

    /// Not a terminal. A page's console looks like one, but saying so makes
    /// musl line-buffer stdout, and then a program's bytes interleave
    /// differently from the same program's under a pipe -- which is what
    /// every comparison in the suite runs the reference against.
    fn is_tty(&self) -> bool {
        false
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
    /// The root filesystem, kept so files can be added before the guest runs.
    root: Arc<gex_kernel::vfs::TmpFs>,
    /// The snapshot `/proc` reads from, refreshed before each syscall that
    /// could look at it.
    proc_state: Arc<gex_kernel::KernelProcSource>,
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
pub extern "C" fn gex_load(ptr: u32, len: u32, argv: u32, argv_len: u32) -> u64 {
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
    let root = Arc::new(gex_kernel::vfs::TmpFs::new());
    kernel.vfs.mount("/", root.clone());
    kernel
        .vfs
        .mount("/dev", Arc::new(gex_kernel::vfs::DevFs::new()));
    // `/tmp` is part of the root filesystem rather than a mount of its own,
    // so a page can bundle a file at any path at all. A mount there meant a
    // bundled `/tmp/x` went into the root and every lookup of it went to the
    // mount, which had never heard of it.
    root.add_dir("/tmp");
    // `/proc` matters more here than it looks: a program that cannot open
    // `/proc/self/maps` gets a null `FILE*` back, and the next `fgets` calls
    // through it. That surfaces as a jump to address zero -- a dispatch miss
    // at pc=0 -- with nothing to say the filesystem was the cause.
    root.add_dir("/proc");
    let proc_state: Arc<gex_kernel::KernelProcSource> = Arc::new(gex_kernel::KernelProcSource(
        std::sync::Mutex::new(Default::default()),
    ));
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    kernel.clock = browser_clock;
    kernel.uid = unsafe { IDENTITY.0 };
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    let _ = kernel.fds.install(
        0,
        OpenFile::new(
            Arc::new(StdinBytes(std::mem::take(&mut *STDIN.0.borrow_mut()))),
            "/dev/stdin".to_string(),
            0,
            true,
            false,
        ),
        false,
    );
    for fd in [1, 2] {
        let f = OpenFile::new(
            Arc::new(Console { fd }),
            if fd == 1 {
                "/dev/stdout"
            } else {
                "/dev/stderr"
            }
            .to_string(),
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
        kernel
            .mem
            .insert(seg.vaddr, seg.vaddr + seg.mem_size, p, "[image]");
    }

    // argv arrives as a NUL-separated blob in the shim's own region.
    let args: Vec<String> = if argv_len == 0 {
        vec!["prog".to_string()]
    } else {
        let blob = unsafe {
            std::slice::from_raw_parts(
                std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(argv as usize)),
                argv_len as usize,
            )
        };
        blob.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    let env = ["PATH=/usr/bin:/bin".to_string(), "HOME=/root".to_string()];
    // `/proc/self/{cmdline,exe}` read these. A relative argv[0] is resolved
    // against the root, which is where a bundled program lives.
    kernel.exe_path = if args[0].starts_with('/') {
        args[0].clone()
    } else {
        format!("/{}", args[0])
    };
    kernel.argv = args.clone();
    kernel.envp = env.to_vec();
    let layout = gex_ld::build_initial_stack(
        &image,
        GUEST_LIMIT,
        &args,
        &env,
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: unsafe { IDENTITY.0 },
            gid: unsafe { IDENTITY.1 },
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
            root,
            proc_state,
            thread: ThreadState::new(gex_abi::ctx_addr(0), 1),
            exit: None,
            resume: None,
        });
    });
    entry
}

/// Put a file into the guest's filesystem, before it runs.
///
/// A browser has no filesystem to pass through to, so the page bundles what
/// the program needs: a script to interpret, data to read. Both pointers are
/// into the shim's own region, written by JS after [`gex_alloc`].
#[no_mangle]
pub extern "C" fn gex_add_file(path: u32, path_len: u32, data: u32, data_len: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    let bytes = shim_slice(data, data_len).to_vec();
    with_host(|h| {
        // Directories along the way, so a nested path resolves.
        let mut at = String::new();
        for part in name
            .trim_start_matches('/')
            .split('/')
            .rev()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            at.push('/');
            at.push_str(part);
            h.root.add_dir(&at);
        }
        h.root.add_file(name, bytes);
    });
}

/// The identity the guest is given, which the embedder may override before
/// loading. A page has no user, so 1000 is as good an answer as any; Node
/// passes its own so a comparison against QEMU on the same machine agrees.
static mut IDENTITY: (u32, u32) = (1000, 1000);

/// Bytes JS wrote into the shim's own region, which is above the guest's
/// limit and so is reached through a pointer rather than through
/// [`guest_bytes`].
fn shim_slice(p: u32, n: u32) -> &'static [u8] {
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(p as usize)),
            n as usize,
        )
    }
}

/// Standard input, staged until the guest is loaded.
static STDIN: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Hand the guest its standard input, from `len` bytes at `ptr` in the shim's
/// own region. Call before `gex_load`.
#[no_mangle]
pub extern "C" fn gex_set_stdin(ptr: u32, len: u32) {
    *STDIN.0.borrow_mut() = shim_slice(ptr, len).to_vec();
}

#[no_mangle]
pub extern "C" fn gex_set_identity(uid: i32, gid: i32) {
    unsafe { IDENTITY = (uid as u32, gid as u32) };
}

/// Whether to narrate every syscall to fd 2, the browser's `--strace`.
///
/// `gex-kernel`'s own tracing writes with `eprintln!`, which on
/// `wasm32-unknown-unknown` goes nowhere, so the shim prints its own -- in the
/// same shape as the native host's, so the two traces diff directly. That
/// diff is the only way to see what a guest is doing differently under V8,
/// which is where every browser-only bug so far has been found.
static mut TRACE: bool = false;

#[no_mangle]
pub extern "C" fn gex_set_trace(on: i32) {
    unsafe { TRACE = on != 0 };
}

fn tracing() -> bool {
    unsafe { TRACE }
}

/// Write a line to fd 2 without going through the guest's descriptor table.
fn trace_line(s: &str) {
    unsafe { write(2, s.as_ptr() as u32, s.len() as u32) };
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
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let base = gex_abi::ctx_addr(0);
        let g = |i: usize| gm.read_u64(base + ctx::x(i as u8) as u64).unwrap_or(0);
        let n = g(8);
        trace_line(&format!(
            "[gex] pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}) ...\n",
            gex_kernel::syscalls::syscall_name(n),
            g(0),
            g(1),
            g(2)
        ));
        // For a gather write, what the kernel sees at each iovec. A wrong
        // byte here and a right one in the guest means the two disagree about
        // the memory they share, which is the failure mode this host has.
        if n == 66 {
            for i in 0..g(2).min(8) {
                let e = g(1) + i * 16;
                let (base, len) = (
                    gm.read_u64(e).unwrap_or(0),
                    gm.read_u64(e + 8).unwrap_or(0).min(32),
                );
                let bytes = gm.read(base, len as usize).unwrap_or(&[]);
                trace_line(&format!("[gex]   iov[{i}] {base:#x}+{len} {bytes:02x?}\n"));
            }
        }
    }
    let stop = with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        h.kernel.enter_thread(&h.thread);
        // Rebuilding the snapshot costs more than most syscalls do, and only
        // a handful can observe it, so the rest skip it.
        let nr = gm
            .read_u64(h.thread.ctx_addr + ctx::x(8) as u64)
            .unwrap_or(!0);
        if !gex_kernel::skips_proc_snapshot(nr) {
            let st = Arc::clone(&h.proc_state);
            h.kernel.refresh_proc(&st.0);
        }
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
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let x0 = gm
            .read_u64(gex_abi::ctx_addr(0) + ctx::x(0) as u64)
            .unwrap_or(0);
        trace_line(&format!("[gex]   = {}\n", x0 as i64));
    }
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
pub extern "C" fn gex_illegal(_ctx: i32, pc: i64, raw: i32) {
    trace_line(&format!(
        "gex: illegal instruction {:#010x} at pc={:#x}\n",
        raw as u32, pc
    ));
    with_host(|h| h.exit = Some(128 + 4));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_breakpoint(_ctx: i32, pc: i64, _raw: i32) {
    trace_line(&format!("gex: breakpoint at pc={pc:#x}\n"));
    with_host(|h| h.exit = Some(128 + 5));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_sysreg_read(_ctx: i32, _pc: i64, reg: i32) -> i64 {
    match reg {
        // CNTVCT_EL0, at the frequency the lifter tells the guest CNTFRQ is,
        // so the guest's own arithmetic on it comes out exact.
        -3 => unsafe { now() },
        // MIDR_EL1: a plausible Cortex-A72, the same one the native host
        // claims to be.
        -7 => 0x410f_d083,
        _ => 0,
    }
}
#[no_mangle]
pub extern "C" fn gex_sysreg_write(_ctx: i32, _pc: i64, _reg: i32, _val: i64) {}

/// Cache maintenance.
///
/// Only `dc zva` does anything, and it has to: the lifter reports a 64-byte
/// block through `DCZID_EL0`, and musl's `memset` zeroes whole blocks with it
/// rather than storing. Leaving it a no-op means large `memset`s silently
/// don't zero -- which showed up as Lua printing stale bytes where it had
/// formatted a number, three layers down inside `vsnprintf`.
#[no_mangle]
pub extern "C" fn gex_cache_op(_ctx: i32, _pc: i64, op: i32, addr: i64) {
    if op != gex_abi::cache_op::DC_ZVA {
        // Clean, invalidate and barrier are all no-ops: the memory is
        // coherent and nothing here translates at runtime yet.
        return;
    }
    let block = gex_abi::DCZID_BLOCK_BYTES as usize;
    let start = (addr as u64 & !(gex_abi::DCZID_BLOCK_BYTES - 1)) as usize;
    let mem = guest_bytes();
    if let Some(dst) = mem.get_mut(start..start + block) {
        dst.fill(0);
    }
}
#[no_mangle]
pub extern "C" fn gex_safepoint(_ctx: i32, _pc: i64) {}

/// A dispatch miss, with no runtime translator behind it. A module built with
/// `--exhaustive` should never reach one.
#[no_mangle]
pub extern "C" fn gex_dispatch_miss(_ctx: i32, pc: i64) -> i32 {
    // Silence here is the worst outcome: the guest simply stops, and the
    // exit status looks like a segfault it never took.
    trace_line(&format!("gex: no translation for pc={pc:#x}\n"));
    with_host(|h| h.exit = Some(128 + 11));
    trap();
}
