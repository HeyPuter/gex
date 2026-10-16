//! `/proc` for one process, read from the process's own published state
//! rather than from a snapshot the kernel refreshes (docs/kernel-locking.md,
//! L14a).
//!
//! What `/proc/self` shows is live everywhere it can be: the memory map
//! through its own lock (`Mm::shared`), the descriptor table through its
//! published copy, the working directory through the process's fs_struct
//! and the process relations through the process table. What only changes
//! at a process's start and at `execve` -- its image's name, command line
//! and environment -- is published then ([`Kernel::publish_proc`]). So an
//! open of a `/proc` file needs nothing from the kernel lock, and nothing
//! has to be brought up to date before a syscall that might open one.

use crate::file::FdTable;
use crate::mem::Mm;
use crate::proc::ProcTable;
use crate::syscalls::Kernel;
use crate::vfs::{ProcSource, VfsView};
use std::sync::{Arc, Mutex, RwLock, Weak};

/// The handles a process's `/proc` reads, and its image.
///
/// The descriptor table and the namespace are held weakly. The namespace
/// mounts this process's `/proc`, whose source holds this: strong handles
/// made a cycle, so an ended process's descriptor table was never dropped
/// and its pipes and sockets never closed (native W3: the wineserver never
/// saw a client go, and the process that waited for it hung).
struct Live {
    pid: i32,
    procs: Arc<ProcTable>,
    ptys: Option<crate::pty::Remote>,
    mm: Mm,
    fds: Weak<FdTable>,
    vfs: Weak<VfsView>,
    /// The process's fs_struct: its working directory.
    fs: Weak<crate::syscalls::FsCwd>,
    exe: String,
    argv: Vec<String>,
    envp: Vec<String>,
}

/// One process's `/proc` source: empty until the process is first
/// published, then replaced whole at each publication.
#[derive(Default)]
pub struct ProcCell {
    live: RwLock<Option<Arc<Live>>>,
    /// `maps` as last rendered, and the map's generation it was rendered at:
    /// formatting the VMA tree is the expensive part, and between two
    /// changes to the map it is the same string.
    maps: Mutex<(u64, String)>,
}

