//! Syscall entry and the T0-T2 implementations (PLAN §8).
//!
//! `svc #0` arrives here with the guest context still in linear memory: the
//! number in `x8`, arguments in `x0`-`x5`. The kernel reads them from the
//! context, dispatches, and writes the result back to `x0` — the same shape
//! the wasm-compiled kernel will use in the browser.
//!
//! Everything filesystem-shaped goes through [`crate::vfs`], so the same
//! implementation serves a host passthrough, an in-memory root and the
//! synthetic `/proc` without the syscall layer knowing which is which.

use crate::errno::{encode, Errno, SysResult};
use crate::file::{FdTable, OpenFile, PipeEnd};
use crate::fs::{oflags, Stat};
use crate::mem::{flags as mflags, MemoryMap};
use crate::futex::{self, FutexTable};
use crate::pipe::Pipe;
use crate::signal::{self, Delivery, SigAction, SigAltStack, Signals};
use crate::stdio::{StdStream, Stream};
use crate::vfs::{FileType, ProcSource, Vfs};
use crate::GuestMem;
use gex_abi::{ctx, ctx_addr, EXCLUSIVE_SCRATCH};
use std::sync::{Arc, Mutex};

/// aarch64 syscall numbers (the asm-generic ABI).
pub mod nr {
    pub const GETCWD: u64 = 17;
    pub const DUP: u64 = 23;
    pub const DUP3: u64 = 24;
    pub const FCNTL: u64 = 25;
    pub const IOCTL: u64 = 29;
    pub const MKDIRAT: u64 = 34;
    pub const UNLINKAT: u64 = 35;
    pub const SYMLINKAT: u64 = 36;
    pub const LINKAT: u64 = 37;
    pub const RENAMEAT: u64 = 38;
    pub const FTRUNCATE: u64 = 46;
    pub const FACCESSAT: u64 = 48;
    pub const CHDIR: u64 = 49;
    pub const FCHMOD: u64 = 52;
    pub const FCHMODAT: u64 = 53;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const PIPE2: u64 = 59;
    pub const GETDENTS64: u64 = 61;
    pub const LSEEK: u64 = 62;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const PREAD64: u64 = 67;
    pub const PWRITE64: u64 = 68;
    pub const PPOLL: u64 = 73;
    pub const READLINKAT: u64 = 78;
    pub const NEWFSTATAT: u64 = 79;
    pub const FSTAT: u64 = 80;
    pub const FSYNC: u64 = 82;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const GET_ROBUST_LIST: u64 = 100;
    pub const NANOSLEEP: u64 = 101;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const CLOCK_GETRES: u64 = 114;
    pub const CLOCK_NANOSLEEP: u64 = 115;
    pub const SCHED_YIELD: u64 = 124;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGRETURN: u64 = 139;
    pub const SIGALTSTACK: u64 = 132;
    pub const TIMES: u64 = 153;
    pub const UNAME: u64 = 160;
    pub const GETRUSAGE: u64 = 165;
    pub const GETTIMEOFDAY: u64 = 169;
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETUID: u64 = 174;
    pub const GETEUID: u64 = 175;
    pub const GETGID: u64 = 176;
    pub const GETEGID: u64 = 177;
    pub const GETTID: u64 = 178;
    pub const SYSINFO: u64 = 179;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const CLONE: u64 = 220;
    pub const EXECVE: u64 = 221;
    pub const MMAP: u64 = 222;
    pub const MPROTECT: u64 = 226;
    pub const MADVISE: u64 = 233;
    pub const WAIT4: u64 = 260;
    pub const PRLIMIT64: u64 = 261;
    pub const GETRANDOM: u64 = 278;
    pub const RSEQ: u64 = 293;
    pub const FACCESSAT2: u64 = 439;
}

/// `*at` calls use this to mean "relative to the current directory".
const AT_FDCWD: i32 = -100;
const AT_EMPTY_PATH: u64 = 0x1000;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;

/// What the host should do after a syscall returns.
#[derive(Debug, Clone)]
pub enum SyscallOutcome {
    /// Resume the guest where it left off.
    Continue,
    /// Re-enter translated code at this address instead.
    ///
    /// Produced by `rt_sigreturn` and by signal delivery. Redirecting is safe
    /// at any host call because the guest's registers are already flushed to
    /// the context, so there is no wasm stack to unwind (PLAN §2.4).
    Resume(u64),
    /// The process is over.
    Exit(i32),
    /// A signal with no handler ended the process.
    Killed { signal: u32, core: bool },
    /// `clone` set up a new thread's context; the host must run it.
    ///
    /// The kernel cannot spawn a thread itself -- a wasmtime store belongs to
    /// one -- so it prepares the context in shared guest memory and hands the
    /// host what it needs.
    SpawnThread {
        ctx_addr: u64,
        tid: i32,
        /// `CLONE_CHILD_CLEARTID`: where the child must write zero and
        /// futex-wake when it exits. Zero when the flag was not set.
        clear_child_tid: u64,
    },
    /// This thread is finished; the process is not.
    ThreadExit,
    /// The syscall has to sleep, and must do so with the kernel unlocked.
    ///
    /// A blocking call that held the shared kernel would stop every other
    /// thread from making the very progress it is waiting for -- a futex wait
    /// could never be woken, because the waker needs the same lock to issue
    /// its wake. The host performs the operation, then hands the result back
    /// through [`Kernel::finish_blocking`].
    Blocking(BlockOp),
}

/// A syscall's blocking half, to be run without the kernel lock.
#[derive(Debug, Clone)]
pub enum BlockOp {
    FutexWait {
        table: Arc<FutexTable>,
        uaddr: u64,
        val: u32,
        deadline: Option<std::time::Duration>,
    },
    Sleep(std::time::Duration),
}

/// `clone` flags gex understands.
pub mod clone_flags {
    pub const VM: u64 = 0x0000_0100;
    pub const THREAD: u64 = 0x0001_0000;
    pub const SETTLS: u64 = 0x0008_0000;
    pub const PARENT_SETTID: u64 = 0x0010_0000;
    pub const CHILD_CLEARTID: u64 = 0x0020_0000;
    pub const CHILD_SETTID: u64 = 0x0100_0000;
}

/// One guest process's kernel state.
pub struct Kernel {
    pub fds: FdTable,
    pub mem: MemoryMap,
    pub vfs: Vfs,
    /// Address of the running thread's context struct.
    pub ctx_addr: u64,
    pub pid: i32,
    pub tid: i32,
    pub clear_child_tid: u64,
    /// Guest path of the running program.
    pub exe_path: String,
    /// The argument and environment vectors, for `/proc/self/cmdline`.
    pub argv: Vec<String>,
    pub envp: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    /// Trace every syscall to stderr (PLAN §11's strace mode).
    pub trace: bool,
    /// Signal dispositions, masks and pending set.
    pub signals: Signals,
    /// Futex wait queues. A single-threaded guest still uses these: musl's
    /// locks take the futex path whenever a lock is contended, and `EAGAIN`
    /// from an already-changed word is the answer it needs.
    pub futexes: Arc<FutexTable>,
    /// Syscalls seen but not implemented, for the ENOSYS telemetry in
    /// PLAN §14.
    pub unimplemented: Vec<u64>,
    /// Next thread id to hand out. Thread ids are process-wide, so this lives
    /// with the shared state rather than with a thread.
    pub next_tid: i32,
    /// Next free register-context slot. Slot 0 is the initial thread's.
    pub next_ctx_slot: u32,
    /// Threads that have been created and not yet exited, including this one.
    pub live_threads: u32,
}

