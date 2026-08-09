// `memory.atomic.wait32` and `memory.atomic.notify` are still unstable in
// core; this module is nightly-only already, for the same reason.
#![feature(stdarch_wasm_atomic_wait)]
//! The global kernel module.
//!
//! One module, one memory, shared by every Worker of every guest process.
//! That is the whole point of it: the process table, and in time the
//! filesystem, the pipes and the sockets, live *here* -- in a memory every
//! Worker instantiated against -- so a syscall stays a direct call rather
//! than becoming a message and a wait.
//!
//! It is what lets the snapshot switcher go. The switcher exists because
//! every process shares one address space, so running a second one means
//! writing the first out and reading the second in. Give each process its own
//! memory and nothing collides and nothing needs saving -- but then the
//! things processes are *supposed* to share stop being shared too, and this
//! is where they go instead.
//!
//! The process table is first because it is what `fork`, `wait4` and `kill`
//! need across processes, and those are exactly the calls the switcher
//! currently fakes by keeping every process in one place.
//!
//! `gex_kernel::ProcTable` moves in unchanged. It was already written for
//! this: its state is behind a lock, signals are left for the target to
//! collect rather than delivered by the sender, and its comments describe a
//! world where "the target's signal state lives in its own kernel". The only
//! thing that was missing was somewhere for the shared half to live that is
//! not one process's memory.
//!
//! A guest buffer is in the *process's* memory, which this module imports as
//! a second memory and cannot address with a pointer; `gex-kmem` copies
//! across the two with `memory.copy`.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use gex_kernel::proc::ProcTable;

/// Stacks, one per Worker.
///
/// A wasm global belongs to an *instance*, and every Worker makes its own
/// instance of this module -- so each has its own `__stack_pointer`, and
/// wasm-ld initialises every one of them to the same address. The memory
/// underneath is shared. Two Workers inside this module at the same moment
/// therefore write their locals over each other, which is as bad as it
/// sounds: the failures were an unaligned atomic, a hang, and a fork that
/// worked whenever tracing slowed the child down enough to miss the overlap.
///
/// So each Worker gets a slice of this array and points its stack pointer at
/// the top of it. The array is in `.bss` -- it costs address space in the
/// kernel memory and nothing else until it is written.
///
/// A quarter of a megabyte each. This module's deepest call is a `TmpFs`
/// lookup and a `Vec` grow; the browser shim, which does real work, gets
/// eight megabytes.
pub const STACK_BYTES: u32 = 256 * 1024;
/// As many as there are guest processes plus their threads.
///
/// It is not free: `.bss` counts towards the module's *declared* initial
/// memory, so every slice here is memory a host must provide before the
/// kernel will link at all.
///
/// A hundred and twenty-eight, because threads made thirty-two too few
/// straight away: a program with eight workers and a few detached ones
/// exhausted them before it had finished starting. The host hands a slice
/// back when a Worker ends, so this bounds what runs *at once* rather than
/// what a run may ever start.
pub const STACK_COUNT: u32 = 128;
static mut STACKS: [u8; (STACK_BYTES * STACK_COUNT) as usize] =
    [0; (STACK_BYTES * STACK_COUNT) as usize];

/// Where the stacks start, and how big each is.
///
/// Asked by the *first* Worker, which is the one that safely can: it holds
/// the stack wasm-ld gave the module, and nobody else is in here yet. It
/// hands the numbers to the host, and the host tells every later Worker which
/// slice is its own -- which that Worker installs before it calls anything.
#[no_mangle]
pub extern "C" fn k_stack_area() -> u32 {
    // SAFETY: an address, not a read.
    unsafe { core::ptr::addr_of!(STACKS) as u32 }
}

#[no_mangle]
pub extern "C" fn k_stack_size() -> u32 {
    STACK_BYTES
}

#[no_mangle]
pub extern "C" fn k_stack_count() -> u32 {
    STACK_COUNT
}

/// The word every Worker waits on, and every Worker bumps.
///
/// A guest that blocks -- reading a socket the other end of which is another
/// process, waiting for a child, waiting on a futex -- used to be woken by
/// nothing at all. Its Worker unwound out of wasm and the JavaScript around
/// it asked "is anything runnable yet?" over and over, once a millisecond,
/// until the answer changed. Nobody told it. That millisecond was the floor
/// on every interaction between two guest processes, and an X client and an
/// X server pay it twice a frame.
///
/// This is where the telling happens. It has to be here and nowhere else:
/// `memory.atomic.wait32` and `memory.atomic.notify` act on the memory the
/// module addresses, and this module's memory is the one every Worker of
/// every process shares. A word in a *process's* memory can only be seen by
/// that process's own threads, which is the wrong scope entirely.
///
/// A counter rather than a flag, so a wake that arrives between deciding to
/// park and parking is not lost: the parker samples the value first and asks
/// to sleep only while it is still that value.
static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn wake_addr() -> *mut i32 {
    &WAKE as *const std::sync::atomic::AtomicI32 as *mut i32
}

