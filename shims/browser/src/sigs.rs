//! Signals without the kernel lock (docs/kernel-locking.md §3.8, step 8).
//!
//! A process's signal state is two kinds of shared object
//! ([`gex_kernel::signal`]): the process's `ProcSig` (dispositions, the
//! process-directed pending word, the interval timers) and each thread's
//! `ThreadSig` (its own pending word and mask). Both are reference-counted
//! and synchronise themselves, so a Worker asks whether its own thread has a
//! signal to take, and builds the handler frame for it, without the kernel:
//! what a safepoint does a thousand back edges at a time.

use crate::{k_present, k_signal_seq};
use gex_kernel::proc::ProcTable;
use gex_kernel::signal::{self, Delivery, ProcSig, Signals, ThreadSig};
use gex_kernel::futex::FutexTable;
use gex_kernel::mem::MapView;
use gex_kernel::syscalls::{self, ClockFn, FdIo, Free, FreeCall, FreeOutcome, FutexCx};
use gex_kernel::{GuestMem, Kernel};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

/// What this process's signals need that is not in the signal state: where
/// other processes leave theirs, and the clock its timers run on. Constant
/// for the process's life.
pub(crate) struct Home {
    pub sig: Arc<ProcSig>,
    procs: Arc<ProcTable>,
    pub pid: i32,
    uid: u32,
    clock: ClockFn,
    /// Its futexes and its memory map's published view, for `futex`
    /// without the kernel (docs/kernel-locking.md §5.2 step 4).
    pub futexes: Arc<FutexTable>,
    map: Arc<MapView>,
    /// Its namespace's published view, for a descriptor rebuilt from its
    /// name (`recvmsg`'s `SCM_RIGHTS`).
    vfs: Arc<gex_kernel::vfs::VfsView>,
    /// Its terminal, for `ioctl`.
    tty: Arc<gex_kernel::tty::TtyRef>,
    /// `flock`'s locks, released by the last `close` of a description.
    file_locks: Arc<gex_kernel::unix::FileLocks>,
}

/// This process's [`Home`], published by `set_host`. A new one replaces it
/// only when a new process takes over the memory, which happens once per
/// process; the old one is left, a few words, rather than freed under a
/// reader.
static HOME: AtomicPtr<Home> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    /// This Worker's thread's signal state, and its tid.
    static MINE: RefCell<Option<(i32, Arc<ThreadSig>)>> = const { RefCell::new(None) };
    /// `k_signal_seq` (or the table's own sequence) as this Worker last
    /// collected at.
    static SEQ_SEEN: Cell<u32> = const { Cell::new(0) };
}

/// Publish `k`'s process as this memory's (`set_host`).
pub(crate) fn publish(k: &Kernel) {
    HOME.store(Box::into_raw(Box::new(home_of(k))), Ordering::Release);
}

fn home_of(k: &Kernel) -> Home {
    Home {
        sig: Arc::clone(k.signals.process()),
        procs: Arc::clone(&k.procs),
        pid: k.pid,
        uid: k.uid,
        clock: k.clock,
        futexes: Arc::clone(&k.futexes),
        map: Arc::clone(k.mem.view()),
        vfs: Arc::clone(k.vfs.view()),
        tty: Arc::clone(&k.tty),
        file_locks: Arc::clone(&k.file_locks),
    }
}

thread_local! {
    /// A vfork child's [`Home`] while it runs in this Worker of its parent
    /// (`publish_here`): this Worker's process is the child's, the memory's
    /// other Workers the parent's.
    static HERE: Cell<*mut Home> = const { Cell::new(std::ptr::null_mut()) };
}

/// Publish a vfork child's process for this Worker alone, or (`None`) go
/// back to the memory's (vfork.rs).
pub(crate) fn publish_here(k: Option<&Kernel>) {
    let new = k.map_or(std::ptr::null_mut(), |k| Box::into_raw(Box::new(home_of(k))));
    let old = HERE.with(|c| c.replace(new));
    if !old.is_null() {
        // SAFETY: made above by an earlier call, and only this Worker reads
        // it, never across a call to here.
        drop(unsafe { Box::from_raw(old) });
    }
    // The next collection is the new process's.
    SEQ_SEEN.with(|c| c.set(0));
}

