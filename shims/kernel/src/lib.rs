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

use std::collections::{BTreeMap, BTreeSet};
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

/// Standard input, in the memory every Worker shares.
///
/// It used to be a queue in each guest Worker's own shim, filled by a message
/// from the host: the Worker asked for input, gave the wasm stack back, and
/// the answer arrived as a `postMessage`. That is why a guest waiting for a
/// keystroke could not be parked -- a Worker asleep inside wasm is not
/// running the event loop the answer arrives on -- and why every wait had to
/// be classified into "the page ends this" and "another Worker ends this".
///
/// Here instead, so whoever has the keystroke writes it and calls
/// [`k_wake`], and a parked reader is woken by the same counter a pipe write
/// would have bumped. No classification, and nothing to misclassify.
fn stdin() -> &'static Mutex<(std::collections::VecDeque<u8>, bool)> {
    note_panics();
    static Q: OnceLock<Mutex<(std::collections::VecDeque<u8>, bool)>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new((std::collections::VecDeque::new(), false)))
}

/// Append what was typed, from the caller's process memory.
#[no_mangle]
pub extern "C" fn k_stdin_write(at: u32, len: u32) -> i32 {
    let mut buf = vec![0u8; len as usize];
    // SAFETY: the same copy every pipe write makes, and `pull_checked`
    // refuses a range outside the process memory rather than reading it.
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    let mut s = stdin().lock().unwrap_or_else(|e| e.into_inner());
    s.0.extend(buf);
    len as i32
}

/// There will be no more input.
#[no_mangle]
pub extern "C" fn k_stdin_eof() {
    stdin().lock().unwrap_or_else(|e| e.into_inner()).1 = true;
}

/// Bytes waiting, or -1 once there will be no more and none are left.
#[no_mangle]
pub extern "C" fn k_stdin_ready() -> i32 {
    let s = stdin().lock().unwrap_or_else(|e| e.into_inner());
    if s.0.is_empty() && s.1 {
        return -1;
    }
    s.0.len() as i32
}

/// Take up to `len` bytes into the caller's process memory.
#[no_mangle]
pub extern "C" fn k_stdin_read(at: u32, len: u32) -> i32 {
    let mut s = stdin().lock().unwrap_or_else(|e| e.into_inner());
    let n = (len as usize).min(s.0.len());
    if n == 0 {
        return if s.1 { 0 } else { -11 }; // EOF, or EAGAIN
    }
    let bytes: Vec<u8> = s.0.drain(..n).collect();
    // SAFETY: as above, in the other direction.
    unsafe { gex_kmem::push_checked(at, bytes.as_ptr(), n as u32) };
    n as i32
}

/// The system-wide slot allocation mark.
///
/// Dispatch slots have to be unique across *processes*, not just within one,
/// because a module translated by one Worker can be installed in another. A
/// page table that allocates from its own counter cannot give that: two
/// processes running the same program start from the same mark -- the one
/// their image was built with -- and issue the same numbers for different
/// code.
///
/// So runtime translation draws from here instead. Ranges are handed out in
/// chunks and never returned; the space is 8 tables of 9,999,360 slots and a
/// page costs 1,024, which is 78,120 pages before it is spent.
/// Where runtime-translated slots start: the first slot of table 4.
///
/// Tables 0..3 belong to statically recompiled closures, whose slot numbers
/// are baked into their modules at build time and start at zero. Tables 4..7
/// belong to whatever the run translates.
///
/// The two must not share a table, and not because they would collide -- the
/// counter used to be advanced past the closure's reservation, which stopped
/// that. It is because a wasm table is per *instance*, so it is grown per
/// guest process, and growing to slot N means allocating N entries. With one
/// shared space, the first block a process lifted took a slot above the whole
/// closure's 2.85M and the table had to be that long: 405ms and 22 MB in
/// every process, a hundred of them in an `apt-get update`, whether or not
/// the process ever entered a single closure library.
///
/// Split, a process grows table 0 for the libraries it actually reaches and
/// table 4 for the handful of blocks it lifts.
const RUNTIME_SLOT_BASE: u64 =
    4 * ((gex_abi::DISPATCH_TABLE_MAX_SLOTS / gex_abi::SLOTS_PER_PAGE) * gex_abi::SLOTS_PER_PAGE);