/// What the counter reads now. Sampled before a Worker decides to park.
#[no_mangle]
pub extern "C" fn k_wake_seq() -> i32 {
    WAKE.load(std::sync::atomic::Ordering::Acquire)
}

/// Something happened that another Worker may have been waiting for.
///
/// Called after every syscall rather than at the handful of places that
/// obviously make someone else runnable. Deciding *which* calls those are is
/// a judgement that has to be right every time -- a socket write, a pipe
/// write, an exit, a futex wake, a signal, a file created where somebody is
/// waiting for it to appear -- and being wrong once is a process that sleeps
/// through its wake-up. A notify with nobody waiting is a few nanoseconds.
#[no_mangle]
pub extern "C" fn k_wake() {
    WAKE.fetch_add(1, std::sync::atomic::Ordering::Release);
    // SAFETY: the address of a static in this module's own shared memory,
    // which is what this instruction requires.
    unsafe {
        core::arch::wasm32::memory_atomic_notify(wake_addr(), u32::MAX);
    }
}

/// Sleep until the counter leaves `seq`, or `ns` nanoseconds pass.
///
/// Returns 0 woken, 1 "the value had already moved", 2 timed out -- the
/// instruction's own answers. A negative `ns` waits for ever, which nothing
/// here asks for: a bound is what keeps a Worker answerable to the page.
#[no_mangle]
pub extern "C" fn k_park(seq: i32, ns: i64) -> i32 {
    // SAFETY: as above. Blocking is allowed here because every guest runs in
    // a Worker; the page's own thread never enters this module.
    unsafe { core::arch::wasm32::memory_atomic_wait32(wake_addr(), seq, ns) }
}

/// What this module last panicked with, so a crash in here can be read.
///
/// It has no console: the browser shim writes through an import the page
/// supplies, and this module imports nothing but two memories. So a panic in
/// here unwound to a bare `unreachable` with no message at all -- and since
/// the shim is mid-syscall when it happens, every later call into the shim
/// found the kernel still borrowed and panicked on top of it. What a run
/// showed was three re-entrancy complaints and no cause.
///
/// The host reads this straight out of the shared memory it created; see
/// `k_panic_ptr`.
static PANIC: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Install the hook, once. Called at the top of every export that could
/// panic -- which is all of them, because any of them can be the first.
fn note_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let mut slot = PANIC.lock().unwrap_or_else(|e| e.into_inner());
            *slot = format!("{info}");
        }));
    });
}

/// Where the last panic message is, in *this* module's memory, and how long.
///
/// A host that made the memory can read it directly; nothing has to be
/// copied. Zero length means this module has not panicked.
#[no_mangle]
pub extern "C" fn k_panic_ptr() -> u32 {
    PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn k_panic_len() -> u32 {
    PANIC.lock().unwrap_or_else(|e| e.into_inner()).len() as u32
}

/// The system's processes, in the memory every Worker shares.
///
/// A `static` rather than anything handed across a boundary: this module's
/// data lives in its own memory, that memory is the global one, and so a
/// static here is already the same bytes for every Worker that instantiated
/// against it. That is the entire trick.
fn procs() -> &'static ProcTable {
    note_panics();
    static PROCS: OnceLock<ProcTable> = OnceLock::new();
    PROCS.get_or_init(ProcTable::new)
}

/// Which dispatch slots the code of a given identity was translated into.
///
/// Keyed by the dispatch page and by what the bytes there *are* -- the file,
/// the offset within it, the length of the mapping -- so two processes that
/// map the same library at the same address agree without either knowing the
/// other exists.
///
/// This lived in the shim, per Worker, and travelled parent to child in the
/// fork record and nowhere else. That made sibling reuse impossible: a shell
/// loop that execs `ls` three times lifted the same 20,868 blocks three
/// times, because each `ls` started from the shell's snapshot and the shell
/// has never run `ls`. Counted with `gex_reuse_stat`: `hit=0`, every time.
///
/// Here it is one map for the whole system, for the same reason the process
/// table and the filesystem are: this module's data lives in the memory every
/// Worker instantiates against, so a static is already shared bytes.
///
/// Base zero means absent. A real base never is -- slot zero and the span
/// above it are the miss stubs, reserved before anything is handed out.
fn slots() -> &'static Mutex<BTreeMap<(u64, u64), u32>> {
    note_panics();
    static SLOTS: OnceLock<Mutex<BTreeMap<(u64, u64), u32>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The slots page `page` of the object identified by `key` was translated
/// into, or zero.
#[no_mangle]
pub extern "C" fn k_slot_get(page: u64, key: u64) -> u32 {
    slots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(page, key))
        .copied()
        .unwrap_or(0)
}

/// Record it, if nothing has claimed that page and identity already.
///
/// First writer wins, which is the same rule the per-Worker map used: a page
/// that already has an owner keeps it, because the code that was emitted
/// names whichever base it was given.
#[no_mangle]
pub extern "C" fn k_slot_put(page: u64, key: u64, base: u32) {
    if base == 0 {
        return;
    }
    slots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry((page, key))
        .or_insert(base);
}

