//! The synthetic `/proc`.
//!
//! Contents are generated on open from live kernel state rather than stored,
//! because that is what makes them true. `self/maps` in particular has to
//! reflect the VMA tree exactly: glibc, OpenSSL and every JVM read it, and
//! C++ exception unwinding locates `.eh_frame` through the same picture of
//! the address space.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Live process state that `/proc` reports on.
///
/// The kernel implements this; keeping it a trait means procfs does not need
/// a reference back into the whole kernel, which would be a borrow knot.
pub trait ProcSource: Send + Sync {
    fn pid(&self) -> i32;
    fn maps(&self) -> String;
    fn cmdline(&self) -> Vec<u8>;
    fn environ(&self) -> Vec<u8>;
    fn exe(&self) -> String;
    fn cwd(&self) -> String;
    /// Names of the currently open descriptors, as `(fd, target)`.
    fn fds(&self) -> Vec<(i32, String)>;
    /// What `/proc/self/fd/N` links to: the file's path with its symlinks
    /// followed, as Linux shows it. By default the name [`Self::fds`] lists.
    fn fd_link(&self, fd: i32) -> Option<String> {
        self.fds().into_iter().find(|&(f, _)| f == fd).map(|(_, t)| t)
    }
    /// `/proc/self/fdinfo/N`: the descriptor's position and flags, and for a
    /// process descriptor its process (`Pid:`), as Linux writes them. polkit
    /// opens a pidfd for the process it checks and finds the pid again here.
    fn fd_info(&self, _fd: i32) -> Option<String> {
        None
    }
    /// `stat` for task `tid` of this process (its pid for the process
    /// itself), one of `threads`; `task`, the task's own time and name, for
    /// `task/<tid>/stat` (`None`: the process's, `/proc/self/stat`).
    fn stat(&self, tid: i32, threads: usize, task: Option<&TaskUsage>) -> String;
    /// `status` for task `tid`, as [`Self::stat`].
    fn status(&self, tid: i32, threads: usize, task: Option<&TaskUsage>) -> String;
    /// Every process on the system, this one included, for `/proc/<pid>`
    /// and the listing of `/proc` (what `ps` and `htop` read). Empty where
    /// the kernel knows only this process.
    fn others(&self) -> Vec<crate::proc::ProcEntry> {
        Vec::new()
    }
    /// A session's controlling terminal, as its pts number and the terminal's
    /// foreground process group (`stat`'s `tty_nr` and `tpgid`).
    fn tty_of(&self, _sid: i32) -> Option<(u32, i32)> {
        None
    }
    /// The process task `id` is part of, where `id` is a thread that is not
    /// its process's first (`ProcTable::task_owner`): `/proc/<tid>` is that
    /// thread's process's, as Linux shows it.
    fn task_owner(&self, _id: i32) -> Option<i32> {
        None
    }
    /// The process's mounts ([`crate::vfs::Namespace::mount_table`]).
    fn mount_table(&self) -> Vec<(String, &'static str)> {
        Vec::new()
    }
    /// How many times the mount table has changed, and the wait queue
    /// notified at each change ([`crate::vfs::VfsView::mounts_queue`]).
    fn mount_changes(&self) -> (u64, u32) {
        (0, 0)
    }
    /// What this process has used: `stat`'s times and sizes, `statm`.
    fn usage(&self) -> Usage {
        Usage::default()
    }
    /// The system's uptime, processor time and load, for `/proc/uptime`,
    /// `/proc/stat` and `/proc/loadavg`; `None` where the kernel knows none.
    fn system(&self) -> Option<System> {
        None
    }
    /// The wall clock, seconds since the epoch (`/proc/stat`'s `btime`).
    fn wall_secs(&self) -> u64 {
        0
    }
}

use crate::proc::{TaskUsage, Usage};

/// The whole system as `/proc` shows it.
#[derive(Clone, Copy, Debug, Default)]
pub struct System {
    /// The monotonic clock: time since boot.
    pub uptime_ns: u64,
    /// The processor time every process has used ([`crate::proc::ProcTable::cpu_total`]).
    pub cpu_ns: u64,
    /// Hundredths ([`crate::proc::ProcTable::loadavg`]).
    pub load: [u32; 3],
    /// The memory every process has mapped and accessible, summed.
    pub rss: u64,
    /// How many processes, and the newest pid.
    pub procs: usize,
    pub last_pid: i32,
}

/// `USER_HZ`: the clock ticks `/proc` counts processor time in.
const TICK_NS: u64 = 10_000_000;

/// `/proc/uptime`: seconds since boot, and the seconds the processors
/// spent idle (all of them, so up to their count times the first): their
/// time less what every process used.
pub fn uptime_text(sys: &System, cpus: u64) -> String {
    let idle = (sys.uptime_ns * cpus).saturating_sub(sys.cpu_ns);
    let cs = |ns: u64| format!("{}.{:02}", ns / 1_000_000_000, ns % 1_000_000_000 / 10_000_000);
    format!("{} {}\n", cs(sys.uptime_ns), cs(idle))
}

