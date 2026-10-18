//! The synthetic `/proc`.
//!
//! Contents are generated on open from live kernel state rather than stored,
//! because that is what makes them true. `self/maps` in particular has to
//! reflect the VMA tree exactly: glibc, OpenSSL and every JVM read it, and
//! C++ exception unwinding locates `.eh_frame` through the same picture of
//! the address space (PLAN §6.4, §8).

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
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
    fn stat(&self) -> String;
    fn status(&self) -> String;
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
}

/// A `/proc/<pid>/stat` line: all 52 fields, as Linux has them. `ps` and
/// `htop` parse it strictly -- by position after the `)` -- and a short line
/// is a process they skip (htop listed none) or a fatal error (`ps`: "fatal
/// library error, reap").
pub fn stat_line(pid: i32, comm: &str, state: char, ppid: i32, pgid: i32, sid: i32, tty: Option<(u32, i32)>) -> String {
    let (tty_nr, tpgid) = tty.map_or((0, -1), |(n, fg)| (crate::pty::tty_nr(n), if fg > 0 { fg } else { -1 }));
    // pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt
    // majflt cmajflt utime stime cutime cstime priority nice num_threads
    // itrealvalue starttime vsize rss rsslim startcode endcode startstack
    // kstkesp kstkeip signal blocked sigignore sigcatch wchan nswap cnswap
    // exit_signal processor rt_priority policy delayacct_blkio_ticks
    // guest_time cguest_time start_data end_data start_brk arg_start arg_end
    // env_start env_end exit_code
    format!(
        "{pid} ({comm}) {state} {ppid} {pgid} {sid} {tty_nr} {tpgid} 4194560 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 \
         18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
    )
}

/// `/proc/<pid>/status`, the fields programs look for.
pub fn status_text(pid: i32, comm: &str, state: char, ppid: i32) -> String {
    let st = if state == 'Z' { "Z (zombie)" } else { "R (running)" };
    format!(
        "Name:\t{comm}\nUmask:\t0022\nState:\t{st}\nTgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\n\
         PPid:\t{ppid}\nTracerPid:\t0\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t64\n\
         Groups:\t\nVmRSS:\t       0 kB\nThreads:\t1\nvoluntary_ctxt_switches:\t0\n\
         nonvoluntary_ctxt_switches:\t0\n"
    )
}

