//! A value published to lock-free readers.
//!
//! gex-kernel forbids unsafe code, and handing a reader a reference to a
//! value a writer may replace at any moment needs a raw pointer and a rule
//! for when the old value may go. This is that rule, in one place:
//!
//! - the value is a `Box<T>` published through an `AtomicPtr`;
//! - a reader announces itself in an epoch array (one slot per reader
//!   thread, the global epoch while it reads, 0 otherwise), loads the
//!   pointer, and looks;
//! - a writer takes the cell's mutex, builds the new value from the old one,
//!   swaps it in, moves the epoch on, and drops the old value once every
//!   slot that was reading at or before the swap has moved on. Readers look
//!   for a few loads, so that is almost always at once, which matters to a
//!   caller whose old value holds something that must close now (a
//!   descriptor table's pipe ends).
//!
//! A reader with no slot (slot 0, or past the array: a native thread, the
//! single-threaded page) reads under the writers' mutex instead.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering::SeqCst};
use std::sync::Mutex;

pub struct Published<T> {
    cur: AtomicPtr<T>,
    writer: Mutex<()>,
    epoch: AtomicU64,
    slots: Box<[AtomicU64]>,
    /// Old values a reader held on to past the writer's wait, with the
    /// epoch they were replaced at; dropped by a later write.
    retired: Mutex<Vec<(u64, Box<T>)>>,
    /// Old values taken off `retired` and being dropped, outside its lock
    /// (a value's drop may be long): what a `settle` waits for too.
    dropping: AtomicU64,
}

// SAFETY: a `Published<T>` hands out `&T` to any thread and drops `T`s on
// whichever thread writes, so it is exactly as shareable as `T` is.
unsafe impl<T: Send + Sync> Sync for Published<T> {}
unsafe impl<T: Send> Send for Published<T> {}

/// A reader's announcement: clears its slot when dropped, panic or not.
struct Pin<'a>(&'a AtomicU64);

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.0.store(0, SeqCst);
    }
}

impl<T> Published<T> {
    /// `v` published, with `slots` reader slots (slot ids `1..slots`).
    pub fn new(v: T, slots: usize) -> Self {
        Published {
            cur: AtomicPtr::new(Box::into_raw(Box::new(v))),
            writer: Mutex::new(()),
            epoch: AtomicU64::new(1),
            slots: (0..slots.max(1)).map(|_| AtomicU64::new(0)).collect(),
            retired: Mutex::new(Vec::new()),
            dropping: AtomicU64::new(0),
        }
    }

    /// Look at the current value from reader slot `slot`. `f` should be
    /// short (a lookup and a reference count): a writer waits for it. A
    /// slot belongs to one thread, and reads from it do not nest.
    pub fn read<R>(&self, slot: usize, f: impl FnOnce(&T) -> R) -> R {
        if slot == 0 || slot >= self.slots.len() {
            let _g = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `cur` is always a live `Box::into_raw` pointer; it is
            // replaced only under `writer`, which is held, and an old value
            // is freed only after its replacement.
            return f(unsafe { &*self.cur.load(SeqCst) });
        }
        let s = &self.slots[slot];
        // The slot's store before the pointer's load, and a writer's swap
        // before its epoch bump: a reader that pinned an epoch after the
        // bump sees the new pointer.
        s.store(self.epoch.load(SeqCst), SeqCst);
        let _pin = Pin(s);
        // SAFETY: the value is freed only once every slot pinned at or
        // before the epoch of its replacement has cleared (`write`); this
        // slot is pinned, so the value it loads outlives `f`.
        f(unsafe { &*self.cur.load(SeqCst) })
    }