/// How many pages the map answers for. A diagnostic.
#[no_mangle]
pub extern "C" fn k_slot_count() -> u32 {
    slots().lock().unwrap_or_else(|e| e.into_inner()).len() as u32
}

/// Every translated module, once, and which object's pages each one covers.
///
/// The slot map above lets a Worker learn *where* code of a given identity
/// was put. That is not enough to use it: a dispatch table belongs to a
/// module instance, so a Worker that has not instantiated the module cannot
/// reach the code however well it knows the slot number. This is the other
/// half -- the module itself, in the memory everybody shares, so the second
/// process to run `ls` installs what the first one produced instead of
/// lifting it again.
///
/// Bytes rather than anything compiled, because compiled code is a JavaScript
/// object and this is a wasm memory. The Worker copies them out and hands
/// them to the same `pending` queue a fresh translation goes through, which
/// is why this needs no new path through the host at all.
struct SharedModule {
    wasm: Vec<u8>,
    /// How large the dispatch table must be for it.
    ///
    /// Kept here rather than recomputed because it is a fact about the
    /// process that *produced* the module, and the one installing it has a
    /// different high-water mark. Passing the local number understates it,
    /// the table is not grown far enough, and the install throws -- which is
    /// how the first attempt at this wedged.
    slots: u64,
    /// Which pages it holds code for, and at which base. The installing
    /// Worker has to assign these before the install, or the blocks resolve
    /// to no slot and are silently dropped.
    dirs: Vec<(u64, u32)>,
}

struct ModStore {
    /// By id. Index zero is never used: zero means "none".
    mods: Vec<SharedModule>,
    /// Which module holds the code for a page of a given object.
    by_page: BTreeMap<(u64, u64), u32>,
    /// What has been stored, against [`MOD_BUDGET`].
    total: usize,
}

/// How much translated wasm to keep for sharing.
///
/// The kernel's memory is capped at 1 GiB and the filesystem is in it too, so
/// this cannot be unbounded. A quarter is enough for the libraries a forking
/// program shares -- apt's whole closure is 80 MB of wasm and it only ever
/// translates the part it reaches -- and running out means falling back to
/// translating, which is what happened before this existed.
const MOD_BUDGET: usize = 256 << 20;

fn modules() -> &'static Mutex<ModStore> {
    note_panics();
    static MODS: OnceLock<Mutex<ModStore>> = OnceLock::new();
    MODS.get_or_init(|| {
        Mutex::new(ModStore {
            mods: vec![SharedModule {
                wasm: Vec::new(),
                slots: 0,
                dirs: Vec::new(),
            }],
            by_page: BTreeMap::new(),
            total: 0,
        })
    })
}

/// Keep a module. Returns its id, or zero when the budget is spent.
///
/// `dirs` is `(u64 page, u32 base)` pairs, twelve bytes each, little-endian.
///
/// SAFETY: every pointer names the *calling process's* memory, not the
/// kernel's. The two are different wasm memories and a Rust pointer here
/// means the kernel's, so the bytes have to be pulled across rather than
/// read -- reading them directly is how this first shipped, and it stored a
/// module of zeros that `WebAssembly.compile` then rejected for its magic
/// word.
#[no_mangle]
pub unsafe extern "C" fn k_mod_store(
    ptr: u32,
    len: u32,
    slots: u64,
    dirs_ptr: u32,
    dirs_len: u32,
) -> u32 {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    if len == 0 || m.total + len as usize > MOD_BUDGET {
        return 0;
    }
    let mut wasm = vec![0u8; len as usize];
    unsafe { gex_kmem::pull(wasm.as_mut_ptr(), ptr, len) };
    let mut raw = vec![0u8; dirs_len as usize];
    if dirs_len > 0 {
        unsafe { gex_kmem::pull(raw.as_mut_ptr(), dirs_ptr, dirs_len) };
    }
    let mut dirs = Vec::with_capacity(raw.len() / 12);
    for c in raw.chunks_exact(12) {
        let page = u64::from_le_bytes(c[0..8].try_into().unwrap());
        let base = u32::from_le_bytes(c[8..12].try_into().unwrap());
        dirs.push((page, base));
    }
    m.total += len as usize;
    m.mods.push(SharedModule { wasm, slots, dirs });
    (m.mods.len() - 1) as u32
}

/// Say that module `id` holds the code for page `page` of object `key`.
#[no_mangle]
pub extern "C" fn k_mod_claim(page: u64, key: u64, id: u32) {
    if id == 0 {
        return;
    }
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_page
        .entry((page, key))
        .or_insert(id);
}

/// Which module holds it, or zero.
#[no_mangle]
pub extern "C" fn k_mod_for(page: u64, key: u64) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_page
        .get(&(page, key))
        .copied()
        .unwrap_or(0)
}

/// How large a dispatch table module `id` needs.
#[no_mangle]
pub extern "C" fn k_mod_slots(id: u32) -> u64 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mods
        .get(id as usize)
        .map(|m| m.slots)
        .unwrap_or(0)
}

