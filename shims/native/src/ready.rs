//! Waking threads parked on descriptors (`perform_block`), natively.
//!
//! A thread blocked in `poll`, `read`, `accept`, `connect` or a full `write`
//! re-checks its descriptors on every turn. It used to sleep 1 ms between
//! turns, so every handoff between processes (a wineserver request and its
//! reply, a pipe between two programs) cost about a millisecond. Now the
//! turn ends early: any syscall that can change descriptor readiness
//! ([`gex_kernel::syscalls::wakes_fd_waiters`]) bumps a host-wide sequence
//! number and wakes the parked threads, which re-check at once. The 1 ms
//! turn stays as the bound for what no syscall announces (host devices,
//! timers, the compositor's own sockets).
//!
//! One sequence for the whole host rather than a queue per descriptor: a
//! wake costs every parked thread one re-check, but only syscalls that touch
//! descriptors wake anyone, and only while someone is parked.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(0);
static PARKED: AtomicUsize = AtomicUsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());
static CV: Condvar = Condvar::new();

/// The current sequence number: take it before checking the descriptors,
/// then pass it to [`wait`].
pub fn seq() -> u64 {
    SEQ.load(SeqCst)
}

/// Something may have become ready: wake every parked thread.
pub fn notify() {
    SEQ.fetch_add(1, SeqCst);
    if PARKED.load(SeqCst) > 0 {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        CV.notify_all();
    }
}

/// Park until [`notify`] has run since `seen` was taken, or `max` passes.
/// The check of the sequence and the sleep are under one lock, and a
/// notifier takes that lock to wake, so no notification is lost between
/// the caller's check of its descriptors and the sleep.
pub fn wait(seen: u64, max: Duration) {
    PARKED.fetch_add(1, SeqCst);
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if SEQ.load(SeqCst) == seen {
        let _ = CV.wait_timeout(g, max);
    }
    PARKED.fetch_sub(1, SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notify_ends_the_wait_early() {
        let s = seq();
        let t = std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            wait(s, Duration::from_secs(5));
            t0.elapsed()
        });
        std::thread::sleep(Duration::from_millis(20));
        notify();
        assert!(t.join().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn a_notify_before_the_wait_is_not_lost() {
        let s = seq();
        notify();
        let t0 = std::time::Instant::now();
        wait(s, Duration::from_secs(5));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