/// `/proc/stat`: the processors' time in ticks, all of it, then each's.
/// Busy time is all user time (nothing tells a process's own work from the
/// kernel's on its behalf), and is split evenly between the processors: the
/// browser does not say which core ran a Worker. Idle is the rest of each
/// one's time since boot. `btime` is the wall clock at boot.
pub fn stat_text(sys: &System, cpus: u64, wall_s: u64) -> String {
    let cpus = cpus.max(1);
    let up = sys.uptime_ns / TICK_NS;
    let busy = sys.cpu_ns / TICK_NS;
    let mut s = format!("cpu  {busy} 0 0 {} 0 0 0 0 0 0\n", (up * cpus).saturating_sub(busy));
    for i in 0..cpus {
        let b = busy / cpus + u64::from(i < busy % cpus);
        s.push_str(&format!("cpu{i} {b} 0 0 {} 0 0 0 0 0 0\n", up.saturating_sub(b)));
    }
    s.push_str(&format!(
        "intr 0\nctxt 0\nbtime {}\nprocesses {}\nprocs_running 1\nprocs_blocked 0\n",
        wall_s.saturating_sub(sys.uptime_ns / 1_000_000_000),
        sys.last_pid.max(1)
    ));
    s
}

/// `/proc/loadavg`. Of the running count, only the reader is known to be
/// running: a parked Worker says nothing as it parks.
pub fn loadavg_text(sys: &System) -> String {
    let l = |h: u32| format!("{}.{:02}", h / 100, h % 100);
    format!("{} {} {} 1/{} {}\n", l(sys.load[0]), l(sys.load[1]), l(sys.load[2]), sys.procs.max(1), sys.last_pid)
}

/// `/proc/<pid>/statm`, in pages: the size, what is resident, and nothing
/// for the shared, text, library, data and dirty counts, which are not kept.
pub fn statm_text(u: &Usage) -> String {
    format!("{} {} 0 0 0 0 0\n", u.vsize / 4096, u.rss / 4096)
}

