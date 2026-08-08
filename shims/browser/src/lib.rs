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
//! Guest threads run too, cooperatively on the one wasm stack rather than on
//! Workers: pthreads, mutexes, condition variables and thread-local storage
//! all work, and six threads sharing one SQLite connection give byte-identical
//! output. See [`schedule`].
//!
//! Not yet: real parallelism (a Worker each, with the memory declared shared),
//! `fork`, and runtime translation, so a module must be built with
//! `--exhaustive`.
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
use gex_kernel::net;
use gex_kernel::{GuestMem, Kernel, SyscallOutcome, ThreadState};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
pub mod gl;
pub mod virgl;

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

    // The network. A page cannot open a socket, so every one of these is a
    // question for JS, which has a Wisp connection to a proxy. All are
    // non-blocking: the waiting is the kernel's job, because the kernel is
    // what can park a guest thread, and JS blocking here would take the
    // page's only wasm stack with it.
    //
    // A stream is named by a small integer JS hands out. Zero is never a
    // stream, so it can mean failure.
    fn net_open() -> i32;
    fn net_connect(id: i32, host: u32, host_len: u32, port: i32);
    /// 0 connecting, 1 open, 2 closed, 3 refused.
    fn net_status(id: i32) -> i32;
    /// Bytes taken, or -1.
    fn net_send(id: i32, ptr: u32, len: u32) -> i32;
    /// Bytes delivered, 0 at end of stream, -1 when nothing has arrived yet.
    fn net_recv(id: i32, ptr: u32, len: u32) -> i32;
    /// Whether `net_recv` would answer without waiting.
    fn net_avail(id: i32) -> i32;
    fn net_close(id: i32);
}

/// Whether the page gave the shim a network at all.
static mut HAVE_NET: bool = false;

/// Say that the page has a Wisp connection behind the `net_*` imports.
///
/// Off by default: a page with no proxy configured should have its guest see
/// a machine with no network, which fails cleanly, rather than one whose
/// every connection hangs.
#[no_mangle]
pub extern "C" fn gex_set_network(on: i32) {
    unsafe { HAVE_NET = on != 0 };
    with_host(|h| {
        h.kernel.net = unsafe { HAVE_NET }.then(|| Arc::new(WispNet) as Arc<dyn net::NetStack>);
    });
}

/// One Wisp stream, as the kernel's [`net::Transport`].
#[derive(Debug)]
struct WispStream {
    id: Cell<i32>,
}

// Single-threaded by construction, like everything else here.
unsafe impl Send for WispStream {}
unsafe impl Sync for WispStream {}

impl net::Transport for WispStream {
    fn connect(&self, host: &str, port: u16) -> Result<(), Errno> {
        let id = unsafe { net_open() };
        if id <= 0 {
            return Err(Errno::ENETUNREACH);
        }
        self.id.set(id);
        unsafe { net_connect(id, host.as_ptr() as u32, host.len() as u32, port as i32) };
        Ok(())
    }

    fn status(&self) -> net::Status {
        match self.id.get() {
            0 => net::Status::Closed,
            id => match unsafe { net_status(id) } {
                0 => net::Status::Connecting,
                1 => net::Status::Open,
                3 => net::Status::Refused,
                _ => net::Status::Closed,
            },
        }
    }

    fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        let id = self.id.get();
        if id == 0 {
            return Err(Errno::ENOTCONN);
        }
        match unsafe { net_send(id, data.as_ptr() as u32, data.len() as u32) } {
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::EPIPE),
        }
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let id = self.id.get();
        if id == 0 {
            return Ok(0);
        }
        match unsafe { net_recv(id, buf.as_mut_ptr() as u32, buf.len() as u32) } {
            -1 => Err(Errno::EAGAIN),
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::ECONNRESET),
        }
    }

    fn has_data(&self) -> bool {
        let id = self.id.get();
        id != 0 && unsafe { net_avail(id) } != 0
    }

    fn close(&self) {
        let id = self.id.replace(0);
        if id != 0 {
            unsafe { net_close(id) };
        }
    }
}

/// Where the browser's connections come from.
#[derive(Debug)]
struct WispNet;

unsafe impl Send for WispNet {}
unsafe impl Sync for WispNet {}

impl net::NetStack for WispNet {
    fn tcp(&self) -> Result<Arc<dyn net::Transport>, Errno> {
        Ok(Arc::new(WispStream { id: Cell::new(0) }))
    }
}

/// Back-edges a guest may take between safepoint calls, matching the native
/// host's interval.
const SAFEPOINT_INTERVAL: u32 = 1024;

