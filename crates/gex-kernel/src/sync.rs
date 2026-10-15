//! The locks that replace the one kernel lock (docs/kernel-locking.md §4).
//!
//! One type per kind, the same source in all three builds:
//!
//! - **native:** `std::sync`, a futex mutex;
//! - **the shared-memory page shim** (`+atomics`): `std::sync` again, which
//!   on wasm32 with atomics is a CAS word parked with
//!   `memory.atomic.wait32/notify`, as `KLock` is;
//! - **the SP page build** (no atomics): `std::sync`'s single-thread form, a
//!   flag that panics on re-entry. That is the no-op cell §4.0 asks for, with
//!   its re-entry check.
//!
//! The kernel already keeps pipes, sockets and ptys behind `std::sync::Mutex`
//! in all three builds, so none of this is new to any of them. What the
//! wrappers add:
//!
//! - **the holder:** each lock records the thread that holds it (an id the
//!   host sets with [`set_thread_id`]: the Worker number in the page), for a
//!   deadlock report;
//! - **a held count** per thread, which the page shim checks every time it
//!   traps back to JS ([`check_trap_out`], §4.1). A trap does not unwind, so a
//!   guard held across one never drops and the lock stays taken for good;
//! - **no poisoning:** a native panic in one thread does not make the state
//!   unreadable to the rest. The page aborts on panic anyway.
//!
//! - **a rank** for the locks that nest ([`rank`]): a thread may take a
//!   ranked lock only while every ranked lock it holds ranks lower, checked
//!   in debug builds and with the `lock-ranks` feature ([`rank_take`]), so
//!   a change that would deadlock two Workers in the page fails at once on
//!   one.
//!
//! [`Epochs`] is the reclamation scheme of §4.2: readers announce an epoch,
//! and a writer frees what it retired once every reader has moved past it.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

thread_local! {
    /// How many [`Mutex`]/[`RwLock`] guards this thread holds.
    static HELD: Cell<u32> = const { Cell::new(0) };
    /// This thread's id for [`Mutex::holder`]; 0 until the host sets one.
    static ME: Cell<u32> = const { Cell::new(0) };
    /// The ranks of the ranked locks this thread holds, one bit each.
    static RANKS: Cell<u32> = const { Cell::new(0) };
}

/// Whether the order is checked: a debug build, or the `lock-ranks` feature.
const RANKED: bool = cfg!(any(debug_assertions, feature = "lock-ranks"));

/// The order the nesting locks are taken in (docs/kernel-locking.md §4.3):
/// a lower rank first. The kernel lock (`KLock`, the page shim's) is
/// outermost; the JIT state's lock may be taken under it; the memory map's
/// under either. Leaf locks (a pipe's, a futex shard's, a descriptor
/// table's) take nothing under them and are not ranked.
pub mod rank {
    pub const KERNEL: u8 = 1;
    pub const JIT: u8 = 2;
    pub const MM: u8 = 3;
}

/// About to take a lock of rank `r`: in a debug build (or with
/// `lock-ranks`), a panic if this thread already holds one of rank `r` or
/// above (a re-entry, or the order of [`rank`] broken), before it can block.
pub fn rank_take(r: u8) {
    if RANKED && r != 0 {
        let held = RANKS.with(|c| c.get());
        if held >> r != 0 {
            panic!(
                "gex: lock order: taking rank {r} while holding ranks {:?} (docs/kernel-locking.md §4.3)",
                (1..32).filter(|i| held & (1 << i) != 0).collect::<Vec<_>>()
            );
        }
        RANKS.with(|c| c.set(held | 1 << r));
    }
}

/// A lock of rank `r` given back ([`rank_take`]).
pub fn rank_give(r: u8) {
    if RANKED && r != 0 {
        RANKS.with(|c| c.set(c.get() & !(1 << r)));
    }
}

/// Name this thread for the holder records (the page: its Worker number + 1,
/// so that 0 stays "nobody").
pub fn set_thread_id(id: u32) {
    ME.with(|m| m.set(id));
}

