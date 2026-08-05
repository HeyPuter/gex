//! Syscall entry and the T0 implementations (PLAN §8).
//!
//! `svc #0` arrives here with the guest context still in linear memory: the
//! number in `x8`, arguments in `x0`–`x5`. The kernel reads them from the
//! context, dispatches, and writes the result back to `x0` — the same shape
//! the wasm-compiled kernel will use in the browser.

use crate::errno::{encode, Errno, SysResult};
use crate::fs::{self, mode, oflags, FdTable, OpenFile};
use crate::mem::{flags as mflags, prot, MemoryMap, PAGE_SIZE};
use crate::GuestMem;
use gex_abi::ctx;

/// aarch64 syscall numbers (the asm-generic ABI).
pub mod nr {
    pub const GETCWD: u64 = 17;
    pub const DUP: u64 = 23;
    pub const DUP3: u64 = 24;
    pub const FCNTL: u64 = 25;
    pub const IOCTL: u64 = 29;
    pub const FACCESSAT: u64 = 48;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const LSEEK: u64 = 62;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const READLINKAT: u64 = 78;
    pub const NEWFSTATAT: u64 = 79;
    pub const FSTAT: u64 = 80;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const SCHED_YIELD: u64 = 124;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const UNAME: u64 = 160;
    pub const GETTIMEOFDAY: u64 = 169;
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETUID: u64 = 174;
    pub const GETEUID: u64 = 175;
    pub const GETGID: u64 = 176;
    pub const GETEGID: u64 = 177;
    pub const GETTID: u64 = 178;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const MMAP: u64 = 222;
    pub const MPROTECT: u64 = 226;
    pub const MADVISE: u64 = 233;
    pub const PRLIMIT64: u64 = 261;
    pub const GETRANDOM: u64 = 278;
    pub const RSEQ: u64 = 293;
}

/// What the host should do after a syscall returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyscallOutcome {
    /// Resume the guest.
    Continue,
    /// The process asked to terminate.
    Exit(i32),
}

/// One guest process's kernel state.
pub struct Kernel {
    pub fds: FdTable,
    pub mem: MemoryMap,
    /// Address of the running thread's context struct.
    pub ctx_addr: u64,
    pub pid: i32,
    pub tid: i32,
    /// Where `set_tid_address` asked for the TID to be cleared on exit.
    pub clear_child_tid: u64,
    /// Program name, for `/proc/self/exe` and `AT_EXECFN`.
    pub exe_path: String,
    /// Identity reported to the guest.
    ///
    /// The native shim sets these from the host process so a passthrough run
    /// sees the same identity a native run would; the browser runtime, which
    /// has no host identity to report, keeps the defaults.
    pub uid: u32,
    pub gid: u32,
    /// Trace every syscall to stderr (PLAN §11's strace mode).
    pub trace: bool,
    /// Syscalls seen but not implemented, for the ENOSYS telemetry in
    /// PLAN §14.
    pub unimplemented: Vec<u64>,
}

impl Kernel {
    pub fn new(ctx_addr: u64, brk_start: u64, exe_path: String) -> Kernel {
        Kernel {
            fds: FdTable::new(),
            mem: MemoryMap::new(brk_start),
            ctx_addr,
            pid: 1,
            tid: 1,
            clear_child_tid: 0,
            exe_path,
            uid: 1000,
            gid: 1000,
            trace: false,
            unimplemented: Vec::new(),
        }
    }

    fn arg(&self, m: &GuestMem<'_>, i: u8) -> u64 {
        m.read_u64(self.ctx_addr + ctx::x(i) as u64).unwrap_or(0)
    }