/// The host's clock, in the shape `gex-kernel` wants.
fn browser_clock() -> (u64, u32) {
    let ns = unsafe { now() } as u64;
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

/// The guest's standard input, as a queue the page appends to.
///
/// A page that hands over a fixed buffer up front can run a filter. A page
/// with a terminal in it cannot: the bytes do not exist until someone types
/// them, and a shell asks for them long before that. So this is a queue --
/// `gex_push_stdin` adds to it, a read takes from it, and a read that finds
/// it empty parks the calling thread until it is not (see `Waiting::Read`).
///
/// `eof` is the difference between "nothing yet" and "nothing ever". Without
/// it a shell reading its script from stdin waits forever at the end instead
/// of exiting.
#[derive(Debug, Default)]
struct StdinQueue {
    bytes: RefCell<VecDeque<u8>>,
    eof: Cell<bool>,
}

impl StdinQueue {
    fn push(&self, data: &[u8]) {
        self.bytes.borrow_mut().extend(data.iter().copied());
    }
}

// The browser shim is single-threaded by construction; `SingleThreaded` is
// the wrapper the rest of this file uses to say so.
unsafe impl Send for StdinQueue {}
unsafe impl Sync for StdinQueue {}

impl FileHandle for StdinQueue {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        let mut q = self.bytes.borrow_mut();
        let n = buf.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EBADF)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::CharDevice,
            size: 0,
            mode: 0o600,
            ino: 2,
            mtime: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// A terminal, when the page says there is one. musl line-buffers stdout
    /// on a tty, and a shell will not print a prompt without one.
    fn is_tty(&self) -> bool {
        unsafe { STDIN_TTY }
    }

    /// Nothing typed yet is a wait, unless the page has said there will be no
    /// more.
    fn poll_readable(&self) -> bool {
        self.eof.get() || !self.bytes.borrow().is_empty()
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
// lands where the ABI says. So the heap is a fixed region instead --
// everything between the end of this module's data and the end of the address
// space, which is the rest of the two gigabytes the guest never touches.
//
// It has to be a *good* allocator, which only became obvious once runtime
// translation started running inside the shim. A first-fit linked list walks
// its free list on every allocation and the translator allocates constantly:
// lifting tcc's largest source took 15.2 seconds that way, against 0.3 with a
// bump allocator that never freed. Talc bins its free lists, so allocation
// does not degrade as the heap fragments, and it still frees.
#[global_allocator]
static ALLOC: talc::cell::TalcSyncCell<ShimRegion, talc::wasm::WasmBinning> =
    talc::cell::TalcSyncCell::new_wasm(ShimRegion);

extern "C" {
    static __heap_base: u8;
}

/// Hands the allocator everything above this module's data, once.
///
/// A linker symbol's address is not a constant, so the region cannot be named
/// in the allocator's initialiser; it is claimed the first time anything is
/// allocated instead.
#[derive(Debug)]
struct ShimRegion;

// SAFETY: `acquire` claims a region of this module's own linear memory and
// touches no other allocator.
unsafe impl talc::source::Source for ShimRegion {
    fn acquire<B: talc::base::binning::Binning>(
        talc: &mut talc::base::Talc<Self, B>,
        _layout: std::alloc::Layout,
    ) -> Result<(), ()> {
        // There is exactly one region; a second call means it is exhausted,
        // and saying so beats claiming it again and looping forever.
        static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if CLAIMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return Err(());
        }
        let base = (&raw const __heap_base) as *mut u8;
        // Short of the very top, so a span that would end exactly at the end
        // of memory cannot be rounded past it.
        let size = gex_abi::GUEST_MEM_BYTES as usize - base as usize - 65536;
        // SAFETY: the linker put this module's data below `__heap_base`, and
        // the guest maps nothing above `STACK_TOP`, so the region is ours.
        unsafe { talc.claim(base, size).ok_or(())? };
        Ok(())
    }
}

/// State wrapped so it can be a `static`: wasm without threads is
/// single-threaded, and this module is only ever entered from JS one call at a
/// time, so there is nothing to race with.
struct SingleThreaded<T>(RefCell<T>);
// SAFETY: see above.
unsafe impl<T> Sync for SingleThreaded<T> {}

/// Why a guest thread is not running.
enum Waiting {
    /// Ready to be picked up.
    Runnable,
    /// Parked on a futex, until it is woken or the deadline passes.
    Futex {
        parked: gex_kernel::futex::Parked,
        deadline: Option<u64>,
    },
    /// Sleeping until this many nanoseconds on the host clock.
    Sleep(u64),
    /// Waiting on a stream with nothing in it: a terminal nobody has typed at
    /// yet. Ready as soon as the handle says a read would return.
    Read(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting for room in a stream: a pipe nobody has drained yet.
    Write(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting in `wait4` for a child to finish.
    ///
    /// This used to be impossible: the parent was frozen until the child
    /// exited, so a `wait4` that ran at all was asking about a child whose
    /// status was already recorded. Now that processes take turns, a parent
    /// can reach `wait4` while its child is still running, and has to park
    /// like anything else.
    Child,
    /// Waiting on the network -- a connection coming up, or a `poll` with
    /// nothing ready. Whether it is ready is a question for the operation
    /// itself, which the scheduler holds in `blocked_on`, so this only
    /// carries the deadline.
    Net(Option<u64>),
    /// Finished; the slot stays so a `join` can still find the tid.
    Exited,
}

/// A parent process, frozen while its child runs.
///
/// There is one address space here and no way to make a second: a guest
/// address is a byte offset into the page's wasm memory, and a page gets one
/// memory. So `fork` is done by checkpoint. The parent's memory, kernel and
/// threads are copied aside, the child runs in place, and when the child
/// exits the parent is put back exactly as it was, with the child's pid
/// already in `x0` where the kernel wrote it before any of this started.
///
/// What this buys is the whole of a Unix userspace in a page: a shell that
/// starts real processes, a pipeline, `make`, `apt`.
///
/// Processes take turns rather than nesting. A saved process is not "the
/// parent, waiting" -- it is any process that is not the one on the wasm
/// stack, and [`switch_process`] puts one back when the running one has
/// nothing left that can run. That is what makes a background process work:
/// `Xvfb &` starts a daemon that never exits, and while the parent was only
/// ever put back *on the child's exit*, the shell after an `&` never ran
/// again. An X server and its client are two processes that talk to each
/// other and neither finishes first, which is most of what a graphical
/// program is.
///
/// It is a swap and not real concurrency: one runs at a time, and switching
/// copies both memory images. Only the mapped regions move, and only when
/// nothing in the current process can run, so a `fork` immediately followed
/// by a `wait` -- the shape every shell has -- still costs one copy and not
/// one per timeslice.
struct Suspended {
    pid: i32,
    /// Guest memory as it was, region by region. Includes the register
    /// contexts and the dispatch directory, which live below the image and
    /// are not in the memory map.
    snapshot: Vec<(u64, Vec<u8>)>,
    kernel: Kernel,
    threads: Vec<Thread>,
    current: usize,
    /// Where the parent resumes: the instruction after its `clone`.
    resume_pc: u64,
    resume_ctx: u64,
    /// The dispatch directory as the parent left it. A child that execs a
    /// different program translates different code into the same slots, so
    /// without this the parent comes back dispatching into its child's.
    page_table: gex_xlate::PageTable,
    translated: std::collections::HashSet<u64>,
}

/// One guest thread.
///
/// Every register it has is in its context in guest memory, so switching to it
/// is a call to the guest module's entry with a different context pointer.
/// There is no stack to save: the shim leaves the guest's wasm stack behind
/// entirely, which is the same thing the native host does for a signal.
struct Thread {
    kstate: ThreadState,
    /// Where to resume, which for a thread that blocked is the instruction
    /// after its `svc`.
    pc: u64,
    waiting: Waiting,
    /// The syscall to finish before it runs again.
    blocked_on: Option<gex_kernel::BlockOp>,
}

/// Runtime translation state.
///
/// The shim can lift and encode a module -- `gex-xlate` is ordinary Rust and
/// compiles to wasm like everything else -- but it cannot *compile* one:
/// nothing in wasm can hand bytes to the engine. So a miss produces the
/// module here, leaves it for JS to instantiate, and comes back.
struct Jit {
    /// Which guest pages own which slots in the dispatch table.
    page_table: gex_xlate::PageTable,
    /// Whether the module said where it had got to. Without that this would
    /// start handing out slots from zero and quietly overwrite the image
    /// module's own blocks in the table, which is a very confusing way to
    /// find out the module is too old.
    described: bool,
    /// Addresses whose slot now holds real code.
    translated: std::collections::HashSet<u64>,
    /// A module JS has yet to instantiate.
    pending: Option<Vec<u8>>,
    /// The slots it defines, once JS asks.
    pending_dirs: Vec<(u64, u32)>,
    /// How many slots the table must hold for it.
    pending_slots: u64,
    blocks: u64,
    compiles: u64,
    /// Nanoseconds spent lifting and encoding, which is the shim's own share
    /// of a miss -- the engine's compile of what comes out is JS's.
    nanos: u64,
}

struct Host {
    kernel: Kernel,
    /// The virtio-gpu renderer, when the page brought one. Held here rather
    /// than in the kernel because the scheduler is what pumps it, and the
    /// scheduler is the one place every thread passes through on its way to
    /// waiting for a reply.
    virgl: Option<crate::virgl::VirglDevice>,
    /// The root filesystem, kept so files can be added before the guest runs.
    root: Arc<gex_kernel::vfs::TmpFs>,
    /// The snapshot `/proc` reads from, refreshed before each syscall that
    /// could look at it.
    proc_state: Arc<gex_kernel::KernelProcSource>,
    /// Every guest thread, the running one included.
    threads: Vec<Thread>,
    /// Index into `threads` of the one on the wasm stack right now.
    current: usize,
    exit: Option<i32>,
    resume: Option<u64>,
    /// Set alongside `resume` when the scheduler switched threads.
    resume_ctx: Option<u64>,
    /// Every process except the one on the wasm stack.
    suspended: Vec<Suspended>,
    /// Why the scheduler gave the stack back to JS, when it was not an exit.
    /// 1: every thread is waiting for input the page must supply. 2: every
    /// thread is waiting on the network, and the page's event loop is what
    /// makes the bytes arrive. Either way the page is expected to call
    /// `gex_wake`; the difference is what it should do first.
    paused: u8,
    jit: Jit,
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

/// Make a panic say what it was.
///
/// On wasm32 a Rust panic is a bare `unreachable`, and what reaches the page
/// is `RuntimeError: unreachable` with no file, no line and no message. That
/// is the whole of the diagnosis for any bug in the shim, which is not
/// enough to act on -- it took a bisect to find out even which subsystem had
/// failed. The hook writes the message and location to stderr through the
/// import the guest's own output uses, so a panic reads like a panic.
///
/// Installed on the first `with_host`, which every entry point goes through.
/// Write to the guest's stderr, which in a page is the only channel the shim
/// has to say anything at all.
///
/// # Safety
/// The one thread a page has; `write` only reads the bytes.
pub(crate) unsafe fn write_stderr(text: &str) {
    write(2, text.as_ptr() as u32, text.len() as u32);
}

fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let text = format!("gex: panic in the shim: {info}\n");
            // SAFETY: the one thread a page has, and `write` only reads.
            unsafe { write(2, text.as_ptr() as u32, text.len() as u32) };
        }));
    });
}

fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    install_panic_hook();
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