fn slot_mark() -> &'static core::sync::atomic::AtomicU64 {
    static MARK: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(RUNTIME_SLOT_BASE);
    &MARK
}

/// Reserve `count` slots for the caller alone, returning the first.
///
/// The range is aligned so that it cannot straddle two dispatch tables: a
/// page's slots are written as a unit and reached from one base, so a base
/// near the end of a table would index past it.
#[no_mangle]
pub extern "C" fn k_slot_alloc(count: u64) -> u64 {
    // The same divisor the emitter uses to turn a slot number into a table
    // index and an offset: whole pages only, so the two agree about which
    // table a number is in.
    let per_table =
        (gex_abi::DISPATCH_TABLE_MAX_SLOTS / gex_abi::SLOTS_PER_PAGE) * gex_abi::SLOTS_PER_PAGE;
    let mut cur = slot_mark().load(core::sync::atomic::Ordering::SeqCst);
    loop {
        let mut base = cur;
        // Do not let a reservation cross a table boundary.
        if base / per_table != (base + count - 1) / per_table {
            base = (base / per_table + 1) * per_table;
        }
        match slot_mark().compare_exchange(
            cur,
            base + count,
            core::sync::atomic::Ordering::SeqCst,
            core::sync::atomic::Ordering::SeqCst,
        ) {
            Ok(_) => return base,
            Err(seen) => cur = seen,
        }
    }
}

/// Raise the mark so nothing is handed out below `floor`.
///
/// Statically recompiled images carry slot numbers assigned when they were
/// built, and those are the same in every process that runs the image -- which
/// is right, because it is the same code. The global counter has to start
/// above every such range, or a runtime allocation would land on one.
#[no_mangle]
pub extern "C" fn k_slot_floor(floor: u64) {
    slot_mark().fetch_max(floor, core::sync::atomic::Ordering::SeqCst);
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
    /// Which modules hold code for a page of a given object.
    ///
    /// Every one of them, not the first. A page is not translated once: the
    /// translator discovers what it can reach from the block that missed, so
    /// a page fills in over many units as the program takes new paths through
    /// it. Keeping only the first claim meant a process could install one
    /// unit's worth of a page and then had to translate the rest itself --
    /// which is why sharing recognised four hundred pages and served
    /// nineteen modules.
    by_page: BTreeMap<(u64, u64), Vec<u32>>,
    /// Which modules hold code for an *object*, whatever page it is on.
    ///
    /// Per page is the wrong grain to install at. A translation unit holds
    /// what was reachable from the block that faulted, so another process
    /// entering the same page at a different block installs the page's module
    /// and misses anyway -- measured, 361 of 381 recognised pages had every
    /// covering module already installed and still had to translate. Asking
    /// per object instead lets the second process to enter libc take
    /// everything the first one ever lifted of it, in one go.
    by_object: BTreeMap<u64, Vec<u32>>,
    /// Objects some process is translating right now.
    ///
    /// The store answers "who has already done this"; nothing answered "who
    /// is doing it". apt runs its ten processes at once in parallel Workers,
    /// so all of them reach `libapt` before any of them has published it,
    /// all of them find nothing, and all of them translate it: 3,381,984
    /// blocks in one `apt-get update` against 1,724,482 for the entire
    /// decodable text of every library it touches.
    translating: BTreeMap<u64, u32>,
    /// Objects somebody has already translated whole, and published.
    ///
    /// The claim above is released the moment its owner finishes, which makes
    /// it a guard against two processes doing the work *at the same time* and
    /// nothing else. The next process to start finds no claim, and lifts the
    /// whole object again -- so three `ls` in one page lift 78,000 blocks
    /// each while the store reports `hit=241 miss=0`, which is to say they
    /// find what was published and re-translate it anyway.
    ///
    /// Recorded separately from the claim because the two answer different
    /// questions: "is anyone doing this right now" and "has this ever been
    /// done". Only the second survives its owner exiting, and only the second
    /// is what a later process needs to know.
    translated: BTreeSet<u64>,
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
            by_object: BTreeMap::new(),
            translating: BTreeMap::new(),
            translated: BTreeSet::new(),
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
    unsafe { gex_kmem::pull_checked(wasm.as_mut_ptr(), ptr, len) };
    let mut raw = vec![0u8; dirs_len as usize];
    if dirs_len > 0 {
        unsafe { gex_kmem::pull_checked(raw.as_mut_ptr(), dirs_ptr, dirs_len) };
    }
    let mut dirs = Vec::with_capacity(raw.len() / 12);
    for c in raw.chunks_exact(12) {
        let page = u64::from_le_bytes(c[0..8].try_into().unwrap());
        let base = u32::from_le_bytes(c[8..12].try_into().unwrap());
        dirs.push((page, base));
    }
    m.total += len as usize;
    m.mods.push(SharedModule { wasm, slots, dirs });
    // One-based. Zero is how every caller says "no module", so a module at
    // index zero would be stored, claimed for nothing, and never found.
    m.mods.len() as u32
}

