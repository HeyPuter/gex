//! The process table: pids, parents, process groups and sessions.
//!
//! Every process in the system shares one of these. It has to be shared
//! because three of the things a process table is for cannot be answered from
//! inside a single process:
//!
//!   * **Pids must be unique system-wide.** Before this existed each forked
//!     kernel carried its own counter, offset from its parent's by a thousand,
//!     which avoids collisions for a shallow tree and stops doing so for a
//!     long-lived one. An init system is precisely the long-lived case: it
//!     respawns for as long as the machine is up.
//!
//!   * **Orphans are reparented to pid 1.** When a process exits its surviving
//!     children become init's, and init reaps them. That is most of what an
//!     init system is for. A child list owned by each parent cannot express
//!     it: the exiting process would have to reach into init's kernel, which
//!     is behind another lock held by another thread.
//!
//!   * **Groups and sessions span processes by definition.** `kill(-pgid)`
//!     asks a question about processes the caller may not be related to.
//!
//! So the table owns the [`Child`] record for every process, and a kernel asks
//! it for the children rather than holding them.
//!
//! What is deliberately not modelled: controlling terminals, orphaned process
//! group handling (the SIGHUP/SIGCONT dance in POSIX 3.1.2.2), and session
//! keyrings. A guest that asks gets plausible answers, not Linux's.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// The signal a parent is owed when a child ends.
const SIGCHLD: u32 = crate::signal::sig::SIGCHLD;
use std::sync::{Arc, Mutex};

use crate::errno::Errno;
use crate::syscalls::{Child, ChildHandle};

/// The pid the kernel gives the first process, and the one orphans go to.
pub const INIT_PID: i32 = 1;

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
    /// Signals sent to this process by another one, as a bitmask of
    /// `1 << (signo - 1)`.
    ///
    /// A signal cannot be delivered by the sender -- the target's signal state
    /// lives in its own kernel, behind its own lock, and it may be running,
    /// blocked in the host, or between the two. So the sender leaves it here
    /// and the target picks it up at its next syscall, which is the same
    /// arrangement Linux has: a signal becomes pending immediately and is
    /// delivered at a point the target chooses.
    pending: Arc<AtomicU64>,
}

struct Table {
    next_pid: i32,
    procs: Vec<Proc>,
}

/// The system's processes.
pub struct ProcTable {
    inner: Mutex<Table>,
    /// Set once some process has called `reboot` with a shutdown command.
    ///
    /// A halt is the one event that is not addressed to a process: it stops
    /// the machine, and every process on it. Rather than a signal to each --
    /// which would need a delivery path into processes blocked in the host --
    /// each process notices at its next syscall and exits, and the ones parked
    /// in `wait4` notice on the poll they already do. So a shutdown takes
    /// effect at the next place a guest asks the kernel for anything, which is
    /// soon, and never in the middle of one.
    halting: AtomicBool,
    halt_code: AtomicI32,
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
            inner: Mutex::new(Table {
                next_pid: INIT_PID + 1,
                procs: vec![Proc {
                    pid: INIT_PID,
                    ppid: 0,
                    pgid: INIT_PID,
                    sid: INIT_PID,
                    child: Child::new(INIT_PID),
                    pending: Arc::new(AtomicU64::new(0)),
                }],
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a child of `ppid` and return its pid.
    ///
    /// A new process joins its parent's group and session, which is what
    /// `fork` does and what lets a shell put a whole pipeline in one group by
    /// calling `setpgid` afterwards.
    pub fn spawn(&self, ppid: i32) -> i32 {
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
        });
        pid
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
        self.lock().procs.iter().any(|p| p.ppid == ppid)
    }

    /// Record that `pid` has exited, wake anyone waiting on it, and give its
    /// surviving children to init.
    pub fn exit(&self, pid: i32, status: i32) {
        let (cell, done) = {
            let mut t = self.lock();
            // The parent is owed a SIGCHLD. A `wait4` that parks on the
            // condvar below does not need one, which is why this was missing
            // and nothing noticed -- but a parent that polls with `WNOHANG`
            // and sleeps in `sigsuspend` between polls has nothing else to
            // wake it. coreutils `timeout` installs an empty SIGCHLD handler
            // for exactly that purpose, and without the signal it waited for
            // a child that had already exited.
            let ppid = t.procs.iter().find(|p| p.pid == pid).map(|p| p.ppid);
            if let Some(ppid) = ppid {
                if let Some(parent) = t.procs.iter().find(|p| p.pid == ppid) {
                    parent
                        .pending
                        .fetch_or(1u64 << (SIGCHLD - 1), Ordering::AcqRel);
                }
            }
            for p in t.procs.iter_mut() {
                if p.ppid == pid {
                    p.ppid = INIT_PID;
                }
            }
            match t.procs.iter().find(|p| p.pid == pid) {
                Some(p) => (Arc::clone(&p.child.status), Arc::clone(&p.child.done)),
                None => return,
            }
        };
        // Published outside the table lock: a waiter wakes on `done` and then
        // wants the table, and holding both here in the other order is how
        // that becomes a deadlock.
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        done.notify_all();
    }