/// How long its bytes are, and how many bytes its `dirs` take.
#[no_mangle]
pub extern "C" fn k_mod_len(id: u32) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mods
        .get(id as usize)
        .map(|m| m.wasm.len() as u32)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn k_mod_dirs_len(id: u32) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mods
        .get(id as usize)
        .map(|m| (m.dirs.len() * 12) as u32)
        .unwrap_or(0)
}

/// Copy the bytes out. Returns how much was written, or zero if it did not
/// fit.
///
/// SAFETY: `out` and `cap` describe a range of the calling process's memory.
#[no_mangle]
pub unsafe extern "C" fn k_mod_read(id: u32, out: u32, cap: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sm) = m.mods.get(id as usize) else {
        return 0;
    };
    if sm.wasm.len() > cap as usize {
        return 0;
    }
    unsafe { gex_kmem::push(out, sm.wasm.as_ptr(), sm.wasm.len() as u32) };
    sm.wasm.len() as u32
}

/// And the `dirs`, in the same twelve-byte encoding they arrived in.
///
/// SAFETY: as above.
#[no_mangle]
pub unsafe extern "C" fn k_mod_dirs_read(id: u32, out: u32, cap: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sm) = m.mods.get(id as usize) else {
        return 0;
    };
    let n = sm.dirs.len() * 12;
    if n > cap as usize {
        return 0;
    }
    let mut buf = vec![0u8; n];
    for (i, (page, base)) in sm.dirs.iter().enumerate() {
        buf[i * 12..i * 12 + 8].copy_from_slice(&page.to_le_bytes());
        buf[i * 12 + 8..i * 12 + 12].copy_from_slice(&base.to_le_bytes());
    }
    unsafe { gex_kmem::push(out, buf.as_ptr(), n as u32) };
    n as u32
}

/// Somewhere to stage a guest buffer copied out of a process's memory.
static mut SCRATCH: [u8; 4096] = [0; 4096];

/// Create a process with `ppid` as its parent. Returns the new pid.
#[no_mangle]
pub extern "C" fn k_spawn(ppid: i32) -> i32 {
    procs().spawn(ppid)
}

/// Record that `pid` has exited with `status`.
///
/// `status` is an exit *code*, or the negated signal number when the process
/// died of one -- the same convention `ProcTable::exit` takes. The two are
/// different things to `wait4`, and [`k_wait`] is where they become the one
/// word a parent reads.
#[no_mangle]
pub extern "C" fn k_exit(pid: i32, status: i32) {
    procs().exit(pid, status);
}

/// Collect a finished child of `ppid`. Zero if none is ready.
///
/// Returns the pid in the high 32 bits and the wait status in the low 32:
/// one call and two results, rather than a second call for the status. The
/// pairing has to be atomic, because two processes in two Workers can be
/// collecting from this table at the same moment and a status left in a
/// static between calls belongs to whichever of them asked last.
///
/// `want` is a specific pid, or -1 for any.
#[no_mangle]
pub extern "C" fn k_wait(ppid: i32, want: i32) -> i64 {
    for c in procs().children(ppid) {
        if want > 0 && c.pid != want {
            continue;
        }
        let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(code) = st.take() {
            c.reaped.store(true, std::sync::atomic::Ordering::Release);
            procs().reap(c.pid);
            // `WIFEXITED`: the code in the second byte, low bits clear.
            // `WIFSIGNALED`: the signal in the low seven, which is what a
            // negative code means here.
            let word = if code < 0 {
                (-code) as u32 & 0x7f
            } else {
                ((code as u32) & 0xff) << 8
            };
            return ((c.pid as i64) << 32) | word as i64;
        }
    }
    0
}

/// Whether a `wait4` by `ppid` for `want` would answer now: a collectable
/// child, or no matching child at all.
///
/// Separate from [`k_wait`] because collecting *takes* the child, and a
/// scheduler asking whether a parked thread can run must not reap one.
#[no_mangle]
pub extern "C" fn k_wait_ready(ppid: i32, want: i32) -> i32 {
    procs().wait_ready(ppid, want) as i32
}

/// Whether `ppid` has any live children, which is what tells `wait4` the
/// difference between "nothing ready yet" and `ECHILD`.
#[no_mangle]
pub extern "C" fn k_has_children(ppid: i32) -> i32 {
    procs().has_children(ppid) as i32
}