/// Say that module `id` holds the code for page `page` of object `key`.
#[no_mangle]
pub extern "C" fn k_mod_claim(page: u64, key: u64, id: u32) {
    if id == 0 {
        return;
    }
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let claims = m.by_page.entry((page, key)).or_default();
    if !claims.contains(&id) {
        claims.push(id);
    }
}

/// Say that module `id` holds code somewhere in object `obj`.
#[no_mangle]
pub extern "C" fn k_mod_claim_object(obj: u64, id: u32) {
    if id == 0 {
        return;
    }
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let claims = m.by_object.entry(obj).or_default();
    if claims.last() != Some(&id) {
        claims.push(id);
    }
}

/// Claim an object to translate, or find out who already has.
///
/// Returns 1 to the process that may go ahead and 0 to everyone else, which
/// is a claim rather than a lock: the loser does not wait, it returns to the
/// miss path and lifts the handful of blocks it actually needs, and it picks
/// up the whole object from the store once the winner publishes. Waiting
/// would be the other design and it trades duplicated work for a stall on
/// whichever process happens to be slowest.
#[no_mangle]
pub extern "C" fn k_obj_translating(obj: u64, pid: u32) -> u32 {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    // Already done, by somebody who has since exited. Its modules are in the
    // store and the miss path will serve them; lifting the object again
    // produces the same code and throws the first copy away.
    if m.translated.contains(&obj) {
        return 0;
    }
    match m.translating.get(&obj) {
        Some(&who) if who != pid => 0,
        _ => {
            m.translating.insert(obj, pid);
            1
        }
    }
}

/// Done with it. `produced` is how many modules came of it.
///
/// The claim is released either way, because a translation that produced
/// nothing -- an object over the size cap, a process that died holding it --
/// must not stop the next process from trying.
///
/// It is recorded as *done* only when something came of it. That is the part
/// that makes the next `ls` cheap: the object is in the store, the miss path
/// will serve it, and lifting it a second time produces identical code and
/// discards the first copy.
#[no_mangle]
pub extern "C" fn k_obj_translated(obj: u64, produced: u32) {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    m.translating.remove(&obj);
    if produced > 0 {
        m.translated.insert(obj);
    }
}

/// Has this object been translated whole and published?
///
/// The miss path asks so it can take *all* of a finished object in one go
/// rather than sixteen units at a time. Sixteen is right for an object being
/// discovered a block at a time -- a stored unit holds only what was
/// reachable from the block that faulted in the process that built it, so
/// most of them do not contain the address this process wants. When the whole
/// object is there, that reasoning stops applying: everything is covered, and
/// the process should stop missing in it entirely.
#[no_mangle]
pub extern "C" fn k_obj_is_translated(obj: u64) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .translated
        .contains(&obj) as u32
}

/// The `n`th module holding code for this object, or zero past the end.
#[no_mangle]
pub extern "C" fn k_mod_for_object(obj: u64, n: u32) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_object
        .get(&obj)
        .and_then(|v| v.get(n as usize))
        .copied()
        .unwrap_or(0)
}