pub(crate) fn home() -> Option<&'static Home> {
    let here = HERE.with(|c| c.get());
    if !here.is_null() {
        // SAFETY: freed only by `publish_here`, on this Worker, between calls.
        return unsafe { here.as_ref() };
    }
    // SAFETY: a published `Home` is never freed (see `HOME`).
    unsafe { HOME.load(Ordering::Acquire).as_ref() }
}

/// Make thread `tid` this Worker's, under the kernel lock (`set_current`).
pub(crate) fn bind(k: &mut Kernel, tid: i32) {
    MINE.with(|m| {
        let mut m = m.borrow_mut();
        if m.as_ref().is_some_and(|(t, _)| *t == tid) {
            return;
        }
        *m = Some((tid, k.thread_sig(tid)));
    });
}

/// Bring in what other processes have sent this one, when anyone has sent
/// anything since this Worker last looked, and what its timers have raised.
fn gather(home: &Home) {
    let seq = if unsafe { k_present() } != 0 {
        unsafe { k_signal_seq() as u32 }
    } else {
        gex_kernel::proc::signal_seq()
    };
    if SEQ_SEEN.with(|c| c.replace(seq)) != seq {
        home.sig.collect(&home.procs, home.pid, home.uid);
    }
    if home.sig.timers_armed() {
        let (secs, nanos) = (home.clock)();
        home.sig.fire_timers(secs as u128 * 1_000_000_000 + nanos as u128);
    }
}

/// Whether a signal would do something to this Worker's thread: a handler,
/// or a default that is not "ignore". No kernel lock.
pub(crate) fn ready() -> bool {
    let Some(home) = home() else { return false };
    gather(home);
    MINE.with(|m| m.borrow().as_ref().is_some_and(|(_, t)| t.ready(&home.sig)))
}

/// Deliver the signal [`ready`] found to this Worker's thread, whose
/// context is at `ctx_addr`: build its handler frame, or say the process
/// ends. No kernel lock.
pub(crate) fn deliver(gm: &mut GuestMem<'_>, ctx_addr: u64) -> Delivery {
    let Some(home) = home() else { return Delivery::None };
    let Some(view) = view(home) else { return Delivery::None };
    dispatch(&view, gm, ctx_addr)
}

/// This Worker's thread's signal state as one `Signals`.
fn view(home: &Home) -> Option<Signals> {
    MINE.with(|m| m.borrow().as_ref().map(|(_, t)| Signals::view(Arc::clone(&home.sig), Arc::clone(t))))
}

fn dispatch(view: &Signals, gm: &mut GuestMem<'_>, ctx_addr: u64) -> Delivery {
    // The ABI the frame is in: an i386 process's, or the native one's
    // (`clock_constants` publishes it at load and exec).
    let compat = crate::CLOCK_ABI.load(Ordering::Acquire) == 1;
    signal::dispatch(view, gm, ctx_addr, compat).unwrap_or(Delivery::None)
}

/// The signal half of finishing this Worker's thread's parked call, whose
/// answer is already in its context at `ctx_addr` (what the kernel's
/// `finish_blocking` does, without it): a write that met a closed pipe
/// raises `SIGPIPE` (`sigpipe`), `rt_sigsuspend`'s mask goes back through
/// the handler's frame or, with no frame, here (`suspended`), and a pending
/// signal is delivered. No kernel lock.
pub(crate) fn finish(gm: &mut GuestMem<'_>, ctx_addr: u64, suspended: Option<u64>, sigpipe: bool) -> Delivery {
    let Some(home) = home() else { return Delivery::None };
    let Some(view) = view(home) else { return Delivery::None };
    gather(home);
    if sigpipe {
        // Linux sends it as the writer itself sending it (SI_USER).
        view.raise_from(
            signal::sig::SIGPIPE,
            signal::Source { code: signal::SI_USER, pid: home.pid, uid: home.uid },
        );
    }
    if suspended.is_some() {
        view.set_suspend_restore(suspended);
    }
    let delivery = if view.has_pending() { dispatch(&view, gm, ctx_addr) } else { Delivery::None };
    // Nothing was delivered, so no frame took the mask: put it back here.
    if let Some(mask) = view.take_suspend_restore() {
        view.set_blocked(mask);
    }
    delivery
}

