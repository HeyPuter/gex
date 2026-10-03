//! `GEX_PROF=<file>`: a flat CPU profile of the whole process, for
//! attributing time without `perf` (docs/noita-perf.md).
//!
//! `ITIMER_PROF` delivers SIGPROF to whichever thread is burning CPU, every
//! millisecond of process CPU time; the handler records the interrupted PC
//! (atomics only, so it is async-signal-safe). A thread writes the
//! histogram (`pc count`, hex) to the file every 10 s and `/proc/self/maps`
//! beside it (`<file>.maps`), so a run ended by a timeout still leaves a
//! profile. Symbolise with `GEX_PERFMAP=1`'s /tmp/perf-<pid>.map for
//! compiled wasm and the binaries' symbol tables for the rest
//! (tools/prof-report.py).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const N: usize = 1 << 23;
static SAMPLES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static NEXT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_prof(_sig: i32, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel hands a SA_SIGINFO handler a valid ucontext_t.
    let pc = unsafe {
        let uc = &*(ctx as *const libc::ucontext_t);
        #[cfg(target_arch = "x86_64")]
        {
            uc.uc_mcontext.gregs[libc::REG_RIP as usize] as u64
        }
        #[cfg(target_arch = "aarch64")]
        {
            uc.uc_mcontext.pc
        }
    };
    let i = NEXT.fetch_add(1, Ordering::Relaxed);
    if i < N {
        SAMPLES[i].store(pc, Ordering::Relaxed);
    }
}

/// Start profiling if `GEX_PROF` names a file. Once per process.
pub fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let Some(path) = std::env::var_os("GEX_PROF") else { return };
    ONCE.call_once(|| {
        // SAFETY: plain libc calls with initialised arguments.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_prof as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
        }
        // `GEX_PROF_START=<s>`: start sampling that many seconds in (a
        // steady-state profile, past loading).
        let start = std::env::var("GEX_PROF_START").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let path = std::path::PathBuf::from(path);
        gex_native::aot_share::set_sampler(Box::new(take_new));
        let _ = std::thread::Builder::new().name("gex-prof".into()).spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(start));
            // SAFETY: as above; the timer is the process's, set from any thread.
            unsafe {
                let tv = libc::timeval { tv_sec: 0, tv_usec: 1000 };
                let it = libc::itimerval { it_interval: tv, it_value: tv };
                libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                dump(&path);
            }
        });
    });
}

/// The samples since the previous call (for `gex_native::aot_share`).
fn take_new() -> Vec<u64> {
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.load(Ordering::Relaxed).min(N);
    let from = SEEN.swap(n, Ordering::Relaxed).min(n);
    SAMPLES[from..n].iter().map(|s| s.load(Ordering::Relaxed)).collect()
}

/// Write the profile now (at the end of a run).
pub fn finish() {
    if let Some(path) = std::env::var_os("GEX_PROF") {
        dump(std::path::Path::new(&path));
    }
}

fn dump(path: &std::path::Path) {
    let n = NEXT.load(Ordering::Relaxed).min(N);
    let mut hist: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for s in &SAMPLES[..n] {
        *hist.entry(s.load(Ordering::Relaxed)).or_default() += 1;
    }
    let mut out = format!("pid {}\nsamples {}\n", std::process::id(), n);
    for (pc, c) in hist {
        out.push_str(&format!("{pc:x} {c}\n"));
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    if let Ok(maps) = std::fs::read("/proc/self/maps") {
        let _ = std::fs::write(path.with_extension("maps"), maps);
    }
}
