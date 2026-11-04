//! Where one system call's time goes, phase by phase: a measuring build's
//! (feature `phases`; the page's `GEX_PHASE_MARKS=1 hosts/browser/build.sh`),
//! read with the run's `GEX_PHASES=1` (`gex_set_phases`). Without the
//! feature every mark compiles to nothing. The caller marks the end of each
//! phase with [`mark`]; the time since the last mark goes to that phase.
//! A call [`begin`]s at its entry and [`end`]s at its return, kept only if
//! [`keep`] said so (one family of calls at a time, so the phases are one
//! call's). Built in but not asked for (no clock given): a mark is a
//! thread-local load.

/// The phases' slots.
pub const N: usize = 24;

#[cfg(not(feature = "phases"))]
pub use off::*;
#[cfg(feature = "phases")]
pub use on::*;

#[cfg(not(feature = "phases"))]
mod off {
    pub fn set_clock(_: fn() -> u64) {}
    pub fn enabled() -> bool {
        false
    }
    #[inline(always)]
    pub fn begin() {}
    #[inline(always)]
    pub fn keep() {}
    #[inline(always)]
    pub fn mark(_: usize) {}
    #[inline(always)]
    pub fn end(_: usize) -> u64 {
        0
    }
    pub fn take() -> (Vec<(u64, u64)>, u64) {
        (Vec::new(), 0)
    }
}

#[cfg(feature = "phases")]
mod on {
    use super::N;
    use std::cell::Cell;
    use std::sync::OnceLock;

    static CLOCK: OnceLock<fn() -> u64> = OnceLock::new();

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static KEEP: Cell<bool> = const { Cell::new(false) };
        static LAST: Cell<u64> = const { Cell::new(0) };
        static CUR: [Cell<u64>; N] = const { [const { Cell::new(0) }; N] };
        static ACC: [Cell<u64>; N] = const { [const { Cell::new(0) }; N] };
        static CNT: [Cell<u64>; N] = const { [const { Cell::new(0) }; N] };
        static CALLS: Cell<u64> = const { Cell::new(0) };
    }

    /// Turn the timing on, with the host's nanosecond clock.
    pub fn set_clock(f: fn() -> u64) {
        let _ = CLOCK.set(f);
    }

    pub fn enabled() -> bool {
        CLOCK.get().is_some()
    }

    /// A call starts.
    pub fn begin() {
        let Some(c) = CLOCK.get() else { return };
        ON.with(|o| o.set(true));
        KEEP.with(|k| k.set(false));
        CUR.with(|a| a.iter().for_each(|s| s.set(0)));
        LAST.with(|l| l.set(c()));
    }

    /// This call is one of those measured.
    pub fn keep() {
        KEEP.with(|k| k.set(true));
    }

    /// The phase `slot` ends now.
    #[inline]
    pub fn mark(slot: usize) {
        if !ON.with(|o| o.get()) {
            return;
        }
        let Some(c) = CLOCK.get() else { return };
        let t = c();
        let last = LAST.with(|l| l.replace(t));
        CUR.with(|a| a[slot].set(a[slot].get() + t.saturating_sub(last)));
        CNT.with(|a| a[slot].set(a[slot].get() + KEEP.with(|k| k.get()) as u64));
    }

    /// The call returns: its phases added in if kept. The number of kept calls.
    pub fn end(slot: usize) -> u64 {
        mark(slot);
        ON.with(|o| o.set(false));
        if !KEEP.with(|k| k.get()) {
            return CALLS.with(|c| c.get());
        }
        CUR.with(|cur| {
            ACC.with(|acc| {
                cur.iter()
                    .zip(acc)
                    .for_each(|(c, a)| a.set(a.get() + c.get()))
            })
        });
        CALLS.with(|c| {
            c.set(c.get() + 1);
            c.get()
        })
    }

    /// The totals so far (nanoseconds, marks) per slot, and the calls; reset.
    pub fn take() -> (Vec<(u64, u64)>, u64) {
        let v = ACC.with(|acc| {
            CNT.with(|cnt| {
                acc.iter()
                    .zip(cnt)
                    .map(|(a, c)| (a.replace(0), c.replace(0)))
                    .collect()
            })
        });
        (v, CALLS.with(|c| c.replace(0)))
    }
}
