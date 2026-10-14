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
//!
//! Each sample also records the thread it hit, and every thread with at
//! least `GEX_PROF_MIN_PCT` (default 5) percent of the samples gets a
//! profile of its own beside the file, `<stem>-<thread name>-<tid>.<ext>`
//! (and its `.maps`), in the same format: what the main thread, the
//! critical path of a game's frame, spends its time on. Every thread's
//! total is in `<stem>.threads`.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

const N: usize = 1 << 23;
static SAMPLES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static TIDS: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static NEXT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_prof(_sig: i32, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel hands a SA_SIGINFO handler a valid ucontext_t.
    let pc = unsafe {
        let uc = &*(ctx as *const libc::ucontext_t);
        #[cfg(target_arch = "x86_64")]
        {
            uc.uc_mcontext.gregs[libc::REG_RIP as usize] as u64
        }
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        {
            uc.uc_mcontext.pc
        }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            (*uc.uc_mcontext).__ss.__pc
        }
    };
    let i = NEXT.fetch_add(1, Ordering::Relaxed);
    if i < N {
        SAMPLES[i].store(pc, Ordering::Relaxed);
        TIDS[i].store(tid(), Ordering::Relaxed);
    }
}

/// The calling thread's id (async-signal-safe).
#[cfg(target_os = "linux")]
fn tid() -> u32 {
    // SAFETY: gettid is async-signal-safe.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(target_os = "macos")]
fn tid() -> u32 {
    // SAFETY: pthread_mach_thread_np only reads the calling thread's port.
    unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
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
            // `GEX_PROF_SHARE=1`: the AOT-share line (which tier the samples
            // hit) every 10 s on stderr, for guests with no GEX_NT_STATS
            // (Linux programs).
            let share = std::env::var_os("GEX_PROF_SHARE").is_some_and(|v| v != "0");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                if share {
                    if let Some(l) = gex_native::aot_share::line() {
                        eprintln!("[gex-prof] {l}");
                    }
                }
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
    let maps = std::fs::read("/proc/self/maps").ok();
    let mut hist: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut by_tid: std::collections::HashMap<u32, std::collections::HashMap<u64, u64>> = Default::default();
    for (s, t) in SAMPLES[..n].iter().zip(&TIDS[..n]) {
        let pc = s.load(Ordering::Relaxed);
        *hist.entry(pc).or_default() += 1;
        *by_tid.entry(t.load(Ordering::Relaxed)).or_default().entry(pc).or_default() += 1;
    }
    write_one(path, &hist, maps.as_deref());
    // Threads with at least GEX_PROF_MIN_PCT (default 5) percent of the
    // samples, each on its own; every thread's total in `<stem>.threads`
    // (`tid comm samples`, most first), so the time spread over many small
    // threads (compile workers, guest threads) is still attributable.
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "prof".into());
    let min_pct = std::env::var("GEX_PROF_MIN_PCT").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(5);
    let mut totals = Vec::new();
    for (tid, h) in by_tid {
        let k: u64 = h.values().sum();
        let comm = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
            .map(|c| c.trim().replace(['/', ' ', ':'], "_"))
            .unwrap_or_else(|_| "gone".into());
        totals.push((k, tid, comm.clone()));
        if tid == 0 || k * 100 < n as u64 * min_pct {
            continue;
        }
        write_one(&path.with_file_name(format!("{stem}-{comm}-{tid}.{ext}")), &h, maps.as_deref());
    }
    totals.sort_by(|a, b| b.cmp(a));
    let list: String = totals.iter().map(|(k, t, c)| format!("{t} {c} {k}\n")).collect();
    let _ = std::fs::write(path.with_extension("threads"), list);
}

/// One histogram as a profile file (`pid`, `samples`, `pc count`), with
/// the maps beside it.
fn write_one(path: &std::path::Path, hist: &std::collections::HashMap<u64, u64>, maps: Option<&[u8]>) {
    let n: u64 = hist.values().sum();
    let mut out = format!("pid {}\nsamples {}\n", std::process::id(), n);
    for (pc, c) in hist {
        out.push_str(&format!("{pc:x} {c}\n"));
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    if let Some(m) = maps {
        let _ = std::fs::write(path.with_extension("maps"), m);
    }
}
