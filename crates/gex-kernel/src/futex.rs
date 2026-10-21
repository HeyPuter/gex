//! Futexes.
//!
//! These map onto the host's blocking primitives, which is what
//! makes them near-native: a guest thread is a real host thread, so waiting
//! is a real wait rather than a scheduler simulation.
//!
//! The contract that matters is the atomic compare-and-sleep. `FUTEX_WAIT`
//! must observe the word and block in one step relative to `FUTEX_WAKE`, or a
//! wake landing in between is lost and the waiter sleeps forever. That is
//! arranged by taking the queue's lock *before* reading guest memory and
//! holding it across the wait, so no waker can slip through the gap.

use crate::errno::Errno;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Futex operation codes, with the private and clock bits masked off.
pub mod op {
    pub const WAIT: u32 = 0;
    pub const WAKE: u32 = 1;
    pub const REQUEUE: u32 = 3;
    pub const CMP_REQUEUE: u32 = 4;
    /// `FUTEX_WAKE_OP`: atomically apply an operation to a second futex word,
    /// wake waiters on the first, and conditionally wake waiters on the second.
    /// glibc's condvar signalling path uses it; without it a `pthread_cond`
    /// wake is lost and the waiter deadlocks (Qt's threads hit exactly this).
    pub const WAKE_OP: u32 = 5;
    /// Priority-inheritance locks (glibc's `PTHREAD_PRIO_INHERIT` mutexes):
    /// the word holds the owner's tid, plus [`super::pi::WAITERS`] when
    /// someone sleeps on it. gex has no priorities to inherit, so these are
    /// plain owner-tracking locks (`Kernel::sys_futex`, `BlockOp::FutexLockPi`).
    pub const LOCK_PI: u32 = 6;
    pub const UNLOCK_PI: u32 = 7;
    pub const TRYLOCK_PI: u32 = 8;
    pub const LOCK_PI2: u32 = 13;
    pub const WAIT_BITSET: u32 = 9;
    pub const WAKE_BITSET: u32 = 10;
    /// `FUTEX_PRIVATE_FLAG`; these queues are per-process and all waiters share
    /// its memory, so it changes nothing.
    pub const PRIVATE_FLAG: u32 = 128;
    pub const CLOCK_REALTIME: u32 = 256;
}

/// The bits of a priority-inheritance futex word.
pub mod pi {
    pub const WAITERS: u32 = 0x8000_0000;
    pub const OWNER_DIED: u32 = 0x4000_0000;
    pub const TID_MASK: u32 = 0x3fff_ffff;
}

/// One wait queue, keyed by guest address.
#[derive(Default, Debug)]
struct Queue {
    /// Bumped on every wake, so a waiter can tell a real wake from a spurious
    /// wakeup without re-reading guest memory.
    generation: Mutex<u64>,
    ready: Condvar,
}

/// A place on a futex queue, held by a host that polls instead of blocking.
///
/// Holding it keeps the queue alive, so [`FutexTable::gc`] cannot drop the
/// generation counter this is watching. `Clone` shares the same queue and
/// snapshot -- both observe the same generation, which is what lets the
/// snapshot be taken in the kernel and carried to the host in a `BlockOp`.
#[derive(Debug, Clone)]
pub struct Parked {
    queue: Arc<Queue>,
    at: u64,
    /// The guest address parked on, so a deadlock can be named rather than
    /// just counted.
    addr: u64,
}

impl Parked {
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// Whether anything has woken this address since the park.
    pub fn woken(&self) -> bool {
        *self.queue.generation.lock().unwrap() != self.at
    }
}

/// How many shards a table's queues are split over: a wait and a wake on
/// different words rarely meet on one lock.
const SHARDS: usize = 256;

/// The shard of `addr`'s queue: the word's index, mixed so that neighbouring
/// words (a lock and its condition variable) go to different shards.
fn shard(addr: u64) -> usize {
    ((addr >> 2).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize % SHARDS
}

/// The process's futex queues.
///
/// No lock shared by every call: the queues are sharded by address, each
/// shard behind its own mutex, and each queue's generation behind its own.
/// A wait and a wake on one word meet only on that word's shard and queue.
#[derive(Debug)]
pub struct FutexTable {
    queues: Box<[Mutex<HashMap<u64, Arc<Queue>>>]>,
    /// Addresses woken since the host last drained them, for a host that relays
    /// wakes across threads that do not share this table -- the browser, where
    /// each Worker has its own `FutexTable` and a cross-Worker wake must reach
    /// the waiter through a shared per-address counter. Recorded on every wake,
    /// even one with no local waiter (the waiter is in the *other* table).
    /// Bounded: the browser drains it after each syscall; a native run, which
    /// blocks in [`Self::wait`] and never drains, must not grow it without end.
    woken: Mutex<VecDeque<u64>>,
    /// Whether `woken` may hold anything, so a caller with nothing to relay
    /// does not take its lock.
    any_woken: std::sync::atomic::AtomicBool,
    /// How many callers wait for each priority-inheritance lock: a
    /// `FUTEX_LOCK_PI` that found it held counts until its wait ends, so an
    /// unlock with nobody left clears the waiters bit, as Linux does (a
    /// trylock in user space takes only a bare 0).
    pi: Mutex<HashMap<u64, u32>>,
}

/// [`FutexTable::pi_lock`]'s guard.
pub struct PiCounts<'a>(std::sync::MutexGuard<'a, HashMap<u64, u32>>);