impl ProcCell {
    fn get(&self) -> Option<Arc<Live>> {
        self.live.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The `/proc` source a process's namespace mounts.
pub struct KernelProcSource(pub Arc<ProcCell>);

impl KernelProcSource {
    pub fn new() -> KernelProcSource {
        KernelProcSource(Arc::new(ProcCell::default()))
    }
}

impl Default for KernelProcSource {
    fn default() -> Self {
        Self::new()
    }
}

fn comm(exe: &str) -> String {
    exe.rsplit('/').next().unwrap_or("gex").chars().take(15).collect()
}

impl ProcSource for KernelProcSource {
    fn pid(&self) -> i32 {
        self.0.get().map_or(0, |l| l.pid)
    }
    fn maps(&self) -> String {
        let Some(l) = self.0.get() else { return String::new() };
        let mut cache = self.0.maps.lock().unwrap_or_else(|e| e.into_inner());
        let mm = l.mm.lock();
        let generation = mm.generation();
        if cache.0 != generation || cache.1.is_empty() {
            *cache = (generation, mm.proc_maps());
        }
        cache.1.clone()
    }
    fn cmdline(&self) -> Vec<u8> {
        self.0.get().map(|l| nul_joined(&l.argv)).unwrap_or_default()
    }
    fn environ(&self) -> Vec<u8> {
        self.0.get().map(|l| nul_joined(&l.envp)).unwrap_or_default()
    }
    fn exe(&self) -> String {
        self.0.get().map(|l| l.exe.clone()).unwrap_or_default()
    }
    fn cwd(&self) -> String {
        self.0.get().and_then(|l| l.fs.upgrade()).map_or_else(
            || "/".to_string(),
            |fs| match fs.get() {
                // Removed, as Linux shows one.
                c if c == crate::syscalls::GONE_CWD => "/ (deleted)".to_string(),
                c => c,
            },
        )
    }
    fn fds(&self) -> Vec<(i32, String)> {
        // A pty end under the name a program can open and `stat`, which is
        // how `ttyname` and `ps` find it.
        self.0
            .get()
            .and_then(|l| l.fds.upgrade())
            .map(|fds| fds.list().into_iter().map(|(fd, p)| (fd, crate::pty::link_name(&p))).collect())
            .unwrap_or_default()
    }
    fn fd_link(&self, fd: i32) -> Option<String> {
        // One descriptor, not the whole table rendered. Its path as opened,
        // with the symlinks on it followed: a program asks this to learn a
        // file's real name in one call (W3's wineserver, for every file it
        // opens, where `realpath` was a `readlink` per directory, each one a
        // proxied system call out of a door).
        let l = self.0.get()?;
        let path = l.fds.upgrade()?.get(fd).ok()?.path.clone();
        if !path.starts_with('/') {
            return Some(crate::pty::link_name(&path));
        }
        let canon = l.vfs.upgrade().map_or_else(|| path.clone(), |v| v.get().canonical(&path));
        Some(crate::pty::link_name(&canon))
    }
    fn stat(&self) -> String {
        let Some(l) = self.0.get() else { return String::new() };
        let (ppid, pgid, sid) = relations(&l);
        let tty = tty_of(l.ptys, sid);
        crate::vfs::procfs::stat_line(l.pid, &comm(&l.exe), 'R', ppid, pgid, sid, tty)
    }
    fn others(&self) -> Vec<crate::proc::ProcEntry> {
        self.0.get().map(|l| l.procs.entries()).unwrap_or_default()
    }
    fn tty_of(&self, sid: i32) -> Option<(u32, i32)> {
        tty_of(self.0.get()?.ptys, sid)
    }
    fn task_owner(&self, id: i32) -> Option<i32> {
        self.0.get()?.procs.task_owner(id)
    }
    fn status(&self) -> String {
        let Some(l) = self.0.get() else { return String::new() };
        let (ppid, _, _) = relations(&l);
        // (no peak is kept: the peaks are the current sizes)
        let (size, rss, anon) = l.mm.lock().vm_counters();
        let (size, rss, anon) = (size >> 10, rss >> 10, anon >> 10);
        format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\n\
             VmPeak:\t{size:8} kB\nVmSize:\t{size:8} kB\nVmHWM:\t{rss:8} kB\nVmRSS:\t{rss:8} kB\n\
             RssAnon:\t{anon:8} kB\nVmSwap:\t       0 kB\nThreads:\t1\n",
            l.exe.rsplit('/').next().unwrap_or("gex"),
            l.pid,
            l.pid,
            ppid
        )
    }
}

/// Parent, group and session, as the process table has them now.
fn relations(l: &Live) -> (i32, i32, i32) {
    (l.procs.ppid(l.pid), l.procs.pgid(l.pid, 0).unwrap_or(l.pid), l.procs.sid(l.pid, 0).unwrap_or(l.pid))
}

/// The session's controlling terminal: its pts number (the pair's id) and
/// foreground group.
fn tty_of(ptys: Option<crate::pty::Remote>, sid: i32) -> Option<(u32, i32)> {
    let r = ptys?;
    let id = (r.for_sid)(sid);
    (id >= 0).then(|| (id as u32, (r.fg)(id)))
}

fn nul_joined(v: &[String]) -> Vec<u8> {
    v.iter().flat_map(|a| a.bytes().chain(std::iter::once(0))).collect()
}

impl Kernel {
    /// Publish this process to the `/proc` it mounts: at its start (and a
    /// fork's, a vfork child's), and after `execve`, which changes the
    /// image. Cheap when nothing changed. Also tells the process table the
    /// image's name and command line once per image, for the other
    /// processes' `/proc/<pid>` (`ps`, `htop`).
    pub fn publish_proc(&self, cell: &ProcCell) {
        let old = cell.get();
        let same = old.as_ref().is_some_and(|o| {
            o.pid == self.pid
                && o.exe == self.exe_path
                && Arc::ptr_eq(&o.procs, &self.procs)
                // (A dead handle's address may be reused: only a live one matches.)
                && o.fds.strong_count() > 0
                && o.vfs.strong_count() > 0
                && std::ptr::eq(o.fds.as_ptr(), Arc::as_ptr(&self.fds))
                && std::ptr::eq(o.vfs.as_ptr(), Arc::as_ptr(self.vfs.view()))
                && o.fs.strong_count() > 0
                && std::ptr::eq(o.fs.as_ptr(), Arc::as_ptr(&self.proc_fs))
                && o.mm.same(&self.mem)
                && o.ptys.is_some() == self.ptys.is_some()
                && o.argv == self.argv
                && o.envp == self.envp
        });
        if same {
            return;
        }
        let live = Live {
            pid: self.pid,
            procs: Arc::clone(&self.procs),
            ptys: self.ptys,
            mm: self.mem.shared(),
            fds: Arc::downgrade(&self.fds),
            vfs: Arc::downgrade(self.vfs.view()),
            fs: Arc::downgrade(&self.proc_fs),
            exe: self.exe_path.clone(),
            argv: self.argv.clone(),
            envp: self.envp.clone(),
        };
        if old.as_ref().is_none_or(|o| o.exe != live.exe || o.argv != live.argv || o.pid != live.pid) {
            let name = live.exe.rsplit('/').next().unwrap_or("gex");
            let mut info = name.as_bytes().to_vec();
            info.push(b'\n');
            info.extend_from_slice(&nul_joined(&live.argv));
            self.procs.set_info(self.pid, &info);
        }
        *cell.maps.lock().unwrap_or_else(|e| e.into_inner()) = (0, String::new());
        *cell.live.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(live));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::tmpfs::TmpFs;

