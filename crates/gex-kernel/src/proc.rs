//! The process table: pids, parents, process groups and sessions.
//!
//! Shared across every process, because three things cannot be answered from
//! inside a single process: pids must be unique system-wide (a per-kernel
//! counter offset by a thousand collides for a long-lived init); orphans must
//! be reparented to pid 1 and reaped there; and groups and sessions span
//! processes by definition (`kill(-pgid)`).
//!
//! Not modelled: controlling terminals, orphaned-process-group handling, and
//! session keyrings. A guest that asks gets plausible answers, not Linux's.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

/// The signal a parent is owed when a child ends.
const SIGCHLD: u32 = crate::signal::sig::SIGCHLD;
use std::sync::{Arc, Mutex};

use crate::errno::Errno;
use crate::syscalls::{Child, ChildHandle};

/// The pid the kernel gives the first process, and the one orphans go to.
pub const INIT_PID: i32 = 1;

/// The process table when it is not in this process's memory.
///
/// Between the *threads* of one process the table above is shared directly.
/// Between processes it lives elsewhere: a second wasm module every Worker
/// instantiates against one `SharedArrayBuffer`, so its `Vec<Proc>` is the same
/// bytes for all of them. A process reaches it through calls, not pointers -- so
/// this is the call surface, plain function pointers a host fills in, every
/// argument a scalar because an `Arc` is an address in a memory the other side
/// cannot read. `None` is the single-process default.
#[derive(Clone, Copy)]
pub struct Remote {
    pub spawn: extern "C" fn(ppid: i32) -> i32,
    pub exit: extern "C" fn(pid: i32, status: i32),
    pub reap: extern "C" fn(pid: i32),
    /// Collect a finished child: pid in the high 32 bits, wait status in the low
    /// 32, or zero if none. Packed into one return because two Workers may
    /// collect at once and a status left between calls belongs to whoever asked
    /// last.
    pub wait: extern "C" fn(ppid: i32, want: i32) -> i64,
    pub has_children: extern "C" fn(ppid: i32) -> i32,
    pub kill: extern "C" fn(caller: i32, target: i32, signo: u32) -> i32,
    /// Signal every process in a group, naming it *positively*.
    ///
    /// `kill(-pgid)` cannot: a target of -1 is POSIX's "everyone", so group 1
    /// has no negative spelling -- and the demos run a shell as pid 1, so `^C`
    /// to the foreground group is exactly this case.
    pub kill_group: extern "C" fn(caller: i32, pgid: i32, signo: u32) -> i32,
    pub take_signals: extern "C" fn(pid: i32) -> u64,
    /// Of `taken`, the signals `kill` sent (see [`ProcTable::take_kill_senders`]).
    pub take_kill_signals: extern "C" fn(pid: i32, taken: u64) -> u64,
    pub kill_sender: extern "C" fn(pid: i32, signo: u32) -> i32,
    pub has_signals: extern "C" fn(pid: i32) -> i32,
    pub ppid: extern "C" fn(pid: i32) -> i32,
    pub pgid: extern "C" fn(caller: i32, of: i32) -> i32,
    pub sid: extern "C" fn(caller: i32, of: i32) -> i32,
    pub setsid: extern "C" fn(pid: i32) -> i32,
    pub setpgid: extern "C" fn(caller: i32, target: i32, pgid: i32) -> i32,
    /// Would a `wait4` answer now? See [`ProcTable::wait_ready`].
    pub wait_ready: extern "C" fn(ppid: i32, want: i32) -> i32,
    pub request_halt: extern "C" fn(code: i32),
    pub halt_code: extern "C" fn() -> i32,
    /// The process's wait queue. See [`ProcTable::queue`].
    pub queue: extern "C" fn(pid: i32) -> u32,
    /// What `/proc/<pid>` says about a process: see [`ProcTable::set_info`].
    pub set_info: extern "C" fn(pid: i32, at: crate::PAddr, len: u32),
    /// Every process, encoded by [`encode_entries`], into `cap` bytes at `at`;
    /// returns the full length (larger than `cap`: ask again with more room).
    pub snapshot: extern "C" fn(at: crate::PAddr, cap: u32) -> i32,
    /// A new thread of `pid`, numbered with the processes: see
    /// [`ProcTable::new_tid`].
    pub new_tid: extern "C" fn(pid: i32) -> i32,
    /// See [`ProcTable::end_tid`].
    pub end_tid: extern "C" fn(tid: i32),
    /// See [`ProcTable::task_owner`]; zero for no such task.
    pub task_owner: extern "C" fn(id: i32) -> i32,
}

/// One process as another process's `/proc` sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    /// Exited and not yet collected by its parent.
    pub zombie: bool,
    /// What the process published: its name, a newline, then its command
    /// line (NUL-separated arguments). Empty until it has said.
    pub info: Vec<u8>,
}