/// Give back an allocation from [`gex_alloc`].
///
/// The setup calls allocate once and never care; interactive input is the
/// case that does, because it allocates per keystroke and a page stays open.
///
/// # Safety
/// `ptr` and `len` must be exactly what a previous `gex_alloc` returned.
#[no_mangle]
pub unsafe extern "C" fn gex_free(ptr: u32, len: usize) {
    if ptr != 0 && len != 0 {
        drop(Vec::from_raw_parts(ptr as *mut u8, len, len));
    }
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
    // The bundle is a filesystem before anything else happens, because a
    // dynamic executable's interpreter is read out of it.
    let root = Arc::new(gex_kernel::vfs::TmpFs::new());
    install_bundle(&root);
    let program = match gex_ld::load_program(bytes, gex_abi::PIE_BASE, INTERP_BASE, &|path| {
        use gex_kernel::vfs::FileSystem;
        let f = root.open(path.trim_start_matches('/'), 0, 0).ok()?;
        let len = f.metadata().ok()?.size as usize;
        let mut buf = vec![0u8; len];
        f.read_at(0, &mut buf).ok()?;
        Some(buf)
    }) {
        Ok(p) => p,
        Err(_) => return 0,
    };

    let mut kernel = Kernel::new(gex_abi::ctx_addr(0), program.brk(), String::new());
    kernel.vfs.mount("/", root.clone());
    // The graphics device, when the page said it has a canvas to draw on.
    // Absent otherwise, so a guest sees a machine with no GL rather than one
    // whose drawing vanishes.
    let mut devfs = gex_kernel::vfs::DevFs::new();
    if crate::gl::have_gl() {
        devfs.add("gex-gl", crate::gl::GlDevice::new());
    }
    kernel.vfs.mount("/dev", Arc::new(devfs));
    // The virtio-gpu renderer, bound where Mesa's vtest winsys looks. Only
    // when the page brought one: a guest that connects to a socket nobody
    // serves is worse off than one that finds no socket, because Mesa's
    // winsys does not fall back -- it writes its handshake into the dead
    // descriptor and blocks reading a reply that cannot come.
    let virgl = crate::virgl::have_virgl().then(|| {
        let device = crate::virgl::VirglDevice::new();
        kernel.socket_names.bind_host(
            crate::virgl::DEFAULT_SOCKET,
            Arc::new(device.clone()) as Arc<dyn gex_kernel::unix::HostListener>,
        );
        device
    });
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
        std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
    ));
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    kernel.clock = browser_clock;
    kernel.net = unsafe { HAVE_NET }.then(|| Arc::new(WispNet) as Arc<dyn net::NetStack>);
    {
        let cwd = std::mem::take(&mut *CWD.0.borrow_mut());
        if !cwd.is_empty() {
            let _ = kernel.vfs.set_cwd(&cwd);
        }
    }
    kernel.uid = unsafe { IDENTITY.0 };
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    let _ = kernel.fds.install(
        0,
        OpenFile::new(
            Arc::clone(&*STDIN) as Arc<dyn FileHandle>,
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
    for seg in program.segments() {
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
        &program.exe,
        GUEST_LIMIT,
        &args,
        &env,
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: unsafe { IDENTITY.0 },
            gid: unsafe { IDENTITY.1 },
            // Where the interpreter ended up, which is how it finds itself.
            interp_base: program.interp_base(),
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
    write_u64(data, c + ctx::PC, program.entry());
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }

    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let entry = program.entry();
    HOST.with(|h| {
        *h.borrow_mut() = Some(Host {
            kernel,
            virgl,
            root,
            proc_state,
            threads: vec![Thread {
                kstate: ThreadState::new(gex_abi::ctx_addr(0), 1),
                pc: entry as u64,
                waiting: Waiting::Runnable,
                blocked_on: None,
            }],
            current: 0,
            exit: None,
            resume: None,
            resume_ctx: None,
            suspended: Vec::new(),
            paused: 0,
            jit: Jit {
                described: meta.next_slot != 0,
                page_table: gex_xlate::PageTable::from_entries(&meta.dir_entries, meta.next_slot),
                translated: Default::default(),
                pending: None,
                pending_dirs: Vec::new(),
                pending_slots: 0,
                blocks: 0,
                compiles: 0,
                nanos: 0,
            },
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
    // Staged rather than installed. The bundle has to exist before the
    // program is loaded, not after: a dynamic executable names its
    // interpreter and the loader has to read it out of the same filesystem
    // the guest will see.
    FILES.0.borrow_mut().push((name.to_string(), bytes));
}

/// Files bundled by the page, in the order they arrived.
type Bundle = Vec<(String, Vec<u8>)>;
static FILES: SingleThreaded<Bundle> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Put the staged bundle into a filesystem, creating the directories on the
/// way to each file.
fn install_bundle(root: &gex_kernel::vfs::TmpFs) {
    for (name, bytes) in std::mem::take(&mut *FILES.0.borrow_mut()) {
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
            root.add_dir(&at);
        }
        root.add_file(&name, bytes);
    }
}

/// Where the dynamic linker is placed, clear of the executable so neither can
/// overlap the other's mappings. The same address `gexc` uses, so a program
/// looks identical under both hosts.
const INTERP_BASE: u64 = 0x2800_0000;

/// A file read back out of the guest's filesystem, kept alive for JS.
static EXTRACTED: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Read a file out of the guest's filesystem, and return its length.
///
/// The other half of bundling: a page that runs a compiler wants the object
/// file back. Zero means there is nothing there -- a length of zero and a
/// missing file are the same answer, which is all a caller can act on anyway.
#[no_mangle]
pub extern "C" fn gex_get_file(path: u32, path_len: u32) -> u32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return 0;
    };
    let bytes = with_host(|h| {
        let f = h.kernel.vfs.open(name, 0, 0).ok()?;
        let len = f.metadata().ok()?.size as usize;
        let mut buf = vec![0u8; len];
        let n = f.read_at(0, &mut buf).ok()? as usize;
        buf.truncate(n);
        Some(buf)
    })
    .unwrap_or_default();
    let len = bytes.len() as u32;
    *EXTRACTED.0.borrow_mut() = bytes;
    len
}

/// Where the last [`gex_get_file`] left its contents.
#[no_mangle]
pub extern "C" fn gex_file_data() -> u32 {
    EXTRACTED.0.borrow().as_ptr() as u32
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

/// The module's dispatch metadata, staged until the guest is loaded.
///
/// It rides in the module's own `gex.meta` custom section, which JS finds and
/// hands over: everything runtime translation needs to carry on allocating
/// table slots where the ahead-of-time build left off.
static METADATA: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

#[no_mangle]
pub extern "C" fn gex_set_metadata(ptr: u32, len: u32) {
    *METADATA.0.borrow_mut() = shim_slice(ptr, len).to_vec();
}

/// The directory the guest starts in, staged until it is loaded.
static CWD: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

/// Set the guest's working directory. A program given a relative path -- a
/// benchmark reading its data files, a compiler given a source name -- has
/// nowhere to resolve it from otherwise.
#[no_mangle]
pub extern "C" fn gex_set_cwd(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        *CWD.0.borrow_mut() = s.to_string();
    }
}

/// Standard input, staged until the guest is loaded.
/// The one stdin queue, reachable both from the descriptor the kernel holds
/// and from the exports the page calls.
static STDIN: StdinHandle = StdinHandle(std::sync::LazyLock::new(|| Arc::new(StdinQueue::default())));

struct StdinHandle(std::sync::LazyLock<Arc<StdinQueue>>);
unsafe impl Sync for StdinHandle {}
impl std::ops::Deref for StdinHandle {
    type Target = Arc<StdinQueue>;
    fn deref(&self) -> &Arc<StdinQueue> {
        &self.0
    }
}

/// Whether stdin should report itself a terminal.
static mut STDIN_TTY: bool = false;

/// Append what the user typed. The page calls this from its keyboard handler,
/// between runs of the guest.
#[no_mangle]
pub extern "C" fn gex_push_stdin(ptr: u32, len: u32) {
    STDIN.push(shim_slice(ptr, len));
}

/// There will be no more input. A read that finds the queue empty returns
/// zero from here on rather than waiting.
#[no_mangle]
pub extern "C" fn gex_stdin_eof() {
    STDIN.eof.set(true);
}

/// Whether the guest stopped because it is waiting for input rather than
/// because it finished.
///
/// A page with a terminal in it needs to tell the two apart: one means "show
/// a cursor and wait", the other means "the program is over". Both arrive in
/// JS the same way -- the guest gives up the wasm stack -- so the difference
/// has to be asked for.
#[no_mangle]
pub extern "C" fn gex_paused_for_input() -> i32 {
    with_host(|h| (h.paused & 1 != 0) as i32)
}

/// Whether the guest stopped because it is waiting on the network. The page
/// has nothing to supply -- it only has to let its event loop run, so the
/// data it is waiting for can arrive -- and then call [`gex_wake`].
#[no_mangle]
pub extern "C" fn gex_paused_for_net() -> i32 {
    with_host(|h| (h.paused & 2 != 0) as i32)
}

/// Carry on after input has arrived: pick a thread and say where to re-enter.
///
/// Returns 1 when there is somewhere to go, 2 when the guest is finished.
#[no_mangle]
pub extern "C" fn gex_wake() -> i32 {
    with_host(|h| h.paused = 0);
    schedule()
}

/// Hand the guest its standard input, from `len` bytes at `ptr` in the shim's
/// own region. Call before `gex_load`.
#[no_mangle]
pub extern "C" fn gex_set_stdin(ptr: u32, len: u32) {
    STDIN.push(shim_slice(ptr, len));
    // A page that hands over the whole of stdin at once is not going to type
    // any more, and a guest that reads to the end must see the end.
    STDIN.eof.set(true);
}

/// The guest's current `c_lflag`, so the page can follow it between cooked
/// and raw mode.
///
/// Bit 1 is ICANON and bit 3 is ECHO. While they are set the page does the
/// line editing -- collect a line, echo it, deliver it on Enter. When the
/// guest clears them it has taken over, and the page must pass keystrokes
/// through untouched and echo nothing.
#[no_mangle]
pub extern "C" fn gex_tty_lflag() -> u32 {
    with_host(|h| u32::from_le_bytes(h.kernel.termios[12..16].try_into().unwrap()))
}

/// Tell the guest how big the terminal is.
#[no_mangle]
pub extern "C" fn gex_set_winsize(rows: u32, cols: u32) {
    with_host(|h| h.kernel.winsize = (rows as u16, cols as u16));
}

/// Say that stdin is a terminal, which is what makes a shell print a prompt.
#[no_mangle]
pub extern "C" fn gex_set_tty(on: i32) {
    unsafe { STDIN_TTY = on != 0 };
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
        let base = with_host(|h| h.threads[h.current].kstate.ctx_addr);
        let g = |i: usize| gm.read_u64(base + ctx::x(i as u8) as u64).unwrap_or(0);
        let n = g(8);
        let who = with_host(|h| h.current);
        trace_line(&format!(
            "[gex] t{who} pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}) ...\n",
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
        let kstate = h.threads[h.current].kstate.clone();
        h.kernel.enter_thread(&kstate);
        // Rebuilding the snapshot costs more than most syscalls do, and only
        // a handful can observe it, so the rest skip it.
        let nr = gm
            .read_u64(kstate.ctx_addr + ctx::x(8) as u64)
            .unwrap_or(!0);
        if !gex_kernel::skips_proc_snapshot(nr) {
            let st = Arc::clone(&h.proc_state);
            h.kernel.refresh_proc(&st.0);
        }
        let outcome = h.kernel.syscall(&mut gm, pc as u64);
        h.kernel.leave_thread(&mut h.threads[h.current].kstate);
        match outcome {
            SyscallOutcome::Continue => 0,
            SyscallOutcome::Resume(target) => {
                h.resume = Some(target);
                1
            }
            SyscallOutcome::Exit(code) => exit_process(h, code),
            SyscallOutcome::Killed { signal, .. } => {
                h.exit = Some(128 + signal as i32);
                2
            }
            SyscallOutcome::SpawnThread {
                ctx_addr,
                tid,
                clear_child_tid,
            } => {
                // The kernel has already written the child's context; it
                // starts at the pc that context carries.
                let start = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
                h.threads.push(Thread {
                    kstate: ThreadState {
                        ctx_addr,
                        tid,
                        clear_child_tid,
                    },
                    pc: start,
                    waiting: Waiting::Runnable,
                    blocked_on: None,
                });
                // The parent carries straight on, as it does on Linux. The
                // child waits for the first time the parent gives up the
                // stack, which is what a safepoint or a blocking call is.
                0
            }
            SyscallOutcome::ThreadExit => {
                h.threads[h.current].waiting = Waiting::Exited;
                3
            }
            SyscallOutcome::Blocking(op) => {
                let waiting = match &op {
                    gex_kernel::BlockOp::FutexWait {
                        table,
                        uaddr,
                        deadline,
                        ..
                    } => Waiting::Futex {
                        parked: table.park(*uaddr),
                        deadline: deadline.map(|d| now_ns() + d.as_nanos() as u64),
                    },
                    gex_kernel::BlockOp::Sleep(d) => Waiting::Sleep(now_ns() + d.as_nanos() as u64),
                    gex_kernel::BlockOp::ReadStream { handle, .. } => {
                        Waiting::Read(Arc::clone(handle))
                    }
                    gex_kernel::BlockOp::WriteStream { handle, .. } => {
                        Waiting::Write(Arc::clone(handle))
                    }
                    // Both of these wait on something outside the guest, so
                    // both give the stack back and are re-checked by the
                    // scheduler on its way round.
                    gex_kernel::BlockOp::Connect { .. } => Waiting::Net(None),
                    gex_kernel::BlockOp::Poll { deadline, .. } => {
                        Waiting::Net(deadline.map(|d| now_ns() + d.as_nanos() as u64))
                    }
                    // `WNOHANG` answers straight away; anything else waits
                    // for the child, which may not have started yet.
                    gex_kernel::BlockOp::WaitChild { nohang, .. } => {
                        if *nohang {
                            Waiting::Runnable
                        } else {
                            Waiting::Child
                        }
                    }
                    // A listener with nothing waiting. The same shape as a
                    // poll: the scheduler asks again each turn.
                    gex_kernel::BlockOp::AcceptWait { .. } => Waiting::Runnable,
                    // `sigsuspend` waits for a signal and nothing else. The
                    // scheduler is where a signal is delivered, so coming
                    // back round each turn is the whole implementation: if
                    // one arrived, delivery happens before this thread runs
                    // its next instruction, and if not the turn is given up
                    // again.
                    gex_kernel::BlockOp::SigSuspend { .. } => Waiting::Runnable,
                };
                let t = &mut h.threads[h.current];
                t.pc = pc as u64 + 4;
                t.waiting = waiting;
                t.blocked_on = Some(op);
                3
            }
            SyscallOutcome::SpawnProcess {
                pid,
                child_stack,
                tls,
            } => {
                do_fork(h, pid, pc as u64, child_stack, tls);
                // The child is on this stack already, at the instruction
                // after the `clone`. Nothing to switch to.
                0
            }
            SyscallOutcome::Exec { path, argv, envp } => {
                let entry = do_exec(h, &path, &argv, &envp);
                if entry == 0 {
                    // The kernel checked the path before saying `Exec`, so a
                    // failure here is the loader's: not an ELF, or one this
                    // cannot place. `ENOEXEC` is what the guest expects.
                    let mut gm = GuestMem::new(guest_bytes());
                    let ctx_addr = h.threads[h.current].kstate.ctx_addr;
                    let _ = gm.write_u64(
                        ctx_addr + ctx::x(0) as u64,
                        Errno::ENOEXEC.as_return(),
                    );
                    0
                } else {
                    h.resume = Some(entry);
                    h.resume_ctx = Some(h.threads[h.current].kstate.ctx_addr);
                    1
                }
            }
            _ => {
                h.exit = Some(128 + 31);
                2
            }
        }
    });
    // 3 means the running thread gave up the wasm stack: pick another.
    let stop = if stop == 3 { schedule() } else { stop };
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let (i, ctx_addr) = with_host(|h| (h.current, h.threads[h.current].kstate.ctx_addr));
        let x0 = gm.read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        trace_line(&format!("[gex]   t{i} = {}\n", x0 as i64));
    }
    if stop != 0 {
        trap();
    }
}


/// Everything below the guest's image: the register contexts, the exclusive
/// monitor scratch, and the dispatch directory. None of it is in the memory
/// map, and all of it is per-process.
const LOW_END: u64 = gex_abi::DISPATCH_DIR + gex_abi::DISPATCH_MAX_PAGES * 4;

/// Copy out every part of guest memory a process owns.
fn snapshot_memory(h: &Host) -> Vec<(u64, Vec<u8>)> {
    let data = guest_bytes();
    let mut out = vec![(0u64, data[..LOW_END as usize].to_vec())];
    for v in h.kernel.mem.vmas() {
        let (a, b) = (v.start as usize, (v.end as usize).min(data.len()));
        if a < b {
            out.push((v.start, data[a..b].to_vec()));
        }
    }
    out
}

/// Blank everything the running process has mapped.
///
/// Called on the way out of a process, and it is not tidiness. There is one
/// address space, so a region this process mapped and the next one does not
/// would otherwise still be sitting there when the next one looks -- and what
/// it finds is a *relocated* copy of a library at exactly the address its own
/// ld.so is about to use. That is how `exec` broke: the second program's
/// .got.plt kept its link-time addresses because ld.so found a world that
/// already looked done, and the first call through the PLT branched to an
/// unbiased PLT0 and died in unmapped memory.
///
/// Regions the incoming process does have are filled in immediately after by
/// [`restore_memory`], so this only ever leaves behind what nothing owns.
fn clear_mapped(h: &Host) {
    let data = guest_bytes();
    for v in h.kernel.mem.vmas() {
        let (a, b) = (v.start as usize, (v.end as usize).min(data.len()));
        if a < b {
            data[a..b].fill(0);
        }
    }
}

fn restore_memory(snapshot: &[(u64, Vec<u8>)]) {
    let data = guest_bytes();
    for (at, bytes) in snapshot {
        let a = *at as usize;
        let b = (a + bytes.len()).min(data.len());
        if a < b {
            data[a..b].copy_from_slice(&bytes[..b - a]);
        }
    }
}

/// `fork`: freeze the parent and carry on as the child.
///
/// The child needs no new stack and no scheduler switch -- it is this very
/// thread, in this very address space, with `x0` changed from the child's pid
/// to zero. Everything that makes it a *different* process is in what got put
/// aside.
fn do_fork(h: &mut Host, pid: i32, pc: u64, child_stack: u64, tls: Option<u64>) {
    let ctx_addr = h.threads[h.current].kstate.ctx_addr;
    // The parent carries on after its `clone`, whenever it next gets a turn.
    h.threads[h.current].pc = pc + 4;
    h.threads[h.current].waiting = Waiting::Runnable;
    h.threads[h.current].blocked_on = None;
    let snapshot = snapshot_memory(h);
    let mut child_kernel = h.kernel.fork_kernel();
    child_kernel.pid = pid;
    child_kernel.tid = pid;
    let parent_kernel = std::mem::replace(&mut h.kernel, child_kernel);
    h.suspended.push(Suspended {
        pid,
        snapshot,
        kernel: parent_kernel,
        threads: std::mem::take(&mut h.threads),
        current: h.current,
        resume_pc: pc + 4,
        resume_ctx: ctx_addr,
        page_table: h.jit.page_table.clone(),
        translated: h.jit.translated.clone(),
    });
    // The child is this thread, renumbered, and alone: `fork` does not carry
    // the parent's other threads across.
    h.threads = vec![Thread {
        kstate: ThreadState::new(ctx_addr, pid),
        pc: pc + 4,
        waiting: Waiting::Runnable,
        blocked_on: None,
    }];
    h.current = 0;
    let mut gm = GuestMem::new(guest_bytes());
    // What `fork` returns in the child.
    let _ = gm.write_u64(ctx_addr + ctx::x(0) as u64, 0);
    if child_stack != 0 {
        let _ = gm.write_u64(ctx_addr + ctx::SP as u64, child_stack);
    }
    if let Some(tls) = tls {
        let _ = gm.write_u64(ctx_addr + ctx::TPIDR as u64, tls);
    }
    // A monitor is per-thread and the child's starts clear.
    let _ = gm.write_u64(ctx_addr + ctx::MON_SIZE as u64, 0);
}

/// Put a saved process back on the wasm stack, and set `h` to be it.
///
/// The one that was running is *not* preserved by this; the two callers
/// differ in exactly that, and each does its own half:
/// [`switch_process`] saves it first, and [`exit_process`] throws it away.
fn install_process(h: &mut Host, p: Suspended) {
    restore_memory(&p.snapshot);
    h.kernel = p.kernel;
    h.threads = p.threads;
    h.current = p.current;
    // The restored process's pages keep the slots they had -- its directory,
    // just restored, names them -- but allocation carries on from wherever
    // the other one got to. Rewinding the mark would hand this process's next
    // translation slots the other is still using, and it would dispatch one
    // page's code from another page's directory entry.
    let mark = h.jit.page_table.slots();
    h.jit.page_table = p.page_table;
    h.jit.page_table.advance_to(mark);
    h.jit.translated = p.translated;
    h.resume = Some(p.resume_pc);
    h.resume_ctx = Some(p.resume_ctx);
}

/// Give the stack to the saved process at `idx`, saving the running one.
///
/// The caller has already decided that nothing in the running process can
/// make progress, which is the whole of the scheduling policy: a process runs
/// until it blocks. There is no timeslice, because a switch copies two memory
/// images and preempting a busy process would pay that for nothing.
///
/// The two images are exchanged rather than saved and then loaded, so there
/// is never a moment when the host holds neither a kernel nor a process.
fn switch_process(h: &mut Host, idx: usize) {
    let target = h.suspended.remove(idx);
    // Everything read out of the running process, before any of it moves.
    let pid = h.kernel.pid;
    let t = &h.threads[h.current];
    let (resume_pc, resume_ctx) = (t.pc, t.kstate.ctx_addr);
    let snapshot = snapshot_memory(h);
    let mark = h.jit.page_table.slots();

    let mine = Suspended {
        pid,
        snapshot,
        resume_pc,
        resume_ctx,
        kernel: std::mem::replace(&mut h.kernel, target.kernel),
        threads: std::mem::replace(&mut h.threads, target.threads),
        current: std::mem::replace(&mut h.current, target.current),
        page_table: std::mem::replace(&mut h.jit.page_table, target.page_table),
        translated: std::mem::replace(&mut h.jit.translated, target.translated),
    };
    // Cleared through the *outgoing* process's map, which is why this reads
    // from `mine` rather than from `h` -- by now `h.kernel` is the target's.
    {
        let data = guest_bytes();
        for v in mine.kernel.mem.vmas() {
            let (a, b) = (v.start as usize, (v.end as usize).min(data.len()));
            if a < b {
                data[a..b].fill(0);
            }
        }
    }
    restore_memory(&target.snapshot);
    h.jit.page_table.advance_to(mark);
    h.resume = Some(target.resume_pc);
    h.resume_ctx = Some(target.resume_ctx);
    h.suspended.push(mine);
}

/// This process has exited. Hand the stack to another, or end the run.
///
/// Returns 1 when another process took over, 2 when this was the last one.
fn exit_process(h: &mut Host, code: i32) -> i32 {
    // Published before anything else, because the table is shared between
    // every kernel and this is the record a parent's `wait4` will find --
    // and the parent may be parked on exactly that.
    let pid = h.kernel.pid;
    h.kernel.procs.exit(pid, code);
    // The parent first when there is one, so `fork` then `wait` costs one
    // switch and lands where it did before any of this existed. Otherwise
    // whatever is left, in the order it was saved.
    let ppid = h.kernel.ppid;
    let idx = h
        .suspended
        .iter()
        .position(|p| p.pid == ppid)
        .or(h.suspended.len().checked_sub(1));
    let Some(idx) = idx else {
        h.exit = Some(code);
        return 2;
    };
    let target = h.suspended.remove(idx);
    clear_mapped(h);
    install_process(h, target);
    // 3: the scheduler picks a thread out of the process just installed, and
    // finishes whatever syscall it was parked in.
    3
}

/// `execve`: replace this process's image with another program's.
///
/// Everything about the process that is not the image survives -- pid,
/// parent, descriptors that are not close-on-exec, the working directory --
/// which is what makes the shell's fork-then-exec work at all.
///
/// Returns the entry point, or zero if the program could not be loaded, in
/// which case the caller has already been given an errno.
fn do_exec(h: &mut Host, path: &str, argv: &[String], envp: &[String]) -> u64 {
    let Some(bytes) = h.kernel.read_file(path) else {
        return 0;
    };
    let read = |p: &str| h.kernel.read_file(p);
    let program = match gex_ld::load_program(&bytes, gex_abi::PIE_BASE, INTERP_BASE, &read) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    h.kernel.close_on_exec();
    // Handlers are addresses in the image being replaced; see Signals::exec.
    h.kernel.signals.exec();
    // The old image goes, and not only from the memory map.
    //
    // On a real kernel `execve` gets a new address space and everything the
    // old program had is gone. Here there is one address space, so anything
    // not deliberately cleared is still sitting in it -- and the next program
    // maps its libraries at the same addresses, because a fresh map allocates
    // from the same place. What it finds there is the previous program's
    // *relocated* copy of the same library.
    //
    // That is not harmless. `sleep` after `dash` came up with its .got.plt
    // still holding link-time addresses: ld.so had decided there was nothing
    // to do, and the first call through the PLT branched to 0x15c0 -- the
    // unbiased PLT0 -- and died in unmapped memory.
    let old: Vec<(u64, u64)> = h.kernel.mem.vmas().iter().map(|v| (v.start, v.end)).collect();
    h.kernel.mem = gex_kernel::mem::MemoryMap::new(program.brk());
    {
        let data = guest_bytes();
        for (start, end) in old {
            let (a, b) = (start as usize, (end as usize).min(data.len()));
            if a < b {
                data[a..b].fill(0);
            }
        }
    }
    h.kernel.exe_path = path.to_string();
    h.kernel.argv = argv.to_vec();
    h.kernel.envp = envp.to_vec();
    h.kernel.live_threads = 1;
    h.kernel.next_ctx_slot = 1;
    // Signal handlers do not survive an exec; the mask does.
    h.kernel.signals = gex_kernel::signal::Signals::new();

    let data = guest_bytes();
    // The new image goes where its own headers say, which for a static
    // executable is the same place the old one was.
    for seg in program.segments() {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        if b > data.len() {
            return 0;
        }
        data[a..b].fill(0);
        data[a..a + seg.bytes.len()].copy_from_slice(&seg.bytes);
        let mut prot = 0;
        if seg.readable {
            prot |= gex_kernel::mem::prot::READ;
        }
        if seg.writable {
            prot |= gex_kernel::mem::prot::WRITE;
        }
        if seg.executable {
            prot |= gex_kernel::mem::prot::EXEC;
        }
        h.kernel
            .mem
            .insert(seg.vaddr, seg.vaddr + seg.mem_size, prot, "[image]");
    }
    let layout = gex_ld::build_initial_stack(
        &program.exe,
        GUEST_LIMIT,
        argv,
        envp,
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: unsafe { IDENTITY.0 },
            gid: unsafe { IDENTITY.1 },
            interp_base: program.interp_base(),
        },
    );
    let start = layout.low as usize;
    data[start..start + layout.bytes.len()].copy_from_slice(&layout.bytes);
    h.kernel.mem.insert(
        GUEST_LIMIT - gex_abi::STACK_SIZE,
        GUEST_LIMIT,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    // The new program's pages have nothing translated for them, and anything
    // the old one left in the directory is code at the same addresses doing
    // something else entirely. Clearing it sends every branch back through
    // the miss stub, which is where a fresh translation comes from.
    data[gex_abi::DISPATCH_DIR as usize..LOW_END as usize].fill(0);
    // Slots already handed out are never reused -- a page that comes back
    // through a `fork` checkpoint still points at its own -- so allocation
    // carries on from the high-water mark rather than restarting.
    h.jit.page_table = gex_xlate::PageTable::from_entries(&[], h.jit.page_table.slots());
    h.jit.translated.clear();

    let ctx_addr = h.threads[h.current].kstate.ctx_addr;
    let c = ctx_addr as usize;
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }
    write_u64(data, c + ctx::SP, layout.sp);
    write_u64(data, c + ctx::PC, program.entry());
    write_u64(data, c + ctx::MON_SIZE, 0);
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    h.threads = vec![Thread {
        kstate: ThreadState::new(ctx_addr, h.kernel.pid),
        pc: program.entry(),
        waiting: Waiting::Runnable,
        blocked_on: None,
    }];
    h.current = 0;
    program.entry()
}

