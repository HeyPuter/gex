//! Processor time, natively. A guest thread is a host thread here, so its
//! time is the host thread's, as Linux's scheduler counts it
//! (`/proc/self/task/<tid>/schedstat`, nanoseconds on a processor), and a
//! process's is its threads': the live ones read as they go, the ended ones
//! kept. As the browser shim's `usage.rs` does, a process says its total to
//! the process table (what another process's `/proc/<pid>/stat`,
//! `/proc/stat` and a parent's `getrusage(RUSAGE_CHILDREN)` read): at a
//! system call once 50 ms have passed since it last did, and as each of its
//! threads ends. Each thread says its own time the same way, to its task's
//! count (`ProcTable::add_task_cpu`, `/proc/<pid>/task/<tid>/stat`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use gex_kernel::proc::ProcTable;
use gex_kernel::syscalls::Kernel;

/// `USER_HZ`'s tick: what `/proc` counts in, so saying less is not worth it.
const SAY_EVERY_NS: u64 = 10_000_000;
/// How often a running thread looks (a look reads a file per thread).
const LOOK_EVERY: Duration = Duration::from_millis(50);

/// A live thread of a process: its host thread, and what that thread had
/// used when it began running this process (a host thread an `execve` keeps
/// has run the program before).
struct Running {
    tid: u32,
    base: u64,
}

#[derive(Default)]
struct Proc {
    /// Its ended threads' time.
    done: u64,
    live: HashMap<u64, Running>,
    said: u64,
    /// When it was first seen (monotonic).
    start_ns: u64,
}

static PROCS: LazyLock<Mutex<HashMap<i32, Proc>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Mine {
    pid: i32,
    /// The guest thread, and this host thread's time when it last said it.
    tid: i32,
    task_said: u64,
    token: u64,
    procs: Arc<ProcTable>,
    mm: gex_kernel::mem::Mm,
    calls: u32,
    looked: Instant,
}

thread_local! {
    static MINE: RefCell<Option<Mine>> = const { RefCell::new(None) };
}

/// A schedstat's first field: time on a processor, in nanoseconds.
fn schedstat(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.split_whitespace().next()?.parse().ok()
}

fn thread_now() -> u64 {
    schedstat("/proc/thread-self/schedstat").unwrap_or(0)
}

fn of_thread(tid: u32) -> Option<u64> {
    schedstat(&format!("/proc/self/task/{tid}/schedstat"))
}

/// This host thread's id: `/proc/thread-self` is `<pid>/task/<tid>`.
fn host_tid() -> u32 {
    std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|p| p.file_name()?.to_str()?.parse().ok())
        .unwrap_or(0)
}

/// The kernel's processor-time clocks read these from now on.
pub(crate) fn install() {
    gex_kernel::proc::set_cpu_clock(cpu_ns);
}

/// This host thread runs a thread of `k`'s process until the guard drops.
pub(crate) struct Counting(());

impl Counting {
    pub(crate) fn enter(k: &Kernel) -> Counting {
        leave();
        let token = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let start = gex_kernel::syscalls::clock_ns(k.clock, 1);
        let base = thread_now();
        let running = Running { tid: host_tid(), base };
        {
            let mut all = PROCS.lock().unwrap_or_else(|e| e.into_inner());
            let p = all.entry(k.pid).or_insert_with(|| Proc { start_ns: start, ..Proc::default() });
            p.live.insert(token, running);
        }
        let mine = Mine { pid: k.pid, tid: k.tid, task_said: base, token, procs: Arc::clone(&k.procs), mm: k.mem.shared(), calls: 0, looked: Instant::now() };
        MINE.with(|m| *m.borrow_mut() = Some(mine));
        Counting(())
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        leave();
    }
}

/// This thread's stretch ends: its time is the process's, said now.
fn leave() {
    let Some(mut mine) = MINE.with(|m| m.borrow_mut().take()) else { return };
    let now = thread_now();
    say_task(&mut mine, now, 0);
    let total = {
        let mut all = PROCS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = all.get_mut(&mine.pid) else { return };
        if let Some(r) = p.live.remove(&mine.token) {
            p.done += now.saturating_sub(r.base);
        }
        p.done
    };
    // the other live threads, read outside the lock
    say(&mine, total + live_of(mine.pid));
}

/// The live threads' time (a thread gone meanwhile counts once its own
/// `leave` has added it).
fn live_of(pid: i32) -> u64 {
    let live: Vec<(u32, u64)> = {
        let all = PROCS.lock().unwrap_or_else(|e| e.into_inner());
        all.get(&pid).map(|p| p.live.values().map(|r| (r.tid, r.base)).collect()).unwrap_or_default()
    };
    live.into_iter().map(|(tid, base)| of_thread(tid).map_or(0, |t| t.saturating_sub(base))).sum()
}

fn total(pid: i32) -> u64 {
    let done = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get(&pid).map_or(0, |p| p.done);
    done + live_of(pid)
}

/// `gex_kernel::proc::set_cpu_clock`: this thread's, or its process's.
fn cpu_ns(thread: bool) -> u64 {
    if thread {
        return thread_now();
    }
    MINE.with(|m| m.borrow().as_ref().map(|m| m.pid)).map_or(0, total)
}

/// A system call: every 64th looks at the clock, and once `LOOK_EVERY` has
/// passed, the process's time is said if it has grown by a tick, so a
/// process that runs for long is seen as it goes.
pub(crate) fn tick() {
    MINE.with(|m| {
        let mut m = m.borrow_mut();
        let Some(mine) = m.as_mut() else { return };
        mine.calls = mine.calls.wrapping_add(1);
        if mine.calls % 64 != 0 || mine.looked.elapsed() < LOOK_EVERY {
            return;
        }
        mine.looked = Instant::now();
        say_task(mine, thread_now(), SAY_EVERY_NS);
        let t = total(mine.pid);
        let said = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get(&mine.pid).map_or(0, |p| p.said);
        if t.saturating_sub(said) >= SAY_EVERY_NS {
            say(mine, t);
        }
    });
}

/// This thread's time since it last said it, said to its task once it is
/// `at_least`.
fn say_task(mine: &mut Mine, now: u64, at_least: u64) {
    let d = now.saturating_sub(mine.task_said);
    if d > 0 && d >= at_least {
        mine.procs.add_task_cpu(mine.tid, d);
        mine.task_said = now;
    }
}

fn say(mine: &Mine, total: u64) {
    let start = {
        let mut all = PROCS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = all.get_mut(&mine.pid) else { return };
        p.said = p.said.max(total);
        p.start_ns
    };
    let (size, rss, _) = mine.mm.lock().vm_counters();
    mine.procs.set_usage(mine.pid, total, rss, size, start);
}

/// The process `pid` is gone: its record goes too (the table keeps its time).
pub(crate) fn forget(pid: i32) {
    PROCS.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
}