/// The `n`th module holding code for this page, or zero past the end.
///
/// The caller walks `n` upwards until it finds one it has not installed. A
/// page accumulates a claim per translation unit that touched it, so asking
/// only for the first would hand back whichever unit happened to reach the
/// page earliest and leave the rest of its blocks to be translated again.
#[no_mangle]
pub extern "C" fn k_mod_for(page: u64, key: u64, n: u32) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_page
        .get(&(page, key))
        .and_then(|v| v.get(n as usize))
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
        .get((id as usize).wrapping_sub(1))
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
        .get((id as usize).wrapping_sub(1))
        .map(|m| m.wasm.len() as u32)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn k_mod_dirs_len(id: u32) -> u32 {
    modules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mods
        .get((id as usize).wrapping_sub(1))
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
    let Some(sm) = m.mods.get((id as usize).wrapping_sub(1)) else {
        return 0;
    };
    if sm.wasm.len() > cap as usize {
        return 0;
    }
    unsafe { gex_kmem::push_checked(out, sm.wasm.as_ptr(), sm.wasm.len() as u32) };
    sm.wasm.len() as u32
}

/// And the `dirs`, in the same twelve-byte encoding they arrived in.
///
/// SAFETY: as above.
#[no_mangle]
pub unsafe extern "C" fn k_mod_dirs_read(id: u32, out: u32, cap: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sm) = m.mods.get((id as usize).wrapping_sub(1)) else {
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
    unsafe { gex_kmem::push_checked(out, buf.as_ptr(), n as u32) };
    n as u32
}

/// Where each shared object goes, the same in every process.
///
/// A translated block has its guest addresses baked into it, so a module is
/// only worth sharing between two processes if the object it was lifted from
/// sits at the same address in both. Nothing about the guest requires that
/// they differ -- `ld.so` reserves each object with `mmap(NULL, ..)` and
/// takes whatever it is handed -- but the per-process bump allocator gives a
/// different answer depending on what was mapped before, so the same libc
/// lands at `0x6010000` in one process and `0x6060000` in the next, and the
/// two translations have nothing in common.
///
/// This is the assignment, made once and kept system-wide. First to ask for
/// an object fixes it; everyone after gets the same base.
struct Places {
    /// By a hash of the guest path, because the caller is a different wasm
    /// memory and a scalar crosses that boundary without a copy.
    by_obj: BTreeMap<u64, u64>,
    /// The bottom of the unassigned region.
    next: u64,
}

/// The window objects are placed in: `gex-abi`'s, because every process's
/// arena is arranged around it and the two have to be the same number.
use gex_abi::{PLACE_BOTTOM, PLACE_TOP};

/// Enough that two objects never share a 64 KiB page, and the slack after
/// each one absorbs the alignment `ld.so` asks for on top of the span.
const PLACE_ALIGN: u64 = 0x1_0000;

fn places() -> &'static Mutex<Places> {
    note_panics();
    static PLACES: OnceLock<Mutex<Places>> = OnceLock::new();
    PLACES.get_or_init(|| {
        Mutex::new(Places {
            by_obj: BTreeMap::new(),
            next: PLACE_BOTTOM,
        })
    })
}

/// Where object `key` goes, assigning it a home if it has none. Zero when
/// the window is full, which means "no opinion" and the caller allocates as
/// it did before.
#[no_mangle]
pub extern "C" fn k_place(key: u64, len: u64) -> u64 {
    let mut p = places().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&base) = p.by_obj.get(&key) {
        return base;
    }
    let base = p.next.next_multiple_of(PLACE_ALIGN);
    // The span, rounded up, and a page of slack after it: `ld.so` reserves
    // the span *plus* whatever alignment the object's segments ask for, and
    // a reservation that runs into the next object's home would be refused
    // as not free.
    let Some(end) = len
        .checked_next_multiple_of(PLACE_ALIGN)
        .and_then(|n| base.checked_add(n))
        .and_then(|e| e.checked_add(PLACE_ALIGN))
    else {
        return 0;
    };
    if end > PLACE_TOP {
        return 0;
    }
    p.next = end;
    p.by_obj.insert(key, base);
    base
}

/// Fix where an object goes, rather than letting the counter choose.
///
/// For a closure that was statically recompiled: every module in it has the
/// addresses of the object it came from baked in, so the layout is not a
/// preference but a precondition. The build writes it out and the host
/// replays it here before anything runs.
///
/// The counter is pushed past whatever is pinned, so an object the build did
/// not cover -- one the guest `dlopen`s, say -- is placed after the pinned
/// ones rather than on top of them.
#[no_mangle]
pub extern "C" fn k_place_set(key: u64, base: u64, len: u64) {
    let mut p = places().lock().unwrap_or_else(|e| e.into_inner());
    p.by_obj.insert(key, base);
    let end = base
        .saturating_add(len.next_multiple_of(PLACE_ALIGN))
        .saturating_add(PLACE_ALIGN);
    if p.next < end {
        p.next = end;
    }
}

