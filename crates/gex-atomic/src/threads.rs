//! A process's thread table, read and grown without a lock
//! (docs/kernel-locking.md §3.7, L23).
//!
//! Slots in chunks that are never moved or freed while the table lives: a
//! new thread claims the next slot with one atomic add, fills it, and
//! publishes it by storing its state last. A slot's thread keeps its index
//! for its life, and a reader on any thread walks the table with no lock
//! but what the state has of its own. A slot's state is an `Arc` held by
//! the slot until the table goes, so a reader never meets a freed one; a
//! state a [`ThreadTable::reset`] replaces is left where it was (a few
//! words a thread).

use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Arc;

const CHUNK: usize = 128;
const CHUNKS: usize = 256;

/// One thread's place in the table: its tid, its context's address, and
/// its state `T`.
pub struct Slot<T> {
    tid: AtomicI32,
    ctx: AtomicU64,
    /// `Arc::into_raw` of its state; null until the slot is filled.
    state: AtomicPtr<T>,
}

impl<T> Slot<T> {
    fn empty() -> Slot<T> {
        Slot { tid: AtomicI32::new(0), ctx: AtomicU64::new(0), state: AtomicPtr::new(std::ptr::null_mut()) }
    }

    pub fn tid(&self) -> i32 {
        self.tid.load(Acquire)
    }

    pub fn ctx_addr(&self) -> u64 {
        self.ctx.load(Acquire)
    }

    /// Its state, which lives as long as the table.
    pub fn state(&self) -> &T {
        // SAFETY: an `Arc::into_raw` never released while the table lives
        // (see the module); `get` hands out only filled slots.
        unsafe { self.state.load(Acquire).as_ref() }.expect("a slot read before it was filled")
    }

    /// Its state, to keep past the table's borrow.
    pub fn state_arc(&self) -> Arc<T> {
        let p = self.state.load(Acquire);
        assert!(!p.is_null(), "a slot read before it was filled");
        // SAFETY: `p` is a live `Arc::into_raw` whose count the slot holds
        // for ever; this adds one for the copy returned.
        unsafe {
            Arc::increment_strong_count(p);
            Arc::from_raw(p)
        }
    }

    fn fill(&self, tid: i32, ctx: u64, state: T) {
        self.tid.store(tid, Release);
        self.ctx.store(ctx, Release);
        self.state.store(Arc::into_raw(Arc::new(state)) as *mut T, Release);
    }
}

/// The table: up to [`CHUNKS`] chunks of [`CHUNK`] slots, made as they are
/// first needed.
pub struct ThreadTable<T> {
    chunks: Box<[AtomicPtr<Slot<T>>]>,
    /// Slots claimed; a claimed slot is readable once its state is in.
    len: AtomicUsize,
}

// SAFETY: slots are atomics and `Arc<T>`s handed between threads, so the
// table is as shareable as `Arc<T>` is.
unsafe impl<T: Send + Sync> Send for ThreadTable<T> {}
unsafe impl<T: Send + Sync> Sync for ThreadTable<T> {}

impl<T> Default for ThreadTable<T> {
    fn default() -> Self {
        ThreadTable { chunks: (0..CHUNKS).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(), len: AtomicUsize::new(0) }
    }
}