/// Send a signal, leaving it pending for the target to collect.
///
/// Returns the number of processes signalled, or a negative errno. The
/// sender never delivers: the target picks it up at its next syscall, which
/// is what makes this safe to call from another process's Worker.
#[no_mangle]
pub extern "C" fn k_kill(caller: i32, target: i32, signo: u32) -> i32 {
    match procs().kill(caller, target, signo) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// Take everything pending for `pid`, as a bitmask of `1 << (signo - 1)`.
#[no_mangle]
pub extern "C" fn k_take_signals(pid: i32) -> u64 {
    procs().take_signals(pid)
}

/// Whether anything is pending for `pid`, without taking it.
#[no_mangle]
pub extern "C" fn k_has_signals(pid: i32) -> i32 {
    procs().has_signals(pid) as i32
}

/// Whether `pid` is a live process.
#[no_mangle]
pub extern "C" fn k_exists(pid: i32) -> i32 {
    procs().exists(pid) as i32
}

/// The parent of `pid`.
#[no_mangle]
pub extern "C" fn k_ppid(pid: i32) -> i32 {
    procs().ppid(pid)
}

/// Forget a collected process, so a long-lived init does not accumulate a
/// record per respawn for the life of the machine.
#[no_mangle]
pub extern "C" fn k_reap(pid: i32) {
    procs().reap(pid);
}

/// `getpgid`, with Linux's zero-means-the-caller rule. Negative errno on
/// failure, as every call here reports it.
#[no_mangle]
pub extern "C" fn k_pgid(caller: i32, of: i32) -> i32 {
    match procs().pgid(caller, of) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `getsid`, same rule.
#[no_mangle]
pub extern "C" fn k_sid(caller: i32, of: i32) -> i32 {
    match procs().sid(caller, of) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `setsid`: become the leader of a new session and a new group.
#[no_mangle]
pub extern "C" fn k_setsid(pid: i32) -> i32 {
    match procs().setsid(pid) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `setpgid`. Zero for either argument means the caller.
#[no_mangle]
pub extern "C" fn k_setpgid(caller: i32, target: i32, pgid: i32) -> i32 {
    match procs().setpgid(caller, target, pgid) {
        Ok(()) => 0,
        Err(e) => -e.0,
    }
}

/// Ask every process to stop, with this status. See `ProcTable::request_halt`.
#[no_mangle]
pub extern "C" fn k_request_halt(code: i32) {
    procs().request_halt(code);
}

/// The halt status, or -1 if nothing has asked for one.
#[no_mangle]
pub extern "C" fn k_halt_code() -> i32 {
    procs().halt_code().unwrap_or(-1)
}

// -- pipes -------------------------------------------------------------------
//
// The second thing processes share, and the one a shell pipeline and apt's
// method workers are made of. A pipe is a buffer with two ends and a pair of
// counts, which is exactly the kind of state that forced every process into
// one address space: `Arc<Pipe>` is a pointer, and a pointer means a memory.
//
// Here it is a *number* instead. Each process's file table holds an id and the
// buffer lives in the global memory, so the two ends can be in Workers that
// share nothing else.

/// Every pipe in the system, by id.
fn pipes() -> &'static std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>> {
    note_panics();
    static PIPES: OnceLock<std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>>> =
        OnceLock::new();
    PIPES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn pipe_at(id: i32) -> Option<std::sync::Arc<gex_kernel::pipe::Pipe>> {
    pipes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
}

/// Make a pipe with both ends open. Returns its id.
#[no_mangle]
pub extern "C" fn k_pipe_new() -> i32 {
    let mut t = pipes().lock().unwrap_or_else(|e| e.into_inner());
    t.push(std::sync::Arc::new(gex_kernel::pipe::Pipe::new()));
    (t.len() - 1) as i32
}

/// Another descriptor onto one end, as `dup` and `fork` both make.
#[no_mangle]
pub extern "C" fn k_pipe_open_end(id: i32, write: i32) {
    if let Some(p) = pipe_at(id) {
        p.open_end(write != 0);
    }
}

/// Drop a descriptor. The last one on an end is what a reader sees as EOF.
#[no_mangle]
pub extern "C" fn k_pipe_close_end(id: i32, write: i32) {
    if let Some(p) = pipe_at(id) {
        p.close_end(write != 0);
    }
}

/// Write `len` bytes of the caller's memory into the pipe.
///
/// The bytes are copied out of the calling process's memory first, because
/// this module cannot address it -- that copy is what makes the two ends able
/// to live in different address spaces at all. Returns bytes written, or a
/// negative errno.
#[no_mangle]
pub extern "C" fn k_pipe_write(id: i32, at: u32, len: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -9 }; // EBADF
    let mut buf = vec![0u8; len as usize];
    unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    match p.write(&buf) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// Read up to `len` bytes into the caller's memory. See [`k_pipe_write`].
#[no_mangle]
pub extern "C" fn k_pipe_read(id: i32, at: u32, len: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    match p.read(&mut buf) {
        Ok(n) => {
            unsafe { gex_kmem::push(at, buf.as_ptr(), n as u32) };
            n as i32
        }
        Err(e) => -(e.0 as i32),
    }
}

/// Would a read block? False also when every writer has gone, which is EOF.
#[no_mangle]
pub extern "C" fn k_pipe_readable(id: i32) -> i32 {
    pipe_at(id).is_some_and(|p| p.readable()) as i32
}

/// Would a write block?
#[no_mangle]
pub extern "C" fn k_pipe_writable(id: i32) -> i32 {
    pipe_at(id).is_some_and(|p| p.writable()) as i32
}

/// How many bytes are waiting, for `FIONREAD` and for a poll's report.
#[no_mangle]
pub extern "C" fn k_pipe_buffered(id: i32) -> i32 {
    pipe_at(id).map_or(0, |p| p.buffered() as i32)
}

/// Copy `len` bytes of the calling process's memory in, and report their sum.
///
/// The shape every syscall taking a pointer will use: the argument names an
/// address in a memory this module cannot dereference, so it is copied here
/// first. Kept as a live check that the two-memory path works from inside the
/// real kernel and not only from the toy one it replaced.
#[no_mangle]
pub extern "C" fn k_checksum(at: u32, len: u32) -> u32 {
    let n = (len as usize).min(4096);
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, n as u32) };
    let mut sum: u32 = 0;
    for b in &buf[..n] {
        sum = sum.wrapping_mul(31).wrapping_add(*b as u32);
    }
    sum
}

// -- the filesystem ----------------------------------------------------------
//
// The third thing processes share, and the one without which the other two do
// not help. A shell that unpacks a rootfs and forks hands its child an empty
// root; the child's `execve` cannot find the program it was forked to run.
// Under the switcher the child got the parent's whole address space and the
// question never arose.
//
// So the files live here. Each process mounts a `RemoteFs` -- an id and
// nothing else -- and every call arrives as an address in that process's
// memory, which `gex-kmem` copies across. The layouts on the wire are in
// `gex_kernel::vfs::remote`, written once and used by both sides.

use gex_kernel::vfs::{remote, FileHandle, FileSystem};

/// The root every process sees.
fn rootfs() -> &'static std::sync::Arc<gex_kernel::vfs::TmpFs> {
    note_panics();
    static ROOT: OnceLock<std::sync::Arc<gex_kernel::vfs::TmpFs>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let fs = gex_kernel::vfs::TmpFs::new();
        // The directories a guest expects to be there before anything has
        // been bundled. `/tmp` above all: a shell, a compiler and apt all
        // write into it, and `open(O_CREAT)` under a directory that does not
        // exist is `ENOENT` rather than a directory being made.
        for d in ["/tmp", "/proc", "/dev", "/run", "/var", "/var/tmp"] {
            fs.add_dir(d);
        }
        std::sync::Arc::new(fs)
    })
}