/// Nanoseconds on the host clock.
fn now_ns() -> u64 {
    unsafe { now() as u64 }
}

/// Choose the next guest thread to run, and finish whatever syscall it was in
/// the middle of.

///
/// The browser host has one wasm stack and no way to block on it, so guest
/// threads are cooperative: a thread keeps the stack until it blocks, exits,
/// or spawns another. Every register it owns is in its context in guest
/// memory, so a switch is nothing more than re-entering the guest module with
/// a different context pointer -- which is why this works at all without
/// Workers, shared memory or a second stack.
///
/// What the scheduler decided.
enum Pick {
    /// Run this thread of the process already loaded, having timed out or not.
    Thread(usize, bool),
    /// A different process is now on the stack, with its resume point set.
    Switched,
    /// Nothing anywhere can run yet.
    Idle,
    /// The last process has finished.
    Over,
}

/// Whether a thread could make progress if it were given the stack.
///
/// Asked of threads whose process is not loaded as well as of the running
/// one, which is what lets the scheduler cross a process boundary. Everything
/// it consults -- handles, sockets, futexes, the clock -- is reference-counted
/// and lives outside guest memory, so the answer does not depend on whose
/// image is currently in it.
fn thread_ready(t: &Thread, now: u64) -> bool {
    match &t.waiting {
        Waiting::Runnable => true,
        Waiting::Futex { parked, deadline } => {
            parked.woken() || deadline.is_some_and(|d| now >= d)
        }
        Waiting::Sleep(d) => now >= *d,
        Waiting::Read(handle) => handle.poll_readable(),
        Waiting::Write(handle) => handle.poll_writable(),
        Waiting::Net(deadline) => {
            deadline.is_some_and(|d| now >= d)
                || match &t.blocked_on {
                    Some(gex_kernel::BlockOp::Connect { socket }) => {
                        socket.status() != gex_kernel::net::Status::Connecting
                    }
                    Some(gex_kernel::BlockOp::Poll { fds, .. }) => {
                        gex_kernel::syscalls::poll_ready(fds)
                    }
                    _ => true,
                }
        }
        // Ready when one of the children this call can collect has a status
        // waiting, and also when there are none at all -- that is `ECHILD`,
        // which is an answer and not a reason to sleep forever.
        Waiting::Child => match &t.blocked_on {
            Some(gex_kernel::BlockOp::WaitChild { children, want, .. }) => {
                let mut any = false;
                for c in children.iter().filter(|c| *want <= 0 || c.pid == *want) {
                    any = true;
                    if c.status.lock().map(|st| st.is_some()).unwrap_or(true) {
                        return true;
                    }
                }
                !any
            }
            _ => true,
        },
        Waiting::Exited => false,
    }
}

