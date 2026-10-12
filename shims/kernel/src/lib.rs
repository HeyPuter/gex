// `memory.atomic.wait32` and `memory.atomic.notify` are still unstable in
// core; this module is nightly-only already, for the same reason.
#![feature(stdarch_wasm_atomic_wait)]
//! The global kernel module.
//!
//! One module, one memory, shared by every Worker of every guest process. The
//! state processes share -- the process table, filesystem, pipes, sockets --
//! lives *here*, so a syscall stays a direct call rather than a message and a
//! wait. This is what let the snapshot switcher go: each process now has its
//! own memory instead of one shared address space, so shared state needs
//! somewhere to live that is not one process's memory.
//!
//! A guest buffer is in the *process's* memory, which this module imports as
//! a second memory and cannot address with a pointer; `gex-kmem` copies
//! across the two with `memory.copy`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use gex_kernel::proc::ProcTable;

/// Stacks, one per Worker. Every Worker instantiates this module and shares its
/// memory but has its own `__stack_pointer`, all initialised to the same
/// address by wasm-ld -- so without a distinct slice each, two Workers here at
/// once overwrite each other's locals. Each gets a slice and points its stack
/// pointer at the top. In `.bss`, so it costs address space until written.
pub const STACK_BYTES: u32 = 256 * 1024;
/// Bounds Workers running *at once* (a slice is handed back when one ends).
/// `.bss` counts towards the module's declared initial memory, so each slice is
/// memory a host must provide before the kernel links. 32 was too few once
/// threads existed.
pub const STACK_COUNT: u32 = 128;
static mut STACKS: [u8; (STACK_BYTES * STACK_COUNT) as usize] =
    [0; (STACK_BYTES * STACK_COUNT) as usize];

/// Where the stacks start, and how big each is. Asked by the first Worker (the
/// only one that safely can); the host then assigns every later Worker a slice.
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

/// The word every Worker waits on and bumps, so a blocked guest is woken by a
/// wake rather than by JS polling once a millisecond. It lives here because
/// `memory.atomic.wait32`/`notify` act on the addressed memory, and only this
/// module's is shared across every process; a word in a process's memory is
/// visible only to that process's own threads.
///
/// A counter, not a flag, so a wake between deciding to park and parking is not
/// lost: the parker samples the value and sleeps only while it is unchanged.
static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn wake_addr() -> *mut i32 {
    &WAKE as *const std::sync::atomic::AtomicI32 as *mut i32
}

/// Where the counter is in this module's memory: a Worker that must keep
/// its event loop running (a network wait) waits on it with
/// `Atomics.waitAsync` instead of polling (gex.mjs `#netWait`).
#[no_mangle]
pub extern "C" fn k_wake_addr() -> u32 {
    wake_addr() as u32
}

/// What the counter reads now. Sampled before a Worker decides to park.
#[no_mangle]
pub extern "C" fn k_wake_seq() -> i32 {
    WAKE.load(std::sync::atomic::Ordering::Acquire)
}

/// Something happened that another Worker may have been waiting for. Called
/// after every syscall, not at hand-picked spots: missing one wake-up strands a
/// process, and a notify with nobody waiting costs a few nanoseconds.
#[no_mangle]
pub extern "C" fn k_wake() {
    WAKE.fetch_add(1, std::sync::atomic::Ordering::Release);
    // SAFETY: the address of a static in this module's own shared memory,
    // which is what this instruction requires.
    unsafe {
        core::arch::wasm32::memory_atomic_notify(wake_addr(), u32::MAX);
    }
}

/// Sleep until the counter leaves `seq`, or `ns` nanoseconds pass. Returns 0
/// woken, 1 value already moved, 2 timed out. A bounded wait keeps a Worker
/// answerable to the page.
#[no_mangle]
pub extern "C" fn k_park(seq: i32, ns: i64) -> i32 {
    // SAFETY: as above. Blocking is allowed because every guest runs in a
    // Worker; the page's own thread never enters this module.
    unsafe { core::arch::wasm32::memory_atomic_wait32(wake_addr(), seq, ns) }
}

/// Per-address futex wake generations, shared across every Worker.
///
/// The global [`WAKE`] counter says "somebody did something" and makes every
/// parked Worker re-check; this says *which futex* was woken, precisely. A
/// Worker parking on a futex records the address's generation here, and wakes
/// only when it changes -- a real wake ALWAYS bumps it, whatever the futex word
/// does afterward, so this has none of the ambiguity of guessing from the word
/// (which a heavily-contended condvar counter can leave back at the waited-for
/// value before the sleeper is scheduled, stranding a whole thread pool). It is
/// a per-Worker `FutexTable` writ shared: those generations cannot cross a
/// Worker boundary, and these must.
///
/// Direct-mapped, not a map: a collision (two addresses to one slot) only wakes
/// a waiter that then re-checks its own word and re-parks -- an extra turn, not
/// a wrong answer. 16384 slots is 64 KiB of `.bss`.
const FUTEX_SLOTS: usize = 16384;
static FUTEX_GENS: [std::sync::atomic::AtomicU32; FUTEX_SLOTS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; FUTEX_SLOTS];

fn futex_slot(addr: u64) -> usize {
    // Futex words are 4-byte aligned; mix the high bits down so neighbouring
    // locks (a mutex and the condvar beside it) do not share a slot.
    let a = addr >> 2;
    (((a ^ (a >> 15)).wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 40) as usize & (FUTEX_SLOTS - 1)
}

/// A futex at `addr` was woken: bump its generation and wake every parked
/// Worker to re-check. Called by the browser host for every wake its per-Worker
/// `FutexTable` recorded -- a `FUTEX_WAKE`, a thread exit clearing its tid, any
/// of them.
#[no_mangle]
pub extern "C" fn k_futex_wake(addr: u64) {
    let slot = &FUTEX_GENS[futex_slot(addr)];
    slot.fetch_add(1, std::sync::atomic::Ordering::Release);
    // The Workers parked on this futex wait on its slot (`k_park_futex`), not
    // on `WAKE`: without this notify they slept out their whole bound (20 ms
    // in W3's `gex_park`) on every wake. That was Undertale's page regression
    // once d761d4d4 stopped other threads' deadlines from cutting it short.
    // SAFETY: the address of a static in this module's own shared memory.
    unsafe {
        core::arch::wasm32::memory_atomic_notify(
            slot as *const std::sync::atomic::AtomicU32 as *mut i32,
            u32::MAX,
        );
    }
    k_wake();
}

/// The generation of the futex at `addr`, sampled when a Worker parks on it and
/// compared on every re-check.
#[no_mangle]
pub extern "C" fn k_futex_gen(addr: u64) -> u32 {
    FUTEX_GENS[futex_slot(addr)].load(std::sync::atomic::Ordering::Acquire)
}

/// Sleep until the futex at `addr` is woken (its generation leaves `gen`), or
/// `ns` pass. Returns 0 woken, 1 the generation already moved, 2 timed out.
///
/// Unlike [`k_park`], which sleeps on the one global [`WAKE`] counter that every
/// syscall in every process bumps, this sleeps on just this futex's generation
/// slot -- so an idle condvar waiter (a GLib thread-pool worker, say) is not
/// woken by the blanket notify a busy sibling fires on every syscall. That
/// thundering herd made a multi-threaded GTK program crawl: N idle threads each
/// took the host lock to re-check on every one of another thread's syscalls. A
/// `FUTEX_WAKE` to this address (via `k_futex_wake`) still bumps the slot and
/// wakes it precisely; the caller passes a bounded `ns` so signals, deadlines
/// and page work -- which do not touch this slot -- are still answered in time.
#[no_mangle]
pub extern "C" fn k_park_futex(addr: u64, gen: u32, ns: i64) -> i32 {
    let slot = futex_slot(addr);
    let at = &FUTEX_GENS[slot] as *const std::sync::atomic::AtomicU32 as *mut i32;
    // SAFETY: the address of a static in this module's own shared memory, which
    // is what the wait instruction requires. `gen` is compared as the same bit
    // pattern the generation was stored with (a wrapping u32 counter).
    unsafe { core::arch::wasm32::memory_atomic_wait32(at, gen as i32, ns) }
}

/// What this module last panicked with. It has no console (it imports nothing
/// but two memories), so without this a panic unwound to a bare `unreachable`
/// with no message. The host reads it straight out of shared memory; see
/// `k_panic_ptr`.
static PANIC: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Install the hook, once. Called at the top of every export that could panic
/// -- which is all of them, since any can be the first.
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

/// The system's processes. A `static` here is already the same bytes for every
/// Worker, because this module's memory is the shared one -- that is the trick.
fn procs() -> &'static ProcTable {
    note_panics();
    static PROCS: OnceLock<ProcTable> = OnceLock::new();
    PROCS.get_or_init(ProcTable::new)
}

/// Which dispatch slots the code of a given identity was translated into. Keyed
/// by dispatch page and by what the bytes there *are* (file, offset, length),
/// so two processes mapping the same library at the same address agree without
/// knowing about each other. One system-wide map, so a second process reuses
/// the first's work; per-Worker it could not, and `hit=0` every time.
///
/// Base zero means absent -- a real base never is, since slot zero and above
/// are the miss stubs.
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

/// Record it, unless that page and identity are already claimed. First writer
/// wins: emitted code names whichever base it was given.
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

/// How many pages the slot map holds. A diagnostic to confirm the map is shared:
/// two processes each storing eight pages should both then see sixteen.
#[no_mangle]
pub extern "C" fn k_slot_len() -> u32 {
    slots().lock().unwrap_or_else(|e| e.into_inner()).len() as u32
}

/// Standard input, shared. Whoever has the keystroke writes it and calls
/// [`k_wake`], so a parked reader is woken by the same counter a pipe write
/// bumps -- rather than by a `postMessage` a Worker parked inside wasm cannot
/// receive.
fn stdin() -> &'static Mutex<(std::collections::VecDeque<u8>, bool)> {
    note_panics();
    static Q: OnceLock<Mutex<(std::collections::VecDeque<u8>, bool)>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new((std::collections::VecDeque::new(), false)))
}