/// A mount point as Linux writes one in a mount table: space, tab, newline
/// and backslash in octal.
fn mount_path(at: &str) -> String {
    let mut out = String::with_capacity(at.len());
    for c in at.chars() {
        match c {
            ' ' | '\t' | '\n' | '\\' => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `/proc/self/mountinfo`: one line per mount, Linux's fields -- id,
/// parent's id, device (`st_dev`, 0 everywhere here), root, mount point,
/// options, `-`, type, source, superblock options. The root's parent is 0,
/// one not listed, as a chroot's is.
pub fn mountinfo(table: &[(String, &'static str)]) -> String {
    let mut out = String::new();
    for (at, ty) in table {
        let parent = if at == "/" {
            0
        } else {
            table
                .iter()
                .filter(|(p, _)| p != at && (p == "/" || at.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('/'))))
                .max_by_key(|(p, _)| p.len())
                .map_or(1, |(p, _)| crate::vfs::mount_id(p))
        };
        let id = crate::vfs::mount_id(at);
        out.push_str(&format!("{id} {parent} 0:0 / {} rw,relatime - {ty} {ty} rw\n", mount_path(at)));
    }
    out
}

/// `/proc/mounts` (`/proc/self/mounts`, what `/etc/mtab` links to): source,
/// mount point, type, options, and the two zeros.
pub fn mounts(table: &[(String, &'static str)]) -> String {
    table.iter().map(|(at, ty)| format!("{ty} {} {ty} rw,relatime 0 0\n", mount_path(at))).collect()
}

/// A `/proc/<pid>/stat` line: all 52 fields, as Linux has them. `ps` and
/// `htop` parse it strictly -- by position after the `)` -- and a short line
/// is a process they skip (htop listed none) or a fatal error (`ps`: "fatal
/// library error, reap"). `id` is the task's: the pid for a process, a
/// thread's id in its `task/<tid>/stat`; `threads` counts the process's.
/// The times, start and sizes are the process's `u`: all of its processor
/// time counts as user time, a thread's as its process's.
#[allow(clippy::too_many_arguments)]
pub fn stat_line(id: i32, comm: &str, state: char, ppid: i32, pgid: i32, sid: i32, tty: Option<(u32, i32)>, threads: usize, u: &Usage) -> String {
    let (tty_nr, tpgid) = tty.map_or((0, -1), |(n, fg)| (crate::pty::tty_nr(n), if fg > 0 { fg } else { -1 }));
    // pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt
    // majflt cmajflt utime stime cutime cstime priority nice num_threads
    // itrealvalue starttime vsize rss rsslim startcode endcode startstack
    // kstkesp kstkeip signal blocked sigignore sigcatch wchan nswap cnswap
    // exit_signal processor rt_priority policy delayacct_blkio_ticks
    // guest_time cguest_time start_data end_data start_brk arg_start arg_end
    // env_start env_end exit_code
    let (utime, cutime, start) = (u.cpu_ns / TICK_NS, u.child_cpu_ns / TICK_NS, u.start_ns / TICK_NS);
    let (vsize, rss) = (u.vsize, u.rss / 4096);
    format!(
        "{id} ({comm}) {state} {ppid} {pgid} {sid} {tty_nr} {tpgid} 4194560 0 0 0 0 {utime} 0 {cutime} 0 20 0 {threads} 0 {start} {vsize} {rss} \
         18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
    )
}

/// `/proc/<pid>/status`, the fields programs look for: task `id` of process
/// `tgid`, which has `threads`.
pub fn status_text(id: i32, tgid: i32, comm: &str, state: char, ppid: i32, threads: usize, creds: &[u8], u: &Usage) -> String {
    let st = if state == 'Z' { "Z (zombie)" } else { "R (running)" };
    let (uid, gid, caps) = status_ids(creds);
    let (size, rss) = (u.vsize >> 10, u.rss >> 10);
    format!(
        "Name:\t{comm}\nUmask:\t0022\nState:\t{st}\nTgid:\t{tgid}\nNgid:\t0\nPid:\t{id}\n\
         PPid:\t{ppid}\nTracerPid:\t0\n{uid}{gid}FDSize:\t64\n\
         Groups:\t\nVmSize:\t{size:8} kB\nVmRSS:\t{rss:8} kB\nThreads:\t{threads}\n{caps}voluntary_ctxt_switches:\t0\n\
         nonvoluntary_ctxt_switches:\t0\n"
    )
}

/// `status`'s `Uid:` and `Gid:` lines (real, effective, saved and
/// filesystem ids, the last the effective one) and its `Cap*:` lines, from
/// [`crate::syscalls::Creds::save`]'s bytes; root's, every capability, for
/// a process that has published none.
pub fn status_ids(creds: &[u8]) -> (String, String, String) {
    let c = crate::syscalls::Creds::new(0, 0);
    c.restore(creds);
    let ([r, e, s], [gr, ge, gs]) = (c.uids(), c.gids());
    let [eff, perm, inh] = c.caps();
    (
        format!("Uid:\t{r}\t{e}\t{s}\t{e}\n"),
        format!("Gid:\t{gr}\t{ge}\t{gs}\t{ge}\n"),
        format!(
            "CapInh:\t{inh:016x}\nCapPrm:\t{perm:016x}\nCapEff:\t{eff:016x}\nCapBnd:\t{:016x}\nCapAmb:\t{:016x}\n",
            crate::syscalls::CAPS_ALL,
            0
        ),
    )
}

/// A generated file: contents are captured when it is opened.
#[derive(Debug)]
struct Generated {
    data: Vec<u8>,
}

/// `/proc/self/mountinfo` as opened: the table then, and, as Linux's, a
/// wakeup at each later change of the mount table, which a wait reports
/// as `POLLPRI` (with `POLLERR`) once. libmount's monitor waits on it,
/// edge-triggered, for mounts that come and go.
struct MountInfo {
    file: Generated,
    source: Arc<dyn ProcSource>,
    /// The change count this reader last had reported to it.
    seen: AtomicU64,
}

impl std::fmt::Debug for MountInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountInfo").field("seen", &self.seen).finish()
    }
}

impl FileHandle for MountInfo {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        self.file.read_at(offset, buf)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        self.file.metadata()
    }
    fn wait_queue(&self) -> u32 {
        self.source.mount_changes().1
    }
    fn wakes(&self) -> Option<u64> {
        Some(self.source.mount_changes().0)
    }
    fn poll_pri(&self) -> bool {
        self.source.mount_changes().0 != self.seen.load(Ordering::SeqCst)
    }
    fn poll_reported(&self, revents: u16) {
        const POLLPRI: u16 = 0x002;
        if revents & POLLPRI != 0 {
            self.seen.store(self.source.mount_changes().0, Ordering::SeqCst);
        }
    }
}

impl FileHandle for Generated {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let start = (offset as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        Ok(n as u64)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        // Linux reports size 0 for most of /proc; programs are expected to
        // read until EOF rather than trust it, and some break if a stale
        // size disagrees with the contents.
        Ok(Metadata {
            file_type: FileType::Regular,
            size: 0,
            mode: 0o444,
            ino: 1,
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }
}

#[derive(Debug)]
struct GenDir {
    entries: Vec<DirEntry>,
}

impl FileHandle for GenDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::dir())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        Ok(self.entries.clone())
    }
}

/// The `/proc` filesystem.
pub struct ProcFs {
    source: Arc<dyn ProcSource>,
}

impl ProcFs {
    pub fn new(source: Arc<dyn ProcSource>) -> ProcFs {
        ProcFs { source }
    }

    /// Rewrite a `<pid>/…` path to `self/…` so both spellings share one
    /// implementation, as they do on Linux for the current process, and say
    /// which task it is about: 0 for the process (its first thread), or a
    /// thread's id. A thread's files are under `<pid>/task/<tid>`, and under
    /// `/proc/<tid>` too, which Linux serves though it does not list it;
    /// `ENOENT` for a `task/<tid>` that is not one of the process's.
    fn canonical(&self, path: &str) -> Result<(String, i32), Errno> {
        let (first, rest) = path.split_at(path.find('/').unwrap_or(path.len()));
        let mut tid = 0;
        let mut path = match first.parse::<i32>().ok().and_then(|id| Some(id).zip(self.source.task_owner(id))) {
            Some((id, owner)) if owner != id => {
                tid = id;
                format!("{owner}{rest}")
            }
            _ => path.to_string(),
        };
        let pid = self.source.pid().to_string();
        if let Some(rest) = path.strip_prefix(&pid).filter(|r| r.is_empty() || r.starts_with('/')) {
            path = format!("self{rest}");
        }
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        if parts.len() >= 3 && parts[1] == "task" {
            let t: i32 = parts[2].parse().map_err(|_| Errno::ENOENT)?;
            if !self.tasks_of(parts[0]).contains(&t) {
                return Err(Errno::ENOENT);
            }
            tid = t;
            path = match parts.get(3) {
                Some(r) => format!("{}/{r}", parts[0]),
                None => parts[0].to_string(),
            };
        }
        Ok((path, tid))
    }

    /// The tasks of `self` or of process `<pid>`: its first thread, then the
    /// others. Just the first where the table does not list it.
    fn tasks_of(&self, who: &str) -> Vec<i32> {
        let pid = if who == "self" { self.source.pid() } else { who.parse().unwrap_or(0) };
        self.source.others().into_iter().find(|e| e.pid == pid).map_or_else(|| vec![pid], |e| e.tasks())
    }

    /// Another process's `/proc/<pid>/…`: its entry and the rest of the path.
    fn other(&self, path: &str) -> Option<(crate::proc::ProcEntry, String)> {
        let (first, rest) = path.split_once('/').unwrap_or((path, ""));
        let pid: i32 = first.parse().ok()?;
        let e = self.source.others().into_iter().find(|e| e.pid == pid)?;
        Some((e, rest.to_string()))
    }

    fn generate_other(&self, e: &crate::proc::ProcEntry, rest: &str, tid: i32) -> Option<Vec<u8>> {
        let state = if e.zombie { 'Z' } else { 'S' };
        let (id, threads) = (if tid != 0 { tid } else { e.pid }, e.tasks().len());
        // a task's own name and time; the process's for `/proc/<pid>/…`
        let (comm, usage) = if tid != 0 {
            (e.task_comm(tid), Usage { cpu_ns: e.task(tid).cpu_ns, child_cpu_ns: 0, ..e.usage })
        } else {
            (e.comm(), e.usage)
        };
        Some(match rest {
            "stat" => stat_line(id, &comm, state, e.ppid, e.pgid, e.sid, self.source.tty_of(e.sid), threads, &usage).into_bytes(),
            "status" => status_text(id, e.pid, &comm, state, e.ppid, threads, &e.creds, &usage).into_bytes(),
            "statm" => statm_text(&e.usage).into_bytes(),
            "cmdline" => e.cmdline().to_vec(),
            "comm" => format!("{comm}\n").into_bytes(),
            // Another process's memory and environment are not ours to show.
            "environ" | "maps" => Vec::new(),
            _ => return None,
        })
    }

    fn generate(&self, path: &str, tid: i32) -> Option<Vec<u8>> {
        let s = &self.source;
        let id = if tid != 0 { tid } else { s.pid() };
        // `task/<tid>/…`: the task's own time and name, as the table has them
        let task = || (tid != 0).then(|| s.others().into_iter().find(|e| e.pid == s.pid()).map(|e| e.task(tid)).unwrap_or_default());
        Some(match path {
            "self/maps" => s.maps().into_bytes(),
            "self/cmdline" => s.cmdline(),
            "self/environ" => s.environ(),
            "self/stat" => s.stat(id, self.tasks_of("self").len(), task().as_ref()).into_bytes(),
            "self/status" => s.status(id, self.tasks_of("self").len(), task().as_ref()).into_bytes(),
            "self/statm" => statm_text(&s.usage()).into_bytes(),
            "self/mountinfo" => mountinfo(&s.mount_table()).into_bytes(),
            "self/mounts" | "mounts" => mounts(&s.mount_table()).into_bytes(),
            p if p.starts_with("self/fdinfo/") => s.fd_info(p["self/fdinfo/".len()..].parse().ok()?)?.into_bytes(),
            "loadavg" => match s.system() {
                Some(sys) => loadavg_text(&sys).into_bytes(),
                None => {
                    let n = s.others().len().max(1);
                    format!("0.00 0.00 0.00 1/{n} {}\n", s.pid()).into_bytes()
                }
            },
            "self/comm" => {
                if let Some(name) = task().and_then(|t| t.own_name()) {
                    return Some(format!("{name}\n").into_bytes());
                }
                let exe = s.exe();
                let name = exe.rsplit('/').next().unwrap_or("gex").to_string();
                format!("{name}\n").into_bytes()
            }
            "meminfo" => match s.system() {
                // A fixed 4 GiB total: the browser does not say how much
                // the machine has. In use: what the processes have mapped
                // and accessible, summed.
                Some(sys) => {
                    let free = (4u64 << 30).saturating_sub(sys.rss) >> 10;
                    format!(
                        "MemTotal:        4194304 kB\nMemFree:        {free:8} kB\nMemAvailable:   {free:8} kB\n\
                         Buffers:               0 kB\nCached:                0 kB\nSwapTotal:             0 kB\n\
                         SwapFree:              0 kB\n"
                    )
                    .into_bytes()
                }
                None => concat!(
                    "MemTotal:        4194304 kB\n",
                    "MemFree:         2097152 kB\n",
                    "MemAvailable:    3145728 kB\n",
                    "Buffers:               0 kB\n",
                    "Cached:                0 kB\n",
                    "SwapTotal:             0 kB\n",
                    "SwapFree:              0 kB\n",
                )
                .as_bytes()
                .to_vec(),
            },
            "cpuinfo" if gex_abi::X86 => crate::cpus::proc_cpuinfo(crate::x86::CPUINFO).into_bytes(),
            "cpuinfo" => crate::cpus::proc_cpuinfo(concat!(
                "processor\t: 0\n",
                "BogoMIPS\t: 100.00\n",
                "Features\t: fp asimd atomics cpuid\n",
                "CPU implementer\t: 0x41\n",
                "CPU architecture: 8\n",
                "CPU variant\t: 0x0\n",
                "CPU part\t: 0xd08\n",
                "CPU revision\t: 3\n\n",
            ))
            .into_bytes(),
            "version" => b"Linux version 6.1.0-gex (gex@puter) #1 SMP\n".to_vec(),
            "uptime" => match s.system() {
                Some(sys) => uptime_text(&sys, crate::cpus::count() as u64).into_bytes(),
                None => b"1000.00 1000.00\n".to_vec(),
            },
            // One cpu line per processor (`crate::cpus`). Boehm's GC counts
            // the cpu lines to size its markers, and glibc's get_nprocs falls
            // back to them when there is no sysfs.
            "stat" => match s.system() {
                Some(sys) => stat_text(&sys, crate::cpus::count() as u64, s.wall_secs()).into_bytes(),
                None => crate::cpus::proc_stat().into_bytes(),
            },
            "filesystems" => b"nodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\n\text4\n".to_vec(),
            "sys/kernel/osrelease" => b"6.1.0-gex\n".to_vec(),
            "sys/kernel/pid_max" => b"32768\n".to_vec(),
            // glibc and musl both consult this before using large mappings.
            "sys/vm/overcommit_memory" => b"0\n".to_vec(),
            _ => return None,
        })
    }
}

impl FileSystem for ProcFs {
    fn fs_type(&self) -> &'static str {
        "proc"
    }
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        // A directory opens for reading only.
        if super::writes(flags) && self.metadata(path).is_ok_and(|md| md.file_type == FileType::Directory) {
            return Err(Errno::EISDIR);
        }
        let (path, tid) = self.canonical(path)?;
        if path == "self/mountinfo" {
            // The count before the table: a change after it is one this
            // reader has not seen.
            let (seen, _) = self.source.mount_changes();
            let data = mountinfo(&self.source.mount_table()).into_bytes();
            return Ok(Arc::new(MountInfo {
                file: Generated { data },
                source: Arc::clone(&self.source),
                seen: AtomicU64::new(seen),
            }));
        }
        if let Some(data) = self.generate(&path, tid) {
            return Ok(Arc::new(Generated { data }));
        }
        if let Some((e, rest)) = self.other(&path) {
            if let Some(data) = self.generate_other(&e, &rest, tid) {
                return Ok(Arc::new(Generated { data }));
            }
            let mut out = base_entries();
            let names: &[&str] = match rest.as_str() {
                "" => &["stat", "status", "statm", "cmdline", "comm", "environ", "maps", "task"],
                "task" => {
                    for t in e.tasks() {
                        out.push(DirEntry { name: t.to_string(), file_type: FileType::Directory, ino: 1 });
                    }
                    &[]
                }
                _ => return Err(Errno::ENOENT),
            };
            for name in names {
                let file_type = if *name == "task" { FileType::Directory } else { FileType::Regular };
                out.push(DirEntry { name: name.to_string(), file_type, ino: 1 });
            }
            return Ok(Arc::new(GenDir { entries: out }));
        }
        if path == "self/task" {
            let mut out = base_entries();
            for t in self.tasks_of("self") {
                out.push(DirEntry { name: t.to_string(), file_type: FileType::Directory, ino: 1 });
            }
            return Ok(Arc::new(GenDir { entries: out }));
        }
        let entries = match path.as_str() {
            "" => {
                // Every process, as Linux lists them: what `ps` and `htop`
                // scan for. This one too, under its number as well as `self`.
                let mut out = base_entries();
                let mut pids: Vec<i32> = self.source.others().iter().map(|e| e.pid).collect();
                if !pids.contains(&self.source.pid()) {
                    pids.push(self.source.pid());
                }
                pids.sort_unstable();
                for pid in pids {
                    out.push(DirEntry { name: pid.to_string(), file_type: FileType::Directory, ino: 1 });
                }
                for name in ["self", "meminfo", "cpuinfo", "version", "uptime", "stat", "loadavg", "mounts", "sys"] {
                    let file_type = if matches!(name, "self" | "sys") { FileType::Directory } else { FileType::Regular };
                    out.push(DirEntry { name: name.to_string(), file_type, ino: 1 });
                }
                return Ok(Arc::new(GenDir { entries: out }));
            }
            "self" => vec![
                "maps", "cmdline", "environ", "stat", "status", "statm", "comm", "exe", "cwd", "fd",
                "fdinfo", "task", "mountinfo", "mounts",
            ],
            "self/fd" => {
                let mut out = base_entries();
                for (fd, _) in self.source.fds() {
                    out.push(DirEntry {
                        name: fd.to_string(),
                        file_type: FileType::Symlink,
                        ino: 1,
                    });
                }
                return Ok(Arc::new(GenDir { entries: out }));
            }
            "self/fdinfo" => {
                let mut out = base_entries();
                for (fd, _) in self.source.fds() {
                    out.push(DirEntry { name: fd.to_string(), file_type: FileType::Regular, ino: 1 });
                }
                return Ok(Arc::new(GenDir { entries: out }));
            }
            "sys" => vec!["kernel", "vm"],
            "sys/kernel" => vec!["osrelease", "pid_max"],
            "sys/vm" => vec!["overcommit_memory"],
            _ => return Err(Errno::ENOENT),
        };
        let mut out = base_entries();
        for name in entries {
            out.push(DirEntry {
                name: name.to_string(),
                file_type: FileType::Regular,
                ino: 1,
            });
        }
        Ok(Arc::new(GenDir { entries: out }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let (path, tid) = self.canonical(path)?;
        if self.generate(&path, tid).is_some() {
            return Ok(Metadata {
                file_type: FileType::Regular,
                size: 0,
                mode: 0o444,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            });
        }
        if let Some((e, rest)) = self.other(&path) {
            return match rest.as_str() {
                "" | "task" => Ok(Metadata::dir()),
                r if self.generate_other(&e, r, tid).is_some() => Ok(Metadata {
                    file_type: FileType::Regular,
                    size: 0,
                    mode: 0o444,
                    ino: 1,
                    mtime: 0,
                    nlink: 1,
                    rdev: 0,
                    times: Default::default(),
                }),
                _ => Err(Errno::ENOENT),
            };
        }
        match path.as_str() {
            "" | "self" | "self/fd" | "self/fdinfo" | "self/task" | "sys" | "sys/kernel" | "sys/vm" => Ok(Metadata::dir()),
            "self/exe" | "self/cwd" => Ok(Metadata {
                file_type: FileType::Symlink,
                size: 0,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            }),
            _ => Err(Errno::ENOENT),
        }
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let (path, _) = self.canonical(path)?;
        match path.as_str() {
            "self/exe" => Ok(self.source.exe()),
            "self/cwd" => Ok(self.source.cwd()),
            other => {
                if let Some(n) = other.strip_prefix("self/fd/") {
                    let fd: i32 = n.parse().map_err(|_| Errno::ENOENT)?;
                    return self.source.fd_link(fd).ok_or(Errno::ENOENT);
                }
                self.metadata(other)?;
                Err(Errno::EINVAL)
            }
        }
    }
}

fn base_entries() -> Vec<DirEntry> {
    vec![
        DirEntry {
            name: ".".into(),
            file_type: FileType::Directory,
            ino: 1,
        },
        DirEntry {
            name: "..".into(),
            file_type: FileType::Directory,
            ino: 1,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl ProcSource for Fake {
        fn pid(&self) -> i32 {
            42
        }
        fn maps(&self) -> String {
            "00400000-00401000 r-xp 00000000 00:00 0 [image]\n".into()
        }
        fn cmdline(&self) -> Vec<u8> {
            b"/bin/prog\0arg\0".to_vec()
        }
        fn environ(&self) -> Vec<u8> {
            b"PATH=/bin\0".to_vec()
        }
        fn exe(&self) -> String {
            "/bin/prog".into()
        }
        fn cwd(&self) -> String {
            "/tmp".into()
        }
        fn fds(&self) -> Vec<(i32, String)> {
            vec![(0, "/dev/tty".into()), (3, "/tmp/x".into())]
        }
        fn stat(&self, tid: i32, threads: usize, _task: Option<&TaskUsage>) -> String {
            stat_line(tid, "prog", 'R', 7, 42, 1, None, threads, &Usage::default())
        }
        fn status(&self, tid: i32, threads: usize, _task: Option<&TaskUsage>) -> String {
            status_text(tid, 42, "prog", 'R', 7, threads, &[], &Usage::default())
        }
        fn task_owner(&self, id: i32) -> Option<i32> {
            self.others().into_iter().find(|e| e.tasks().contains(&id)).map(|e| e.pid)
        }
        fn others(&self) -> Vec<crate::proc::ProcEntry> {
            let e = |pid, ppid, info: &[u8], tids: &[i32]| crate::proc::ProcEntry {
                pid,
                ppid,
                pgid: pid,
                sid: 1,
                zombie: false,
                info: info.to_vec(),
                tids: tids.to_vec(),
                creds: Vec::new(),
                usage: Usage::default(),
                task_usage: Vec::new(),
            };
            let mut io = [0u8; 16];
            io[..2].copy_from_slice(b"io");
            let mut xterm = e(7, 1, b"xterm\nxterm\0-e\0bash\0", &[8, 9]);
            xterm.usage.cpu_ns = 4_000_000_000;
            xterm.task_usage = vec![TaskUsage::default(), TaskUsage { cpu_ns: 2_500_000_000, name: io }, TaskUsage { cpu_ns: 1_500_000_000, name: [0; 16] }];
            vec![e(1, 0, b"bash\n/bin/bash\0/etc/session\0", &[]), xterm, e(42, 7, b"prog\n", &[43])]
        }
    }

    fn read(f: &ProcFs, path: &str) -> String {
        let h = f.open(path, 0, 0).unwrap();
        let mut buf = [0u8; 4096];
        let n = h.read_at(0, &mut buf).unwrap() as usize;
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn the_root_lists_every_process() {
        let names: Vec<String> = fs().open("", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        for pid in ["1", "7", "42", "self", "loadavg"] {
            assert!(names.iter().any(|n| n == pid), "{pid} missing from {names:?}");
        }
    }

    #[test]
    fn another_process_has_its_stat_and_cmdline() {
        let f = fs();
        let stat = read(&f, "7/stat");
        assert!(stat.starts_with("7 (xterm) S 1 7 1 "), "{stat}");
        // htop and procps parse by position after the ')': all 52 fields.
        assert_eq!(stat.split_whitespace().count(), 52, "{stat}");
        assert_eq!(read(&f, "7/cmdline"), "xterm\0-e\0bash\0");
        assert_eq!(read(&f, "1/comm"), "bash\n");
        assert!(read(&f, "1/status").contains("PPid:\t0\n"));
        // Its first thread's directory is the process's, with the first
        // thread's own time.
        assert_eq!(read(&f, "7/task/7/stat").replace(" 0 0 0 0 20 ", " 400 0 0 0 20 "), stat);
        let task: Vec<String> = f.open("7/task", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(task, [".", "..", "7", "8", "9"]);
        assert!(f.metadata("7").unwrap().file_type == FileType::Directory);
        assert_eq!(f.open("99/stat", 0, 0).err().map(|_| ()), Some(()));
    }

    #[test]
    fn this_process_keeps_its_own_view() {
        // Its own pid is `self`, served from its own state, not the table's.
        assert_eq!(read(&fs(), "42/cmdline"), "/bin/prog\0arg\0");
        assert_eq!(stat_line(1, "x", 'R', 0, 1, 1, None, 1, &Usage::default()).split_whitespace().count(), 52);
        let f: Vec<String> = stat_line(5, "sh", 'S', 1, 5, 5, Some((3, 7)), 1, &Usage::default()).split_whitespace().map(String::from).collect();
        assert_eq!((f[6].as_str(), f[7].as_str()), ("34819", "7"), "tty_nr is 136:3, tpgid the foreground group");
    }

    #[test]
    fn the_times_and_sizes_are_the_processes_own() {
        let u = Usage { cpu_ns: 2_340_000_000, child_cpu_ns: 50_000_000, rss: 8 << 20, vsize: 64 << 20, start_ns: 12_000_000_000 };
        let f: Vec<String> = stat_line(9, "btop", 'R', 1, 9, 1, None, 2, &u).split_whitespace().map(String::from).collect();
        assert_eq!(f.len(), 52);
        // utime stime cutime cstime (14-17), starttime (22), vsize, rss (pages)
        assert_eq!(&f[13..17], ["234", "0", "5", "0"]);
        assert_eq!(&f[21..24], ["1200", "67108864", "2048"]);
        assert_eq!(statm_text(&u), "16384 2048 0 0 0 0 0\n");
        assert!(status_text(9, 9, "btop", 'R', 1, 2, &[], &u).contains("VmRSS:\t    8192 kB\n"));
    }

    #[test]
    fn the_system_is_its_clock_and_its_processes_time() {
        let sys = System { uptime_ns: 100_000_000_000, cpu_ns: 30_000_000_000, load: [125, 50, 3], rss: 1 << 30, procs: 12, last_pid: 77 };
        // two processors for 100 s, 30 s of them busy
        assert_eq!(uptime_text(&sys, 2), "100.00 170.00\n");
        let st = stat_text(&sys, 2, 1_700_000_100);
        let lines: Vec<&str> = st.lines().collect();
        assert_eq!(lines[0], "cpu  3000 0 0 17000 0 0 0 0 0 0");
        assert_eq!(lines[1], "cpu0 1500 0 0 8500 0 0 0 0 0 0");
        assert_eq!(lines[2], "cpu1 1500 0 0 8500 0 0 0 0 0 0");
        assert!(st.contains("btime 1700000000\n"), "{st}");
        assert_eq!(loadavg_text(&sys), "1.25 0.50 0.03 1/12 77\n");
    }

    #[test]
    fn each_thread_is_a_task() {
        let f = fs();
        let field = |s: String, i: usize| s.split_whitespace().nth(i).unwrap().to_string();
        // Another process's threads: each its own stat and status, all
        // counting the process's three.
        let t = read(&f, "7/task/9/stat");
        assert!(t.starts_with("9 (xterm) S 1 7 1 "), "{t}");
        assert_eq!(field(t, 19), "3");
        let st = read(&f, "7/task/8/status");
        assert!(st.contains("Tgid:\t7\n") && st.contains("Pid:\t8\n") && st.contains("Threads:\t3\n"), "{st}");
        assert!(read(&f, "7/status").contains("Pid:\t7\n"));
        // `/proc/<tid>` is the thread's, though `/proc` does not list it.
        assert!(read(&f, "8/status").contains("Pid:\t8\n"));
        let root: Vec<String> = f.open("", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert!(!root.contains(&"8".to_string()));
        // Not a thread of that process.
        assert_eq!(f.open("7/task/43/stat", 0, 0).err(), Some(Errno::ENOENT));
        assert_eq!(f.metadata("1/task/8").err(), Some(Errno::ENOENT));
        // This process's own, from its own state.
        let task: Vec<String> = f.open("self/task", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(task, [".", "..", "42", "43"]);
        assert_eq!(field(read(&f, "self/stat"), 19), "2");
        assert!(read(&f, "self/task/43/stat").starts_with("43 (prog) "));
        assert!(read(&f, "43/status").contains("Pid:\t43\n"));
        assert!(read(&f, "self/status").contains("Threads:\t2\n"));
        assert_eq!(read(&f, "self/task/43/cmdline"), "/bin/prog\0arg\0");
    }

    #[test]
    fn a_task_has_its_own_name_and_time() {
        let f = fs();
        let field = |s: &str, i: usize| s.split_whitespace().nth(i).unwrap().to_string();
        // the process: all of its time, its name
        let p = read(&f, "7/stat");
        assert_eq!((field(&p, 1), field(&p, 13)), ("(xterm)".into(), "400".into()));
        // a thread that named itself: its name and its time, in stat, comm
        // and status, and under /proc/<tid> as well
        let t = read(&f, "7/task/8/stat");
        assert_eq!((field(&t, 0), field(&t, 1), field(&t, 13)), ("8".into(), "(io)".into(), "250".into()));
        assert_eq!(read(&f, "7/task/8/comm"), "io\n");
        assert!(read(&f, "8/status").starts_with("Name:\tio\n"));
        // one that did not: the process's name, its own time
        let t = read(&f, "7/task/9/stat");
        assert_eq!((field(&t, 1), field(&t, 13)), ("(xterm)".into(), "150".into()));
        assert_eq!(read(&f, "7/task/9/comm"), "xterm\n");
    }

    fn fs() -> ProcFs {
        ProcFs::new(Arc::new(Fake))
    }

    #[test]
    fn maps_comes_from_live_state() {
        let f = fs().open("self/maps", 0, 0).unwrap();
        let mut buf = [0u8; 128];
        let n = f.read_at(0, &mut buf).unwrap() as usize;
        assert!(String::from_utf8_lossy(&buf[..n]).contains("[image]"));
    }

    #[test]
    fn numeric_pid_paths_alias_self() {
        let f = fs().open("42/cmdline", 0, 0).unwrap();
        let mut buf = [0u8; 32];
        let n = f.read_at(0, &mut buf).unwrap() as usize;
        assert_eq!(&buf[..n], b"/bin/prog\0arg\0");
    }

    #[test]
    fn exe_and_fd_links_resolve() {
        assert_eq!(fs().read_link("self/exe").unwrap(), "/bin/prog");
        assert_eq!(fs().read_link("self/fd/3").unwrap(), "/tmp/x");
        assert_eq!(fs().read_link("self/fd/9").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn reads_resume_from_an_offset() {
        let f = fs().open("self/maps", 0, 0).unwrap();
        let mut buf = [0u8; 4];
        f.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"0040");
        f.read_at(4, &mut buf).unwrap();
        assert_eq!(&buf, b"0000");
    }
}

/// `/sys/devices/system/cpu`, for the processors `crate::cpus` reports: the
/// `online`, `possible` and `present` lists, and each `cpuN/topology` as one
/// package where every processor is a core of its own. glibc's `get_nprocs`
/// reads `online` first. Wine's `GetLogicalProcessorInformation` needs
/// `online` and reads the topology (it is `STATUS_NOT_IMPLEMENTED` without
/// them), and engines size their thread pools from that.
pub struct SysCpuFs;

impl SysCpuFs {
    fn list(n: u32) -> String {
        if n == 1 { "0\n".into() } else { format!("0-{}\n", n - 1) }
    }

    /// The processor a `cpuN...` path names, and what follows it.
    fn cpu(path: &str) -> Option<(u32, &str)> {
        let rest = path.strip_prefix("cpu")?;
        let end = rest.find('/').unwrap_or(rest.len());
        let i: u32 = rest[..end].parse().ok()?;
        (i < crate::cpus::count()).then_some((i, &rest[end..]))
    }

    fn generate(path: &str) -> Option<Vec<u8>> {
        let n = crate::cpus::count();
        if matches!(path, "online" | "possible" | "present") {
            return Some(Self::list(n).into_bytes());
        }
        let (i, rest) = Self::cpu(path)?;
        let all = crate::cpus::mask();
        Some(
            match rest {
                "/online" => "1\n".to_string(),
                "/topology/physical_package_id" | "/topology/die_id" => "0\n".to_string(),
                "/topology/core_id" | "/topology/cluster_id" => format!("{i}\n"),
                "/topology/thread_siblings" | "/topology/core_cpus" => format!("{:x}\n", 1u64 << i),
                "/topology/thread_siblings_list" | "/topology/core_cpus_list" => format!("{i}\n"),
                "/topology/core_siblings" | "/topology/package_cpus" => format!("{all:x}\n"),
                "/topology/core_siblings_list" | "/topology/package_cpus_list" => Self::list(n),
                _ => return None,
            }
            .into_bytes(),
        )
    }

    fn dir(path: &str) -> Option<Vec<(String, FileType)>> {
        let f = |names: &[&str]| names.iter().map(|s| (s.to_string(), FileType::Regular)).collect::<Vec<_>>();
        if path.is_empty() {
            let mut v = f(&["online", "possible", "present"]);
            for i in 0..crate::cpus::count() {
                v.push((format!("cpu{i}"), FileType::Directory));
            }
            return Some(v);
        }
        match Self::cpu(path)?.1 {
            "" => Some(vec![("online".into(), FileType::Regular), ("topology".into(), FileType::Directory)]),
            "/topology" => Some(f(&[
                "physical_package_id", "die_id", "core_id", "cluster_id", "thread_siblings",
                "thread_siblings_list", "core_cpus", "core_cpus_list", "core_siblings",
                "core_siblings_list", "package_cpus", "package_cpus_list",
            ])),
            _ => None,
        }
    }
}

impl FileSystem for SysCpuFs {
    fn fs_type(&self) -> &'static str {
        "sysfs"
    }
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let path = path.trim_matches('/');
        if let Some(data) = Self::generate(path) {
            if super::writes(flags) {
                return Err(Errno::EACCES);
            }
            return Ok(Arc::new(Generated { data }));
        }
        let names = Self::dir(path).ok_or(Errno::ENOENT)?;
        if super::writes(flags) {
            return Err(Errno::EISDIR);
        }
        let mut entries = base_entries();
        entries.extend(names.into_iter().map(|(name, file_type)| DirEntry { name, file_type, ino: 1 }));
        Ok(Arc::new(GenDir { entries }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let path = path.trim_matches('/');
        if Self::generate(path).is_some() {
            return Ok(Metadata {
                file_type: FileType::Regular,
                size: 4096,
                mode: 0o444,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            });
        }
        Self::dir(path).map(|_| Metadata::dir()).ok_or(Errno::ENOENT)
    }
}

#[cfg(test)]
mod syscpu_tests {
    use super::*;

    fn read(p: &str) -> String {
        let h = SysCpuFs.open(p, 0, 0).unwrap();
        let mut b = vec![0u8; 256];
        let n = h.read_at(0, &mut b).unwrap() as usize;
        String::from_utf8(b[..n].to_vec()).unwrap()
    }

    #[test]
    fn lists_and_topology_follow_the_count() {
        let _g = crate::cpus::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::cpus::set(4);
        assert_eq!(read("online"), "0-3\n");
        assert_eq!(read("/present"), "0-3\n");
        assert_eq!(read("cpu2/topology/thread_siblings"), "4\n");
        assert_eq!(read("cpu2/topology/thread_siblings_list"), "2\n");
        assert_eq!(read("cpu3/topology/core_siblings"), "f\n");
        assert_eq!(read("cpu0/topology/physical_package_id"), "0\n");
        assert!(SysCpuFs.metadata("cpu3/topology").is_ok());
        assert_eq!(SysCpuFs.metadata("cpu4").unwrap_err(), Errno::ENOENT);
        // A filesystem without links: lstat and readlink, each the other's
        // default, answer without asking each other round.
        assert_eq!(SysCpuFs.metadata_nofollow("cpu3/topology").unwrap().file_type, FileType::Directory);
        assert_eq!(SysCpuFs.read_link("cpu3/topology"), Err(Errno::EINVAL));
        assert_eq!(SysCpuFs.read_link("cpu4"), Err(Errno::ENOENT));
        assert!(SysCpuFs.open("cpu9/topology/core_id", 0, 0).is_err());
    }
}