/// A thread the kernel has prepared and the host must start.
#[derive(Clone, Copy, Debug)]
pub struct SpawnRequest {
    pub ctx_addr: u64,
    pub tid: i32,
    pub clear_child_tid: u64,
}

/// The parts of a kernel's state that belong to one thread, not the process.
///
/// [`Kernel`] is shared between every guest thread, so the fields describing
/// "the thread making this call" cannot live there permanently. They are
/// installed by [`Kernel::enter_thread`] under the same lock that guards the
/// syscall and read back by [`Kernel::leave_thread`], so what the kernel holds
/// always describes whichever thread currently owns it.
#[derive(Clone, Copy, Debug)]
pub struct ThreadState {
    /// Address of this thread's register context.
    pub ctx_addr: u64,
    pub tid: i32,
    /// Where to write zero and futex-wake when this thread exits, which is
    /// what `pthread_join` is waiting on.
    pub clear_child_tid: u64,
}

impl ThreadState {
    pub fn new(ctx_addr: u64, tid: i32) -> ThreadState {
        ThreadState {
            ctx_addr,
            tid,
            clear_child_tid: 0,
        }
    }
}

impl Kernel {
    /// Make the shared kernel describe `t` for the duration of a syscall.
    pub fn enter_thread(&mut self, t: &ThreadState) {
        self.ctx_addr = t.ctx_addr;
        self.tid = t.tid;
        self.clear_child_tid = t.clear_child_tid;
    }

    /// Copy back what the syscall changed about the calling thread.
    pub fn leave_thread(&self, t: &mut ThreadState) {
        t.clear_child_tid = self.clear_child_tid;
    }

    pub fn new(ctx_addr: u64, brk_start: u64, exe_path: String) -> Kernel {
        let mut fds = FdTable::new();
        // The three standard streams, reported as terminals when the host's
        // are, because that is what decides libc's buffering mode.
        let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let tty_out = std::io::IsTerminal::is_terminal(&std::io::stdout());
        for (stream, name, readable, writable, is_tty) in [
            (Stream::In, "/dev/stdin", true, false, tty),
            (Stream::Out, "/dev/stdout", false, true, tty_out),
            (Stream::Err, "/dev/stderr", false, true, tty_out),
        ] {
            let handle = Arc::new(StdStream::new(stream, is_tty));
            let _ = fds.insert(
                OpenFile::new(handle, name.to_string(), 0, readable, writable),
                false,
            );
        }
        Kernel {
            fds,
            mem: MemoryMap::new(brk_start),
            vfs: Vfs::new(),
            ctx_addr,
            pid: 1,
            tid: 1,
            clear_child_tid: 0,
            exe_path,
            argv: Vec::new(),
            envp: Vec::new(),
            uid: 1000,
            gid: 1000,
            signals: Signals::new(),
            futexes: Arc::new(FutexTable::new()),
            trace: false,
            unimplemented: Vec::new(),
            next_tid: 2,
            next_ctx_slot: 1,
            live_threads: 1,
        }
    }

    /// `clone`, for the `CLONE_THREAD` case that `pthread_create` uses.
    ///
    /// The child's context is built in shared guest memory from the parent's,
    /// so it inherits everything the ABI does not say otherwise about, and
    /// then differs in exactly what the architecture and the flags require:
    /// `x0` is zero because that is what the child's `clone` returns, `sp` and
    /// the thread pointer come from the caller, and the exclusive monitor
    /// starts clear because a monitor is per-thread and the parent's says
    /// nothing about the child's.
    ///
    /// A `clone` without `CLONE_THREAD` is `fork`, which gex does not
    /// implement -- one address space, one process.
    fn sys_clone(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> Result<SpawnRequest, Errno> {
        let (flags, child_stack, parent_tid, tls, child_tid) = (a[0], a[1], a[2], a[3], a[4]);
        if flags & clone_flags::THREAD == 0 || flags & clone_flags::VM == 0 {
            return Err(Errno::ENOSYS);
        }
        if child_stack == 0 {
            return Err(Errno::EINVAL);
        }

        let tid = self.next_tid;
        self.next_tid += 1;
        let slot = self.next_ctx_slot;
        self.next_ctx_slot += 1;
        let child = ctx_addr(slot);
        // Contexts live in the reserved region below the guest's own memory;
        // running out of slots is a hard limit, not something to paper over.
        if child + ctx::SIZE as u64 >= EXCLUSIVE_SCRATCH {
            return Err(Errno::EAGAIN);
        }

        let parent = self.ctx_addr;
        for off in (0..ctx::SIZE).step_by(8) {
            let v = m.read_u64(parent + off as u64)?;
            m.write_u64(child + off as u64, v)?;
        }
        m.write_u64(child + ctx::x(0) as u64, 0)?;
        m.write_u64(child + ctx::SP as u64, child_stack)
            ?;
        m.write_u64(child + ctx::MON_SIZE as u64, 0)
            ?;
        m.write_u64(child + ctx::TID as u64, tid as u64)
            ?;
        if flags & clone_flags::SETTLS != 0 {
            m.write_u64(child + ctx::TPIDR as u64, tls)
                ?;
        }
        if flags & clone_flags::PARENT_SETTID != 0 && parent_tid != 0 {
            m.write_u32(parent_tid, tid as u32)?;
        }
        if flags & clone_flags::CHILD_SETTID != 0 && child_tid != 0 {
            m.write_u32(child_tid, tid as u32)?;
        }
        self.live_threads += 1;
        Ok(SpawnRequest {
            ctx_addr: child,
            tid,
            clear_child_tid: if flags & clone_flags::CHILD_CLEARTID != 0 {
                child_tid
            } else {
                0
            },
        })
    }

    /// What a thread must do on its way out: release whatever is waiting on
    /// it. `pthread_join` blocks on exactly this futex.
    fn thread_exit(&mut self, m: &mut GuestMem<'_>) {
        self.live_threads = self.live_threads.saturating_sub(1);
        let addr = self.clear_child_tid;
        if addr != 0 {
            let _ = m.write_u32(addr, 0);
            self.futexes.wake(addr, u32::MAX);
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

        // Returning from a handler restores a whole context, so it cannot go
        // through the normal "write x0 and continue" path.
        if n == nr::RT_SIGRETURN {
            return match self.sigreturn(m) {
                Ok(pc) => {
                    if self.trace {
                        eprintln!("[gex] rt_sigreturn -> 0x{pc:x}");
                    }
                    SyscallOutcome::Resume(pc)
                }
                Err(_) => SyscallOutcome::Killed {
                    signal: signal::sig::SIGSEGV,
                    core: true,
                },
            };
        }

        // `clone` and a thread's `exit` change which threads exist, so they
        // answer with an outcome rather than a return value.
        if n == nr::CLONE {
            return match self.sys_clone(m, a) {
                Ok(req) => {
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, req.tid as u64);
                    if self.trace {
                        eprintln!("[gex] clone(...) = {} (new thread)", req.tid);
                    }
                    SyscallOutcome::SpawnThread {
                        ctx_addr: req.ctx_addr,
                        tid: req.tid,
                        clear_child_tid: req.clear_child_tid,
                    }
                }
                Err(e) => {
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, e.as_return());
                    if self.trace {
                        eprintln!("[gex] clone(...) = {}", -(e.0 as i64));
                    }
                    SyscallOutcome::Continue
                }
            };
        }
        // `exit` ends one thread; only `exit_group` ends the process. For the
        // last thread the two are the same thing.
        if n == nr::EXIT && self.live_threads > 1 {
            if self.trace {
                eprintln!("[gex] exit({}) (thread {})", a[0] as i32, self.tid);
            }
            self.thread_exit(m);
            return SyscallOutcome::ThreadExit;
        }

        // A syscall that sleeps must do so with the kernel unlocked, or the
        // thread that would wake it can never get in.
        if let Some(op) = self.blocking_op(m, n, a) {
            if self.trace {
                eprintln!("[gex] {} blocking", syscall_name(n));
            }
            return SyscallOutcome::Blocking(op);
        }

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

        // A syscall boundary is a safepoint: the context is complete, so a
        // pending signal can be delivered precisely here.
        match self.deliver_pending(m) {
            Delivery::None => SyscallOutcome::Continue,
            Delivery::Resume(pc) => SyscallOutcome::Resume(pc),
            Delivery::Terminate { signal, core } => SyscallOutcome::Killed { signal, core },
        }
    }