/// A generated file: contents are captured when it is opened.
#[derive(Debug)]
struct Generated {
    data: Vec<u8>,
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
    /// implementation, as they do on Linux for the current process. A
    /// thread's directory (`<pid>/task/<tid>/…`) is its process's: every
    /// process here has the one thread `/proc` reports.
    fn canonical(&self, path: &str) -> String {
        let path = untask(path);
        // `/proc/<tid>`: the thread's process's directory.
        let (first, rest) = path.split_at(path.find('/').unwrap_or(path.len()));
        let path = match first.parse::<i32>().ok().and_then(|id| Some(id).zip(self.source.task_owner(id))) {
            Some((id, owner)) if owner != id => format!("{owner}{rest}"),
            _ => path,
        };
        let pid = self.source.pid().to_string();
        match path.strip_prefix(&pid) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("self{rest}")
            }
            _ => path,
        }
    }

    /// Another process's `/proc/<pid>/…`: its entry and the rest of the path.
    fn other(&self, path: &str) -> Option<(crate::proc::ProcEntry, String)> {
        let (first, rest) = path.split_once('/').unwrap_or((path, ""));
        let pid: i32 = first.parse().ok()?;
        let e = self.source.others().into_iter().find(|e| e.pid == pid)?;
        Some((e, rest.to_string()))
    }

    fn generate_other(&self, e: &crate::proc::ProcEntry, rest: &str) -> Option<Vec<u8>> {
        let comm = e.comm();
        let state = if e.zombie { 'Z' } else { 'S' };
        Some(match rest {
            "stat" => stat_line(e.pid, &comm, state, e.ppid, e.pgid, e.sid, self.source.tty_of(e.sid)).into_bytes(),
            "status" => status_text(e.pid, &comm, state, e.ppid).into_bytes(),
            "statm" => b"0 0 0 0 0 0 0\n".to_vec(),
            "cmdline" => e.cmdline().to_vec(),
            "comm" => format!("{comm}\n").into_bytes(),
            // Another process's memory and environment are not ours to show.
            "environ" | "maps" => Vec::new(),
            _ => return None,
        })
    }

    fn generate(&self, path: &str) -> Option<Vec<u8>> {
        let s = &self.source;
        Some(match path {
            "self/maps" => s.maps().into_bytes(),
            "self/cmdline" => s.cmdline(),
            "self/environ" => s.environ(),
            "self/stat" => s.stat().into_bytes(),
            "self/status" => s.status().into_bytes(),
            "self/statm" => b"0 0 0 0 0 0 0\n".to_vec(),
            "loadavg" => {
                let n = s.others().len().max(1);
                format!("0.00 0.00 0.00 1/{n} {}\n", s.pid()).into_bytes()
            }
            "self/comm" => {
                let exe = s.exe();
                let name = exe.rsplit('/').next().unwrap_or("gex").to_string();
                format!("{name}\n").into_bytes()
            }
            "meminfo" => concat!(
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
            "uptime" => b"1000.00 1000.00\n".to_vec(),
            // One cpu line per processor (`crate::cpus`), all idle, booted at a
            // fixed time. Boehm's GC counts the cpu lines to size its markers,
            // and glibc's get_nprocs falls back to them when there is no sysfs.
            "stat" => crate::cpus::proc_stat().into_bytes(),
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
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        // A directory opens for reading only.
        if super::writes(flags) && self.metadata(path).is_ok_and(|md| md.file_type == FileType::Directory) {
            return Err(Errno::EISDIR);
        }
        let path = self.canonical(path);
        if let Some(data) = self.generate(&path) {
            return Ok(Arc::new(Generated { data }));
        }
        if let Some((e, rest)) = self.other(&path) {
            if let Some(data) = self.generate_other(&e, &rest) {
                return Ok(Arc::new(Generated { data }));
            }
            let mut out = base_entries();
            let names: &[&str] = match rest.as_str() {
                "" => &["stat", "status", "statm", "cmdline", "comm", "environ", "maps", "task"],
                "task" => {
                    out.push(DirEntry { name: e.pid.to_string(), file_type: FileType::Directory, ino: 1 });
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
            out.push(DirEntry { name: self.source.pid().to_string(), file_type: FileType::Directory, ino: 1 });
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
                for name in ["self", "meminfo", "cpuinfo", "version", "uptime", "stat", "loadavg", "sys"] {
                    let file_type = if matches!(name, "self" | "sys") { FileType::Directory } else { FileType::Regular };
                    out.push(DirEntry { name: name.to_string(), file_type, ino: 1 });
                }
                return Ok(Arc::new(GenDir { entries: out }));
            }
            "self" => vec![
                "maps", "cmdline", "environ", "stat", "status", "statm", "comm", "exe", "cwd", "fd",
                "task",
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
        let path = self.canonical(path);
        if self.generate(&path).is_some() {
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
                r if self.generate_other(&e, r).is_some() => Ok(Metadata {
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
            "" | "self" | "self/fd" | "self/task" | "sys" | "sys/kernel" | "sys/vm" => Ok(Metadata::dir()),
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
        let path = self.canonical(path);
        match path.as_str() {
            "self/exe" => Ok(self.source.exe()),
            "self/cwd" => Ok(self.source.cwd()),
            other => {
                if let Some(n) = other.strip_prefix("self/fd/") {
                    let fd: i32 = n.parse().map_err(|_| Errno::ENOENT)?;
                    return self.source.fd_link(fd).ok_or(Errno::ENOENT);
                }
                Err(Errno::EINVAL)
            }
        }
    }
}

/// `<pid>/task/<tid>/rest` -> `<pid>/rest`, and `<pid>/task/<tid>` ->
/// `<pid>`: a thread's directory is its process's.
fn untask(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 3 && parts[1] == "task" && parts[2].parse::<i32>().is_ok() {
        let mut out = parts[0].to_string();
        for p in &parts[3..] {
            out.push('/');
            out.push_str(p);
        }
        return out;
    }
    path.to_string()
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
        fn stat(&self) -> String {
            "42 (prog) R 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0\n".into()
        }
        fn status(&self) -> String {
            "Name:\tprog\nPid:\t42\n".into()
        }
        fn others(&self) -> Vec<crate::proc::ProcEntry> {
            let e = |pid, ppid, info: &[u8]| crate::proc::ProcEntry {
                pid,
                ppid,
                pgid: pid,
                sid: 1,
                zombie: false,
                info: info.to_vec(),
            };
            vec![e(1, 0, b"bash\n/bin/bash\0/etc/session\0"), e(7, 1, b"xterm\nxterm\0-e\0bash\0"), e(42, 7, b"prog\n")]
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
        // A thread's directory is its process's.
        assert_eq!(read(&f, "7/task/7/stat"), stat);
        let task: Vec<String> = f.open("7/task", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert!(task.contains(&"7".to_string()));
        assert!(f.metadata("7").unwrap().file_type == FileType::Directory);
        assert_eq!(f.open("99/stat", 0, 0).err().map(|_| ()), Some(()));
    }

    #[test]
    fn this_process_keeps_its_own_view() {
        // Its own pid is `self`, served from its own state, not the table's.
        assert_eq!(read(&fs(), "42/cmdline"), "/bin/prog\0arg\0");
        assert_eq!(stat_line(1, "x", 'R', 0, 1, 1, None).split_whitespace().count(), 52);
        let f: Vec<String> = stat_line(5, "sh", 'S', 1, 5, 5, Some((3, 7))).split_whitespace().map(String::from).collect();
        assert_eq!((f[6].as_str(), f[7].as_str()), ("34819", "7"), "tty_nr is 136:3, tpgid the foreground group");
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
        assert!(SysCpuFs.open("cpu9/topology/core_id", 0, 0).is_err());
    }
}
