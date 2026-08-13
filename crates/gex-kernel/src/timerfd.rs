//! `timerfd`: a clock as a descriptor, for event loops (libuv arms one per
//! timer heap and lets `epoll_wait` return it).
//!
//! The object is a monotonic count of expirations. `timerfd_settime` arms it
//! with an initial delay and optional interval; a `read` returns how many
//! times it fired since the last read and clears that count. Readiness is
//! time-based -- [`poll_readable`] asks the clock -- so the handle carries its
//! own [`ClockFn`].
//!
//! Not wired: a *blocking* read does not wake precisely at the deadline; the
//! supported path is `TFD_NONBLOCK` plus `epoll`, where the loop's own
//! `epoll_wait` timeout carries the timing.
//!
//! [`poll_readable`]: crate::vfs::FileHandle::poll_readable
//! [`ClockFn`]: crate::syscalls::ClockFn

use crate::errno::{Errno, SysResult};
use crate::syscalls::ClockFn;
use crate::vfs::{FileHandle, FileType, Metadata};
use std::sync::Mutex;

/// `timerfd_settime` flag: `it_value` is an absolute time on the clock rather
/// than a delay from now.
pub const TFD_TIMER_ABSTIME: u64 = 1;

#[derive(Debug, Default, Clone, Copy)]
struct State {
    /// Absolute nanoseconds of the next expiry, or 0 when disarmed.
    next: u64,
    /// Interval in nanoseconds, or 0 for a one-shot timer.
    interval: u64,
}

/// A `timerfd` object.
pub struct TimerFd {
    clock: ClockFn,
    state: Mutex<State>,
}

impl std::fmt::Debug for TimerFd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerFd").field("state", &*self.state.lock().unwrap()).finish()
    }
}

impl TimerFd {
    pub fn new(clock: ClockFn) -> TimerFd {
        TimerFd {
            clock,
            state: Mutex::new(State::default()),
        }
    }

    fn now_ns(&self) -> u64 {
        let (secs, nanos) = (self.clock)();
        secs.saturating_mul(1_000_000_000).saturating_add(nanos as u64)
    }

    /// How many times the timer has fired at `now` but not yet been read.
    fn expirations_at(s: &State, now: u64) -> u64 {
        if s.next == 0 || now < s.next {
            return 0;
        }
        if s.interval == 0 {
            1
        } else {
            1 + (now - s.next) / s.interval
        }
    }

    /// Arm or disarm the timer. `value` is the initial delay in nanoseconds
    /// (or an absolute time when `abstime`), zero disarms; `interval` repeats.
    /// Returns the previous `(remaining, interval)` for `timerfd_gettime`-style
    /// old-value reporting.
    pub fn set(&self, value: u64, interval: u64, abstime: bool) -> (u64, u64) {
        let now = self.now_ns();
        let mut s = self.state.lock().unwrap();
        let prev = Self::remaining(&s, now);
        if value == 0 {
            *s = State::default();
        } else {
            s.next = if abstime { value } else { now.saturating_add(value) };
            s.interval = interval;
        }
        prev
    }

    /// `(remaining until next expiry, interval)` for `timerfd_gettime`. A
    /// disarmed timer reads back all zeros.
    fn remaining(s: &State, now: u64) -> (u64, u64) {
        if s.next == 0 {
            (0, s.interval)
        } else if now >= s.next {
            // Already due: Linux reports the time to the *next* tick, which for
            // a periodic timer is one interval minus how far past it is.
            if s.interval == 0 {
                (0, 0)
            } else {
                (s.interval - (now - s.next) % s.interval, s.interval)
            }
        } else {
            (s.next - now, s.interval)
        }
    }

    /// The current `(remaining, interval)`, for `timerfd_gettime`.
    pub fn get(&self) -> (u64, u64) {
        let now = self.now_ns();
        Self::remaining(&self.state.lock().unwrap(), now)
    }
}