impl PiCounts<'_> {
    /// A caller starts waiting for the PI lock at `addr`.
    pub fn wait(&mut self, addr: u64) {
        *self.0.entry(addr).or_insert(0) += 1;
    }

    /// Whether anyone waits for the PI lock at `addr`.
    pub fn waiters(&self, addr: u64) -> bool {
        self.0.contains_key(&addr)
    }
}

impl Default for FutexTable {
    fn default() -> Self {
        FutexTable {
            queues: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            woken: Mutex::new(VecDeque::new()),
            any_woken: std::sync::atomic::AtomicBool::new(false),
            pi: Mutex::new(HashMap::new()),
        }
    }
}

impl FutexTable {
    pub fn new() -> FutexTable {
        FutexTable::default()
    }

    /// The waiter counts, held while a `FUTEX_LOCK_PI` that finds the lock
    /// held sets the waiters bit and counts itself, and while a
    /// `FUTEX_UNLOCK_PI` reads the count and stores the word: the two are
    /// one step each, so an unlock never clears the bit of a waiter that
    /// has just set it (a bare 0 is a lock user space takes and releases
    /// without a wake, and the waiter would sleep on).
    pub fn pi_lock(&self) -> PiCounts<'_> {
        PiCounts(self.pi.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Its wait for the PI lock at `addr` is over, however it ended.
    pub fn pi_done(&self, addr: u64) {
        let mut pi = self.pi.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = pi.get_mut(&addr) {
            *n -= 1;
            if *n == 0 {
                pi.remove(&addr);
            }
        }
    }


    fn queue(&self, addr: u64) -> Arc<Queue> {
        let mut q = self.queues[shard(addr)].lock().unwrap();
        Arc::clone(q.entry(addr).or_default())
    }

    /// Wait on `addr` while it still holds `expected`.
    ///
    /// `read` re-reads the guest word and is called with the queue lock held,
    /// so the comparison and the sleep cannot be split by a waker. Returns
    /// `EAGAIN` when the value had already changed and `ETIMEDOUT` on expiry.
    pub fn wait(
        &self,
        addr: u64,
        expected: u32,
        timeout: Option<Duration>,
        read: &dyn Fn() -> Option<u32>,
    ) -> Result<(), Errno> {
        let queue = self.queue(addr);
        let mut gen = queue.generation.lock().unwrap();
        match read() {
            None => return Err(Errno::EFAULT),
            Some(v) if v != expected => return Err(Errno::EAGAIN),
            _ => {}
        }
        let start = *gen;
        loop {
            match timeout {
                Some(d) => {
                    let (g, r) = queue.ready.wait_timeout(gen, d).unwrap();
                    gen = g;
                    if *gen != start {
                        return Ok(());
                    }
                    if r.timed_out() {
                        return Err(Errno::ETIMEDOUT);
                    }
                }
                None => {
                    gen = queue.ready.wait(gen).unwrap();
                    if *gen != start {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Wake up to `count` waiters on `addr`, returning how many were woken.
    ///
    /// Approximate in the same way Linux's is: it reports how many could be
    /// woken, and a woken thread re-checks its own condition regardless.
    pub fn wake(&self, addr: u64, count: u32) -> u64 {
        // Record for the cross-thread relay BEFORE the local-queue lookup: a
        // waiter in another Worker leaves this table with no queue for `addr`,
        // so returning early here would drop the record and the wake would be
        // invisible to it. Bounded to the most recent wakes -- a drop only costs
        // an extra re-check, never a lost wake, since the waiter also re-reads
        // the word.
        if count > 0 {
            let mut w = self.woken.lock().unwrap();
            if w.len() >= 512 {
                w.pop_front();
            }
            w.push_back(addr);
            self.any_woken.store(true, std::sync::atomic::Ordering::Release);
        }
        let Some(queue) = self.queues[shard(addr)].lock().unwrap().get(&addr).cloned() else {
            return 0;
        };
        let mut gen = queue.generation.lock().unwrap();
        *gen = gen.wrapping_add(1);
        match count {
            0 => 0,
            1 => {
                queue.ready.notify_one();
                1
            }
            n => {
                queue.ready.notify_all();
                n as u64
            }
        }
    }

    /// Addresses woken since this was last called, for the browser host to
    /// relay across Workers via its shared per-address wake counter. A native
    /// host blocks in [`Self::wait`] and never calls this; the buffer is bounded
    /// so that is safe.
    pub fn take_woken(&self) -> Vec<u64> {
        if !self.any_woken.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return Vec::new();
        }
        Vec::from(std::mem::take(&mut *self.woken.lock().unwrap()))
    }

    /// Take a place on `addr`'s queue without blocking, for the browser host
    /// where all guest threads share one wasm stack and the scheduler is
    /// cooperative: it parks here and polls [`Parked::woken`]. No race to close
    /// as in [`Self::wait`] -- a single-threaded host cannot be raced.
    pub fn park(&self, addr: u64) -> Parked {
        let queue = self.queue(addr);
        let at = *queue.generation.lock().unwrap();
        Parked { queue, at, addr }
    }

    /// Wake every waiter on every queue.
    ///
    /// Used when the process is ending: a thread parked on a futex will never
    /// be woken by the guest, because the guest is not going to run again.
    pub fn wake_everything(&self) {
        let queues: Vec<Arc<Queue>> =
            self.queues.iter().flat_map(|q| q.lock().unwrap().values().cloned().collect::<Vec<_>>()).collect();
        for q in queues {
            let mut gen = q.generation.lock().unwrap();
            *gen = gen.wrapping_add(1);
            q.ready.notify_all();
        }
    }

    /// Drop queues nobody is using, so a long-running process does not keep
    /// an entry for every lock it ever touched.
    pub fn gc(&self) {
        for q in self.queues.iter() {
            q.lock().unwrap().retain(|_, q| Arc::strong_count(q) > 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn a_parked_waiter_sees_a_wake_without_blocking() {
        let t = FutexTable::new();
        let p = t.park(0x1000);
        assert!(!p.woken());
        assert_eq!(t.wake(0x2000, 1), 0);
        assert!(!p.woken());
        t.wake(0x1000, 1);
        assert!(p.woken());
    }

    #[test]
    fn parking_keeps_the_queue_alive_through_a_collection() {
        let t = FutexTable::new();
        let p = t.park(0x1000);
        t.gc();
        t.wake(0x1000, 1);
        assert!(p.woken());
    }

    #[test]
    fn a_changed_value_does_not_sleep() {
        let f = FutexTable::new();
        assert_eq!(
            f.wait(0x1000, 1, None, &|| Some(2)).err(),
            Some(Errno::EAGAIN)
        );
    }

    #[test]
    fn a_wake_releases_a_waiter() {
        let f = Arc::new(FutexTable::new());
        let word = Arc::new(AtomicU32::new(1));
        let waiter = {
            let (f, word) = (Arc::clone(&f), Arc::clone(&word));
            std::thread::spawn(move || {
                f.wait(0x2000, 1, None, &|| Some(word.load(Ordering::SeqCst)))
            })
        };
        // Wake until the waiter is actually queued: a wake that comes
        // first finds nobody and is retried. The word stays 1, so however
        // late the waiter arrives it sleeps rather than returning EAGAIN.
        // The deadline is generous; under load the thread can take a while
        // to be scheduled at all.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while f.wake(0x2000, 1) == 0 {
            assert!(std::time::Instant::now() < deadline, "the waiter never queued");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(waiter.join().unwrap().is_ok());
        let _ = word.load(Ordering::SeqCst);
    }

    #[test]
    fn a_timeout_is_reported() {
        let f = FutexTable::new();
        let r = f.wait(0x3000, 1, Some(Duration::from_millis(20)), &|| Some(1));
        assert_eq!(r.err(), Some(Errno::ETIMEDOUT));
    }

    #[test]
    fn waking_an_address_nobody_waits_on_is_harmless() {
        assert_eq!(FutexTable::new().wake(0x4000, 1), 0);
    }

    #[test]
    fn a_wake_between_the_check_and_the_sleep_is_not_lost() {
        // The race the generation counter exists for: a waker running after
        // the value check but before the sleep must still be observed.
        let f = Arc::new(FutexTable::new());
        let started = Arc::new(AtomicU32::new(0));
        let waiter = {
            let (f, started) = (Arc::clone(&f), Arc::clone(&started));
            std::thread::spawn(move || {
                f.wait(0x5000, 1, Some(Duration::from_secs(5)), &|| {
                    started.store(1, Ordering::SeqCst);
                    Some(1)
                })
            })
        };
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        for _ in 0..50 {
            f.wake(0x5000, 1);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(waiter.join().unwrap().is_ok(), "the wake must not be lost");
    }

    #[test]
    fn unused_queues_are_collected() {
        let f = FutexTable::new();
        let _ = f.wait(0x6000, 1, Some(Duration::from_millis(1)), &|| Some(2));
        f.gc();
        assert!(f.queues.iter().all(|q| q.lock().unwrap().is_empty()));
    }
}
