//! `GEX_STALL=MS`: when no guest thread has entered a system call for MS
//! milliseconds, say what every guest thread is doing -- inside a call
//! (which, for how long) or outside one (running guest code, or parked in a
//! wasm32 `memory.atomic.wait`, which is no syscall) -- and how much CPU
//! each used over the next 200 ms, which tells a busy thread from a waiting
//! one. `GEX_STALL_EVERY=MS` dumps on that period regardless.
//! `GEX_STALL_FRAME=MS` dumps when the compositor has shown no new frame
//! for MS (and again each further MS): a GUI session that boots to nothing
//! is often busy with syscalls the whole time, so only the screen says it
//! stalled. For the
//! stalls a CPU profile cannot see: a whole session idle, waiting on a
//! wakeup that comes late. Off unless set; when off, one load per syscall.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Conf {
    quiet: Option<Duration>,
    every: Option<Duration>,
    frame: Option<Duration>,
}

fn conf() -> &'static Conf {
    static C: OnceLock<Conf> = OnceLock::new();
    C.get_or_init(|| {
        let ms = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis);
        Conf { quiet: ms("GEX_STALL"), every: ms("GEX_STALL_EVERY"), frame: ms("GEX_STALL_FRAME") }
    })
}

pub fn on() -> bool {
    let c = conf();
    c.quiet.is_some() || c.every.is_some() || c.frame.is_some()
}

/// Milliseconds since `START` of the compositor's latest frame.
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);

/// The compositor showed a frame (`GEX_STALL_FRAME`).
pub fn frame() {
    if conf().frame.is_some() {
        LAST_FRAME_MS.store(start().elapsed().as_millis() as u64, Relaxed);
    }
}

#[derive(Clone)]
struct Entry {
    tid: i32,
    pid: i32,
    /// The raw number of the call it is in, or `None` when outside one.
    inside: Option<u64>,
    /// That call's canonical number, once the kernel has decoded it.
    call: Option<u64>,
    since: Instant,
    last: u64,
    /// The latest call-out from a unit: target, return address; how many
    /// call-outs, and how many of them ended without the callee returning.
    co: (u64, u64),
    co_n: u64,
    co_aborts: u64,
}

static START: OnceLock<Instant> = OnceLock::new();
/// Milliseconds since `START` of the latest syscall entry by any thread.
static LAST_ENTRY_MS: AtomicU64 = AtomicU64::new(0);
static THREADS: Mutex<Option<HashMap<u32, Entry>>> = Mutex::new(None);

thread_local! {
    static HOST_TID: u32 = std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse().ok()))
        .unwrap_or(0);
}

fn start() -> Instant {
    *START.get_or_init(|| {
        let _ = std::thread::Builder::new().name("gex-stall".into()).spawn(watch);
        Instant::now()
    })
}

/// Guest thread `tid` of process `pid` is entering raw syscall `nr`.
pub fn enter(pid: i32, tid: i32, nr: u64) {
    let now = Instant::now();
    LAST_ENTRY_MS.store(now.duration_since(start()).as_millis() as u64, Relaxed);
    let host = HOST_TID.with(|t| *t);
    let mut g = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    let e = g.get_or_insert_with(HashMap::new).entry(host).or_insert(Entry { tid, pid, inside: None, call: None, since: now, last: !0, co: (0, 0), co_n: 0, co_aborts: 0 });
    e.tid = tid;
    e.pid = pid;
    e.inside = Some(nr);
    e.call = None;
    e.since = now;
}

/// The kernel has decoded the call this thread is in: its canonical number,
/// which a stall dump names (the raw number is the guest ABI's).
pub fn decoded(canonical: u64) {
    let host = HOST_TID.with(|t| *t);
    let mut g = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.get_or_insert_with(HashMap::new).get_mut(&host) {
        e.call = Some(canonical);
    }
}

/// The call it entered has returned; `name` is the kernel's canonical number.
pub fn exit(canonical: u64) {
    let host = HOST_TID.with(|t| *t);
    let mut g = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.get_or_insert_with(HashMap::new).get_mut(&host) {
        e.inside = None;
        e.since = Instant::now();
        e.last = canonical;
    }
}

/// A unit's call-out to `target`, returning to `ret`, has ended; `aborted`
/// when the callee did not return through it.
pub fn callout(target: u64, ret: u64, aborted: bool) {
    let host = HOST_TID.with(|t| *t);
    let mut g = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    let e = g.get_or_insert_with(HashMap::new).entry(host).or_insert(Entry {
        tid: 0,
        pid: 0,
        inside: None,
        call: None,
        since: Instant::now(),
        last: !0,
        co: (0, 0),
        co_n: 0,
        co_aborts: 0,
    });
    e.co = (target, ret);
    e.co_n += 1;
    e.co_aborts += aborted as u64;
}