/// Linked address spaces, keyed by a hash of the program and the environment
/// that was linked for it.
///
/// A dynamically linked program spends its first half-second running the
/// guest's `ld.so`, which maps its libraries, hashes their symbols and
/// applies their relocations -- and reaches, every time, the same answer,
/// because a closure pins every object at an address decided at build time.
/// The first process of a program pays for it and leaves the result here;
/// every process after it starts from the result.
///
/// A fixed arena and a fixed index, with no allocation anywhere on the path.
/// The first version kept a `BTreeMap<u64, Vec<u8>>` and let the megabyte and
/// a half come from the kernel's heap, and storing one image from a child
/// killed the *parent* -- a wasm trap in a Worker that had not called this at
/// all. Every Worker instantiates this module against one shared memory, so
/// they share its allocator's state as well as its data, and a heap grown
/// from two Workers at once is not a heap either of them can trust. Nothing
/// here allocates: the bytes go at a bump offset in `.bss` and the index is
/// an array.
const SNAP_ARENA_BYTES: usize = 48 * 1024 * 1024;
const SNAP_MAX: usize = 64;

static mut SNAP_ARENA: [u8; SNAP_ARENA_BYTES] = [0; SNAP_ARENA_BYTES];

/// `(key, offset, len)` for each image kept, and how much of the arena is
/// spent. Zero-keyed entries are free.
struct Snaps {
    at: [(u64, u32, u32); SNAP_MAX],
    used: usize,
    n: usize,
}

fn snaps() -> &'static Mutex<Snaps> {
    note_panics();
    static SNAPS: OnceLock<Mutex<Snaps>> = OnceLock::new();
    SNAPS.get_or_init(|| {
        Mutex::new(Snaps {
            at: [(0, 0, 0); SNAP_MAX],
            used: 0,
            n: 0,
        })
    })
}

/// Keep the linked image of `key`. Silently declines when the arena is full
/// or the key is already known: the first process to finish linking wins, and
/// a second one arriving with the same answer has nothing to add.
///
/// SAFETY: `ptr` names the calling process's memory, not the kernel's, so
/// the bytes are pulled across rather than read. See `k_mod_store`.
#[no_mangle]
pub unsafe extern "C" fn k_snap_store(key: u64, ptr: u32, len: u32) {
    let mut s = snaps().lock().unwrap_or_else(|e| e.into_inner());
    if key == 0 || len == 0 || s.n == SNAP_MAX {
        return;
    }
    if s.at[..s.n].iter().any(|(k, _, _)| *k == key) {
        return;
    }
    let at = s.used;
    let Some(end) = at.checked_add(len as usize) else { return };
    if end > SNAP_ARENA_BYTES {
        return;
    }
    // SAFETY: the range is inside the arena, and the lock this holds is the
    // only thing that hands out arena offsets.
    let dst = unsafe { (&raw mut SNAP_ARENA).cast::<u8>().add(at) };
    if !unsafe { gex_kmem::pull_checked(dst, ptr, len) } {
        return;
    }
    s.used = end;
    let n = s.n;
    s.at[n] = (key, at as u32, len);
    s.n = n + 1;
}

/// How many bytes the linked image of `key` takes, or zero if there is none.
#[no_mangle]
pub extern "C" fn k_snap_len(key: u64) -> u32 {
    let s = snaps().lock().unwrap_or_else(|e| e.into_inner());
    s.at[..s.n]
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, _, len)| *len)
        .unwrap_or(0)
}

