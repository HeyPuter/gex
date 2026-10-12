//! Waiting, the Linux way (docs/kernel-locking.md §5.2 step 2).
//!
//! A Worker runs one guest thread, and the thread owns one *wait slot*: a
//! word in the global kernel's memory (`k_ws_*`), found from the Worker's
//! kernel stack. When the thread blocks, this registers the slot on the wait
//! queue of every object the thread waits for -- the pipe, the socket, each
//! descriptor of a `poll`, the epoll set (whose queue is chained onto its
//! descriptors'), the futex's bucket -- and on its process's queue, which a
//! signal, a child's exit or the process ending notifies. Then it checks
//! readiness, and parks on the slot until a notify or its own deadline. A
//! write to a pipe wakes the threads registered on that pipe and nobody
//! else: there is no global wake word any more, and no syscall wakes every
//! parked Worker of every process.
//!
//! The scheduler's idle turn asks only its own thread, from the snapshot
//! taken when the wait began ([`MyWait`]), with no kernel lock: a turn takes
//! KLock only to finish the syscall once the wait is over, to pump the
//! compositor when it has work, or when a signal may have arrived (the
//! signal state is still under KLock, §5.2 step 7).

use super::*;

thread_local! {
    /// This Worker's wait slot, -1 until first asked.
    static SLOT: Cell<i32> = const { Cell::new(-1) };
    /// What the slot read at the start of this scheduler turn: the park that
    /// ends the turn sleeps only while it still reads this, so a notify that
    /// lands between the readiness check and the park is not slept through.
    static SEQ: Cell<i32> = const { Cell::new(0) };
    /// The wait this Worker's thread is in, as it was when it began.
    static WAIT: RefCell<Option<MyWait>> = const { RefCell::new(None) };
}

/// This process's wait queue (`k_proc_queue`), with the pid it is for.
static PROC_Q: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When this process's next interval timer fires, on `now_ns`'s clock;
/// `u64::MAX` for none. Published after every syscall and every locked idle
/// turn, for a parked thread's deadline (a timer's signal is what ends a
/// `sigsuspend` or interrupts a wait).
pub(crate) static TIMER_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// A wait, as the scheduler turn that found it saw it under the lock.
pub(crate) struct MyWait {
    /// The thread (`cur()`), and what it waits for.
    thread: usize,
    waiting: Waiting,
    blocked_on: Option<gex_kernel::BlockOp>,
    /// The queues its slot is on.
    queues: Vec<u32>,
    /// The idle turn's verdict, which a lock-free turn repeats.
    pub why: u8,
    pub all_blocked: bool,
    pub ends_by_itself: bool,
    /// The thread's own deadline, and what its descriptors become ready at
    /// by the clock alone (a timerfd).
    soonest: Option<u64>,
}

/// This Worker's slot. The slot is the Worker's kernel stack's number, so
/// it lives exactly as long as the Worker and needs no freeing.
pub(crate) fn slot() -> i32 {
    SLOT.with(|s| {
        if s.get() < 0 {
            s.set(unsafe { k_ws_self() });
        }
        s.get()
    })
}

/// This process's wait queue.
pub(crate) fn proc_queue() -> u32 {
    use std::sync::atomic::Ordering::Relaxed;
    let pid = my_pid();
    let v = PROC_Q.load(Relaxed);
    if pid != 0 && (v >> 32) as i32 == pid {
        return v as u32;
    }
    let q = unsafe { k_proc_queue(pid) };
    PROC_Q.store(((pid as u32 as u64) << 32) | q as u64, Relaxed);
    q
}

/// Wake every waiting thread of this process: it is ending, or one of its
/// own threads raised a signal for another (`tgkill`), which only this
/// process's kernel knows about.
pub(crate) fn wake_process() {
    if unsafe { k_present() } != 0 {
        unsafe { k_wq_notify(proc_queue()) };
    }
}