    /// The blocking half of a syscall, if it has one.
    ///
    /// Recognised before dispatch, because these are exactly the calls whose
    /// whole purpose is to wait for another thread.
    fn blocking_op(&mut self, m: &GuestMem<'_>, n: u64, a: [u64; 6]) -> Option<BlockOp> {
        match n {
            nr::FUTEX => {
                let operation =
                    (a[1] as u32) & !(futex::op::PRIVATE_FLAG | futex::op::CLOCK_REALTIME);
                if operation != futex::op::WAIT && operation != futex::op::WAIT_BITSET {
                    return None;
                }
                let deadline = if a[3] == 0 {
                    None
                } else {
                    let secs = m.read_u64(a[3]).ok()?;
                    let nanos = m.read_u64(a[3] + 8).ok()?;
                    Some(std::time::Duration::new(
                        secs.min(3600),
                        (nanos % 1_000_000_000) as u32,
                    ))
                };
                Some(BlockOp::FutexWait {
                    table: Arc::clone(&self.futexes),
                    uaddr: a[0],
                    val: a[2] as u32,
                    deadline,
                })
            }
            nr::NANOSLEEP | nr::CLOCK_NANOSLEEP => {
                // `clock_nanosleep` puts the request in a[2]; `nanosleep` in
                // a[0]. Neither clock nor the absolute-time flag is modelled.
                let req = if n == nr::NANOSLEEP { a[0] } else { a[2] };
                if req == 0 {
                    return None;
                }
                let secs = m.read_u64(req).ok()?;
                let nanos = m.read_u64(req + 8).ok()?;
                Some(BlockOp::Sleep(std::time::Duration::new(
                    secs.min(60),
                    (nanos % 1_000_000_000) as u32,
                )))
            }
            _ => None,
        }
    }

