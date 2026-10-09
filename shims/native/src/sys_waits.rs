//! `GEX_SYS_WAITS=1`: where guest threads spend their wall time in system
//! calls, blocking included -- the half of a slow program that a CPU
//! profile (`GEX_PROF`) cannot see. Every 10 s, on stderr, the (thread,
//! call) pairs that took the most wall time in that window:
//! `[gex-sys] 10.0 s: tid 1 futex x812 7210 ms, tid 1 read x40 1800 ms, ...`,
//! and the threads that spent the most time outside syscalls (running guest
//! code, or in a host call such as a GL submit):
//! `[gex-sys] outside: tid 1 9100 ms, ...`, credited when the thread's next
//! syscall starts (so one long gap can exceed the window). Outside is not
//! CPU: a wasm32 guest's futex waits are `memory.atomic.wait`, no syscall,
//! so a thread parked in pthread_cond_wait counts there; compare
//! `GEX_PROF`'s per-thread CPU. Thread ids are the guest's, which may repeat
//! across processes. Off unless set; when off, the cost is one load per
//! syscall.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_SYS_WAITS").is_some_and(|v| v != "0"))
}

struct Window {
    start: Instant,
    by: HashMap<(i32, u64), (u64, Duration)>,
    outside: HashMap<i32, Duration>,
}

struct State {
    window: Option<Window>,
    /// When each thread's last syscall returned.
    last_exit: Option<HashMap<i32, Instant>>,
}

static S: Mutex<State> = Mutex::new(State {
    window: None,
    last_exit: None,
});

/// A syscall `nr` by `tid` that started at `t0` and has just returned.
pub fn record(tid: i32, nr: u64, t0: Instant) {
    let end = Instant::now();
    let mut g = S.lock().unwrap_or_else(|e| e.into_inner());
    let gap = g
        .last_exit
        .get_or_insert_with(HashMap::new)
        .insert(tid, end)
        .map(|l| t0.saturating_duration_since(l));
    let w = g.window.get_or_insert_with(|| Window {
        start: Instant::now(),
        by: HashMap::new(),
        outside: HashMap::new(),
    });
    let e = w.by.entry((tid, nr)).or_default();
    e.0 += 1;
    e.1 += end - t0;
    if let Some(gap) = gap {
        *w.outside.entry(tid).or_default() += gap;
    }
    let dt = w.start.elapsed();
    if dt >= Duration::from_secs(10) {
        let mut top: Vec<_> = w.by.iter().map(|(&(t, n), &(c, d))| (d, t, n, c)).collect();
        top.sort_by(|a, b| b.0.cmp(&a.0));
        let line: Vec<String> = top
            .iter()
            .take(12)
            .map(|(d, t, n, c)| {
                format!(
                    "tid {t} {} x{c} {} ms",
                    gex_kernel::syscalls::syscall_name(*n),
                    d.as_millis()
                )
            })
            .collect();
        let mut out: Vec<_> = w.outside.iter().map(|(&t, &d)| (d, t)).collect();
        out.sort_by(|a, b| b.cmp(a));
        let out: Vec<String> = out
            .iter()
            .take(8)
            .map(|(d, t)| format!("tid {t} {} ms", d.as_millis()))
            .collect();
        eprintln!("[gex-sys] {:.1} s: {}", dt.as_secs_f64(), line.join(", "));
        eprintln!("[gex-sys] outside: {}", out.join(", "));
        g.window = None;
    }
}
