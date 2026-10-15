//! `/proc` for one process, read from the process's own published state
//! rather than from a snapshot the kernel refreshes (docs/kernel-locking.md,
//! L14a).
//!
//! What `/proc/self` shows is live everywhere it can be: the memory map
//! through its own lock (`Mm::shared`), the descriptor table through its
//! published copy, the working directory through the published namespace
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
        self.0.get().and_then(|l| l.vfs.upgrade()).map_or_else(|| "/".to_string(), |v| v.get().cwd.clone())
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
    fn status(&self) -> String {
        let Some(l) = self.0.get() else { return String::new() };
        let (ppid, _, _) = relations(&l);
        format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nThreads:\t1\n",
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
        k.vfs.set_cwd("/d").unwrap();
        assert_eq!(src.cwd(), "/d");
        // An exec: the new image once published.
        k.argv = vec!["a".into(), "two".into()];
        assert_eq!(src.cmdline(), b"a\0one\0");
        k.publish_proc(&src.0);
        assert_eq!(src.cmdline(), b"a\0two\0");
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
}