    /// Finish a syscall whose blocking half the host has now run.
    pub fn finish_blocking(&mut self, m: &mut GuestMem<'_>, r: SysResult) -> SyscallOutcome {
        let ret = encode(r);
        let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, ret);
        match self.deliver_pending(m) {
            Delivery::None => SyscallOutcome::Continue,
            Delivery::Resume(pc) => SyscallOutcome::Resume(pc),
            Delivery::Terminate { signal, core } => SyscallOutcome::Killed { signal, core },
        }
    }

    /// The dispatch table. `Err(code)` means the process is exiting.
    fn dispatch(&mut self, m: &mut GuestMem<'_>, n: u64, a: [u64; 6]) -> Result<SysResult, i32> {
        let r: SysResult = match n {
            nr::EXIT | nr::EXIT_GROUP => return Err((a[0] & 0xff) as i32),

            // -- I/O ---------------------------------------------------------
            nr::WRITE => self.sys_write(m, a[0] as i32, a[1], a[2]),
            nr::READ => self.sys_read(m, a[0] as i32, a[1], a[2]),
            nr::WRITEV => self.sys_writev(m, a[0] as i32, a[1], a[2]),
            nr::READV => self.sys_readv(m, a[0] as i32, a[1], a[2]),
            nr::PREAD64 => self.sys_pread(m, a[0] as i32, a[1], a[2], a[3]),
            nr::PWRITE64 => self.sys_pwrite(m, a[0] as i32, a[1], a[2], a[3]),
            nr::OPENAT => self.sys_openat(m, a[0] as i32, a[1], a[2], a[3] as u32),
            nr::CLOSE => self.fds.close(a[0] as i32).map(|_| 0),
            nr::LSEEK => self
                .fds
                .get(a[0] as i32)
                .and_then(|f| f.seek(a[1] as i64, a[2] as u32)),
            nr::DUP => self.sys_dup(a[0] as i32),
            nr::DUP3 => self.sys_dup3(a[0] as i32, a[1] as i32, a[2]),
            nr::PIPE2 => self.sys_pipe2(m, a[0], a[1]),
            nr::GETDENTS64 => self.sys_getdents(m, a[0] as i32, a[1], a[2]),
            nr::FSTAT => self.sys_fstat(m, a[0] as i32, a[1]),
            nr::NEWFSTATAT => self.sys_fstatat(m, a[0] as i32, a[1], a[2], a[3]),
            nr::FCNTL => self.sys_fcntl(a[0] as i32, a[1], a[2]),
            nr::IOCTL => self.sys_ioctl(m, a[0] as i32, a[1], a[2]),
            nr::FTRUNCATE => self
                .fds
                .get(a[0] as i32)
                .and_then(|f| f.handle.truncate(a[1]).map(|_| 0)),
            nr::FSYNC => self.fds.get(a[0] as i32).map(|_| 0),
            nr::FACCESSAT | nr::FACCESSAT2 => self.sys_faccessat(m, a[0] as i32, a[1]),
            nr::READLINKAT => self.sys_readlinkat(m, a[0] as i32, a[1], a[2], a[3]),
            nr::GETCWD => self.sys_getcwd(m, a[0], a[1]),
            nr::CHDIR => self.sys_chdir(m, a[0]),
            nr::MKDIRAT => self.sys_mkdirat(m, a[0] as i32, a[1], a[2] as u32),
            nr::UNLINKAT => self.sys_unlinkat(m, a[0] as i32, a[1], a[2]),
            nr::RENAMEAT => self.sys_renameat(m, a[0] as i32, a[1], a[2] as i32, a[3]),
            nr::SYMLINKAT => self.sys_symlinkat(m, a[0], a[2]),
            nr::PPOLL => self.sys_ppoll(m, a[0], a[1]),
            // Permission bits are not modelled; reporting success keeps
            // installers and build systems moving.
            nr::FCHMOD | nr::FCHMODAT => Ok(0),

            // -- memory ------------------------------------------------------
            nr::BRK => Ok(self.mem.brk(a[0])),
            nr::MMAP => self.sys_mmap(m, a),
            nr::MUNMAP => self.mem.unmap(a[0], a[1]).map(|_| 0),
            nr::MPROTECT => self.mem.protect(a[0], a[1], (a[2] & 0x7) as u32).map(|_| 0),
            nr::MADVISE => Ok(0),

            // -- process and time --------------------------------------------
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
            nr::CLOCK_GETRES => self.sys_clock_getres(m, a[1]),
            nr::GETTIMEOFDAY => self.sys_gettimeofday(m, a[0]),
            nr::NANOSLEEP => self.sys_nanosleep(m, a[0]),
            nr::CLOCK_NANOSLEEP => self.sys_nanosleep(m, a[2]),
            nr::GETRANDOM => self.sys_getrandom(m, a[0], a[1]),
            nr::SCHED_YIELD => {
                std::thread::yield_now();
                Ok(0)
            }
            nr::TIMES => self.sys_times(m, a[0]),
            nr::GETRUSAGE => self.sys_getrusage(m, a[1]),
            nr::SYSINFO => self.sys_sysinfo(m, a[0]),
            nr::PRLIMIT64 => self.sys_prlimit(m, a[2], a[3]),

            // -- signals -----------------------------------------------------
            nr::RT_SIGACTION => self.sys_rt_sigaction(m, a[0] as u32, a[1], a[2]),
            nr::RT_SIGPROCMASK => self.sys_rt_sigprocmask(m, a[0] as u32, a[1], a[2]),
            nr::SIGALTSTACK => self.sys_sigaltstack(m, a[0], a[1]),
            // One process and one thread for now, so any target is us.
            // `raise` goes through tkill, which is why it needs its own entry
            // rather than falling into the unimplemented bucket.
            nr::KILL | nr::TKILL | nr::TGKILL => {
                let signo = match n {
                    nr::KILL | nr::TKILL => a[1],
                    _ => a[2],
                } as u32;
                if signo > 0 {
                    self.signals.raise(signo);
                }
                Ok(0)
            }
            nr::RT_SIGRETURN => {
                // Handled by the caller, which must redirect execution to the
                // restored PC rather than returning here.
                Ok(0)
            }

            nr::SET_ROBUST_LIST | nr::GET_ROBUST_LIST => Ok(0),
            nr::FUTEX => self.sys_futex(m, a),
            // Restartable sequences are an optimisation the guest can live
            // without, and both libcs treat ENOSYS as "not available".
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

    fn sys_futex(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (uaddr, raw_op, val, timeout) = (a[0], a[1] as u32, a[2] as u32, a[3]);
        // The private and clock bits change nothing here: gex has one address
        // space, and the timeout is treated as relative either way.
        let operation = raw_op & !(futex::op::PRIVATE_FLAG | futex::op::CLOCK_REALTIME);
        match operation {
            // The waiting forms never reach here: `blocking_op` takes them
            // before dispatch so the host can run them unlocked.
            futex::op::WAIT | futex::op::WAIT_BITSET => Err(Errno::EINVAL),
            futex::op::WAKE | futex::op::WAKE_BITSET => Ok(self.futexes.wake(uaddr, val)),
            // Requeue is an optimisation for condition variables; waking
            // everyone is correct, if less efficient.
            futex::op::REQUEUE | futex::op::CMP_REQUEUE => Ok(self.futexes.wake(uaddr, u32::MAX)),
            _ => Err(Errno::ENOSYS),
        }
    }

    // -- signals -------------------------------------------------------------

    fn sys_rt_sigaction(
        &mut self,
        m: &mut GuestMem<'_>,
        signo: u32,
        act: u64,
        old: u64,
    ) -> SysResult {
        if signo == 0 || signo > signal::sig::MAX {
            return Err(Errno::EINVAL);
        }
        if old != 0 {
            let a = self.signals.action(signo);
            m.write_u64(old, a.handler)?;
            m.write_u64(old + 8, a.flags)?;
            m.write_u64(old + 16, a.restorer)?;
            m.write_u64(old + 24, a.mask)?;
        }
        if act != 0 {
            let a = SigAction {
                handler: m.read_u64(act)?,
                flags: m.read_u64(act + 8)?,
                restorer: m.read_u64(act + 16)?,
                mask: m.read_u64(act + 24)?,
            };
            self.signals.set_action(signo, a)?;
        }
        Ok(0)
    }

    fn sys_rt_sigprocmask(
        &mut self,
        m: &mut GuestMem<'_>,
        how: u32,
        set: u64,
        old: u64,
    ) -> SysResult {
        const SIG_BLOCK: u32 = 0;
        const SIG_UNBLOCK: u32 = 1;
        const SIG_SETMASK: u32 = 2;
        if old != 0 {
            m.write_u64(old, self.signals.blocked)?;
        }
        if set != 0 {
            let mask = m.read_u64(set)?;
            self.signals.blocked = match how {
                SIG_BLOCK => self.signals.blocked | mask,
                SIG_UNBLOCK => self.signals.blocked & !mask,
                SIG_SETMASK => mask,
                _ => return Err(Errno::EINVAL),
            };
            // Neither of these can ever be blocked.
            let unblockable =
                (1u64 << (signal::sig::SIGKILL - 1)) | (1u64 << (signal::sig::SIGSTOP - 1));
            self.signals.blocked &= !unblockable;
        }
        Ok(0)
    }

    fn sys_sigaltstack(&mut self, m: &mut GuestMem<'_>, new: u64, old: u64) -> SysResult {
        if old != 0 {
            let a = self.signals.alt_stack;
            m.write_u64(old, a.sp)?;
            m.write_u32(old + 8, a.flags)?;
            m.write_u64(old + 16, a.size)?;
        }
        if new != 0 {
            self.signals.alt_stack = SigAltStack {
                sp: m.read_u64(new)?,
                flags: m.read_u32(new + 8)?,
                size: m.read_u64(new + 16)?,
            };
        }
        Ok(0)
    }

    /// Deliver a pending signal, if any is deliverable right now.
    ///
    /// Called after every syscall and at safepoints. A `Resume` result means
    /// the caller must re-enter translated code at the returned address
    /// rather than continuing where it was.
    pub fn deliver_pending(&mut self, m: &mut GuestMem<'_>) -> Delivery {
        if !self.signals.has_pending() {
            return Delivery::None;
        }
        signal::dispatch(&mut self.signals, m, self.ctx_addr).unwrap_or(Delivery::None)
    }

    /// Raise a synchronous fault against the running instruction.
    ///
    /// Returns where to resume if a handler took it. Unlike an asynchronous
    /// signal these cannot simply be discarded: a program with no handler for
    /// its own SIGSEGV must die.
    pub fn raise_fault(
        &mut self,
        m: &mut GuestMem<'_>,
        signo: u32,
        pc: u64,
        fault_address: u64,
    ) -> Delivery {
        let action = self.signals.action(signo);
        let blocked = self.signals.blocked & (1u64 << (signo - 1)) != 0;
        // A synchronous fault that is blocked or ignored is fatal, because
        // returning would re-execute the faulting instruction forever.
        if action.handler == signal::SIG_DFL || action.handler == signal::SIG_IGN || blocked {
            return Delivery::Terminate {
                signal: signo,
                core: matches!(
                    signal::default_action(signo),
                    signal::DefaultAction::Core
                ),
            };
        }
        if m.write_u64(self.ctx_addr + ctx::PC as u64, pc).is_err() {
            return Delivery::Terminate {
                signal: signo,
                core: true,
            };
        }
        match signal::setup_frame(&mut self.signals, m, self.ctx_addr, signo, fault_address) {
            Ok(resume) => Delivery::Resume(resume),
            Err(_) => Delivery::Terminate {
                signal: signo,
                core: true,
            },
        }
    }

    /// Restore the context a handler frame captured.
    pub fn sigreturn(&mut self, m: &mut GuestMem<'_>) -> Result<u64, Errno> {
        signal::sigreturn(&mut self.signals, m, self.ctx_addr)
    }

    // -- path helpers --------------------------------------------------------

    /// Resolve a `*at`-style `(dirfd, path)` pair to an absolute guest path.
    fn at_path(&self, m: &GuestMem<'_>, dirfd: i32, path: u64) -> Result<String, Errno> {
        let raw = m.read_cstr(path, 4096)?;
        let p = String::from_utf8(raw).map_err(|_| Errno::EINVAL)?;
        if p.starts_with('/') || dirfd == AT_FDCWD {
            return Ok(self.vfs.absolute(&p));
        }
        // Relative to an open directory: its recorded path is the base.
        let base = self.fds.get(dirfd)?.path.clone();
        Ok(crate::vfs::normalize(&p, &base))
    }

    // -- I/O -----------------------------------------------------------------

    fn sys_write(&mut self, m: &GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        let data = m.read(buf, len as usize)?.to_vec();
        self.fds.get(fd)?.write(&data)
    }

    fn sys_read(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        let len = len.min(1 << 24) as usize;
        let mut tmp = vec![0u8; len];
        let n = self.fds.get(fd)?.read(&mut tmp)?;
        m.write(buf, &tmp[..n as usize])?;
        Ok(n)
    }

    fn sys_pread(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        buf: u64,
        len: u64,
        off: u64,
    ) -> SysResult {
        let len = len.min(1 << 24) as usize;
        let mut tmp = vec![0u8; len];
        let n = self.fds.get(fd)?.pread(off, &mut tmp)?;
        m.write(buf, &tmp[..n as usize])?;
        Ok(n)
    }

    fn sys_pwrite(
        &mut self,
        m: &GuestMem<'_>,
        fd: i32,
        buf: u64,
        len: u64,
        off: u64,
    ) -> SysResult {
        let data = m.read(buf, len as usize)?.to_vec();
        self.fds.get(fd)?.pwrite(off, &data)
    }

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
        let file = self.fds.get(fd)?;
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let data = m.read(base, len as usize)?.to_vec();
            let n = file.write(&data)?;
            total += n;
            // A short write ends the call; the caller retries the remainder.
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    fn sys_readv(&mut self, m: &mut GuestMem<'_>, fd: i32, iov: u64, count: u64) -> SysResult {
        let vecs = Self::read_iovecs(m, iov, count)?;
        let file = self.fds.get(fd)?;
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let mut tmp = vec![0u8; len as usize];
            let n = file.read(&mut tmp)?;
            m.write(base, &tmp[..n as usize])?;
            total += n;
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    fn sys_openat(
        &mut self,
        m: &GuestMem<'_>,
        dirfd: i32,
        path: u64,
        flags: u64,
        mode: u32,
    ) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        let handle = self.vfs.open(&p, flags, mode)?;
        let acc = flags & oflags::ACCMODE;
        let readable = acc == oflags::O_RDONLY || acc == oflags::O_RDWR;
        let writable = acc == oflags::O_WRONLY || acc == oflags::O_RDWR;
        let file = OpenFile::new(handle, p, flags, readable, writable);
        self.fds
            .insert(file, flags & oflags::O_CLOEXEC != 0)
    }

    fn sys_dup(&mut self, fd: i32) -> SysResult {
        let file = self.fds.get(fd)?;
        self.fds.insert(file, false)
    }

    fn sys_dup3(&mut self, old: i32, new: i32, flags: u64) -> SysResult {
        if old == new {
            return Err(Errno::EINVAL);
        }
        let file = self.fds.get(old)?;
        self.fds
            .install(new, file, flags & oflags::O_CLOEXEC != 0)
    }

    fn sys_pipe2(&mut self, m: &mut GuestMem<'_>, fds: u64, flags: u64) -> SysResult {
        let pipe = Arc::new(Pipe::new());
        pipe.set_nonblocking(flags);
        let cloexec = flags & oflags::O_CLOEXEC != 0;
        let read_end = OpenFile::new(
            Arc::new(PipeEnd {
                pipe: Arc::clone(&pipe),
                write: false,
            }),
            "pipe:[0]".to_string(),
            flags,
            true,
            false,
        );
        let write_end = OpenFile::new(
            Arc::new(PipeEnd {
                pipe: Arc::clone(&pipe),
                write: true,
            }),
            "pipe:[0]".to_string(),
            flags,
            false,
            true,
        );
        // Both ends start with one reference each, which `Pipe::new` already
        // accounted for.
        let rfd = self.fds.insert(read_end, cloexec)?;
        let wfd = self.fds.insert(write_end, cloexec)?;
        m.write_u32(fds, rfd as u32)?;
        m.write_u32(fds + 4, wfd as u32)?;
        Ok(0)
    }

    fn sys_getdents(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        let file = self.fds.get(fd)?;
        let bytes = file.read_dirents(len as usize)?;
        m.write(buf, &bytes)?;
        Ok(bytes.len() as u64)
    }

    fn stat_from(md: &crate::vfs::Metadata) -> Stat {
        Stat {
            ino: md.ino,
            mode: md.file_type.mode_bits() | md.mode,
            nlink: 1,
            size: md.size as i64,
            blksize: 4096,
            blocks: (md.size as i64 + 511) / 512,
            mtime: md.mtime,
            rdev: if md.file_type == FileType::CharDevice {
                0x8800
            } else {
                0
            },
            ..Default::default()
        }
    }

    fn sys_fstat(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64) -> SysResult {
        let md = self.fds.get(fd)?.metadata()?;
        m.write(buf, &Self::stat_from(&md).encode())?;
        Ok(0)
    }

    fn sys_fstatat(
        &mut self,
        m: &mut GuestMem<'_>,
        dirfd: i32,
        path: u64,
        buf: u64,
        flags: u64,
    ) -> SysResult {
        let raw = m.read_cstr(path, 4096)?;
        if raw.is_empty() && flags & AT_EMPTY_PATH != 0 {
            return self.sys_fstat(m, dirfd, buf);
        }
        let p = self.at_path(m, dirfd, path)?;
        let md = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            self.vfs.metadata_nofollow(&p)?
        } else {
            self.vfs.metadata(&p)?
        };
        m.write(buf, &Self::stat_from(&md).encode())?;
        Ok(0)
    }

    fn sys_fcntl(&mut self, fd: i32, cmd: u64, arg: u64) -> SysResult {
        const F_DUPFD: u64 = 0;
        const F_GETFD: u64 = 1;
        const F_SETFD: u64 = 2;
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const F_SETLK: u64 = 6;
        const F_SETLKW: u64 = 7;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let file = self.fds.get(fd)?;
                self.fds
                    .insert_at(file, arg as i32, cmd == F_DUPFD_CLOEXEC)
            }
            F_GETFD => Ok(self.fds.cloexec(fd)? as u64),
            F_SETFD => {
                self.fds.set_cloexec(fd, arg & 1 != 0)?;
                Ok(0)
            }
            F_GETFL => Ok(self.fds.get(fd)?.flags()),
            F_SETFL => {
                self.fds.get(fd)?.set_flags(arg);
                Ok(0)
            }
            // Advisory locks always succeed: gex runs one process tree over
            // one namespace, so there is nobody to contend with.
            F_SETLK | F_SETLKW => {
                self.fds.get(fd)?;
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn sys_ioctl(&mut self, m: &mut GuestMem<'_>, fd: i32, req: u64, arg: u64) -> SysResult {
        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TIOCGWINSZ: u64 = 0x5413;
        const FIONREAD: u64 = 0x541b;
        const TIOCGPGRP: u64 = 0x540f;
        let file = self.fds.get(fd)?;
        let is_tty = file.is_tty();
        match req {
            TCGETS if is_tty => {
                // A plausible cooked-mode termios keeps isatty and the
                // line-buffering decision correct.
                let mut t = [0u8; 60];
                t[0..4].copy_from_slice(&0x2502u32.to_le_bytes()); // c_iflag
                t[4..8].copy_from_slice(&0x5u32.to_le_bytes()); // c_oflag
                t[8..12].copy_from_slice(&0xbfu32.to_le_bytes()); // c_cflag
                t[12..16].copy_from_slice(&0x8a3bu32.to_le_bytes()); // c_lflag
                m.write(arg, &t)?;
                Ok(0)
            }
            TCSETS if is_tty => Ok(0),
            TIOCGWINSZ if is_tty => {
                let mut ws = [0u8; 8];
                ws[0..2].copy_from_slice(&24u16.to_le_bytes());
                ws[2..4].copy_from_slice(&80u16.to_le_bytes());
                m.write(arg, &ws)?;
                Ok(0)
            }
            TIOCGPGRP if is_tty => {
                m.write_u32(arg, self.pid as u32)?;
                Ok(0)
            }
            FIONREAD => {
                let md = file.metadata()?;
                let remaining = md.size.saturating_sub(file.offset());
                m.write_u32(arg, remaining as u32)?;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn sys_faccessat(&mut self, m: &GuestMem<'_>, dirfd: i32, path: u64) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        self.vfs.metadata(&p).map(|_| 0)
    }

    fn sys_readlinkat(
        &mut self,
        m: &mut GuestMem<'_>,
        dirfd: i32,
        path: u64,
        buf: u64,
        size: u64,
    ) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        let target = self.vfs.read_link(&p)?;
        let bytes = target.as_bytes();
        let n = bytes.len().min(size as usize);
        m.write(buf, &bytes[..n])?;
        Ok(n as u64)
    }

    fn sys_getcwd(&mut self, m: &mut GuestMem<'_>, buf: u64, size: u64) -> SysResult {
        let mut bytes = self.vfs.cwd.clone().into_bytes();
        bytes.push(0);
        if bytes.len() as u64 > size {
            return Err(Errno::ERANGE);
        }
        m.write(buf, &bytes)?;
        Ok(bytes.len() as u64)
    }

    fn sys_chdir(&mut self, m: &GuestMem<'_>, path: u64) -> SysResult {
        let raw = m.read_cstr(path, 4096)?;
        let p = String::from_utf8(raw).map_err(|_| Errno::EINVAL)?;
        self.vfs.set_cwd(&p).map(|_| 0)
    }

    fn sys_mkdirat(&mut self, m: &GuestMem<'_>, dirfd: i32, path: u64, mode: u32) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        self.vfs.mkdir(&p, mode).map(|_| 0)
    }

    fn sys_unlinkat(&mut self, m: &GuestMem<'_>, dirfd: i32, path: u64, flags: u64) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        if flags & AT_REMOVEDIR != 0 {
            self.vfs.rmdir(&p).map(|_| 0)
        } else {
            self.vfs.unlink(&p).map(|_| 0)
        }
    }

    fn sys_renameat(
        &mut self,
        m: &GuestMem<'_>,
        old_fd: i32,
        old: u64,
        new_fd: i32,
        new: u64,
    ) -> SysResult {
        let from = self.at_path(m, old_fd, old)?;
        let to = self.at_path(m, new_fd, new)?;
        self.vfs.rename(&from, &to).map(|_| 0)
    }

    fn sys_symlinkat(&mut self, m: &GuestMem<'_>, target: u64, path: u64) -> SysResult {
        let t = String::from_utf8(m.read_cstr(target, 4096)?).map_err(|_| Errno::EINVAL)?;
        let p = self.at_path(m, AT_FDCWD, path)?;
        self.vfs.symlink(&t, &p).map(|_| 0)
    }

    /// `ppoll` over the descriptors gex can answer for.
    ///
    /// Regular files are always ready, which is what Linux reports; pipes
    /// answer from their buffer state. There is no waiting yet, so a poll
    /// that would block returns zero ready descriptors after the timeout is
    /// checked, which callers handle as a spurious wakeup.
    fn sys_ppoll(&mut self, m: &mut GuestMem<'_>, fds: u64, nfds: u64) -> SysResult {
        const POLLIN: u16 = 0x001;
        const POLLOUT: u16 = 0x004;
        const POLLNVAL: u16 = 0x020;
        let mut ready = 0u64;
        for i in 0..nfds.min(1024) {
            let base = fds + i * 8;
            let fd = m.read_u32(base)? as i32;
            let events = (m.read_u32(base + 4)? & 0xffff) as u16;
            let revents = match self.fds.get(fd) {
                Err(_) => POLLNVAL,
                Ok(file) => {
                    let mut r = 0u16;
                    if events & POLLIN != 0 && file.readable {
                        r |= POLLIN;
                    }
                    if events & POLLOUT != 0 && file.writable {
                        r |= POLLOUT;
                    }
                    r
                }
            };
            if revents != 0 {
                ready += 1;
            }
            m.write_u32(base + 4, ((revents as u32) << 16) | events as u32)?;
        }
        Ok(ready)
    }

    // -- memory --------------------------------------------------------------

    fn sys_mmap(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (addr, len, prot_bits, flags, fd, offset) =
            (a[0], a[1], a[2] as u32, a[3], a[4] as i32, a[5]);
        if flags & mflags::ANONYMOUS == 0 && fd >= 0 {
            // A private file mapping is an eager copy, which is exactly
            // correct and needs no page faults (PLAN §7.4).
            let len_aligned = crate::mem::page_align_up(len);
            let file = self.fds.get(fd)?;
            let name = file.path.clone();
            let start = self.mem.mmap(addr, len_aligned, prot_bits, flags, &name)?;
            let mut tmp = vec![0u8; len as usize];
            let n = file.pread(offset, &mut tmp)? as usize;
            m.zero(start, len_aligned as usize)?;
            m.write(start, &tmp[..n])?;
            return Ok(start);
        }
        let start = self.mem.mmap(addr, len, prot_bits, flags, "")?;
        // Anonymous memory is defined to read as zero.
        m.zero(start, crate::mem::page_align_up(len) as usize)?;
        Ok(start)
    }

    // -- process and time ----------------------------------------------------

    fn sys_uname(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
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

    fn sys_clock_getres(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf == 0 {
            return Ok(0);
        }
        m.write_u64(buf, 0)?;
        m.write_u64(buf + 8, 1)?;
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

    fn sys_nanosleep(&mut self, m: &GuestMem<'_>, req: u64) -> SysResult {
        if req == 0 {
            return Ok(0);
        }
        let secs = m.read_u64(req)?;
        let nanos = m.read_u64(req + 8)?;
        // A cap keeps a guest from parking the runtime indefinitely before
        // signals exist to interrupt it.
        let _ = (secs, nanos);
        // The sleep itself is `BlockOp::Sleep`, run by the host with the
        // kernel unlocked; reaching here means a zero request.
        Ok(0)
    }

    fn sys_times(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf != 0 {
            m.zero(buf, 32)?;
        }
        Ok(0)
    }

    fn sys_getrusage(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf != 0 {
            m.zero(buf, 144)?;
        }
        Ok(0)
    }

    fn sys_sysinfo(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf == 0 {
            return Err(Errno::EFAULT);
        }
        m.zero(buf, 112)?;
        m.write_u64(buf, 1000)?; // uptime
        m.write_u64(buf + 40, 4 * 1024 * 1024 * 1024)?; // totalram
        m.write_u64(buf + 48, 2 * 1024 * 1024 * 1024)?; // freeram
        m.write_u32(buf + 96, 1)?; // procs
        Ok(0)
    }

    fn sys_getrandom(&mut self, m: &mut GuestMem<'_>, buf: u64, len: u64) -> SysResult {
        let len = len.min(1 << 20) as usize;
        let handle = self.vfs.open("/dev/urandom", oflags::O_RDONLY, 0)?;
        let mut tmp = vec![0u8; len];
        handle.read_at(0, &mut tmp)?;
        m.write(buf, &tmp)?;
        Ok(len as u64)
    }

    fn sys_prlimit(&mut self, m: &mut GuestMem<'_>, new: u64, old: u64) -> SysResult {
        if old != 0 {
            m.write_u64(old, 8 * 1024 * 1024)?;
            m.write_u64(old + 8, u64::MAX)?;
        }
        let _ = new;
        Ok(0)
    }
}

/// Live process state, exposed to `/proc`.
///
/// Held behind a lock so procfs can read it without borrowing the kernel,
/// which would deadlock against the syscall that is currently running.
pub struct ProcState {
    pub pid: i32,
    pub maps: String,
    pub cmdline: Vec<u8>,
    pub environ: Vec<u8>,
    pub exe: String,
    pub cwd: String,
    pub fds: Vec<(i32, String)>,
}

/// A `/proc` source backed by a snapshot the kernel refreshes.
pub struct KernelProcSource(pub Mutex<ProcState>);

impl ProcSource for KernelProcSource {
    fn pid(&self) -> i32 {
        self.0.lock().unwrap().pid
    }
    fn maps(&self) -> String {
        self.0.lock().unwrap().maps.clone()
    }
    fn cmdline(&self) -> Vec<u8> {
        self.0.lock().unwrap().cmdline.clone()
    }
    fn environ(&self) -> Vec<u8> {
        self.0.lock().unwrap().environ.clone()
    }
    fn exe(&self) -> String {
        self.0.lock().unwrap().exe.clone()
    }
    fn cwd(&self) -> String {
        self.0.lock().unwrap().cwd.clone()
    }
    fn fds(&self) -> Vec<(i32, String)> {
        self.0.lock().unwrap().fds.clone()
    }
    fn stat(&self) -> String {
        let s = self.0.lock().unwrap();
        format!(
            "{} ({}) R 0 {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0\n",
            s.pid,
            s.exe.rsplit('/').next().unwrap_or("gex"),
            s.pid,
            s.pid
        )
    }
    fn status(&self) -> String {
        let s = self.0.lock().unwrap();
        format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t0\nThreads:\t1\n",
            s.exe.rsplit('/').next().unwrap_or("gex"),
            s.pid,
            s.pid
        )
    }
}

impl Kernel {
    /// Refresh the snapshot `/proc` reads from.
    pub fn refresh_proc(&self, state: &Mutex<ProcState>) {
        let mut s = state.lock().unwrap();
        s.pid = self.pid;
        s.maps = self.mem.proc_maps();
        s.exe = self.exe_path.clone();
        s.cwd = self.vfs.cwd.clone();
        s.fds = self.fds.list();
        // argv and envp never change, so they are built once.
        if s.cmdline.is_empty() {
            s.cmdline = self
                .argv
                .iter()
                .flat_map(|a| a.bytes().chain(std::iter::once(0)))
                .collect();
            s.environ = self
                .envp
                .iter()
                .flat_map(|a| a.bytes().chain(std::iter::once(0)))
                .collect();
        }
    }
}

/// Whether a syscall could resolve a path through the VFS, and so needs
/// `/proc` to be up to date before it runs.
///
/// A `/proc` file's contents are captured when it is *opened*, so a `read` on
/// an already-open one needs nothing. Refreshing the snapshot means formatting
/// the whole VMA tree and listing every descriptor, which is far too expensive
/// to do before a `clock_gettime`.
///
/// This is deliberately a list of what does *not* need it rather than what
/// does: forgetting to exclude a syscall costs a little speed, while
/// forgetting to include one would serve a stale `/proc`.
pub fn skips_proc_snapshot(n: u64) -> bool {
    matches!(
        n,
        nr::CLOCK_GETTIME
            | nr::CLOCK_GETRES
            | nr::GETTIMEOFDAY
            | nr::CLOCK_NANOSLEEP
            | nr::NANOSLEEP
            | nr::GETPID
            | nr::GETPPID
            | nr::GETTID
            | nr::GETUID
            | nr::GETEUID
            | nr::GETGID
            | nr::GETEGID
            | nr::FUTEX
            | nr::SCHED_YIELD
            | nr::READ
            | nr::WRITE
            | nr::READV
            | nr::WRITEV
            | nr::PREAD64
            | nr::PWRITE64
            | nr::LSEEK
            | nr::CLOSE
            | nr::FCNTL
            | nr::IOCTL
            | nr::MMAP
            | nr::MUNMAP
            | nr::MPROTECT
            | nr::MADVISE
            | nr::BRK
            | nr::RT_SIGPROCMASK
            | nr::RT_SIGACTION
            | nr::RT_SIGRETURN
            | nr::SIGALTSTACK
            | nr::SET_ROBUST_LIST
            | nr::SET_TID_ADDRESS
            | nr::TKILL
            | nr::CLONE
            | nr::EXIT
            | nr::EXIT_GROUP
    )
}

/// Name a syscall for trace output.
pub fn syscall_name(n: u64) -> &'static str {
    match n {
        nr::GETCWD => "getcwd",
        nr::DUP => "dup",
        nr::DUP3 => "dup3",
        nr::FCNTL => "fcntl",
        nr::IOCTL => "ioctl",
        nr::MKDIRAT => "mkdirat",
        nr::UNLINKAT => "unlinkat",
        nr::SYMLINKAT => "symlinkat",
        nr::RENAMEAT => "renameat",
        nr::FTRUNCATE => "ftruncate",
        nr::FACCESSAT | nr::FACCESSAT2 => "faccessat",
        nr::CHDIR => "chdir",
        nr::FCHMOD => "fchmod",
        nr::FCHMODAT => "fchmodat",
        nr::OPENAT => "openat",
        nr::CLOSE => "close",
        nr::PIPE2 => "pipe2",
        nr::GETDENTS64 => "getdents64",
        nr::LSEEK => "lseek",
        nr::READ => "read",
        nr::WRITE => "write",
        nr::READV => "readv",
        nr::WRITEV => "writev",
        nr::PREAD64 => "pread64",
        nr::PWRITE64 => "pwrite64",
        nr::PPOLL => "ppoll",
        nr::READLINKAT => "readlinkat",
        nr::NEWFSTATAT => "newfstatat",
        nr::FSTAT => "fstat",
        nr::FSYNC => "fsync",
        nr::EXIT => "exit",
        nr::EXIT_GROUP => "exit_group",
        nr::SET_TID_ADDRESS => "set_tid_address",
        nr::FUTEX => "futex",
        nr::SET_ROBUST_LIST => "set_robust_list",
        nr::NANOSLEEP => "nanosleep",
        nr::CLOCK_GETTIME => "clock_gettime",
        nr::CLOCK_GETRES => "clock_getres",
        nr::CLOCK_NANOSLEEP => "clock_nanosleep",
        nr::SCHED_YIELD => "sched_yield",
        nr::KILL => "kill",
        nr::TKILL => "tkill",
        nr::TGKILL => "tgkill",
        nr::RT_SIGACTION => "rt_sigaction",
        nr::RT_SIGPROCMASK => "rt_sigprocmask",
        nr::RT_SIGRETURN => "rt_sigreturn",
        nr::SIGALTSTACK => "sigaltstack",
        nr::TIMES => "times",
        nr::UNAME => "uname",
        nr::GETRUSAGE => "getrusage",
        nr::GETTIMEOFDAY => "gettimeofday",
        nr::GETPID => "getpid",
        nr::GETPPID => "getppid",
        nr::GETUID => "getuid",
        nr::GETEUID => "geteuid",
        nr::GETGID => "getgid",
        nr::GETEGID => "getegid",
        nr::GETTID => "gettid",
        nr::SYSINFO => "sysinfo",
        nr::BRK => "brk",
        nr::MUNMAP => "munmap",
        nr::CLONE => "clone",
        nr::EXECVE => "execve",
        nr::MMAP => "mmap",
        nr::MPROTECT => "mprotect",
        nr::MADVISE => "madvise",
        nr::WAIT4 => "wait4",
        nr::PRLIMIT64 => "prlimit64",
        nr::GETRANDOM => "getrandom",
        nr::RSEQ => "rseq",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::TmpFs;

    fn setup() -> (Vec<u8>, Kernel) {
        let mem = vec![0u8; 0x40000];
        let mut k = Kernel::new(0x1000, 0x10000, "/prog".into());
        k.vfs.mount("/", Arc::new(TmpFs::new()));
        k.vfs.mount("/dev", Arc::new(crate::vfs::DevFs::new()));
        (mem, k)
    }

    fn set_args(m: &mut GuestMem<'_>, ctx_addr: u64, n: u64, args: &[u64]) {
        m.write_u64(ctx_addr + ctx::x(8) as u64, n).unwrap();
        for i in 0..6 {
            let v = args.get(i).copied().unwrap_or(0);
            m.write_u64(ctx_addr + ctx::x(i as u8) as u64, v).unwrap();
        }
    }

    fn ret(m: &GuestMem<'_>, ctx_addr: u64) -> i64 {
        m.read_u64(ctx_addr + ctx::x(0) as u64).unwrap() as i64
    }

    fn put_str(m: &mut GuestMem<'_>, addr: u64, s: &str) {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        m.write(addr, &b).unwrap();
    }

    #[test]
    fn openat_write_read_roundtrip() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        put_str(&mut m, 0x20000, "/hello.txt");

        set_args(
            &mut m,
            k.ctx_addr,
            nr::OPENAT,
            &[
                (-100i64) as u64,
                0x20000,
                oflags::O_CREAT | oflags::O_RDWR,
                0o644,
            ],
        );
        k.syscall(&mut m, 0);
        let fd = ret(&m, k.ctx_addr);
        assert!(fd >= 3, "expected a fresh descriptor, got {fd}");

        m.write(0x21000, b"payload").unwrap();
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[fd as u64, 0x21000, 7]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 7);

        set_args(&mut m, k.ctx_addr, nr::LSEEK, &[fd as u64, 0, 0]);
        k.syscall(&mut m, 0);
        set_args(&mut m, k.ctx_addr, nr::READ, &[fd as u64, 0x22000, 16]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 7);
        assert_eq!(m.read(0x22000, 7).unwrap(), b"payload");
    }

    #[test]
    fn dup_shares_the_file_offset() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        put_str(&mut m, 0x20000, "/f");
        set_args(
            &mut m,
            k.ctx_addr,
            nr::OPENAT,
            &[(-100i64) as u64, 0x20000, oflags::O_CREAT | oflags::O_RDWR, 0],
        );
        k.syscall(&mut m, 0);
        let fd = ret(&m, k.ctx_addr) as u64;
        set_args(&mut m, k.ctx_addr, nr::DUP, &[fd]);
        k.syscall(&mut m, 0);
        let dup = ret(&m, k.ctx_addr) as u64;

        m.write(0x21000, b"abcd").unwrap();
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[fd, 0x21000, 4]);
        k.syscall(&mut m, 0);
        // The duplicate must see the advanced offset, not zero.
        set_args(&mut m, k.ctx_addr, nr::LSEEK, &[dup, 0, 1]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 4);
    }

    #[test]
    fn pipes_carry_data_between_descriptors() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::PIPE2, &[0x20000, 0]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 0);
        let rfd = m.read_u32(0x20000).unwrap() as u64;
        let wfd = m.read_u32(0x20004).unwrap() as u64;

        m.write(0x21000, b"through").unwrap();
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[wfd, 0x21000, 7]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 7);
        set_args(&mut m, k.ctx_addr, nr::READ, &[rfd, 0x22000, 16]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 7);
        assert_eq!(m.read(0x22000, 7).unwrap(), b"through");
    }

    #[test]
    fn getdents_lists_a_directory() {
        let (mut buf, mut k) = setup();
        let tmp = TmpFs::new();
        tmp.add_file("one", b"1".to_vec());
        tmp.add_file("two", b"2".to_vec());
        k.vfs.mount("/d", Arc::new(tmp));
        let mut m = GuestMem::new(&mut buf);
        put_str(&mut m, 0x20000, "/d");
        set_args(
            &mut m,
            k.ctx_addr,
            nr::OPENAT,
            &[(-100i64) as u64, 0x20000, 0, 0],
        );
        k.syscall(&mut m, 0);
        let fd = ret(&m, k.ctx_addr) as u64;
        set_args(&mut m, k.ctx_addr, nr::GETDENTS64, &[fd, 0x21000, 512]);
        k.syscall(&mut m, 0);
        let n = ret(&m, k.ctx_addr);
        assert!(n > 0);
        let bytes = m.read(0x21000, n as usize).unwrap();
        let text = String::from_utf8_lossy(bytes);
        assert!(text.contains("one") && text.contains("two"));
        // A second call at the end of the directory returns zero.
        set_args(&mut m, k.ctx_addr, nr::GETDENTS64, &[fd, 0x21000, 512]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 0);
    }

    #[test]
    fn chdir_makes_relative_paths_resolve() {
        let (mut buf, mut k) = setup();
        k.vfs.mkdir("/work", 0o755).unwrap();
        let mut m = GuestMem::new(&mut buf);
        put_str(&mut m, 0x20000, "/work");
        set_args(&mut m, k.ctx_addr, nr::CHDIR, &[0x20000]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 0);

        put_str(&mut m, 0x20100, "rel.txt");
        set_args(
            &mut m,
            k.ctx_addr,
            nr::OPENAT,
            &[(-100i64) as u64, 0x20100, oflags::O_CREAT | oflags::O_RDWR, 0],
        );
        k.syscall(&mut m, 0);
        assert!(ret(&m, k.ctx_addr) >= 3);
        assert!(k.vfs.metadata("/work/rel.txt").is_ok());
    }

    #[test]
    fn dev_null_and_zero_behave() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        put_str(&mut m, 0x20000, "/dev/zero");
        set_args(
            &mut m,
            k.ctx_addr,
            nr::OPENAT,
            &[(-100i64) as u64, 0x20000, 0, 0],
        );
        k.syscall(&mut m, 0);
        let fd = ret(&m, k.ctx_addr) as u64;
        m.write(0x21000, &[0xff; 8]).unwrap();
        set_args(&mut m, k.ctx_addr, nr::READ, &[fd, 0x21000, 8]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 8);
        assert_eq!(m.read(0x21000, 8).unwrap(), &[0u8; 8]);
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
    fn exit_group_reports_the_status() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::EXIT_GROUP, &[7]);
        assert_eq!(k.syscall(&mut m, 0), SyscallOutcome::Exit(7));
    }

    #[test]
    fn faults_are_reported_rather_than_panicking() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[1, 0xffff_0000, 16]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), -14);
    }

    #[test]
    fn brk_round_trips() {
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
        set_args(&mut m, k.ctx_addr, nr::UNAME, &[0x20000]);
        k.syscall(&mut m, 0);
        assert_eq!(m.read(0x20000 + 65 * 4, 7).unwrap(), b"aarch64");
    }
}