    /// Replace the value: `f` gets the current one and returns the new one
    /// (or `None`, which leaves it alone) and an answer. Writers are
    /// serialised; the replaced value is dropped before this returns unless
    /// a reader holds on to it unusually long, in which case a later write
    /// drops it.
    pub fn write<R>(&self, f: impl FnOnce(&T) -> (Option<T>, R)) -> R {
        let (replaced, r) = {
            let _g = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `writer` is held, so `cur` is not replaced or freed
            // while `f` looks at it.
            let (new, r) = f(unsafe { &*self.cur.load(SeqCst) });
            match new {
                Some(v) => {
                    let old = self.cur.swap(Box::into_raw(Box::new(v)), SeqCst);
                    // Retired before the next writer runs, so a value
                    // replaced is always somewhere a `settle` looks.
                    let e = self.epoch.fetch_add(1, SeqCst);
                    // SAFETY: `old` came from `Box::into_raw` and is no
                    // longer published; it is owned here.
                    let old = unsafe { Box::from_raw(old) };
                    self.retired.lock().unwrap_or_else(|e| e.into_inner()).push((e, old));
                    (true, r)
                }
                None => (false, r),
            }
        };
        if replaced {
            // Readers hold a pin for a few loads; a bounded wait for them.
            for _ in 0..1_000_000 {
                if self.reclaim() {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        r
    }

    /// Wait until every value replaced before this call is dropped: each
    /// reader that could see one has moved on (`synchronize_rcu`). Readers
    /// look for a few loads, so this is short; it is for a caller that must
    /// know an old value's contents are gone.
    pub fn settle(&self) {
        let now = self.epoch.load(SeqCst);
        loop {
            self.reclaim();
            let done = self.retired.lock().unwrap_or_else(|e| e.into_inner()).iter().all(|r| r.0 >= now);
            if done && self.dropping.load(SeqCst) == 0 {
                return;
            }
            std::hint::spin_loop();
        }
    }

    /// Drop what no reader can see; true when nothing is left waiting.
    fn reclaim(&self) -> bool {
        // The epoch before the slots are looked at bounds what they can
        // vouch for. This runs outside the writers' mutex, so a value can be
        // retired after the scan below, while a reader pinned since the scan
        // is still holding it: with no bound, a scan that found every slot
        // idle freed that value too (the settle test's intermittent SIGSEGV).
        // A value retired before this load was replaced before the scan, so
        // a reader that still holds it was pinned during the scan.
        let limit = self.epoch.load(SeqCst);
        let oldest = self
            .slots
            .iter()
            .map(|s| s.load(SeqCst))
            .filter(|&e| e != 0)
            .min()
            .unwrap_or(u64::MAX)
            .min(limit);
        let (gone, empty) = {
            let mut r = self.retired.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = Vec::new();
            let mut i = 0;
            while i < r.len() {
                if r[i].0 < oldest {
                    gone.push(r.swap_remove(i).1);
                } else {
                    i += 1;
                }
            }
            // Counted while still under the lock, so a `settle` that finds
            // the list empty also finds these.
            if !gone.is_empty() {
                self.dropping.fetch_add(1, SeqCst);
            }
            (gone, r.is_empty())
        };
        if !gone.is_empty() {
            drop(gone);
            self.dropping.fetch_sub(1, SeqCst);
        }
        empty
    }

    /// Which value is current: it changes when a write replaces it, after
    /// the new one is in. A reader that kept a copy of the value it read
    /// along with this, and finds it unchanged, holds the current value (or
    /// the one a write in progress is replacing).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(SeqCst)
    }

    /// The value, for its only owner.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self`: nobody else can be reading or writing.
        unsafe { &mut *self.cur.load(SeqCst) }
    }

    /// How many replaced values wait for a reader (a test's question).
    pub fn pending(&self) -> usize {
        self.retired.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl<T> Drop for Published<T> {
    fn drop(&mut self) {
        // SAFETY: the last owner: no reader or writer is left.
        drop(unsafe { Box::from_raw(*self.cur.get_mut()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn a_write_drops_the_old_value_at_once_with_no_reader() {
        let v = Arc::new(1);
        let w = Arc::downgrade(&v);
        let p = Published::new(v, 4);
        p.write(|_| (Some(Arc::new(2)), ()));
        assert!(w.upgrade().is_none(), "the replaced value outlived the write");
        assert_eq!(p.read(1, |v| **v), 2);
        assert_eq!(p.read(0, |v| **v), 2);
    }

    #[test]
    fn settle_drops_every_value_replaced_before_it() {
        // Readers busy on their slots and other writers replacing it
        // throughout: once a write and a settle are done, nothing the
        // write replaced is left, however long its own wait was.
        let p = Arc::new(Published::new(Arc::new(0u64), 8));
        let stop = Arc::new(AtomicBool::new(false));
        let busy: Vec<_> = (1..7)
            .map(|slot| {
                let (p, stop) = (p.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(SeqCst) {
                        if slot % 2 == 0 {
                            p.write(|v| (Some(Arc::new(v.wrapping_add(1))), ()));
                        } else {
                            p.read(slot, |v| std::hint::black_box(**v));
                        }
                    }
                })
            })
            .collect();
        for _ in 0..2_000 {
            let mine = Arc::new(u64::MAX);
            let w = Arc::downgrade(&mine);
            p.write(|_| (Some(mine), ()));
            p.write(|v| (Some(Arc::new(**v)), ()));
            p.settle();
            // Replaced by the second write at the latest: gone.
            assert_eq!(w.strong_count(), 0, "a replaced value outlived settle");
        }
        stop.store(true, SeqCst);
        for b in busy {
            b.join().unwrap();
        }
    }

    /// A value whose drop marks it dead, so a reader still holding it after
    /// it was freed would notice (natively; the page has no sanitizer).
    struct Snap(Arc<AtomicBool>);
    impl Drop for Snap {
        fn drop(&mut self) {
            self.0.store(false, SeqCst);
        }
    }

    #[test]
    fn readers_never_see_a_freed_value() {
        let p = Arc::new(Published::new(Snap(Arc::new(AtomicBool::new(true))), 8));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (1..5)
            .map(|slot| {
                let (p, stop) = (p.clone(), stop.clone());
                std::thread::spawn(move || {
                    // At least one read, even if the writer finishes before this thread
                    // first runs (a loaded machine running many tests at once).
                    let mut n = 0u64;
                    loop {
                        p.read(slot, |s| {
                            for _ in 0..50 {
                                assert!(s.0.load(SeqCst), "read a freed value");
                            }
                        });
                        n += 1;
                        if stop.load(SeqCst) {
                            break n;
                        }
                    }
                })
            })
            .collect();
        // And one with no slot, under the mutex.
        let (p2, stop2) = (p.clone(), stop.clone());
        let slotless = std::thread::spawn(move || {
            while !stop2.load(SeqCst) {
                p2.read(0, |s| assert!(s.0.load(SeqCst)));
            }
        });
        for _ in 0..5_000 {
            p.write(|_| (Some(Snap(Arc::new(AtomicBool::new(true)))), ()));
        }
        stop.store(true, SeqCst);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
        slotless.join().unwrap();
        assert_eq!(p.pending(), 0);
    }
}