/// Append what was typed, from the caller's process memory.
#[no_mangle]
pub extern "C" fn k_stdin_write(at: gex_kmem::PAddr, len: u32) -> i32 {
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
pub extern "C" fn k_stdin_read(at: gex_kmem::PAddr, len: u32) -> i32 {
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

/// The terminal, shared: one termios, window size, foreground group and line
/// discipline for the whole system. The page instantiates this module too, so
/// it reaches the discipline by a call rather than a message to a parked Worker.
fn tty() -> &'static gex_kernel::tty::Tty {
    note_panics();
    static T: OnceLock<gex_kernel::tty::Tty> = OnceLock::new();
    T.get_or_init(gex_kernel::tty::Tty::new)
}

/// What the terminal owes the screen: echoed keystrokes, not yet drawn.
fn echoed() -> &'static Mutex<std::collections::VecDeque<u8>> {
    static E: OnceLock<Mutex<std::collections::VecDeque<u8>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Who a `^C` reaches: the foreground process group. Before any `tcsetpgrp`
/// there is none, and the fallback is pid 1's group, not "everybody" -- init is
/// the shell here, and a shell without job control keeps its children in its
/// own group, so the running command is signalled with it.
fn fg_group() -> i32 {
    let fg = tty().fg();
    if fg > 0 {
        return fg;
    }
    procs()
        .pgid(gex_kernel::INIT_PID, gex_kernel::INIT_PID)
        .unwrap_or(gex_kernel::INIT_PID)
}

/// Keystrokes, through the line discipline. `^C` signalling the foreground
/// group is what could only be done here, next to the process table. Returns
/// how many bytes of echo are queued, so the caller knows whether to drain.
#[no_mangle]
pub extern "C" fn k_tty_input(at: gex_kmem::PAddr, len: u32) -> i32 {
    let mut buf = vec![0u8; len as usize];
    // SAFETY: the same copy `k_stdin_write` makes, and `pull_checked` refuses
    // a range outside the caller's memory rather than reading it.
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    let r = tty().input(&buf);
    {
        let mut s = stdin().lock().unwrap_or_else(|e| e.into_inner());
        // A signal character discards what was typed and not yet read, so the
        // abandoned half-line is not handed to the shell after the `^C`.
        if r.flush {
            s.0.clear();
        }
        s.0.extend(r.to_reader);
        if r.eof {
            s.1 = true;
        }
    }
    if !r.echo.is_empty() {
        echoed()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(r.echo);
    }
    for sig in r.signals {
        note_kill(0, -fg_group(), sig);
        let _ = procs().kill_group(gex_kernel::INIT_PID, fg_group(), sig);
    }
    // Wake a reader parked inside wasm, which runs no event loop.
    k_wake();
    echoed().lock().unwrap_or_else(|e| e.into_inner()).len() as i32
}

/// Take what the terminal should display, into the caller's memory.
#[no_mangle]
pub extern "C" fn k_tty_echo(at: gex_kmem::PAddr, cap: u32) -> i32 {
    let mut q = echoed().lock().unwrap_or_else(|e| e.into_inner());
    let n = (cap as usize).min(q.len());
    if n == 0 {
        return 0;
    }
    let bytes: Vec<u8> = q.drain(..n).collect();
    // SAFETY: as above, in the other direction.
    unsafe { gex_kmem::push_checked(at, bytes.as_ptr(), n as u32) };
    n as i32
}

#[no_mangle]
pub extern "C" fn k_tty_get_termios(at: gex_kmem::PAddr, len: u32) -> i32 {
    let t = tty().termios();
    let n = (len as usize).min(t.len());
    unsafe { gex_kmem::push_checked(at, t.as_ptr(), n as u32) };
    n as i32
}

#[no_mangle]
pub extern "C" fn k_tty_set_termios(at: gex_kmem::PAddr, len: u32) -> i32 {
    let mut buf = vec![0u8; (len as usize).min(60)];
    let n = buf.len() as u32;
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, n) };
    tty().set_termios(&buf);
    n as i32
}

#[no_mangle]
pub extern "C" fn k_tty_get_locked(at: gex_kmem::PAddr, len: u32) -> i32 {
    let t = tty().locked_termios();
    let n = (len as usize).min(t.len());
    unsafe { gex_kmem::push_checked(at, t.as_ptr(), n as u32) };
    n as i32
}

#[no_mangle]
pub extern "C" fn k_tty_set_locked(at: gex_kmem::PAddr, len: u32) -> i32 {
    let mut buf = vec![0u8; (len as usize).min(60)];
    let n = buf.len() as u32;
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, n) };
    tty().set_locked_termios(&buf);
    n as i32
}

/// `rows << 48 | cols << 32 | xpixel << 16 | ypixel`.
#[no_mangle]
pub extern "C" fn k_tty_winsize() -> u64 {
    gex_kernel::tty::pack_ws(tty().winsize())
}

/// Returns 1 when the size actually changed, which is when `SIGWINCH` is owed.
#[no_mangle]
pub extern "C" fn k_tty_set_winsize(packed: u64) -> i32 {
    tty().set_winsize(gex_kernel::tty::unpack_ws(packed)) as i32
}

/// Resize from the page, which also owes the foreground group its `SIGWINCH`.
/// Separate from [`k_tty_set_winsize`]: the page has no pid, so it signals as
/// init, where a guest's `TIOCSWINSZ` would signal from its own kernel.
#[no_mangle]
pub extern "C" fn k_tty_resize(packed: u64) -> i32 {
    if !tty().set_winsize(gex_kernel::tty::unpack_ws(packed)) {
        return 0;
    }
    k_tty_signal(gex_kernel::signal::sig::SIGWINCH);
    1
}

/// Signal the foreground process group, for a page with no pid of its own --
/// used for what the discipline does not own (there is no `c_cc` for "tab
/// closed").
#[no_mangle]
pub extern "C" fn k_tty_signal(signo: u32) -> i32 {
    if signo == 0 || signo > gex_kernel::signal::sig::MAX {
        return -22;
    }
    note_kill(0, -fg_group(), signo);
    let n = match procs().kill_group(gex_kernel::INIT_PID, fg_group(), signo) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    };
    k_wake();
    n
}

#[no_mangle]
pub extern "C" fn k_tty_fg() -> i32 {
    tty().fg()
}

#[no_mangle]
pub extern "C" fn k_tty_set_fg(pgrp: i32) {
    tty().set_fg(pgrp);
}

#[no_mangle]
pub extern "C" fn k_tty_sid() -> i32 {
    tty().sid()
}

#[no_mangle]
pub extern "C" fn k_tty_set_sid(sid: i32) {
    tty().set_sid(sid);
}

#[no_mangle]
pub extern "C" fn k_tty_ldisc() -> i32 {
    tty().ldisc()
}

#[no_mangle]
pub extern "C" fn k_tty_set_ldisc(n: i32) -> i32 {
    tty().set_ldisc(n) as i32
}

/// Exclusive use, soft carrier and stopped-by-`^S`, packed: three booleans
/// are not worth three imports each way. See `gex_kernel::tty::BIT_EXCL`.
#[no_mangle]
pub extern "C" fn k_tty_bits() -> i32 {
    use gex_kernel::tty::{BIT_EXCL, BIT_SOFTCAR, BIT_STOPPED};
    let t = tty();
    (t.exclusive() as i32) * BIT_EXCL
        + (t.softcar() as i32) * BIT_SOFTCAR
        + (t.stopped() as i32) * BIT_STOPPED
}

#[no_mangle]
pub extern "C" fn k_tty_set_bits(which: i32, on: i32) {
    use gex_kernel::tty::{BIT_EXCL, BIT_SOFTCAR};
    let t = tty();
    match which {
        BIT_EXCL => t.set_exclusive(on != 0),
        BIT_SOFTCAR => t.set_softcar(on != 0),
        _ => t.set_stopped(on != 0),
    }
}

/// `TCFLSH`: throw away what has been typed and not read. Returns how many
/// bytes went.
#[no_mangle]
pub extern "C" fn k_tty_flush(which: i32) -> i32 {
    if which == gex_kernel::tty::TCOFLUSH {
        return 0;
    }
    tty().flush_line();
    let mut s = stdin().lock().unwrap_or_else(|e| e.into_inner());
    let n = s.0.len();
    s.0.clear();
    n as i32
}

/// Bytes typed and not yet read, for `FIONREAD`.
#[no_mangle]
pub extern "C" fn k_tty_queued() -> i32 {
    stdin().lock().unwrap_or_else(|e| e.into_inner()).0.len() as i32
}

/// `TIOCSTI`: bytes into the input queue as though they had been typed. They
/// go through the discipline, because that is what "as though typed" means --
/// a `\n` pushed this way completes a line.
#[no_mangle]
pub extern "C" fn k_tty_push(at: gex_kmem::PAddr, len: u32) -> i32 {
    k_tty_input(at, len)
}

/// Where runtime-translated slots start: the first slot of table 4. Slots must
/// be unique across *processes* (a module one Worker translates can install in
/// another), so runtime translation draws from one system-wide mark.
///
/// Tables 0..3 hold statically recompiled closures, slot numbers baked in at
/// build time from zero; tables 4..7 hold what the run translates. They must
/// not share a table because a wasm table is per-instance and grown to slot N
/// means N entries: mixing them made every process grow table 0 past the whole
/// closure's 2.85M slots (405ms, 22 MB) whether or not it entered a closure
/// library. Split, a process grows table 0 for libraries it reaches and table 4
/// for the blocks it lifts.
const RUNTIME_SLOT_BASE: u64 =
    4 * ((gex_abi::DISPATCH_TABLE_MAX_SLOTS / gex_abi::SLOTS_PER_PAGE) * gex_abi::SLOTS_PER_PAGE);

fn slot_mark() -> &'static core::sync::atomic::AtomicU64 {
    static MARK: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(RUNTIME_SLOT_BASE);
    &MARK
}