impl ProcEntry {
    /// The name `/proc/<pid>/comm` and `stat` give (at most 15 bytes).
    pub fn comm(&self) -> String {
        let end = self.info.iter().position(|&b| b == b'\n').unwrap_or(self.info.len());
        let name = String::from_utf8_lossy(&self.info[..end]).into_owned();
        if name.is_empty() {
            return "gex".into();
        }
        name.chars().take(15).collect()
    }
    /// `/proc/<pid>/cmdline`.
    pub fn cmdline(&self) -> &[u8] {
        match self.info.iter().position(|&b| b == b'\n') {
            Some(i) => &self.info[i + 1..],
            None => &[],
        }
    }
}

/// The wire form of a process list: per entry five `i32`s (pid, ppid, pgid,
/// sid, zombie), the info's length, the info, padded to four bytes.
pub fn encode_entries(entries: &[ProcEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        for v in [e.pid, e.ppid, e.pgid, e.sid, i32::from(e.zombie), e.info.len() as i32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&e.info);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

pub fn decode_entries(mut b: &[u8]) -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let word = |b: &[u8], i: usize| i32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    while b.len() >= 24 {
        let n = word(b, 5).max(0) as usize;
        if b.len() < 24 + n {
            break;
        }
        out.push(ProcEntry {
            pid: word(b, 0),
            ppid: word(b, 1),
            pgid: word(b, 2),
            sid: word(b, 3),
            zombie: word(b, 4) != 0,
            info: b[24..24 + n].to_vec(),
        });
        b = &b[(24 + n + 3) & !3..];
    }
    out
}

/// One process, as the table sees it.
struct Proc {
    pid: i32,
    ppid: i32,
    /// Process group. A group leader has `pgid == pid`.
    pgid: i32,
    /// Session. A session leader has `sid == pid`.
    sid: i32,
    /// The exit-status cell a `wait4` collects. The root process has one too,
    /// so nothing has to special-case it, even though nobody waits on it.
    child: Child,
    /// Signals sent to this process by another, as a bitmask of `1 << (signo-1)`.
    ///
    /// The sender can't deliver -- the target's signal state is behind its own
    /// lock and it may be running or blocked -- so it leaves the bit here and the
    /// target picks it up at its next syscall, as Linux delivers at a point the
    /// target chooses.
    pending: Arc<AtomicU64>,
    /// The part of `pending` that `kill` sent, which the target reports as
    /// `SI_USER` (the rest -- `SIGCHLD`, a terminal's `^C` -- the kernel
    /// raised), and the pid that sent each. Under the table's lock.
    killed: u64,
    senders: [i32; 64],
    /// The process's own wait queue (`crate::wait`), on which each of its
    /// threads keeps its wait slot registered for as long as it runs: a
    /// signal for the process, a child's exit or a halt notifies it.
    queue: crate::wait::Queue,
    /// See [`ProcTable::set_info`].
    info: Vec<u8>,
}

struct Table {
    /// The next task id, a process's or a thread's: one numbering, as
    /// Linux's pids and tids are one, so an id names exactly one task.
    next_pid: i32,
    procs: Vec<Proc>,
    /// Each live thread that is not its process's first, and its process.
    threads: std::collections::HashMap<i32, i32>,
}

/// Moved after every signal left in a process's inbox (`kill`, a group's
/// signal, a child's SIGCHLD), once the bit is set: a thread that saw it
/// unmoved has nothing new to collect ([`ProcTable::take_signals`]), and one
/// that saw it move finds the bit there. For the threads that ask without
/// the kernel lock; the table's own (a page's kernel module) is the one that
/// counts.
static SIGNAL_SEQ: AtomicU32 = AtomicU32::new(0);

/// [`SIGNAL_SEQ`].
pub fn signal_seq() -> u32 {
    SIGNAL_SEQ.load(Ordering::Acquire)
}

/// The system's processes.
pub struct ProcTable {
    inner: Mutex<Table>,
    /// Set once some process has called `reboot` with a shutdown command.
    ///
    /// A halt is not addressed to a process: rather than signal each, every
    /// process notices at its next syscall and exits, and waiters notice on the
    /// poll they already do. So it takes effect soon, and never mid-syscall.
    halting: AtomicBool,
    halt_code: AtomicI32,
    /// Where the table really is, when it is not here. See [`Remote`].
    remote: Option<Remote>,
}

impl std::fmt::Debug for ProcTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.lock();
        f.debug_struct("ProcTable")
            .field("live", &t.procs.len())
            .field("next_pid", &t.next_pid)
            .finish()
    }
}

impl Default for ProcTable {
    fn default() -> ProcTable {
        ProcTable::new()
    }
}