impl FileHandle for TimerFd {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let now = self.now_ns();
        let mut s = self.state.lock().unwrap();
        let n = Self::expirations_at(&s, now);
        if n == 0 {
            return Err(Errno::EAGAIN);
        }
        if s.interval == 0 {
            *s = State::default();
        } else {
            // Advance past every tick that has already fired, so the next read
            // counts only ticks after this one.
            s.next = s.next.saturating_add(n.saturating_mul(s.interval));
        }
        buf[..8].copy_from_slice(&n.to_le_bytes());
        Ok(8)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        let now = self.now_ns();
        Self::expirations_at(&self.state.lock().unwrap(), now) > 0
    }

    fn available(&self) -> Option<u64> {
        Some(if self.poll_readable() { 8 } else { 0 })
    }

    fn timerfd_set(&self, value_ns: u64, interval_ns: u64, abstime: bool) -> Option<(u64, u64)> {
        Some(self.set(value_ns, interval_ns, abstime))
    }

    fn timerfd_get(&self) -> Option<(u64, u64)> {
        Some(self.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // A clock the test drives by hand. `ClockFn` is a bare `fn` pointer, so the
    // "current time" lives in a static the closure-free function reads.
    static CLOCK_NS: AtomicU64 = AtomicU64::new(0);
    fn test_clock() -> (u64, u32) {
        let ns = CLOCK_NS.load(Ordering::Relaxed);
        (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
    }
    fn set_now(ns: u64) {
        CLOCK_NS.store(ns, Ordering::Relaxed);
    }
    fn read(t: &TimerFd) -> Result<u64, Errno> {
        let mut b = [0u8; 8];
        t.read_at(0, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    #[test]
    fn a_one_shot_fires_once() {
        set_now(0);
        let t = TimerFd::new(test_clock);
        t.set(100, 0, false); // 100ns from now
        assert!(!t.poll_readable());
        assert_eq!(read(&t).err(), Some(Errno::EAGAIN));
        set_now(100);
        assert!(t.poll_readable());
        assert_eq!(read(&t).unwrap(), 1);
        // Cleared, and one-shot does not re-arm.
        assert!(!t.poll_readable());
        set_now(1_000_000);
        assert!(!t.poll_readable());
    }

    #[test]
    fn a_periodic_timer_counts_missed_ticks() {
        set_now(0);
        let t = TimerFd::new(test_clock);
        t.set(100, 100, false); // first at 100, every 100 after
        set_now(100);
        assert_eq!(read(&t).unwrap(), 1);
        // Fall four intervals behind before reading again.
        set_now(100 + 4 * 100);
        assert!(t.poll_readable());
        assert_eq!(read(&t).unwrap(), 4);
        // And it keeps going.
        assert!(!t.poll_readable());
        set_now(100 + 5 * 100);
        assert_eq!(read(&t).unwrap(), 1);
    }

    #[test]
    fn abstime_arms_at_an_absolute_point() {
        set_now(500);
        let t = TimerFd::new(test_clock);
        t.set(1000, 0, true); // absolute 1000, i.e. 500ns from now
        assert!(!t.poll_readable());
        set_now(1000);
        assert_eq!(read(&t).unwrap(), 1);
    }

    #[test]
    fn setting_zero_disarms() {
        set_now(0);
        let t = TimerFd::new(test_clock);
        t.set(100, 0, false);
        t.set(0, 0, false); // disarm
        set_now(1000);
        assert!(!t.poll_readable());
        assert_eq!(t.get(), (0, 0));
    }

    #[test]
    fn gettime_reports_remaining() {
        set_now(0);
        let t = TimerFd::new(test_clock);
        t.set(1000, 250, false);
        set_now(400);
        let (rem, int) = t.get();
        assert_eq!((rem, int), (600, 250));
    }

    #[test]
    fn a_short_read_buffer_is_einval() {
        set_now(0);
        let t = TimerFd::new(test_clock);
        t.set(1, 0, false);
        set_now(1);
        assert_eq!(t.read_at(0, &mut [0u8; 4]).err(), Some(Errno::EINVAL));
    }
}