/// Sample the slot at the start of a scheduler turn.
pub(crate) fn begin_turn() {
    if unsafe { k_present() } != 0 {
        SEQ.with(|q| q.set(unsafe { k_ws_seq(slot()) }));
    }
}

/// Publish when this process's next timer fires.
pub(crate) fn publish_timer(h: &Host) {
    let at = h.kernel.timer_in_ns().map_or(u64::MAX, |d| now_ns().saturating_add(d));
    let was = TIMER_AT.swap(at, std::sync::atomic::Ordering::Relaxed);
    // Sooner than the threads parked in other Workers sleep to: they look
    // again (`park_ns` reads it when they park).
    if at < was {
        wake_process();
    }
}

/// The queues a wait registers on, and when it becomes ready by the clock.
fn queues_of(waiting: &Waiting, blocked_on: &Option<gex_kernel::BlockOp>, now: u64, page: bool) -> (Vec<u32>, Option<u64>) {
    let mut qs = vec![proc_queue()];
    let mut soonest = match waiting {
        Waiting::Sleep(at) => Some(*at),
        Waiting::Futex { deadline, .. } | Waiting::Net(deadline) | Waiting::Pi { deadline, .. } => *deadline,
        _ => None,
    };
    let mut handle = |h: &Arc<dyn gex_kernel::vfs::FileHandle>, qs: &mut Vec<u32>| {
        let q = h.wait_queue();
        if q == 0 && !page && h.ready_in_ns().is_none() {
            unqueued(h);
        }
        qs.push(q);
        if let Some(d) = h.ready_in_ns() {
            let at = now.saturating_add(d);
            soonest = Some(soonest.map_or(at, |s| s.min(at)));
        }
    };
    match waiting {
        Waiting::Futex { parked, .. } => qs.push(unsafe { k_futex_queue(parked.addr()) }),
        Waiting::Pi { addr, .. } => qs.push(unsafe { k_futex_queue(*addr) }),
        Waiting::Read(h) | Waiting::Write(h) | Waiting::Accept(h) => handle(h, &mut qs),
        Waiting::Net(_) => {
            if let Some(gex_kernel::BlockOp::Poll { fds, report, .. }) = blocked_on {
                match report {
                    // The set's own queue, chained onto every descriptor it
                    // watches; a timerfd in the set still bounds the park.
                    gex_kernel::syscalls::PollReport::Epoll { set, .. } => {
                        qs.push(set.queue_id());
                        for p in fds {
                            if let Some(d) = p.handle.as_ref().and_then(|h| h.ready_in_ns()) {
                                let at = now.saturating_add(d);
                                soonest = Some(soonest.map_or(at, |s| s.min(at)));
                            }
                        }
                    }
                    _ => {
                        for p in fds {
                            if let Some(h) = &p.handle {
                                handle(h, &mut qs);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    qs.retain(|&q| q != 0);
    qs.sort_unstable();
    qs.dedup();
    (qs, soonest)
}

/// A wait on a descriptor with no queue and no clock: nothing will notify
/// it, so a thread that parks on it sleeps until its deadline. Said once per
/// process, by type, so a kind of descriptor that should have a queue is
/// found rather than hung on. (A wait the page ends -- a Wisp socket, a
/// socket the page serves -- never parks, and says nothing.)
fn unqueued(h: &Arc<dyn gex_kernel::vfs::FileHandle>) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        warn_line(&format!("[gex] pid {}: a wait on a descriptor with no wait queue: {h:?}\n", my_pid()));
    }
}

/// The locked half of an idle turn: this Worker's thread `i` is waiting and
/// not ready. Register its slot for the wait if this is a new one (and say
/// so: the caller checks readiness again, now that a change will notify).
pub(crate) fn enter(h: &mut Host, i: usize, why: u8, all_blocked: bool, ends_by_itself: bool) -> bool {
    if unsafe { k_present() } == 0 {
        return false;
    }
    let now = now_ns();
    publish_timer(h);
    let t = h.threads[i].run();
    let fresh = WAIT.with(|w| {
        let mut w = w.borrow_mut();
        if let Some(m) = w.as_mut().filter(|m| m.thread == i && same_wait(&m.waiting, &t.waiting)) {
            m.why = why;
            m.all_blocked = all_blocked;
            m.ends_by_itself = ends_by_itself;
            return false;
        }
        if let Some(old) = w.take() {
            unregister(&old);
        }
        let (queues, soonest) = queues_of(&t.waiting, &t.blocked_on, now, why & 4 != 0);
        let slot = slot() as u32;
        for &q in &queues {
            unsafe { k_wq_add(q, slot) };
        }
        *w = Some(MyWait {
            thread: i,
            waiting: t.waiting.clone(),
            blocked_on: t.blocked_on.clone(),
            queues,
            why,
            all_blocked,
            ends_by_itself,
            soonest,
        });
        true
    });
    fresh
}

/// Whether two waits are the same wait (a thread that blocked again on
/// something else must register afresh).
fn same_wait(a: &Waiting, b: &Waiting) -> bool {
    match (a, b) {
        (Waiting::Futex { parked: p, wake_gen: g, .. }, Waiting::Futex { parked: q, wake_gen: k, .. }) => {
            p.addr() == q.addr() && g == k
        }
        (Waiting::Sleep(x), Waiting::Sleep(y)) => x == y,
        (Waiting::Read(x), Waiting::Read(y))
        | (Waiting::Write(x), Waiting::Write(y))
        | (Waiting::Accept(x), Waiting::Accept(y)) => Arc::ptr_eq(x, y),
        (Waiting::Net(x), Waiting::Net(y)) => x == y,
        (Waiting::Pi { addr: a, gen: g, .. }, Waiting::Pi { addr: b, gen: k, .. }) => a == b && g == k,
        (Waiting::Child, Waiting::Child) | (Waiting::Signal, Waiting::Signal) => true,
        _ => false,
    }
}

fn unregister(m: &MyWait) {
    let slot = slot() as u32;
    for &q in &m.queues {
        unsafe { k_wq_del(q, slot) };
    }
}

/// The wait is over (the thread was picked, or the process is ending).
pub(crate) fn leave() {
    WAIT.with(|w| {
        if let Some(old) = w.borrow_mut().take() {
            unregister(&old);
        }
    });
}

/// An idle turn without the kernel lock: `Some((why, ends_by_itself))` when this Worker's
/// thread is still in the wait its last locked turn registered and is not
/// ready -- `why` the `paused()` bits to set. `None` when the turn must take
/// the lock: the wait may be over, a signal may have arrived, the
/// compositor has work, or there is no wait in hand.
pub(crate) fn idle_turn() -> Turn {
    if unsafe { k_present() } == 0 || EXITED.load(std::sync::atomic::Ordering::Relaxed) {
        return Turn::Locked;
    }
    WAIT.with(|w| {
        let w = w.borrow();
        let Some(m) = w.as_ref() else { return Turn::Locked };
        if m.thread != cur() || m.all_blocked {
            return Turn::Locked;
        }
        if crate::wl::pump_wanted() {
            return Turn::Locked;
        }
        // A signal for this thread, from another process (`kill`), from
        // one of its own threads (`tgkill`), or from its timer, that would
        // do something: asked of the signal state itself (`sigs`). It ends
        // the wait.
        if crate::sigs::ready() {
            return Turn::Ready(true);
        }
        if wait_ready(&m.waiting, &m.blocked_on, now_ns(), false) {
            return Turn::Ready(false);
        }
        Turn::Idle(m.why, m.ends_by_itself)
    })
}

/// What an idle turn without the kernel lock found ([`idle_turn`]).
pub(crate) enum Turn {
    /// Still waiting: the `paused()` bits to set, and whether the wait
    /// ends by itself.
    Idle(u8, bool),
    /// The wait is over (`true`: a signal that would do something ended
    /// it): the thread's own Worker finishes its call (`finish_mine`).
    Ready(bool),
    /// The turn is the kernel's: the compositor has work, a deadlock is
    /// being confirmed, or there is no wait in hand.
    Locked,
}

/// How long this Worker's thread may sleep: until its own deadline, a
/// descriptor's clock, or its process's timer; `None` for as long as it
/// takes. A deadlock candidate is looked at again (`DEADLOCK_CONFIRM`).
pub(crate) fn park_ns(now: u64) -> Option<u64> {
    let (soonest, all_blocked) = WAIT.with(|w| {
        w.borrow()
            .as_ref()
            .filter(|m| m.thread == cur())
            .map_or((None, false), |m| (m.soonest, m.all_blocked))
    });
    let timer = TIMER_AT.load(std::sync::atomic::Ordering::Relaxed);
    let timer = (timer != u64::MAX).then_some(timer);
    let at = match (soonest, timer) {
        (Some(s), Some(t)) => Some(s.min(t)),
        (s, t) => s.or(t),
    };
    let mut ns = at.map(|at| at.saturating_sub(now).max(50_000));
    if all_blocked {
        ns = Some(ns.map_or(DEADLOCK_LOOK_NS, |n| n.min(DEADLOCK_LOOK_NS)));
    }
    ns
}

/// How often a process whose every thread is parked on a futex nothing in
/// it can wake is looked at again, to confirm the deadlock it reports.
const DEADLOCK_LOOK_NS: u64 = 20_000_000;

/// Park this Worker on its slot: until a notify, or `park_ns`. Returns the
/// wait instruction's answer (0 woken, 1 moved already, 2 timed out).
pub(crate) fn park() -> i32 {
    let now = now_ns();
    let ns = park_ns(now).map_or(-1, |n| n as i64);
    let seq = SEQ.with(|q| q.get());
    unsafe { k_ws_park(slot(), seq, ns) }
}

/// Park on the slot for at most `ns`: a wait on the page, from a Worker
/// whose frames keep it from turning its event loop (`wasm32_wait`).
pub(crate) fn park_bounded(ns: u64) -> i32 {
    let ns = park_ns(now_ns()).map_or(ns, |n| n.min(ns));
    let seq = SEQ.with(|q| q.get());
    unsafe { k_ws_park(slot(), seq, ns as i64) }
}

/// The slot word's address and the value this turn sampled, for a wait the
/// page's event loop must keep turning through (`gex.mjs` `#netWait`).
#[no_mangle]
pub extern "C" fn gex_wait_word() -> u32 {
    if unsafe { k_present() } == 0 {
        return 0;
    }
    unsafe { k_ws_addr(slot()) }
}

#[no_mangle]
pub extern "C" fn gex_wait_seq() -> i32 {
    SEQ.with(|q| q.get())
}

/// Where this process's wait queue is in the global kernel's memory, and
/// where the slot words start: the page's compositor, which has no kernel
/// instance of its own, wakes the process's parked threads after putting an
/// input event in its ring by walking the queue's bitmap (`wl-bridge.mjs`
/// `sendEvent`). 0 with no global kernel.
#[no_mangle]
pub extern "C" fn gex_proc_queue_addr() -> u32 {
    if unsafe { k_present() } == 0 {
        return 0;
    }
    unsafe { k_wq_addr(proc_queue()) }
}

#[no_mangle]
pub extern "C" fn gex_slots_addr() -> u32 {
    if unsafe { k_present() } == 0 {
        return 0;
    }
    unsafe { k_ws_addr(0) }
}

/// Whether this Worker's thread is in a registered wait, for `wasm32_wait`.
pub(crate) fn waiting_here() -> bool {
    WAIT.with(|w| w.borrow().as_ref().is_some_and(|m| m.thread == cur()))
}

/// Whether the registered wait wants the page (`paused()` bit 4).
pub(crate) fn wants_page() -> Option<bool> {
    WAIT.with(|w| w.borrow().as_ref().filter(|m| m.thread == cur()).map(|m| m.why & 4 != 0))
}