impl<T> ThreadTable<T> {
    fn chunk(&self, c: usize) -> &[Slot<T>] {
        let mut p = self.chunks[c].load(Acquire);
        if p.is_null() {
            let new: Box<[Slot<T>]> = (0..CHUNK).map(|_| Slot::empty()).collect();
            let new = Box::into_raw(new) as *mut Slot<T>;
            p = match self.chunks[c].compare_exchange(std::ptr::null_mut(), new, AcqRel, Acquire) {
                Ok(_) => new,
                Err(won) => {
                    // SAFETY: `new` was never published.
                    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(new, CHUNK)) });
                    won
                }
            };
        }
        // SAFETY: a published chunk is `CHUNK` slots, never moved or freed
        // while the table lives.
        unsafe { std::slice::from_raw_parts(p, CHUNK) }
    }

    /// Add a thread, from any thread: its index for its life.
    pub fn push(&self, tid: i32, ctx: u64, state: T) -> usize {
        let i = self.len.fetch_add(1, AcqRel);
        assert!(i < CHUNK * CHUNKS, "more than {} threads in one process", CHUNK * CHUNKS);
        self.chunk(i / CHUNK)[i % CHUNK].fill(tid, ctx, state);
        i
    }

    /// An `execve`'s table: the one thread that made it, as slot 0. The
    /// others are gone; their slots are hidden, not freed.
    pub fn reset(&self, tid: i32, ctx: u64, state: T) {
        self.len.store(1, Release);
        self.chunk(0)[0].fill(tid, ctx, state);
    }

    /// Slot `i`, if it is filled.
    pub fn get(&self, i: usize) -> Option<&Slot<T>> {
        if i >= self.len.load(Acquire) {
            return None;
        }
        let p = self.chunks[i / CHUNK].load(Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: as in `chunk`.
        let s = unsafe { &*p.add(i % CHUNK) };
        (!s.state.load(Acquire).is_null()).then_some(s)
    }

    /// How many slots are claimed (some perhaps not yet filled).
    pub fn len(&self) -> usize {
        self.len.load(Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The filled slots, with their indices, as they stand.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Slot<T>)> + '_ {
        (0..self.len()).filter_map(move |i| self.get(i).map(|s| (i, s)))
    }

    /// The index of thread `tid`.
    pub fn position(&self, tid: i32) -> Option<usize> {
        self.iter().find(|(_, s)| s.tid() == tid).map(|(i, _)| i)
    }
}

impl<T> Drop for ThreadTable<T> {
    /// What the slots hold now goes with the table (no borrow of it can be
    /// left); a state a fill or a reset replaced stays leaked, as a reader
    /// may have had it while the table lived.
    fn drop(&mut self) {
        for c in self.chunks.iter() {
            let p = c.load(Acquire);
            if p.is_null() {
                continue;
            }
            // SAFETY: a published chunk of `CHUNK` slots (see `chunk`),
            // reachable now only through `&mut self`.
            let chunk = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, CHUNK)) };
            for slot in chunk.iter() {
                let st = slot.state.load(Acquire);
                if !st.is_null() {
                    // SAFETY: the slot's own count of an `Arc::into_raw`.
                    drop(unsafe { Arc::from_raw(st) });
                }
            }
        }
    }
}

impl<T> std::ops::Index<usize> for ThreadTable<T> {
    type Output = Slot<T>;
    fn index(&self, i: usize) -> &Slot<T> {
        self.get(i).unwrap_or_else(|| panic!("no thread {i}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Threads added from several threads at once each get a slot of their
    /// own, readable from another while they are added; a reset leaves the
    /// one thread.
    #[test]
    fn pushes_from_many_threads_each_get_their_own_slot() {
        let t: ThreadTable<Mutex<u64>> = ThreadTable::default();
        t.push(1, 0x1000, Mutex::new(0));
        std::thread::scope(|s| {
            for w in 0..4 {
                let t = &t;
                s.spawn(move || {
                    for k in 0..200 {
                        let tid = 2 + w * 200 + k;
                        let i = t.push(tid, 0x10_0000 + tid as u64 * 0x100, Mutex::new(tid as u64));
                        assert_eq!(t[i].tid(), tid);
                    }
                });
            }
            s.spawn(|| {
                for _ in 0..1000 {
                    for (_, slot) in t.iter() {
                        let want = if slot.tid() == 1 { 0 } else { slot.tid() as u64 };
                        assert_eq!(*slot.state().lock().unwrap(), want);
                    }
                }
            });
        });
        assert_eq!(t.iter().count(), 801);
        let mut tids: Vec<i32> = t.iter().map(|(_, s)| s.tid()).collect();
        tids.sort();
        assert_eq!(tids, (1..=801).collect::<Vec<_>>());
        for (i, s) in t.iter() {
            assert_eq!(s.ctx_addr(), if i == 0 { 0x1000 } else { 0x10_0000 + s.tid() as u64 * 0x100 });
            assert_eq!(t.position(s.tid()), Some(i));
        }
        let kept = t[5].state_arc();
        t.reset(9, 0x2000, Mutex::new(7));
        assert_eq!(t.iter().map(|(_, s)| s.tid()).collect::<Vec<_>>(), vec![9]);
        assert_eq!(*t[0].state().lock().unwrap(), 7);
        assert!(*kept.lock().unwrap() > 1, "an old state outlives the reset");
    }
}