/// Reserve `count` slots for the caller alone, returning the first. Aligned so
/// the range cannot straddle two dispatch tables (a page is reached from one
/// base).
#[no_mangle]
pub extern "C" fn k_slot_alloc(count: u64) -> u64 {
    // Whole pages only, matching the emitter's slot-to-table divisor.
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

/// Raise the mark so nothing is handed out below `floor` -- above every
/// statically recompiled image's baked-in slot range, or a runtime allocation
/// would land on one.
#[no_mangle]
pub extern "C" fn k_slot_floor(floor: u64) {
    slot_mark().fetch_max(floor, core::sync::atomic::Ordering::SeqCst);
}

/// Runtime slots nobody has claimed yet: from the mark to the top of the last
/// dispatch table.
///
/// There is no reclaiming a dead process's slots -- the shared slot map is what
/// lets the second process to run `ls` install what the first translated, so a
/// number handed out means the same thing for the page's whole life. That makes
/// this a budget, and one worth asking about before spending a large piece of
/// it: past the top, `PageTable::base` divides into a table index of
/// `DISPATCH_TABLES` and the emitted module imports a table that does not
/// exist. The engine then refuses it with "out of bounds table index 8", in
/// whichever forked child happened to be translating, and the run stops on a
/// message about wasm validation that says nothing about slots.
#[no_mangle]
pub extern "C" fn k_slot_left() -> u64 {
    let per_table =
        (gex_abi::DISPATCH_TABLE_MAX_SLOTS / gex_abi::SLOTS_PER_PAGE) * gex_abi::SLOTS_PER_PAGE;
    let top = gex_abi::DISPATCH_TABLES as u64 * per_table;
    top.saturating_sub(slot_mark().load(core::sync::atomic::Ordering::SeqCst))
}

/// How many pages the map answers for. A diagnostic.
#[no_mangle]
pub extern "C" fn k_slot_count() -> u32 {
    slots().lock().unwrap_or_else(|e| e.into_inner()).len() as u32
}

/// Every translated module, once, and which object's pages each one covers. The
/// slot map says *where* code went; a Worker also needs the module itself,
/// since a dispatch table belongs to a module instance. Shared here so the
/// second process to run `ls` installs what the first produced. Bytes, not
/// compiled code (that is a JS object): the Worker feeds them to the same
/// `pending` queue a fresh translation uses.
struct SharedModule {
    /// Where its bytes are in [`ModStore::slab`]: chunk, offset, length.
    at: (u32, u32, u32),
    /// How large the dispatch table must be. A fact about the *producing*
    /// process; the installer's own high-water mark understates it, and the
    /// install then throws.
    slots: u64,
    /// Which pages it holds code for, and at which base. The installing Worker
    /// must assign these first, or the blocks resolve to no slot and are dropped.
    dirs: Vec<(u64, u32)>,
}

struct ModStore {
    /// By id. Index zero is never used: zero means "none".
    mods: Vec<SharedModule>,
    /// Every module holding code for a page of an object, not just the first: a
    /// page fills in over many translation units, so keeping only the first
    /// claim left a process translating the rest itself.
    by_page: BTreeMap<(u64, u64), Vec<u32>>,
    /// Every module holding code for an *object*, whatever page. Per-page is too
    /// fine: entering the same page at a different block still missed 361 of 381
    /// times. Per object, the second process to enter libc takes it all at once.
    by_object: BTreeMap<u64, Vec<u32>>,
    /// Objects some process is translating right now. Without this, apt's ten
    /// parallel processes all reach `libapt` before any publishes it and all
    /// translate it.
    translating: BTreeMap<u64, u32>,
    /// Objects translated whole and published. The claim above is released when
    /// its owner finishes, so it only guards against *concurrent* work; only
    /// this survives the owner exiting and tells a later process not to re-lift.
    translated: BTreeSet<u64>,
    /// What has been stored, against [`MOD_BUDGET`].
    total: usize,
    /// The modules' bytes, appended into chunks of [`SLAB_CHUNK`] (a larger
    /// module gets a chunk of its own). One allocation per module grew the
    /// kernel's shared memory every few modules -- dlmalloc asks `memory.grow`
    /// for little more than each request -- and growing a shared memory is
    /// slow: 1.9 s of a 40 s node start-up in the page's runtime.
    slab: Vec<Vec<u8>>,
}

const SLAB_CHUNK: usize = 16 << 20;

impl ModStore {
    fn bytes(&self, sm: &SharedModule) -> &[u8] {
        let (c, off, len) = sm.at;
        self.slab
            .get(c as usize)
            .and_then(|v| v.get(off as usize..off as usize + len as usize))
            .unwrap_or(&[])
    }

    /// Room for `len` bytes: the chunk and offset they go at (written by the caller).
    fn reserve(&mut self, len: usize) -> (u32, u32) {
        let fits = self.slab.last().is_some_and(|v| v.capacity() - v.len() >= len);
        if !fits {
            self.slab.push(Vec::with_capacity(len.max(SLAB_CHUNK)));
        }
        let c = self.slab.len() - 1;
        let v = &mut self.slab[c];
        let off = v.len();
        v.resize(off + len, 0);
        (c as u32, off as u32)
    }
}

/// How much translated wasm to keep for sharing. The kernel memory is capped at
/// 1 GiB with the filesystem in it too, so this cannot be unbounded; a quarter
/// covers the libraries a forking program shares. Running out falls back to
/// translating.
const MOD_BUDGET: usize = 256 << 20;

fn modules() -> &'static Mutex<ModStore> {
    note_panics();
    static MODS: OnceLock<Mutex<ModStore>> = OnceLock::new();
    MODS.get_or_init(|| {
        Mutex::new(ModStore {
            mods: vec![SharedModule {
                at: (u32::MAX, 0, 0),
                slots: 0,
                dirs: Vec::new(),
            }],
            by_page: BTreeMap::new(),
            by_object: BTreeMap::new(),
            translating: BTreeMap::new(),
            translated: BTreeSet::new(),
            total: 0,
            slab: Vec::new(),
        })
    })
}

/// Keep a module. Returns its id, or zero when the budget is spent. `dirs` is
/// `(u64 page, u32 base)` pairs, twelve bytes each, little-endian.
///
/// SAFETY: every pointer names the *calling process's* memory, a different wasm
/// memory, so the bytes must be pulled across rather than read directly.
#[no_mangle]
pub unsafe extern "C" fn k_mod_store(
    ptr: gex_kmem::PAddr,
    len: u32,
    slots: u64,
    dirs_ptr: gex_kmem::PAddr,
    dirs_len: u32,
) -> u32 {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    if len == 0 || m.total + len as usize > MOD_BUDGET {
        return 0;
    }
    let (c, off) = m.reserve(len as usize);
    let dst = m.slab[c as usize][off as usize..].as_mut_ptr();
    unsafe { gex_kmem::pull_checked(dst, ptr, len) };
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
    m.mods.push(SharedModule { at: (c, off, len), slots, dirs });
    // One-based: zero is how every caller says "no module".
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

/// Claim an object to translate, or find out who already has. Returns 1 to the
/// process that may go ahead, 0 to everyone else -- a claim, not a lock: the
/// loser lifts the few blocks it needs from the miss path and picks up the
/// whole object from the store once the winner publishes.
#[no_mangle]
pub extern "C" fn k_obj_translating(obj: u64, pid: u32) -> u32 {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    // Already done by someone since exited; the store serves its modules.
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

/// Done with it. `produced` is how many modules came of it. The claim is
/// released either way (so a translation that produced nothing does not block
/// the next process), but recorded as *done* only when something came of it.
#[no_mangle]
pub extern "C" fn k_obj_translated(obj: u64, produced: u32) {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    m.translating.remove(&obj);
    if produced > 0 {
        m.translated.insert(obj);
    }
}

/// Has this object been translated whole and published? The miss path asks so
/// it can take *all* of a finished object at once, rather than the sixteen
/// units at a time that suit an object still being discovered a block at a time.
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

/// The modules holding code for this object from the `from`th on, as
/// little-endian u32 ids into `out` (at most `cap` of them); how many. One
/// call where walking `k_mod_for_object` took a lock per module, on every
/// miss: a node start-up's libnode has thousands.
///
/// SAFETY: `out` must have room for `cap` u32s.
#[no_mangle]
pub unsafe extern "C" fn k_mod_ids_for_object(obj: u64, from: u32, out: gex_kmem::PAddr, cap: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let Some(v) = m.by_object.get(&obj) else {
        return 0;
    };
    let ids = v.get(from as usize..).unwrap_or(&[]);
    let n = ids.len().min(cap as usize);
    unsafe { gex_kmem::push_checked(out, ids.as_ptr() as *const u8, (n * 4) as u32) };
    n as u32
}

/// The `n`th module holding code for this page, or zero past the end. The
/// caller walks `n` upwards for every unit that touched the page, since one
/// page accumulates a claim per translation unit.
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
        .map(|m| m.at.2)
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
pub unsafe extern "C" fn k_mod_read(id: u32, out: gex_kmem::PAddr, cap: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sm) = m.mods.get((id as usize).wrapping_sub(1)) else {
        return 0;
    };
    let b = m.bytes(sm);
    if b.len() > cap as usize {
        return 0;
    }
    unsafe { gex_kmem::push_checked(out, b.as_ptr(), b.len() as u32) };
    b.len() as u32
}

// ---- The translation cache across page loads (docs/gexcc.md §8) ----
//
// The store above, its claims and the slot map, written out for the page to
// keep (`k_store_save`) and laid back in by the next load's first process
// before anything allocates a slot (`k_store_restore`). Keys are SipHash of a
// file path (and offset); each key is tied to its path, and each path to the
// size and mtime the file had, so what belongs to a file that is no longer
// the same is dropped: checked when a process first computes one of its keys
// (`k_key_path`), not at restore, when the file may not be there yet.

/// A path some key was computed from, as the file was then.
struct XPath {
    path: String,
    size: u64,
    mtime: i64,
}

#[derive(Default)]
struct XCache {
    paths: Vec<XPath>,
    path_ix: BTreeMap<String, u32>,
    keys: BTreeMap<u64, u32>,
    /// Modules in the store when it was last saved or restored: a save with
    /// nothing new since writes nothing.
    saved_mods: usize,
    saving: bool,
    out: Vec<u8>,
    /// A load's saved bytes.
    inbuf: Vec<u8>,
    /// Restored keys whose file no process has looked at yet: each is checked
    /// (size and mtime) the first time a process computes it (`k_key_path`),
    /// since the files are often not there at restore -- the page installs
    /// its packages after pid 1 starts.
    pending: BTreeSet<u64>,
    restored: bool,
    /// This load keeps a cache (`k_xcache_enable`): every process notes its
    /// keys' files, whichever process saves.
    enabled: bool,
}

fn xcache() -> &'static Mutex<XCache> {
    note_panics();
    static X: OnceLock<Mutex<XCache>> = OnceLock::new();
    X.get_or_init(|| Mutex::new(XCache::default()))
}

const XC_MAGIC: &[u8; 8] = b"GEXXC\x00\x00\x01";
/// Module bytes a save writes at most; past it, modules go as empty records
/// (ids stay positions).
const XC_MAX_BYTES: usize = MOD_BUDGET;

/// Key `key` was computed from `path`, whose file has `size` and `mtime`
/// (`size` `u64::MAX`: no file to check, so the key is never restored).
///
/// For a restored key not yet checked, also the check: the same size and
/// mtime confirm every restored key of that path (returns 1); anything else
/// drops their translations, claims and slots (returns -1, the path's
/// record then takes the file as it is now). Otherwise 0.
#[no_mangle]
pub extern "C" fn k_key_path(key: u64, path: gex_kmem::PAddr, len: u32, size: u64, mtime: i64) -> i32 {
    let p = path_of_raw(path, len);
    let mut x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    if x.pending.contains(&key) {
        // Again in `k_store_restore`'s order (modules, xcache, slots), which
        // only a check needs; another Worker may have done it meanwhile.
        drop(x);
        let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
        x = xcache().lock().unwrap_or_else(|e| e.into_inner());
        if !x.pending.contains(&key) {
            return 0;
        }
        let ix = x.keys[&key];
        let same = {
            let r = &x.paths[ix as usize];
            size != u64::MAX && r.path == p && r.size == size && r.mtime == mtime
        };
        let of_path: Vec<u64> = x.keys.iter().filter(|&(k, &i)| i == ix && x.pending.contains(k)).map(|(k, _)| *k).collect();
        for k in &of_path {
            x.pending.remove(k);
        }
        if same {
            return 1;
        }
        let mut sl = slots().lock().unwrap_or_else(|e| e.into_inner());
        let gone: BTreeSet<u64> = of_path.into_iter().collect();
        sl.retain(|(_, k), _| !gone.contains(k));
        let mut freed: Vec<u32> = Vec::new();
        m.by_page.retain(|(_, k), ids| {
            let keep = !gone.contains(k);
            if !keep {
                freed.extend(ids.iter().copied());
            }
            keep
        });
        for k in &gone {
            if let Some(ids) = m.by_object.remove(k) {
                freed.extend(ids);
            }
            m.translated.remove(k);
        }
        if !freed.is_empty() {
            let named: BTreeSet<u32> = m.by_page.values().chain(m.by_object.values()).flatten().copied().collect();
            for id in freed {
                if named.contains(&id) {
                    continue;
                }
                // Its bytes stay in the slab, unreachable (nothing is freed there).
                let Some(sm) = m.mods.get_mut((id as usize).wrapping_sub(1)) else { continue };
                let n = sm.at.2 as usize;
                sm.at = (u32::MAX, 0, 0);
                m.total -= n;
            }
        }
        let r = &mut x.paths[ix as usize];
        r.size = size;
        r.mtime = mtime;
        return -1;
    }
    if x.keys.contains_key(&key) {
        return 0;
    }
    let ix = match x.path_ix.get(&p) {
        Some(&i) => i,
        None => {
            let i = x.paths.len() as u32;
            x.paths.push(XPath { path: p.clone(), size, mtime });
            x.path_ix.insert(p, i);
            i
        }
    };
    x.keys.insert(key, ix);
    0
}