/// Returns 2 when the run is over and 1 to re-enter the guest.
fn schedule() -> i32 {
    loop {
        // Before anything is chosen. A vtest client writes a request and then
        // blocks reading the reply, and both are syscalls, so the renderer
        // has to run in between -- here, which is the one place every thread
        // passes through on its way to waiting. Nothing is ready to run until
        // it has.
        with_host(|h| h.virgl.clone()).map(|v| v.pump());
        let pick = with_host(|h| {
            // Nothing left that could ever run again ends this process. It
            // does not necessarily end the run: another may be waiting for
            // exactly this, and `exit_process` hands it the stack.
            if h.threads
                .iter()
                .all(|t| matches!(t.waiting, Waiting::Exited))
            {
                let code = h.exit.take().unwrap_or(0);
                if exit_process(h, code) == 1 {
                    return Pick::Switched;
                }
                h.exit.get_or_insert(code);
                return Pick::Over;
            }
            let now = now_ns();
            // Round-robin from the one after the current, so a thread that
            // yields does not immediately get the stack back.
            let n = h.threads.len();
            for step in 1..=n {
                let i = (h.current + step) % n;
                if thread_ready(&h.threads[i], now) {
                    let timed_out = match &h.threads[i].waiting {
                        Waiting::Futex { parked, .. } => !parked.woken(),
                        _ => false,
                    };
                    return Pick::Thread(i, timed_out);
                }
            }
            // Nothing here can run. Another process might -- a shell that
            // started a daemon with `&` is sitting Runnable in `suspended`
            // while the daemon waits for a connection, and before processes
            // took turns that shell simply never ran again.
            //
            // The check costs nothing: a saved thread's readiness is asked of
            // the same handles and futexes, which are reference-counted and
            // alive whether or not their process's memory is loaded.
            if let Some(idx) = h
                .suspended
                .iter()
                .position(|p| p.threads.iter().any(|t| thread_ready(t, now)))
            {
                switch_process(h, idx);
                // `switch_process` set the resume point; the caller re-enters
                // the guest with it rather than picking a thread here.
                return Pick::Switched;
            }
            // Everything is asleep or parked with a deadline. There is no
            // other thread to advance the clock, so spinning is the only way
            // to reach the earliest one.
            Pick::Idle
        });
        // A switch installs a process but chooses nothing. Going round again
        // is what picks a thread out of it -- and, crucially, what finishes
        // the syscall that thread parked in. Resuming its saved pc directly
        // skipped that, so it carried on with no return value in x0 and
        // branched through whatever was there: `nothing to translate at
        // pc=0xffff0`, on every pipeline.
        if matches!(pick, Pick::Switched) {
            continue;
        }
        if matches!(pick, Pick::Over) {
            return 2;
        }
        let Pick::Thread(i, timed_out) = pick else {
            // A thread waiting for input is not stuck: the page will deliver
            // some. Give the stack back to JS so it can, and come back here
            // when it has.
            // Nothing can run, but not everything that cannot run is stuck.
            // A thread waiting for a keystroke needs the page to supply one;
            // a thread waiting on the network needs the page's event loop to
            // turn, so the bytes it is waiting for can arrive. Both mean
            // giving the wasm stack back, and the page is told which.
            // A read parks the same way whatever it is reading, so which
            // kind of wait this is comes from the handle. A socket read is a
            // network wait -- telling the page to ask for a keystroke instead
            // means it never turns its event loop, and the bytes it is
            // waiting for never arrive.
            // The two are not exclusive, so this is a pair of bits and not a
            // choice: bit 0 says a thread wants a keystroke, bit 1 says a
            // thread wants the page's event loop to turn. A guest with a
            // shell in one process and a download in another wants both, and
            // reporting only one of them starves the other.
            let why = with_host(|h| {
                let is_socket = |handle: &Arc<dyn gex_kernel::vfs::FileHandle>| {
                    handle
                        .metadata()
                        .map(|m| m.file_type == gex_kernel::vfs::FileType::Socket)
                        .unwrap_or(false)
                };
                let mut mask = 0u8;
                for t in &h.threads {
                    match &t.waiting {
                        Waiting::Write(_) => mask |= 1,
                        Waiting::Read(handle) => {
                            mask |= if is_socket(handle) { 2 } else { 1 };
                        }
                        // Every timed wait and every poll parks as `Net`, but
                        // most of them are not the network. An interactive
                        // shell polls its terminal rather than blocking in
                        // `read` -- busybox's does -- and calling that a
                        // network wait told the page to turn its event loop
                        // and never ask for a keystroke. The terminal then
                        // took input and dropped it on the floor, forever.
                        //
                        // So a poll is classified by what it is polling.
                        Waiting::Net(deadline) => {
                            match &t.blocked_on {
                                Some(gex_kernel::BlockOp::Poll { fds, .. }) => {
                                    let mut saw = false;
                                    for p in fds {
                                        if let Some(handle) = &p.handle {
                                            mask |= if is_socket(handle) { 2 } else { 1 };
                                            saw = true;
                                        }
                                    }
                                    if !saw {
                                        mask |= 2;
                                    }
                                }
                                _ => mask |= 2,
                            }
                            // A wait that can time out needs the clock to
                            // move, and only the page can move it, so it
                            // wants the event loop whatever else it wants.
                            if deadline.is_some() {
                                mask |= 2;
                            }
                        }
                        _ => {}
                    }
                }
                mask
            });
            if why != 0 {
                with_host(|h| h.paused = why);
                return 2;
            }
            if with_host(|h| {
                h.threads
                    .iter()
                    .all(|t| matches!(t.waiting, Waiting::Futex { deadline: None, .. }))
                    && h.suspended.iter().all(|p| {
                        p.threads
                            .iter()
                            .all(|t| matches!(t.waiting, Waiting::Futex { deadline: None, .. }))
                    })
            }) {
                // Every remaining thread is parked on a futex nothing will
                // ever wake. On Linux this hangs; saying so is more useful.
                trace_line("gex: every thread is blocked on a futex\n");
                with_host(|h| h.exit = Some(128 + 31));
                return 2;
            }
            continue;
        };
        if tracing() {
            let (pc, blocked) = with_host(|h| (h.threads[i].pc, h.threads[i].blocked_on.is_some()));
            trace_line(&format!(
                "[gex] switch to thread {i} at pc={pc:#x} blocked={blocked}
"
            ));
        }
        let resume = with_host(|h| {
            h.current = i;
            let t = &mut h.threads[i];
            t.waiting = Waiting::Runnable;
            let op = t.blocked_on.take();
            let (mut pc, kstate) = (t.pc, t.kstate.clone());
            if let Some(op) = op {
                // The syscall that parked this thread still owes it a return
                // value, and the kernel writes that into the context of
                // whichever thread it thinks is running.
                let mut gm = GuestMem::new(guest_bytes());
                h.kernel.enter_thread(&kstate);
                let r = finish_parked(&op, timed_out, &mut gm);
                let outcome = h.kernel.finish_blocking(&mut gm, r);
                h.kernel.leave_thread(&mut h.threads[h.current].kstate);
                match outcome {
                    SyscallOutcome::Exit(code) => {
                        h.exit = Some(code);
                        return None;
                    }
                    // Finishing a blocked syscall can *deliver a signal*.
                    // Delivery builds the handler's frame and says where to
                    // go, and this is the only place that can honour it: the
                    // thread's saved pc is the instruction after its `svc`,
                    // which is not where a handler starts.
                    //
                    // Dropping it is why every `fork` from a shell broke. A
                    // child exiting is SIGCHLD and a shell handles SIGCHLD,
                    // so `(echo sub); echo after` printed `sub`, ran the
                    // instruction after the `svc` on top of the handler's
                    // frame, and branched to address 0x80 with 17 -- SIGCHLD
                    // -- still sitting in x0 where the handler's first
                    // argument goes.
                    SyscallOutcome::Resume(target) => pc = target,
                    SyscallOutcome::Killed { signal, .. } => {
                        h.exit = Some(128 + signal as i32);
                        return None;
                    }
                    _ => {}
                }
            }
            Some((kstate.ctx_addr, pc))
        });
        match resume {
            Some((ctx_addr, pc)) => {
                with_host(|h| {
                    h.resume = Some(pc);
                    h.resume_ctx = Some(ctx_addr);
                });
                return 1;
            }
            None => return 2,
        }
    }
}