/// How many kernel lock guards this thread holds now.
pub fn held() -> u32 {
    HELD.with(|h| h.get())
}

/// Called where the page shim leaves Rust by trapping back to JS. `Err(n)`
/// when this thread still holds `n` guards: a bug, since those locks stay
/// taken for good (§4.1). The count is reset so one bug is reported once.
pub fn check_trap_out() -> Result<(), u32> {
    let n = held();
    if n == 0 {
        return Ok(());
    }
    HELD.with(|h| h.set(0));
    RANKS.with(|r| r.set(0));
    Err(n)
}

fn took() {
    HELD.with(|h| h.set(h.get() + 1));
}

fn gave() {
    HELD.with(|h| h.set(h.get().saturating_sub(1)));
}

/// This thread's id ([`set_thread_id`]), or 0.
pub fn me() -> u32 {
    ME.with(|m| m.get())
}

/// A mutex over kernel state. See the module notes.
#[derive(Default)]
pub struct Mutex<T> {
    inner: std::sync::Mutex<T>,
    holder: AtomicU32,
    /// Its [`rank`], 0 for a leaf lock.
    rank: u8,
}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex::ranked(v, 0)
    }

    /// A lock that nests, taken in the order of [`rank`].
    pub const fn ranked(v: T, rank: u8) -> Self {
        Mutex {
            inner: std::sync::Mutex::new(v),
            holder: AtomicU32::new(0),
            rank,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        rank_take(self.rank);
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.holder.store(me(), Ordering::Relaxed);
        took();
        MutexGuard { g, lock: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let g = match self.inner.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        // Taken without waiting, so out of order is no deadlock: noted, not
        // checked.
        if RANKED && self.rank != 0 {
            RANKS.with(|c| c.set(c.get() | 1 << self.rank));
        }
        self.holder.store(me(), Ordering::Relaxed);
        took();
        Some(MutexGuard { g, lock: self })
    }

    /// The thread id ([`set_thread_id`]) that last took this lock, or 0. A
    /// hint for a deadlock report, not a synchronised answer.
    pub fn holder(&self) -> u32 {
        self.holder.load(Ordering::Relaxed)
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    pub fn into_inner(self) -> T {
        self.inner.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct MutexGuard<'a, T> {
    g: std::sync::MutexGuard<'a, T>,
    lock: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.holder.store(0, Ordering::Relaxed);
        rank_give(self.lock.rank);
        gave();
    }
}

impl<T> std::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> std::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.g
    }
}

/// A reader-writer lock over kernel state. The holder records the last
/// writer only.
#[derive(Default)]
pub struct RwLock<T> {
    inner: std::sync::RwLock<T>,
    writer: AtomicU32,
}

impl<T> RwLock<T> {
    pub const fn new(v: T) -> Self {
        RwLock {
            inner: std::sync::RwLock::new(v),
            writer: AtomicU32::new(0),
        }
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        took();
        ReadGuard { g }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        let g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        self.writer.store(me(), Ordering::Relaxed);
        took();
        WriteGuard { g, lock: self }
    }

    /// The thread id that last took this lock to write, or 0.
    pub fn writer(&self) -> u32 {
        self.writer.load(Ordering::Relaxed)
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct ReadGuard<'a, T> {
    g: std::sync::RwLockReadGuard<'a, T>,
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        gave();
    }
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

pub struct WriteGuard<'a, T> {
    g: std::sync::RwLockWriteGuard<'a, T>,
    lock: &'a RwLock<T>,
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.writer.store(0, Ordering::Relaxed);
        gave();
    }
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.g
    }
}