/// Every file a process has open here, by id.
///
/// A `Vec` with holes rather than a map: an id is handed straight back to a
/// process and used on every read, so the lookup is the hot part and the
/// churn is not.
type Handles = std::sync::Mutex<Vec<Option<std::sync::Arc<dyn FileHandle>>>>;
fn handles() -> &'static Handles {
    note_panics();
    static H: OnceLock<Handles> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn handle_at(id: i32) -> Option<std::sync::Arc<dyn FileHandle>> {
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
        .flatten()
}

/// A path argument: copied out of the caller's memory, because this module
/// cannot dereference an address in it.
fn path_of(at: u32, len: u32) -> String {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    // Leading slashes go. A mount hands its filesystem a relative path and a
    // host handing one in from outside naturally writes the absolute one, and
    // `TmpFs` keys on the relative form -- so accepting both here is the
    // difference between a bundle that is readable and one that is only
    // writable.
    String::from_utf8_lossy(&buf)
        .trim_start_matches('/')
        .to_string()
}

/// A `Result` as the caller reads it: a count, or a negated errno.
fn as_ret(r: Result<u64, gex_kernel::errno::Errno>) -> i32 {
    match r {
        Ok(n) => n as i32,
        Err(e) => -e.0,
    }
}

fn as_unit(r: Result<(), gex_kernel::errno::Errno>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => -e.0,
    }
}

/// Open a path, returning a handle id or a negated errno.
#[no_mangle]
pub extern "C" fn k_fs_open(path: u32, path_len: u32, flags: u64, mode: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().open(&p, flags, mode) {
        Ok(h) => {
            let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
            // The first free slot, so a process that opens and closes in a
            // loop does not grow this without bound.
            match t.iter().position(|s| s.is_none()) {
                Some(i) => {
                    t[i] = Some(h);
                    i as i32
                }
                None => {
                    t.push(Some(h));
                    (t.len() - 1) as i32
                }
            }
        }
        Err(e) => -e.0,
    }
}

/// Drop a handle. Called from the client's `Drop`, so a process that exits
/// without closing still frees what it held.
#[no_mangle]
pub extern "C" fn k_fs_close(handle: i32) {
    let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = t.get_mut(handle as usize) {
        *slot = None;
    }
}

/// Read into the caller's memory. Returns bytes read, or a negated errno.
#[no_mangle]
pub extern "C" fn k_fs_read(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    match h.read_at(offset, &mut buf) {
        Ok(n) => {
            if n > 0 {
                unsafe { gex_kmem::push(at, buf.as_ptr(), n as u32) };
            }
            n as i32
        }
        Err(e) => -e.0,
    }
}

/// Write from the caller's memory.
#[no_mangle]
pub extern "C" fn k_fs_write(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    as_ret(h.write_at(offset, &buf))
}