impl ProcTable {
    /// A table holding just the init process, which is its own group and
    /// session leader and whose parent is the kernel that is not there.
    pub fn new() -> ProcTable {
        ProcTable {
            halting: AtomicBool::new(false),
            halt_code: AtomicI32::new(0),
            remote: None,
            inner: Mutex::new(Table {
                next_pid: INIT_PID + 1,
                threads: std::collections::HashMap::new(),
                procs: vec![Proc {
                    pid: INIT_PID,
                    ppid: 0,
                    pgid: INIT_PID,
                    sid: INIT_PID,
                    child: Child::new(INIT_PID),
                    pending: Arc::new(AtomicU64::new(0)),
                    killed: 0,
                    senders: [0; 64],
                    queue: crate::wait::Queue::new(),
                    info: Vec::new(),
                }],
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a child of `ppid` and return its pid. It joins its parent's
    /// group and session, so a shell can later `setpgid` a whole pipeline into
    /// one group.
    pub fn spawn(&self, ppid: i32) -> i32 {
        if let Some(r) = self.remote {
            return (r.spawn)(ppid);
        }
        let mut t = self.lock();
        let pid = t.next_pid;
        t.next_pid += 1;
        let (pgid, sid) = match t.procs.iter().find(|p| p.pid == ppid) {
            Some(p) => (p.pgid, p.sid),
            None => (pid, pid),
        };
        t.procs.push(Proc {
            pid,
            ppid,
            pgid,
            sid,
            child: Child::new(pid),
            pending: Arc::new(AtomicU64::new(0)),
            killed: 0,
            senders: [0; 64],
            queue: crate::wait::Queue::new(),
            info: Vec::new(),
        });
        pid
    }

    /// A new thread of `pid`, and its id: from the numbering the processes'
    /// come from, so no thread shares a number with a process or with a
    /// thread of another (a per-process count did both: `tkill(3)` meant pid
    /// 2's second thread and pid 3 alike). `kill`, `tgkill` and `/proc` find
    /// its process by it ([`Self::task_owner`]) until [`Self::end_tid`].
    pub fn new_tid(&self, pid: i32) -> i32 {
        if let Some(r) = self.remote {
            return (r.new_tid)(pid);
        }
        let mut t = self.lock();
        let tid = t.next_pid;
        t.next_pid += 1;
        t.threads.insert(tid, pid);
        tid
    }

    /// Thread `tid` has ended (`exit`, or the others at an `execve`).
    pub fn end_tid(&self, tid: i32) {
        if let Some(r) = self.remote {
            return (r.end_tid)(tid);
        }
        self.lock().threads.remove(&tid);
    }

    /// The process task `id` is part of: `id` itself when it is a process
    /// (its first thread's id is its pid), the process of a thread
    /// otherwise; `None` when neither is in the table.
    pub fn task_owner(&self, id: i32) -> Option<i32> {
        if let Some(r) = self.remote {
            return Some((r.task_owner)(id)).filter(|&p| p > 0);
        }
        let t = self.lock();
        if t.procs.iter().any(|p| p.pid == id) {
            return Some(id);
        }
        t.threads.get(&id).copied()
    }

    /// What `/proc/<pid>` shows for `pid` in every other process: its name
    /// and command line (see [`ProcEntry`]), said by the process itself when
    /// it starts and again when it execs.
    pub fn set_info(&self, pid: i32, info: &[u8]) {
        let info = &info[..info.len().min(4096)];
        if let Some(r) = self.remote {
            return (r.set_info)(pid, info.as_ptr() as usize as crate::PAddr, info.len() as u32);
        }
        if let Some(p) = self.lock().procs.iter_mut().find(|p| p.pid == pid) {
            p.info = info.to_vec();
        }
    }

    /// Every process in the table, for `/proc`'s listing.
    pub fn entries(&self) -> Vec<ProcEntry> {
        if let Some(r) = self.remote {
            let mut cap = 16 * 1024usize;
            for _ in 0..4 {
                let mut buf = vec![0u8; cap];
                let n = (r.snapshot)(buf.as_mut_ptr() as usize as crate::PAddr, cap as u32);
                if n < 0 {
                    return Vec::new();
                }
                if n as usize <= cap {
                    buf.truncate(n as usize);
                    return decode_entries(&buf);
                }
                cap = n as usize;
            }
            return Vec::new();
        }
        self.lock()
            .procs
            .iter()
            .map(|p| ProcEntry {
                pid: p.pid,
                ppid: p.ppid,
                pgid: p.pgid,
                sid: p.sid,
                zombie: p.child.status.lock().map(|s| s.is_some()).unwrap_or(false),
                info: p.info.clone(),
            })
            .collect()
    }

    /// The handle a host needs to publish `pid`'s exit status.
    pub fn handle(&self, pid: i32) -> Option<ChildHandle> {
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.child.handle())
    }

    /// Every child of `ppid` that has not yet been collected by a `wait4`.
    pub fn children(&self, ppid: i32) -> Vec<ChildHandle> {
        self.lock()
            .procs
            .iter()
            .filter(|p| p.ppid == ppid && !p.child.reaped.load(Ordering::Acquire))
            .map(|p| p.child.handle())
            .collect()
    }

    /// Whether `pid` has any child at all, reaped or not.
    ///
    /// `wait4` needs this to tell "no children" (`ECHILD`) from "children, all
    /// already collected" -- which is also `ECHILD`, but reaching it by a
    /// different route matters for the trace.
    pub fn has_children(&self, ppid: i32) -> bool {
        if let Some(r) = self.remote {
            return (r.has_children)(ppid) != 0;
        }
        self.lock().procs.iter().any(|p| p.ppid == ppid)
    }

    /// Record that `pid` has exited, wake anyone waiting on it, and give its
    /// surviving children to init.
    pub fn exit(&self, pid: i32, status: i32) {
        if let Some(r) = self.remote {
            return (r.exit)(pid, status);
        }
        let (cell, done, parent_q) = {
            let mut t = self.lock();
            // The parent is owed a SIGCHLD. A `wait4` parked on the condvar
            // doesn't need one, but a parent that polls `WNOHANG` and sleeps in
            // `sigsuspend` between polls has nothing else to wake it -- coreutils
            // `timeout` does exactly that.
            let ppid = t.procs.iter().find(|p| p.pid == pid).map(|p| p.ppid);
            if let Some(ppid) = ppid {
                if let Some(parent) = t.procs.iter().find(|p| p.pid == ppid) {
                    parent
                        .pending
                        .fetch_or(1u64 << (SIGCHLD - 1), Ordering::AcqRel);
                    SIGNAL_SEQ.fetch_add(1, Ordering::Release);
                }
            }
            for p in t.procs.iter_mut() {
                if p.ppid == pid {
                    p.ppid = INIT_PID;
                }
            }
            // Its threads went with it.
            t.threads.retain(|_, p| *p != pid);
            let parent_q = ppid
                .and_then(|ppid| t.procs.iter().find(|p| p.pid == ppid))
                .map_or(0, |p| p.queue.id());
            match t.procs.iter().find(|p| p.pid == pid) {
                Some(p) => (Arc::clone(&p.child.status), Arc::clone(&p.child.done), parent_q),
                None => return,
            }
        };
        // Published outside the table lock: a waiter wakes on `done` then wants
        // the table, so holding both here in the other order deadlocks.
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        done.notify_all();
        // The parent's `wait4`, and its SIGCHLD.
        crate::wait::notify(parent_q);
    }

    /// Forget a collected process, so a long-lived init doesn't accumulate a
    /// record per respawn.
    pub fn reap(&self, pid: i32) {
        if let Some(r) = self.remote {
            return (r.reap)(pid);
        }
        let mut t = self.lock();
        t.procs.retain(|p| p.pid != pid);
    }

    pub fn ppid(&self, pid: i32) -> i32 {
        if let Some(r) = self.remote {
            return (r.ppid)(pid);
        }
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .map_or(0, |p| p.ppid)
    }

    /// `getpgid`. A pid of zero means the caller, as it does in Linux.
    pub fn pgid(&self, caller: i32, of: i32) -> Result<i32, Errno> {
        if let Some(r) = self.remote {
            return errno_or(( r.pgid)(caller, of));
        }
        let want = if of == 0 { caller } else { of };
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == want)
            .map(|p| p.pgid)
            .ok_or(Errno::ESRCH)
    }

    /// `getsid`, with the same zero-means-me rule.
    pub fn sid(&self, caller: i32, of: i32) -> Result<i32, Errno> {
        if let Some(r) = self.remote {
            return errno_or((r.sid)(caller, of));
        }
        let want = if of == 0 { caller } else { of };
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == want)
            .map(|p| p.sid)
            .ok_or(Errno::ESRCH)
    }

    /// `setsid`: become leader of a new session and group. `EPERM` if the caller
    /// already leads a group (it would split across two sessions) -- a shell
    /// forks first so the child is not a leader.
    pub fn setsid(&self, pid: i32) -> Result<i32, Errno> {
        if let Some(r) = self.remote {
            return errno_or((r.setsid)(pid));
        }
        let mut t = self.lock();
        if t.procs.iter().any(|p| p.pgid == pid) {
            return Err(Errno::EPERM);
        }
        match t.procs.iter_mut().find(|p| p.pid == pid) {
            Some(p) => {
                p.sid = pid;
                p.pgid = pid;
                Ok(pid)
            }
            None => Err(Errno::ESRCH),
        }
    }

    /// `setpgid(target, pgid)`, either zero meaning the caller. You may move
    /// only yourself or a child, never across a session boundary, never a
    /// session leader.
    pub fn setpgid(&self, caller: i32, target: i32, pgid: i32) -> Result<(), Errno> {
        if let Some(r) = self.remote {
            return errno_or((r.setpgid)(caller, target, pgid)).map(|_| ());
        }
        let target = if target == 0 { caller } else { target };
        let pgid = if pgid == 0 { target } else { pgid };
        if pgid < 0 {
            return Err(Errno::EINVAL);
        }
        let mut t = self.lock();
        let (t_sid, t_ppid, t_is_leader) = match t.procs.iter().find(|p| p.pid == target) {
            Some(p) => (p.sid, p.ppid, p.sid == p.pid),
            None => return Err(Errno::ESRCH),
        };
        if target != caller && t_ppid != caller {
            return Err(Errno::ESRCH);
        }
        if t_is_leader {
            return Err(Errno::EPERM);
        }
        // Joining an existing group is only allowed within one session;
        // `pgid == target` is the create-a-group case and always in session.
        if pgid != target {
            match t.procs.iter().find(|p| p.pgid == pgid) {
                Some(p) if p.sid == t_sid => {}
                Some(_) => return Err(Errno::EPERM),
                None => return Err(Errno::EPERM),
            }
        }
        if let Some(p) = t.procs.iter_mut().find(|p| p.pid == target) {
            p.pgid = pgid;
        }
        Ok(())
    }

    /// Every live process in group `pgid`, for `kill(-pgid, sig)`.
    pub fn group_members(&self, pgid: i32) -> Vec<i32> {
        self.lock()
            .procs
            .iter()
            .filter(|p| p.pgid == pgid)
            .map(|p| p.pid)
            .collect()
    }

    /// Deliver `signo` as `kill` addresses it: to one process, a group, or
    /// everything. Returns how many it reached. `signo` zero sends nothing and
    /// only reports whether there was anyone -- what `kill(pid, 0)` is for.
    pub fn kill(&self, caller: i32, target: i32, signo: u32) -> Result<usize, Errno> {
        if let Some(r) = self.remote {
            return errno_or((r.kill)(caller, target, signo)).map(|n| n as usize);
        }
        if signo > 64 {
            return Err(Errno::EINVAL);
        }
        let mut t = self.lock();
        let hits: Vec<usize> = match target {
            // Everything but init: killing pid 1 stops the machine (use `reboot`
            // for that). The caller is included, as Linux includes it.
            -1 => (0..t.procs.len()).filter(|&i| t.procs[i].pid != INIT_PID).collect(),
            // A negative pid is the group of that number; zero is the
            // caller's own group.
            n if n <= 0 => {
                let pgid = if n == 0 {
                    t.procs
                        .iter()
                        .find(|p| p.pid == caller)
                        .map_or(caller, |p| p.pgid)
                } else {
                    -n
                };
                (0..t.procs.len()).filter(|&i| t.procs[i].pgid == pgid).collect()
            }
            // A thread's id is its process, as on Linux (`kill` is
            // process-directed whichever of its tasks it names).
            n => {
                let n = t.threads.get(&n).copied().unwrap_or(n);
                (0..t.procs.len()).filter(|&i| t.procs[i].pid == n).collect()
            }
        };
        if hits.is_empty() {
            return Err(Errno::ESRCH);
        }
        if signo > 0 {
            let bit = 1u64 << (signo - 1);
            for &i in &hits {
                let p = &mut t.procs[i];
                p.killed |= bit;
                p.senders[signo as usize - 1] = caller;
                p.pending.fetch_or(bit, Ordering::AcqRel);
            }
            SIGNAL_SEQ.fetch_add(1, Ordering::Release);
            // A signal ends a wait: wake each target's waiting threads.
            for &i in &hits {
                t.procs[i].queue.notify();
            }
        }
        Ok(hits.len())
    }

    /// Signal every process in group `pgid`, which is named positively.
    ///
    /// See [`Remote::kill_group`] for why this is not `kill(-pgid)`.
    pub fn kill_group(&self, caller: i32, pgid: i32, signo: u32) -> Result<usize, Errno> {
        if let Some(r) = self.remote {
            return errno_or((r.kill_group)(caller, pgid, signo)).map(|n| n as usize);
        }
        if signo > 64 {
            return Err(Errno::EINVAL);
        }
        let t = self.lock();
        let hits: Vec<&Proc> = t.procs.iter().filter(|p| p.pgid == pgid).collect();
        if hits.is_empty() {
            return Err(Errno::ESRCH);
        }
        if signo > 0 {
            let bit = 1u64 << (signo - 1);
            for p in &hits {
                p.pending.fetch_or(bit, Ordering::AcqRel);
            }
            SIGNAL_SEQ.fetch_add(1, Ordering::Release);
            for p in &hits {
                p.queue.notify();
            }
        }
        Ok(hits.len())
    }

    /// Take everything sent to `pid` since the last call.
    pub fn take_signals(&self, pid: i32) -> u64 {
        if let Some(r) = self.remote {
            return (r.take_signals)(pid);
        }
        match self.lock().procs.iter().find(|p| p.pid == pid) {
            Some(p) => p.pending.swap(0, Ordering::AcqRel),
            None => 0,
        }
    }

    /// Of the signals in `taken` (what [`ProcTable::take_signals`] just
    /// returned), those `kill` sent, each with the pid that sent it; the rest
    /// the kernel raised. Clears what it reports.
    pub fn take_kill_senders(&self, pid: i32, taken: u64) -> Vec<(u32, i32)> {
        let mut out = Vec::new();
        if let Some(r) = self.remote {
            let mut killed = (r.take_kill_signals)(pid, taken);
            while killed != 0 {
                let signo = killed.trailing_zeros() + 1;
                killed &= killed - 1;
                out.push((signo, (r.kill_sender)(pid, signo)));
            }
            return out;
        }
        let mut t = self.lock();
        if let Some(p) = t.procs.iter_mut().find(|p| p.pid == pid) {
            let mut killed = p.killed & taken;
            p.killed &= !taken;
            while killed != 0 {
                let signo = killed.trailing_zeros() + 1;
                killed &= killed - 1;
                out.push((signo, p.senders[signo as usize - 1]));
            }
        }
        out
    }

    /// For [`Remote::take_kill_signals`]: clear and return the bits of `taken`
    /// that `kill` sent to `pid`.
    pub fn take_kill_signals(&self, pid: i32, taken: u64) -> u64 {
        let mut t = self.lock();
        match t.procs.iter_mut().find(|p| p.pid == pid) {
            Some(p) => {
                let k = p.killed & taken;
                p.killed &= !taken;
                k
            }
            None => 0,
        }
    }

    /// For [`Remote::kill_sender`]: who last sent `signo` to `pid` by `kill`.
    pub fn kill_sender(&self, pid: i32, signo: u32) -> i32 {
        if signo == 0 || signo > 64 {
            return 0;
        }
        self.lock().procs.iter().find(|p| p.pid == pid).map_or(0, |p| p.senders[signo as usize - 1])
    }

    /// Whether anything is waiting for `pid`, without taking it. Cheap enough
    /// to ask on every syscall.
    pub fn has_signals(&self, pid: i32) -> bool {
        if let Some(r) = self.remote {
            return (r.has_signals)(pid) != 0;
        }
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .is_some_and(|p| p.pending.load(Ordering::Acquire) != 0)
    }

    /// Stop the machine. Every process exits with `code` at its next syscall.
    pub fn request_halt(&self, code: i32) {
        if let Some(r) = self.remote {
            return (r.request_halt)(code);
        }
        self.halt_code.store(code, Ordering::Release);
        self.halting.store(true, Ordering::Release);
        // Every waiter notices the halt on its way out of its wait.
        for p in self.lock().procs.iter() {
            p.queue.notify();
        }
    }

    /// `pid`'s wait queue, 0 for none: a thread of `pid` keeps its wait slot
    /// on it while it runs (see [`Proc::queue`]).
    pub fn queue(&self, pid: i32) -> u32 {
        if let Some(r) = self.remote {
            return (r.queue)(pid);
        }
        self.lock().procs.iter().find(|p| p.pid == pid).map_or(0, |p| p.queue.id())
    }

    /// The exit code the machine is halting with, if it is.
    pub fn halt_code(&self) -> Option<i32> {
        if let Some(r) = self.remote {
            // -1 for "nobody has asked", because a halt code is a status and
            // a status is unsigned; there is no legal -1 to be confused with.
            let c = (r.halt_code)();
            return (c >= 0).then_some(c);
        }
        self.halting
            .load(Ordering::Acquire)
            .then(|| self.halt_code.load(Ordering::Acquire))
    }

    /// Whether `pid` names a live process, for `kill(pid, 0)`.
    pub fn exists(&self, pid: i32) -> bool {
        self.lock().procs.iter().any(|p| p.pid == pid)
    }

    /// Whether `pid` is in the table and has not exited: [`ProcTable::exists`]
    /// without the zombies, which are in the table until reaped.
    pub fn alive(&self, pid: i32) -> bool {
        self.lock()
            .procs
            .iter()
            .any(|p| p.pid == pid && p.child.status.lock().map(|s| s.is_none()).unwrap_or(true))
    }

    /// Whether a `wait4` by `ppid` for `want` would answer now: a child with a
    /// status ready, or no matching child at all (`ECHILD`). Separate from
    /// `collect`, which *takes* the child -- asking must not reap.
    pub fn wait_ready(&self, ppid: i32, want: i32) -> bool {
        if let Some(r) = self.remote {
            return (r.wait_ready)(ppid, want) != 0;
        }
        let mut any = false;
        for c in self
            .children(ppid)
            .iter()
            .filter(|c| want <= 0 || c.pid == want)
        {
            any = true;
            if c.status.lock().map(|st| st.is_some()).unwrap_or(true) {
                return true;
            }
        }
        !any
    }

    /// A table that is really somewhere else. See [`Remote`]. The local `Table`
    /// is still built and still holds init, so methods with no remote
    /// counterpart keep working.
    pub fn shared(remote: Remote) -> ProcTable {
        ProcTable {
            remote: Some(remote),
            ..ProcTable::new()
        }
    }

    /// Whether this table's answers come from somewhere else. Callers must know
    /// because `wait4` differs: a local table hands out `ChildHandle`s to park
    /// on, a remote one has none -- its children are in another memory.
    pub fn is_shared(&self) -> bool {
        self.remote.is_some()
    }

    /// Collect a finished child of `ppid` from a remote table. `Some((pid,
    /// status))` when ready, `None` when none -- distinguish `ECHILD` with
    /// `wait_ready`. Also `None` if the table is local: check `is_shared` first.
    pub fn collect(&self, ppid: i32, want: i32) -> Option<(i32, i32)> {
        let r = self.remote?;
        let packed = (r.wait)(ppid, want);
        let pid = (packed >> 32) as i32;
        (pid > 0).then(|| (pid, packed as u32 as i32))
    }
}

/// A remote call's `i32`: negative is a negated errno, as the kernel's own
/// syscall returns are.
fn errno_or(v: i32) -> Result<i32, Errno> {
    if v < 0 {
        Err(Errno(-v))
    } else {
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Threads and processes are numbered from one count, so no id names
    /// two tasks; a thread's id finds its process for `kill` and `/proc`
    /// until it ends, and a process's ending takes its threads' ids along.
    #[test]
    fn thread_ids_are_numbered_with_the_processes() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        let a2 = t.new_tid(a);
        let b = t.spawn(INIT_PID);
        let b2 = t.new_tid(b);
        let ids = [INIT_PID, a, a2, b, b2];
        for (i, x) in ids.iter().enumerate() {
            assert!(!ids[i + 1..].contains(x), "{x} names two tasks: {ids:?}");
        }
        assert_eq!(t.task_owner(a), Some(a));
        assert_eq!(t.task_owner(a2), Some(a));
        assert_eq!(t.task_owner(b2), Some(b));
        assert_eq!(t.task_owner(b2 + 100), None);
        // `kill` of a thread's id is its process's signal, and only its.
        assert_eq!(t.kill(INIT_PID, a2, 10), Ok(1));
        assert_eq!(t.take_signals(a), 1 << 9);
        assert_eq!(t.take_signals(b), 0);
        t.end_tid(a2);
        assert_eq!(t.task_owner(a2), None);
        assert_eq!(t.kill(INIT_PID, a2, 10), Err(Errno::ESRCH));
        t.exit(b, 0);
        assert_eq!(t.task_owner(b2), None);
        assert_eq!(t.kill(INIT_PID, b2, 0), Err(Errno::ESRCH));
    }

    #[test]
    fn the_table_lists_processes_with_what_they_said() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        t.set_info(a, b"sh\n/bin/sh\0-c\0x\0");
        let e = t.entries();
        assert_eq!(e.len(), 2);
        let mine = e.iter().find(|e| e.pid == a).unwrap();
        assert_eq!(mine.comm(), "sh");
        assert_eq!(mine.cmdline(), b"/bin/sh\0-c\0x\0");
        assert_eq!(decode_entries(&encode_entries(&e)), e);
        t.exit(a, 0);
        assert!(t.entries().iter().find(|e| e.pid == a).unwrap().zombie);
    }