/// Copy the linked image of `key` into the caller's memory. Returns how many
/// bytes were written.
#[no_mangle]
pub unsafe extern "C" fn k_snap_get(key: u64, at: u32, cap: u32) -> u32 {
    let s = snaps().lock().unwrap_or_else(|e| e.into_inner());
    let Some(&(_, off, len)) = s.at[..s.n].iter().find(|(k, _, _)| *k == key) else {
        return 0;
    };
    if len > cap {
        return 0;
    }
    // SAFETY: the range was written by `k_snap_store` under this same lock
    // and nothing ever moves or frees it.
    let src = unsafe { (&raw const SNAP_ARENA).cast::<u8>().add(off as usize) };
    if !unsafe { gex_kmem::push_checked(at, src, len) } {
        return 0;
    }
    len
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

/// Which pipe ends each process still holds, so a process that dies without
/// closing them does not hold a reader's end-of-file for ever.
///
/// `exit_process` clears a process's descriptor table, and `RemotePipeEnd`'s
/// `Drop` is what tells this module an end has gone. Neither runs when a
/// guest leaves by any route other than `exit_group`: a trap does not unwind
/// and a terminated Worker drops nothing. `dpkg` trapped out inside `openat`
/// with no resume address, so `apt` sat on `read` waiting for an end-of-file
/// that nothing was ever going to send, and an `apt-get update` deadlocked
/// two processes in.
///
/// A real kernel closes a dead process's descriptors however it died. This is
/// where that has to happen, because by then the process's own memory -- and
/// the table in it -- belongs to a Worker that is gone.
fn pipe_holders() -> &'static std::sync::Mutex<BTreeMap<i32, Vec<(i32, bool)>>> {
    note_panics();
    static HELD: OnceLock<std::sync::Mutex<BTreeMap<i32, Vec<(i32, bool)>>>> = OnceLock::new();
    HELD.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Close every pipe end `pid` still holds. Called when a process ends, by
/// whatever route.
#[no_mangle]
pub extern "C" fn k_pipe_forget(pid: i32) -> u32 {
    let held = {
        let mut h = pipe_holders().lock().unwrap_or_else(|e| e.into_inner());
        h.remove(&pid).unwrap_or_default()
    };
    let n = held.len() as u32;
    for (id, write) in held {
        if let Some(p) = pipe_at(id) {
            p.close_end(write);
        }
    }
    n
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
pub extern "C" fn k_pipe_open_end(id: i32, write: i32, pid: i32) {
    if let Some(p) = pipe_at(id) {
        p.open_end(write != 0);
        let mut h = pipe_holders().lock().unwrap_or_else(|e| e.into_inner());
        h.entry(pid).or_default().push((id, write != 0));
    }
}

/// Drop a descriptor. The last one on an end is what a reader sees as EOF.
#[no_mangle]
pub extern "C" fn k_pipe_close_end(id: i32, write: i32, pid: i32) {
    if let Some(p) = pipe_at(id) {
        p.close_end(write != 0);
        let mut h = pipe_holders().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = h.get_mut(&pid) {
            if let Some(i) = v.iter().position(|e| *e == (id, write != 0)) {
                v.swap_remove(i);
            }
        }
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
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
            unsafe { gex_kmem::push_checked(at, buf.as_ptr(), n as u32) };
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
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, n as u32) };
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
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
                unsafe { gex_kmem::push_checked(at, buf.as_ptr(), n as u32) };
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
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
            unsafe { gex_kmem::push_checked(at, b.as_ptr(), b.len() as u32) };
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
                unsafe { gex_kmem::push_checked(at, b.as_ptr(), b.len() as u32) };
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
            unsafe { gex_kmem::push_checked(at, b.as_ptr(), b.len() as u32) };
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
                unsafe { gex_kmem::push_checked(at, b.as_ptr(), b.len() as u32) };
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

/// A symlink's target is text, not a path.
///
/// `path_of` strips leading slashes, because a mount hands its filesystem a
/// relative path -- right for the name being created, wrong for what the
/// link says. `ln -s /var/cache/apt/archives/x.deb d/00-x.deb` stored
/// `var/cache/apt/archives/x.deb`, which resolves against the link's own
/// directory and reaches nothing:
///
///     lrwxrwxrwx 00-x.deb -> var/cache/apt/archives/x.deb
///     ls: cannot access '/tmp/apt-dpkg-install-wiWg4U/00-x.deb'
///
/// which is where every `apt-get install` stopped. apt links the archives it
/// downloaded into a temporary directory by absolute path and hands dpkg the
/// directory; dpkg stats each name and reports "cannot stat pathname". The
/// link was there and pointed at nothing.
#[no_mangle]
pub extern "C" fn k_fs_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().symlink(&path_of_raw(target, target_len), &path_of(path, path_len)))
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
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
                unsafe { gex_kmem::push_checked(at, buf.as_ptr(), n as u32) };
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
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
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