    /// Forget a process that has been collected, so a long-lived init does not
    /// accumulate a record per respawn for the life of the machine.
    pub fn reap(&self, pid: i32) {
        let mut t = self.lock();
        t.procs.retain(|p| p.pid != pid);
    }

    pub fn ppid(&self, pid: i32) -> i32 {
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .map_or(0, |p| p.ppid)
    }

    /// `getpgid`. A pid of zero means the caller, as it does in Linux.
    pub fn pgid(&self, caller: i32, of: i32) -> Result<i32, Errno> {
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
        let want = if of == 0 { caller } else { of };
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == want)
            .map(|p| p.sid)
            .ok_or(Errno::ESRCH)
    }

    /// `setsid`: become the leader of a new session and a new group.
    ///
    /// Fails with `EPERM` if the caller already leads a group, because that
    /// group would end up split across two sessions. A shell forks before
    /// calling this precisely so the child is not a leader and the call can
    /// succeed.
    pub fn setsid(&self, pid: i32) -> Result<i32, Errno> {
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

    /// `setpgid(target, pgid)`, where either being zero means the caller.
    ///
    /// The rules enforced are the ones a shell depends on: you may only move
    /// yourself or a child, never across a session boundary, and never a
    /// session leader.
    pub fn setpgid(&self, caller: i32, target: i32, pgid: i32) -> Result<(), Errno> {
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

    /// Deliver `signo` the way `kill` addresses it: to one process, to a
    /// process group, or to everything.
    ///
    /// Returns the number of processes it reached. A `signo` of zero sends
    /// nothing and only reports whether there was anyone to send to, which is
    /// what `kill(pid, 0)` is for.
    pub fn kill(&self, caller: i32, target: i32, signo: u32) -> Result<usize, Errno> {
        if signo > 64 {
            return Err(Errno::EINVAL);
        }
        let t = self.lock();
        let hits: Vec<&Arc<AtomicU64>> = match target {
            // Everything the caller may signal, which here is everything but
            // init -- killing pid 1 stops the machine, and a guest that means
            // that has `reboot`. The caller is included, as Linux includes
            // it: a shutdown script that signals everything expects to be
            // signalled too.
            -1 => t
                .procs
                .iter()
                .filter(|p| p.pid != INIT_PID)
                .map(|p| &p.pending)
                .collect(),
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
                t.procs
                    .iter()
                    .filter(|p| p.pgid == pgid)
                    .map(|p| &p.pending)
                    .collect()
            }
            n => t
                .procs
                .iter()
                .filter(|p| p.pid == n)
                .map(|p| &p.pending)
                .collect(),
        };
        if hits.is_empty() {
            return Err(Errno::ESRCH);
        }
        if signo > 0 {
            let bit = 1u64 << (signo - 1);
            for p in &hits {
                p.fetch_or(bit, Ordering::AcqRel);
            }
        }
        Ok(hits.len())
    }

    /// Take everything sent to `pid` since the last call.
    pub fn take_signals(&self, pid: i32) -> u64 {
        match self.lock().procs.iter().find(|p| p.pid == pid) {
            Some(p) => p.pending.swap(0, Ordering::AcqRel),
            None => 0,
        }
    }

    /// Whether anything is waiting for `pid`, without taking it. Cheap enough
    /// to ask on every syscall.
    pub fn has_signals(&self, pid: i32) -> bool {
        self.lock()
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .is_some_and(|p| p.pending.load(Ordering::Acquire) != 0)
    }

    /// Stop the machine. Every process exits with `code` at its next syscall.
    pub fn request_halt(&self, code: i32) {
        self.halt_code.store(code, Ordering::Release);
        self.halting.store(true, Ordering::Release);
    }

    /// The exit code the machine is halting with, if it is.
    pub fn halt_code(&self) -> Option<i32> {
        self.halting
            .load(Ordering::Acquire)
            .then(|| self.halt_code.load(Ordering::Acquire))
    }

    /// Whether `pid` names a live process, for `kill(pid, 0)`.
    pub fn exists(&self, pid: i32) -> bool {
        self.lock().procs.iter().any(|p| p.pid == pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