/// Whether this Worker's thread's parked call of restart class `class`,
/// ended by a signal, starts again ([`Signals::restarts`]). No kernel lock.
pub(crate) fn restarts(class: signal::Restart) -> bool {
    let Some(home) = home() else { return false };
    let Some(view) = view(home) else { return false };
    gather(home);
    view.restarts(class)
}

/// The call of the lock-free class `call` (`syscalls::free_class`) for this
/// Worker's thread, whose context is at `ctx_addr`: the whole call, its park
/// and its signal included, without the kernel.
pub(crate) fn free_call(gm: &mut GuestMem<'_>, ctx_addr: u64, call: FreeCall, compat: bool) -> FreeOutcome {
    let home = home().expect("a syscall before its process was published");
    let view = view(home).expect("a syscall from a Worker with no thread bound");
    let fds = crate::fds_free().expect("a syscall before its descriptor table was published");
    gather(home);
    // Whether a futex word is mapped: the map's published view where other
    // Workers may be changing the map; the map itself in the single-threaded
    // build, which keeps no view and has nobody inside the kernel now.
    #[cfg(not(feature = "shared-memory"))]
    let by_kernel = |addr: u64, len: u64| {
        crate::with_host(|h| h.kernel.mem.vma_at(addr).is_some_and(|v| addr + len <= v.end))
    };
    let ns = home.vfs.get();
    let futex = FutexCx {
        table: &home.futexes,
        #[cfg(feature = "shared-memory")]
        map: Some(&*home.map),
        #[cfg(not(feature = "shared-memory"))]
        map: None,
        #[cfg(feature = "shared-memory")]
        mapped_by: None,
        #[cfg(not(feature = "shared-memory"))]
        mapped_by: Some(&by_kernel),
        tid: crate::my_tid() as u32,
        clock: home.clock,
    };
    let f = Free {
        io: FdIo { fds, compat, ns: &ns, pid: home.pid, file_locks: &home.file_locks },
        signals: &view,
        procs: &home.procs,
        tty: &home.tty,
        pid: home.pid,
        uid: home.uid,
        wasm32: crate::WASM32.load(Ordering::Acquire),
        futex,
        started: crate::clock_started(),
    };
    syscalls::free_call(&f, gm, ctx_addr, call)
}

/// Return from a signal handler for this Worker's thread, whose context is
/// at `ctx_addr`: the frame's context and mask back (`signal::sigreturn`).
/// The pc to go on at. No kernel lock: the thread's signal state is its own.
pub(crate) fn sigreturn(gm: &mut GuestMem<'_>, ctx_addr: u64) -> Result<u64, gex_kernel::Errno> {
    let home = home().ok_or(gex_kernel::Errno::EFAULT)?;
    let view = view(home).ok_or(gex_kernel::Errno::EFAULT)?;
    let compat = crate::CLOCK_ABI.load(Ordering::Acquire) == 1;
    signal::sigreturn(&view, gm, ctx_addr, compat, false)
}

/// Whether `pc` is in an executable mapping of this process: the map's
/// published view where other Workers may be changing the map; the map
/// itself in the single-threaded build, which keeps no view and has nobody
/// inside the kernel now.
pub(crate) fn executable(pc: u64) -> bool {
    #[cfg(feature = "shared-memory")]
    {
        home().and_then(|h| h.map.prot_at(pc)).is_some_and(|p| p & gex_kernel::mem::prot::EXEC != 0)
    }
    #[cfg(not(feature = "shared-memory"))]
    {
        crate::with_host(|h| h.kernel.mem.vma_at(pc).is_some_and(|v| v.prot & gex_kernel::mem::prot::EXEC != 0))
    }
}

/// How long until this process's next interval timer fires, by its own
/// clock; `None` with none armed. No kernel lock (`Kernel::timer_in_ns`).
pub(crate) fn timer_in_ns() -> Option<u64> {
    let home = home()?;
    if !home.sig.timers_armed() {
        return None;
    }
    let (secs, nanos) = (home.clock)();
    home.sig.timer_in_ns(secs as u128 * 1_000_000_000 + nanos as u128)
}
