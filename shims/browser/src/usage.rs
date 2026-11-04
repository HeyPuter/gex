//! Processor time, as a page can measure it: the time a Worker is not
//! parked. A Worker parks when every thread it runs waits (the run loop's
//! pause, `set_paused_to`) and in a wait from inside a call (`waitq::park`);
//! the rest of its time it is running guest code or the calls it makes. The
//! browser does not say which core ran a Worker, nor for how long the system
//! had it descheduled, so that time counts too.
//!
//! A Worker's busy stretches add up per thread (this Worker's, for
//! `CLOCK_THREAD_CPUTIME_ID`) and per process (every Worker of it, in the
//! process's memory). The process says its total to the process table
//! (`ProcTable::set_usage`, what another process's `/proc/<pid>/stat` and
//! `/proc/stat` read) as a Worker parks, once it has grown by a clock tick,
//! and as the process exits.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// `USER_HZ`'s tick: what `/proc` counts in, so saying less is not worth a call.
const SAY_EVERY_NS: u64 = 10_000_000;

/// The process's busy time, its Workers' finished stretches.
static PROC_NS: AtomicU64 = AtomicU64::new(0);
/// What the process had used before this program's instance: an `execve`
/// into a fresh Worker and memory keeps the process and its time
/// (`ProcTable::set_usage` keeps the table's own). Read from the table at
/// the first saying.
static BASE_NS: AtomicU64 = AtomicU64::new(0);
static BASE_READ: AtomicBool = AtomicBool::new(false);
/// What was last said to the table (of `PROC_NS`).
static SAID_NS: AtomicU64 = AtomicU64::new(0);
/// When the process's first Worker started, on its host clock (0: unknown).
static STARTED_HOST_NS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This Worker's finished busy stretches.
    static MINE_NS: Cell<u64> = const { Cell::new(0) };
    /// When this Worker's busy stretch began (host clock), 0 while parked.
    static BUSY_SINCE: Cell<u64> = const { Cell::new(0) };
}

fn now() -> u64 {
    crate::host_now_ns().max(1)
}

/// A Worker starts running: a process's first, a thread's, a fork child's.
pub(crate) fn worker_start() {
    let t = now();
    BUSY_SINCE.with(|c| c.set(t));
    MINE_NS.with(|c| c.set(0));
    let _ = STARTED_HOST_NS.compare_exchange(0, t, Ordering::AcqRel, Ordering::Acquire);
}

/// A fork's child: a new process, which has used nothing yet, in a copy of
/// its parent's memory.
pub(crate) fn fork_child() {
    PROC_NS.store(0, Ordering::Release);
    BASE_NS.store(0, Ordering::Release);
    BASE_READ.store(true, Ordering::Release);
    SAID_NS.store(0, Ordering::Release);
    STARTED_HOST_NS.store(0, Ordering::Release);
    worker_start();
}

/// This Worker stops running: its stretch is the process's.
pub(crate) fn park_begin() {
    let since = BUSY_SINCE.with(|c| c.replace(0));
    if since == 0 {
        return;
    }
    let d = now().saturating_sub(since);
    MINE_NS.with(|c| c.set(c.get() + d));
    let total = PROC_NS.fetch_add(d, Ordering::AcqRel) + d;
    if total.saturating_sub(SAID_NS.load(Ordering::Acquire)) >= SAY_EVERY_NS {
        say(total);
    }
}

/// This Worker runs again.
pub(crate) fn park_end() {
    BUSY_SINCE.with(|c| {
        if c.get() == 0 {
            c.set(now());
        }
    });
}

/// Whether this Worker is in a busy stretch.
pub(crate) fn busy() -> bool {
    BUSY_SINCE.with(|c| c.get() != 0)
}

/// `f`, a wait, counted as parked unless this Worker is already.
pub(crate) fn parked<R>(f: impl FnOnce() -> R) -> R {
    let was = busy();
    if was {
        park_begin();
    }
    let r = f();
    if was {
        park_end();
    }
    r
}

fn current_stretch() -> u64 {
    match BUSY_SINCE.with(|c| c.get()) {
        0 => 0,
        since => now().saturating_sub(since),
    }
}

/// The processor-time clocks (`gex_kernel::proc::set_cpu_clock`): the
/// process's, its finished stretches and this Worker's current one (a
/// sibling's current stretch counts once it parks), or this thread's.
pub(crate) fn cpu_ns(thread: bool) -> u64 {
    let cur = current_stretch();
    if thread {
        return MINE_NS.with(|c| c.get()) + cur;
    }
    BASE_NS.load(Ordering::Acquire) + PROC_NS.load(Ordering::Acquire) + cur
}

/// The process exits: everything, said now, before its parent can collect it.
pub(crate) fn exiting() {
    let total = PROC_NS.load(Ordering::Acquire) + current_stretch();
    say(total);
}

/// Tell the process table the process's time (`total` of this instance's),
/// its memory and its start.
fn say(total: u64) {
    let Some(home) = crate::sigs::home() else { return };
    SAID_NS.fetch_max(total, Ordering::AcqRel);
    if !BASE_READ.swap(true, Ordering::AcqRel) {
        let had = home.procs().entries().into_iter().find(|e| e.pid == home.pid).map_or(0, |e| e.usage.cpu_ns);
        BASE_NS.store(had, Ordering::Release);
    }
    let started = STARTED_HOST_NS.load(Ordering::Acquire);
    let age = if started == 0 { 0 } else { now().saturating_sub(started) };
    home.say_usage(BASE_NS.load(Ordering::Acquire) + total, age);
}