struct W(Vec<u8>);
impl W {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b)
    }
}

struct R<'a>(&'a [u8], usize);
impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let b = self.0.get(self.1..self.1.checked_add(n)?)?;
        self.1 += n;
        Some(b)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
}

/// Write the store out, if it has grown by `min_bytes` of modules since the
/// last save or restore; the length (read it at `k_store_save_ptr`, then
/// `k_store_save_done`), or zero for nothing to write or a save already
/// under way.
#[no_mangle]
pub extern "C" fn k_store_save(min_bytes: u32) -> u32 {
    let m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let mut x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    if x.saving || m.mods.len() <= x.saved_mods {
        return 0;
    }
    let fresh: usize = m.mods[x.saved_mods.max(1)..].iter().map(|sm| sm.at.2 as usize).sum();
    if fresh < min_bytes as usize {
        return 0;
    }
    let mut w = W(Vec::with_capacity(m.total.min(XC_MAX_BYTES) + (1 << 20)));
    w.0.extend_from_slice(XC_MAGIC);
    w.u32(x.paths.len() as u32);
    for p in &x.paths {
        w.bytes(p.path.as_bytes());
        w.u64(p.size);
        w.u64(p.mtime as u64);
    }
    w.u32(x.keys.len() as u32);
    for (k, i) in &x.keys {
        w.u64(*k);
        w.u32(*i);
    }
    {
        let sl = slots().lock().unwrap_or_else(|e| e.into_inner());
        w.u32(sl.len() as u32);
        for ((page, key), base) in sl.iter() {
            w.u64(*page);
            w.u64(*key);
            w.u32(*base);
        }
    }
    w.u64(slot_mark().load(core::sync::atomic::Ordering::SeqCst));
    w.u32((m.mods.len() - 1) as u32);
    let mut budget = XC_MAX_BYTES;
    for sm in &m.mods[1..] {
        w.u64(sm.slots);
        w.u32(sm.dirs.len() as u32);
        for (page, base) in &sm.dirs {
            w.u64(*page);
            w.u32(*base);
        }
        let wasm = m.bytes(sm);
        if wasm.len() <= budget {
            budget -= wasm.len();
            w.bytes(wasm);
        } else {
            w.bytes(&[]);
        }
    }
    w.u32(m.by_page.len() as u32);
    for ((page, key), ids) in &m.by_page {
        w.u64(*page);
        w.u64(*key);
        w.u32(ids.len() as u32);
        for id in ids {
            w.u32(*id);
        }
    }
    w.u32(m.by_object.len() as u32);
    for (obj, ids) in &m.by_object {
        w.u64(*obj);
        w.u32(ids.len() as u32);
        for id in ids {
            w.u32(*id);
        }
    }
    w.u32(m.translated.len() as u32);
    for obj in &m.translated {
        w.u64(*obj);
    }
    x.saved_mods = m.mods.len();
    x.saving = true;
    x.out = w.0;
    x.out.len() as u32
}

/// Where the bytes `k_store_save` wrote are, in this memory.
#[no_mangle]
pub extern "C" fn k_store_save_ptr() -> u32 {
    xcache().lock().unwrap_or_else(|e| e.into_inner()).out.as_ptr() as u32
}

/// The host has them.
#[no_mangle]
pub extern "C" fn k_store_save_done() {
    let mut x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    x.out = Vec::new();
    x.saving = false;
}

/// Room for a load's saved bytes, in this memory: the host copies them to the
/// address returned, then the first process restores (`k_store_restore`).
#[no_mangle]
pub extern "C" fn k_store_restore_buf(len: u32) -> u32 {
    let mut x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    x.inbuf = vec![0u8; len as usize];
    x.inbuf.as_ptr() as u32
}

/// This load keeps a cache: set by the host before its first process runs.
#[no_mangle]
pub extern "C" fn k_xcache_enable() {
    xcache().lock().unwrap_or_else(|e| e.into_inner()).enabled = true;
}

/// Whether processes note their keys' files (`k_key_path`): the load keeps a
/// cache, or restored one (whose keys are then checked on first use).
#[no_mangle]
pub extern "C" fn k_xcache_on() -> u32 {
    let x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    (x.enabled || x.restored) as u32
}

/// Lay the saved store in: slot map, mark, modules, claims, every key pending
/// its file's check (`k_key_path`). Only into an empty store with nothing
/// allocated, once. Returns the modules restored with bytes, or negative: -1
/// not a cache, -2 too late (something is in the store or the slot map, or a
/// slot was handed out), -3 restored before.
#[no_mangle]
pub extern "C" fn k_store_restore() -> i32 {
    let mut m = modules().lock().unwrap_or_else(|e| e.into_inner());
    let mut x = xcache().lock().unwrap_or_else(|e| e.into_inner());
    if x.restored {
        return -3;
    }
    let mut sl = slots().lock().unwrap_or_else(|e| e.into_inner());
    if m.mods.len() > 1 || !sl.is_empty() || slot_mark().load(core::sync::atomic::Ordering::SeqCst) != RUNTIME_SLOT_BASE {
        return -2;
    }
    x.restored = true;
    let inbuf = std::mem::take(&mut x.inbuf);
    let mut r = R(&inbuf, 0);
    let res = (|| -> Option<i32> {
        if r.take(8)? != XC_MAGIC {
            return None;
        }
        let n = r.u32()?;
        let mut in_paths = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let path = String::from_utf8(r.bytes()?).ok()?;
            let size = r.u64()?;
            let mtime = r.u64()? as i64;
            in_paths.push(XPath { path, size, mtime });
        }
        // Every key with a file to check (keys saved without one are left
        // out), pending that check.
        let mut good: BTreeSet<u64> = BTreeSet::new();
        let mut remap: BTreeMap<u32, u32> = BTreeMap::new();
        for _ in 0..r.u32()? {
            let k = r.u64()?;
            let i = r.u32()?;
            let p = in_paths.get(i as usize)?;
            if p.size == u64::MAX {
                continue;
            }
            good.insert(k);
            let ix = match remap.get(&i) {
                Some(&j) => j,
                None => {
                    let j = x.paths.len() as u32;
                    x.paths.push(XPath { path: p.path.clone(), size: p.size, mtime: p.mtime });
                    x.path_ix.insert(p.path.clone(), j);
                    remap.insert(i, j);
                    j
                }
            };
            x.keys.insert(k, ix);
            x.pending.insert(k);
        }
        for _ in 0..r.u32()? {
            let page = r.u64()?;
            let key = r.u64()?;
            let base = r.u32()?;
            if good.contains(&key) {
                sl.insert((page, key), base);
            }
        }
        let mark = r.u64()?;
        // Read the modules, then keep the bytes only of those a kept claim names.
        let nm = r.u32()?;
        let mut mods = Vec::with_capacity(nm as usize);
        for _ in 0..nm {
            let slots = r.u64()?;
            let nd = r.u32()?;
            let mut dirs = Vec::with_capacity(nd as usize);
            for _ in 0..nd {
                let page = r.u64()?;
                let base = r.u32()?;
                dirs.push((page, base));
            }
            // Where its bytes are in the saved file, copied into the slab below.
            let n = r.u32()? as usize;
            let at = r.1;
            r.take(n)?;
            mods.push((slots, dirs, at, n));
        }
        let mut wanted: BTreeSet<u32> = BTreeSet::new();
        let mut by_page = BTreeMap::new();
        for _ in 0..r.u32()? {
            let page = r.u64()?;
            let key = r.u64()?;
            let k = r.u32()?;
            let mut ids = Vec::with_capacity(k as usize);
            for _ in 0..k {
                ids.push(r.u32()?);
            }
            if good.contains(&key) {
                wanted.extend(ids.iter().copied());
                by_page.insert((page, key), ids);
            }
        }
        let mut by_object = BTreeMap::new();
        for _ in 0..r.u32()? {
            let obj = r.u64()?;
            let k = r.u32()?;
            let mut ids = Vec::with_capacity(k as usize);
            for _ in 0..k {
                ids.push(r.u32()?);
            }
            if good.contains(&obj) {
                wanted.extend(ids.iter().copied());
                by_object.insert(obj, ids);
            }
        }
        let mut translated = BTreeSet::new();
        for _ in 0..r.u32()? {
            let obj = r.u64()?;
            if good.contains(&obj) {
                translated.insert(obj);
            }
        }
        let mut kept = 0;
        for (i, (slots, dirs, at, n)) in mods.into_iter().enumerate() {
            // Store ids are one past the index (index zero is "none"); a module
            // no kept claim names comes back without bytes.
            let mut sm = SharedModule { at: (u32::MAX, 0, 0), slots, dirs };
            if n > 0 && wanted.contains(&(i as u32 + 2)) && m.total + n <= MOD_BUDGET {
                let (c, off) = m.reserve(n);
                m.slab[c as usize][off as usize..off as usize + n].copy_from_slice(&inbuf[at..at + n]);
                sm.at = (c, off, n as u32);
                m.total += n;
                kept += 1;
            }
            m.mods.push(sm);
        }
        m.by_page = by_page;
        m.by_object = by_object;
        m.translated = translated;
        slot_mark().fetch_max(mark, core::sync::atomic::Ordering::SeqCst);
        Some(kept)
    })();
    x.saved_mods = m.mods.len();
    match res {
        Some(k) => k,
        None => {
            // A torn file: start empty rather than half-restored.
            m.mods.truncate(1);
            m.slab.clear();
            m.by_page.clear();
            m.by_object.clear();
            m.translated.clear();
            m.total = 0;
            sl.clear();
            x.keys.clear();
            x.paths.clear();
            x.path_ix.clear();
            x.pending.clear();
            x.saved_mods = 1;
            -1
        }
    }
}