/// What a parked syscall should return now that its thread is running again.
///
/// A woken futex returns zero and the guest re-checks its own condition, the
/// same as on Linux, where a wake is never a promise about the word.
fn finish_parked(
    op: &gex_kernel::BlockOp,
    timed_out: bool,
    gm: &mut GuestMem<'_>,
) -> Result<u64, Errno> {
    match op {
        gex_kernel::BlockOp::FutexWait { .. } if timed_out => Err(Errno::ETIMEDOUT),
        gex_kernel::BlockOp::FutexWait { .. } | gex_kernel::BlockOp::Sleep(_) => Ok(0),
        // The read the guest asked for happens now, into the buffer it named.
        // Doing it here rather than re-executing the instruction is what keeps
        // the guest's pc moving forward: a syscall that parks is finished by
        // the scheduler, never retried.
        gex_kernel::BlockOp::ReadStream { handle, buf, len } => {
            let mut tmp = vec![0u8; *len];
            let n = handle.read_at(0, &mut tmp)?;
            gm.write(*buf, &tmp[..n as usize])?;
            Ok(n)
        }
        gex_kernel::BlockOp::WriteStream { handle, buf, len } => {
            // One turn's worth; the scheduler brings the thread back while
            // anything is left, because the descriptor stays unwritable until
            // the reader has drained.
            let data = gm.read(*buf, *len)?.to_vec();
            handle.write_at(0, &data)
        }
        // `accept` is finished by the kernel, which is the only thing with a
        // descriptor table; the wait here is only for a connection to arrive.
        gex_kernel::BlockOp::AcceptWait { socket } => {
            if socket.has_pending() {
                Ok(0)
            } else {
                Err(Errno::EAGAIN)
            }
        }
        gex_kernel::BlockOp::Connect { socket } => match socket.status() {
            gex_kernel::net::Status::Open => Ok(0),
            gex_kernel::net::Status::Refused => Err(Errno::ECONNREFUSED),
            gex_kernel::net::Status::Closed => Err(Errno::ECONNRESET),
            // Woken by a deadline that a connect does not have; nothing else
            // could have woken it.
            gex_kernel::net::Status::Connecting => Err(Errno::EINPROGRESS),
        },
        gex_kernel::BlockOp::Poll { fds, report, .. } => {
            gex_kernel::syscalls::poll_finish(fds, report, gm)
        }
        // `sigsuspend` only ever ends one way, whether or not a handler ran.
        gex_kernel::BlockOp::SigSuspend { .. } => Err(Errno::EINTR),
        gex_kernel::BlockOp::WaitChild {
            children,
            procs,
            want,
            wstatus,
            ..
        } => {
            for c in children {
                if *want > 0 && c.pid != *want {
                    continue;
                }
                let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(code) = st.take() {
                    c.reaped.store(true, std::sync::atomic::Ordering::Release);
                    procs.reap(c.pid);
                    if *wstatus != 0 {
                        gm.write_u32(*wstatus, ((code as u32) & 0xff) << 8)?;
                    }
                    return Ok(c.pid as u64);
                }
            }
            Err(Errno::ECHILD)
        }
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

/// The context to re-enter with, which changes when the scheduler switches
/// threads. Zero means "the same one as before".
#[no_mangle]
pub extern "C" fn gex_resume_ctx() -> i32 {
    with_host(|h| h.resume_ctx.take().unwrap_or(0) as i32)
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
    // Counted, because "the guest does a lot of fused multiply-adds" is a
    // guess until it is a number. wasm has no FMA instruction, so every one
    // of these leaves the guest module for the shim's -- and a cross-module
    // call in the middle of a floating-point inner loop is the sort of thing
    // that costs more than the arithmetic.
    FMA_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// How many times the guest has asked for a fused multiply-add.
pub static FMA_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read and reset the count, for a harness that wants it per phase.
#[no_mangle]
pub extern "C" fn gex_fma_calls() -> i64 {
    FMA_CALLS.swap(0, std::sync::atomic::Ordering::Relaxed) as i64
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
/// A poll on a loop back-edge, and the browser host's only preemption point.
///
/// With one wasm stack shared by every guest thread, a thread that neither
/// blocks nor exits would otherwise keep the machine to itself for as long as
/// it liked -- and a spin loop waiting on another thread would hang the
/// process outright. The guest's registers are all flushed to its context
/// before this call and the pc it is given is the branch target, so this is
/// exactly the point at which the stack can be handed to someone else.
#[no_mangle]
pub extern "C" fn gex_safepoint(ctx: i32, pc: i64) {
    // Rearm, as the native host does. Translated code counts down a word in
    // the context and calls here when it reaches zero; leaving it there means
    // it stays at zero and every later back-edge calls out. That is a
    // thousand-fold difference in how often this runs, which is worth having
    // right whether or not anything switches.
    {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write_u32(
            ctx as u32 as u64 + ctx::SAFEPOINT_COUNTER as u64,
            SAFEPOINT_INTERVAL,
        );
    }
    // Leaving wasm is not free -- it is a trap, a JS catch and a re-entry --
    // so a thread gets several intervals' worth of machine before it is asked
    // to give it up. The slice is still short enough that a spin loop waiting
    // on another thread makes progress promptly.
    const SLICES_PER_SWITCH: u32 = 4;
    static mut TICK: u32 = 0;
    let due = unsafe {
        TICK = TICK.wrapping_add(1);
        TICK.is_multiple_of(SLICES_PER_SWITCH)
    };
    if !due {
        return;
    }
    let switch = with_host(|h| {
        if h.threads.len() < 2 {
            return false;
        }
        let now = now_ns();
        let runnable = h.threads.iter().enumerate().any(|(i, t)| {
            i != h.current
                && match &t.waiting {
                    Waiting::Runnable => true,
                    Waiting::Futex { parked, deadline } => {
                        parked.woken() || deadline.is_some_and(|d| now >= d)
                    }
                    Waiting::Sleep(d) => now >= *d,
                    Waiting::Read(handle) => handle.poll_readable(),
                    Waiting::Write(handle) => handle.poll_writable(),
                    // Only the scheduler proper re-checks these; a safepoint
                    // switch does not need to.
                    Waiting::Net(deadline) => deadline.is_some_and(|d| now >= d),
                    Waiting::Child => false,
                    Waiting::Exited => false,
                }
        });
        if runnable {
            if tracing() {
                trace_line(&format!(
                    "[gex] preempt t{} at pc={:#x}\n",
                    h.current, pc as u64
                ));
            }
            // The context's own PC has to be exact too: a plain branch does
            // not store it, and the next thing to read it -- a signal frame,
            // a `clone` copying this context -- would otherwise see wherever
            // the guest last happened to leave it.
            let a = h.threads[h.current].kstate.ctx_addr;
            let mut gm = GuestMem::new(guest_bytes());
            let _ = gm.write_u64(a + ctx::PC as u64, pc as u64);
            h.threads[h.current].pc = pc as u64;
            h.threads[h.current].waiting = Waiting::Runnable;
        }
        runnable
    });
    if switch && schedule() != 0 {
        trap();
    }
}

/// A dispatch miss: translate the block at `pc`, and everything reachable
/// from it, into a module for JS to instantiate.
///
/// This is what lets a page ship a small module. Translating a whole program
/// ahead of time costs 62 MiB of wasm for SQLite; discovering only what the
/// entry point reaches costs 14 MiB, and the rest arrives here as it is
/// needed.
///
/// The shim can lift and encode but not compile -- nothing inside wasm can
/// hand bytes to the engine -- so it leaves the module behind and traps out,
/// and JS instantiates it against the same memory and dispatch table before
/// resuming the guest at exactly the address that missed.
#[no_mangle]
pub extern "C" fn gex_dispatch_miss(_ctx: i32, pc: i64) -> i32 {
    let pc = pc as u64;
    if tracing() {
        trace_line(&format!("[gex] miss at pc={pc:#x}\n"));
    }
    // A signal handler returning. When the guest gave no `sa_restorer` the
    // kernel puts `SIGRETURN_TRAMPOLINE` in x30, an address inside the
    // reserved region that deliberately has nothing translated at it -- so
    // the handler's `ret` arrives here, which is the whole mechanism.
    //
    // The native host has always done this. The browser did not, and the
    // failure was spectacular rather than quiet: any handler at all ended the
    // run with `nothing to translate at pc=0xffff0`. What made it look like a
    // fork bug is that a shell's first handler is SIGCHLD, so it took a child
    // exiting to reach one.
    if pc == gex_kernel::signal::SIGRETURN_TRAMPOLINE {
        let resumed = with_host(|h| {
            let mut gm = GuestMem::new(guest_bytes());
            let kstate = h.threads[h.current].kstate.clone();
            h.kernel.enter_thread(&kstate);
            let r = h.kernel.sigreturn(&mut gm);
            h.kernel.leave_thread(&mut h.threads[h.current].kstate);
            match r {
                Ok(target) => {
                    h.threads[h.current].pc = target;
                    h.resume = Some(target);
                    h.resume_ctx = Some(kstate.ctx_addr);
                    true
                }
                Err(_) => {
                    trace_line("gex: corrupt signal frame on sigreturn\n");
                    h.exit = Some(128 + 11);
                    false
                }
            }
        });
        let _ = resumed;
        trap();
    }
    let translated = with_host(|h| {
        if !h.jit.described {
            return false;
        }
        let exec: Vec<(u64, u64)> = h
            .kernel
            .mem
            .vmas()
            .iter()
            .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect();
        if !exec.iter().any(|(a, b)| pc >= *a && pc + 4 <= *b) {
            return false;
        }
        let started = now_ns();
        let known = std::mem::take(&mut h.jit.translated);
        let (unit, stats) = gex_xlate::translate_runtime(
            guest_bytes(),
            pc,
            &|a| exec.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
            &|a| known.contains(&a),
            h.jit.page_table.clone(),
            Default::default(),
            // A cap keeps one miss from translating a whole library at once;
            // the next miss picks up where this stopped.
            2048,
            // The browser's memory is not declared shared, and an import that
            // disagrees with the memory it is given fails to instantiate.
            false,
        );
        h.jit.translated = known;
        if stats.blocks == 0 {
            return false;
        }
        h.jit.page_table = unit.page_table.clone();
        for (addr, _) in &unit.exported_blocks {
            h.jit.translated.insert(*addr);
        }
        h.jit.pending_dirs = unit.dir_entries.clone();
        h.jit.pending_slots = unit.table_slots;
        h.jit.pending = Some(unit.wasm);
        h.jit.blocks += stats.blocks as u64;
        h.jit.compiles += 1;
        h.jit.nanos += now_ns().saturating_sub(started);
        if tracing() {
            trace_line(&format!(
                "[gex]   translated {} blocks in {} ms, {} vmas, {} slots, {} pages\n",
                stats.blocks,
                (now_ns().saturating_sub(started)) / 1_000_000,
                exec.len(),
                unit.table_slots,
                unit.dir_entries.len()
            ));
        }
        // Come back at the address that missed, now that it will resolve.
        h.resume = Some(pc);
        h.resume_ctx = Some(h.threads[h.current].kstate.ctx_addr);
        true
    });
    if !translated {
        if !with_host(|h| h.jit.described) {
            trace_line("gex: the guest module carries no gex.meta section, so there is\n");
            trace_line("gex: nowhere for runtime translation to carry on from. Rebuild it.\n");
        }
        trace_line(&format!("gex: nothing to translate at pc={pc:#x}\n"));
        with_host(|h| h.exit = Some(128 + 11));
    }
    trap();
}

/// The module JS has to instantiate, or zero length if there is none.
#[no_mangle]
pub extern "C" fn gex_jit_pending_ptr() -> u32 {
    with_host(|h| h.jit.pending.as_ref().map_or(0, |v| v.as_ptr() as u32))
}

#[no_mangle]
pub extern "C" fn gex_jit_pending_len() -> u32 {
    with_host(|h| h.jit.pending.as_ref().map_or(0, |v| v.len() as u32))
}

/// How many slots the dispatch table must hold for the pending module.
#[no_mangle]
pub extern "C" fn gex_jit_slots() -> u32 {
    with_host(|h| h.jit.pending_slots as u32)
}

/// The dispatch-table slot a translated block belongs in.
#[no_mangle]
pub extern "C" fn gex_dispatch_slot(addr: i64) -> i32 {
    with_host(|h| {
        let page = gex_abi::dispatch_page(addr as u64);
        match h.jit.page_table.lookup(page) {
            Some(base) => gex_abi::dispatch_index(addr as u64, base) as i32,
            None => -1,
        }
    })
}

/// Publish the pending module's pages, now that its blocks are in the table.
///
/// Deliberately last: until the directory entry points at real slots, a guest
/// branch into one of these pages must keep reaching the miss stub.
#[no_mangle]
pub extern "C" fn gex_jit_installed() {
    with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in std::mem::take(&mut h.jit.pending_dirs) {
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
        }
        h.jit.pending = None;
        h.jit.pending_slots = 0;
    });
}

/// Blocks translated at run time, and how many times the translator ran.
#[no_mangle]
pub extern "C" fn gex_jit_blocks() -> i64 {
    with_host(|h| h.jit.blocks as i64)
}

#[no_mangle]
pub extern "C" fn gex_jit_compiles() -> i64 {
    with_host(|h| h.jit.compiles as i64)
}

/// Milliseconds the shim itself spent translating.
#[no_mangle]
pub extern "C" fn gex_jit_millis() -> i64 {
    with_host(|h| (h.jit.nanos / 1_000_000) as i64)
}