    /// Handle one `svc #0`.
    pub fn syscall(&mut self, m: &mut GuestMem<'_>, pc: u64) -> SyscallOutcome {
        let n = m.read_u64(self.ctx_addr + ctx::x(8) as u64).unwrap_or(!0);
        let a: [u64; 6] = [
            self.arg(m, 0),
            self.arg(m, 1),
            self.arg(m, 2),
            self.arg(m, 3),
            self.arg(m, 4),
            self.arg(m, 5),
        ];

        let outcome = self.dispatch(m, n, a);
        let ret = match &outcome {
            Ok(r) => encode(*r),
            Err(exit) => {
                if self.trace {
                    eprintln!("[gex] exit_group({exit})");
                }
                return SyscallOutcome::Exit(*exit);
            }
        };
        if self.trace {
            eprintln!(
                "[gex] pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}) = {}",
                syscall_name(n),
                a[0],
                a[1],
                a[2],
                ret as i64
            );
        }
        let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, ret);
        SyscallOutcome::Continue
    }

    /// The dispatch table. `Err(code)` means the process is exiting.
    fn dispatch(&mut self, m: &mut GuestMem<'_>, n: u64, a: [u64; 6]) -> Result<SysResult, i32> {
        let r: SysResult = match n {
            nr::EXIT | nr::EXIT_GROUP => return Err((a[0] & 0xff) as i32),

            nr::WRITE => self.sys_write(m, a[0] as i32, a[1], a[2]),
            nr::READ => self.sys_read(m, a[0] as i32, a[1], a[2]),
            nr::WRITEV => self.sys_writev(m, a[0] as i32, a[1], a[2]),
            nr::READV => self.sys_readv(m, a[0] as i32, a[1], a[2]),
            nr::OPENAT => self.sys_openat(m, a[0] as i32, a[1], a[2]),
            nr::CLOSE => self.fds.close(a[0] as i32).map(|_| 0),
            nr::LSEEK => self
                .fds
                .get(a[0] as i32)
                .and_then(|f| f.seek(a[1] as i64, a[2] as u32)),
            nr::DUP => self.fds.dup(a[0] as i32, 0),
            nr::DUP3 => self.sys_dup3(a[0] as i32, a[1] as i32),
            nr::FSTAT => self.sys_fstat(m, a[0] as i32, a[1]),
            nr::NEWFSTATAT => self.sys_fstatat(m, a[0] as i32, a[1], a[2], a[3]),
            nr::FCNTL => self.sys_fcntl(a[0] as i32, a[1], a[2]),
            nr::IOCTL => self.sys_ioctl(m, a[0] as i32, a[1], a[2]),
            nr::FACCESSAT => self.sys_faccessat(m, a[1]),
            nr::READLINKAT => self.sys_readlinkat(m, a[1], a[2], a[3]),
            nr::GETCWD => self.sys_getcwd(m, a[0], a[1]),

            nr::BRK => Ok(self.mem.brk(a[0])),
            nr::MMAP => self.sys_mmap(m, a),
            nr::MUNMAP => self.mem.unmap(a[0], a[1]).map(|_| 0),
            nr::MPROTECT => self
                .mem
                .protect(a[0], a[1], (a[2] & 0x7) as u32)
                .map(|_| 0),
            // Advisory only: gex has no page reclaim to advise.
            nr::MADVISE => Ok(0),

            nr::UNAME => self.sys_uname(m, a[0]),
            nr::GETPID => Ok(self.pid as u64),
            nr::GETPPID => Ok(0),
            nr::GETTID => Ok(self.tid as u64),
            nr::GETUID | nr::GETEUID => Ok(self.uid as u64),
            nr::GETGID | nr::GETEGID => Ok(self.gid as u64),
            nr::SET_TID_ADDRESS => {
                self.clear_child_tid = a[0];
                Ok(self.tid as u64)
            }
            nr::CLOCK_GETTIME => self.sys_clock_gettime(m, a[0], a[1]),
            nr::GETTIMEOFDAY => self.sys_gettimeofday(m, a[0]),
            nr::GETRANDOM => self.sys_getrandom(m, a[0], a[1]),
            nr::SCHED_YIELD => Ok(0),

            // Accepted and recorded, but with no threads or signals yet there
            // is nothing to attach them to. Returning success is what these
            // callers require to proceed; returning ENOSYS would abort musl's
            // startup.
            nr::SET_ROBUST_LIST | nr::RT_SIGACTION | nr::RT_SIGPROCMASK => Ok(0),
            nr::PRLIMIT64 => self.sys_prlimit(m, a[2], a[3]),
            // Restartable sequences are an optimisation the guest can live
            // without, and glibc treats ENOSYS as "not available".
            nr::RSEQ => Err(Errno::ENOSYS),

            _ => {
                if !self.unimplemented.contains(&n) {
                    self.unimplemented.push(n);
                }
                if self.trace {
                    eprintln!("[gex] unimplemented syscall {n} -> ENOSYS");
                }
                Err(Errno::ENOSYS)
            }
        };
        Ok(r)
    }

    // -- I/O ---------------------------------------------------------------

    fn sys_write(&mut self, m: &GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        let data = m.read(buf, len as usize)?.to_vec();
        self.fds.get(fd)?.write(&data)
    }

    fn sys_read(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        let len = len.min(1 << 20) as usize;
        let mut tmp = vec![0u8; len];
        let n = self.fds.get(fd)?.read(&mut tmp)?;
        m.write(buf, &tmp[..n as usize])?;
        Ok(n)
    }

    /// Read an `iovec` array from guest memory.
    fn read_iovecs(m: &GuestMem<'_>, iov: u64, count: u64) -> Result<Vec<(u64, u64)>, Errno> {
        if count > 1024 {
            return Err(Errno::EINVAL);
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let base = iov + i * 16;
            out.push((m.read_u64(base)?, m.read_u64(base + 8)?));
        }
        Ok(out)
    }

    fn sys_writev(&mut self, m: &GuestMem<'_>, fd: i32, iov: u64, count: u64) -> SysResult {
        let vecs = Self::read_iovecs(m, iov, count)?;
        // Gather first so a short write on a later vector cannot be reported
        // as a failure of the whole call.
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let data = m.read(base, len as usize)?.to_vec();
            let n = self.fds.get(fd)?.write(&data)?;
            total += n;
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    fn sys_readv(&mut self, m: &mut GuestMem<'_>, fd: i32, iov: u64, count: u64) -> SysResult {
        let vecs = Self::read_iovecs(m, iov, count)?;
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let mut tmp = vec![0u8; len as usize];
            let n = self.fds.get(fd)?.read(&mut tmp)?;
            m.write(base, &tmp[..n as usize])?;
            total += n;
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    fn resolve_path(&self, m: &GuestMem<'_>, path: u64) -> Result<String, Errno> {
        let raw = m.read_cstr(path, 4096)?;
        String::from_utf8(raw).map_err(|_| Errno::EINVAL)
    }

    fn sys_openat(&mut self, m: &GuestMem<'_>, _dirfd: i32, path: u64, flags: u64) -> SysResult {
        let p = self.resolve_path(m, path)?;
        let f = fs::open_host(&p, flags)?;
        self.fds.insert(f)
    }

    fn sys_dup3(&mut self, old: i32, new: i32) -> SysResult {
        if old == new {
            return Err(Errno::EINVAL);
        }
        let _ = self.fds.close(new);
        self.fds.dup(old, new)
    }

    fn sys_fstat(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64) -> SysResult {
        let st = self.fds.get(fd)?.stat()?;
        m.write(buf, &st.encode())?;
        Ok(0)
    }

    fn sys_fstatat(
        &mut self,
        m: &mut GuestMem<'_>,
        _dirfd: i32,
        path: u64,
        buf: u64,
        flags: u64,
    ) -> SysResult {
        // AT_EMPTY_PATH with an empty name means "stat the descriptor".
        const AT_EMPTY_PATH: u64 = 0x1000;
        let p = self.resolve_path(m, path)?;
        if p.is_empty() && flags & AT_EMPTY_PATH != 0 {
            return self.sys_fstat(m, _dirfd, buf);
        }
        let md = std::fs::metadata(&p).map_err(|e| Errno::from_io(&e))?;
        let st = fs::Stat {
            ino: 1,
            mode: if md.is_dir() {
                mode::S_IFDIR | 0o755
            } else {
                mode::S_IFREG | 0o644
            },
            nlink: 1,
            size: md.len() as i64,
            blksize: 4096,
            blocks: (md.len() as i64 + 511) / 512,
            ..Default::default()
        };
        m.write(buf, &st.encode())?;
        Ok(0)
    }

    fn sys_fcntl(&mut self, fd: i32, cmd: u64, arg: u64) -> SysResult {
        const F_DUPFD: u64 = 0;
        const F_GETFD: u64 = 1;
        const F_SETFD: u64 = 2;
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => self.fds.dup(fd, arg as i32),
            F_GETFD => {
                self.fds.get(fd)?;
                Ok(0)
            }
            F_SETFD => {
                self.fds.get(fd)?;
                Ok(0)
            }
            F_GETFL => Ok(self.fds.get(fd)?.flags),
            F_SETFL => {
                self.fds.get(fd)?.flags = arg;
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn sys_ioctl(&mut self, m: &mut GuestMem<'_>, fd: i32, req: u64, arg: u64) -> SysResult {
        const TCGETS: u64 = 0x5401;
        const TIOCGWINSZ: u64 = 0x5413;
        let is_tty = self.fds.get(fd)?.is_tty();
        match req {
            TCGETS if is_tty => {
                // A plausible cooked-mode termios keeps libc's isatty and
                // line-buffering decisions correct.
                let mut termios = [0u8; 60];
                termios[0..4].copy_from_slice(&0x2502u32.to_le_bytes()); // c_iflag
                termios[4..8].copy_from_slice(&0x5u32.to_le_bytes()); // c_oflag
                termios[8..12].copy_from_slice(&0xbfu32.to_le_bytes()); // c_cflag
                termios[12..16].copy_from_slice(&0x8a3bu32.to_le_bytes()); // c_lflag
                m.write(arg, &termios)?;
                Ok(0)
            }
            TIOCGWINSZ if is_tty => {
                let mut ws = [0u8; 8];
                ws[0..2].copy_from_slice(&24u16.to_le_bytes());
                ws[2..4].copy_from_slice(&80u16.to_le_bytes());
                m.write(arg, &ws)?;
                Ok(0)
            }
            TCGETS | TIOCGWINSZ => Err(Errno::ENOTTY),
            _ => Err(Errno::ENOTTY),
        }
    }

    fn sys_faccessat(&mut self, m: &GuestMem<'_>, path: u64) -> SysResult {
        let p = self.resolve_path(m, path)?;
        if std::path::Path::new(&p).exists() {
            Ok(0)
        } else {
            Err(Errno::ENOENT)
        }
    }

    fn sys_readlinkat(
        &mut self,
        m: &mut GuestMem<'_>,
        path: u64,
        buf: u64,
        size: u64,
    ) -> SysResult {
        let p = self.resolve_path(m, path)?;
        // `/proc/self/exe` must answer from our own bookkeeping, not the
        // host's: the guest's notion of "this program" is the guest binary.
        let target = if p == "/proc/self/exe" {
            self.exe_path.clone()
        } else {
            std::fs::read_link(&p)
                .map_err(|e| Errno::from_io(&e))?
                .to_string_lossy()
                .into_owned()
        };
        let bytes = target.as_bytes();
        let n = bytes.len().min(size as usize);
        m.write(buf, &bytes[..n])?;
        Ok(n as u64)
    }

    fn sys_getcwd(&mut self, m: &mut GuestMem<'_>, buf: u64, size: u64) -> SysResult {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        let mut bytes = cwd.into_bytes();
        bytes.push(0);
        if bytes.len() as u64 > size {
            return Err(Errno::ERANGE);
        }
        m.write(buf, &bytes)?;
        Ok(bytes.len() as u64)
    }

    // -- memory ------------------------------------------------------------

    fn sys_mmap(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (addr, len, prot_bits, flags, fd, offset) = (a[0], a[1], a[2] as u32, a[3], a[4] as i32, a[5]);
        if flags & mflags::ANONYMOUS == 0 && fd >= 0 {
            // A private file mapping is an eager copy, which is exactly
            // correct and avoids needing page faults (PLAN §7.4).
            let len_aligned = crate::mem::page_align_up(len);
            let start = self.mem.mmap(addr, len_aligned, prot_bits, flags, "")?;
            let mut tmp = vec![0u8; len as usize];
            let f = self.fds.get(fd)?;
            f.seek(offset as i64, 0)?;
            let n = f.read(&mut tmp)? as usize;
            m.zero(start, len_aligned as usize)?;
            m.write(start, &tmp[..n])?;
            return Ok(start);
        }
        let start = self.mem.mmap(addr, len, prot_bits, flags, "")?;
        // Anonymous memory is defined to read as zero.
        m.zero(start, crate::mem::page_align_up(len) as usize)?;
        Ok(start)
    }

    // -- process and time --------------------------------------------------

    fn sys_uname(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        // Six 65-byte NUL-padded fields.
        let fields = [
            "Linux",
            "gex",
            "6.1.0-gex",
            "#1 SMP gex",
            "aarch64",
            "(none)",
        ];
        let mut out = vec![0u8; 65 * 6];
        for (i, f) in fields.iter().enumerate() {
            let b = f.as_bytes();
            out[i * 65..i * 65 + b.len()].copy_from_slice(b);
        }
        m.write(buf, &out)?;
        Ok(0)
    }

    fn sys_clock_gettime(&mut self, m: &mut GuestMem<'_>, _clk: u64, buf: u64) -> SysResult {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        m.write_u64(buf, now.as_secs())?;
        m.write_u64(buf + 8, now.subsec_nanos() as u64)?;
        Ok(0)
    }

    fn sys_gettimeofday(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf == 0 {
            return Ok(0);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        m.write_u64(buf, now.as_secs())?;
        m.write_u64(buf + 8, now.subsec_micros() as u64)?;
        Ok(0)
    }

    fn sys_getrandom(&mut self, m: &mut GuestMem<'_>, buf: u64, len: u64) -> SysResult {
        let len = len.min(1 << 20) as usize;
        let mut tmp = vec![0u8; len];
        let mut f = OpenFile {
            backend: fs::FileBackend::Random,
            flags: 0,
            readable: true,
            writable: false,
        };
        f.read(&mut tmp)?;
        m.write(buf, &tmp)?;
        Ok(len as u64)
    }

    fn sys_prlimit(&mut self, m: &mut GuestMem<'_>, new: u64, old: u64) -> SysResult {
        // Reporting limits is enough; changing them has nothing to act on.
        if old != 0 {
            m.write_u64(old, 8 * 1024 * 1024)?;
            m.write_u64(old + 8, u64::MAX)?;
        }
        let _ = new;
        Ok(0)
    }
}

/// Name a syscall for trace output.
pub fn syscall_name(n: u64) -> &'static str {
    match n {
        nr::GETCWD => "getcwd",
        nr::DUP => "dup",
        nr::DUP3 => "dup3",
        nr::FCNTL => "fcntl",
        nr::IOCTL => "ioctl",
        nr::FACCESSAT => "faccessat",
        nr::OPENAT => "openat",
        nr::CLOSE => "close",
        nr::LSEEK => "lseek",
        nr::READ => "read",
        nr::WRITE => "write",
        nr::READV => "readv",
        nr::WRITEV => "writev",
        nr::READLINKAT => "readlinkat",
        nr::NEWFSTATAT => "newfstatat",
        nr::FSTAT => "fstat",
        nr::EXIT => "exit",
        nr::EXIT_GROUP => "exit_group",
        nr::SET_TID_ADDRESS => "set_tid_address",
        nr::SET_ROBUST_LIST => "set_robust_list",
        nr::CLOCK_GETTIME => "clock_gettime",
        nr::SCHED_YIELD => "sched_yield",
        nr::RT_SIGACTION => "rt_sigaction",
        nr::RT_SIGPROCMASK => "rt_sigprocmask",
        nr::UNAME => "uname",
        nr::GETTIMEOFDAY => "gettimeofday",
        nr::GETPID => "getpid",
        nr::GETPPID => "getppid",
        nr::GETUID => "getuid",
        nr::GETEUID => "geteuid",
        nr::GETGID => "getgid",
        nr::GETEGID => "getegid",
        nr::GETTID => "gettid",
        nr::BRK => "brk",
        nr::MUNMAP => "munmap",
        nr::MMAP => "mmap",
        nr::MPROTECT => "mprotect",
        nr::MADVISE => "madvise",
        nr::PRLIMIT64 => "prlimit64",
        nr::GETRANDOM => "getrandom",
        nr::RSEQ => "rseq",
        _ => "unknown",
    }
}

/// Convenience re-export so hosts can reference protections symbolically.
pub use crate::mem::prot as mem_prot;
const _: () = {
    // Keep the unused-import checker honest about the protection constants
    // used through `mem_prot`.
    let _ = prot::READ;
    let _ = PAGE_SIZE;
    let _ = oflags::O_RDONLY;
};

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Vec<u8>, Kernel) {
        // A small fake address space: the context sits at 0x1000.
        let mem = vec![0u8; 0x20000];
        let k = Kernel::new(0x1000, 0x10000, "/prog".into());
        (mem, k)
    }

    fn set_args(m: &mut GuestMem<'_>, ctx_addr: u64, n: u64, args: &[u64]) {
        m.write_u64(ctx_addr + ctx::x(8) as u64, n).unwrap();
        for (i, v) in args.iter().enumerate() {
            m.write_u64(ctx_addr + ctx::x(i as u8) as u64, *v).unwrap();
        }
    }

    fn ret(m: &GuestMem<'_>, ctx_addr: u64) -> i64 {
        m.read_u64(ctx_addr + ctx::x(0) as u64).unwrap() as i64
    }

    #[test]
    fn exit_group_reports_the_status() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::EXIT_GROUP, &[7]);
        assert_eq!(k.syscall(&mut m, 0x400000), SyscallOutcome::Exit(7));
    }

    #[test]
    fn unknown_syscalls_return_enosys_and_are_recorded() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, 9999, &[]);
        assert_eq!(k.syscall(&mut m, 0), SyscallOutcome::Continue);
        assert_eq!(ret(&m, k.ctx_addr), -38);
        assert_eq!(k.unimplemented, vec![9999]);
    }

    #[test]
    fn brk_round_trips_through_the_syscall_path() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::BRK, &[0]);
        k.syscall(&mut m, 0);
        let base = ret(&m, k.ctx_addr) as u64;
        set_args(&mut m, k.ctx_addr, nr::BRK, &[base + 0x2000]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr) as u64, base + 0x2000);
    }

    #[test]
    fn uname_reports_aarch64() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::UNAME, &[0x8000]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 0);
        let machine = m.read(0x8000 + 65 * 4, 7).unwrap();
        assert_eq!(machine, b"aarch64");
    }

    #[test]
    fn write_to_a_bad_descriptor_fails() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[99, 0x8000, 4]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), -9);
    }

    #[test]
    fn faults_are_reported_rather_than_panicking() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        // A buffer past the end of the address space.
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[1, 0xffff_0000, 16]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), -14);
    }

    #[test]
    fn mmap_returns_zeroed_anonymous_memory() {
        let (mut buf, mut k) = setup();
        // Give the map a small address space to work with by pre-filling.
        for b in buf.iter_mut() {
            *b = 0xaa;
        }
        let mut m = GuestMem::new(&mut buf);
        // Force a fixed mapping inside the fake memory.
        set_args(
            &mut m,
            k.ctx_addr,
            nr::MMAP,
            &[
                0x8000,
                0x1000,
                (prot::READ | prot::WRITE) as u64,
                mflags::ANONYMOUS | mflags::PRIVATE | mflags::FIXED,
                u64::MAX,
                0,
            ],
        );
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr) as u64, 0x8000);
        assert!(m.read(0x8000, 0x1000).unwrap().iter().all(|b| *b == 0));
    }
}