/// Epoch-based reclamation for snapshots published to lock-free readers
/// (§4.2).
///
/// The page has no quiescent point every thread passes through, so readers
/// say where they are: each thread has a slot, holding the global epoch
/// while it reads and 0 otherwise. A writer that replaces a snapshot
/// [`retire`](Epochs::retire)s the old one at the current epoch and moves the
/// epoch on; [`reclaim`](Epochs::reclaim) frees whatever no reader can still
/// be looking at.
///
/// Cost to a reader: two stores to its own slot and one load of the epoch.
pub struct Epochs<T> {
    epoch: AtomicU64,
    slots: Box<[AtomicU64]>,
    retired: Mutex<Vec<(u64, T)>>,
}

impl<T> Epochs<T> {
    /// An array for `n` reader slots (the page: one per Worker).
    pub fn new(n: usize) -> Self {
        Epochs {
            epoch: AtomicU64::new(1),
            slots: (0..n).map(|_| AtomicU64::new(0)).collect(),
            retired: Mutex::new(Vec::new()),
        }
    }

    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Announce a read from `slot` until the guard drops. A slot belongs to
    /// one thread; reads from it do not nest.
    pub fn pin(&self, slot: usize) -> Pin<'_, T> {
        let s = &self.slots[slot];
        // SeqCst on both sides: the slot's store must be visible before the
        // reader loads anything published, and a writer's epoch bump before
        // it scans the slots.
        s.store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        Pin { slot: s, _e: std::marker::PhantomData }
    }

    /// Hand over `old`, which readers pinned now may still be using. It is
    /// dropped by a later [`reclaim`](Self::reclaim) once they are done.
    pub fn retire(&self, old: T) {
        let e = self.epoch.fetch_add(1, Ordering::SeqCst);
        self.retired.lock().push((e, old));
    }

    /// Drop everything retired before the oldest epoch a reader still holds.
    /// Returns how many items were dropped.
    pub fn reclaim(&self) -> usize {
        let oldest = self
            .slots
            .iter()
            .map(|s| s.load(Ordering::SeqCst))
            .filter(|&e| e != 0)
            .min()
            .unwrap_or(u64::MAX);
        let freed: Vec<T> = {
            let mut r = self.retired.lock();
            let (gone, keep): (Vec<_>, Vec<_>) = r.drain(..).partition(|(e, _)| *e < oldest);
            *r = keep;
            gone.into_iter().map(|(_, t)| t).collect()
        };
        // Dropped outside the lock: a snapshot's destructor may be large.
        freed.len()
    }

    /// How many retired items are waiting for readers.
    pub fn pending(&self) -> usize {
        self.retired.lock().len()
    }
}

/// A reader's announcement; see [`Epochs::pin`].
pub struct Pin<'a, T> {
    slot: &'a AtomicU64,
    _e: std::marker::PhantomData<&'a Epochs<T>>,
}

