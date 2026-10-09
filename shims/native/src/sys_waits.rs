//! `GEX_SYS_WAITS=1`: where guest threads spend their wall time in system
//! calls, blocking included -- the half of a slow program that a CPU
//! profile (`GEX_PROF`) cannot see. Every 10 s, on stderr, the (thread,
//! call) pairs that took the most wall time in that window:
//! `[gex-sys] 10.0 s: tid 1 futex x812 7210 ms, tid 1 read x40 1800 ms, ...`.
//! Off unless set; when off, the cost is one load per syscall.

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
}

static W: Mutex<Option<Window>> = Mutex::new(None);

pub fn record(tid: i32, nr: u64, took: Duration) {
    let mut g = W.lock().unwrap_or_else(|e| e.into_inner());
    let w = g.get_or_insert_with(|| Window {
        start: Instant::now(),
        by: HashMap::new(),
    });
    let e = w.by.entry((tid, nr)).or_default();
    e.0 += 1;
    e.1 += took;
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
        eprintln!("[gex-sys] {:.1} s: {}", dt.as_secs_f64(), line.join(", "));
        *g = None;
    }
}