    /// What `/proc/self` shows follows the process without republishing:
    /// the map, the descriptors and the directory live; the image after a
    /// publication.
    #[test]
    fn proc_reads_the_process_live() {
        let mut k = Kernel::new(0x1000, 0x10000, "/bin/a".into());
        k.vfs.mount("/", Arc::new(TmpFs::new()));
        k.vfs.mkdir("/d", 0o755).unwrap();
        k.argv = vec!["a".into(), "one".into()];
        let src = KernelProcSource::new();
        k.publish_proc(&src.0);
        assert_eq!(src.pid(), k.pid);
        assert_eq!(src.cmdline(), b"a\0one\0");
        let at = k.mem.lock().mmap(0, 0x3000, crate::mem::prot::READ, crate::mem::flags::PRIVATE | crate::mem::flags::ANONYMOUS, "").unwrap();
        assert!(src.maps().contains(&format!("{at:x}-{:x} ", at + 0x3000)));
        k.mem.lock().unmap(at, 0x3000).unwrap();
        assert!(!src.maps().contains(&format!("{at:x}-")));
        let h = k.vfs.open("/d/f", crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR, 0o644).unwrap();
        let fd = k.fds.insert(crate::file::OpenFile::new(h, "/d/f".into(), 0, true, true), false).unwrap() as i32;
        assert!(src.fds().contains(&(fd, "/d/f".to_string())));
        k.set_cwd("/d").unwrap();
        assert_eq!(src.cwd(), "/d");
        // An exec: the new image once published.
        k.argv = vec!["a".into(), "two".into()];
        assert_eq!(src.cmdline(), b"a\0one\0");
        k.publish_proc(&src.0);
        assert_eq!(src.cmdline(), b"a\0two\0");
    }

    /// `/proc/self/fd/N` names the file with the symlinks on the path it
    /// was opened by followed, as Linux does, so one `readlink` of it is a
    /// `realpath` of an open file.
    #[test]
    fn an_fd_link_is_the_real_path() {
        let mut k = Kernel::new(0x1000, 0x10000, "/bin/a".into());
        k.vfs.mount("/", Arc::new(TmpFs::new()));
        k.vfs.mkdir("/real", 0o755).unwrap();
        k.vfs.symlink("real", "/via").unwrap();
        k.vfs.symlink("f", "/real/g").unwrap();
        let src = KernelProcSource::new();
        k.publish_proc(&src.0);
        let h = k.vfs.open("/via/f", crate::fs::oflags::O_CREAT | crate::fs::oflags::O_RDWR, 0o644).unwrap();
        let fd = k.fds.insert(crate::file::OpenFile::new(h, "/via/g".into(), 0, true, true), false).unwrap() as i32;
        assert_eq!(src.fd_link(fd).as_deref(), Some("/real/f"));
        // Listed as opened; and a name that walks nowhere is kept.
        assert!(src.fds().contains(&(fd, "/via/g".to_string())));
        let h = k.vfs.open("/real/f", crate::fs::oflags::O_RDONLY, 0).unwrap();
        let gone = k.fds.insert(crate::file::OpenFile::new(h, "/nowhere/x".into(), 0, true, false), false).unwrap() as i32;
        assert_eq!(src.fd_link(gone).as_deref(), Some("/nowhere/x"));
        assert_eq!(src.fd_link(999), None);
    }

    /// A process that mounts its own `/proc` (as every gex process does)
    /// and ends lets go of its descriptors: the source must not keep the
    /// table or the namespace alive through the mount that holds it.
    #[test]
    fn an_ended_process_drops_its_descriptors() {
        let mut k = Kernel::new(0x1000, 0x10000, "/bin/a".into());
        k.vfs.mount("/", Arc::new(TmpFs::new()));
        k.vfs.mkdir("/proc", 0o755).unwrap();
        let cell = Arc::new(ProcCell::default());
        k.vfs.mount("/proc", Arc::new(crate::vfs::ProcFs::new(Arc::new(KernelProcSource(Arc::clone(&cell))))));
        k.publish_proc(&cell);
        let fds = Arc::downgrade(&k.fds);
        drop(cell);
        drop(k);
        assert!(fds.upgrade().is_none(), "the descriptor table outlived its process");
    }

    /// The same with the process's working directory in /proc: its
    /// fs_struct holds that directory open, and `Live` holds the fs_struct
    /// (weakly, like the table; a /proc directory handle does not hold the
    /// cell today, so this guards the shape rather than a known cycle).
    #[test]
    fn an_ended_process_in_proc_drops_its_descriptors() {
        let mut k = Kernel::new(0x1000, 0x10000, "/bin/a".into());
        k.vfs.mount("/", Arc::new(TmpFs::new()));
        k.vfs.mkdir("/proc", 0o755).unwrap();
        let cell = Arc::new(ProcCell::default());
        k.vfs.mount("/proc", Arc::new(crate::vfs::ProcFs::new(Arc::new(KernelProcSource(Arc::clone(&cell))))));
        k.publish_proc(&cell);
        k.set_cwd("/proc/self").unwrap();
        k.publish_proc(&cell);
        assert_eq!(KernelProcSource(Arc::clone(&cell)).cwd(), "/proc/self");
        let fds = Arc::downgrade(&k.fds);
        let fs = Arc::downgrade(&k.proc_fs);
        drop(cell);
        drop(k);
        assert!(fds.upgrade().is_none(), "the descriptor table outlived its process");
        assert!(fs.upgrade().is_none(), "the fs_struct outlived its process");
    }
}