impl<T> Drop for Pin<'_, T> {
    fn drop(&mut self) {
        self.slot.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_mutex_serialises_threads_and_records_its_holder() {
        let m = Arc::new(Mutex::new(0u64));
        let ts: Vec<_> = (0..4)
            .map(|t| {
                let m = m.clone();
                std::thread::spawn(move || {
                    set_thread_id(t + 1);
                    for _ in 0..10_000 {
                        let mut g = m.lock();
                        assert_eq!(m.holder(), t + 1);
                        // A read, a yield and a write: lost updates if two
                        // threads were ever inside together.
                        let v = *g;
                        std::hint::spin_loop();
                        *g = v + 1;
                    }
                    assert_eq!(held(), 0);
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(*m.lock(), 40_000);
        assert_eq!(m.holder(), 0);
    }

    #[test]
    fn ranked_locks_nest_in_rank_order() {
        let outer = Mutex::ranked((), rank::JIT);
        let inner = Mutex::ranked((), rank::MM);
        let leaf = Mutex::new(());
        {
            let _a = outer.lock();
            let _b = inner.lock();
            let _c = leaf.lock();
        }
        // Given back: the inner one alone, then the outer again.
        drop(inner.lock());
        drop(outer.lock());
    }

    #[test]
    #[should_panic(expected = "lock order")]
    fn a_ranked_lock_under_a_higher_one_panics() {
        let jit = Mutex::ranked((), rank::JIT);
        let mm = Mutex::ranked((), rank::MM);
        let _m = mm.lock();
        let _j = jit.lock();
    }

    #[test]
    #[should_panic(expected = "lock order")]
    fn a_ranked_lock_taken_again_panics_rather_than_hangs() {
        let mm = Mutex::ranked((), rank::MM);
        let _a = mm.lock();
        let _b = mm.lock();
    }

    #[test]
    fn try_lock_fails_while_another_thread_holds_it() {
        let m = Arc::new(Mutex::new(()));
        let g = m.lock();
        let m2 = m.clone();
        assert!(std::thread::spawn(move || m2.try_lock().is_none()).join().unwrap());
        drop(g);
        assert!(m.try_lock().is_some());
    }

    #[test]
    fn rwlock_readers_share_and_writers_exclude() {
        let l = Arc::new(RwLock::new(0u64));
        let r1 = l.read();
        let r2 = l.read();
        assert_eq!(held(), 2);
        let l2 = l.clone();
        let w = std::thread::spawn(move || {
            set_thread_id(9);
            let mut g = l2.write();
            *g += 1;
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(*r1 + *r2, 0, "the writer got in past two readers");
        drop((r1, r2));
        w.join().unwrap();
        assert_eq!(*l.read(), 1);
        assert_eq!(l.writer(), 0);
        assert_eq!(held(), 0);
    }

    /// The trap-out check (§4.1): a guard alive at a trap is reported, once.
    #[test]
    fn a_guard_held_at_a_trap_out_is_reported() {
        let m = Mutex::new(1);
        assert_eq!(check_trap_out(), Ok(()));
        let g = m.lock();
        let r = m.inner.try_lock().is_err();
        assert!(r);
        // What a trap does to a guard: it never drops.
        std::mem::forget(g);
        assert_eq!(check_trap_out(), Err(1));
        assert_eq!(check_trap_out(), Ok(()));
    }

    #[test]
    fn a_guard_dropped_before_the_trap_is_not_reported() {
        let m = RwLock::new(1);
        {
            let _w = m.write();
            let _ = Mutex::new(()).lock();
        }
        assert_eq!(check_trap_out(), Ok(()));
    }

    #[test]
    fn epochs_free_only_what_no_reader_can_see() {
        let e: Epochs<Arc<u32>> = Epochs::new(4);
        let snap = Arc::new(7);
        let watch = Arc::downgrade(&snap);
        let p = e.pin(2);
        e.retire(snap);
        assert_eq!(e.reclaim(), 0, "freed under a reader pinned before the retire");
        assert!(watch.upgrade().is_some());
        drop(p);
        assert_eq!(e.reclaim(), 1);
        assert!(watch.upgrade().is_none());
        // A reader pinned after the retire does not hold it back.
        e.retire(Arc::new(8));
        let _late = e.pin(0);
        assert_eq!(e.reclaim(), 1);
        assert_eq!(e.pending(), 0);
    }

    /// A retired snapshot whose drop marks it dead, so a reader that could
    /// still see it after reclamation would notice.
    struct Snap(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Snap {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn epochs_under_concurrent_readers_never_free_a_pinned_snapshot() {
        use std::sync::atomic::AtomicBool;
        let e = Arc::new(Epochs::<Snap>::new(4));
        let cur = Arc::new(Mutex::new(Snap(Arc::new(AtomicBool::new(true)))));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|slot| {
                let (e, cur, stop) = (e.clone(), cur.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _p = e.pin(slot);
                        let s = cur.lock().0.clone();
                        for _ in 0..100 {
                            assert!(s.load(Ordering::SeqCst), "read a reclaimed snapshot");
                        }
                    }
                })
            })
            .collect();
        for _ in 0..2_000 {
            let old = std::mem::replace(&mut *cur.lock(), Snap(Arc::new(AtomicBool::new(true))));
            e.retire(old);
            e.reclaim();
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        e.reclaim();
        assert_eq!(e.pending(), 0);
    }
}