    #[test]
    fn a_child_inherits_its_parents_group_and_session() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        assert_eq!(t.pgid(a, 0), Ok(INIT_PID));
        assert_eq!(t.sid(a, 0), Ok(INIT_PID));
        assert_eq!(t.ppid(a), INIT_PID);
    }

    #[test]
    fn setsid_makes_a_leader_of_both() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        assert_eq!(t.setsid(a), Ok(a));
        assert_eq!(t.sid(a, 0), Ok(a));
        assert_eq!(t.pgid(a, 0), Ok(a));
        // Now a group leader, so a second call must fail.
        assert_eq!(t.setsid(a), Err(Errno::EPERM));
    }

    #[test]
    fn setpgid_puts_a_pipeline_in_one_group() {
        let t = ProcTable::new();
        let sh = t.spawn(INIT_PID);
        t.setsid(sh).unwrap();
        let first = t.spawn(sh);
        let second = t.spawn(sh);
        // The shell makes the first child a group leader and the second joins.
        assert_eq!(t.setpgid(sh, first, first), Ok(()));
        assert_eq!(t.setpgid(sh, second, first), Ok(()));
        assert_eq!(t.group_members(first), vec![first, second]);
    }

    #[test]
    fn a_group_may_not_be_joined_across_a_session() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        t.setsid(a).unwrap();
        let b = t.spawn(INIT_PID);
        assert_eq!(t.setpgid(INIT_PID, b, a), Err(Errno::EPERM));
    }

    #[test]
    fn orphans_are_reparented_to_init() {
        let t = ProcTable::new();
        let parent = t.spawn(INIT_PID);
        let orphan = t.spawn(parent);
        assert_eq!(t.ppid(orphan), parent);
        t.exit(parent, 0);
        assert_eq!(t.ppid(orphan), INIT_PID);
        // And init can now see it as something to wait for.
        let seen: Vec<i32> = t.children(INIT_PID).iter().map(|c| c.pid).collect();
        assert!(seen.contains(&orphan));
    }

    #[test]
    fn a_reaped_child_stops_being_waitable() {
        let t = ProcTable::new();
        let c = t.spawn(INIT_PID);
        t.exit(c, 0);
        let h = t.handle(c).unwrap();
        assert_eq!(*h.status.lock().unwrap(), Some(0));
        h.reaped.store(true, Ordering::Release);
        assert!(t.children(INIT_PID).iter().all(|x| x.pid != c));
        assert!(t.has_children(INIT_PID));
        t.reap(c);
        assert!(!t.has_children(INIT_PID));
    }

    #[test]
    fn kill_reaches_one_process() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        let b = t.spawn(INIT_PID);
        assert_eq!(t.kill(INIT_PID, a, 15), Ok(1));
        assert_eq!(t.take_signals(a), 1 << 14);
        assert_eq!(t.take_signals(b), 0);
        // And taking is destructive.
        assert_eq!(t.take_signals(a), 0);
    }

    #[test]
    fn a_group_can_be_named_even_when_it_is_group_one() {
        // The case `kill(-pgid)` cannot express: -1 is "everyone", so group 1
        // has no negative spelling. It is the shell's group in every demo.
        let t = ProcTable::new();
        let child = t.spawn(INIT_PID);
        let outsider = t.spawn(INIT_PID);
        t.setsid(outsider).unwrap();
        assert_eq!(t.kill_group(INIT_PID, INIT_PID, 2), Ok(2));
        assert_eq!(t.take_signals(INIT_PID), 1 << 1);
        assert_eq!(t.take_signals(child), 1 << 1);
        assert_eq!(t.take_signals(outsider), 0, "a different session is untouched");
    }

    #[test]
    fn kill_reaches_a_whole_group() {
        let t = ProcTable::new();
        let lead = t.spawn(INIT_PID);
        t.setsid(lead).unwrap();
        let member = t.spawn(lead);
        let outsider = t.spawn(INIT_PID);
        assert_eq!(t.kill(INIT_PID, -lead, 9), Ok(2));
        assert_eq!(t.take_signals(lead), 1 << 8);
        assert_eq!(t.take_signals(member), 1 << 8);
        assert_eq!(t.take_signals(outsider), 0);
    }

    #[test]
    fn kill_to_everyone_spares_init_but_not_the_sender() {
        let t = ProcTable::new();
        let sender = t.spawn(INIT_PID);
        let other = t.spawn(INIT_PID);
        assert_eq!(t.kill(sender, -1, 15), Ok(2));
        assert_eq!(t.take_signals(other), 1 << 14);
        assert_eq!(t.take_signals(sender), 1 << 14);
        // Init is the one process `kill(-1)` may not reach.
        assert_eq!(t.take_signals(INIT_PID), 0);
    }

    #[test]
    fn signal_zero_only_asks_whether_anyone_is_there() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        assert_eq!(t.kill(INIT_PID, a, 0), Ok(1));
        assert_eq!(t.take_signals(a), 0);
        assert_eq!(t.kill(INIT_PID, 12345, 0), Err(Errno::ESRCH));
    }

    #[test]
    fn a_halt_is_visible_to_every_process() {
        let t = ProcTable::new();
        assert_eq!(t.halt_code(), None);
        t.request_halt(3);
        assert_eq!(t.halt_code(), Some(3));
    }

    #[test]
    fn pids_are_unique_across_the_whole_tree() {
        let t = ProcTable::new();
        let a = t.spawn(INIT_PID);
        let b = t.spawn(a);
        let c = t.spawn(b);
        let d = t.spawn(INIT_PID);
        let all = [INIT_PID, a, b, c, d];
        for (i, x) in all.iter().enumerate() {
            assert!(!all[..i].contains(x), "pid {x} handed out twice");
        }
    }
}