/// And the `dirs`, in the same twelve-byte encoding they arrived in.
///
/// SAFETY: as above.
#[no_mangle]
pub unsafe extern "C" fn k_mod_dirs_read(id: u32, out: gex_kmem::PAddr, cap: u32) -> u32 {
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

/// Where each shared object goes, the same base in every process. A translated
/// block bakes in guest addresses, so a module is only shareable if its object
/// sits at the same address in both processes -- which the per-process bump
/// allocator does not guarantee. Assigned once, system-wide: first to ask fixes
/// it, everyone after gets the same base.
struct Places {
    /// Keyed by a hash of the guest path (a scalar crosses the memory boundary
    /// without a copy).
    by_obj: BTreeMap<u64, u64>,
    /// The bottom of the unassigned region.
    next: u64,
}

/// The window objects are placed in: `gex-abi`'s, which every process's arena
/// is arranged around.
use gex_abi::{PLACE_BOTTOM, PLACE_TOP};

/// Enough that two objects never share a 64 KiB page; the slack after each
/// absorbs the alignment `ld.so` asks for on top of the span.
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
    // The span plus a page of slack: `ld.so` reserves the span *plus* its
    // segments' alignment, and a reservation into the next home would be refused.
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

/// Fix where an object goes, rather than letting the counter choose. For a
/// statically recompiled closure, whose modules bake in their object's address,
/// so the layout is a precondition. The counter is pushed past whatever is
/// pinned, so a later `dlopen`ed object lands after the pinned ones.
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

/// Linked address spaces, keyed by a hash of the program and its environment. A
/// dynamically linked program spends its first half-second in `ld.so` reaching
/// the same layout every time; the first process pays for it and leaves the
/// result here for the rest.
///
/// A fixed arena and index, allocating nothing: every Worker shares this
/// module's allocator state as well as its data, so a heap grown from two
/// Workers at once is untrustworthy -- storing from a child once trapped the
/// parent. The bytes go at a bump offset in `.bss` and the index is an array.
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

/// Keep the linked image of `key`. Silently declines when the arena is full or
/// the key is already known: first to finish linking wins.
///
/// SAFETY: `ptr` names the calling process's memory, pulled across not read. See
/// `k_mod_store`.
#[no_mangle]
pub unsafe extern "C" fn k_snap_store(key: u64, ptr: gex_kmem::PAddr, len: u32) {
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
pub unsafe extern "C" fn k_snap_get(key: u64, at: gex_kmem::PAddr, cap: u32) -> u32 {
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

/// Record that `pid` has exited. `status` is an exit code, or the negated
/// signal number when it died of one (the `ProcTable::exit` convention);
/// [`k_wait`] turns the two into the one word a parent reads.
#[no_mangle]
pub extern "C" fn k_exit(pid: i32, status: i32) {
    // The exit owes this process's parent a SIGCHLD, which `exit` sets. Record
    // it like any other signal so a delivery can be traced back to the process
    // that ended -- "why did the terminal think its shell died" needs the pid.
    note_kill(pid, procs().ppid(pid), gex_kernel::signal::sig::SIGCHLD);
    procs().exit(pid, status);
}

/// Collect a finished child of `ppid`. Zero if none is ready. `want` is a
/// specific pid, or -1 for any.
///
/// Returns pid in the high 32 bits, wait status in the low 32 -- paired
/// atomically, since two Workers can collect at once and a status left in a
/// static between calls would go to whichever asked last.
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

/// Send a signal, left pending for the target to collect at its next syscall --
/// which is what makes this safe to call from another process's Worker. Returns
/// the number signalled, or a negative errno.
#[no_mangle]
pub extern "C" fn k_kill(caller: i32, target: i32, signo: u32) -> i32 {
    if signo != 0 {
        note_signal();
        note_kill(caller, target, signo);
    }
    match procs().kill(caller, target, signo) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// How many signals have been *sent* to anyone, ever. A safepoint fires once per
/// thousand back edges in every Worker at once, so it cannot take this module's
/// lock and walk every process each time; one relaxed load of this counter says
/// "has anybody sent anything since I last looked", and only a change pays for
/// the real check.
static SIGNAL_SEQ: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The last signal this kernel handed to anyone: caller, target, number.
///
/// "Killed by signal 1" says nothing about who decided that, and the senders
/// are spread out -- the shared terminal's line discipline, a pty hangup, and
/// any guest's own `kill`. The shim reads this for its death report.
static LAST_KILL: Mutex<(i32, i32, u32)> = Mutex::new((0, 0, 0));
/// How many signals have been sent, so notes from different Workers can be put
/// in order -- which is the whole question when one process's signal makes
/// another one misbehave.
static KILL_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn note_kill(caller: i32, target: i32, signo: u32) {
    if signo != 0 {
        *LAST_KILL.lock().unwrap_or_else(|e| e.into_inner()) = (caller, target, signo);
        KILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// How many signals have been sent so far. See `note_kill`.
#[no_mangle]
pub extern "C" fn k_last_kill_seq() -> u32 {
    KILL_SEQ.load(std::sync::atomic::Ordering::Acquire)
}

/// Caller in the high bits, target in the middle, signal in the low byte. A
/// negative target is a process group, as `kill` spells it.
#[no_mangle]
pub extern "C" fn k_last_kill() -> u64 {
    let (caller, target, signo) = *LAST_KILL.lock().unwrap_or_else(|e| e.into_inner());
    ((caller as i64 as u64) << 40) | (((target as i64 as u64) & 0xffff_ffff) << 8)
        | (signo as u64 & 0xff)
}

fn note_signal() {
    SIGNAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// The counter above, for a safepoint to compare against what it last saw.
#[no_mangle]
pub extern "C" fn k_signal_seq() -> i32 {
    SIGNAL_SEQ.load(std::sync::atomic::Ordering::Acquire)
}

/// Signal every process in a group, naming the group positively. `k_kill`
/// cannot reach group 1: target -1 means "every process", so the group a shell
/// running as pid 1 is in has no negative spelling.
#[no_mangle]
pub extern "C" fn k_kill_group(caller: i32, pgid: i32, signo: u32) -> i32 {
    note_kill(caller, -pgid, signo);
    if signo != 0 {
        note_signal();
    }
    match procs().kill_group(caller, pgid, signo) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// Take everything pending for `pid`, as a bitmask of `1 << (signo - 1)`.
#[no_mangle]
pub extern "C" fn k_take_signals(pid: i32) -> u64 {
    procs().take_signals(pid)
}

/// Of `taken` (what `k_take_signals` just returned), the signals `kill` sent,
/// which the target reports as `SI_USER`.
#[no_mangle]
pub extern "C" fn k_take_kill_signals(pid: i32, taken: u64) -> u64 {
    procs().take_kill_signals(pid, taken)
}

/// Who last sent `signo` to `pid` by `kill`.
#[no_mangle]
pub extern "C" fn k_kill_sender(pid: i32, signo: u32) -> i32 {
    procs().kill_sender(pid, signo)
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

/// A process's name and command line, for the others' `/proc/<pid>`.
#[no_mangle]
pub extern "C" fn k_proc_set_info(pid: i32, at: gex_kmem::PAddr, len: u32) {
    let len = len.min(4096);
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    procs().set_info(pid, &buf);
}

/// Every process, encoded (`gex_kernel::proc::encode_entries`), into up to
/// `cap` bytes at `at`. Returns the whole length: more than `cap` means ask
/// again with that much room.
#[no_mangle]
pub extern "C" fn k_proc_snapshot(at: gex_kmem::PAddr, cap: u32) -> i32 {
    let b = gex_kernel::proc::encode_entries(&procs().entries());
    let n = (b.len() as u32).min(cap);
    if n > 0 {
        unsafe { gex_kmem::push_checked(at, b.as_ptr(), n) };
    }
    b.len() as i32
}

// -- pipes -------------------------------------------------------------------
//
// A pipe crosses processes as a *number*: each file table holds an id and the
// buffer lives in the global memory, so the two ends can be in Workers that
// share nothing else. An `Arc<Pipe>` could not -- a pointer means a memory.

/// Every pipe in the system, by id.
fn pipes() -> &'static std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>> {
    note_panics();
    static PIPES: OnceLock<std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>>> =
        OnceLock::new();
    PIPES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Which pipe ends each process still holds, so a process that dies without
/// closing them does not strand a reader on a missing end-of-file. `Drop` and
/// `exit_process` do not run when a guest leaves by a trap (which does not
/// unwind), so a real kernel's "close descriptors however it died" has to
/// happen here -- by then the process's own table belongs to a gone Worker.
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

/// Make a pipe with both ends open. Returns its id. `Pipe::new` starts each end
/// at one reference.
#[no_mangle]
pub extern "C" fn k_pipe_new(pid: i32) -> i32 {
    let id = {
        let mut t = pipes().lock().unwrap_or_else(|e| e.into_inner());
        t.push(std::sync::Arc::new(gex_kernel::pipe::Pipe::new()));
        (t.len() - 1) as i32
    };
    // Holder registration for these two ends is disabled while a trap after
    // `exit_group` on `ls | cat` is ruled out; `pid` stays in the signature so
    // the ABI does not churn. See tasks 143 and 146.
    let _ = pid;
    id
}

/// The last message written to a pipe, into the caller's memory. A diagnostic;
/// see `Pipe::last_message`.
#[no_mangle]
pub extern "C" fn k_pipe_last_message(id: i32, at: gex_kmem::PAddr, cap: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -1 };
    let text = p.last_message();
    let n = text.len().min(cap as usize);
    if n > 0 {
        unsafe { gex_kmem::push_checked(at, text.as_ptr(), n as u32) };
    }
    n as i32
}

#[no_mangle]
pub extern "C" fn k_pipe_writes(id: i32) -> i32 {
    let Some(p) = pipe_at(id) else { return -1 };
    let (writes, _) = p.write_totals();
    writes.min(i32::MAX as u64) as i32
}

#[no_mangle]
pub extern "C" fn k_pipe_write_stats(id: i32) -> i32 {
    let Some(p) = pipe_at(id) else { return -1 };
    let (short, refused) = p.write_stats();
    (((short as i32) & 0xffff) << 16) | ((refused as i32) & 0xffff)
}

#[no_mangle]
pub extern "C" fn k_pipe_ends(id: i32) -> i32 {
    let Some(p) = pipe_at(id) else { return -1 };
    let (r, w) = p.ends();
    (((r as i32) & 0xffff) << 16) | ((w as i32) & 0xffff)
}

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

/// Write `len` bytes of the caller's memory into the pipe. The bytes are copied
/// out first (this module cannot address the process's memory) -- which is what
/// lets the two ends live in different address spaces. Returns bytes or -errno.
#[no_mangle]
pub extern "C" fn k_pipe_write(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
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
pub extern "C" fn k_pipe_read(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -9 };
    // No bigger than what is buffered (at least one byte, so an empty pipe
    // still answers EOF or EAGAIN): a reader asking for 128 KiB of a pipe
    // holding a few bytes zeroed the whole request on every call.
    let want = (len as usize).min(p.buffered().max(1));
    let mut buf = vec![0u8; want];
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

// ---------------------------------------------------------------------------
// Unix98 pseudoterminals. The same shape as pipes -- a numbered pair in the
// global kernel, bytes copied across memories -- with the terminal added: the
// master->slave direction runs the shared `Pty`'s line discipline, and `^C`,
// hangup and window-size changes become real signals delivered here, next to
// the process table, the way `k_tty_input` does for the console.

/// Every pseudoterminal in the system, by id. The id is also its pts number, so
/// `/dev/pts/N` is the pair at index N.
fn ptys() -> &'static Mutex<Vec<std::sync::Arc<gex_kernel::pty::Pty>>> {
    note_panics();
    static PTYS: OnceLock<Mutex<Vec<std::sync::Arc<gex_kernel::pty::Pty>>>> = OnceLock::new();
    PTYS.get_or_init(|| Mutex::new(Vec::new()))
}

/// How many descriptors each process has on each pty end, keyed
/// `pid -> (id, master) -> count`.
///
/// The *end's* own count is a count of PROCESSES, not of descriptors, and this
/// is what makes that possible: a process's first descriptor on an end opens it
/// and its last one closes it. That is the semantic that matters -- the slave
/// is at end of file when no process has it open, and the last master going is
/// a hangup for the whole session -- and it is the only one gex can keep exact.
/// A descriptor count cannot be: `fork` hands a child every descriptor (so the
/// parent counts three for one file it dup'd to stdin, stdout and stderr) while
/// the child's kernel holds one object for them and closes once. Counting
/// descriptors, those two disagree, and the disagreement showed up as a
/// terminal whose shell died the moment its first command finished: the slave
/// count reached zero while three descriptors still pointed at it, the master
/// read end of file, and the session took a SIGHUP.
///
/// It also survives a Worker that leaves by a trap (no unwind, no `Drop`):
/// `k_pty_forget` releases whatever that pid still held.
fn pty_holders() -> &'static Mutex<BTreeMap<i32, BTreeMap<(i32, bool), u32>>> {
    note_panics();
    static HELD: OnceLock<Mutex<BTreeMap<i32, BTreeMap<(i32, bool), u32>>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn pty_at(id: i32) -> Option<std::sync::Arc<gex_kernel::pty::Pty>> {
    if id < 0 {
        return None;
    }
    ptys()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
}

/// Note a descriptor on an end. True when this process did not have one before,
/// which is when the end itself gains a holder.
fn pty_hold(pid: i32, id: i32, master: bool) -> bool {
    let mut h = pty_holders().lock().unwrap_or_else(|e| e.into_inner());
    let n = h.entry(pid).or_default().entry((id, master)).or_insert(0);
    *n += 1;
    *n == 1
}

/// Drop a descriptor on an end. True when this process has none left, which is
/// when the end itself loses a holder.
fn pty_unhold(pid: i32, id: i32, master: bool) -> bool {
    let mut h = pty_holders().lock().unwrap_or_else(|e| e.into_inner());
    let Some(mine) = h.get_mut(&pid) else {
        return false;
    };
    let Some(n) = mine.get_mut(&(id, master)) else {
        return false;
    };
    *n = n.saturating_sub(1);
    if *n == 0 {
        mine.remove(&(id, master));
        return true;
    }
    false
}

/// A signal to the pty's foreground group, if it has claimed one. `^C`, hangup
/// and `SIGWINCH` all reach the slave's process this way.
fn pty_signal(p: &gex_kernel::pty::Pty, sig: u32) {
    let fg = p.tty.fg();
    if fg > 0 {
        note_kill(0, -fg, sig);
    let _ = procs().kill_group(gex_kernel::INIT_PID, fg, sig);
    }
}

/// Open `/dev/ptmx`: a fresh locked pair with the master held by `pid`. Returns
/// `(id << 16) | ptn`; the id and the pts number are the same.
#[no_mangle]
pub extern "C" fn k_pty_open_master(pid: i32) -> i32 {
    let id = {
        let mut t = ptys().lock().unwrap_or_else(|e| e.into_inner());
        let id = t.len() as u32;
        t.push(std::sync::Arc::new(gex_kernel::pty::Pty::new(id)));
        id as i32
    };
    pty_hold(pid, id, true);
    (id << 16) | id
}

/// Open `/dev/pts/N`: take a slave, held by `pid`. Returns the id, `-1` for no
/// such pts, or `-5` (EIO) while it is still locked.
#[no_mangle]
pub extern "C" fn k_pty_open_slave(ptn: i32, pid: i32) -> i32 {
    let Some(p) = pty_at(ptn) else { return -1 };
    if p.locked() {
        return -5; // EIO -- unlockpt was not called
    }
    if pty_hold(pid, ptn, false) {
        p.open_slave();
    }
    ptn
}

/// Another descriptor onto an end, as `dup` and `fork` make.
#[no_mangle]
pub extern "C" fn k_pty_open_end(id: i32, master: i32, pid: i32) {
    if let Some(p) = pty_at(id) {
        // Only the process's *first* descriptor on this end opens it: see
        // `pty_holders`.
        if pty_hold(pid, id, master != 0) {
            if master != 0 {
                p.open_master();
            } else {
                p.open_slave();
            }
        }
    }
}

/// Drop a descriptor. The last master hangs the slave up (`SIGHUP`); the last
/// slave is the master's end-of-file.
#[no_mangle]
pub extern "C" fn k_pty_close_end(id: i32, master: i32, pid: i32) {
    if let Some(p) = pty_at(id) {
        // And only its *last* descriptor closes it.
        if pty_unhold(pid, id, master != 0) {
            if master != 0 {
                if p.close_master() {
                    pty_signal(&p, gex_kernel::signal::sig::SIGHUP);
                }
            } else {
                p.close_slave();
                last_slave_closer()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, pid);
            }
        }
        k_wake();
    }
}

/// Close every pty end `pid` still holds. Called when a process ends.
#[no_mangle]
pub extern "C" fn k_pty_forget(pid: i32) -> u32 {
    let held = {
        let mut h = pty_holders().lock().unwrap_or_else(|e| e.into_inner());
        h.remove(&pid).unwrap_or_default()
    };
    let n = held.len() as u32;
    // One close per end, whatever the process had open on it: the end counts
    // holders. See `pty_holders`.
    for ((id, master), _) in held {
        if let Some(p) = pty_at(id) {
            if master {
                if p.close_master() {
                    pty_signal(&p, gex_kernel::signal::sig::SIGHUP);
                }
            } else {
                p.close_slave();
            }
        }
    }
    if n > 0 {
        k_wake();
    }
    n
}

/// Whether the session that owns this pty's slave is still running.
///
/// A pty master reports end of file when the last slave descriptor goes -- that
/// is how a terminal emulator learns its shell is gone. But a *transient*
/// close is not a hangup, and a terminal emulator's own child makes one: it
/// opens the slave, closes it, calls `setsid`, and opens it again so that the
/// reopen is what claims the controlling terminal. On Linux that window is a
/// few instructions wide and nothing observes it; here every syscall crosses
/// into JavaScript, so the window is milliseconds and the emulator's poll saw
/// it every time -- read end of file, kill the process group, and a terminal
/// that ran exactly one command before its shell died of SIGHUP.
///
/// So the question is not "is a descriptor open" but "is whoever owned the slave
/// still there": the session that claimed the terminal, if one has, and
/// otherwise the process that let the last descriptor go -- because the window
/// above opens *before* `TIOCSCTTY`, when no session owns it yet. Either one
/// alive means the slave is merely between descriptors.
fn hangup_is_real(p: &gex_kernel::pty::Pty, id: i32) -> bool {
    // Alive, not merely in the table: a session leader that exited is a
    // zombie until its parent waits, and the parent waiting is often the very
    // reader that needs the hangup to stop reading (`pty.fork`, then read the
    // master until EIO, then `waitpid`).
    let sid = p.tty.sid();
    if sid > 0 {
        return !procs().alive(sid);
    }
    let who = last_slave_closer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .copied();
    match who {
        Some(pid) if pid > 0 => !procs().alive(pid),
        // Nobody has closed one yet, or the closer is unknown: the old rule.
        _ => true,
    }
}

/// Who last let go of a pty's slave. See `hangup_is_real`.
fn last_slave_closer() -> &'static Mutex<BTreeMap<i32, i32>> {
    note_panics();
    static WHO: OnceLock<Mutex<BTreeMap<i32, i32>>> = OnceLock::new();
    WHO.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Write `len` bytes of the caller's memory to an end. On the master this is
/// terminal input: the line discipline runs and `^C`-style signals reach the
/// foreground group. On the slave it is program output. Returns bytes or -errno.
#[no_mangle]
pub extern "C" fn k_pty_write(id: i32, master: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(p) = pty_at(id) else { return -9 }; // EBADF
    let mut buf = vec![0u8; len as usize];
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    if master != 0 {
        let signals = p.master_write(&buf);
        for sig in signals {
            pty_signal(&p, sig);
        }
    } else {
        p.slave_write(&buf);
    }
    // Wake a reader parked in another Worker, which runs no event loop.
    k_wake();
    len as i32
}

/// Read up to `len` bytes from an end into the caller's memory.
#[no_mangle]
pub extern "C" fn k_pty_read(id: i32, master: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(p) = pty_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    let r = if master != 0 {
        match p.master_read(&mut buf) {
            // Hung up, but the session that owns the slave is still there:
            // this is the reopen window, not a hangup. See
            // `hangup_is_real`.
            Err(e) if e == gex_kernel::errno::Errno::EIO && !hangup_is_real(&p, id) => Err(gex_kernel::errno::Errno(11)),
            other => other,
        }
    } else {
        p.slave_read(&mut buf)
    };
    match r {
        Ok(n) => {
            unsafe { gex_kmem::push_checked(at, buf.as_ptr(), n as u32) };
            n as i32
        }
        Err(e) => -(e.0 as i32),
    }
}

/// How many descriptors hold each end: masters in the high half, slaves in the
/// low. Only a diagnostic -- but the whole session rests on these being right
/// (the last master closing hangs the slave up with SIGHUP), and an unbalanced
/// inherit is otherwise invisible until a shell dies for no reason.
#[no_mangle]
pub extern "C" fn k_pty_counts(id: i32) -> u32 {
    let Some(p) = pty_at(id) else { return 0 };
    (p.master_count() << 16) | (p.slave_count() & 0xffff)
}

#[no_mangle]
pub extern "C" fn k_pty_readable(id: i32, master: i32) -> i32 {
    let Some(p) = pty_at(id) else { return 1 }; // gone: readable, so EOF is seen
    if master != 0 {
        // Readable for the same reasons a read would answer with something:
        // bytes, or a hangup that is real. See `hangup_is_real`.
        if p.master_available() > 0 {
            return 1;
        }
        return (p.master_readable() && hangup_is_real(&p, id)) as i32;
    }
    p.slave_readable() as i32
}

#[no_mangle]
pub extern "C" fn k_pty_available(id: i32, master: i32) -> i32 {
    let Some(p) = pty_at(id) else { return 0 };
    (if master != 0 { p.master_available() } else { p.slave_available() }) as i32
}

#[no_mangle]
pub extern "C" fn k_pty_ptn(id: i32) -> i32 {
    pty_at(id).map_or(-1, |p| p.ptn() as i32)
}

#[no_mangle]
pub extern "C" fn k_pty_get_lock(id: i32) -> i32 {
    pty_at(id).map_or(1, |p| p.locked() as i32)
}

#[no_mangle]
pub extern "C" fn k_pty_set_lock(id: i32, locked: i32) {
    if let Some(p) = pty_at(id) {
        p.set_locked(locked != 0);
    }
}

#[no_mangle]
pub extern "C" fn k_pty_get_termios(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(p) = pty_at(id) else { return -1 };
    let t = p.tty.termios();
    let n = (len as usize).min(t.len());
    unsafe { gex_kmem::push_checked(at, t.as_ptr(), n as u32) };
    n as i32
}

#[no_mangle]
pub extern "C" fn k_pty_set_termios(id: i32, at: gex_kmem::PAddr, len: u32, flush: i32) -> i32 {
    let Some(p) = pty_at(id) else { return -1 };
    let mut buf = vec![0u8; len as usize];
    unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    if flush != 0 {
        p.flush(gex_kernel::tty::TCIFLUSH);
    }
    p.tty.set_termios(&buf);
    0
}

#[no_mangle]
pub extern "C" fn k_pty_winsize(id: i32) -> u64 {
    pty_at(id).map_or(0, |p| gex_kernel::tty::pack_ws(p.tty.winsize()))
}

#[no_mangle]
pub extern "C" fn k_pty_set_winsize(id: i32, packed: u64) -> i32 {
    let Some(p) = pty_at(id) else { return 0 };
    let changed = p.tty.set_winsize(gex_kernel::tty::unpack_ws(packed));
    if changed {
        pty_signal(&p, gex_kernel::signal::sig::SIGWINCH);
    }
    changed as i32
}

/// Which pty this session claimed as its controlling terminal, or -1.
///
/// `/dev/tty` means "the caller's controlling terminal", and for anything
/// running inside a terminal emulator that is a pty in here -- not the page's
/// own terminal, which is what the name used to resolve to. A shell opens
/// /dev/tty for job control and compares its foreground group with its own; the
/// page's terminal never matches, so the shell spun raising SIGTTIN at itself
/// and ran nothing.
#[no_mangle]
pub extern "C" fn k_pty_for_sid(sid: i32) -> i32 {
    if sid <= 0 {
        return -1;
    }
    let t = ptys().lock().unwrap_or_else(|e| e.into_inner());
    for (i, p) in t.iter().enumerate() {
        if p.tty.sid() == sid {
            return i as i32;
        }
    }
    -1
}

#[no_mangle]
pub extern "C" fn k_pty_fg(id: i32) -> i32 {
    pty_at(id).map_or(0, |p| p.tty.fg())
}

#[no_mangle]
pub extern "C" fn k_pty_set_fg(id: i32, pgrp: i32) {
    if let Some(p) = pty_at(id) {
        p.tty.set_fg(pgrp);
    }
}

#[no_mangle]
pub extern "C" fn k_pty_sid(id: i32) -> i32 {
    pty_at(id).map_or(0, |p| p.tty.sid())
}

#[no_mangle]
pub extern "C" fn k_pty_set_sid(id: i32, sid: i32) {
    if let Some(p) = pty_at(id) {
        p.tty.set_sid(sid);
    }
}

#[no_mangle]
pub extern "C" fn k_pty_flush(id: i32, which: i32) {
    if let Some(p) = pty_at(id) {
        p.flush(which);
    }
}

/// Copy `len` bytes of the calling process's memory in, and report their sum. A
/// live check that the two-memory pull path works.
#[no_mangle]
pub extern "C" fn k_checksum(at: gex_kmem::PAddr, len: u32) -> u32 {
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
// Shared, so a forked child's `execve` can find its program -- under the
// switcher the child inherited the parent's whole address space and the
// question never arose. Each process mounts a `RemoteFs` (just an id); every
// call arrives as an address `gex-kmem` copies across. Wire layouts are in
// `gex_kernel::vfs::remote`.

use gex_kernel::vfs::{remote, FileHandle, FileSystem};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Fill `len` bytes at `dst` (this module's memory) with the bytes of
    /// the URL at `url` from `offset`: the host's synchronous ranged
    /// request, in the Worker that reads. Returns the count, or negative.
    fn lazy_fetch(url: u32, url_len: u32, offset: u64, dst: u32, len: u32) -> i32;
}

fn fetch_lazy(url: &str, offset: u64, into: &mut [u8]) -> bool {
    // SAFETY: both ranges are in this module's memory and stay borrowed for
    // the call only.
    let n = unsafe { lazy_fetch(url.as_ptr() as u32, url.len() as u32, offset, into.as_mut_ptr() as u32, into.len() as u32) };
    n >= 0 && n as usize == into.len()
}

/// The root every process sees.
fn rootfs() -> &'static std::sync::Arc<gex_kernel::vfs::TmpFs> {
    note_panics();
    static ROOT: OnceLock<std::sync::Arc<gex_kernel::vfs::TmpFs>> = OnceLock::new();
    ROOT.get_or_init(|| {
        gex_kernel::vfs::tmpfs::set_lazy_fetch(fetch_lazy);
        let fs = gex_kernel::vfs::TmpFs::new();
        // Directories a guest expects before anything is bundled: `open(O_CREAT)`
        // under a missing directory is `ENOENT`, not a directory being made.
        for d in ["/tmp", "/proc", "/dev", "/run", "/var", "/var/tmp"] {
            fs.add_dir(d);
        }
        std::sync::Arc::new(fs)
    })
}

/// Every file a process has open here, by id. A `Vec` with holes, not a map:
/// lookup is the hot path, churn is not.
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

/// A path argument, copied out of the caller's memory (this module cannot
/// dereference an address in it).
fn path_of(at: gex_kmem::PAddr, len: u32) -> String {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    // `TmpFs` keys on the relative form, so leading slashes go -- accept both an
    // absolute path from the host and a relative one from a mount.
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
pub extern "C" fn k_fs_open(path: gex_kmem::PAddr, path_len: u32, flags: u64, mode: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().open(&p, flags, mode) {
        Ok(h) => {
            let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
            // First free slot, so an open/close loop does not grow this unbounded.
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

/// A second id for one open file: two ids onto one `Arc`, which is what a
/// descriptor shared across a `fork` is. Either side closing its own id leaves
/// the other's alone. The offset is *not* shared (it lives in each process's
/// file table), which is where this stops short of Linux. Reopening by name
/// instead broke on files `dpkg-deb` creates and immediately unlinks.
#[no_mangle]
pub extern "C" fn k_fs_dup(handle: i32) -> i32 {
    let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(h)) = t.get(handle as usize).map(|s| s.as_ref().map(std::sync::Arc::clone))
    else {
        return -9;
    };
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
pub extern "C" fn k_fs_read(handle: i32, offset: u64, at: gex_kmem::PAddr, len: u32) -> i32 {
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
pub extern "C" fn k_fs_write(handle: i32, offset: u64, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    as_ret(h.write_at(offset, &buf))
}

/// This handle's metadata, as `remote::META_BYTES` bytes at `at`.
#[no_mangle]
pub extern "C" fn k_fs_meta(handle: i32, at: gex_kmem::PAddr) -> i32 {
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
pub extern "C" fn k_fs_read_dir(handle: i32, at: gex_kmem::PAddr, cap: u32) -> i32 {
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
pub extern "C" fn k_fs_path_meta(path: gex_kmem::PAddr, path_len: u32, at: gex_kmem::PAddr) -> i32 {
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
pub extern "C" fn k_fs_read_link(path: gex_kmem::PAddr, path_len: u32, at: gex_kmem::PAddr, cap: u32) -> i32 {
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
pub extern "C" fn k_fs_unlink(path: gex_kmem::PAddr, path_len: u32) -> i32 {
    as_unit(rootfs().unlink(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_mkdir(path: gex_kmem::PAddr, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().mkdir(&path_of(path, path_len), mode))
}

#[no_mangle]
pub extern "C" fn k_fs_rmdir(path: gex_kmem::PAddr, path_len: u32) -> i32 {
    as_unit(rootfs().rmdir(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_rename(from: gex_kmem::PAddr, from_len: u32, to: gex_kmem::PAddr, to_len: u32) -> i32 {
    as_unit(rootfs().rename(&path_of(from, from_len), &path_of(to, to_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_chmod(path: gex_kmem::PAddr, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().chmod(&path_of(path, path_len), mode))
}

/// A path's modification time: `utimensat`, or a bundle's file times.
#[no_mangle]
pub extern "C" fn k_fs_set_mtime(path: gex_kmem::PAddr, path_len: u32, secs: i64) -> i32 {
    as_unit(rootfs().set_mtime(&path_of(path, path_len), secs))
}

/// `utimensat` with nanoseconds (`remote::Remote::set_times`).
#[no_mangle]
pub extern "C" fn k_fs_set_times(path: gex_kmem::PAddr, path_len: u32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32 {
    as_unit(rootfs().set_times_now(&path_of(path, path_len), remote::time_of(a_s, a_ns), remote::time_of(m_s, m_ns), now))
}

/// `futimens` (`remote::Remote::set_times_fd`).
#[no_mangle]
pub extern "C" fn k_fs_set_times_fd(handle: i32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    as_unit(h.set_times_now(remote::time_of(a_s, a_ns), remote::time_of(m_s, m_ns), now))
}

/// An open file's modification time. This module has no clock, so a process
/// that writes stamps the file with its own (see `vfs::remote::Remote::touch`).
#[no_mangle]
pub extern "C" fn k_fs_touch(handle: i32, secs: i64) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    as_unit(h.set_mtime(secs))
}

/// A symlink's target is text, not a path, so it uses `path_of_raw`: `path_of`
/// strips leading slashes (right for the name being created, wrong for what an
/// absolute-path target says, which would then resolve against the link's own
/// directory and reach nothing).
#[no_mangle]
pub extern "C" fn k_fs_symlink(target: gex_kmem::PAddr, target_len: u32, path: gex_kmem::PAddr, path_len: u32) -> i32 {
    as_unit(rootfs().symlink(&path_of_raw(target, target_len), &path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_link(from: gex_kmem::PAddr, from_len: u32, to: gex_kmem::PAddr, to_len: u32) -> i32 {
    as_unit(rootfs().link(&path_of(from, from_len), &path_of(to, to_len)))
}

/// Put a socket node at a path, so the name can be *seen*, not only connected
/// to: Xvfb binds `/tmp/.X11-unix/X0` and its start script waits for that path
/// to exist before running a client.
#[no_mangle]
pub extern "C" fn k_fs_mksocket(path: gex_kmem::PAddr, path_len: u32) -> i32 {
    rootfs().add_socket(&path_of(path, path_len));
    0
}

/// Put a file into the shared root before anything runs -- how a page's bundle
/// gets in, once, rather than through one process only it could then see.
#[no_mangle]
pub extern "C" fn k_fs_add(path: gex_kmem::PAddr, path_len: u32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let p = path_of(path, path_len);
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    rootfs().add_file(&p, buf);
    0
}

/// A file in the shared root whose bytes are fetched from `url` as they are
/// read (`TmpFs::add_lazy`): a game's data served to a page without
/// bundling it. A path the bundle already has is left alone.
#[no_mangle]
pub extern "C" fn k_fs_add_lazy(path: gex_kmem::PAddr, path_len: u32, url: gex_kmem::PAddr, url_len: u32, size: u64) -> i32 {
    let p = path_of(path, path_len);
    let u = path_of(url, url_len);
    rootfs().add_lazy(&p, size, &format!("/{u}"));
    0
}

/// Whether the shared root is filled yet, so only the first Worker unpacks the
/// rootfs.
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
// A socket's two ends are in two processes, so with a Worker per process the
// name table one binds in would be invisible to the other. Both ends live here
// instead, local to each other (`gex_kernel::unix` unchanged); each process
// holds an id.

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

/// Bind and listen in one: a remote socket has no state between `bind` and
/// `listen` worth modelling. Returns a listener id, or a negated errno.
#[no_mangle]
pub extern "C" fn k_sock_bind(path: gex_kmem::PAddr, path_len: u32) -> i32 {
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
pub extern "C" fn k_sock_connect(path: gex_kmem::PAddr, path_len: u32) -> i32 {
    let p = path_of_raw(path, path_len);
    let s = std::sync::Arc::new(UnixSocket::new(true));
    match names().connect(&s, &p) {
        Ok(_) => {
            // A connection is waiting on someone's listener, and that someone
            // may be parked. The guest syscall path wakes after every call;
            // these entry points are reached from the *host* -- the
            // compositor's X window manager, the page's own sockets -- and woke
            // nothing at all. A parked Worker re-checks on its own every 20 ms
            // whatever happens, so this is latency rather than correctness:
            // without it a host-side connect waits for that timer.
            k_wake();
            keep(s)
        }
        Err(e) => -e.0,
    }
}

/// Take a waiting connection. Returns the server end's id, or `-EAGAIN`.
#[no_mangle]
pub extern "C" fn k_sock_accept(listener: i32) -> i32 {
    let Some(l) = sock_at(listener) else { return -9 };
    match l.accept() {
        Ok(s) => {
            // The other end is connected now, which a parked client may be
            // waiting to hear.
            k_wake();
            keep(s)
        }
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
pub extern "C" fn k_sock_read(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
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
pub extern "C" fn k_sock_write(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let Some(s) = sock_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    match s.send(&buf) {
        Ok(n) => {
            // Bytes for whoever is reading, who may be parked. Same reason as
            // `k_sock_connect`: the host has no syscall path to wake for it.
            k_wake();
            n as i32
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_sock_readable(id: i32) -> i32 {
    use gex_kernel::vfs::FileHandle;
    sock_at(id).is_some_and(|s| s.poll_readable()) as i32
}

/// Hang-ups on an end: bit 0 `POLLHUP`, bit 1 `POLLRDHUP` (see
/// `gex_kernel::unix::Remote::hup`).
#[no_mangle]
pub extern "C" fn k_sock_hup(id: i32) -> i32 {
    use gex_kernel::vfs::FileHandle;
    sock_at(id).map_or(0, |s| s.poll_hup() as i32 | (s.poll_rdhup() as i32) << 1)
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
    // Out of the table under the lock, dropped after it: the last reference
    // to an end may free descriptors still queued in it, and releasing one of
    // those can be a `k_sock_close` of its own.
    let s = {
        let mut t = socks().lock().unwrap_or_else(|e| e.into_inner());
        t.get_mut(id as usize).and_then(Option::take)
    };
    // A listener on an abstract name (a leading NUL) gives the name up as it
    // closes, as on Linux; a path name stays until it is unlinked. (TCP ports
    // between a page's processes are abstract names.)
    if let Some(s) = s {
        if let Some(p) = s.bound_path().filter(|p| p.starts_with('\0')) {
            names().unbind_socket(&p, &s);
        }
        drop(s);
    }
}

/// A connected stream pair, for `socketpair`: both ids as two `u32`s at `out`.
#[no_mangle]
pub extern "C" fn k_sock_pair(out: gex_kmem::PAddr) -> i32 {
    let (a, b) = UnixSocket::pair();
    let ids = [keep(a) as u32, keep(b) as u32];
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&ids[0].to_le_bytes());
    bytes[4..].copy_from_slice(&ids[1].to_le_bytes());
    unsafe { gex_kmem::push_checked(out, bytes.as_ptr(), 8) };
    0
}

/// Another id on the same end, so a descriptor in flight to another process
/// keeps the end open whatever the sender does with its own.
#[no_mangle]
pub extern "C" fn k_sock_dup(id: i32) -> i32 {
    match sock_at(id) {
        Some(s) => keep(s),
        None => -9,
    }
}

// -- the boot instant ---------------------------------------------------------

/// Where every process's monotonic clocks start, in wall-clock seconds
/// (gex-kernel's `MONOTONIC_BASE`): the first process to ask sets it to
/// `want`, and every later one gets that. One base for the machine, as on
/// Linux: a process hands its monotonic times to another (Wine's clients
/// give wineserver absolute monotonic deadlines), and each Worker basing its
/// own at its first reading made them disagree by its start time.
static BOOT_SECS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn k_boot_secs(want: u32) -> u32 {
    use std::sync::atomic::Ordering;
    match BOOT_SECS.compare_exchange(0, want.max(1), Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => want.max(1),
        Err(had) => had,
    }
}

// -- descriptors in flight ---------------------------------------------------
//
// `SCM_RIGHTS` between processes: the sender's process describes each
// descriptor as text and takes a reference that keeps it alive in flight (a
// pipe end opened for no one, a duplicated file or socket id); here each
// description rides the socket's own descriptor queue as a token, so it
// arrives with the byte it was sent with. The receiver takes the description
// and with it the reference. A token dropped untaken -- the socket closed
// with the message unread -- gives the reference back.

/// Tokens whose reference a receiver has taken, by serial.
fn taken() -> &'static std::sync::Mutex<std::collections::BTreeSet<u64>> {
    static T: OnceLock<std::sync::Mutex<std::collections::BTreeSet<u64>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()))
}

/// The holder a pipe end in flight is credited to.
const IN_FLIGHT: i32 = -1;

#[derive(Debug)]
struct FdToken {
    serial: u64,
    desc: String,
}

/// `<flags> <offset> <name>`, as the shim writes a description.
fn desc_name(desc: &str) -> (u64, &str) {
    let mut p = desc.splitn(3, ' ');
    let flags = p.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let _offset = p.next();
    (flags, p.next().unwrap_or(""))
}

fn pipe_of(name: &str) -> Option<i32> {
    name.strip_prefix("pipe:[")?.strip_suffix(']')?.parse().ok()
}

impl Drop for FdToken {
    fn drop(&mut self) {
        if taken().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.serial) {
            return;
        }
        let (flags, name) = desc_name(&self.desc);
        if let Some(id) = pipe_of(name) {
            k_pipe_close_end(id, (flags & 3 != 0) as i32, IN_FLIGHT);
        } else if let Some(id) = name.strip_prefix('#').and_then(|v| v.parse().ok()) {
            k_fs_close(id);
        } else if let Some(id) = name.strip_prefix("sock:").and_then(|v| v.split(':').next()?.parse().ok()) {
            k_sock_close(id);
        }
    }
}

impl gex_kernel::vfs::FileHandle for FdToken {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> gex_kernel::errno::SysResult {
        Err(gex_kernel::errno::Errno::EBADF)
    }

    fn metadata(&self) -> Result<gex_kernel::vfs::Metadata, gex_kernel::errno::Errno> {
        Err(gex_kernel::errno::Errno::EBADF)
    }
}

/// `send` with descriptors: `descs` is one description per line.
#[no_mangle]
pub extern "C" fn k_sock_write_fds(
    id: i32,
    at: gex_kmem::PAddr,
    len: u32,
    descs: gex_kmem::PAddr,
    descs_len: u32,
) -> i32 {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let text = path_of_raw(descs, descs_len);
    let files: Vec<_> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|d| {
            let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gex_kernel::file::OpenFile::new(
                std::sync::Arc::new(FdToken {
                    serial,
                    desc: d.to_string(),
                }),
                format!("{serial} {d}"),
                0,
                false,
                false,
            )
        })
        .collect();
    let Some(s) = sock_at(id) else {
        drop(files);
        return -9;
    };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    s.stage_send(files);
    let r = s.send(&buf);
    // A send that wrote nothing attached nothing; what is still staged goes
    // back (and its references with it) rather than riding a later send.
    s.stage_send(Vec::new());
    match r {
        Ok(n) => {
            k_wake();
            n as i32
        }
        Err(e) => -e.0,
    }
}

/// `recv` that takes up to `max_fds` descriptors too. Their descriptions go
/// to `out` as a `u32` length and the text; their references are now `pid`'s.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn k_sock_read_fds(
    id: i32,
    at: gex_kmem::PAddr,
    len: u32,
    max_fds: u32,
    peek: i32,
    pid: i32,
    out: gex_kmem::PAddr,
    out_cap: u32,
) -> i32 {
    let Some(s) = sock_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    let n = match s.recv(&mut buf, max_fds as usize, peek != 0) {
        Ok(n) => n as usize,
        Err(e) => return -e.0,
    };
    if n > 0 {
        unsafe { gex_kmem::push_checked(at, buf.as_ptr(), n as u32) };
    }
    let mut text = String::new();
    for f in s.take_received() {
        let Some((serial, desc)) = f.path.split_once(' ') else { continue };
        let Ok(serial) = serial.parse::<u64>() else { continue };
        if 4 + text.len() + desc.len() + 1 > out_cap as usize {
            // No room to say it: the token drops untaken and its reference
            // goes back, as Linux closes what MSG_CTRUNC cut off.
            continue;
        }
        let (flags, name) = desc_name(desc);
        if let Some(pipe) = pipe_of(name) {
            let write = flags & 3 != 0;
            let mut h = pipe_holders().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = h.get_mut(&IN_FLIGHT) {
                if let Some(i) = v.iter().position(|e| *e == (pipe, write)) {
                    v.swap_remove(i);
                }
            }
            h.entry(pid).or_default().push((pipe, write));
        }
        taken().lock().unwrap_or_else(|e| e.into_inner()).insert(serial);
        text.push_str(desc);
        text.push('\n');
    }
    let mut hdr = (text.len() as u32).to_le_bytes().to_vec();
    hdr.extend_from_slice(text.as_bytes());
    unsafe { gex_kmem::push_checked(out, hdr.as_ptr(), hdr.len() as u32) };
    n as i32
}

#[no_mangle]
pub extern "C" fn k_sock_shutdown(id: i32, how: u32) -> i32 {
    match sock_at(id) {
        Some(s) => match s.shutdown_how(how as u64) {
            Ok(_) => 0,
            Err(e) => -e.0,
        },
        None => -9,
    }
}

/// Whether anything is listening on a name -- which is what a client that
/// looks before it leaps is asking. Xlib does; Mesa does not.
#[no_mangle]
pub extern "C" fn k_sock_exists(path: gex_kmem::PAddr, path_len: u32) -> i32 {
    names().exists(&path_of_raw(path, path_len)) as i32
}

/// A path exactly as the caller wrote it. A socket name is compared for
/// equality, never looked up, so the leading slash must survive -- trimming one
/// end's would make two different names.
fn path_of_raw(at: gex_kmem::PAddr, len: u32) -> String {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(buf.as_mut_ptr(), at, len) };
    }
    String::from_utf8_lossy(&buf).into_owned()
}