fn cpu_ms(host: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/self/task/{host}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime, stime: fields 14 and 15 of the whole line, in clock ticks.
    let (u, st): (u64, u64) = (f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?);
    Some((u + st) * 10)
}

/// Every host thread's name and CPU, by host tid.
fn all_threads_cpu() -> HashMap<u32, (String, u64)> {
    let mut out = HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc/self/task") {
        for e in rd.flatten() {
            if let Some(h) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) {
                if let Some(c) = cpu_ms(h) {
                    out.insert(h, (comm(h), c));
                }
            }
        }
    }
    out
}

fn comm(host: u32) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{host}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn dump(why: &str) {
    let snap: Vec<(u32, Entry)> = {
        let g = THREADS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect()).unwrap_or_default()
    };
    // Threads that have exited have no /proc entry: left out.
    let before: HashMap<u32, u64> = snap.iter().filter_map(|(h, _)| Some((*h, cpu_ms(*h)?))).collect();
    let all_before = all_threads_cpu();
    std::thread::sleep(Duration::from_millis(200));
    let all_after = all_threads_cpu();
    // The host's other threads (compilers, the compositor, GPU) by name.
    let mut others: HashMap<String, u64> = HashMap::new();
    for (h, (name, a)) in &all_after {
        if snap.iter().any(|(g, _)| g == h) {
            continue;
        }
        let d = a.saturating_sub(all_before.get(h).map(|x| x.1).unwrap_or(0));
        if d > 0 {
            *others.entry(name.clone()).or_default() += d;
        }
    }
    let t = start().elapsed().as_secs_f64();
    let mut lines = Vec::new();
    for (h, e) in &snap {
        let (Some(b), Some(a)) = (before.get(h), cpu_ms(*h)) else { continue };
        let state = match e.inside {
            Some(nr) => match e.call {
                Some(c) => format!("in {} (raw {nr})", gex_kernel::syscalls::syscall_name(c)),
                None => format!("in raw {nr}"),
            },
            None => format!("outside (last {})", gex_kernel::syscalls::syscall_name(e.last)),
        };
        let co = if e.co_n > 0 {
            format!("; call-outs {} ({} aborted), last to {:#x} ret {:#x}", e.co_n, e.co_aborts, e.co.0, e.co.1)
        } else {
            String::new()
        };
        lines.push((e.pid, e.tid, format!(
            "  pid {} tid {} [{} {}] {} for {} ms, cpu {} ms/200{co}",
            e.pid, e.tid, h, comm(*h), state, e.since.elapsed().as_millis(), a.saturating_sub(*b)
        )));
    }
    lines.sort();
    let mut busy: Vec<_> = others.into_iter().collect();
    busy.sort_by(|a, b| b.1.cmp(&a.1));
    let busy: Vec<String> = busy.iter().map(|(n, d)| format!("{n} {d}")).collect();
    let mut out = format!("[gex-stall] {t:.3} s: {why}; other threads' cpu ms/200: {}\n", busy.join(", "));
    for (_, _, l) in lines {
        out.push_str(&l);
        out.push('\n');
    }
    eprint!("{out}");
}

fn watch() {
    let c = conf();
    let mut last_every = Instant::now();
    let mut dumped_for = u64::MAX;
    let mut frame_dumped_at = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let Some(st) = START.get() else { continue };
        if let Some(every) = c.every {
            if last_every.elapsed() >= every {
                last_every = Instant::now();
                dump("periodic");
            }
        }
        if let Some(q) = c.quiet {
            let last = LAST_ENTRY_MS.load(Relaxed);
            let now = st.elapsed().as_millis() as u64;
            if now.saturating_sub(last) >= q.as_millis() as u64 && dumped_for != last {
                dumped_for = last;
                dump(&format!("no syscall for {} ms", now - last));
            }
        }
        if let Some(q) = c.frame {
            let last = LAST_FRAME_MS.load(Relaxed);
            let now = st.elapsed().as_millis() as u64;
            let q = q.as_millis() as u64;
            if now.saturating_sub(last) >= q && now.saturating_sub(frame_dumped_at.max(last)) >= q {
                frame_dumped_at = now;
                dump(&format!("no frame for {} ms", now - last));
            }
        }
    }
}