/// This handle's metadata, as `remote::META_BYTES` bytes at `at`.
#[no_mangle]
pub extern "C" fn k_fs_meta(handle: i32, at: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    match h.metadata() {
        Ok(m) => {
            let b = remote::encode_meta(&m);
            unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            0
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_truncate(handle: i32, len: u64) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    as_unit(h.truncate(len))
}

/// A directory listing, or `-ERANGE` when `cap` will not hold it.
#[no_mangle]
pub extern "C" fn k_fs_read_dir(handle: i32, at: u32, cap: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    match h.read_dir() {
        Ok(entries) => {
            let b = remote::encode_dir(&entries);
            if b.len() as u32 > cap {
                return -34; // ERANGE
            }
            if !b.is_empty() {
                unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            }
            b.len() as i32
        }
        Err(e) => -e.0,
    }
}

/// A path's metadata, without opening it -- which matters for a symlink and
/// for anything an open would refuse.
#[no_mangle]
pub extern "C" fn k_fs_path_meta(path: u32, path_len: u32, at: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().metadata(&p) {
        Ok(m) => {
            let b = remote::encode_meta(&m);
            unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            0
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_read_link(path: u32, path_len: u32, at: u32, cap: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().read_link(&p) {
        Ok(t) => {
            let b = t.as_bytes();
            if b.len() as u32 > cap {
                return -34; // ERANGE
            }
            if !b.is_empty() {
                unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            }
            b.len() as i32
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_unlink(path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().unlink(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_mkdir(path: u32, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().mkdir(&path_of(path, path_len), mode))
}

#[no_mangle]
pub extern "C" fn k_fs_rmdir(path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().rmdir(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_rename(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    as_unit(rootfs().rename(&path_of(from, from_len), &path_of(to, to_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_chmod(path: u32, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().chmod(&path_of(path, path_len), mode))
}

#[no_mangle]
pub extern "C" fn k_fs_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().symlink(&path_of(target, target_len), &path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_link(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    as_unit(rootfs().link(&path_of(from, from_len), &path_of(to, to_len)))
}

/// Put a socket node at a path, so the name can be *seen* as well as
/// connected to.
///
/// Xvfb binds `/tmp/.X11-unix/X0`, and the script that starts it waits for
/// that path to exist before it runs a client. Without the node the server
/// was listening, the client was never started, and the run idled with three
/// processes and nothing to say why.
#[no_mangle]
pub extern "C" fn k_fs_mksocket(path: u32, path_len: u32) -> i32 {
    rootfs().add_socket(&path_of(path, path_len));
    0
}

/// Put a file into the shared root before anything runs.
///
/// How a page's bundle gets in: it is the host's, not a guest's, and it
/// arrives once rather than through a process that would then be the only one
/// able to see it.
#[no_mangle]
pub extern "C" fn k_fs_add(path: u32, path_len: u32, at: u32, len: u32) -> i32 {
    let p = path_of(path, path_len);
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    rootfs().add_file(&p, buf);
    0
}

/// Whether the shared root has anything in it yet.
///
/// The first Worker fills it and the rest must not: they would each unpack
/// the same rootfs over the top, at some cost and to no effect.
#[no_mangle]
pub extern "C" fn k_fs_populated() -> i32 {
    POPULATED.load(std::sync::atomic::Ordering::Acquire) as i32
}

/// Say the root is filled, so no other Worker fills it again.
#[no_mangle]
pub extern "C" fn k_fs_populate_done() {
    POPULATED.store(true, std::sync::atomic::Ordering::Release);
}

static POPULATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// -- unix sockets ------------------------------------------------------------
//
// The fourth thing processes share, and the one an X server and its client
// are made of. A socket's two ends are by definition in two processes, and
// with a Worker per process those are two memories -- so the name table one
// binds in is invisible to the other. The gears demo sat at "listening on
// /tmp/.X11-unix/X0, 0 waiting" forever, with the client in a third Worker
// looking somewhere else entirely.
//
// Both ends live here instead, where they are local to each other, and the
// implementation is `gex_kernel::unix` unchanged. Each process holds an id.

use gex_kernel::unix::{SocketNames, UnixSocket};

/// Every name a guest has bound, in the memory every Worker shares.
fn names() -> &'static SocketNames {
    note_panics();
    static NAMES: OnceLock<SocketNames> = OnceLock::new();
    NAMES.get_or_init(SocketNames::new)
}

/// Every socket end a process holds, by id.
type Socks = std::sync::Mutex<Vec<Option<std::sync::Arc<UnixSocket>>>>;
fn socks() -> &'static Socks {
    note_panics();
    static S: OnceLock<Socks> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn sock_at(id: i32) -> Option<std::sync::Arc<UnixSocket>> {
    socks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
        .flatten()
}

fn keep(s: std::sync::Arc<UnixSocket>) -> i32 {
    let mut t = socks().lock().unwrap_or_else(|e| e.into_inner());
    match t.iter().position(|e| e.is_none()) {
        Some(i) => {
            t[i] = Some(s);
            i as i32
        }
        None => {
            t.push(Some(s));
            (t.len() - 1) as i32
        }
    }
}

/// Bind and listen in one. Returns a listener id, or a negated errno.
///
/// One call because a remote socket has no state between `bind` and `listen`
/// worth modelling: what a guest does between them is local to its own
/// process, and what the other end can see is only whether the name answers.
#[no_mangle]
pub extern "C" fn k_sock_bind(path: u32, path_len: u32) -> i32 {
    let p = path_of_raw(path, path_len);
    let s = std::sync::Arc::new(UnixSocket::new(true));
    if let Err(e) = s.bind(&p) {
        return -e.0;
    }
    if let Err(e) = names().bind(&p, std::sync::Arc::clone(&s)) {
        return -e.0;
    }
    if let Err(e) = s.listen() {
        return -e.0;
    }
    keep(s)
}

/// Connect to a name. Returns this end's id, or a negated errno.
#[no_mangle]
pub extern "C" fn k_sock_connect(path: u32, path_len: u32) -> i32 {
    let p = path_of_raw(path, path_len);
    let s = std::sync::Arc::new(UnixSocket::new(true));
    match names().connect(&s, &p) {
        Ok(_) => keep(s),
        Err(e) => -e.0,
    }
}

/// Take a waiting connection. Returns the server end's id, or `-EAGAIN`.
#[no_mangle]
pub extern "C" fn k_sock_accept(listener: i32) -> i32 {
    let Some(l) = sock_at(listener) else { return -9 };
    match l.accept() {
        Ok(s) => keep(s),
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_sock_pending(listener: i32) -> i32 {
    sock_at(listener).is_some_and(|s| s.has_pending()) as i32
}

/// Read into the caller's memory. The bytes cross by `memory.copy`, which is
/// what makes two ends in two address spaces possible at all.
#[no_mangle]
pub extern "C" fn k_sock_read(id: i32, at: u32, len: u32) -> i32 {
    let Some(s) = sock_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    match s.recv(&mut buf, 0, false) {
        Ok(n) => {
            if n > 0 {
                unsafe { gex_kmem::push(at, buf.as_ptr(), n as u32) };
            }
            n as i32
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_sock_write(id: i32, at: u32, len: u32) -> i32 {
    let Some(s) = sock_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    match s.send(&buf) {
        Ok(n) => n as i32,
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_sock_readable(id: i32) -> i32 {
    use gex_kernel::vfs::FileHandle;
    sock_at(id).is_some_and(|s| s.poll_readable()) as i32
}

#[no_mangle]
pub extern "C" fn k_sock_writable(id: i32) -> i32 {
    use gex_kernel::vfs::FileHandle;
    sock_at(id).is_some_and(|s| s.poll_writable()) as i32
}

#[no_mangle]
pub extern "C" fn k_sock_available(id: i32) -> i32 {
    use gex_kernel::vfs::FileHandle;
    sock_at(id).map_or(0, |s| s.available().unwrap_or(0) as i32)
}

/// Drop an end. The last reference going is what the far end reads as an end
/// of file, so a process whose Worker exits still releases its peer.
#[no_mangle]
pub extern "C" fn k_sock_close(id: i32) {
    let mut t = socks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = t.get_mut(id as usize) {
        *slot = None;
    }
}

#[no_mangle]
pub extern "C" fn k_sock_shutdown(id: i32) {
    if let Some(s) = sock_at(id) {
        s.shutdown();
    }
}

/// Whether anything is listening on a name -- which is what a client that
/// looks before it leaps is asking. Xlib does; Mesa does not.
#[no_mangle]
pub extern "C" fn k_sock_exists(path: u32, path_len: u32) -> i32 {
    names().exists(&path_of_raw(path, path_len)) as i32
}

/// A path exactly as the caller wrote it.
///
/// Unlike a filesystem path, a socket name is compared for equality and never
/// looked up, so the leading slash has to survive: a server that binds
/// `/tmp/.X11-unix/X0` and a client that connects to the same string must
/// agree, and trimming one of them would make two different names.
fn path_of_raw(at: u32, len: u32) -> String {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// -- the GPU's session numbers -----------------------------------------------
//
// There is one renderer and one canvas, and every guest process that draws is
// a client of it. The number identifying a client is the renderer's own
// context id, so two processes must never pick the same one -- and each
// process builds its own device object, so scanning a local table gave both
// of them zero. What that looked like was two guests announcing "session 1"
// and the renderer refusing the stream: a second client had claimed the
// first's resources, which is corruption rather than contention.
//
// So the numbers come from here, where there is one of them.

/// Which client numbers the renderer has out. Sixteen, matching the shim's
/// `MAX_SESSIONS` -- the renderer keeps a fixed table, so a long run that
/// opens and closes windows gives numbers back rather than counting upwards.
static VIRGL_TAKEN: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

/// Claim a session number, or -1 if the renderer has no room.
#[no_mangle]
pub extern "C" fn k_virgl_take() -> i32 {
    note_panics();
    let mut bits = VIRGL_TAKEN.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..16u32 {
        if *bits & (1 << i) == 0 {
            *bits |= 1 << i;
            return i as i32;
        }
    }
    -1
}

/// Give one back, when the client's socket closes.
#[no_mangle]
pub extern "C" fn k_virgl_release(id: i32) {
    note_panics();
    if (0..16).contains(&id) {
        *VIRGL_TAKEN.lock().unwrap_or_else(|e| e.into_inner()) &= !(1u32 << id);
    }
}
