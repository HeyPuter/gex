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

use gex_abi::GuestEndian;
use crate::errno::{encode, Errno, SysResult};
use crate::file::{FdTable, OpenFile, PipeEnd};
use crate::fs::{oflags, Stat};
use crate::futex::{self, FutexTable};
use crate::mem::{flags as mflags, MemoryMap};
use crate::net;
use crate::proc::ProcTable;
use crate::pipe::Pipe;
use crate::unix;
use crate::signal::{self, Delivery, SigAction, SigAltStack, Signals};
use crate::stdio::{StdStream, Stream};
use crate::vfs::{FileType, ProcSource, Vfs};
use crate::GuestMem;
use gex_abi::{ctx, ctx_addr};
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
    pub const RENAMEAT2: u64 = 276;
    pub const SENDFILE: u64 = 71;
    pub const COPY_FILE_RANGE: u64 = 285;
    pub const SPLICE: u64 = 76;
    pub const FTRUNCATE: u64 = 46;
    pub const FACCESSAT: u64 = 48;
    pub const CHDIR: u64 = 49;
    pub const FCHDIR: u64 = 50;
    pub const UNSHARE: u64 = 97;
    pub const FCHMOD: u64 = 52;
    pub const FCHMODAT: u64 = 53;
    pub const OPENAT: u64 = 56;
    pub const OPENAT2: u64 = 437;
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
    pub const FDATASYNC: u64 = 83;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const GET_ROBUST_LIST: u64 = 100;
    pub const TIMERFD_CREATE: u64 = 85;
    pub const TIMERFD_SETTIME: u64 = 86;
    pub const TIMERFD_GETTIME: u64 = 87;
    pub const NANOSLEEP: u64 = 101;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const CLOCK_GETRES: u64 = 114;
    pub const CLOCK_NANOSLEEP: u64 = 115;
    pub const SYNC: u64 = 81;
    pub const STATX: u64 = 291;
    pub const PSELECT6: u64 = 72;
    pub const FCHOWNAT: u64 = 54;
    pub const FCHOWN: u64 = 55;
    pub const UTIMENSAT: u64 = 88;
    pub const STATFS: u64 = 43;
    pub const FSTATFS: u64 = 44;
    pub const UMASK: u64 = 166;
    pub const UMOUNT2: u64 = 39;
    pub const MOUNT: u64 = 40;
    pub const GETITIMER: u64 = 102;
    pub const SETITIMER: u64 = 103;
    pub const SCHED_SETPARAM: u64 = 118;
    pub const SCHED_SETSCHEDULER: u64 = 119;
    pub const SCHED_GETSCHEDULER: u64 = 120;
    pub const SCHED_GETPARAM: u64 = 121;
    pub const SCHED_GETAFFINITY: u64 = 123;
    pub const SCHED_YIELD: u64 = 124;
    pub const SCHED_GET_PRIORITY_MAX: u64 = 125;
    pub const SCHED_GET_PRIORITY_MIN: u64 = 126;
    pub const SETPRIORITY: u64 = 140;
    pub const GETPRIORITY: u64 = 141;
    pub const GETCPU: u64 = 168;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGPENDING: u64 = 136;
    pub const RT_SIGRETURN: u64 = 139;
    pub const SIGALTSTACK: u64 = 132;
    pub const RT_SIGTIMEDWAIT: u64 = 137;
    pub const REBOOT: u64 = 142;
    pub const SETREGID: u64 = 143;
    pub const SETGID: u64 = 144;
    pub const SETREUID: u64 = 145;
    pub const SETUID: u64 = 146;
    pub const SETRESUID: u64 = 147;
    pub const GETRESUID: u64 = 148;
    pub const SETRESGID: u64 = 149;
    pub const GETRESGID: u64 = 150;
    pub const TIMES: u64 = 153;
    pub const SETPGID: u64 = 154;
    pub const GETPGID: u64 = 155;
    pub const GETSID: u64 = 156;
    pub const SETSID: u64 = 157;
    pub const GETGROUPS: u64 = 158;
    pub const SETGROUPS: u64 = 159;
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
    pub const PRCTL: u64 = 167;
    pub const SOCKET: u64 = 198;
    pub const SOCKETPAIR: u64 = 199;
    pub const BIND: u64 = 200;
    pub const LISTEN: u64 = 201;
    pub const ACCEPT: u64 = 202;
    pub const ACCEPT4: u64 = 242;
    pub const MEMFD_CREATE: u64 = 279;
    pub const FLOCK: u64 = 32;
    pub const FALLOCATE: u64 = 47;
    pub const EVENTFD2: u64 = 19;
    pub const EPOLL_CREATE1: u64 = 20;
    pub const EPOLL_CTL: u64 = 21;
    pub const EPOLL_PWAIT: u64 = 22;
    pub const EPOLL_PWAIT2: u64 = 441;
    pub const CLOSE_RANGE: u64 = 436;
    pub const MEMBARRIER: u64 = 283;
    pub const CONNECT: u64 = 203;
    pub const GETSOCKNAME: u64 = 204;
    pub const GETPEERNAME: u64 = 205;
    pub const SENDTO: u64 = 206;
    pub const RECVFROM: u64 = 207;
    pub const SETSOCKOPT: u64 = 208;
    pub const GETSOCKOPT: u64 = 209;
    pub const SHUTDOWN: u64 = 210;
    pub const SENDMSG: u64 = 211;
    pub const RECVMSG: u64 = 212;
    pub const RECVMMSG: u64 = 243;
    pub const SENDMMSG: u64 = 269;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const CLONE: u64 = 220;
    pub const EXECVE: u64 = 221;
    pub const CLONE3: u64 = 435;
    pub const MMAP: u64 = 222;
    pub const MREMAP: u64 = 216;
    pub const MPROTECT: u64 = 226;
    pub const MADVISE: u64 = 233;
    pub const MSYNC: u64 = 227;
    pub const WAIT4: u64 = 260;
    pub const PRLIMIT64: u64 = 261;
    pub const GETRANDOM: u64 = 278;
    pub const RSEQ: u64 = 293;
    pub const FACCESSAT2: u64 = 439;

    // Legacy calls only x86-64 has, given canonical numbers above anything
    // the asm-generic ABI assigns. See `crate::x86::translate`.
    pub const DUP2: u64 = 0x1000;
    pub const TIME: u64 = 0x1001;
    pub const ALARM: u64 = 0x1002;
    pub const ARCH_PRCTL: u64 = 0x1003;
    /// Advisory calls a guest may issue and never checks: `fadvise64`,
    /// `readahead`, `mlock`... Answered with success and nothing done.
    pub const NOOP_OK: u64 = 0x1004;
}

/// Hand a system call's result back to the guest: `x0`, or `rax` (both
/// `x(0)`), or on PowerPC r3 with an error as a positive errno and CR0.SO
/// set (docs/ppc.md §6). `ret` is the kernel's own convention, `-errno` for
/// a failure. Every place a result is written goes through here.
pub fn set_result(m: &mut GuestMem<'_>, ctx_addr: u64, ret: u64) -> Result<(), Errno> {
    if gex_abi::PPC {
        let err = (-4095..0).contains(&(ret as i64));
        let v = if err { (ret as i64).wrapping_neg() as u64 } else { ret & 0xffff_ffff };
        m.write_u64(ctx_addr + ctx::SYSCALL_RET as u64, v)?;
        return m.write(ctx_addr + (ctx::PPC_CR + 3) as u64, &[err as u8]);
    }
    m.write_u64(ctx_addr + ctx::x(0) as u64, crate::gpr(ret))
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
    /// `clone` without `CLONE_THREAD`: a new *process*.
    ///
    /// The host copies the address space, duplicates this kernel, and runs
    /// the child from the same instruction with `x0` zero. Nothing about the
    /// child's memory can be prepared here, because the kernel can only see
    /// the parent's.
    SpawnProcess {
        pid: i32,
        /// The stack the child must start on, or zero to keep the parent's.
        ///
        /// A plain `fork` passes zero. `posix_spawn` does not: musl clones
        /// onto a stack it allocated, and its `__clone` trampoline pops the
        /// function to run off that stack, so a child left on the parent's
        /// stack pointer reads whatever happens to be there.
        child_stack: u64,
        /// `CLONE_SETTLS`: the thread pointer for the child.
        tls: Option<u64>,
    },
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
    /// `execve`: replace this process's program.
    ///
    /// The kernel has read the strings out of the address space it is about
    /// to lose; everything else -- loading the image, building a new address
    /// space -- is the host's, because only the host can make one.
    Exec {
        path: String,
        argv: Vec<String>,
        envp: Vec<String>,
    },
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
        /// A queue place snapshotted BEFORE the word was checked, for a host
        /// (the browser) that parks cooperatively instead of blocking under
        /// the queue lock. Taking it before the check is what stops a
        /// `FUTEX_WAKE` that lands between the check and the park from being
        /// lost -- the native host holds the lock across both instead and
        /// ignores this. See [`futex::FutexTable::park`].
        parked: Option<crate::futex::Parked>,
    },
    Sleep(std::time::Duration),
    /// `FUTEX_LOCK_PI(2)`: take the priority-inheritance lock at `uaddr` for
    /// `tid`, waiting as long as it is held. The host loops: an atomic take
    /// when the word is free (keeping the waiters bit, since others may
    /// sleep), else set the waiters bit and wait on the word; `EDEADLK` for
    /// a lock `tid` already owns; `ETIMEDOUT` at `deadline`.
    FutexLockPi {
        table: Arc<FutexTable>,
        uaddr: u64,
        tid: u32,
        deadline: Option<std::time::Duration>,
    },
    /// `rt_sigsuspend`: wait until a signal the caller can see arrives.
    ///
    /// The blocked mask has already been replaced with the one the caller
    /// asked for, so "can see" means whatever is deliverable now.
    /// `restore_mask` is what to put back afterwards, which
    /// [`Kernel::finish_blocking`] does before the call returns `EINTR` --
    /// the only thing it ever returns.
    SigSuspend { restore_mask: u64 },
    /// A `read` on a stream with nothing in it yet: a terminal nobody has
    /// typed at, a socket with nothing arrived.
    ///
    /// The handle travels with the operation so the host can both ask whether
    /// it has become readable and perform the read once it has, without
    /// holding the kernel meanwhile. On a host with real threads that is a
    /// poll; in the browser, where there is one wasm stack, it is the only
    /// way a shell can wait for a keystroke at all.
    ReadStream {
        handle: Arc<dyn crate::vfs::FileHandle>,
        buf: u64,
        len: usize,
    },
    /// A `connect` on a descriptor that is not non-blocking. Ready once the
    /// connection is up, refused, or gone.
    Connect { socket: Arc<net::Socket> },
    /// An `accept` on a listener with nothing waiting. The host only waits;
    /// the descriptor is made by the kernel afterwards, because only the
    /// kernel has a descriptor table. See `Kernel::retry_accept`.
    ///
    /// A `FileHandle` rather than a `UnixSocket`, because the socket may be
    /// in the global kernel instead of this process -- and what the scheduler
    /// wants of it is the one question both kinds answer the same way:
    /// `poll_readable`, which for a listener means a connection is waiting.
    AcceptWait {
        socket: Arc<dyn crate::vfs::FileHandle>,
    },
    /// A `poll` with nothing ready yet. The descriptors travel with it so the
    /// host can ask again without going back through the kernel.
    Poll {
        fds: Vec<PollFd>,
        deadline: Option<std::time::Duration>,
        /// How the answer is reported, which is the only thing that differs
        /// between `poll` and `select`.
        report: PollReport,
    },
    /// A `write` to a stream with no room: a pipe nobody is draining.
    WriteStream {
        handle: Arc<dyn crate::vfs::FileHandle>,
        buf: u64,
        len: usize,
    },
    /// `wait4`. Blocks until one of `children` has a status to collect.
    WaitChild {
        /// Empty when the table is shared: a `ChildHandle` is an `Arc`, and
        /// an `Arc` names a memory the other processes cannot read. A shared
        /// table is asked instead -- `wait_ready` to see whether this thread
        /// can run, `collect` to take the child.
        children: Vec<ChildHandle>,
        /// So the waiter can drop the record once it has collected the child,
        /// and can notice a halt while parked.
        procs: Arc<ProcTable>,
        /// Who is waiting. Implicit in `children` when the table is local,
        /// and needed on every call when it is not.
        ppid: i32,
        /// Which child, or -1 for any.
        want: i32,
        /// Where to write the wait status, or zero.
        wstatus: u64,
        /// `WNOHANG`: report nothing rather than blocking.
        nohang: bool,
    },
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

/// Whether a path names a shared object, by the only thing available at
/// `openat`: its name.
///
/// `libc.so.6`, `ld-linux-aarch64.so.1`, `libfoo.so` -- the component is a
/// shared object when it ends in `.so` or has `.so.` in it. Reading the ELF
/// header would be exact, but an `openat` has no bytes yet and the cost of
/// being wrong is small: the mapping goes somewhere no other process will
/// look, which is where every mapping went before this existed.
fn is_shared_object(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".so") || name.contains(".so.")
}

/// Somewhere that knows where a shared object belongs.
///
/// Implemented by the host, because the answer has to be the same in every
/// process and a process's kernel state is its own. The browser answers out
/// of the global kernel every Worker shares; a host with nothing to share
/// leaves it unset and the arena allocates as it always did.
pub trait ObjectPlace: Send + Sync {
    /// Where `path` goes, given how long the reservation for it is. `None`
    /// is no opinion.
    fn place(&self, path: &str, len: u64) -> Option<u64>;
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
    /// The system's processes: pids, parents, groups and sessions. Shared by
    /// every process, because reparenting an orphan and killing a process
    /// group are both questions no single process can answer. See [`crate::proc`].
    pub procs: Arc<ProcTable>,
    /// Where a pipe's buffer goes when the two ends may be in different
    /// address spaces. `None` keeps the local `Pipe`, which is right whenever
    /// every process really is one process. See [`crate::pipe::Remote`].
    pub pipes: Option<crate::pipe::Remote>,
    /// Where a unix socket lives when its two ends may be in different
    /// address spaces. `None` keeps the local `UnixSocket`, which is right
    /// whenever every process really is one process.
    ///
    /// It matters for exactly the case the switcher used to hide: an X server
    /// binds a name in its own Worker and its client, another Worker, looks
    /// in a name table that has never heard of it. See [`crate::unix::Remote`].
    pub unix_remote: Option<crate::unix::Remote>,
    /// Where a pseudoterminal pair lives when its master and slave may be in
    /// different address spaces (xterm and its shell). `None` keeps a local
    /// `Pty`, right whenever every process really is one process.
    pub ptys: Option<crate::pty::Remote>,
    /// This process's parent, cached from the table so `getppid` is not a
    /// lock. It never changes for a live process except when the parent dies,
    /// and `getppid` re-reads the table in that case.
    pub ppid: i32,
    /// How to read the wall clock.
    pub clock: ClockFn,
    /// The interval timers, indexed by `which`. See [`Kernel::check_timers`].
    pub itimers: [Option<Itimer>; 3],
    /// An `accept` that waited, and the arguments to finish it with.
    ///
    /// A blocking operation is performed by the host with the kernel
    /// unlocked, and the host cannot allocate a descriptor. So `accept`
    /// splits: the host waits for a connection to arrive, and the work
    /// happens back here.
    pub retry_accept: Option<(i32, u64, u64, u64)>,
    /// A write parked as a blocking operation: its `EPIPE`, when it comes
    /// back through `finish_blocking`, raises SIGPIPE as a direct one does.
    pub write_blocked: bool,
    /// A `recvmsg` on a blocking socket with nothing to read: the fd and the
    /// `msghdr`, re-run once the socket becomes readable. Like `retry_accept`,
    /// the real work waits for the kernel lock (the msghdr may carry
    /// descriptors, which only the kernel's table can place). Wine's protocol
    /// with wineserver is a blocking `recvmsg` for every reply.
    pub retry_recvmsg: Option<(i32, u64, u64)>,
    /// A `splice`, `sendfile` or `copy_file_range` (number, arguments) from
    /// an empty blocking pipe or socket, re-run once it is readable, as
    /// `retry_recvmsg`.
    pub retry_transfer: Option<(u64, [u64; 6])>,
    /// The blocked mask an `rt_sigsuspend` has to put back when it returns.
    pub restore_sigmask: Option<u64>,
    /// A 32-bit (i386) process: its code runs in compatibility mode, it
    /// enters with `int 0x80`, and its numbering and structures are the i386
    /// ones (see `crate::i386`). Set at exec from the image's class,
    /// inherited across fork.
    pub compat: bool,
    /// Set by a syscall that may have made a descriptor readable or
    /// writable for someone else (a write, a read that freed room, a close,
    /// a signal); the host takes it after the call and wakes threads parked
    /// on descriptors ([`Kernel::take_fd_activity`]).
    pub fd_activity: bool,
    /// The wasm32 personality (docs/wine-native.md §3.11): a native wasm32
    /// program on the i386 ABI. Everything is i386's except how control
    /// moves: a signal frame or a `sigreturn` is left in the thread context
    /// and the syscall returns normally ([`SyscallOutcome::Continue`]), for
    /// the program's own syscall stub to run the handler or carry on -- a
    /// wasm program's frames cannot be resumed at a new guest PC. Set by the
    /// host for each program it starts; kept across fork, cleared by exec.
    pub wasm32: bool,
    /// The i386 process entered through the non-rt `sigreturn` (119), whose
    /// frame is laid out differently from `rt_sigreturn`'s. Set at entry,
    /// consumed by `sigreturn`.
    pub sigreturn_legacy: bool,
    /// When this process began, on the guest's clock, so it can be asked how
    /// much time it has used. See [`Kernel::cpu_nanos`].
    ///
    /// Zero until something asks, rather than set at construction: a host may
    /// replace `clock` after building the kernel -- the browser does, because
    /// `SystemTime` panics there -- and a start time taken from the clock that
    /// was in place beforehand is fifty years out.
    pub started: u128,
    /// Where connections come from, when the host has anywhere to get them.
    /// `None` is a guest with no network, which is every guest until a host
    /// says otherwise.
    pub net: Option<Arc<dyn net::NetStack>>,
    /// Names the guest has resolved. Shared across `fork`, because a child
    /// connecting to an address its parent resolved must reach the same host.
    pub resolver: Arc<net::Resolver>,
    /// The TCP ports guests listen on. Shared across `fork` like the resolver.
    pub ports: Arc<net::Ports>,
    /// Sockets by handle address, so a descriptor can be got back to the
    /// concrete socket the network syscalls need.
    pub sockets: std::collections::HashMap<usize, Arc<net::Socket>>,
    /// The same, for the ones that live in the global kernel. Two tables
    /// rather than a trait object because the calls that need the concrete
    /// type -- `accept`, `has_pending`, descriptor passing -- are exactly the
    /// ones the two kinds do not share.
    pub remote_sockets: std::collections::HashMap<usize, Arc<unix::RemoteUnix>>,
    /// And for epoll sets.
    pub epolls: std::collections::HashMap<usize, Arc<unix::EpollSet>>,
    /// And for anonymous shared files, which have seals.
    pub memfds: std::collections::HashMap<usize, Arc<unix::MemFd>>,
    /// Which unix socket answers to which path. Shared system-wide, because
    /// a server and its clients are different processes and the whole point
    /// of a named socket is that they find each other by name.
    pub socket_names: Arc<unix::SocketNames>,
    /// Names *this process's host* listens on, which is not the same table.
    ///
    /// The compositor and the GPU device are in the shim, one per process
    /// (`mount_devices` gives every adopted child its own), and they can only
    /// read the memory of the process they are in -- a `wl_shm` pool is guest
    /// memory. So `wayland-0` must resolve to the compositor of the process
    /// that is connecting, and not to whichever one bound the name last. It is
    /// deliberately NOT shared across a fork: a child gets its own.
    pub host_names: Arc<unix::SocketNames>,
    /// Advisory whole-file locks, shared for the same reason.
    pub file_locks: Arc<unix::FileLocks>,
    /// Where each object of a statically recompiled closure must be placed.
    ///
    /// A translated block has its guest addresses baked into it, so a module
    /// built ahead of time is only correct if the object it was built from
    /// lands where the build assumed. Nothing needs to *predict* that: the
    /// guest's `ld.so` reserves each object with `mmap(NULL, ..)` and takes
    /// whatever address it is given, so the layout is gex's to assign. This
    /// is the assignment, by guest path.
    ///
    /// Empty unless a closure build asked for it, and then honoured only for
    /// the reservation described at `armed_pin`.
    pub pins: std::collections::HashMap<String, u64>,
    /// The base the next anonymous reservation should get, and for what.
    ///
    /// `ld.so` does not map an object in one call. It opens the file, reads
    /// the header, reserves the whole span with an *anonymous* `PROT_NONE`
    /// mapping, and only then maps the segments into it with `MAP_FIXED`. The
    /// reservation is the one address gex chooses and the only one worth
    /// pinning -- and being anonymous it carries no file to key on. So the
    /// `openat` immediately before it is the signal, and this is what that
    /// signal sets.
    ///
    /// The predicate has to include `PROT_NONE`: a real trace of glibc's
    /// loader shows a 8 KiB `PROT_READ|WRITE` allocation of its own between
    /// the `fstat` and the reservation, so "the next anonymous mapping" would
    /// pin the wrong one.
    pub armed_pin: Option<(String, u64)>,
    /// Who decides where a shared object goes, when anybody does.
    ///
    /// [`Self::pins`] is the same idea decided ahead of time by a closure
    /// build. This one is decided as the run goes: the first process to map
    /// an object fixes its address and every later one is given the same,
    /// which is what makes a translated module worth sharing between them.
    pub place: Option<Arc<dyn ObjectPlace>>,
    /// Which object the next anonymous reservation is for.
    ///
    /// The same signal [`Self::armed_pin`] uses and for the same reason: the
    /// reservation carries no file, so the `openat` before it is what names
    /// the object. Kept separate because a pin knows its address already and
    /// this one has to ask, and can only ask once the reservation says how
    /// long the object is.
    pub armed_place: Option<String>,
    /// Every thread's own signal state but the running one's, which is in
    /// `signals` (`Signals::save_thread`/`load_thread`): masks, thread-directed
    /// pending signals and alternate stacks are per thread, as on Linux.
    pub thread_signals: std::collections::HashMap<i32, crate::signal::ThreadSignals>,
    /// The thread whose part `signals` holds now.
    pub sig_tid: i32,
    /// Every other thread's half-finished blocking call (`PendingCall`),
    /// swapped with `signals`: one thread's `rt_sigsuspend` must not hand
    /// its mask to the next thread that finishes a futex wait (Mono's GC
    /// left the main thread with SIGPWR blocked that way).
    pub thread_calls: std::collections::HashMap<i32, PendingCall>,
    /// The pc of the syscall being handled (docs/mem64.md: a mapping the
    /// dynamic loader asks for stays below 4 GiB, since it will hold code).
    pub syscall_pc: u64,
    /// Where each pinned object actually landed, for checking afterwards.
    ///
    /// A pin that silently does not take is the worst outcome available: the
    /// module is built for one address and the code runs at another, and the
    /// guest branches into whatever is there. Recorded so a mismatch can be
    /// reported rather than executed.
    pub pin_landed: Vec<(String, u64, u64)>,
    /// File mappings whose writes are meant to reach the file.
    pub shared_maps: Vec<SharedMap>,
    /// The working directories of threads that left the process's
    /// (`unshare(CLONE_FS)`), by context address: swapped into `vfs` for
    /// the length of each of that thread's system calls.
    pub thread_cwd: std::collections::HashMap<u64, String>,
    /// Executable mappings made since the host last looked.
    pub new_code: Vec<CodeMap>,
    /// Executable ranges *unmapped* since the host last looked.
    ///
    /// A translation is only valid while the code it was made from is still
    /// there. `dlclose` unmaps a library and a later `dlopen` maps a
    /// different one over the same addresses, and without this the guest
    /// branches into the previous library's translated code -- the dispatch
    /// slot still answers for the page. Nothing reports an error; the guest
    /// simply runs the wrong function. A long-running X server does exactly
    /// this every time its last client disconnects and it resets.
    pub retired_code: Vec<(u64, u64)>,
    /// Serial for the unnamed files `O_TMPFILE` asks for.
    pub next_tmpfile: u64,
    /// The file-creation mask. Recorded rather than applied: nothing here
    /// enforces permissions, but `umask(022)` is expected to hand back what
    /// it replaced, and a build system that does not get one back concludes
    /// the mask was zero and re-runs itself.
    pub umask: u32,
    /// The terminal: flags, size, foreground process group, and the line
    /// discipline behind them. See [`crate::tty`].
    ///
    /// Shared, not copied, and that is the point of it. There is one terminal
    /// and its settings outlive any process using it: `stty -echo` in one
    /// command must still be off in the next, a shell that puts the terminal
    /// in raw mode and dies must leave it that way for whoever cleans up, and
    /// `^C` goes to the *foreground group* -- which is a fact about the
    /// system, not about the process that happened to type it.
    pub tty: Arc<crate::tty::TtyRef>,
}

/// One armed interval timer.
///
/// Times are absolute nanoseconds on the same clock the guest reads, because
/// a deadline compared against a clock that can be replaced per host has to
/// be in that clock's terms.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Itimer {
    /// When it next fires.
    pub next: u128,
    /// What to reload on firing. Zero is a one-shot timer, which disarms.
    pub interval: u128,
}

/// The three interval timers, in the order `which` names them.
pub const ITIMER_REAL: usize = 0;
pub const ITIMER_VIRTUAL: usize = 1;
pub const ITIMER_PROF: usize = 2;

/// How the host tells the time: seconds and nanoseconds since the epoch.
///
/// A function rather than a call to `SystemTime`, because there are hosts
/// without one. `wasm32-unknown-unknown` has no clock at all -- `SystemTime`
/// panics there -- so the browser host passes its own, and everything else
/// gets [`system_clock`].
pub type ClockFn = fn() -> (u64, u32);

/// Whether an `EPIPE` from syscall `n` also raises SIGPIPE, as Linux does
/// for a write to a pipe or stream socket with no reader -- except a `send`
/// with `MSG_NOSIGNAL`. (A shell pipeline's `yes | head` ends with the
/// writer killed by it; without it the writer sees EPIPE and complains.)
fn raises_sigpipe(n: u64, a: &[u64; 6]) -> bool {
    const MSG_NOSIGNAL: u64 = 0x4000;
    match n {
        nr::WRITE | nr::WRITEV | nr::PWRITE64 | nr::SENDFILE | nr::SPLICE => true,
        nr::SENDTO => a[3] & MSG_NOSIGNAL == 0,
        nr::SENDMSG => a[2] & MSG_NOSIGNAL == 0,
        nr::SENDMMSG => a[3] & MSG_NOSIGNAL == 0,
        _ => false,
    }
}

/// Read a `struct timeval` -- seconds then microseconds -- as nanoseconds.
fn read_timeval(m: &GuestMem<'_>, at: u64) -> Result<u128, Errno> {
    let secs = m.read_u64(at)?;
    let usecs = m.read_u64(at + 8)?;
    Ok(secs as u128 * 1_000_000_000 + (usecs as u128).min(999_999) * 1_000)
}

/// Write nanoseconds back as a `struct timeval`.
fn write_timeval(m: &mut GuestMem<'_>, at: u64, nanos: u128) -> Result<(), Errno> {
    m.write_u64(at, (nanos / 1_000_000_000) as u64)?;
    m.write_u64(at + 8, (nanos % 1_000_000_000 / 1_000) as u64)?;
    Ok(())
}

/// The clock of a host that has one.
/// CLOCK_MONOTONIC and its kin (RAW 4, COARSE 6, BOOTTIME 7, BOOTTIME_ALARM 9).
fn is_monotonic_clock(clk: u64) -> bool {
    matches!(clk, 1 | 4 | 6 | 7 | 9)
}

/// Where the monotonic clocks start: the wall clock at the first reading,
/// less [`MONOTONIC_START_SECS`]. One base for every guest process of this
/// host process, as on a machine every process shares the boot time.
static MONOTONIC_BASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The "uptime" the guest sees at its first monotonic reading.
const MONOTONIC_START_SECS: u64 = 1000;

/// A monotonic clock reading made from a wall clock reading: seconds since
/// a shared boot instant, not since the epoch. Programs truncate monotonic
/// milliseconds to 32 bits and compare them across processes -- X server
/// event times are `(CARD32)` CLOCK_MONOTONIC milliseconds, and Unity
/// compares them with its own clock -- which only works while the uptime
/// is under 49.7 days, as it is on real machines; an epoch-based value
/// (1.8e12 ms) never agrees with its own truncation.
fn monotonic_from((secs, nanos): (u64, u32)) -> (u64, u32) {
    use std::sync::atomic::Ordering;
    let mut base = MONOTONIC_BASE.load(Ordering::Relaxed);
    if base == 0 {
        let want = secs.saturating_sub(MONOTONIC_START_SECS).max(1);
        base = match MONOTONIC_BASE.compare_exchange(0, want, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => want,
            Err(other) => other,
        };
    }
    (secs.saturating_sub(base), nanos)
}

pub fn system_clock() -> (u64, u32) {
    #[cfg(not(target_family = "wasm"))]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (now.as_secs(), now.subsec_nanos())
    }
    // Standing still is wrong but it is not a panic, and a host that cares
    // replaces this.
    #[cfg(target_family = "wasm")]
    {
        (0, 0)
    }
}

/// Apply a `set*id` request, where the all-ones value means "leave alone".
fn set_id(slot: &mut u32, want: u64) -> SysResult {
    if want as u32 != u32::MAX {
        *slot = want as u32;
    }
    Ok(0)
}

/// The path a syscall names, for the trace.
///
/// A pointer is not a useful thing to read in a trace of a package manager:
/// half the diagnosis is which file it could not open. Only the calls that
/// take a path at a known argument are decoded, and a bad pointer prints
/// nothing rather than failing.
fn traced_path(m: &GuestMem<'_>, n: u64, a: [u64; 6]) -> String {
    let at = match n {
        nr::OPENAT
        | nr::OPENAT2
        | nr::NEWFSTATAT
        | nr::FACCESSAT
        | nr::FACCESSAT2
        | nr::UNLINKAT
        | nr::MKDIRAT
        | nr::READLINKAT
        | nr::UTIMENSAT
        | nr::FCHOWNAT
        | nr::FCHMODAT
        | nr::STATX => a[1],
        nr::CHDIR | nr::STATFS | nr::MOUNT | nr::UMOUNT2 => a[0],
        nr::RENAMEAT | nr::RENAMEAT2 | nr::LINKAT => a[1],
        // `symlinkat` names the link it creates third, not second.
        nr::SYMLINKAT => a[2],
        _ => return String::new(),
    };
    match m.read_cstr(at, 4096) {
        Ok(raw) => format!(" {:?}", String::from_utf8_lossy(&raw)),
        Err(_) => String::new(),
    }
}

/// The terminal's settings and how much of them the kernel exchanges.
///
/// Both live in [`crate::tty`] now, next to the discipline that obeys them --
/// there is no second definition here to fall out of step with it.
pub use crate::tty::{lflag::ECHO, lflag::ICANON, COOKED as COOKED_TERMIOS, KERNEL_TERMIOS};


/// One entry of a `poll` set, kept so the host can re-check it without going
/// back through the kernel.
#[derive(Clone, Debug)]
pub struct PollFd {
    /// Where the guest's `struct pollfd` sits, so `revents` can be written.
    /// Unused when the waiter is `select`, which reports through bitmaps.
    pub addr: u64,
    /// The descriptor, which `select` needs to find its bit again.
    pub fd: i32,
    pub events: u16,
    /// `None` for a descriptor that does not exist, which is `POLLNVAL`.
    pub handle: Option<Arc<dyn crate::vfs::FileHandle>>,
    pub writable: bool,
}

/// How a waiter wants its answer written back.
///
/// `poll` and `select` ask the same question and differ only in the shape of
/// the reply: one writes `revents` next to each descriptor, the other sets
/// bits in three bitmaps. Everything between them is shared.
#[derive(Clone, Debug)]
pub enum PollReport {
    /// `revents` in place, in the caller's `pollfd` array.
    Poll,
    /// `epoll_wait`'s output array: one `struct epoll_event` per ready
    /// descriptor, rather than a slot for every one that was asked about.
    Epoll {
        /// Where the array starts.
        out: u64,
        /// How many entries fit.
        max: u32,
        /// The opaque word to hand back for each descriptor.
        data: Vec<(i32, u64)>,
    },
    /// `select`'s three `fd_set`s. A zero address means the caller did not
    /// pass that set.
    Select {
        nfds: u32,
        readfds: u64,
        writefds: u64,
        exceptfds: u64,
    },
}

/// What a `poll` entry would report right now.
pub fn poll_revents(p: &PollFd) -> u16 {
    const POLLIN: u16 = 0x001;
    const POLLOUT: u16 = 0x004;
    const POLLNVAL: u16 = 0x020;
    let Some(h) = &p.handle else {
        return POLLNVAL;
    };
    let mut r = 0;
    if p.events & POLLIN != 0 && h.poll_readable() {
        r |= POLLIN;
    }
    // Whether a write would block is asked of the handle every time, not
    // frozen: a parked poll outlives the state it was built from, and a
    // stored "not writable" never moved once the reader drained the buffer.
    if p.events & POLLOUT != 0 && p.writable && h.poll_writable() {
        r |= POLLOUT;
    }
    // Never masked by `events`, as on Linux.
    const POLLHUP: u16 = 0x010;
    if h.poll_hup() {
        r |= POLLHUP;
    }
    r
}

pub fn poll_ready(fds: &[PollFd]) -> bool {
    fds.iter().any(|p| poll_revents(p) != 0)
}

/// Read-ready and write-ready counts from the last `select`. See the note
/// where they are set.
pub static SELECT_READY: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

pub fn poll_finish(fds: &[PollFd], report: &PollReport, m: &mut GuestMem<'_>) -> SysResult {
    const POLLIN: u16 = 0x001;
    const POLLOUT: u16 = 0x004;
    match report {
        PollReport::Poll => {
            let mut ready = 0u64;
            for p in fds {
                let r = poll_revents(p);
                if r != 0 {
                    ready += 1;
                }
                m.write_u32(p.addr + 4, ((r as u32) << 16) | p.events as u32)?;
            }
            Ok(ready)
        }
        PollReport::Epoll { out, max, data } => {
            // Only the ready ones, each with the caller's own token, which is
            // the whole difference between this and `poll`.
            let mut n = 0u64;
            for p in fds {
                if n >= *max as u64 {
                    break;
                }
                let r = poll_revents(p);
                if r == 0 {
                    continue;
                }
                let token = data
                    .iter()
                    .find(|(fd, _)| *fd == p.fd)
                    .map(|(_, d)| *d)
                    .unwrap_or(0);
                // The same layout `write_epoll_events` uses: packed 12 bytes
                // on x86-64 (the token at offset four), 16 on aarch64. Xvfb
                // read its `data.ptr` from the wrong offset after every wait
                // that had to block, and dereferenced the mask.
                if gex_abi::X86 {
                    let at = out + n * 12;
                    m.write_u32(at, r as u32)?;
                    m.write_u64(at + 4, token)?;
                } else {
                    let at = out + n * 16;
                    m.write_u32(at, r as u32)?;
                    m.write_u32(at + 4, 0)?;
                    m.write_u64(at + 8, token)?;
                }
                n += 1;
            }
            Ok(n)
        }
        PollReport::Select {
            nfds,
            readfds,
            writefds,
            exceptfds,
        } => {
            // `select` reports by rewriting the caller's sets in place, so
            // every set it was given is cleared first: a descriptor that is
            // not ready must come back clear even though it went in set.
            let words = (*nfds as usize).div_ceil(64).max(1);
            let mut out = [[0u64; 16]; 2];
            for p in fds {
                if p.fd < 0 || p.fd as u32 >= *nfds {
                    continue;
                }
                let r = poll_revents(p);
                let (w, bit) = ((p.fd / 64) as usize, p.fd % 64);
                if w >= 16 {
                    continue;
                }
                if r & POLLIN != 0 {
                    out[0][w] |= 1 << bit;
                }
                if r & POLLOUT != 0 {
                    out[1][w] |= 1 << bit;
                }
            }
            let mut ready = 0u64;
            let mut per = [0u64; 2];
            for (i, set) in [*readfds, *writefds].into_iter().enumerate() {
                if set == 0 {
                    continue;
                }
                for w in 0..words.min(16) {
                    m.write_bits64(set + (w as u64) * 8, out[i][w])?;
                    per[i] += out[i][w].count_ones() as u64;
                    ready += out[i][w].count_ones() as u64;
                }
            }
            // Split the count, because the total hides the answer.
            //
            // apt spins on `pselect6(7, ..) = 7` and never reads: seven
            // descriptors are ready and it believes none of them is readable.
            // Whether those seven bits are readability or writability decides
            // whether the bug is in what a pipe reports or in what apt is
            // waiting for. See task 140.
            SELECT_READY[0].store(per[0], core::sync::atomic::Ordering::Relaxed);
            SELECT_READY[1].store(per[1], core::sync::atomic::Ordering::Relaxed);
            // Nothing here ever reports an exceptional condition, but a
            // caller that passed the set expects it cleared.
            if *exceptfds != 0 {
                for w in 0..words.min(16) {
                    m.write_bits64(*exceptfds + (w as u64) * 8, 0)?;
                }
            }
            Ok(ready)
        }
    }
}

/// An executable mapping the guest has just made.
///
/// The host drains these and statically recompiles what is in them. Only the
/// executable arrives through gex's own loader; every shared library is
/// mapped here, by the interpreter, out of guest code -- so without this the
/// static recompiler never sees any of them.
#[derive(Clone, Debug)]
pub struct CodeMap {
    pub start: u64,
    pub end: u64,
    /// The file it came from, for the trace.
    pub path: String,
    /// The file offset mapped at `start` (where a shared object's load bias
    /// is worked out from: the host's ELF units, nt_cgen::on_elf_maps).
    pub offset: u64,
}

/// A `MAP_SHARED` file mapping, and where its bytes belong.
///
/// A file mapping here is an eager copy rather than a page table pointed at
/// the file, so the two are only the same thing until somebody writes. This
/// is what makes them the same thing again.
#[derive(Debug)]
pub struct SharedMap {
    pub start: u64,
    pub len: u64,
    pub file: Arc<OpenFile>,
    pub offset: u64,
}

/// A child process the kernel is tracking.
#[derive(Debug)]
pub struct Child {
    pub pid: i32,
    /// Set when the child exits, with the status `wait4` should report.
    pub status: Arc<Mutex<Option<i32>>>,
    /// Signalled when `status` is filled in.
    pub done: Arc<std::sync::Condvar>,
    /// Set once a `wait4` has collected this child, so a second one reports
    /// `ECHILD` rather than blocking forever.
    pub reaped: Arc<std::sync::atomic::AtomicBool>,
}

impl Child {
    pub fn new(pid: i32) -> Child {
        Child {
            pid,
            status: Arc::new(Mutex::new(None)),
            done: Arc::new(std::sync::Condvar::new()),
            reaped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The handle the host needs to wait on this child.
    pub fn handle(&self) -> ChildHandle {
        ChildHandle {
            pid: self.pid,
            status: Arc::clone(&self.status),
            done: Arc::clone(&self.done),
            reaped: Arc::clone(&self.reaped),
        }
    }
}

/// A child, as the host sees it while waiting with the kernel unlocked.
#[derive(Debug, Clone)]
pub struct ChildHandle {
    pub pid: i32,
    pub status: Arc<Mutex<Option<i32>>>,
    pub done: Arc<std::sync::Condvar>,
    pub reaped: Arc<std::sync::atomic::AtomicBool>,
}

/// A blocking call's state that waits in the kernel for the host to finish
/// the wait (`finish_blocking`), kept per thread: see `Kernel::thread_calls`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendingCall {
    pub retry_accept: Option<(i32, u64, u64, u64)>,
    pub write_blocked: bool,
    pub retry_recvmsg: Option<(i32, u64, u64)>,
    pub retry_transfer: Option<(u64, [u64; 6])>,
    pub restore_sigmask: Option<u64>,
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
        self.switch_signals(t.tid);
    }

    /// Make `tid`'s signal state the running one (see `thread_signals`).
    fn switch_signals(&mut self, tid: i32) {
        if tid == self.sig_tid {
            return;
        }
        // A new kernel (and a forked child's) holds the state of the one
        // thread it has, whatever its tid: the first thread in adopts it.
        if self.sig_tid == 0 {
            self.sig_tid = tid;
            return;
        }
        let saved = self.signals.save_thread();
        self.thread_signals.insert(self.sig_tid, saved);
        let call = PendingCall {
            retry_accept: self.retry_accept.take(),
            write_blocked: std::mem::take(&mut self.write_blocked),
            retry_recvmsg: self.retry_recvmsg.take(),
            retry_transfer: self.retry_transfer.take(),
            restore_sigmask: self.restore_sigmask.take(),
        };
        if call != PendingCall::default() {
            self.thread_calls.insert(self.sig_tid, call);
        }
        let call = self.thread_calls.remove(&tid).unwrap_or_default();
        self.retry_accept = call.retry_accept;
        self.write_blocked = call.write_blocked;
        self.retry_recvmsg = call.retry_recvmsg;
        self.retry_transfer = call.retry_transfer;
        self.restore_sigmask = call.restore_sigmask;
        let next = self
            .thread_signals
            .remove(&tid)
            .unwrap_or_else(|| crate::signal::ThreadSignals::new_thread(self.signals.blocked));
        self.signals.load_thread(next);
        self.sig_tid = tid;
    }

    /// Whether a signal is ready for thread `tid` (the host's wait loops ask
    /// for the thread that is waiting, which need not be the one whose state
    /// is loaded).
    /// Whether a signal would end thread `tid`'s wait: one that does
    /// something when delivered (a handler, or a default that is not
    /// "ignore"), as the browser host asks (`would_deliver`). An ignored
    /// SIGCHLD from a pipeline's writer exiting must not turn the reader's
    /// blocking read or splice into EINTR, which Linux never does.
    pub fn signal_pending_for(&mut self, tid: i32) -> bool {
        self.collect_signals();
        self.check_timers();
        if tid == self.sig_tid {
            return self.signals.would_deliver();
        }
        match self.thread_signals.get(&tid) {
            Some(t) => t.would_deliver(&self.signals),
            None => self.signals.would_deliver(),
        }
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
            procs: Arc::new(ProcTable::new()),
            pipes: None,
            unix_remote: None,
            ptys: None,
            ppid: 0,
            clock: system_clock,
            itimers: [None; 3],
            retry_accept: None,
            write_blocked: false,
            retry_recvmsg: None,
            retry_transfer: None,
            restore_sigmask: None,
            started: 0,
            pins: std::collections::HashMap::new(),
            place: None,
            armed_place: None,
            syscall_pc: 0,
            thread_signals: std::collections::HashMap::new(),
            thread_calls: std::collections::HashMap::new(),
            sig_tid: 0,
            armed_pin: None,
            compat: false,
            fd_activity: false,
            wasm32: false,
            sigreturn_legacy: false,
            pin_landed: Vec::new(),
            shared_maps: Vec::new(),
            thread_cwd: std::collections::HashMap::new(),
            new_code: Vec::new(),
            retired_code: Vec::new(),
            next_tmpfile: 0,
            umask: 0o022,
            net: None,
            resolver: Arc::new(net::Resolver::new()),
            ports: Arc::new(net::Ports::new()),
            sockets: Default::default(),
            remote_sockets: Default::default(),
            epolls: Default::default(),
            memfds: Default::default(),
            socket_names: Arc::new(unix::SocketNames::new()),
            host_names: Arc::new(unix::SocketNames::new()),
            file_locks: Arc::new(unix::FileLocks::new()),
            tty: Arc::new(crate::tty::TtyRef::new()),
        }
    }

    /// Read one range of a file into `out`, returning bytes filled; the rest
    /// of `out` is left alone. Unlike `read_file`, this serves a caller that
    /// wants only a page of a large file rather than the whole thing.
    pub fn read_file_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Option<usize> {
        let h = self.vfs.open(path, 0, 0).ok()?;
        let mut done = 0usize;
        while done < out.len() {
            let n = h.read_at(offset + done as u64, &mut out[done..]).ok()? as usize;
            if n == 0 {
                break;
            }
            done += n;
        }
        Some(done)
    }

    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let h = self.vfs.open(path, 0, 0).ok()?;
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = h.read_at(out.len() as u64, &mut buf).ok()? as usize;
            if n == 0 {
                return Some(out);
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 512 * 1024 * 1024 {
                return None;
            }
        }
    }

    /// Drop the descriptors marked close-on-exec.
    pub fn close_on_exec(&mut self) {
        self.fds.close_on_exec();
    }

    /// Everything `execve` can fail at before the point of no return.
    ///
    /// The strings have to be read here because the address space holding
    /// them is about to go away, and the file has to be checked here because
    /// a failed `execve` *returns*: the guest sees `-ENOENT` and carries on in
    /// the program it already had. Once the host starts building the new
    /// address space there is nothing to return to.
    fn prepare_exec(
        &mut self,
        m: &GuestMem<'_>,
        a: [u64; 6],
    ) -> Result<(String, Vec<String>, Vec<String>), Errno> {
        let (path, argv, envp) = self.read_exec_args(m, a)?;
        let (path, argv) = self.resolve_interpreter(path, argv)?;
        let image = self.read_file(&path).ok_or(Errno::ENOENT)?;
        if image.len() < 64 || &image[..4] != b"\x7fELF" {
            return Err(Errno::ENOEXEC);
        }
        Ok((path, argv, envp))
    }

    /// Follow a `#!` line, so a script can be executed like a program.
    ///
    /// Linux does this in the kernel, not the shell: `execve` on a file
    /// beginning `#!` runs the named interpreter with the script's path
    /// inserted as its first argument. Without it every interpreted script in
    /// a distribution fails to start, and what the caller sees is not a clear
    /// error but whatever its shell decides to do next -- Debian's
    /// `x11-common` postinst runs a Perl script through debconf, dash got
    /// ENOENT, fell back to reading it as shell, and reported
    /// `use: not found`.
    ///
    /// The line is `#!` then optional spaces, the interpreter, then at most
    /// *one* argument, which is Linux's rule and not an oversight: everything
    /// after the first space is a single argument even if it contains more
    /// spaces. `#!/usr/bin/perl -w` passes `-w`; `#!/usr/bin/env perl -w`
    /// passes `perl -w` as one word, which is why that form does not work on
    /// Linux either.
    ///
    /// Nesting is bounded. A script naming itself would otherwise loop.
    fn resolve_interpreter(
        &mut self,
        mut path: String,
        mut argv: Vec<String>,
    ) -> Result<(String, Vec<String>), Errno> {
        for _ in 0..4 {
            let head = self.read_file(&path).ok_or(Errno::ENOENT)?;
            if head.len() >= 4 && &head[..4] == b"\x7fELF" {
                return Ok((path, argv));
            }
            if head.len() < 2 || &head[..2] != b"#!" {
                return Ok((path, argv));
            }
            // Only the first line, and Linux truncates it at 255 bytes.
            let line_end = head[..head.len().min(255)]
                .iter()
                .position(|&c| c == b'\n')
                .unwrap_or(head.len().min(255));
            let line = String::from_utf8_lossy(&head[2..line_end]);
            let line = line.trim_start();
            let (interp, arg) = match line.find(char::is_whitespace) {
                Some(at) => (&line[..at], line[at..].trim()),
                None => (line, ""),
            };
            if interp.is_empty() {
                return Err(Errno::ENOEXEC);
            }
            // argv[0] as the caller gave it is replaced by the script's path,
            // which is what the interpreter is asked to open.
            let mut next = vec![interp.to_string()];
            if !arg.is_empty() {
                next.push(arg.to_string());
            }
            next.push(path.clone());
            next.extend(argv.into_iter().skip(1));
            argv = next;
            path = self.vfs.absolute(interp);
        }
        Err(Errno::ELOOP)
    }

    /// Read `execve`'s path and its two string vectors out of guest memory,
    /// before the address space holding them goes away.
    fn read_exec_args(
        &mut self,
        m: &GuestMem<'_>,
        a: [u64; 6],
    ) -> Result<(String, Vec<String>, Vec<String>), Errno> {
        let bytes_to_string = |b: Vec<u8>| String::from_utf8(b).map_err(|_| Errno::EINVAL);
        let path = bytes_to_string(m.read_cstr(a[0], 4096)?)?;
        // A 32-bit process hands over vectors of 32-bit pointers.
        let compat = self.compat;
        let read_vec = |mut p: u64| -> Result<Vec<String>, Errno> {
            let mut out = Vec::new();
            // A bound, because the vector is the guest's and its terminator
            // may not be there.
            for _ in 0..4096 {
                if p == 0 {
                    break;
                }
                let ptr = if compat { m.read_u32(p)? as u64 } else { m.read_u64(p)? };
                if ptr == 0 {
                    break;
                }
                out.push(bytes_to_string(m.read_cstr(ptr, 4096)?)?);
                p += if compat { 4 } else { 8 };
            }
            Ok(out)
        };
        let argv = read_vec(a[1])?;
        let envp = read_vec(a[2])?;
        Ok((path, argv, envp))
    }

    /// The kernel a forked child starts with.
    ///
    /// Descriptors carry `Arc` handles, so parent and child share the open
    /// file *descriptions* -- offsets included -- exactly as Linux does, while
    /// the table itself is separate so a `close` in one does not affect the
    /// other. The VMA tree is copied because the child gets its own address
    /// space, and the child list starts empty because a grandchild is not the
    /// parent's to reap.
    pub fn fork_kernel(&self) -> Kernel {
        Kernel {
            fds: self.fds.clone(),
            mem: self.mem.clone(),
            vfs: self.vfs.clone(),
            ctx_addr: self.ctx_addr,
            pid: self.pid,
            tid: self.tid,
            clear_child_tid: 0,
            exe_path: self.exe_path.clone(),
            argv: self.argv.clone(),
            envp: self.envp.clone(),
            uid: self.uid,
            gid: self.gid,
            trace: self.trace,
            // A fork resets pending signals but keeps the dispositions.
            signals: self.signals.fork(),
            // Futexes live in the address space, and the child has its own.
            futexes: Arc::new(FutexTable::new()),
            unimplemented: Vec::new(),
            next_tid: self.next_tid,
            next_ctx_slot: self.next_ctx_slot,
            live_threads: 1,
            // The table is the system's, not the process's.
            procs: Arc::clone(&self.procs),
            pipes: self.pipes,
            unix_remote: self.unix_remote,
            ptys: self.ptys,
            ppid: self.pid,
            clock: self.clock,
            // A fork does not inherit its parent's timers.
            itimers: [None; 3],
            retry_accept: None,
            write_blocked: false,
            retry_recvmsg: None,
            retry_transfer: None,
            restore_sigmask: None,
            // Nor its accumulated time: the child's clock starts now.
            started: self.now_nanos(),
            pins: std::collections::HashMap::new(),
            // The placement outlives any one process: a child that maps the
            // same library must be given the same address its parent was.
            place: self.place.clone(),
            armed_place: None,
            syscall_pc: 0,
            thread_signals: std::collections::HashMap::new(),
            thread_calls: std::collections::HashMap::new(),
            sig_tid: 0,
            armed_pin: None,
            compat: self.compat,
            fd_activity: false,
            wasm32: self.wasm32,
            sigreturn_legacy: false,
            pin_landed: Vec::new(),
            shared_maps: Vec::new(),
            thread_cwd: std::collections::HashMap::new(),
            new_code: Vec::new(),
            retired_code: Vec::new(),
            next_tmpfile: self.next_tmpfile,
            umask: self.umask,
            net: self.net.clone(),
            resolver: Arc::clone(&self.resolver),
            ports: Arc::clone(&self.ports),
            sockets: self.sockets.clone(),
            remote_sockets: self.remote_sockets.clone(),
            epolls: self.epolls.clone(),
            memfds: self.memfds.clone(),
            // Named sockets are system-wide: a forked child must still be
            // able to reach a server its parent could.
            socket_names: Arc::clone(&self.socket_names),
            // Host-served names are not: the child's own shim will bind its own
            // compositor and GPU device over these, and connecting to the
            // parent's would hand a client's surfaces to a process that cannot
            // read them.
            host_names: Arc::new(unix::SocketNames::new()),
            file_locks: Arc::clone(&self.file_locks),
            // The terminal is the system's, not this process's: a child
            // shares it rather than inheriting a copy.
            tty: Arc::clone(&self.tty),
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
    /// A `clone` without `CLONE_THREAD` never reaches here; the new-process
    /// case is handled in `syscall`.
    fn sys_clone(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> Result<SpawnRequest, Errno> {
        let (flags, child_stack, parent_tid, tls, child_tid) = (a[0], a[1], a[2], a[3], a[4]);
        if flags & clone_flags::THREAD == 0 {
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
        if child + ctx::SIZE as u64 > gex_abi::CTX_LIMIT {
            return Err(Errno::EAGAIN);
        }

        let parent = self.ctx_addr;
        for off in (0..ctx::SIZE).step_by(8) {
            let v = m.read_u64(parent + off as u64)?;
            m.write_u64(child + off as u64, v)?;
        }
        set_result(m, child, 0)?;
        m.write_u64(child + ctx::SP as u64, child_stack)?;
        m.write_u64(child + ctx::MON_SIZE as u64, 0)?;
        m.write_u64(child + ctx::TID as u64, tid as u64)?;
        if flags & clone_flags::SETTLS != 0 {
            // A 32-bit thread's TLS descriptor is loaded into %gs, a 64-bit
            // one's base into %fs.
            // PowerPC's thread pointer is r2.
            let slot = if gex_abi::PPC {
                ctx::x(2)
            } else if self.compat {
                ctx::TPIDRRO
            } else {
                ctx::TPIDR
            };
            m.write_u64(child + slot as u64, tls)?;
        }
        if flags & clone_flags::PARENT_SETTID != 0 && parent_tid != 0 {
            m.write_u32(parent_tid, tid as u32)?;
        }
        if flags & clone_flags::CHILD_SETTID != 0 && child_tid != 0 {
            m.write_u32(child_tid, tid as u32)?;
        }
        self.live_threads += 1;
        // The new thread's signal state: its creator's mask, nothing pending.
        self.thread_signals.insert(tid, crate::signal::ThreadSignals::new_thread(self.signals.blocked));
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
        m.read_u64(self.ctx_addr + ctx::SYSCALL_ARGS[i as usize] as u64)
            .unwrap_or(0)
    }

    /// Handle one system call (`svc #0`, or `syscall`).
    pub fn syscall(&mut self, m: &mut GuestMem<'_>, pc: u64) -> SyscallOutcome {
        // A thread with a working directory of its own (`unshare(CLONE_FS)`)
        // has it in `vfs` while its call runs.
        if self.thread_cwd.is_empty() {
            return self.syscall_inner(m, pc);
        }
        let me = self.ctx_addr;
        let Some(mut cwd) = self.thread_cwd.remove(&me) else {
            return self.syscall_inner(m, pc);
        };
        std::mem::swap(&mut self.vfs.cwd, &mut cwd);
        let out = self.syscall_inner(m, pc);
        std::mem::swap(&mut self.vfs.cwd, &mut cwd);
        self.thread_cwd.insert(me, cwd);
        out
    }

    fn syscall_inner(&mut self, m: &mut GuestMem<'_>, pc: u64) -> SyscallOutcome {
        self.syscall_pc = pc;
        let raw = m
            .read_u64(self.ctx_addr + ctx::SYSCALL_NR as u64)
            .unwrap_or(!0);
        let mut a: [u64; 6] = if self.compat && !gex_abi::PPC {
            let mut a = [0u64; 6];
            for (i, slot) in crate::i386::ARG_SLOTS.iter().enumerate() {
                a[i] = m.read_u64(self.ctx_addr + *slot as u64).unwrap_or(0) & 0xffff_ffff;
            }
            a
        } else {
            [
                self.arg(m, 0),
                self.arg(m, 1),
                self.arg(m, 2),
                self.arg(m, 3),
                self.arg(m, 4),
                self.arg(m, 5),
            ]
        };
        // The number's register doubles as the result register on x86-64, so
        // keep the raw number where a host can read it after the fact.
        if gex_abi::X86 {
            let _ = m.write_u64(self.ctx_addr + ctx::LAST_SYSCALL as u64, raw);
        }
        // An x86-64 guest's numbering is translated to the canonical one, and
        // its legacy calls rewritten into the `*at`/extended forms.
        let mut post = crate::i386::Post::None;
        let mut ppc_post = crate::ppc::PpcPost::None;
        let n = if gex_abi::PPC {
            let (n, p, pp) = crate::ppc::translate(
                raw & 0xffff_ffff,
                &mut a,
                m,
                self.ctx_addr + ctx::SCRATCH as u64,
                self.ctx_addr,
            );
            post = p;
            ppc_post = pp;
            n
        } else if gex_abi::X86 && self.compat {
            let (n, p) = crate::i386::translate(
                raw & 0xffff_ffff,
                &mut a,
                m,
                self.ctx_addr + ctx::SCRATCH as u64,
                self.ctx_addr,
            );
            post = p;
            self.sigreturn_legacy = (raw & 0xffff_ffff) == crate::i386::raw::SIGRETURN;
            n
        } else if gex_abi::X86 {
            crate::x86::translate(
                raw,
                &mut a,
                m,
                self.ctx_addr + ctx::SCRATCH as u64,
                self.signals.blocked,
            )
        } else {
            raw
        };

        self.fd_activity |= wakes_fd_waiters(n);

        // Returning from a handler restores a whole context, so it cannot go
        // through the normal "write x0 and continue" path.
        if n == nr::RT_SIGRETURN {
            return match self.sigreturn(m) {
                Ok(pc) => {
                    if self.trace {
                        eprintln!(
                            "[gex:{}.{}] rt_sigreturn -> 0x{pc:x}, blocked {:#x}",
                            self.pid, self.tid, self.signals.blocked
                        );
                    }
                    // The restored context is in ctx, PC included.
                    if self.wasm32 {
                        return SyscallOutcome::Continue;
                    }
                    SyscallOutcome::Resume(pc)
                }
                Err(_) => SyscallOutcome::Killed {
                    signal: signal::sig::SIGSEGV,
                    core: true,
                },
            };
        }

        // Someone has halted the machine. Every process notices here and
        // stops, however deep in its own work it was.
        if let Some(code) = self.procs.halt_code() {
            return SyscallOutcome::Exit(code);
        }
        // Anything another process has sent becomes pending here, at the one
        // point where this process is definitely holding its own signal state
        // and not running guest code.
        self.take_sent_signals();
        // `reboot` is two syscalls wearing one number. The shutdown commands
        // do not return -- there is one machine here and it is this process,
        // so halting it and exiting are the same act -- but the Ctrl-Alt-Del
        // commands are ordinary settings, and busybox init issues `CAD_OFF`
        // as its first act. Treating that as a shutdown ends the boot on line
        // one, which is exactly what it did.
        if n == nr::REBOOT {
            const RESTART: u64 = 0x0123_4567;
            const HALT: u64 = 0xcdef_0123;
            const POWER_OFF: u64 = 0x4321_fedc;
            const RESTART2: u64 = 0xa1b2_c3d4;
            let what = match a[2] {
                RESTART | RESTART2 => "restart",
                HALT => "halt",
                POWER_OFF => "power off",
                // CAD_OFF, CAD_ON, SW_SUSPEND, KEXEC: nothing to do, and a
                // failure would be reported by an init that can carry on.
                _ => {
                    let _ = set_result(m, self.ctx_addr, 0);
                    return SyscallOutcome::Continue;
                }
            };
            if self.trace {
                eprintln!("[gex:{}] reboot({what})", self.pid);
            }
            self.procs.request_halt(0);
            return SyscallOutcome::Exit(0);
        }
        if n == nr::EXECVE {
            return match self.prepare_exec(m, a) {
                Ok((path, argv, envp)) => {
                    if self.trace {
                        eprintln!("[gex:{}] execve({path:?}, {} args)", self.pid, argv.len());
                    }
                    SyscallOutcome::Exec { path, argv, envp }
                }
                Err(e) => {
                    let _ = set_result(m, self.ctx_addr, e.as_return());
                    SyscallOutcome::Continue
                }
            };
        }
        // `clone3` (435): glibc >= 2.34 uses it for `pthread_create` (and
        // `posix_spawn`), so without it every threaded program dies at the first
        // thread with EFAULT. It passes a `struct clone_args *` + size rather
        // than registers; read the fields and route through the same paths as
        // `clone`. clone3's `stack` is the allocation base and `stack_size` its
        // length, but `clone`/our thread setup want the *top* (the SP, which
        // grows down), so hand on `stack + stack_size`.
        if n == nr::CLONE3 {
            let c = a[0];
            // struct clone_args: flags@0, child_tid@16, parent_tid@24,
            // stack@40, stack_size@48, tls@56 (aligned u64s).
            let fields = (|| {
                Some((
                    m.read_u64(c).ok()?,
                    m.read_u64(c + 16).ok()?,
                    m.read_u64(c + 24).ok()?,
                    m.read_u64(c + 40).ok()?,
                    m.read_u64(c + 48).ok()?,
                    m.read_u64(c + 56).ok()?,
                ))
            })();
            let (flags, child_tid, parent_tid, stack, stack_size, tls) = match fields {
                Some(t) => t,
                None => {
                    let _ =
                        set_result(m, self.ctx_addr, Errno::EFAULT.as_return());
                    return SyscallOutcome::Continue;
                }
            };
            let sp = stack.wrapping_add(stack_size);
            // An i386 caller's `tls` is a `struct user_desc *`, as for its
            // `clone` (`crate::i386`): the base is its second word.
            let tls = if self.compat && !gex_abi::PPC && flags & clone_flags::SETTLS != 0 && tls != 0 {
                m.read_u32(tls + 4).map(|b| b as u64).unwrap_or(0)
            } else {
                tls
            };
            if flags & clone_flags::THREAD == 0 {
                let pid = self.procs.spawn(self.pid);
                let _ = set_result(m, self.ctx_addr, pid as u64);
                return SyscallOutcome::SpawnProcess {
                    pid,
                    child_stack: sp,
                    tls: (flags & clone_flags::SETTLS != 0).then_some(tls),
                };
            }
            let ca = [flags, sp, parent_tid, tls, child_tid, 0];
            return match self.sys_clone(m, ca) {
                Ok(req) => {
                    let _ = set_result(m, self.ctx_addr, req.tid as u64);
                    SyscallOutcome::SpawnThread {
                        ctx_addr: req.ctx_addr,
                        tid: req.tid,
                        clear_child_tid: req.clear_child_tid,
                    }
                }
                Err(e) => {
                    let _ = set_result(m, self.ctx_addr, e.as_return());
                    SyscallOutcome::Continue
                }
            };
        }
        // A clone without CLONE_THREAD is a new process, which only the host
        // can build: it needs a second address space.
        if n == nr::CLONE && a[0] & clone_flags::THREAD == 0 {
            let pid = self.procs.spawn(self.pid);
            // The parent sees the child's pid; the child sees zero, which the
            // host writes into the copy it makes.
            let _ = set_result(m, self.ctx_addr, pid as u64);
            if self.trace {
                eprintln!(
                    "[gex] clone(flags={:#x}, stack={:#x}) = {pid} (new process)",
                    a[0], a[1]
                );
            }
            return SyscallOutcome::SpawnProcess {
                pid,
                child_stack: a[1],
                tls: (a[0] & clone_flags::SETTLS != 0).then_some(a[3]),
            };
        }
        if n == nr::CLONE {
            return match self.sys_clone(m, a) {
                Ok(req) => {
                    let _ = set_result(m, self.ctx_addr, req.tid as u64);
                    if self.trace {
                        eprintln!("[gex:{}] clone(...) = {} (new thread)", self.pid, req.tid);
                    }
                    SyscallOutcome::SpawnThread {
                        ctx_addr: req.ctx_addr,
                        tid: req.tid,
                        clear_child_tid: req.clear_child_tid,
                    }
                }
                Err(e) => {
                    let _ = set_result(m, self.ctx_addr, e.as_return());
                    if self.trace {
                        eprintln!("[gex:{}] clone(...) = {}", self.pid, -(e.0 as i64));
                    }
                    SyscallOutcome::Continue
                }
            };
        }
        // `exit` ends one thread; only `exit_group` ends the process. For the
        // last thread the two are the same thing.
        if n == nr::EXIT && self.live_threads > 1 {
            if self.trace {
                eprintln!("[gex:{}] exit({}) (thread {})", self.pid, a[0] as i32, self.tid);
            }
            self.thread_exit(m);
            // Its signal state goes with it; the next thread to enter loads
            // its own.
            self.thread_signals.remove(&self.tid);
            self.thread_calls.remove(&self.tid);
            return SyscallOutcome::ThreadExit;
        }

        // A syscall that sleeps must do so with the kernel unlocked, or the
        // thread that would wake it can never get in.
        if let Some(op) = self.blocking_op(m, n, a) {
            if self.trace {
                // A parked `poll` says what it is parked on: which
                // descriptors, and what it wants from each. Finding out that
                // apt was waiting to read from a worker it had nothing
                // queued for was the whole of one afternoon.
                if let BlockOp::Poll { fds, .. } = &op {
                    let want: Vec<String> = fds
                        .iter()
                        .map(|p| format!("{}:{:x}", p.fd, p.events))
                        .collect();
                    eprintln!("[gex:{}] {} blocking on [{}]", self.pid, syscall_name(n), want.join(" "));
                } else {
                    eprintln!("[gex:{}.{}] {} blocking", self.pid, self.tid, syscall_name(n));
                }
            }
            self.write_blocked = matches!(op, BlockOp::WriteStream { .. });
            return SyscallOutcome::Blocking(op);
        }

        if self.trace && n == u64::MAX {
            // The raw number, since the canonical one below says only "unknown".
            eprintln!(
                "[gex:{}] raw syscall {} ({}) has no canonical form",
                self.pid,
                raw & 0xffff_ffff,
                if self.compat { "i386" } else { "native" }
            );
        }
        // An x86-64 number with no mapping reaches `dispatch` as `UNKNOWN`;
        // record the guest's own number, which is the one worth looking up.
        let outcome = if (gex_abi::X86 || gex_abi::PPC) && n == crate::x86::UNKNOWN {
            let before = self.unimplemented.len();
            let r = self.dispatch(m, n, a);
            if self.unimplemented.len() > before {
                self.unimplemented.pop();
                if !self.unimplemented.contains(&raw) {
                    self.unimplemented.push(raw);
                }
            }
            r
        } else {
            self.dispatch(m, n, a)
        };
        if let Ok(Err(Errno::EPIPE)) = &outcome {
            if raises_sigpipe(n, &a) {
                self.signals.raise_from(
                    crate::signal::sig::SIGPIPE,
                    // Linux sends it as the writer itself sending it (SI_USER).
                    crate::signal::Source { code: crate::signal::SI_USER, pid: self.pid, uid: self.uid },
                );
            }
        }
        let ret = match &outcome {
            Ok(r) => match r {
                Ok(v) if post != crate::i386::Post::None => encode(Ok(post.finish(m, *v))),
                Ok(v) if ppc_post != crate::ppc::PpcPost::None => {
                    encode(Ok(crate::ppc::finish(ppc_post, *v)))
                }
                _ => encode(*r),
            },
            Err(exit) => {
                if self.trace {
                    eprintln!("[gex:{}] exit_group({exit})", self.pid);
                }
                return SyscallOutcome::Exit(*exit);
            }
        };
        if self.trace {
            eprintln!(
                "[gex:{}{}] pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}){} = {}",
                self.pid,
                // The thread, when it is not the process's first.
                if self.tid != self.pid { format!(".{}", self.tid) } else { String::new() },
                syscall_name(n),
                a[0],
                a[1],
                a[2],
                traced_path(m, n, a),
                ret as i64
            );
            // `GEX_STRACE_STACK=N`: the top N stack words too, which in a
            // libc syscall wrapper start with the caller's return address --
            // enough to tell who made a call without a debugger.
            static STACK_WORDS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            let words = *STACK_WORDS.get_or_init(|| {
                std::env::var("GEX_STRACE_STACK").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
            });
            if words > 0 {
                if let Ok(sp) = m.read_u64(self.ctx_addr + ctx::SP as u64) {
                    let ws: Vec<String> = (0..words)
                        .filter_map(|i| m.read_u64(sp + 8 * i).ok())
                        .map(|w| format!("{w:#x}"))
                        .collect();
                    eprintln!("[gex:{}]   sp={sp:#x}: {}", self.pid, ws.join(" "));
                }
            }
        }
        let _ = set_result(m, self.ctx_addr, ret);

        // A syscall boundary is a safepoint: the context is complete, so a
        // pending signal can be delivered precisely here.
        match self.deliver_pending(m) {
            Delivery::None => SyscallOutcome::Continue,
            // The frame is in ctx (PC = the handler): the stub runs it.
            Delivery::Resume(_) if self.wasm32 => SyscallOutcome::Continue,
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
            // `rt_sigsuspend`: replace the blocked mask, wait for a signal
            // the caller can now see, put the mask back.
            //
            // The swap and the wait have to look atomic to the guest -- that
            // is the entire reason the call exists instead of a
            // `sigprocmask` followed by a `pause` -- so the mask changes
            // here, before the kernel is unlocked, and is restored in
            // `finish_blocking`.
            //
            // Without it, `timeout 3 sleep 30` ran for the full thirty
            // seconds and then reported success: musl saw `ENOSYS` and spun,
            // 1.6 million syscalls' worth, until the child ended by itself.
            nr::RT_SIGSUSPEND => {
                let want = m.read_bits64(a[0]).ok()?;
                let restore_mask = self.signals.blocked;
                // Neither `SIGKILL` nor `SIGSTOP` can be blocked, here or
                // anywhere else.
                const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));
                self.signals.blocked = want & !UNBLOCKABLE;
                self.restore_sigmask = Some(restore_mask);
                Some(BlockOp::SigSuspend { restore_mask })
            }
            nr::CONNECT => {
                let bytes = m.read(a[1], (a[2] as usize).min(128)).ok()?.to_vec();
                let (ip, port) = net::parse_sockaddr(&bytes).ok()?;
                let file = self.fds.get(a[0] as i32).ok()?;
                let socket = self.socket_of(a[0] as i32).ok()?;
                // Start it here, whichever way this goes: the non-blocking
                // path reaches `sys_connect`, which finds the connection
                // already under way and only reports on it.
                let started = socket.connect(ip, port);
                let waits = file.would_block() && started == Err(Errno::EINPROGRESS);
                waits.then_some(BlockOp::Connect { socket })
            }
            // `epoll_pwait` waits, and a version of it that always returned
            // at once turned every event loop into a spin. Xwayland's is the
            // one that showed it: it made progress, but only when the timing
            // happened to suit it.
            nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2 => {
                let set = self.epoll_of(a[0] as i32).ok()?;
                let fds = self.epoll_pollfds(&set);
                if poll_ready(&fds) {
                    return None;
                }
                // `epoll_pwait` takes the timeout as a millisecond count with a
                // negative for "forever"; `epoll_pwait2` takes a `timespec`
                // pointer instead, null for "forever". Both fold to the same
                // deadline, and a zero timeout is a non-blocking poll either way.
                let deadline = if n == nr::EPOLL_PWAIT2 {
                    if a[3] == 0 {
                        None // null timespec: wait forever
                    } else {
                        let secs = m.read_u64(a[3]).ok()?;
                        let nanos = m.read_u64(a[3] + 8).ok()?;
                        if secs == 0 && nanos == 0 {
                            return None; // {0,0}: non-blocking
                        }
                        Some(std::time::Duration::new(secs, (nanos % 1_000_000_000) as u32))
                    }
                } else {
                    let millis = a[3] as i32;
                    if millis == 0 {
                        return None;
                    }
                    if millis < 0 {
                        None
                    } else {
                        Some(std::time::Duration::from_millis(millis as u64))
                    }
                };
                Some(BlockOp::Poll {
                    fds,
                    deadline,
                    report: PollReport::Epoll {
                        out: a[1],
                        max: a[2] as u32,
                        data: set.watches().iter().map(|w| (w.fd, w.data)).collect(),
                    },
                })
            }
            nr::PPOLL | nr::PSELECT6 => {
                let (fds, report, timeout) = if n == nr::PPOLL {
                    (
                        self.collect_pollfds(m, a[0], a[1])?,
                        PollReport::Poll,
                        a[2],
                    )
                } else {
                    (
                        self.collect_selectfds(m, a[0] as u32, a[1], a[2], a[3])?,
                        PollReport::Select {
                            nfds: a[0] as u32,
                            readfds: a[1],
                            writefds: a[2],
                            exceptfds: a[3],
                        },
                        a[4],
                    )
                };
                if poll_ready(&fds) {
                    return None;
                }
                // A zero timeout is a question, not a wait.
                let deadline = if timeout == 0 {
                    None
                } else {
                    let secs = m.read_u64(timeout).ok()?;
                    let nanos = m.read_u64(timeout + 8).ok()?;
                    if secs == 0 && nanos == 0 {
                        return None;
                    }
                    Some(std::time::Duration::new(secs, nanos as u32))
                };
                Some(BlockOp::Poll {
                    fds,
                    deadline,
                    report,
                })
            }
            nr::WRITE => {
                let file = self.fds.get(a[0] as i32).ok()?;
                if !file.write_is_atomic() {
                    return None;
                }
                // A handle that can report its room takes the direct path
                // when the write fits (the common case). A pipe cannot --
                // another Worker can change the answer between the claim and
                // the write, so `write_room` returns `None` -- so when the
                // room is unknown, park only if the descriptor is not writable
                // at all; writable takes the short count, which callers retry.
                // Getting this wrong sent a full-pipe write straight through
                // to `EAGAIN`, truncating a `gzip -dc | sha256sum` stream.
                match file.handle.write_room() {
                    Some(room) if a[2] <= room => return None,
                    Some(_) => {}
                    None if !file.write_would_block() => return None,
                    None => {}
                }
                Some(BlockOp::WriteStream {
                    handle: Arc::clone(&file.handle),
                    buf: a[1],
                    len: a[2].min(1 << 24) as usize,
                })
            }
            nr::ACCEPT | nr::ACCEPT4 => {
                let file = self.fds.get(a[0] as i32).ok()?;
                let socket: Arc<dyn crate::vfs::FileHandle> = Arc::clone(&file.handle);
                if !file.would_block() || socket.poll_readable() {
                    return None;
                }
                let flags = if n == nr::ACCEPT4 { a[3] } else { 0 };
                self.retry_accept = Some((a[0] as i32, a[1], a[2], flags));
                Some(BlockOp::AcceptWait { socket })
            }
            nr::READ => {
                let file = self.fds.get(a[0] as i32).ok()?;
                if !file.would_block() {
                    return None;
                }
                Some(BlockOp::ReadStream {
                    handle: Arc::clone(&file.handle),
                    buf: a[1],
                    len: a[2].min(1 << 24) as usize,
                })
            }
            // A transfer whose input is an empty blocking stream (`cat f |
            // grep x`: GNU grep splices its stdin pipe) waits for it to be
            // readable and then does the real transfer (`retry_transfer`).
            nr::SPLICE | nr::SENDFILE | nr::COPY_FILE_RANGE => {
                let (fd, off) = match n {
                    nr::SENDFILE => (a[1] as i32, a[2]),
                    _ => (a[0] as i32, a[1]),
                };
                let file = self.fds.get(fd).ok()?;
                if off != 0 || !file.would_block() || file.handle.poll_readable() {
                    return None;
                }
                self.retry_transfer = Some((n, a));
                Some(BlockOp::AcceptWait { socket: Arc::clone(&file.handle) })
            }
            // `recvfrom` (and `recv`) on an empty blocking socket waits for
            // it to be readable, then reads under the lock, as `recvmsg`
            // does (by `retry_transfer`). It had answered EAGAIN.
            nr::RECVFROM => {
                let file = self.fds.get(a[0] as i32).ok()?;
                if a[3] & Self::MSG_DONTWAIT != 0 || !file.would_block() {
                    return None;
                }
                self.retry_transfer = Some((n, a));
                Some(BlockOp::AcceptWait {
                    socket: Arc::clone(&file.handle),
                })
            }
            nr::RECVMSG => {
                let file = self.fds.get(a[0] as i32).ok()?;
                // A non-blocking socket, one with data, or a call that asked
                // for `MSG_DONTWAIT` does not wait. libwayland reads its
                // blocking socket with `MSG_DONTWAIT` and expects `EAGAIN`
                // once it has drained it; waiting here instead hangs the
                // client in its first round-trip.
                if a[2] & Self::MSG_DONTWAIT != 0 || !file.would_block() {
                    return None;
                }
                // Wait until it is readable, then do the real recvmsg under
                // the lock (see `retry_recvmsg`), the way `accept` does.
                self.retry_recvmsg = Some((a[0] as i32, a[1], a[2]));
                Some(BlockOp::AcceptWait {
                    socket: Arc::clone(&file.handle),
                })
            }
            nr::FUTEX => {
                let operation =
                    (a[1] as u32) & !(futex::op::PRIVATE_FLAG | futex::op::CLOCK_REALTIME);
                if operation == futex::op::LOCK_PI || operation == futex::op::LOCK_PI2 {
                    let mapped = self.mem.vmas().iter().any(|v| a[0] >= v.start && a[0] + 4 <= v.end);
                    if !mapped || a[0] % 4 != 0 {
                        return None;
                    }
                    // An absolute time: CLOCK_REALTIME for LOCK_PI, the
                    // monotonic clock for LOCK_PI2 (unless FUTEX_CLOCK_REALTIME).
                    let deadline = if a[3] == 0 {
                        None
                    } else {
                        let secs = m.read_u64(a[3]).ok()?;
                        let nanos = m.read_u64(a[3] + 8).ok()?;
                        let want = secs as u128 * 1_000_000_000 + (nanos % 1_000_000_000) as u128;
                        let monotonic = operation == futex::op::LOCK_PI2 && (a[1] as u32) & futex::op::CLOCK_REALTIME == 0;
                        let now = if monotonic { self.clock_nanos(1) } else { self.now_nanos() };
                        let left = want.saturating_sub(now).min(3600 * 1_000_000_000);
                        Some(std::time::Duration::new((left / 1_000_000_000) as u64, (left % 1_000_000_000) as u32))
                    };
                    return Some(BlockOp::FutexLockPi {
                        table: Arc::clone(&self.futexes),
                        uaddr: a[0],
                        tid: self.tid as u32,
                        deadline,
                    });
                }
                if operation != futex::op::WAIT && operation != futex::op::WAIT_BITSET {
                    return None;
                }
                // A futex word outside the process's memory map is `EFAULT`,
                // not a wait: the wake would have to come from an address
                // nothing can write. Checked against the map, not with
                // `GuestMem::read` -- a guest address is a linear-memory
                // offset, so reading address zero succeeds and proves nothing
                // (#113).
                if !self.mem.vmas().iter().any(|v| a[0] >= v.start && a[0] + 4 <= v.end) {
                    return None;
                }
                // Take the queue place BEFORE reading the word. A cooperative
                // host (the browser) parks by snapshotting the generation and
                // polling it; if it snapshotted AFTER the check, a `FUTEX_WAKE`
                // from another thread's Worker landing between the check and the
                // snapshot would bump the generation first and the park would
                // baseline against the already-bumped value -- the wake lost,
                // the thread asleep forever (Mesa's util_queue deadlocked here).
                // Snapshotting first means such a wake either changed the word
                // (caught just below as EAGAIN) or bumped a generation the park
                // will see. The native host holds the queue lock across its
                // check-and-sleep instead and ignores this.
                let parked = self.futexes.park(a[0]);
                // The word must still hold what the caller expects, or the
                // wake that changed it has already been and gone and the wait
                // would be permanent. A mismatch declines the wait (EAGAIN via
                // `sys_futex`).
                if m.read_u32(a[0]).ok()? != a[2] as u32 {
                    return None;
                }
                let deadline = if a[3] == 0 {
                    None
                } else {
                    let secs = m.read_u64(a[3]).ok()?;
                    let nanos = m.read_u64(a[3] + 8).ok()?;
                    let want = secs as u128 * 1_000_000_000 + (nanos % 1_000_000_000) as u128;
                    // `FUTEX_WAIT_BITSET` names an absolute moment: on
                    // CLOCK_REALTIME with `FUTEX_CLOCK_REALTIME`, else on
                    // CLOCK_MONOTONIC (what pthread_cond_clockwait and C++'s
                    // steady-clock waits use). Plain `FUTEX_WAIT` really is
                    // relative.
                    let left = if operation == futex::op::WAIT_BITSET {
                        let now = if (a[1] as u32) & futex::op::CLOCK_REALTIME != 0 {
                            self.now_nanos()
                        } else {
                            self.clock_nanos(1)
                        };
                        want.saturating_sub(now)
                    } else {
                        want
                    };
                    let left = left.min(3600 * 1_000_000_000);
                    Some(std::time::Duration::new(
                        (left / 1_000_000_000) as u64,
                        (left % 1_000_000_000) as u32,
                    ))
                };
                Some(BlockOp::FutexWait {
                    table: Arc::clone(&self.futexes),
                    uaddr: a[0],
                    val: a[2] as u32,
                    deadline,
                    parked: Some(parked),
                })
            }
            nr::WAIT4 => {
                // Children come from the table, not from a list this process
                // owns, so a child reparented here when its own parent died
                // is one this `wait4` can collect. That is what makes pid 1
                // able to do its job.
                Some(BlockOp::WaitChild {
                    // Nothing to hand out when the table is elsewhere; the
                    // host asks it directly. See `BlockOp::WaitChild`.
                    children: if self.procs.is_shared() {
                        Vec::new()
                    } else {
                        self.procs.children(self.pid)
                    },
                    procs: Arc::clone(&self.procs),
                    ppid: self.pid,
                    want: a[0] as i32,
                    wstatus: a[1],
                    nohang: a[2] & 1 != 0,
                })
            }
            nr::NANOSLEEP | nr::CLOCK_NANOSLEEP => {
                // `clock_nanosleep` puts the request in a[2]; `nanosleep` in
                // a[0]. `None` leaves the call to `sys_nanosleep`, which
                // answers at once: EINVAL for a malformed request, 0 for
                // one with nothing left to wait for.
                let req = if n == nr::NANOSLEEP { a[0] } else { a[2] };
                if req == 0 {
                    return None;
                }
                let secs = m.read_u64(req).ok()? as i64;
                let nanos = m.read_u64(req + 8).ok()?;
                if secs < 0 || nanos >= 1_000_000_000 {
                    return None;
                }
                let mut wait = secs as u128 * 1_000_000_000 + nanos as u128;
                // TIMER_ABSTIME: a deadline on the clock, not a duration. It
                // was slept as a duration, so every absolute sleep lasted the
                // 60 s cap below.
                const TIMER_ABSTIME: u64 = 1;
                if n == nr::CLOCK_NANOSLEEP && a[1] & TIMER_ABSTIME != 0 {
                    wait = wait.checked_sub(self.clock_nanos(a[0])).filter(|&w| w > 0)?;
                }
                // Capped, as before: a longer sleep returns early.
                let wait = wait.min(60_000_000_000) as u64;
                Some(BlockOp::Sleep(std::time::Duration::from_nanos(wait)))
            }
            _ => None,
        }
    }

    /// Finish a syscall whose blocking half the host has now run.
    /// Whether a syscall since the last call may have changed descriptor
    /// readiness ([`Kernel::fd_activity`]), clearing it.
    pub fn take_fd_activity(&mut self) -> bool {
        std::mem::take(&mut self.fd_activity)
    }

    pub fn finish_blocking(&mut self, m: &mut GuestMem<'_>, r: SysResult) -> SyscallOutcome {
        // An `accept` that waited does its real work here, now that the
        // kernel is locked again and there is a descriptor table to put the
        // new socket in.
        let r = match self.retry_accept.take() {
            Some((fd, addr, len, flags)) if r.is_ok() => {
                self.sys_accept(m, fd, addr, len, flags)
            }
            _ => r,
        };
        let r = match self.retry_recvmsg.take() {
            Some((fd, msg, flags)) if r.is_ok() => self.sys_recvmsg(m, fd, msg, flags),
            _ => r,
        };
        let r = match self.retry_transfer.take() {
            Some((n, a)) if r.is_ok() => self.transfer_syscall(m, n, &a),
            _ => r,
        };
        // `rt_sigsuspend` waited with a mask of the caller's choosing. The
        // old mask goes back *after* the handler runs, which means the frame
        // has to carry it -- restoring it here instead would block the very
        // signal that ended the wait, leaving it pending forever.
        let suspended = self.restore_sigmask.take();
        if std::mem::take(&mut self.write_blocked) && r == Err(Errno::EPIPE) {
            self.signals.raise_from(
                crate::signal::sig::SIGPIPE,
                // Linux sends it as the writer itself sending it (SI_USER).
                crate::signal::Source { code: crate::signal::SI_USER, pid: self.pid, uid: self.uid },
            );
        }
        let ret = encode(r);
        let _ = set_result(m, self.ctx_addr, ret);
        if let Some(mask) = suspended {
            self.signals.suspend_restore = Some(mask);
        }
        let delivery = self.deliver_pending(m);
        // Nothing was delivered, so no frame took the mask: put it back here.
        if let Some(mask) = self.signals.suspend_restore.take() {
            self.signals.blocked = mask;
        }
        if self.trace {
            eprintln!(
                "[gex:{}.{}] after blocking: delivery {:?}, pending {:#x}, blocked {:#x}",
                self.pid,
                self.tid,
                delivery,
                self.signals.pending_raw(),
                self.signals.blocked
            );
        }
        match delivery {
            Delivery::None => SyscallOutcome::Continue,
            Delivery::Resume(_) if self.wasm32 => SyscallOutcome::Continue,
            Delivery::Resume(pc) => SyscallOutcome::Resume(pc),
            Delivery::Terminate { signal, core } => SyscallOutcome::Killed { signal, core },
        }
    }

    /// The dispatch table. `Err(code)` means the process is exiting.
    fn dispatch(&mut self, m: &mut GuestMem<'_>, n: u64, a: [u64; 6]) -> Result<SysResult, i32> {
        let r: SysResult = match n {
            nr::EXIT | nr::EXIT_GROUP => return Err((a[0] & 0xff) as i32),

            // -- x86-64 legacy forms (see `crate::x86`) ----------------------------
            // Only `x86::translate` produces these numbers; an AArch64 guest
            // issuing one raw falls through to ENOSYS below.
            nr::DUP2 if gex_abi::X86 || gex_abi::PPC => self.sys_dup2(a[0] as i32, a[1] as i32),
            nr::TIME if gex_abi::X86 || gex_abi::PPC => self.sys_time(m, a[0]),
            nr::ALARM if gex_abi::X86 || gex_abi::PPC => self.sys_alarm(m, a[0]),
            nr::ARCH_PRCTL if gex_abi::X86 => self.sys_arch_prctl(m, a[0], a[1]),
            nr::NOOP_OK if gex_abi::X86 || gex_abi::PPC => Ok(0),

            // -- I/O ---------------------------------------------------------
            nr::WRITE => self.sys_write(m, a[0] as i32, a[1], a[2]),
            nr::READ => self.sys_read(m, a[0] as i32, a[1], a[2]),
            nr::WRITEV => self.sys_writev(m, a[0] as i32, a[1], a[2]),
            nr::READV => self.sys_readv(m, a[0] as i32, a[1], a[2]),
            nr::PREAD64 => self.sys_pread(m, a[0] as i32, a[1], a[2], a[3]),
            nr::PWRITE64 => self.sys_pwrite(m, a[0] as i32, a[1], a[2], a[3]),
            nr::SENDFILE | nr::COPY_FILE_RANGE | nr::SPLICE => self.transfer_syscall(m, n, &a),
            nr::OPENAT => self.sys_openat(m, a[0] as i32, a[1], a[2], a[3] as u32),
            nr::OPENAT2 => self.sys_openat2(m, a[0] as i32, a[1], a[2], a[3]),
            nr::CLOSE => self.sys_close(a[0] as i32),
            nr::LSEEK => self
                .fds
                .get(a[0] as i32)
                .and_then(|f| f.seek(a[1] as i64, a[2] as u32)),
            nr::DUP => self.sys_dup(a[0] as i32),
            nr::DUP3 => self.sys_dup3(a[0] as i32, a[1] as i32, a[2]),
            nr::PIPE2 => self.sys_pipe2(m, a[0], a[1]),
            nr::EVENTFD2 => self.sys_eventfd2(a[0], a[1]),
            nr::TIMERFD_CREATE => self.sys_timerfd_create(a[0], a[1]),
            nr::TIMERFD_SETTIME => self.sys_timerfd_settime(m, a[0] as i32, a[1], a[2], a[3]),
            nr::TIMERFD_GETTIME => self.sys_timerfd_gettime(m, a[0] as i32, a[1]),
            nr::GETDENTS64 => self.sys_getdents(m, a[0] as i32, a[1], a[2]),
            nr::FSTAT => self.sys_fstat(m, a[0] as i32, a[1]),
            nr::NEWFSTATAT => self.sys_fstatat(m, a[0] as i32, a[1], a[2], a[3]),
            nr::STATX => self.sys_statx(m, a[0] as i32, a[1], a[2], a[4]),
            // F_GETLK and F_OFD_GETLK: nobody holds a lock (see
            // `sys_fcntl`), so the answer is l_type = F_UNLCK, which is the
            // first field in every `struct flock` layout (i386's flock and
            // flock64, x86-64's, wasm32's).
            nr::FCNTL if a[1] == 5 || a[1] == 36 => self.fds.get(a[0] as i32).and_then(|_| {
                const F_UNLCK: u16 = 2;
                m.write(a[2], &F_UNLCK.to_le_bytes()).map(|_| 0)
            }),
            nr::FCNTL => self.sys_fcntl(a[0] as i32, a[1], a[2]),
            nr::IOCTL => self.sys_ioctl(m, a[0] as i32, a[1], a[2]),
            nr::FTRUNCATE => self
                .fds
                .get(a[0] as i32)
                .and_then(|f| f.handle.truncate(a[1]).map(|_| 0)),
            // `fdatasync` like `fsync`: everything is already durable (the VFS
            // is in memory), so validating the fd and returning success is the
            // honest answer. Missing it made sqlite's journal sync return
            // ENOSYS, which sqlite reports as SQLITE_IOERR ("disk I/O error")
            // and refuses to write any file-backed database.
            nr::FSYNC | nr::FDATASYNC => self.fds.get(a[0] as i32).map(|_| 0),
            nr::FACCESSAT | nr::FACCESSAT2 => self.sys_faccessat(m, a[0] as i32, a[1], a[2] as u32),
            nr::READLINKAT => self.sys_readlinkat(m, a[0] as i32, a[1], a[2], a[3]),
            nr::GETCWD => self.sys_getcwd(m, a[0], a[1]),
            nr::CHDIR => self.sys_chdir(m, a[0]),
            nr::FCHDIR => self.sys_fchdir(a[0] as i32),
            // Only a thread's own working directory (CLONE_FS) is unshared;
            // a private descriptor table or namespace is not supported.
            nr::UNSHARE => {
                if a[0] & !0x200 != 0 {
                    Err(Errno::EINVAL)
                } else {
                    if a[0] & 0x200 != 0 {
                        let cwd = self.vfs.cwd.clone();
                        self.thread_cwd.entry(self.ctx_addr).or_insert(cwd);
                    }
                    Ok(0)
                }
            }
            nr::MKDIRAT => self.sys_mkdirat(m, a[0] as i32, a[1], a[2] as u32),
            nr::UNLINKAT => self.sys_unlinkat(m, a[0] as i32, a[1], a[2]),
            nr::RENAMEAT => self.sys_renameat(m, a[0] as i32, a[1], a[2] as i32, a[3]),
            nr::RENAMEAT2 => {
                self.sys_renameat2(m, a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32)
            }
            nr::LINKAT => self.sys_linkat(m, a),
            nr::SYMLINKAT => self.sys_symlinkat(m, a[0], a[1] as i32, a[2]),
            nr::PPOLL => self.sys_ppoll(m, a[0], a[1]),
            nr::PSELECT6 => self.sys_pselect(m, a),
            nr::STATFS | nr::FSTATFS => self.sys_statfs(m, a[1]),
            // Modification times are not settable here: tmpfs keeps one it
            // maintains itself and hostfs reports the host's. Accepting the
            // call is still better than refusing it -- apt sets the time on
            // a file it has just downloaded, and treats the failure as a
            // failure of the download.
            nr::UTIMENSAT => self.sys_utimensat(m, a[0] as i32, a[1], a[2], a[3] as u32),
            // There is one process's view of the world and nothing consults
            // a umask, but a program that sets one wants the old value back.
            nr::UMASK => {
                let old = self.umask;
                self.umask = a[0] as u32 & 0o777;
                Ok(old as u64)
            }
            // Permission bits are not modelled; reporting success keeps
            // installers and build systems moving.
            nr::FCHMOD | nr::FCHMODAT => self.sys_chmod(m, n, a),
            // Ownership is not modelled: there is one user and the guest is
            // it. Refusing would be worse than accepting -- tar treats a
            // failed chown while unpacking as a failure to unpack.
            nr::FCHOWN | nr::FCHOWNAT => Ok(0),

            // -- memory ------------------------------------------------------
            nr::BRK => {
                // Zeroed, because Linux zeroes it and libc counts on that.
                let (result, grew) = self.mem.brk_grow(a[0]);
                if let Some((from, to)) = grew {
                    let _ = m.zero(from, (to - from) as usize);
                }
                Ok(result)
            }
            nr::MMAP => self.sys_mmap(m, a),
            nr::MUNMAP => {
                let flushed = self.flush_shared(m, a[0], a[1]);
                self.shared_maps
                    .retain(|s| s.start + s.len <= a[0] || s.start >= a[0] + a[1]);
                self.retire_code(a[0], a[1]);
                flushed.and_then(|_| self.mem.unmap(a[0], a[1])).map(|_| 0)
            }
            nr::MSYNC => self.flush_shared(m, a[0], a[1]).map(|_| 0),
            nr::MREMAP => self.sys_mremap(m, a),
            nr::MPROTECT => {
                // Losing execute retires the translations as surely as
                // unmapping does: `ld.so` applies RELRO by dropping write,
                // but a JIT in the guest flips a page from executable back to
                // writable precisely so it can put different code there.
                // Making code writable is the other half (a patch, a
                // detour), which x86 needs no cache flush to see.
                if (a[2] as u32) & crate::mem::prot::EXEC == 0 || (a[2] as u32) & crate::mem::prot::WRITE != 0 {
                    self.retire_code(a[0], a[1]);
                }
                self.mem.protect(a[0], a[1], (a[2] & 0x7) as u32).map(|_| 0)
            }
            nr::MADVISE => self.sys_madvise(m, a[0], a[1], a[2] as u32),

            // -- process and time --------------------------------------------
            nr::UNAME => self.sys_uname(m, a[0]),
            nr::GETPID => Ok(self.pid as u64),
            nr::GETPPID => Ok(self.procs.ppid(self.pid) as u64),
            nr::GETTID => Ok(self.tid as u64),
            nr::GETUID | nr::GETEUID => Ok(self.uid as u64),
            nr::GETGID | nr::GETEGID => Ok(self.gid as u64),

            // -- sessions and process groups ---------------------------------
            nr::SETSID => self.procs.setsid(self.pid).map(|s| s as u64),
            nr::GETSID => self.procs.sid(self.pid, a[0] as i32).map(|s| s as u64),
            nr::GETPGID => self.procs.pgid(self.pid, a[0] as i32).map(|g| g as u64),
            nr::SETPGID => self
                .procs
                .setpgid(self.pid, a[0] as i32, a[1] as i32)
                .map(|_| 0),

            // -- credentials -------------------------------------------------
            // There is no privilege model here: the guest is the whole system,
            // so a request to become another user is recorded and granted.
            // Refusing would be the less honest answer, since nothing gex does
            // is protected by the uid in the first place.
            nr::SETUID => set_id(&mut self.uid, a[0]),
            nr::SETGID => set_id(&mut self.gid, a[0]),
            nr::SETREUID => set_id(&mut self.uid, a[1]),
            nr::SETREGID => set_id(&mut self.gid, a[1]),
            nr::SETRESUID => set_id(&mut self.uid, a[1]),
            nr::SETRESGID => set_id(&mut self.gid, a[1]),
            nr::GETRESUID => self.sys_getres(m, a, self.uid),
            nr::GETRESGID => self.sys_getres(m, a, self.gid),
            // No supplementary groups. `getgroups(0, _)` asks for the count.
            nr::GETGROUPS => Ok(0),
            nr::SETGROUPS => Ok(0),
            // Names, death signals and dumpability: all things a process may
            // set about itself that change nothing observable here.
            nr::PRCTL => Ok(0),

            // -- mounts ------------------------------------------------------
            // Nothing here is written back to a device, so there is nothing
            // to flush -- but an init script's last act is usually to call it.
            nr::SYNC => Ok(0),
            nr::MOUNT => self.sys_mount(m, a),
            nr::UMOUNT2 => self.sys_umount(m, a[0]),

            // -- sockets -----------------------------------------------------
            nr::SOCKET => self.sys_socket(a[0], a[1]),
            // A unix socket and an internet socket are different concrete
            // types behind the same descriptor, so which `connect` this is
            // depends on what the descriptor turned out to be.
            nr::CONNECT => match self.sys_unix_connect(m, a[0] as i32, a[1], a[2]) {
                Err(Errno::ENOTSOCK) => self.sys_connect(m, a[0] as i32, a[1], a[2]),
                other => other,
            },
            nr::SENDTO => self.sys_sendto(m, a[0] as i32, a[1], a[2], a[4]),
            nr::RECVFROM => self.sys_recvfrom(m, a[0] as i32, a[1], a[2], a[4], a[5]),
            nr::SENDMSG => self.sys_sendmsg(m, a[0] as i32, a[1]),
            nr::RECVMSG => self.sys_recvmsg(m, a[0] as i32, a[1], a[2]),
            nr::SENDMMSG => self.sys_sendmmsg(m, a[0] as i32, a[1], a[2]),
            nr::RECVMMSG => self.sys_recvmmsg(m, a[0] as i32, a[1], a[2], a[3]),
            nr::GETPEERNAME => self.sys_getpeername(m, a[0] as i32, a[1], a[2]),
            // One interface, unnamed. A guest that asks where it is calling
            // from gets 0.0.0.0, which is what an unbound socket reports.
            nr::GETSOCKNAME => {
                let (ip, port) = self.socket_of(a[0] as i32).map(|s| s.local()).unwrap_or((0, 0));
                self.sys_write_sockaddr(m, ip, port, a[1], a[2])
            }
            // Nothing here has an option worth setting -- no timeouts, no
            // buffer sizes that mean anything -- and refusing makes libraries
            // treat the socket as broken.
            nr::SETSOCKOPT => Ok(0),
            nr::GETSOCKOPT => self.sys_getsockopt(m, a[0] as i32, a[2], a[3]),
            // A unix socket's directions close as asked; any other socket
            // (the proxied network, the global kernel's) just agrees.
            nr::SHUTDOWN => match self.unix_of(a[0] as i32) {
                Ok(sock) => sock.shutdown_how(a[1]),
                Err(_) => self.fds.get(a[0] as i32).map(|_| 0),
            },
            // A local address means nothing when there is one interface and
            // it is a proxy, but a resolver binds its socket before it sends
            // and gives up if that fails.
            nr::BIND => match self.sys_unix_bind(m, a[0] as i32, a[1], a[2]) {
                Err(Errno::ENOTSOCK) => match self.socket_of(a[0] as i32) {
                    // An internet socket remembers where it is (`listen`,
                    // `getsockname`).
                    Ok(sock) => m
                        .read(a[1], (a[2] as usize).min(128))
                        .and_then(|b| net::parse_sockaddr(b))
                        .and_then(|(ip, port)| sock.bind(ip, port).map(|_| 0)),
                    Err(_) => self.fds.get(a[0] as i32).map(|_| 0),
                },
                other => other,
            },
            // A socket in the global kernel was bound and listening in one
            // call; there is nothing left for this to do but agree.
            nr::LISTEN => match self.remote_unix_of(a[0] as i32) {
                Ok(_) => Ok(0),
                Err(_) => match self.unix_of(a[0] as i32) {
                    Ok(s) => s.listen(),
                    // A TCP listener, for guests on this machine.
                    Err(Errno::ENOTSOCK) => self.socket_of(a[0] as i32).and_then(|s| s.listen()).map(|_| 0),
                    Err(e) => Err(e),
                },
            },
            nr::ACCEPT => self.sys_accept(m, a[0] as i32, a[1], a[2], 0),
            nr::ACCEPT4 => self.sys_accept(m, a[0] as i32, a[1], a[2], a[3]),
            nr::SOCKETPAIR => self.sys_socketpair(m, a[0], a[1], a[3]),
            nr::MEMFD_CREATE => self.sys_memfd_create(m, a[0], a[1]),
            nr::FLOCK => self.sys_flock(a[0] as i32, a[1]),
            nr::FALLOCATE => self.sys_fallocate(a[0] as i32, a[1], a[2], a[3]),
            nr::EPOLL_CREATE1 => self.sys_epoll_create(a[0]),
            nr::EPOLL_CTL => self.sys_epoll_ctl(m, a[0] as i32, a[1], a[2] as i32, a[3]),
            nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2 => self.sys_epoll_wait(m, a[0] as i32, a[1], a[2] as u32),
            nr::CLOSE_RANGE => self.sys_close_range(a[0], a[1], a[2]),
            nr::MEMBARRIER => self.sys_membarrier(a[0]),
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
            // One processor, and it is the one the caller is on. dpkg asks
            // to size its worker pool.
            nr::SCHED_GETAFFINITY => self.sys_getaffinity(m, a[1], a[2]),
            // Interval timers are not delivered: nothing here raises SIGALRM.
            // Accepting the call is still right -- a program that cannot arm
            // a timeout does not conclude it has no timeout, it concludes
            // something is badly wrong, and busybox's wget spins on it.
            nr::SETITIMER => self.sys_itimer(m, a[0], a[1], a[2], true),
            nr::GETITIMER => self.sys_itimer(m, a[0], 0, a[1], false),
            nr::SCHED_YIELD => {
                std::thread::yield_now();
                Ok(0)
            }
            // Scheduling policy: every guest thread runs SCHED_OTHER at
            // priority 0, which is what these report. Setting a normal policy
            // is accepted; a real-time one is refused as for an unprivileged
            // process. Mono treats a failed priority query as fatal.
            nr::SCHED_GETSCHEDULER => Ok(0),
            nr::SCHED_GETPARAM => self.sys_sched_getparam(m, a[1]),
            nr::SCHED_SETPARAM => Ok(0),
            nr::SCHED_SETSCHEDULER => match a[1] & 0xff {
                1 | 2 => Err(Errno::EPERM),
                0 | 3 | 5 => Ok(0),
                _ => Err(Errno::EINVAL),
            },
            nr::SCHED_GET_PRIORITY_MAX => Ok(if matches!(a[0], 1 | 2) { 99 } else { 0 }),
            nr::SCHED_GET_PRIORITY_MIN => Ok(if matches!(a[0], 1 | 2) { 1 } else { 0 }),
            // Scheduling niceness: we run every guest thread at the default
            // priority, so report nice 0 and accept any set as a no-op. The
            // raw getpriority ABI returns `20 - nice` (kept non-negative so a
            // real nice value is never mistaken for an -errno), i.e. 20 here.
            nr::GETPRIORITY => Ok(20),
            nr::SETPRIORITY => Ok(0),
            // getcpu(cpu, node, tcache): a single scheduling domain, CPU 0.
            nr::GETCPU => self.sys_getcpu(m, a[0], a[1]),
            nr::TIMES => self.sys_times(m, a[0]),
            nr::GETRUSAGE => self.sys_getrusage(m, a[1]),
            nr::SYSINFO => self.sys_sysinfo(m, a[0]),
            nr::PRLIMIT64 => self.sys_prlimit(m, a[2], a[3]),

            // -- signals -----------------------------------------------------
            nr::RT_SIGACTION => self.sys_rt_sigaction(m, a[0] as u32, a[1], a[2]),
            nr::RT_SIGTIMEDWAIT => self.sys_rt_sigtimedwait(m, a),
            nr::RT_SIGPROCMASK => self.sys_rt_sigprocmask(m, a[0] as u32, a[1], a[2]),
            nr::RT_SIGPENDING => self.sys_rt_sigpending(m, a[0]),
            nr::SIGALTSTACK => self.sys_sigaltstack(m, a[0], a[1]),
            // `tkill` and `tgkill` address a thread: see `sys_tgkill`.
            nr::TKILL | nr::TGKILL => self.sys_tgkill(n, a),
            // `kill` addresses a process, which may be any process on the
            // system. It goes through the table, which leaves the signal
            // pending for the target to collect at its next syscall.
            nr::KILL => {
                let target = a[0] as i32;
                let signo = a[1] as u32;
                if self.trace {
                    eprintln!("[gex:{}] kill -> pid {target} signal {signo}", self.pid);
                }
                match self.procs.kill(self.pid, target, signo) {
                    Ok(_) => {
                        // If the caller was one of the targets -- it signalled
                        // itself, or its own group, or everything -- it must
                        // see the signal before this syscall returns, because
                        // `raise` is expected to have delivered by then. Who
                        // the targets were is the table's question to answer,
                        // not a rule to restate here: getting it wrong the
                        // other way made `kill(-pgid)` signal a caller that
                        // was in a different group entirely.
                        self.take_sent_signals();
                        Ok(0)
                    }
                    Err(e) => Err(e),
                }
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
                    eprintln!("[gex:{}] unimplemented syscall {n} -> ENOSYS", self.pid);
                }
                Err(Errno::ENOSYS)
            }
        };
        Ok(r)
    }

    fn sys_futex(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (uaddr, raw_op, val) = (a[0], a[1] as u32, a[2] as u32);
        // The private and clock bits change nothing here: the timeout is
        // treated as relative either way.
        let operation = raw_op & !(futex::op::PRIVATE_FLAG | futex::op::CLOCK_REALTIME);
        match operation {
            // The waiting forms reach here only when `blocking_op` refused
            // them, which it does for a futex word the process cannot read.
            futex::op::WAIT | futex::op::WAIT_BITSET => {
                let mapped = self
                    .mem
                    .vmas()
                    .iter()
                    .any(|v| uaddr >= v.start && uaddr + 4 <= v.end);
                // `EAGAIN`, not `EINVAL`, for a wait on memory the process
                // has: glibc aborts on a futex error it does not recognise,
                // and `EAGAIN` (word changed, so re-read and loop) is on its
                // recognised list while `EINVAL` is not. An address the
                // process lacks is still `EFAULT` (#113).
                Err(if mapped { Errno::EAGAIN } else { Errno::EFAULT })
            }
            futex::op::WAKE | futex::op::WAKE_BITSET => Ok(self.futexes.wake(uaddr, val)),
            // LOCK_PI reaches here only when `blocking_op` refused it: a
            // word the process does not have, or misaligned.
            futex::op::LOCK_PI | futex::op::LOCK_PI2 => {
                Err(if uaddr % 4 != 0 { Errno::EINVAL } else { Errno::EFAULT })
            }
            // Release a PI lock this thread owns, and wake one waiter, which
            // takes it in its own `FUTEX_LOCK_PI` loop.
            futex::op::UNLOCK_PI => {
                let tid = self.tid as u32;
                loop {
                    let v = gex_atomic::load_u32(m.bytes, uaddr).ok_or(Errno::EFAULT)?;
                    if v & futex::pi::TID_MASK != tid {
                        return Err(Errno::EPERM);
                    }
                    if gex_atomic::cas_u32(m.bytes, uaddr, v, 0).ok_or(Errno::EFAULT)?.is_ok() {
                        break;
                    }
                }
                self.futexes.wake(uaddr, 1);
                Ok(0)
            }
            futex::op::TRYLOCK_PI => {
                let tid = self.tid as u32;
                loop {
                    let v = gex_atomic::load_u32(m.bytes, uaddr).ok_or(Errno::EFAULT)?;
                    match v & futex::pi::TID_MASK {
                        0 => {
                            let new = tid | (v & futex::pi::WAITERS);
                            if gex_atomic::cas_u32(m.bytes, uaddr, v, new).ok_or(Errno::EFAULT)?.is_ok() {
                                return Ok(0);
                            }
                        }
                        owner if owner == tid => return Err(Errno::EDEADLK),
                        _ => return Err(Errno::EAGAIN),
                    }
                }
            }
            // Requeue is an optimisation for condition variables; waking
            // everyone is correct, if less efficient.
            futex::op::REQUEUE | futex::op::CMP_REQUEUE => Ok(self.futexes.wake(uaddr, u32::MAX)),
            // `FUTEX_WAKE_OP`: atomically apply an op to a second word, wake
            // `val` waiters on `uaddr`, then -- if the old second word compares
            // true -- wake `a[3]` waiters on `uaddr2` (`a[4]`). glibc's condvar
            // signalling uses it; returning ENOSYS here lost a wake and left
            // Qt's pthread-cond waiters asleep forever. The operation and
            // comparison are packed into `a[5]` (see the kernel's `FUTEX_OP`).
            futex::op::WAKE_OP => {
                let nr_wake2 = a[3] as u32;
                let uaddr2 = a[4];
                let encoded = a[5] as u32;
                let mut oper = (encoded >> 28) & 0xf;
                let cmp = (encoded >> 24) & 0xf;
                // `oparg`/`cmparg` are 12-bit signed fields.
                let mut oparg = ((((encoded >> 12) & 0xfff) as i32) << 20) >> 20;
                let cmparg = (((encoded & 0xfff) as i32) << 20) >> 20;
                // `FUTEX_OP_OPARG_SHIFT`: the arg is a shift count, not a value.
                if oper & 8 != 0 {
                    oper &= 7;
                    oparg = 1i32.wrapping_shl((oparg & 31) as u32);
                }
                let oldval = m.read_u32(uaddr2)? as i32;
                let newval = match oper {
                    0 => oparg,                        // SET
                    1 => oldval.wrapping_add(oparg),   // ADD
                    2 => oldval | oparg,               // OR
                    3 => oldval & !oparg,              // ANDN
                    4 => oldval ^ oparg,               // XOR
                    _ => return Err(Errno::ENOSYS),
                };
                m.write_u32(uaddr2, newval as u32)?;
                let mut woken = self.futexes.wake(uaddr, val);
                let fire = match cmp {
                    0 => oldval == cmparg, // EQ
                    1 => oldval != cmparg, // NE
                    2 => oldval < cmparg,  // LT
                    3 => oldval <= cmparg, // LE
                    4 => oldval > cmparg,  // GT
                    5 => oldval >= cmparg, // GE
                    _ => return Err(Errno::ENOSYS),
                };
                if fire {
                    woken += self.futexes.wake(uaddr2, nr_wake2);
                }
                Ok(woken)
            }
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
        // The i386 struct sigaction is { u32 handler; u32 flags; u32 restorer;
        // u64 mask }, 20 bytes.
        if old != 0 {
            let a = self.signals.action(signo);
            if self.compat {
                m.write_u32(old, a.handler as u32)?;
                m.write_u32(old + 4, a.flags as u32)?;
                m.write_u32(old + 8, a.restorer as u32)?;
                m.write_bits64(old + 12, a.mask)?;
            } else {
                m.write_u64(old, a.handler)?;
                m.write_u64(old + 8, a.flags)?;
                m.write_u64(old + 16, a.restorer)?;
                m.write_bits64(old + 24, a.mask)?;
            }
        }
        if act != 0 {
            let a = if self.compat {
                SigAction {
                    handler: m.read_u32(act)? as u64,
                    flags: m.read_u32(act + 4)? as u64,
                    restorer: m.read_u32(act + 8)? as u64,
                    mask: m.read_bits64(act + 12)?,
                }
            } else {
                SigAction {
                    handler: m.read_u64(act)?,
                    flags: m.read_u64(act + 8)?,
                    restorer: m.read_u64(act + 16)?,
                    mask: m.read_bits64(act + 24)?,
                }
            };
            self.signals.set_action(signo, a)?;
        }
        Ok(0)
    }

    /// `rt_sigpending`: the signals raised on this process while blocked, which
    /// have not been delivered. Written as a `sigset_t` -- a bitmask with bit
    /// `signal-1` set, which is exactly the shape `Signals::pending` holds.
    fn sys_rt_sigpending(&self, m: &mut GuestMem<'_>, set: u64) -> SysResult {
        if set != 0 {
            m.write_bits64(set, self.signals.pending_raw())?;
        }
        Ok(0)
    }

    /// `getcpu`: which CPU/NUMA node the caller runs on. We present a single
    /// scheduling domain, so both are 0. The third arg (tcache) is ignored on
    /// modern kernels; null pointers are skipped.
    fn sys_getcpu(&self, m: &mut GuestMem<'_>, cpu: u64, node: u64) -> SysResult {
        if cpu != 0 {
            m.write_u32(cpu, 0)?;
        }
        if node != 0 {
            m.write_u32(node, 0)?;
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
            m.write_bits64(old, self.signals.blocked)?;
        }
        if set != 0 {
            let mask = m.read_bits64(set)?;
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
        // The 32-bit stack_t is { u32 sp; i32 flags; u32 size }.
        if old != 0 {
            let a = self.signals.alt_stack;
            if self.compat {
                m.write_u32(old, a.sp as u32)?;
                m.write_u32(old + 4, a.flags)?;
                m.write_u32(old + 8, a.size as u32)?;
            } else {
                m.write_u64(old, a.sp)?;
                m.write_u32(old + 8, a.flags)?;
                m.write_u64(old + 16, a.size)?;
            }
        }
        if new != 0 {
            self.signals.alt_stack = if self.compat {
                SigAltStack {
                    sp: m.read_u32(new)? as u64,
                    flags: m.read_u32(new + 4)?,
                    size: m.read_u32(new + 8)? as u64,
                }
            } else {
                SigAltStack {
                    sp: m.read_u64(new)?,
                    flags: m.read_u32(new + 8)?,
                    size: m.read_u64(new + 16)?,
                }
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
        self.collect_signals();
        self.check_timers();
        if !self.signals.has_pending() {
            return Delivery::None;
        }
        if self.trace {
            eprintln!(
                "[gex:{}.{}] delivering signal {} (pending {:#x}, blocked {:#x})",
                self.pid,
                self.sig_tid,
                self.signals.peek_deliverable().unwrap_or(0),
                self.signals.pending_raw(),
                self.signals.blocked
            );
        }
        signal::dispatch(&mut self.signals, m, self.ctx_addr, self.compat).unwrap_or(Delivery::None)
    }

    /// Move signals other processes have sent into this one's pending set.
    ///
    /// `kill` cannot reach into another process's kernel -- each has its own
    /// -- so it leaves the signal on a shared per-process word and the target
    /// picks it up here. This is what makes `timeout`, job control and every
    /// supervisor work.
    pub fn collect_signals(&mut self) {
        let inbox = self.procs.take_signals(self.pid);
        if inbox == 0 {
            return;
        }
        if self.trace {
            eprintln!("[gex:{}] collected signals {inbox:#x}", self.pid);
        }
        for signo in 1..=64u32 {
            if inbox & (1u64 << (signo - 1)) != 0 {
                self.signals.raise_shared(signo, None);
            }
        }
        self.note_kill_senders(inbox);
    }

    /// Whether anything is waiting to be delivered, counting what other
    /// processes have sent. Asked from the host's wait loops, which have no
    /// other way to notice.
    pub fn signal_pending(&mut self) -> bool {
        self.collect_signals();
        self.check_timers();
        let ready = self.signals.has_pending();
        if self.trace && !ready && self.signals.pending_raw() != 0 {
            // Arrived but not deliverable. Almost always the blocked mask,
            // and worth saying out loud: a process that will not wake looks
            // exactly like one nobody signalled.
            eprintln!(
                "[gex:{}.{}] signal pending {:#x} but blocked {:#x}",
                self.pid,
                self.sig_tid,
                self.signals.pending_raw(),
                self.signals.blocked
            );
        }
        ready
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
                core: matches!(signal::default_action(signo), signal::DefaultAction::Core),
            };
        }
        if m.write_u64(self.ctx_addr + ctx::PC as u64, pc).is_err() {
            return Delivery::Terminate {
                signal: signo,
                core: true,
            };
        }
        self.signals.forget_source(signo);
        match signal::setup_frame(
            &mut self.signals,
            m,
            self.ctx_addr,
            signo,
            fault_address,
            self.compat,
        ) {
            Ok(resume) => Delivery::Resume(resume),
            Err(_) => Delivery::Terminate {
                signal: signo,
                core: true,
            },
        }
    }

    /// Restore the context a handler frame captured.
    pub fn sigreturn(&mut self, m: &mut GuestMem<'_>) -> Result<u64, Errno> {
        let legacy = std::mem::replace(&mut self.sigreturn_legacy, false);
        signal::sigreturn(&mut self.signals, m, self.ctx_addr, self.compat, legacy)
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
        // Straight out of the guest's memory: a guest address is already a
        // linear-memory offset, so the bytes are a slice and a copy would be
        // pure overhead.
        let data = m.read(buf, len as usize)?;
        let file = self.fds.get(fd)?;
        file.write(data)
    }

    fn sys_read(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64, len: u64) -> SysResult {
        // Straight into the guest's memory, no intermediate buffer: the guest
        // picks the length and a copy would allocate that much on every call.
        // Capped at 16 MiB.
        let len = len.min(1 << 24) as usize;
        let dst = m.read_mut(buf, len)?;
        self.fds.get(fd)?.read(dst)
    }

    fn sys_pread(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        buf: u64,
        len: u64,
        off: u64,
    ) -> SysResult {
        // As `sys_read`: the guest's buffer is the destination already.
        let len = len.min(1 << 24) as usize;
        let dst = m.read_mut(buf, len)?;
        self.fds.get(fd)?.pread(off, dst)
    }

    fn sys_pwrite(&mut self, m: &GuestMem<'_>, fd: i32, buf: u64, len: u64, off: u64) -> SysResult {
        let data = m.read(buf, len as usize)?.to_vec();
        self.fds.get(fd)?.pwrite(off, &data)
    }

    /// Copy up to `count` bytes from one descriptor to another through a kernel
    /// buffer -- the shared core of `sendfile` and `copy_file_range`. When
    /// `in_off`/`out_off` is `Some` it is an absolute offset used without
    /// touching that descriptor's own position (the offset-pointer forms);
    /// `None` uses and advances the descriptor's position. Returns the bytes
    /// transferred. A single call moves at most one buffer, as Linux does --
    /// callers loop for more. If the output takes fewer bytes than were read
    /// from a positional-less input, the input is rewound so the tail is not
    /// silently dropped.
    fn transfer_fds(
        &mut self,
        in_fd: i32,
        in_off: Option<u64>,
        out_fd: i32,
        out_off: Option<u64>,
        count: u64,
    ) -> SysResult {
        let cap = count.min(1 << 24) as usize;
        if cap == 0 {
            return Ok(0);
        }
        let in_file = self.fds.get(in_fd)?;
        let out_file = self.fds.get(out_fd)?;
        let mut buf = vec![0u8; cap];
        let n = match in_off {
            Some(o) => in_file.pread(o, &mut buf)?,
            None => in_file.read(&mut buf)?,
        } as usize;
        if n == 0 {
            return Ok(0);
        }
        let mut written = 0usize;
        while written < n {
            let w = match out_off {
                Some(o) => out_file.pwrite(o + written as u64, &buf[written..n])?,
                None => out_file.write(&buf[written..n])?,
            } as usize;
            if w == 0 {
                break;
            }
            written += w;
        }
        // A positional-less input already advanced by the full `n`; hand back
        // the tail the output could not take (best effort: a stream cannot seek).
        if in_off.is_none() && written < n {
            let _ = in_file.seek(-((n - written) as i64), 1 /* SEEK_CUR */);
        }
        Ok(written as u64)
    }

    /// `sendfile(out_fd, in_fd, offset_ptr, count)`: copy in the kernel. With a
    /// non-null `offset_ptr` the read starts there and the pointer is advanced
    /// by the count sent, leaving the input's own position untouched -- so a
    /// server can serve the same file to many clients without seeking.
    fn sys_sendfile(
        &mut self,
        m: &mut GuestMem<'_>,
        out_fd: i32,
        in_fd: i32,
        off_ptr: u64,
        count: u64,
    ) -> SysResult {
        let in_off = if off_ptr != 0 {
            Some(m.read_u64(off_ptr)?)
        } else {
            None
        };
        let sent = self.transfer_fds(in_fd, in_off, out_fd, None, count)?;
        if let Some(o) = in_off {
            m.write_u64(off_ptr, o + sent)?;
        }
        Ok(sent)
    }

    /// `copy_file_range(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags)`.
    /// Both offset pointers are optional; each non-null one is used for the
    /// transfer and then advanced by the bytes copied. `flags` is reserved and
    /// must be zero.
    fn sys_copy_file_range(
        &mut self,
        m: &mut GuestMem<'_>,
        fd_in: i32,
        off_in: u64,
        fd_out: i32,
        off_out: u64,
        len: u64,
        flags: u32,
    ) -> SysResult {
        if flags != 0 {
            return Err(Errno::EINVAL);
        }
        let in_off = if off_in != 0 {
            Some(m.read_u64(off_in)?)
        } else {
            None
        };
        let out_off = if off_out != 0 {
            Some(m.read_u64(off_out)?)
        } else {
            None
        };
        let copied = self.transfer_fds(fd_in, in_off, fd_out, out_off, len)?;
        if let Some(o) = in_off {
            m.write_u64(off_in, o + copied)?;
        }
        if let Some(o) = out_off {
            m.write_u64(off_out, o + copied)?;
        }
        Ok(copied)
    }

    /// sendfile, copy_file_range and splice, by number.
    fn transfer_syscall(&mut self, m: &mut GuestMem<'_>, n: u64, a: &[u64; 6]) -> SysResult {
        match n {
            nr::SENDFILE => self.sys_sendfile(m, a[0] as i32, a[1] as i32, a[2], a[3]),
            // A blocking `recvfrom`/`recv` that waited for a datagram or
            // bytes (the classifier below).
            nr::RECVFROM => self.sys_recvfrom(m, a[0] as i32, a[1], a[2], a[4], a[5]),
            nr::COPY_FILE_RANGE => self.sys_copy_file_range(m, a[0] as i32, a[1], a[2] as i32, a[3], a[4], a[5] as u32),
            _ => self.sys_splice(m, a[0] as i32, a[1], a[2] as i32, a[3], a[4]),
        }
    }

    /// `splice(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags)`: as
    /// copy_file_range, with pipes allowed on either side (a pipe's offset
    /// pointer must be null, which the transfer does not need to check: a
    /// pipe has no `pread`). The SPLICE_F_* flags are hints.
    fn sys_splice(&mut self, m: &mut GuestMem<'_>, fd_in: i32, off_in: u64, fd_out: i32, off_out: u64, len: u64) -> SysResult {
        self.sys_copy_file_range(m, fd_in, off_in, fd_out, off_out, len, 0)
    }

    fn read_iovecs(
        m: &GuestMem<'_>,
        iov: u64,
        count: u64,
        compat: bool,
    ) -> Result<Vec<(u64, u64)>, Errno> {
        if count > 1024 {
            return Err(Errno::EINVAL);
        }
        let mut v = Vec::with_capacity(count as usize);
        for i in 0..count {
            // A 32-bit process's iovec is two 32-bit words.
            if compat {
                let base = iov + i * 8;
                v.push((m.read_u32(base)? as u64, m.read_u32(base + 4)? as u64));
            } else {
                let base = iov + i * 16;
                v.push((m.read_u64(base)?, m.read_u64(base + 8)?));
            }
        }
        Ok(v)
    }

    fn sys_writev(&mut self, m: &GuestMem<'_>, fd: i32, iov: u64, count: u64) -> SysResult {
        let vecs = Self::read_iovecs(m, iov, count, self.compat)?;
        let file = self.fds.get(fd)?;
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let data = match m.read(base, len as usize) {
                Ok(d) => d,
                // Same rule as below: bytes already accepted have to be
                // reported, or the caller sends them twice.
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            };
            let n = match file.write(data) {
                Ok(n) => n,
                // A pipe or socket that has filled up answers `EAGAIN`. If
                // earlier vectors were already accepted, the call has
                // succeeded partially and must say so: returning the error
                // instead throws away the count for bytes that are already
                // gone, and the caller -- believing it sent nothing --
                // retransmits them. An X client doing that corrupts the
                // request stream.
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            };
            total += n;
            // A short write ends the call; the caller retries the remainder.
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    fn sys_readv(&mut self, m: &mut GuestMem<'_>, fd: i32, iov: u64, count: u64) -> SysResult {
        let vecs = Self::read_iovecs(m, iov, count, self.compat)?;
        let file = self.fds.get(fd)?;
        let mut total = 0u64;
        for (base, len) in vecs {
            if len == 0 {
                continue;
            }
            let mut tmp = vec![0u8; len as usize];
            let n = match file.read(&mut tmp) {
                Ok(n) => n,
                // The vector filled and the next one found the socket empty,
                // which is `EAGAIN`. Reporting that would discard the count
                // for bytes this call has *already taken out of the socket*
                // and written into the caller's memory -- they are consumed,
                // and nothing can give them back. The caller sees "nothing
                // arrived" and waits for data it is already holding.
                //
                // That is the X protocol deadlocking: the server reads a
                // request header into the first vector, loses it here, and
                // waits forever for a request the client believes it sent.
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            };
            m.write(base, &tmp[..n as usize])?;
            total += n;
            if n < len {
                break;
            }
        }
        Ok(total)
    }

    /// Open a pseudoterminal end if `p` names one, else `None` so the ordinary
    /// open runs. The pair lives in the global kernel (`self.ptys`); with none
    /// there is no pty support and `/dev/ptmx` falls through to `ENOENT`.
    fn open_pty(&mut self, p: &str, flags: u64) -> Option<SysResult> {
        let r = self.ptys?;
        // `/dev/tty` is the caller's *controlling terminal*. Inside a terminal
        // emulator that is a pty in the global kernel, not the page's own
        // terminal -- a shell opens this for job control and compares its
        // foreground group with its own, and the page's terminal never matches:
        // dash then spins raising SIGTTIN at itself, which shows as a window
        // that echoes what is typed and runs none of it. Only when this
        // session has claimed one; otherwise the name falls through to the
        // console handle, which is right for the page's own shell.
        if p == "/dev/tty" {
            let sid = self.procs.sid(self.pid, 0).unwrap_or(0);
            let id = (r.for_sid)(sid);
            if id >= 0 {
                let handle = Arc::new(crate::pty::RemotePty { id, master: false, remote: r });
                // A descriptor of its own on that end, so the counting stays
                // right, but no claim: this session already owns it.
                (r.open_end)(id, 0);
                let file = OpenFile::new(handle, format!("pty:[{id}:s]"), flags, true, true);
                return Some(self.fds.insert(file, flags & oflags::O_CLOEXEC != 0));
            }
            return None;
        }
        let (id, master) = if p == "/dev/ptmx" {
            let packed = (r.open_master)();
            if packed < 0 {
                return Some(Err(Errno::ENOSPC));
            }
            (packed >> 16, true)
        } else if let Some(n) = p.strip_prefix("/dev/pts/") {
            let ptn: i32 = n.parse().ok()?;
            let id = (r.open_slave)(ptn);
            if id == -1 {
                return None; // no such pts -- let the VFS answer ENOENT
            }
            if id < 0 {
                return Some(Err(Errno(-id))); // locked, or otherwise refused
            }
            (id, false)
        } else {
            return None;
        };
        let handle = Arc::new(crate::pty::RemotePty { id, master, remote: r });
        // Opening a terminal is how a session claims it.
        //
        // A session leader with no controlling terminal that opens one without
        // O_NOCTTY acquires it, and its process group becomes the terminal's
        // foreground group -- Linux does this in the open, not in `TIOCSCTTY`,
        // and a terminal emulator's child relies on it: xterm's child calls
        // `setsid` and opens the slave, and never issues the ioctl. Without
        // this the shell it execs is in a *background* group, so the kernel
        // answers every read from the terminal with SIGTTIN: a window that
        // echoes what is typed (the line discipline does that) and runs none
        // of it.
        if !master && flags & oflags::O_NOCTTY == 0 {
            let leader = self.procs.sid(self.pid, 0) == Ok(self.pid);
            if leader {
                use crate::vfs::FileHandle;
                handle.claim_session(self.pid);
            }
        }
        let path = format!("pty:[{id}:{}]", if master { 'm' } else { 's' });
        let file = OpenFile::new(handle, path, flags, true, true);
        Some(self.fds.insert(file, flags & oflags::O_CLOEXEC != 0))
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
        // Unix98 pseudoterminals. `/dev/ptmx` makes a fresh pair and hands back
        // the master; `/dev/pts/N` takes the matching slave. Both live in the
        // global kernel, so the master (xterm) and the slave (its shell) can be
        // different Workers. Intercepted before the VFS, which has no such nodes.
        if let Some(res) = self.open_pty(&p, flags) {
            return res;
        }
        // `O_TMPFILE` names a directory and asks for an unnamed file inside
        // it: made, opened, and unlinked at once, so the handle is the only
        // way to it. `O_PATH` opens the name, not the file, so it does not
        // follow a dangling symlink to `ENOENT` -- which is how tar sets the
        // mode on a symlink whose target does not exist yet.
        let mut tmpfile_path: Option<String> = None;
        let handle = if flags & oflags::O_PATH != 0 {
            let md = if flags & oflags::O_NOFOLLOW != 0 {
                self.vfs.metadata_nofollow(&p)?
            } else {
                self.vfs.metadata(&p)?
            };
            Arc::new(crate::vfs::PathHandle(md)) as Arc<dyn crate::vfs::FileHandle>
        } else if flags & oflags::O_TMPFILE == oflags::O_TMPFILE {
            let n = self.next_tmpfile;
            self.next_tmpfile += 1;
            let tmp = format!("{}/.gex-tmpfile-{}-{}", p.trim_end_matches('/'), self.pid, n);
            let h = self.vfs.open(
                &tmp,
                oflags::O_RDWR | oflags::O_CREAT | oflags::O_EXCL,
                if mode == 0 { 0o600 } else { mode },
            )?;
            // Kept named (not unlinked here) so `/proc/self/fd/N` resolves to the
            // file and the `linkat(..., AT_SYMLINK_FOLLOW)` idiom can hardlink it
            // into place; the name is removed when the descriptor closes (see
            // `OpenFile::unlink_on_close`). Recording `tmp` as the fd's path --
            // not the directory `p` -- is what makes `/proc/self/fd/N` point at
            // the file rather than at a directory (linking which is EPERM).
            tmpfile_path = Some(tmp);
            h
        } else {
            // Arm a pin if this is an object of the closure. Before the open
            // rather than after, because `open` moves `p` into the call and
            // the path is what identifies the object.
            if let Some(base) = self.pin_for(&p) {
                self.armed_pin = Some((p.clone(), base));
            }
            let h = self.vfs.open(&p, flags, mode)?;
            // And arm a placement, for an object nobody pinned. Only for a
            // shared object: the executable's own base is fixed by the
            // loader rather than reserved, and arming on every open would
            // hand the next unrelated reservation somebody else's address.
            //
            // After the open rather than before, so a path that does not
            // exist -- `ld.so` probes several directories for every library
            // it loads -- leaves nothing armed.
            if self.place.is_some() && is_shared_object(&p) {
                self.armed_place = Some(p.clone());
            }
            h
        };
        let acc = flags & oflags::ACCMODE;
        let readable = acc == oflags::O_RDONLY || acc == oflags::O_RDWR;
        let writable = acc == oflags::O_WRONLY || acc == oflags::O_RDWR;
        let file = match tmpfile_path {
            Some(tp) => OpenFile::tmpfile(handle, tp, flags, readable, writable),
            None => OpenFile::new(handle, p, flags, readable, writable),
        };
        self.fds.insert(file, flags & oflags::O_CLOEXEC != 0)
    }

    /// `openat2(dirfd, path, how, size)`: `openat` with an extensible
    /// `open_how` struct { u64 flags; u64 mode; u64 resolve } instead of a bare
    /// flags word.
    ///
    /// `resolve` is the reason a program reaches for this syscall specifically:
    /// RESOLVE_BENEATH / RESOLVE_IN_ROOT / RESOLVE_NO_SYMLINKS pin path lookup
    /// inside a subtree, which is how a sandbox keeps a lookup from escaping.
    /// This VFS cannot enforce that per-open, and the one thing worse than not
    /// offering containment is offering it falsely -- so any non-zero `resolve`
    /// gets EINVAL rather than an open that silently ignored the constraint.
    /// `resolve == 0` is ordinary resolution and routes to the `openat` path.
    fn sys_openat2(
        &mut self,
        m: &GuestMem<'_>,
        dirfd: i32,
        path: u64,
        how: u64,
        size: u64,
    ) -> SysResult {
        // The struct is versioned by length; Linux rejects one shorter than the
        // first defined version (three u64s = 24 bytes).
        if size < 24 {
            return Err(Errno::EINVAL);
        }
        let flags = m.read_u64(how)?;
        let mode = m.read_u64(how + 8)? as u32;
        let resolve = m.read_u64(how + 16)?;
        if resolve != 0 {
            return Err(Errno::EINVAL);
        }
        self.sys_openat(m, dirfd, path, flags, mode)
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
        self.fds.install(new, file, flags & oflags::O_CLOEXEC != 0)
    }

    /// `linkat`: a second name for a file that already exists.
    fn sys_linkat(&mut self, m: &GuestMem<'_>, a: [u64; 6]) -> SysResult {
        const AT_SYMLINK_FOLLOW: u64 = 0x400;
        let from = self.at_path(m, a[0] as i32, a[1])?;
        // With AT_SYMLINK_FOLLOW the link is to what `from` points at, not the
        // symlink itself. This is how the O_TMPFILE atomic-create idiom names an
        // unnamed file: write to an `O_TMPFILE` fd, then
        // `linkat("/proc/self/fd/N", target, AT_SYMLINK_FOLLOW)`. gex's
        // `/proc/self/fd/N` is a symlink to the temp's real path, so following it
        // here turns a would-be cross-mount link (/proc vs the target's fs, an
        // EXDEV) into the same-fs link it really is. QSaveFile (Qt, hence every
        // KDE app) relies on it -- without it dolphin cannot save its bookmarks.
        let from = if a[4] & AT_SYMLINK_FOLLOW != 0 {
            let mut cur = from;
            for _ in 0..40 {
                match self.vfs.read_link(&cur) {
                    Ok(target) => {
                        cur = if target.starts_with('/') {
                            crate::vfs::normalize(&target, "/")
                        } else {
                            let parent = crate::vfs::parent_of(&cur);
                            crate::vfs::normalize(&target, &parent)
                        };
                    }
                    Err(_) => break,
                }
            }
            cur
        } else {
            from
        };
        let to = self.at_path(m, a[2] as i32, a[3])?;
        self.vfs.link(&from, &to).map(|_| 0)
    }

    /// `eventfd2`: a counting descriptor an event loop pokes to wake itself.
    ///
    /// One descriptor, readable and writable, over a single `EventFd`. The
    /// `EFD_NONBLOCK`/`EFD_CLOEXEC` flags are `O_NONBLOCK`/`O_CLOEXEC` by
    /// another name, so they go straight into the open flags and the
    /// close-on-exec bit; only `EFD_SEMAPHORE` changes the object.
    fn sys_eventfd2(&mut self, initval: u64, flags: u64) -> SysResult {
        use crate::eventfd::{flags as efd, EventFd};
        let handle = Arc::new(EventFd::new(initval, flags & efd::SEMAPHORE != 0));
        let cloexec = flags & oflags::O_CLOEXEC != 0;
        let file = OpenFile::new(
            handle,
            "anon_inode:[eventfd]".to_string(),
            (flags & oflags::O_NONBLOCK) | oflags::O_RDWR,
            true,
            true,
        );
        self.fds.insert(file, cloexec)
    }

    /// `timerfd_create`: a clock as a descriptor for an event loop to poll.
    fn sys_timerfd_create(&mut self, clockid: u64, flags: u64) -> SysResult {
        let mut timer = crate::timerfd::TimerFd::new(self.clock);
        if is_monotonic_clock(clockid) {
            timer = timer.monotonic(monotonic_from);
        }
        let handle = Arc::new(timer);
        let cloexec = flags & oflags::O_CLOEXEC != 0;
        let file = OpenFile::new(
            handle,
            "anon_inode:[timerfd]".to_string(),
            (flags & oflags::O_NONBLOCK) | oflags::O_RDONLY,
            true,
            false,
        );
        self.fds.insert(file, cloexec)
    }

    /// `timerfd_settime`: arm or disarm the timer from an `itimerspec`.
    ///
    /// The struct is two `timespec`s -- `it_interval` then `it_value`, sixteen
    /// bytes each. `it_value` of zero disarms; the `TFD_TIMER_ABSTIME` flag
    /// makes it an absolute time on the clock. The old setting is written back
    /// through `old` when it is non-null, the same shape the kernel returns.
    fn sys_timerfd_settime(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        flags: u64,
        new: u64,
        old: u64,
    ) -> SysResult {
        let read_ns = |m: &GuestMem<'_>, at: u64| -> Result<u64, Errno> {
            let secs = m.read_u64(at)?;
            let nanos = m.read_u64(at + 8)?;
            Ok(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
        };
        let interval = read_ns(m, new)?;
        let value = read_ns(m, new + 16)?;
        let handle = self.fds.get(fd)?.handle.clone();
        let (rem, prev_int) = handle
            .timerfd_set(value, interval, flags & crate::timerfd::TFD_TIMER_ABSTIME != 0)
            .ok_or(Errno::EINVAL)?;
        if old != 0 {
            m.write_u64(old, prev_int / 1_000_000_000)?;
            m.write_u64(old + 8, prev_int % 1_000_000_000)?;
            m.write_u64(old + 16, rem / 1_000_000_000)?;
            m.write_u64(old + 24, rem % 1_000_000_000)?;
        }
        Ok(0)
    }

    /// `timerfd_gettime`: write the time to the next expiry and the interval.
    fn sys_timerfd_gettime(&mut self, m: &mut GuestMem<'_>, fd: i32, curr: u64) -> SysResult {
        if curr == 0 {
            return Ok(0);
        }
        let handle = self.fds.get(fd)?.handle.clone();
        let (rem, interval) = handle.timerfd_get().ok_or(Errno::EINVAL)?;
        m.write_u64(curr, interval / 1_000_000_000)?;
        m.write_u64(curr + 8, interval % 1_000_000_000)?;
        m.write_u64(curr + 16, rem / 1_000_000_000)?;
        m.write_u64(curr + 24, rem % 1_000_000_000)?;
        Ok(0)
    }

    fn sys_pipe2(&mut self, m: &mut GuestMem<'_>, fds: u64, flags: u64) -> SysResult {
        // A pipe in the global kernel when there is one, because the whole
        // point of a pipe is that another process holds the other end -- and
        // with a Worker per process that other end is in another memory,
        // where an `Arc<Pipe>` means nothing.
        if let Some(r) = self.pipes {
            return self.shared_pipe2(m, r, fds, flags);
        }
        let pipe = Arc::new(Pipe::new());
        let cloexec = flags & oflags::O_CLOEXEC != 0;
        let read_end = OpenFile::new(
            Arc::new(PipeEnd {
                pipe: Arc::clone(&pipe),
                write: false,
            }),
            "pipe:[0]".to_string(),
            // The access mode belongs in the flags, not only in the two
            // booleans: `fcntl(F_GETFL)` reports it, and glibc's `fdopen`
            // refuses a descriptor whose mode does not match the one asked
            // for. A write end reporting `O_RDONLY` is why apt could not
            // open its dpkg pipe.
            flags | oflags::O_RDONLY,
            true,
            false,
        );
        let write_end = OpenFile::new(
            Arc::new(PipeEnd {
                pipe: Arc::clone(&pipe),
                write: true,
            }),
            "pipe:[0]".to_string(),
            flags | oflags::O_WRONLY,
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

    /// `pipe2` with the buffer in the global kernel.
    ///
    /// The two descriptions are the same shape as the local ones -- the same
    /// flags, the same access modes -- and differ only in what the handle is.
    /// The path carries the id and the end, because that is what a `fork` has
    /// to send across to rebuild the descriptor in the child: see
    /// `gex_fork_fds` in the browser shim.
    fn shared_pipe2(
        &mut self,
        m: &mut GuestMem<'_>,
        r: crate::pipe::Remote,
        fds: u64,
        flags: u64,
    ) -> SysResult {
        let id = (r.new)(self.pid);
        if id < 0 {
            return Err(Errno::ENFILE);
        }
        let cloexec = flags & oflags::O_CLOEXEC != 0;
        let read_end = OpenFile::new(
            Arc::new(crate::pipe::RemotePipeEnd {
                id,
                write: false,
                remote: r,
            }),
            format!("pipe:[{id}]"),
            flags | oflags::O_RDONLY,
            true,
            false,
        );
        let write_end = OpenFile::new(
            Arc::new(crate::pipe::RemotePipeEnd {
                id,
                write: true,
                remote: r,
            }),
            format!("pipe:[{id}]"),
            flags | oflags::O_WRONLY,
            false,
            true,
        );
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
            nlink: md.nlink.max(1),
            size: md.size as i64,
            blksize: 4096,
            blocks: (md.size as i64 + 511) / 512,
            mtime: md.mtime,
            // What the node says, or for a character device that does not,
            // a pseudo-terminal's number (136:0).
            rdev: if md.rdev != 0 {
                md.rdev
            } else if md.file_type == FileType::CharDevice {
                0x8800
            } else {
                0
            },
            ..Default::default()
        }
    }

    /// `statx`, how glibc stats a file on a modern kernel.
    ///
    /// Implemented rather than left to answer `ENOSYS`: the `fstatat` fallback
    /// returned the right answer but `ls` then branched into padding, so serve
    /// the path glibc actually wants.
    fn sys_statx(
        &mut self,
        m: &mut GuestMem<'_>,
        dirfd: i32,
        path: u64,
        flags: u64,
        buf: u64,
    ) -> SysResult {
        let raw = m.read_cstr(path, 4096)?;
        let md = if raw.is_empty() && flags & AT_EMPTY_PATH != 0 {
            self.fds.get(dirfd)?.metadata()?
        } else {
            let p = self.at_path(m, dirfd, path)?;
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                self.vfs.metadata_nofollow(&p)?
            } else {
                self.vfs.metadata(&p)?
            }
        };
        let st = Self::stat_from(&md);

        // `struct statx` is 256 bytes and mostly reserved.
        let mut out = [0u8; 256];
        let put32 = |o: &mut [u8; 256], at: usize, v: u32| {
            o[at..at + 4].copy_from_slice(&v.to_ge_bytes())
        };
        let put64 = |o: &mut [u8; 256], at: usize, v: u64| {
            o[at..at + 8].copy_from_slice(&v.to_ge_bytes())
        };
        // What is filled in. The caller asks for a mask and the kernel says
        // which parts it actually answered; claiming everything but the
        // birth time is what a Linux filesystem without one reports.
        const STATX_BASIC_STATS: u32 = 0x07ff;
        put32(&mut out, 0, STATX_BASIC_STATS);
        put32(&mut out, 4, st.blksize as u32);
        put32(&mut out, 16, st.nlink as u32);
        put32(&mut out, 20, self.uid);
        put32(&mut out, 24, self.gid);
        out[28..30].copy_from_slice(&(st.mode as u16).to_ge_bytes());
        put64(&mut out, 32, st.ino);
        put64(&mut out, 40, st.size as u64);
        put64(&mut out, 48, st.blocks as u64);
        // atime, btime, ctime, mtime: 16 bytes each from offset 64. Only the
        // seconds are known, and all four get the same one.
        for at in [64usize, 80, 96, 112] {
            put64(&mut out, at, st.mtime as u64);
        }
        put32(&mut out, 128, (st.rdev >> 8) as u32);
        put32(&mut out, 132, (st.rdev & 0xff) as u32);
        m.write(buf, &out)?;
        Ok(0)
    }

    fn sys_fstat(&mut self, m: &mut GuestMem<'_>, fd: i32, buf: u64) -> SysResult {
        let md = self.fds.get(fd)?.metadata()?;
        m.write(buf, &Self::stat_from(&md).encode_for(self.compat))?;
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
            self.vfs.metadata_nofollow(&p)
        } else {
            self.vfs.metadata(&p)
        };
        let md = md?;
        m.write(buf, &Self::stat_from(&md).encode_for(self.compat))?;
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
        // Open file description locks: the same story.
        const F_OFD_SETLK: u64 = 37;
        const F_OFD_SETLKW: u64 = 38;
        const F_ADD_SEALS: u64 = 1033;
        const F_GET_SEALS: u64 = 1034;
        const F_DUPFD_CLOEXEC: u64 = 1030;
        match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let file = self.fds.get(fd)?;
                self.fds.insert_at(file, arg as i32, cmd == F_DUPFD_CLOEXEC)
            }
            F_GETFD => Ok(self.fds.cloexec(fd)? as u64),
            F_SETFD => {
                self.fds.set_cloexec(fd, arg & 1 != 0)?;
                Ok(0)
            }
            // Only the access mode and the status flags. Linux does not
            // keep `O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_TMPFILE` or
            // `O_CLOEXEC` on the open file description, and reporting them
            // is how `fdopen` decided the descriptor was invalid.
            F_GETFL => {
                const STATUS: u64 = oflags::ACCMODE
                    | oflags::O_APPEND
                    | oflags::O_NONBLOCK
                    | 0o40000 // O_DIRECT
                    | 0o4010000; // O_SYNC | O_NOATIME
                Ok(self.fds.get(fd)?.flags() & STATUS)
            }
            F_SETFL => {
                self.fds.get(fd)?.set_flags(arg);
                Ok(0)
            }
            // Advisory locks always succeed: gex runs one process tree over
            // one namespace, so there is nobody to contend with.
            F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => {
                self.fds.get(fd)?;
                Ok(0)
            }
            // Sealing an anonymous shared file. Only a memfd has seals, and
            // Linux says EINVAL for anything else -- which is the answer
            // Xwayland was already getting, and it stopped rather than
            // carrying on without the guarantee.
            F_ADD_SEALS => self.memfd_of(fd)?.add_seals(arg as u32),
            F_GET_SEALS => Ok(self.memfd_of(fd)?.seals() as u64),
            _ => Err(Errno::EINVAL),
        }
    }

    fn sys_ioctl(&mut self, m: &mut GuestMem<'_>, fd: i32, req: u64, arg: u64) -> SysResult {
        use crate::tty::{self, lflag};
        // The kernel takes an `unsigned int` command: a request with the top
        // `_IOC` direction bit set (a read ioctl like TIOCGPTN or TCGETS2)
        // reaches glibc/musl as a negative `int` and is sign-extended when
        // widened into the syscall register, arriving here as `0xffffffff_8...`.
        // Masking to 32 bits is what the real kernel does and what makes those
        // read-direction ioctls match their numbers at all.
        let req = req & 0xffff_ffff;
        // The terminal's own requests. `_IOC` encodes direction and size into
        // the number, which is why the four `termios2` calls and the pty ones
        // look nothing like the rest: they are not arbitrary, they are
        // `_IOR('T', 0x2a, struct termios2)` and so on.
        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TCSETSW: u64 = 0x5403;
        const TCSETSF: u64 = 0x5404;
        const TCGETA: u64 = 0x5405;
        const TCSETA: u64 = 0x5406;
        const TCSETAW: u64 = 0x5407;
        const TCSETAF: u64 = 0x5408;
        const TCSBRK: u64 = 0x5409;
        const TCXONC: u64 = 0x540a;
        const TCFLSH: u64 = 0x540b;
        const TIOCEXCL: u64 = 0x540c;
        const TIOCNXCL: u64 = 0x540d;
        const TIOCSCTTY: u64 = 0x540e;
        const TIOCGPGRP: u64 = 0x540f;
        const TIOCSPGRP: u64 = 0x5410;
        const TIOCOUTQ: u64 = 0x5411;
        const TIOCSTI: u64 = 0x5412;
        const TIOCGWINSZ: u64 = 0x5413;
        const TIOCSWINSZ: u64 = 0x5414;
        const TIOCMGET: u64 = 0x5415;
        const TIOCMBIS: u64 = 0x5416;
        const TIOCMBIC: u64 = 0x5417;
        const TIOCMSET: u64 = 0x5418;
        const TIOCGSOFTCAR: u64 = 0x5419;
        const TIOCSSOFTCAR: u64 = 0x541a;
        const FIONREAD: u64 = 0x541b; // and TIOCINQ, which is the same number
        const TIOCLINUX: u64 = 0x541c;
        const TIOCGSERIAL: u64 = 0x541e;
        const TIOCSSERIAL: u64 = 0x541f;
        const FIONBIO: u64 = 0x5421;
        const TIOCNOTTY: u64 = 0x5422;
        const TIOCSETD: u64 = 0x5423;
        const TIOCGETD: u64 = 0x5424;
        const TCSBRKP: u64 = 0x5425;
        const TIOCGSID: u64 = 0x5429;
        const TCGETS2: u64 = 0x802c_542a;
        const TCSETS2: u64 = 0x402c_542b;
        const TCSETSW2: u64 = 0x402c_542c;
        const TCSETSF2: u64 = 0x402c_542d;
        const TIOCGPTN: u64 = 0x8004_5430;
        const TIOCSPTLCK: u64 = 0x4004_5431;
        const TIOCGDEV: u64 = 0x8004_5432;
        const TIOCSIG: u64 = 0x4004_5436;
        const TIOCVHANGUP: u64 = 0x5437;
        const TIOCGPKT: u64 = 0x8004_5438;
        const TIOCGPTLCK: u64 = 0x8004_5439;
        const TIOCGEXCL: u64 = 0x8004_5440;
        const TIOCGPTPEER: u64 = 0x5441;
        const FIONCLEX: u64 = 0x5450;
        const FIOCLEX: u64 = 0x5451;
        const FIOASYNC: u64 = 0x5452;
        const TIOCGLCKTRMIOS: u64 = 0x5456;
        const TIOCSLCKTRMIOS: u64 = 0x5457;

        let file = self.fds.get(fd)?;
        // A device the host provides answers its own ioctls, and knows things
        // the kernel does not. Asked first, because the request numbers below
        // are the terminal's and a graphics device's are its own.
        // Claiming a controlling terminal has to be recorded where the terminal
        // lives. For a local tty that is the branch further down (it has the
        // process table); for a pty in the global kernel it is the handle, which
        // is the only thing that can reach the other module. See
        // `FileHandle::claim_session`.
        if req == TIOCSCTTY {
            let sid = self.procs.sid(self.pid, 0).unwrap_or(self.pid);
            file.handle.claim_session(sid);
        }
        if let Some(r) = file.handle.ioctl(m, req, arg) {
            return r;
        }
        // The close-on-exec pair are properties of the descriptor and say
        // nothing about the device, so they answer for any file at all --
        // which is what `FIOCLEX` on a pipe expects.
        match req {
            FIOCLEX => {
                self.fds.set_cloexec(fd, true)?;
                return Ok(0);
            }
            FIONCLEX => {
                self.fds.set_cloexec(fd, false)?;
                return Ok(0);
            }
            FIONBIO => {
                let on = m.read_u32(arg)? != 0;
                let f = self.fds.get(fd)?;
                let flags = f.flags();
                f.set_flags(if on { flags | oflags::O_NONBLOCK } else { flags & !oflags::O_NONBLOCK });
                return Ok(0);
            }
            _ => {}
        }
        let is_tty = file.is_tty();
        if !is_tty {
            // Everything below is a terminal's. `FIONREAD` is the one that is
            // also asked of pipes and sockets, so it is answered for those
            // before the terminal cases begin.
            return match req {
                FIONREAD => {
                    let n = match file.handle.available() {
                        Some(n) => n,
                        None => {
                            let md = file.metadata()?;
                            md.size.saturating_sub(file.offset())
                        }
                    };
                    m.write_u32(arg, n as u32)?;
                    Ok(0)
                }
                FIOASYNC => Ok(0),
                _ => Err(Errno::ENOTTY),
            };
        }
        let tty = Arc::clone(&self.tty);
        match req {
            // Only the kernel's part of the structure: glibc's `tcgetattr`
            // passes a 36-byte `struct __kernel_termios` on the stack, and
            // writing the 60-byte userspace shape overruns it onto the stack
            // canary -- `bash -i` reported stack smashing before its prompt.
            TCGETS => {
                m.write(arg, &tty.termios()[..KERNEL_TERMIOS])?;
                Ok(0)
            }
            // Kept, not discarded. A shell that turns off ICANON and ECHO is
            // taking over line editing, and the terminal has to stop doing
            // its own or every keystroke lands twice.
            //
            // The three differ only in when they take effect -- now, after
            // the output drains, and after discarding pending input. There is
            // no output queue to drain here, so the first two are the same
            // call; the third flushes, because a program that asked for a
            // clean slate and did not get one reads the keys typed at the
            // prompt it just replaced.
            TCSETS | TCSETSW | TCSETSF => {
                // The same shape in the other direction: the caller gave a
                // kernel-sized structure, so reading past it would read the
                // caller's stack.
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                if req == TCSETSF {
                    tty.flush(tty::TCIFLUSH);
                }
                tty.set_termios(&src);
                Ok(0)
            }
            // `termios2`, which adds the two `c_ispeed`/`c_ospeed` words
            // after `c_cc`. Nothing here has a baud rate, so they are the
            // 38400 the flags already say, and the rest of the structure is
            // the one above.
            TCGETS2 => {
                let t = tty.termios();
                let mut out = [0u8; KERNEL_TERMIOS + 8];
                out[..KERNEL_TERMIOS].copy_from_slice(&t[..KERNEL_TERMIOS]);
                out[KERNEL_TERMIOS..KERNEL_TERMIOS + 4].copy_from_slice(&38400u32.to_le_bytes());
                out[KERNEL_TERMIOS + 4..].copy_from_slice(&38400u32.to_le_bytes());
                m.write(arg, &out)?;
                Ok(0)
            }
            TCSETS2 | TCSETSW2 | TCSETSF2 => {
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                if req == TCSETSF2 {
                    tty.flush(tty::TCIFLUSH);
                }
                tty.set_termios(&src);
                Ok(0)
            }
            // The `termio` of System V, which is `termios` with one-byte
            // flags. Ancient, and still what `busybox stty` falls back to on
            // a kernel that refuses `TCGETS`.
            TCGETA => {
                let t = tty.termios();
                let mut out = [0u8; 8 + 8];
                out[0..2].copy_from_slice(&t[0..2]);
                out[2..4].copy_from_slice(&t[4..6]);
                out[4..6].copy_from_slice(&t[8..10]);
                out[6..8].copy_from_slice(&t[12..14]);
                out[8] = t[16]; // c_line
                out[9..9 + 7].copy_from_slice(&t[17..17 + 7]);
                m.write(arg, &out)?;
                Ok(0)
            }
            TCSETA | TCSETAW | TCSETAF => {
                let src = m.read(arg, 16)?.to_vec();
                let mut t = tty.termios();
                t[0..2].copy_from_slice(&src[0..2]);
                t[2..4].fill(0);
                t[4..6].copy_from_slice(&src[2..4]);
                t[6..8].fill(0);
                t[8..10].copy_from_slice(&src[4..6]);
                t[10..12].fill(0);
                t[12..14].copy_from_slice(&src[6..8]);
                t[14..16].fill(0);
                t[16] = src[8];
                t[17..17 + 7].copy_from_slice(&src[9..9 + 7]);
                tty.set_termios(&t);
                Ok(0)
            }
            // The lock is answered and not enforced. A program that reads it
            // back is checking whether it may change a setting; all-zero says
            // it may, which is true.
            TIOCGLCKTRMIOS => {
                m.write(arg, &tty.locked_termios()[..KERNEL_TERMIOS])?;
                Ok(0)
            }
            TIOCSLCKTRMIOS => {
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                tty.set_locked_termios(&src);
                Ok(0)
            }
            TIOCGWINSZ => {
                let (rows, cols, x, y) = tty.winsize();
                let mut ws = [0u8; 8];
                ws[0..2].copy_from_slice(&rows.to_ge_bytes());
                ws[2..4].copy_from_slice(&cols.to_ge_bytes());
                ws[4..6].copy_from_slice(&x.to_ge_bytes());
                ws[6..8].copy_from_slice(&y.to_ge_bytes());
                m.write(arg, &ws)?;
                Ok(0)
            }
            // A program may resize the terminal too -- `resize` does, and so
            // does a terminal multiplexer telling its children how big their
            // pane is. Everyone in the foreground group hears about it.
            TIOCSWINSZ => {
                let ws = m.read(arg, 8)?.to_vec();
                let g = |n: usize| u16::from_ge_bytes([ws[n], ws[n + 1]]);
                if tty.set_winsize((g(0), g(2), g(4), g(6))) {
                    self.signal_foreground(crate::signal::sig::SIGWINCH);
                }
                Ok(0)
            }
            // Who is in the foreground. Zero before any shell has claimed the
            // terminal, and Linux answers with a group that does not exist
            // rather than with an error -- but the caller is a better guess
            // than a lie, and it is what `tcgetpgrp` in a script wants.
            TIOCGPGRP => {
                let fg = tty.fg();
                let g = if fg > 0 {
                    fg
                } else {
                    self.procs.pgid(self.pid, 0).unwrap_or(self.pid)
                };
                m.write_u32(arg, g as u32)?;
                Ok(0)
            }
            // A shell handing the terminal to a job. This is what makes job
            // control real: the group named here is the one `^C` reaches, so
            // a shell that backgrounds a pipeline stops it being interrupted
            // and the shell itself starts being.
            TIOCSPGRP => {
                let pgrp = m.read_u32(arg)? as i32;
                if pgrp <= 0 {
                    return Err(Errno::EINVAL);
                }
                tty.set_fg(pgrp);
                Ok(0)
            }
            TIOCGSID => {
                let sid = tty.sid();
                let s = if sid > 0 { sid } else { self.procs.sid(self.pid, 0).unwrap_or(1) };
                m.write_u32(arg, s as u32)?;
                Ok(0)
            }
            // Claiming the terminal. A session leader that has none takes it,
            // and becomes the foreground group by doing so.
            TIOCSCTTY => {
                let sid = self.procs.sid(self.pid, 0).unwrap_or(self.pid);
                let held = tty.sid();
                // `arg` non-zero is the steal-it-anyway form, which only root
                // may use -- and here everybody is root often enough that
                // refusing it would be the surprising answer.
                if held != 0 && held != sid && arg == 0 {
                    return Err(Errno::EPERM);
                }
                tty.set_sid(sid);
                Ok(0)
            }
            TIOCNOTTY => {
                if tty.sid() == self.procs.sid(self.pid, 0).unwrap_or(-1) {
                    tty.set_sid(0);
                }
                Ok(0)
            }
            // Throw away what has been typed and not read, what has been
            // written and not displayed, or both. There is no output queue --
            // a write reaches the page before the call returns -- so only the
            // input half does anything.
            TCFLSH => {
                let which = arg as i32;
                if which == tty::TCIFLUSH || which == tty::TCIOFLUSH {
                    tty.flush(tty::TCIFLUSH);
                }
                Ok(0)
            }
            // Send a break, or wait for output to drain. Both are `tcdrain`
            // and `tcsendbreak` underneath, and both are satisfied the moment
            // they are asked: there is nothing in flight.
            TCSBRK | TCSBRKP | TIOCVHANGUP => Ok(0),
            // Flow control by hand: `tcflow(TCOOFF)` and friends. The same
            // switch `^S` throws, which is why it is the terminal's and not
            // this process's.
            TCXONC => {
                const TCOOFF: u64 = 0;
                const TCOON: u64 = 1;
                const TCIOFF: u64 = 2;
                const TCION: u64 = 3;
                match arg {
                    TCOOFF => tty.set_stopped(true),
                    TCOON => tty.set_stopped(false),
                    // Sending the stop and start characters upstream. There
                    // is no upstream: the keyboard is the page.
                    TCIOFF | TCION => {}
                    _ => return Err(Errno::EINVAL),
                }
                Ok(0)
            }
            // Bytes typed and not yet read. The terminal knows, because the
            // queue is its own; the descriptor's own answer is the fallback
            // for a run with no shared terminal.
            FIONREAD => {
                let n = match tty.queued() {
                    Some(n) if n >= 0 => n as u64,
                    _ => file.handle.available().unwrap_or(0),
                };
                m.write_u32(arg, n as u32)?;
                Ok(0)
            }
            // Bytes written and not yet displayed, which is always none.
            TIOCOUTQ => {
                m.write_u32(arg, 0)?;
                Ok(0)
            }
            // Push a byte back as though it had been typed. `TIOCSTI` is how
            // a shell puts a command on the next prompt, and how `su` used to
            // be escaped from -- Linux now wants `CAP_SYS_ADMIN` for it. Here
            // there is one terminal and one person at it.
            TIOCSTI => {
                let b = m.read(arg, 1)?.to_vec();
                tty.push(&b);
                Ok(0)
            }
            // Exclusive use. Nothing enforces it -- there is one page and one
            // terminal -- but a program that sets it reads it back.
            TIOCEXCL => {
                tty.set_exclusive(true);
                Ok(0)
            }
            TIOCNXCL => {
                tty.set_exclusive(false);
                Ok(0)
            }
            TIOCGEXCL => {
                m.write_u32(arg, tty.exclusive() as u32)?;
                Ok(0)
            }
            // The line discipline. `N_TTY` and nothing else; see
            // `TtyRef::set_ldisc` for why the others are refused rather than
            // accepted and ignored.
            TIOCGETD => {
                m.write_u32(arg, tty.ldisc() as u32)?;
                Ok(0)
            }
            TIOCSETD => {
                let n = m.read_u32(arg)? as i32;
                if tty.set_ldisc(n) {
                    Ok(0)
                } else {
                    Err(Errno::EINVAL)
                }
            }
            // Modem lines. There is no modem, and the honest answer for a
            // terminal that is always connected is carrier plus the two
            // ready lines: DTR | RTS | CTS | CAR | DSR.
            TIOCMGET => {
                const TIOCM_DTR: u32 = 0x002;
                const TIOCM_RTS: u32 = 0x004;
                const TIOCM_CTS: u32 = 0x020;
                const TIOCM_CAR: u32 = 0x040;
                const TIOCM_DSR: u32 = 0x100;
                m.write_u32(arg, TIOCM_DTR | TIOCM_RTS | TIOCM_CTS | TIOCM_CAR | TIOCM_DSR)?;
                Ok(0)
            }
            TIOCMSET | TIOCMBIS | TIOCMBIC => Ok(0),
            TIOCGSOFTCAR => {
                m.write_u32(arg, tty.softcar() as u32)?;
                Ok(0)
            }
            TIOCSSOFTCAR => {
                tty.set_softcar(m.read_u32(arg)? != 0);
                Ok(0)
            }
            // Send a signal to the foreground group, which is what the master
            // end of a pty does for `^C`. Reachable here because the page is
            // the master end.
            TIOCSIG => {
                let signo = m.read_u32(arg)?;
                if signo == 0 || signo > crate::signal::sig::MAX {
                    return Err(Errno::EINVAL);
                }
                self.signal_foreground(signo);
                Ok(0)
            }
            // Asynchronous I/O by SIGIO. Accepted and not armed: nothing here
            // has ever sent one, and a program that asks is asking to be told
            // sooner, not to be told differently.
            FIOASYNC => Ok(0),
            // The pty ones. This terminal is not one half of a pair -- it is
            // the console -- so the number and the lock have no meaning, and
            // `EINVAL` is what Linux says for them on a console too.
            TIOCGPTN | TIOCSPTLCK | TIOCGPTLCK | TIOCGPTPEER | TIOCGPKT => Err(Errno::EINVAL),
            // The device number, for a program tracking which terminal it is
            // on. `/dev/console` is 5:1.
            TIOCGDEV => {
                m.write_u32(arg, (5 << 8) | 1)?;
                Ok(0)
            }
            // A serial port's parameters. There is no serial port; refusing
            // is what `setserial` on a console gets.
            TIOCGSERIAL | TIOCSSERIAL => Err(Errno::ENOTTY),
            // The console multiplexer's own call: screen dumps, selection,
            // keyboard mode. Only the "is the console blanked" subcode has an
            // answer here, and it is no.
            TIOCLINUX => Err(Errno::EINVAL),
            _ => {
                // Worth saying rather than swallowing: an unknown terminal
                // request is usually a feature the guest wanted and quietly
                // did without. `stty` asking for something absent looks
                // exactly like `stty` reporting the wrong thing.
                let _ = lflag::ISIG;
                Err(Errno::ENOTTY)
            }
        }
    }

    /// Raise `signo` on whoever holds the terminal.
    ///
    /// The foreground process group if a shell has set one, and otherwise
    /// this process's own group -- which is right for the case that has no
    /// shell in it at all, a program started directly on the terminal.
    fn signal_foreground(&mut self, signo: u32) {
        let fg = self.tty.fg();
        let g = if fg > 0 {
            fg
        } else {
            self.procs.pgid(self.pid, 0).unwrap_or(self.pid)
        };
        // `kill_group` and not `kill(-g)`: a negative target of -1 is
        // POSIX's "every process", so group 1 is the one group that cannot
        // be named negatively -- and a shell running as pid 1 is in it.
        let _ = self.procs.kill_group(self.pid, g, signo);
    }

    fn sys_faccessat(&mut self, m: &GuestMem<'_>, dirfd: i32, path: u64, mode: u32) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        // A root guest (`--uid 0`) may read and write anything; execute
        // still needs an execute bit, which the host checks.
        let mode = if self.uid == 0 { mode & 1 } else { mode & 7 };
        self.vfs.access(&p, mode).map(|_| 0)
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

    /// `fchdir`: the working directory named by a descriptor.
    ///
    /// The descriptor already knows the absolute path it was opened as, and
    /// `set_cwd` re-checks it is a directory (the error a descriptor on a
    /// regular file must produce). coreutils' `mkdir -p` needs this: gnulib's
    /// `savewd` returns between components with `fchdir`.
    fn sys_fchdir(&mut self, fd: i32) -> SysResult {
        let file = self.fds.get(fd)?;
        let path = file.path.clone();
        // A pipe or a socket has a name that is not a path. `ENOTDIR` is
        // what Linux says about a descriptor that is not a directory.
        if !path.starts_with('/') {
            return Err(Errno::ENOTDIR);
        }
        self.vfs.set_cwd(&path).map(|_| 0)
    }

    fn sys_mkdirat(&mut self, m: &GuestMem<'_>, dirfd: i32, path: u64, mode: u32) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        self.vfs.mkdir(&p, mode & !self.umask).map(|_| 0)
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

    /// `renameat2`: `renameat` plus a flags word. The flags matter -- programs
    /// reach for this syscall specifically to get the atomic guarantee, so
    /// silently ignoring them would corrupt the very race they were avoiding.
    ///
    /// - `flags == 0` is plain `renameat`.
    /// - `RENAME_NOREPLACE` (1): fail with `EEXIST` if the destination already
    ///   exists, rather than clobbering it -- the atomic create-or-fail idiom.
    ///   The KLock serialises syscalls, so the exists-check and rename are not
    ///   interleaved by another guest thread.
    /// - `RENAME_EXCHANGE` (2) and `RENAME_WHITEOUT` (4) are not something this
    ///   VFS can do atomically, so they get `EINVAL` -- the same answer Linux
    ///   gives on a filesystem that does not support them.
    fn sys_renameat2(
        &mut self,
        m: &GuestMem<'_>,
        old_fd: i32,
        old: u64,
        new_fd: i32,
        new: u64,
        flags: u32,
    ) -> SysResult {
        const RENAME_NOREPLACE: u32 = 1;
        const RENAME_EXCHANGE: u32 = 2;
        let from = self.at_path(m, old_fd, old)?;
        let to = self.at_path(m, new_fd, new)?;
        if flags & !RENAME_NOREPLACE != 0 {
            // EXCHANGE, WHITEOUT, or any unknown bit: unsupported here.
            let _ = RENAME_EXCHANGE;
            return Err(Errno::EINVAL);
        }
        if flags & RENAME_NOREPLACE != 0 && self.vfs.metadata_nofollow(&to).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.vfs.rename(&from, &to).map(|_| 0)
    }

    /// `utimensat(dirfd, path, times, flags)`: set a file's timestamps.
    ///
    /// gex keeps one time per file, the modification time (`stat` reports
    /// it for all three), so the access time in `times` is checked and
    /// dropped. `times` NULL or `UTIME_NOW` means now; `UTIME_OMIT` leaves
    /// the time alone. A missing path is `ENOENT`, which `touch` relies on
    /// to create the file. A NULL path is `futimens` on `dirfd`, whose
    /// descriptor is validated; its time is not kept (an open file's name
    /// is not tracked).
    fn sys_utimensat(&self, m: &GuestMem<'_>, dirfd: i32, path: u64, times: u64, flags: u32) -> SysResult {
        const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
        const UTIME_NOW: u64 = (1 << 30) - 1;
        const UTIME_OMIT: u64 = (1 << 30) - 2;
        // The modification time to set, if any.
        let mtime = if times == 0 {
            Some((self.clock)().0 as i64)
        } else {
            for t in [times, times + 16] {
                let ns = m.read_u64(t + 8)?;
                if ns >= 1_000_000_000 && ns != UTIME_NOW && ns != UTIME_OMIT {
                    return Err(Errno::EINVAL);
                }
            }
            match m.read_u64(times + 24)? {
                UTIME_OMIT => None,
                UTIME_NOW => Some((self.clock)().0 as i64),
                _ => Some(m.read_u64(times + 16)? as i64),
            }
        };
        if path == 0 {
            // futimens: the "path" is really the open descriptor in `dirfd`.
            self.fds.get(dirfd)?;
            return Ok(0);
        }
        let p = self.at_path(m, dirfd, path)?;
        let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
        if follow {
            self.vfs.metadata(&p)?;
        } else {
            self.vfs.metadata_nofollow(&p)?;
        }
        if let Some(t) = mtime {
            self.vfs.set_mtime(&p, t, follow)?;
        }
        Ok(0)
    }

    /// `symlinkat(target, newdirfd, linkpath)`. The link path resolves
    /// against `newdirfd`, not the working directory -- tar works relative to
    /// a descriptor it just created, and against the cwd that is `ENOENT`.
    fn sys_symlinkat(
        &mut self,
        m: &GuestMem<'_>,
        target: u64,
        newdirfd: i32,
        path: u64,
    ) -> SysResult {
        let t = String::from_utf8(m.read_cstr(target, 4096)?).map_err(|_| Errno::EINVAL)?;
        let p = self.at_path(m, newdirfd, path)?;
        self.vfs.symlink(&t, &p).map(|_| 0)
    }

    /// Read the guest's `pollfd` array into something the host can re-check.
    fn collect_pollfds(&self, m: &GuestMem<'_>, fds: u64, nfds: u64) -> Option<Vec<PollFd>> {
        let mut out = Vec::with_capacity(nfds.min(1024) as usize);
        for i in 0..nfds.min(1024) {
            let addr = fds + i * 8;
            let fd = m.read_u32(addr).ok()? as i32;
            let events = (m.read_u32(addr + 4).ok()? & 0xffff) as u16;
            let file = self.fds.get(fd).ok();
            out.push(PollFd {
                addr,
                fd,
                events,
                handle: file.as_ref().map(|f| Arc::clone(&f.handle)),
                // The open mode only. Whether a write would *block* is
                // asked of the handle in `poll_revents`, every time, because
                // a parked poll outlives the state it was built from.
                writable: file.as_ref().is_some_and(|f| f.writable),
            });
        }
        Some(out)
    }

    /// Read `select`'s three bitmaps into the same list `poll` uses.
    ///
    /// The two calls ask the same question. Only the reply differs, so only
    /// the reply is written twice.
    fn collect_selectfds(
        &self,
        m: &GuestMem<'_>,
        nfds: u32,
        readfds: u64,
        writefds: u64,
        exceptfds: u64,
    ) -> Option<Vec<PollFd>> {
        const POLLIN: u16 = 0x001;
        const POLLOUT: u16 = 0x004;
        let nfds = nfds.min(1024);
        let mut want: std::collections::BTreeMap<i32, u16> = Default::default();
        for (set, ev) in [(readfds, POLLIN), (writefds, POLLOUT)] {
            if set == 0 {
                continue;
            }
            for w in 0..(nfds as usize).div_ceil(64) {
                let word = m.read_bits64(set + (w as u64) * 8).ok()?;
                if word == 0 {
                    continue;
                }
                for bit in 0..64u32 {
                    let fd = (w as u32) * 64 + bit;
                    if fd >= nfds {
                        break;
                    }
                    if word & (1 << bit) != 0 {
                        *want.entry(fd as i32).or_default() |= ev;
                    }
                }
            }
        }
        // An exceptional condition never happens here, so a descriptor named
        // only in that set is watched for nothing and simply never fires.
        let _ = exceptfds;
        Some(
            want.into_iter()
                .map(|(fd, events)| {
                    let file = self.fds.get(fd).ok();
                    PollFd {
                        addr: 0,
                        fd,
                        events,
                        handle: file.as_ref().map(|f| Arc::clone(&f.handle)),
                        // The open mode only. Whether a write would *block* is
                // asked of the handle in `poll_revents`, every time, because
                // a parked poll outlives the state it was built from.
                writable: file.as_ref().is_some_and(|f| f.writable),
                    }
                })
                .collect(),
        )
    }

    /// Write a shared mapping's bytes back to the file behind it.
    fn flush_shared(&mut self, m: &GuestMem<'_>, addr: u64, len: u64) -> Result<(), Errno> {
        if self.shared_maps.is_empty() {
            return Ok(());
        }
        let (lo, hi) = (addr, addr.saturating_add(len));
        for map in &self.shared_maps {
            let from = map.start.max(lo);
            let to = (map.start + map.len).min(hi);
            if from >= to {
                continue;
            }
            // Only what the file holds: a shared mapping's page past the
            // end of the file is not the file's (Linux never extends it).
            let at = map.offset + (from - map.start);
            let size = map.file.metadata().map(|md| md.size).unwrap_or(u64::MAX);
            let n = (to - from).min(size.saturating_sub(at));
            if n == 0 {
                continue;
            }
            let bytes = m.read(from, n as usize)?;
            map.file.pwrite(at, bytes)?;
        }
        Ok(())
    }

    /// `fchmod` and `fchmodat`. The first names an open file, whose path the
    /// descriptor kept; the second names a path directly.
    fn sys_chmod(&mut self, m: &GuestMem<'_>, n: u64, a: [u64; 6]) -> SysResult {
        let (path, mode, flags) = if n == nr::FCHMOD {
            (self.fds.get(a[0] as i32)?.path.clone(), a[1] as u32, 0)
        } else {
            (self.at_path(m, a[0] as i32, a[1])?, a[2] as u32, a[3])
        };
        // `chmod` on a symlink itself is `EOPNOTSUPP`, as on Linux: a symlink
        // has no mode of its own. Following the link would change a different
        // file, and answers `ENOENT` when the target does not exist yet --
        // which tar hits unpacking a package with a dangling symlink.
        if self
            .vfs
            .metadata_nofollow(&path)
            .is_ok_and(|m| m.file_type == FileType::Symlink)
            && (flags & AT_SYMLINK_NOFOLLOW != 0 || n == nr::FCHMOD)
        {
            return Err(Errno::EOPNOTSUPP);
        }
        self.vfs.chmod(&path, mode).map(|_| 0)
    }

    /// Nanoseconds on the guest's clock, for comparing deadlines against.
    pub fn now_nanos(&self) -> u128 {
        let (secs, nanos) = (self.clock)();
        secs as u128 * 1_000_000_000 + nanos as u128
    }

    /// `setitimer` and `getitimer`. A `struct itimerval` is two `timeval`s
    /// (reload interval then time remaining), each two 64-bit words of seconds
    /// and *microseconds*: 32 bytes. These actually fire -- `openssl speed`
    /// runs its loop until SIGALRM and nothing else stops it.
    fn sys_itimer(
        &mut self,
        m: &mut GuestMem<'_>,
        which: u64,
        new: u64,
        old: u64,
        set: bool,
    ) -> SysResult {
        let which = which as usize;
        if which > ITIMER_PROF {
            return Err(Errno::EINVAL);
        }
        let now = self.now_nanos();
        // What is left, which both calls may be asked to report.
        if old != 0 {
            let (interval, remaining) = match self.itimers[which] {
                Some(t) => (t.interval, t.next.saturating_sub(now)),
                None => (0, 0),
            };
            write_timeval(m, old, interval)?;
            write_timeval(m, old + 16, remaining)?;
        }
        if !set {
            return Ok(0);
        }
        if new == 0 {
            return Err(Errno::EFAULT);
        }
        let interval = read_timeval(m, new)?;
        let value = read_timeval(m, new + 16)?;
        // A zero `it_value` disarms, whatever the interval says.
        self.itimers[which] = if value == 0 {
            None
        } else {
            Some(Itimer {
                next: now + value,
                interval,
            })
        };
        Ok(0)
    }

    /// Whether an interval timer has expired and has yet to raise its signal.
    /// The host asks this on its way past: a process that never traps into
    /// the kernel (a shell's `timeout` spinning on `wait4`/`sigsuspend`) would
    /// otherwise never have its `alarm` delivered, because delivery is only
    /// attempted once a signal is already pending.
    pub fn timer_due(&mut self) -> bool {
        if self.itimers.iter().all(|t| t.is_none()) {
            return false;
        }
        let now = self.now_nanos();
        self.itimers.iter().flatten().any(|t| now >= t.next)
    }

    /// Fire any timer whose deadline has passed, and reload it if it repeats.
    ///
    /// Called at every back-edge safepoint -- so a timer does not fire while
    /// the guest is blocked in the host (no back-edges until the call
    /// returns). `ITIMER_VIRTUAL`/`ITIMER_PROF` should count CPU time but are
    /// driven from the same wall clock as the rest.
    pub fn check_timers(&mut self) {
        if self.itimers.iter().all(|t| t.is_none()) {
            return;
        }
        let now = self.now_nanos();
        for which in 0..3 {
            let Some(timer) = self.itimers[which] else {
                continue;
            };
            if now < timer.next {
                continue;
            }
            self.itimers[which] = if timer.interval == 0 {
                None
            } else {
                // Skip whole missed periods rather than firing once per
                // period afterwards: a guest that was away for a second with
                // a millisecond timer does not want a thousand signals.
                let missed = (now - timer.next) / timer.interval + 1;
                Some(Itimer {
                    next: timer.next + missed * timer.interval,
                    interval: timer.interval,
                })
            };
            self.signals.raise_shared(
                match which {
                    ITIMER_VIRTUAL => signal::sig::SIGVTALRM,
                    ITIMER_PROF => signal::sig::SIGPROF,
                    _ => signal::sig::SIGALRM,
                },
                None,
            );
        }
    }

    /// `sched_getaffinity`. One processor, and it is the one the caller is
    /// already on. dpkg asks in order to size its worker pool.
    fn sys_getaffinity(&mut self, m: &mut GuestMem<'_>, len: u64, mask: u64) -> SysResult {
        let len = (len as usize).min(1024);
        if mask != 0 && len >= 8 {
            m.zero(mask, len)?;
            m.write_u64(mask, 1)?;
        }
        Ok(8)
    }

    /// `tkill(tid, sig)` and `tgkill(tgid, tid, sig)`: a thread-directed
    /// signal goes to that thread. Mono's GC stops the world with
    /// `tgkill(SIGPWR)` to each thread, and raising it on the caller instead
    /// left the target running and the GC waiting.
    fn sys_tgkill(&mut self, n: u64, a: [u64; 6]) -> SysResult {
        let (tgid, tid, signo) = if n == nr::TKILL {
            (self.pid, a[0] as i32, a[1] as u32)
        } else {
            (a[0] as i32, a[1] as i32, a[2] as u32)
        };
        if signo > crate::signal::sig::MAX || tid <= 0 {
            return Err(Errno::EINVAL);
        }
        if tgid != self.pid {
            // Another process's thread: its process, as `kill` does.
            return self.procs.kill(self.pid, tgid, signo).map(|_| 0);
        }
        let source = crate::signal::Source { code: crate::signal::SI_TKILL, pid: self.pid, uid: self.uid };
        if tid == self.tid {
            if signo > 0 {
                self.signals.raise_from(signo, source);
            }
        } else {
            let Some(t) = self.thread_signals.get_mut(&tid) else { return Err(Errno::ESRCH) };
            if signo > 0 {
                t.raise_from(signo, source);
            }
        }
        Ok(0)
    }

    /// `sched_getparam(pid, *param)`: priority 0, the only one there is.
    fn sys_sched_getparam(&mut self, m: &mut GuestMem<'_>, param: u64) -> SysResult {
        m.write_u32(param, 0)?;
        Ok(0)
    }

    /// `statfs`. One filesystem, large and mostly empty, which is what a
    /// program checking for room before it writes wants to hear.
    fn sys_statfs(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf == 0 {
            return Err(Errno::EFAULT);
        }
        let mut out = [0u8; 120];
        let put = |o: &mut [u8; 120], at: usize, v: u64| {
            o[at..at + 8].copy_from_slice(&v.to_le_bytes())
        };
        put(&mut out, 0, 0x0102_1994); // f_type: TMPFS_MAGIC
        put(&mut out, 8, 4096); // f_bsize
        put(&mut out, 16, 1 << 20); // f_blocks
        put(&mut out, 24, 1 << 19); // f_bfree
        put(&mut out, 32, 1 << 19); // f_bavail
        put(&mut out, 40, 1 << 18); // f_files
        put(&mut out, 48, 1 << 17); // f_ffree
        put(&mut out, 72, 255); // f_namelen
        put(&mut out, 80, 4096); // f_frsize
        m.write(buf, &out)?;
        Ok(0)
    }

    /// `pselect6`, for the case where something is ready already. The waiting
    /// case is a [`BlockOp::Poll`], decided before this is reached.
    fn sys_pselect(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let set = self
            .collect_selectfds(m, a[0] as u32, a[1], a[2], a[3])
            .ok_or(Errno::EFAULT)?;
        poll_finish(
            &set,
            &PollReport::Select {
                nfds: a[0] as u32,
                readfds: a[1],
                writefds: a[2],
                exceptfds: a[3],
            },
            m,
        )
    }

    /// `ppoll`, for the case where something is ready already. The waiting
    /// case is a [`BlockOp::Poll`], decided before this is reached.
    ///
    /// This used to report `POLLIN` for any descriptor open for reading,
    /// which is true of a file and a lie about a socket: a caller polling for
    /// data that has not arrived was told it had, read nothing, and polled
    /// again. It asks the handle now.
    fn sys_ppoll(&mut self, m: &mut GuestMem<'_>, fds: u64, nfds: u64) -> SysResult {
        let set = self.collect_pollfds(m, fds, nfds).ok_or(Errno::EFAULT)?;
        poll_finish(&set, &PollReport::Poll, m)
    }

    // -- memory --------------------------------------------------------------

    /// `mremap`. The map decides where; this moves the bytes.
    fn sys_mremap(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        const MREMAP_MAYMOVE: u64 = 1;
        let (addr, old_len, new_len, flags) = (a[0], a[1], a[2], a[3]);
        let (dst, moved) = self
            .mem
            .mremap(addr, old_len, new_len, flags & MREMAP_MAYMOVE != 0)?;
        if moved {
            let keep = old_len.min(new_len) as usize;
            let bytes = m.read(addr, keep)?.to_vec();
            m.write(dst, &bytes)?;
            // The grown tail at the new location is fresh anonymous memory,
            // which Linux exposes as zero -- but `dst` may be a reused hole
            // holding stale bytes, and only `keep` of them were overwritten.
            if new_len > old_len {
                m.zero(dst + keep as u64, (new_len - keep as u64) as usize)?;
            }
        } else if new_len > old_len {
            // Growing in place must expose zeroes, not whatever the guest
            // left behind the last time this range was mapped.
            m.zero(addr + old_len, (new_len - old_len) as usize)?;
        }
        Ok(dst)
    }

    /// The pinned base for a path a closure build named, if there is one.
    ///
    /// Exact spelling first, then the file's name alone: `gexc` records the
    /// path it walked in its sysroot while `ld.so` opens what `ld.so.cache`
    /// gives it, and on Debian those differ by a symlinked prefix. The
    /// name-only fallback is safe because a rootfs does not ship two libraries
    /// of one name that a single program loads both of; the exact match still
    /// wins.
    pub fn pin_for(&self, path: &str) -> Option<u64> {
        if let Some(&base) = self.pins.get(path) {
            return Some(base);
        }
        let want = path.rsplit('/').next()?;
        self.pins
            .iter()
            .find(|(k, _)| k.rsplit('/').next() == Some(want))
            .map(|(_, &v)| v)
    }

    /// `madvise`. Only `MADV_DONTNEED` does anything: on Linux it makes an
    /// anonymous range read as zero on next access, and glibc's malloc frees
    /// arena memory this way and then REUSES it trusting the zero. A no-op left
    /// the reused pages holding stale bytes, so glibc read garbage -- a
    /// deterministic corruption Qt hits (heavy glibc malloc/threads) and musl
    /// does not (it frees via `munmap`, which is re-zeroed on remap). Only
    /// anonymous VMAs are zeroed; a file mapping's `MADV_DONTNEED` reloads from
    /// the file, which the demand path already does, so it is left alone.
    fn sys_madvise(&mut self, m: &mut GuestMem<'_>, addr: u64, len: u64, advice: u32) -> SysResult {
        const MADV_DONTNEED: u32 = 4;
        if advice == MADV_DONTNEED {
            let end = addr.saturating_add(len);
            let anon: Vec<(u64, u64)> = self
                .mem
                .vmas()
                .iter()
                .filter(|v| v.origin.is_none() && v.start < end && addr < v.end)
                .map(|v| (v.start.max(addr), v.end.min(end)))
                .collect();
            for (lo, hi) in anon {
                if hi > lo {
                    m.zero(lo, (hi - lo) as usize)?;
                }
            }
        }
        Ok(0)
    }

    fn sys_mmap(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (addr, len, prot_bits, flags, fd, offset) =
            (a[0], a[1], a[2] as u32, a[3], a[4] as i32, a[5]);
        // A fixed mapping over code replaces it, as an unmap would (Wine's
        // VirtualFree decommits by mapping over the range).
        if flags & mflags::FIXED != 0 {
            self.retire_code(addr, len);
            // And a shared file mapping it replaces ends as an unmap ends
            // it: written back, and no longer handed to the next mapper.
            if self.shared_maps.iter().any(|s| s.start < addr + len && s.start + s.len > addr) {
                self.flush_shared(m, addr, len)?;
                self.shared_maps.retain(|s| s.start + s.len <= addr || s.start >= addr + len);
            }
        }
        if flags & mflags::ANONYMOUS == 0 && fd >= 0 {
            // A private file mapping is an eager copy, which is exactly
            // correct and needs no page faults (PLAN §7.4).
            let len_aligned = crate::mem::page_align_up(len);
            let file = self.fds.get(fd)?;
            let name = file.path.clone();
            // A loader that maps an object's first segment straight from the
            // file, spanning the whole object (glibc 2.35 and later), rather
            // than reserving anonymous memory first: that mapping takes the
            // pin armed when the object was opened, the same way.
            let mut addr = addr;
            let mut pinned = None;
            if addr == 0 && flags & crate::mem::flags::FIXED == 0 {
                if let Some((path, base)) = self.armed_pin.take() {
                    if path.rsplit('/').next() == name.rsplit('/').next() {
                        self.armed_place = None;
                        addr = base;
                        pinned = Some((path, base));
                    } else {
                        self.armed_pin = Some((path, base));
                    }
                }
            }
            // A second `MAP_SHARED` of the same open file must see the first
            // one's writes, so hand back the same pages -- a Wayland client
            // maps a buffer and sends the descriptor, and the next mapper must
            // see the same pixels, not a zeroed copy. Not reference counted:
            // unmapping one alias unmaps both, and nothing here unmaps early.
            if flags & crate::mem::flags::SHARED != 0 {
                // (or any part of one: a window into a mapping another
                // view holds whole, as win32u maps the session's blocks)
                if let Some(existing) = self.shared_maps.iter().find(|s| {
                    Arc::ptr_eq(&s.file, &file) && s.offset <= offset && offset + len_aligned <= s.offset + s.len
                }) {
                    return Ok(existing.start + (offset - existing.offset));
                }
            }
            // The path and offset travel with the mapping. Two processes
            // that map the same library at the same address then agree that
            // the code is the same, which is what lets them share the
            // dispatch slots it was translated into.
            let start = self.mem.mmap_from(
                addr,
                len_aligned,
                prot_bits,
                flags,
                &name,
                Some((name.clone(), offset)),
            )?;
            if let Some((path, base)) = pinned {
                // Said as it happens: the end-of-run report is lost when the
                // process is killed (a game run ends at its timeout).
                if base != start {
                    eprintln!("[pin] {path} WANTED {base:#x} GOT {start:#x}");
                }
                self.pin_landed.push((path, base, start));
            }
            let mut tmp = vec![0u8; len as usize];
            let n = file.pread(offset, &mut tmp)? as usize;
            m.zero(start, len_aligned as usize)?;
            m.write(start, &tmp[..n])?;
            // A shared mapping is written back at `msync` and `munmap` (where
            // a program that never calls `msync` expects its changes to land).
            // An executable file mapping is a shared library arriving: tell the
            // host so the static recompiler gets the whole object at once.
            if prot_bits & crate::mem::prot::EXEC != 0 {
                crate::note_code_change();
                self.new_code.push(CodeMap {
                    start,
                    end: start + len_aligned,
                    path: name.clone(),
                    offset,
                });
            }
            if flags & crate::mem::flags::SHARED != 0 {
                self.shared_maps.push(SharedMap {
                    start,
                    len: len_aligned,
                    file,
                    offset,
                });
            }
            return Ok(start);
        }
        // The anonymous PROT_NONE reservation an ELF loader makes before it
        // maps an object: hand it the pinned base and every `MAP_FIXED` that
        // follows lands where the closure build assumed. Passed as a hint, so
        // a collision is ignored and recorded in `pin_landed`, not forced.
        let mut addr = addr;
        let mut pinned = None;
        if addr == 0 && prot_bits == 0 && flags & mflags::ANONYMOUS != 0 {
            if let Some((path, base)) = self.armed_pin.take() {
                self.armed_place = None;
                addr = base;
                pinned = Some((path, base));
            } else if let Some(path) = self.armed_place.take() {
                // The reservation is the first thing that says how long the
                // object is, which is why the question is asked here and not
                // at the `openat` that armed it.
                if let Some(base) = self
                    .place
                    .as_ref()
                    .and_then(|p| p.place(&path, crate::mem::page_align_up(len)))
                {
                    addr = base;
                }
            }
        }
        // The dynamic loader's reservation for an object (glibc maps an
        // over-aligned library into anonymous PROT_NONE memory) will hold
        // code, so in a 64-bit memory it stays below 4 GiB with the code; the
        // loader runs at INTERP_BASE (docs/mem64.md §3).
        let from_loader = (gex_abi::INTERP_BASE..gex_abi::STACK_TOP - gex_abi::STACK_SIZE).contains(&self.syscall_pc);
        let start = if from_loader || (cfg!(feature = "x86") && flags & mflags::MAP_32BIT != 0) {
            self.mem.mmap_low(addr, len, prot_bits, flags & !mflags::MAP_32BIT, "")?
        } else {
            self.mem.mmap(addr, len, prot_bits, flags, "")?
        };
        if let Some((path, base)) = pinned {
            // Said as it happens: the end-of-run report is lost when the
            // process is killed (a game run ends at its timeout).
            if base != start {
                eprintln!("[pin] {path} WANTED {base:#x} GOT {start:#x}");
            }
            self.pin_landed.push((path, base, start));
        }
        // Anonymous memory is defined to read as zero. Pages that already
        // are stay untouched, so a large reservation costs no memory.
        m.zero_dirty(start, crate::mem::page_align_up(len) as usize)?;
        Ok(start)
    }


    // -- sessions, credentials and mounts --------------------------------------

    /// Move anything another process has sent into this one's pending set.
    fn take_sent_signals(&mut self) {
        if !self.procs.has_signals(self.pid) {
            return;
        }
        let taken = self.procs.take_signals(self.pid);
        let mut sent = taken;
        while sent != 0 {
            let signo = sent.trailing_zeros() + 1;
            sent &= sent - 1;
            self.signals.raise_shared(signo, None);
        }
        self.note_kill_senders(taken);
    }

    /// Of the signals just taken from the process table, mark those `kill`
    /// sent as from their sender (`SI_USER`, `si_pid`), not the kernel.
    fn note_kill_senders(&mut self, taken: u64) {
        for (signo, pid) in self.procs.take_kill_senders(self.pid, taken) {
            self.signals.raise_shared(
                signo,
                Some(crate::signal::Source { code: crate::signal::SI_USER, pid, uid: self.uid }),
            );
        }
    }

    /// `getresuid` and `getresgid`: real, effective and saved, all the same
    /// value here because nothing ever differs between them.
    fn sys_getres(&mut self, m: &mut GuestMem<'_>, a: [u64; 6], id: u32) -> SysResult {
        for slot in [a[0], a[1], a[2]] {
            if slot != 0 {
                m.write_u32(slot, id)?;
            }
        }
        Ok(0)
    }

    /// `mount`, for the filesystems gex has.
    ///
    /// An init script's first act is usually to mount `/proc`, `/sys` and
    /// `/dev`, and failing that is not fatal but it does mean everything
    /// afterwards reads an empty `/proc`. The hosts already mount `/proc` and
    /// `/dev` before the guest starts, so the common case is a request for
    /// something that is there -- which succeeds, because the guest's view
    /// afterwards is the one it asked for.
    fn sys_mount(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let target = self.at_path(m, -100, a[1])?;
        let fstype = if a[2] == 0 {
            String::new()
        } else {
            String::from_utf8_lossy(&m.read_cstr(a[2], 64)?).into_owned()
        };
        // A remount or a bind of something already there changes nothing gex
        // models, and reporting failure would stop an init script that is in
        // fact fine.
        const MS_REMOUNT: u64 = 32;
        const MS_BIND: u64 = 4096;
        if a[3] & (MS_REMOUNT | MS_BIND) != 0 {
            return Ok(0);
        }
        match fstype.as_str() {
            "tmpfs" | "ramfs" => {
                self.vfs.mount(&target, Arc::new(crate::vfs::TmpFs::new()));
                Ok(0)
            }
            "devtmpfs" | "devfs" => {
                self.vfs.mount(&target, Arc::new(crate::vfs::DevFs::new()));
                Ok(0)
            }
            // `/proc` needs a source of process state that only the host can
            // build, so it cannot be created on demand -- but the host has
            // already mounted one, and a second request for it is satisfied.
            "proc" | "procfs" => {
                if self.vfs.is_mount_point(&target) {
                    Ok(0)
                } else {
                    Err(Errno::ENODEV)
                }
            }
            // An empty `/sys` is what most programs want from sysfs: they
            // probe for a path, do not find it, and take the other branch.
            "sysfs" | "debugfs" | "cgroup" | "cgroup2" => {
                if !self.vfs.is_mount_point(&target) {
                    self.vfs.mount(&target, Arc::new(crate::vfs::TmpFs::new()));
                }
                Ok(0)
            }
            _ => Err(Errno::ENODEV),
        }
    }

    fn sys_umount(&mut self, m: &mut GuestMem<'_>, target: u64) -> SysResult {
        let target = self.at_path(m, -100, target)?;
        if self.vfs.unmount(&target) {
            Ok(0)
        } else {
            Err(Errno::EINVAL)
        }
    }

    /// `rt_sigtimedwait`, for a signal already pending. The blocking case is
    /// not implemented: a caller that asks to wait gets `EAGAIN`, which suits
    /// every caller seen so far (draining blocked signals, not sleeping).
    fn sys_rt_sigtimedwait(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let set = if a[0] == 0 { 0 } else { m.read_bits64(a[0])? };
        match self.signals.take_pending_in(set) {
            Some(signal) => {
                // A `siginfo_t`: si_signo, si_errno, si_code, then the sender
                // for one a process sent (the union is 8-aligned in the 64-bit
                // layout).
                let source = self.signals.take_source(signal);
                if a[1] != 0 {
                    m.zero(a[1], 128)?;
                    m.write_u32(a[1], signal)?;
                    match source {
                        Some(src) => {
                            let fields = a[1] + if self.compat || gex_abi::PPC { 12 } else { 16 };
                            m.write_u32(a[1] + 8, src.code as u32)?;
                            m.write_u32(fields, src.pid as u32)?;
                            m.write_u32(fields + 4, src.uid)?;
                        }
                        None => m.write_u32(a[1] + 8, 0x80)?, // SI_KERNEL
                    }
                }
                Ok(signal as u64)
            }
            None => Err(Errno::EAGAIN),
        }
    }


    // -- sockets ---------------------------------------------------------------

    /// `socket`. AF_UNIX, an inert AF_NETLINK (below), and AF_INET with only the two kinds that matter: a stream,
    /// which the host carries, and a datagram, which only ever goes to the
    /// resolver this kernel is.
    fn sys_socket(&mut self, domain: u64, ty: u64) -> SysResult {
        const AF_UNIX: u64 = 1;
        const AF_INET: u64 = 2;
        const SOCK_STREAM: u64 = 1;
        const SOCK_DGRAM: u64 = 2;
        const SOCK_NONBLOCK: u64 = 0o4000;
        const SOCK_CLOEXEC: u64 = 0o2000000;
        // A unix socket needs no network behind it: both ends are in this
        // machine, and one of them may be the host rather than a guest.
        if domain == AF_UNIX {
            let kind = ty & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
            if kind != SOCK_STREAM && kind != 5 {
                // SOCK_SEQPACKET is 5 and behaves closely enough to a stream
                // for what uses it here; datagram unix sockets are not.
                return Err(Errno::EPROTONOSUPPORT);
            }
            let sock = Arc::new(unix::UnixSocket::new(true));
            let nonblock = ty & SOCK_NONBLOCK != 0;
            sock.set_nonblocking(nonblock);
            let flags = if nonblock { oflags::O_NONBLOCK } else { 0 };
            return self.install_unix(sock, flags, ty & SOCK_CLOEXEC != 0);
        }
        // AF_NETLINK, which libudev's device monitor opens (SDL's joystick
        // and haptic start-up through it; a failure there made SDL_Init undo
        // its video subsystem too, and Unity 6 then saw no display). There
        // are no devices to announce, so the socket binds, takes its options
        // and never has a message: an empty eventfd behind the descriptor.
        const AF_NETLINK: u64 = 16;
        if domain == AF_NETLINK {
            let handle: Arc<dyn crate::vfs::FileHandle> = Arc::new(crate::eventfd::EventFd::new(0, false));
            let flags = if ty & SOCK_NONBLOCK != 0 { crate::fs::oflags::O_NONBLOCK } else { 0 };
            let file = OpenFile::new(handle, "socket:[netlink]".to_string(), flags, true, true);
            return self.fds.insert(file, ty & SOCK_CLOEXEC != 0);
        }
        let Some(net) = self.net.clone() else {
            return Err(Errno::EAFNOSUPPORT);
        };
        if domain != AF_INET {
            return Err(Errno::EAFNOSUPPORT);
        }
        let _ = &net;
        let kind = match ty & !(SOCK_NONBLOCK | SOCK_CLOEXEC) {
            SOCK_STREAM => net::Kind::Tcp,
            SOCK_DGRAM => net::Kind::Udp,
            _ => return Err(Errno::EPROTONOSUPPORT),
        };
        let sock = Arc::new(net::Socket::new(kind, net, Arc::clone(&self.resolver), Arc::clone(&self.ports), self.unix_remote));
        sock.set_self();
        sock.set_nonblocking(ty & SOCK_NONBLOCK != 0);
        let flags = if ty & SOCK_NONBLOCK != 0 {
            crate::fs::oflags::O_NONBLOCK
        } else {
            0
        };
        let handle: Arc<dyn crate::vfs::FileHandle> = sock.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, "socket".to_string(), flags, true, true);
        let fd = self.fds.insert(file, ty & SOCK_CLOEXEC != 0)?;
        self.sockets.insert(key, sock);
        Ok(fd)
    }

    /// The unix socket behind a descriptor, if it is one. Asked of the
    /// handle itself: a per-process table of the sockets this process made
    /// did not know one that arrived over `SCM_RIGHTS`, so sending or
    /// receiving descriptors through it silently carried none.
    fn unix_of(&self, fd: i32) -> Result<Arc<unix::UnixSocket>, Errno> {
        let file = self.fds.get(fd)?;
        Arc::clone(&file.handle).as_unix_socket().ok_or(Errno::ENOTSOCK)
    }

    /// The same for a socket that lives in the global kernel.
    fn remote_unix_of(&self, fd: i32) -> Result<Arc<unix::RemoteUnix>, Errno> {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        self.remote_sockets.get(&key).cloned().ok_or(Errno::ENOTSOCK)
    }

    /// Put a socket that lives in the global kernel at `fd`, replacing
    /// whatever was there.
    ///
    /// Replacing, because a unix socket becomes remote only at `bind` or
    /// `connect` -- `socket()` has no name to decide by, so the descriptor
    /// starts as an ordinary local socket and is swapped when its name is
    /// known.
    fn install_remote_unix(
        &mut self,
        fd: i32,
        sock: Arc<unix::RemoteUnix>,
        flags: u64,
        cloexec: bool,
    ) -> Result<(), Errno> {
        let handle: Arc<dyn crate::vfs::FileHandle> = sock.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, "socket".to_string(), flags, true, true);
        self.fds.install(fd, file, cloexec)?;
        self.remote_sockets.insert(key, sock);
        Ok(())
    }

    /// Put a unix socket in the descriptor table.
    fn install_unix(&mut self, sock: Arc<unix::UnixSocket>, flags: u64, cloexec: bool) -> SysResult {
        let file = OpenFile::new(sock, "socket".to_string(), flags, true, true);
        self.fds.insert(file, cloexec)
    }

    /// The path out of a `struct sockaddr_un`: a two-byte family then up to
    /// 108 bytes of NUL-terminated path.
    ///
    /// The abstract namespace -- a leading NUL, which is how D-Bus and X11
    /// usually bind -- is kept as a distinct name by prefixing it, so an
    /// abstract socket and a filesystem one of the same spelling do not
    /// collide.
    fn sockaddr_un(m: &GuestMem<'_>, addr: u64, len: u64) -> Result<String, Errno> {
        if addr == 0 || len < 2 {
            return Err(Errno::EINVAL);
        }
        let bytes = m.read(addr, (len as usize).min(110))?;
        let path = &bytes[2..];
        if path.first() == Some(&0) {
            let end = path.len();
            let name = &path[1..end];
            let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
            return Ok(format!("\0{}", String::from_utf8_lossy(name)));
        }
        let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        Ok(String::from_utf8_lossy(&path[..end]).into_owned())
    }

    /// Write a `struct sockaddr_un` back, for `getsockname` and `accept`.
    fn write_sockaddr_un(
        m: &mut GuestMem<'_>,
        addr: u64,
        len_out: u64,
        path: &str,
    ) -> Result<(), Errno> {
        const AF_UNIX: u16 = 1;
        if addr != 0 {
            let mut buf = vec![0u8; 110];
            buf[..2].copy_from_slice(&AF_UNIX.to_ge_bytes());
            let raw = path.as_bytes();
            let n = raw.len().min(107);
            buf[2..2 + n].copy_from_slice(&raw[..n]);
            m.write(addr, &buf[..2 + n + 1])?;
        }
        if len_out != 0 {
            m.write_u32(len_out, (2 + path.len().min(107) + 1) as u32)?;
        }
        Ok(())
    }

    /// `close`, which also drops any advisory lock the description held.
    ///
    /// Only when this was the last descriptor onto it: `dup` and `fork` share
    /// one description and one lock, so closing a copy releases nothing. And
    /// the lock is keyed by the description's address, which is handed
    /// straight back to the next open -- a stale one would belong to an
    /// unrelated file.
    fn sys_close(&mut self, fd: i32) -> SysResult {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file) as *const () as usize;
        self.fds.close(fd)?;
        // One reference in the table is gone; `file` is the other. Anything
        // above that is another descriptor.
        if Arc::strong_count(&file) == 1 {
            self.file_locks.release(key);
            // An internet socket's last descriptor: the socket goes (its
            // connection closes, a listener gives its port back).
            let handle = Arc::as_ptr(&file.handle) as *const () as usize;
            self.sockets.remove(&handle);
            // Last descriptor to an O_TMPFILE: drop its name. If a `linkat` gave
            // it another (the atomic-write idiom), that name keeps the inode.
            if let Some(tp) = &file.unlink_on_close {
                let _ = self.vfs.unlink(tp);
            }
        }
        Ok(0)
    }

    /// `close_range`: shut every open descriptor in a span at once.
    fn sys_close_range(&mut self, first: u64, last: u64, flags: u64) -> SysResult {
        // `CLOSE_RANGE_CLOEXEC` marks the span close-on-exec instead of closing
        // it now; `CLOSE_RANGE_UNSHARE` asks for a private descriptor table
        // first, which is a no-op here because a process already has its own.
        const CLOSE_RANGE_CLOEXEC: u64 = 4;
        if first > last {
            return Err(Errno::EINVAL);
        }
        self.fds
            .close_range(first as u32, last as u32, flags & CLOSE_RANGE_CLOEXEC != 0);
        Ok(0)
    }

    /// `membarrier`: force a memory barrier across the process's threads.
    ///
    /// A no-op that succeeds. Every gex thread runs against one shared linear
    /// memory and the runtime already emits the atomic accesses the guest asks
    /// for, so there is no per-thread view to reconcile -- which is the whole
    /// job of this call. The `MEMBARRIER_CMD_QUERY` (cmd 0) case returns the
    /// bitmask of supported commands, and reporting none is honest and is what
    /// a caller that falls back to a heavier barrier expects.
    fn sys_membarrier(&mut self, cmd: u64) -> SysResult {
        let _ = cmd;
        Ok(0)
    }

    /// `epoll_create1`.
    fn sys_epoll_create(&mut self, flags: u64) -> SysResult {
        const EPOLL_CLOEXEC: u64 = 0o2000000;
        let set = Arc::new(unix::EpollSet::new());
        let handle: Arc<dyn crate::vfs::FileHandle> = set.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, "anon_inode:[eventpoll]".into(), 0, true, true);
        let fd = self.fds.insert(file, flags & EPOLL_CLOEXEC != 0)?;
        self.epolls.insert(key, set);
        Ok(fd)
    }

    /// The epoll set behind a descriptor.
    ///
    /// Kept in a side table for the same reason sockets are: an
    /// `Arc<dyn FileHandle>` cannot be asked what it really is.
    fn epoll_of(&self, fd: i32) -> Result<Arc<unix::EpollSet>, Errno> {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        self.epolls.get(&key).cloned().ok_or(Errno::EINVAL)
    }

    /// `epoll_ctl`. A `struct epoll_event` on aarch64 is a 32-bit event mask,
    /// four bytes of padding, and the caller's 64-bit token -- unlike x86-64,
    /// where the structure is packed and the token starts at offset four.
    fn sys_epoll_ctl(
        &mut self,
        m: &GuestMem<'_>,
        epfd: i32,
        op: u64,
        fd: i32,
        event: u64,
    ) -> SysResult {
        let set = self.epoll_of(epfd)?;
        // `EPOLL_CTL_DEL` is allowed a null event pointer.
        let (events, data) = if event == 0 {
            (0, 0)
        } else if gex_abi::X86 {
            (m.read_u32(event)?, m.read_u64(event + 4)?)
        } else {
            (m.read_u32(event)?, m.read_u64(event + 8)?)
        };
        // Watching something that is not open is an error, and a program that
        // gets silence instead waits for a readiness that cannot arrive.
        if op != unix::epoll_op::DEL {
            self.fds.get(fd)?;
        }
        set.ctl(op, fd, events, data)
    }

    /// Note that `[addr, addr + len)` no longer holds the code it did.
    ///
    /// Only ranges that overlap an executable mapping are worth reporting:
    /// unmapping the heap or a data file happens constantly and has no
    /// translations behind it.
    pub fn retire_code(&mut self, addr: u64, len: u64) {
        let (start, end) = (
            crate::mem::page_align_down(addr),
            crate::mem::page_align_up(addr.saturating_add(len)),
        );
        if end <= start {
            return;
        }
        let executable = self
            .mem
            .vmas()
            .iter()
            .any(|v| v.prot & crate::mem::prot::EXEC != 0 && v.start < end && v.end > start);
        if executable {
            self.retired_code.push((start, end));
            crate::note_code_change();
        }
    }

    /// `epoll_pwait`, answered from the same readiness the `poll` path uses.
    fn sys_epoll_wait(
        &mut self,
        m: &mut GuestMem<'_>,
        epfd: i32,
        out: u64,
        max: u32,
    ) -> SysResult {
        let set = self.epoll_of(epfd)?;
        let ready = self.epoll_ready(&set);
        let n = self.write_epoll_events(m, out, max, &ready)?;
        Ok(n)
    }

    /// A set's registrations as the list the poll path understands.
    fn epoll_pollfds(&self, set: &unix::EpollSet) -> Vec<PollFd> {
        set.watches()
            .iter()
            .filter_map(|w| {
                let file = self.fds.get(w.fd).ok()?;
                Some(PollFd {
                    addr: 0,
                    fd: w.fd,
                    events: (w.events & 0xffff) as u16,
                    handle: Some(Arc::clone(&file.handle)),
                    writable: file.writable,
                })
            })
            .collect()
    }

    /// Which of a set's descriptors are ready, and with what.
    fn epoll_ready(&self, set: &unix::EpollSet) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        for w in set.watches() {
            let Ok(file) = self.fds.get(w.fd) else {
                // A closed descriptor is dropped from the answer rather than
                // reported; Linux removes it from the set entirely.
                continue;
            };
            let p = PollFd {
                addr: 0,
                fd: w.fd,
                events: (w.events & 0xffff) as u16,
                handle: Some(Arc::clone(&file.handle)),
                writable: file.writable,
            };
            let got = poll_revents(&p) as u32;
            if got != 0 {
                out.push((got, w.data));
            }
        }
        out
    }

    fn write_epoll_events(
        &self,
        m: &mut GuestMem<'_>,
        out: u64,
        max: u32,
        ready: &[(u32, u64)],
    ) -> SysResult {
        let n = ready.len().min(max as usize);
        for (i, (events, data)) in ready.iter().take(n).enumerate() {
            if gex_abi::X86 {
                // Packed: 12 bytes, the token at offset four.
                let at = out + i as u64 * 12;
                m.write_u32(at, *events)?;
                m.write_u64(at + 4, *data)?;
            } else {
                let at = out + i as u64 * 16;
                m.write_u32(at, *events)?;
                m.write_u32(at + 4, 0)?;
                m.write_u64(at + 8, *data)?;
            }
        }
        Ok(n as u64)
    }

    /// The anonymous shared file behind a descriptor, if it is one.
    fn memfd_of(&self, fd: i32) -> Result<Arc<unix::MemFd>, Errno> {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        self.memfds.get(&key).cloned().ok_or(Errno::EINVAL)
    }

    /// `fallocate`. Only mode zero, which is "make sure these bytes exist".
    ///
    /// Xwayland calls this through `posix_fallocate` after sealing its shared
    /// memory, to be certain the pages are there before it hands the
    /// descriptor to a compositor. The other modes -- punching holes,
    /// collapsing ranges -- are refused rather than silently treated as
    /// allocation, which would leave a caller believing it had freed space.
    fn sys_fallocate(&mut self, fd: i32, mode: u64, offset: u64, len: u64) -> SysResult {
        if mode != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if let Ok(memfd) = self.memfd_of(fd) {
            return memfd.reserve(end);
        }
        // An ordinary file grows by being written to its new length.
        let file = self.fds.get(fd)?;
        let size = file.handle.metadata()?.size;
        if end > size {
            file.handle.truncate(end)?;
        }
        Ok(0)
    }

    /// `flock`. The open file description owns the lock, and its address is
    /// a stable name for it while it is open.
    fn sys_flock(&mut self, fd: i32, op: u64) -> SysResult {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file) as *const () as usize;
        self.file_locks.flock(&file.path, key, op)
    }

    fn sys_memfd_create(&mut self, m: &GuestMem<'_>, name: u64, flags: u64) -> SysResult {
        const MFD_CLOEXEC: u64 = 1;
        let raw = m.read_cstr(name, 249)?;
        let name = String::from_utf8_lossy(&raw).into_owned();
        let memfd = Arc::new(unix::MemFd::new(&name));
        let handle: Arc<dyn crate::vfs::FileHandle> = memfd.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, format!("/memfd:{name}"), 0, true, true);
        let fd = self.fds.insert(file, flags & MFD_CLOEXEC != 0)?;
        self.memfds.insert(key, memfd);
        Ok(fd)
    }

    fn sys_socketpair(&mut self, m: &mut GuestMem<'_>, domain: u64, ty: u64, out: u64) -> SysResult {
        const AF_UNIX: u64 = 1;
        const SOCK_NONBLOCK: u64 = 0o4000;
        const SOCK_CLOEXEC: u64 = 0o2000000;
        if domain != AF_UNIX {
            return Err(Errno::EAFNOSUPPORT);
        }
        let (a, b) = unix::UnixSocket::pair();
        let nonblock = ty & SOCK_NONBLOCK != 0;
        a.set_nonblocking(nonblock);
        b.set_nonblocking(nonblock);
        let flags = if nonblock { oflags::O_NONBLOCK } else { 0 };
        let cloexec = ty & SOCK_CLOEXEC != 0;
        let fa = self.install_unix(a, flags, cloexec)?;
        let fb = self.install_unix(b, flags, cloexec)?;
        m.write_u32(out, fa as u32)?;
        m.write_u32(out + 4, fb as u32)?;
        Ok(0)
    }

    fn sys_unix_bind(&mut self, m: &GuestMem<'_>, fd: i32, addr: u64, len: u64) -> SysResult {
        let sock = self.unix_of(fd)?;
        let path = Self::sockaddr_un(m, addr, len)?;
        // A filesystem socket resolves against the guest's root the way any
        // other path does; an abstract one has no filesystem existence.
        let path = if path.starts_with('\0') {
            path
        } else {
            self.vfs.absolute(&path)
        };
        // In the global kernel when there is one, because the client that
        // will connect is another process -- and with a Worker per process
        // that is another memory, where a name bound here means nothing.
        //
        // Bound and listening in one call: what a guest does between `bind`
        // and `listen` is local to itself, and what the far end can see is
        // only whether the name answers.
        if let Some(r) = self.unix_remote {
            let id = (r.bind)(path.as_ptr() as crate::PAddr, path.len() as u32);
            if id < 0 {
                return Err(Errno(-id));
            }
            let flags = self.fds.get(fd)?.flags();
            let cloexec = self.fds.cloexec(fd).unwrap_or(false);
            let remote = Arc::new(unix::RemoteUnix::new(id, true, Some(path.clone()), r));
            self.install_remote_unix(fd, remote, flags, cloexec)?;
            if !path.starts_with('\0') {
                let _ = self.vfs.mksocket(&path);
            }
            return Ok(0);
        }
        sock.bind(&path)?;
        self.socket_names.bind(&path, sock)?;
        // And a node in the filesystem, so the name can be seen as well as
        // connected to. Xvfb binds /tmp/.X11-unix/X0 and every X client waits
        // for that path to appear before trying it; without the node the
        // server was listening and nothing could tell.
        //
        // A filesystem that will not take one is not an error here: the
        // socket is bound and reachable either way, which is the part that
        // matters.
        if !path.starts_with('\0') {
            let _ = self.vfs.mksocket(&path);
        }
        Ok(0)
    }

    fn sys_unix_connect(&mut self, m: &GuestMem<'_>, fd: i32, addr: u64, len: u64) -> SysResult {
        let sock = self.unix_of(fd)?;
        let path = Self::sockaddr_un(m, addr, len)?;
        let path = if path.starts_with('\0') {
            path
        } else {
            self.vfs.absolute(&path)
        };
        // The global kernel first, because that is where a name another
        // process bound will be. A host-bound name -- the virtio-gpu socket --
        // is in *this* process's table and never in the kernel's, so the
        // local path is still tried when the kernel does not know the name.
        if let Some(r) = self.unix_remote {
            let id = (r.connect)(path.as_ptr() as crate::PAddr, path.len() as u32);
            if id >= 0 {
                let flags = self.fds.get(fd)?.flags();
                let cloexec = self.fds.cloexec(fd).unwrap_or(false);
                let remote = Arc::new(unix::RemoteUnix::new(id, false, Some(path.clone()), r));
                self.install_remote_unix(fd, remote, flags, cloexec)?;
                return Ok(0);
            }
        }
        // This process's own host first -- its compositor, its GPU device --
        // then the system-wide table, which is where a guest server (an X
        // server's socket) lives.
        let mine = Arc::clone(&self.host_names);
        if let Ok(r) = mine.connect(&sock, &path) {
            return Ok(r);
        }
        let names = Arc::clone(&self.socket_names);
        names.connect(&sock, &path)
    }

    /// `accept`: a unix listener's, else a TCP listener's (`net::Socket`).
    fn sys_accept(&mut self, m: &mut GuestMem<'_>, fd: i32, addr: u64, len_out: u64, flags: u64) -> SysResult {
        match self.sys_unix_accept(m, fd, addr, len_out, flags) {
            Err(Errno::ENOTSOCK) => self.sys_inet_accept(m, fd, addr, len_out, flags),
            other => other,
        }
    }

    fn sys_inet_accept(&mut self, m: &mut GuestMem<'_>, fd: i32, addr: u64, len_out: u64, flags: u64) -> SysResult {
        const SOCK_NONBLOCK: u64 = 0o4000;
        const SOCK_CLOEXEC: u64 = 0o2000000;
        let listener = self.socket_of(fd)?;
        let sock = listener.accept()?;
        let (ip, port) = sock.peer().unwrap_or((0, 0));
        let nonblock = flags & SOCK_NONBLOCK != 0;
        sock.set_nonblocking(nonblock);
        let oflag = if nonblock { oflags::O_NONBLOCK } else { 0 };
        let handle: Arc<dyn crate::vfs::FileHandle> = sock.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, "socket".to_string(), oflag, true, true);
        let new_fd = self.fds.insert(file, flags & SOCK_CLOEXEC != 0)?;
        self.sockets.insert(key, sock);
        self.sys_write_sockaddr(m, ip, port, addr, len_out)?;
        Ok(new_fd)
    }

    fn sys_unix_accept(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        addr: u64,
        len_out: u64,
        flags: u64,
    ) -> SysResult {
        const SOCK_NONBLOCK: u64 = 0o4000;
        const SOCK_CLOEXEC: u64 = 0o2000000;
        // A listener in the global kernel answers with an id, and the
        // descriptor made from it is the same kind.
        if let Ok(listener) = self.remote_unix_of(fd) {
            let session = listener.accept()?;
            let path = listener.bound_path().unwrap_or_default();
            Self::write_sockaddr_un(m, addr, len_out, &path)?;
            let nonblock = flags & SOCK_NONBLOCK != 0;
            session.set_nonblocking(nonblock);
            let oflag = if nonblock { oflags::O_NONBLOCK } else { 0 };
            let handle: Arc<dyn crate::vfs::FileHandle> = session.clone();
            let key = Arc::as_ptr(&handle) as *const () as usize;
            let file = OpenFile::new(handle, "socket".to_string(), oflag, true, true);
            let new_fd = self.fds.insert(file, flags & SOCK_CLOEXEC != 0)?;
            self.remote_sockets.insert(key, session);
            return Ok(new_fd);
        }
        let listener = self.unix_of(fd)?;
        let session = listener.accept()?;
        let path = listener.bound_path().unwrap_or_default();
        Self::write_sockaddr_un(m, addr, len_out, &path)?;
        let nonblock = flags & SOCK_NONBLOCK != 0;
        session.set_nonblocking(nonblock);
        let oflag = if nonblock { oflags::O_NONBLOCK } else { 0 };
        self.install_unix(session, oflag, flags & SOCK_CLOEXEC != 0)
    }

    fn socket_of(&self, fd: i32) -> Result<Arc<net::Socket>, Errno> {
        let file = self.fds.get(fd)?;
        // `Arc<dyn FileHandle>` cannot be downcast without `Any`, so the
        // socket table is the way back to the concrete type. Keyed by the
        // handle's address, which is stable for as long as the descriptor is.
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        self.sockets.get(&key).cloned().ok_or(Errno::ENOTSOCK)
    }

    fn sys_connect(&mut self, m: &GuestMem<'_>, fd: i32, addr: u64, len: u64) -> SysResult {
        let bytes = m.read(addr, (len as usize).min(128))?.to_vec();
        let (ip, port) = net::parse_sockaddr(&bytes)?;
        let sock = self.socket_of(fd)?;
        match sock.connect(ip, port) {
            Ok(()) => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn sys_sendto(
        &mut self,
        m: &GuestMem<'_>,
        fd: i32,
        buf: u64,
        len: u64,
        dest: u64,
    ) -> SysResult {
        if dest != 0 {
            if let Ok(sock) = self.socket_of(fd) {
                let bytes = m.read(dest, 16)?.to_vec();
                if let Ok((ip, port)) = net::parse_sockaddr(&bytes) {
                    sock.set_peer(ip, port);
                }
            }
        }
        let data = m.read(buf, (len as usize).min(1 << 22))?.to_vec();
        self.fds.get(fd)?.write(&data)
    }

    fn sys_recvfrom(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        buf: u64,
        len: u64,
        addr: u64,
        addrlen: u64,
    ) -> SysResult {
        let len = (len as usize).min(1 << 22);
        let mut tmp = vec![0u8; len];
        let n = self.fds.get(fd)?.read(&mut tmp)?;
        m.write(buf, &tmp[..n as usize])?;
        if addr != 0 {
            // Where the datagram came from (a guest's port, the resolver),
            // else the connected peer.
            let peer = self.socket_of(fd).ok().and_then(|s| s.last_from().or_else(|| s.peer()));
            let (ip, port) = peer.unwrap_or((0, 0));
            self.sys_write_sockaddr(m, ip, port, addr, addrlen)?;
        }
        Ok(n)
    }

    fn sys_write_sockaddr(
        &mut self,
        m: &mut GuestMem<'_>,
        ip: u32,
        port: u16,
        addr: u64,
        addrlen: u64,
    ) -> SysResult {
        if addr == 0 {
            return Ok(0);
        }
        let sa = net::build_sockaddr(ip, port);
        let room = if addrlen != 0 {
            m.read_u32(addrlen)? as usize
        } else {
            sa.len()
        };
        let n = room.min(sa.len());
        m.write(addr, &sa[..n])?;
        if addrlen != 0 {
            m.write_u32(addrlen, sa.len() as u32)?;
        }
        Ok(0)
    }

    fn sys_getpeername(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        addr: u64,
        addrlen: u64,
    ) -> SysResult {
        let (ip, port) = self.socket_of(fd)?.peer().ok_or(Errno::ENOTCONN)?;
        self.sys_write_sockaddr(m, ip, port, addr, addrlen)
    }

    /// `getsockopt`. The only one anybody asks: has the connection come up?
    fn sys_getsockopt(&mut self, m: &mut GuestMem<'_>, fd: i32, name: u64, val: u64) -> SysResult {
        const SO_ERROR: u64 = 4;
        let err = if name == SO_ERROR {
            match self.socket_of(fd)?.status() {
                net::Status::Refused => Errno::ECONNREFUSED.0,
                // Not `EINPROGRESS`. `SO_ERROR` reports how a connect
                // *finished*, and Linux leaves it zero until one has; a
                // caller is meant to learn "not yet" from the descriptor not
                // being writable, which is what `poll_writable` now says.
                // Reporting it here made `apt` treat "still connecting" as
                // the error that stopped it.
                net::Status::Connecting => 0,
                _ => 0,
            }
        } else {
            0
        };
        if val != 0 {
            m.write_u32(val, err as u32)?;
        }
        Ok(0)
    }

    /// A `struct cmsghdr` is a length (a `size_t`), a level, a type, then
    /// the payload, with the whole header aligned to a pointer: 16 bytes,
    /// or 12 for an i386 process. `msghdr`, `iovec` and `mmsghdr` are
    /// pointer-sized fields too ([`Self::msg_word`]).
    fn cmsg_hdr(&self) -> u64 {
        if self.compat {
            12
        } else {
            16
        }
    }

    fn cmsg_align(&self, n: u64) -> u64 {
        let a = self.msg_word();
        n.div_ceil(a) * a
    }

    /// The pointer and `size_t` width of the message structures.
    fn msg_word(&self) -> u64 {
        if self.compat {
            4
        } else {
            8
        }
    }

    /// The `i`th pointer-sized word of a message structure at `at`.
    fn msg_rd(&self, m: &GuestMem<'_>, at: u64, i: u64) -> Result<u64, Errno> {
        if self.compat {
            Ok(m.read_u32(at + 4 * i)? as u64)
        } else {
            m.read_u64(at + 8 * i)
        }
    }

    fn msg_wr(&self, m: &mut GuestMem<'_>, at: u64, i: u64, v: u64) -> Result<(), Errno> {
        if self.compat {
            m.write_u32(at + 4 * i, v as u32)
        } else {
            m.write_u64(at + 8 * i, v)
        }
    }

    /// Collect the descriptors a `sendmsg` is passing.
    ///
    /// `SCM_RIGHTS` is the whole reason unix sockets are here. A Wayland
    /// client draws into shared memory and sends the *descriptor*; without
    /// this the compositor is handed a number that means nothing to it.
    fn read_scm_rights(
        &mut self,
        m: &GuestMem<'_>,
        control: u64,
        len: u64,
    ) -> Result<Vec<Arc<OpenFile>>, Errno> {
        const SOL_SOCKET: u32 = 1;
        const SCM_RIGHTS: u32 = 1;
        let (hdr, w) = (self.cmsg_hdr(), self.msg_word());
        let mut out = Vec::new();
        let mut at = 0u64;
        while at + hdr <= len {
            let cmsg_len = self.msg_rd(m, control + at, 0)?;
            if cmsg_len < hdr || at + cmsg_len > len {
                break;
            }
            let level = m.read_u32(control + at + w)?;
            let ty = m.read_u32(control + at + w + 4)?;
            if level == SOL_SOCKET && ty == SCM_RIGHTS {
                let n = (cmsg_len - hdr) / 4;
                for i in 0..n.min(253) {
                    let fd = m.read_u32(control + at + hdr + i * 4)? as i32;
                    out.push(self.fds.get(fd)?);
                }
            }
            at += self.cmsg_align(cmsg_len);
        }
        Ok(out)
    }

    /// Install received descriptors and describe them in the control buffer.
    ///
    /// Returns the bytes of control data written. A buffer too small for all
    /// of them gets what fits; Linux sets `MSG_CTRUNC` and closes the rest,
    /// and the socket layer here simply never handed over more than fit.
    fn write_scm_rights(
        &mut self,
        m: &mut GuestMem<'_>,
        control: u64,
        len: u64,
        files: Vec<Arc<OpenFile>>,
    ) -> Result<u64, Errno> {
        const SOL_SOCKET: u32 = 1;
        const SCM_RIGHTS: u32 = 1;
        if files.is_empty() || control == 0 {
            return Ok(0);
        }
        let (hdr, w) = (self.cmsg_hdr(), self.msg_word());
        let payload = files.len() as u64 * 4;
        let cmsg_len = hdr + payload;
        if cmsg_len > len {
            return Ok(0);
        }
        self.msg_wr(m, control, 0, cmsg_len)?;
        m.write_u32(control + w, SOL_SOCKET)?;
        m.write_u32(control + w + 4, SCM_RIGHTS)?;
        for (i, f) in files.into_iter().enumerate() {
            let fd = self.fds.insert(f, false)?;
            m.write_u32(control + hdr + i as u64 * 4, fd as u32)?;
        }
        Ok(cmsg_len)
    }

    /// `sendmsg`, which is how glibc writes to a socket, and the only way to
    /// pass a descriptor to another process.
    fn sys_sendmsg(&mut self, m: &GuestMem<'_>, fd: i32, msg: u64) -> SysResult {
        // `msg_name` is where this one goes, and a reply has to be reported
        // as having come from there or a resolver drops it as spoofed.
        let name = self.msg_rd(m, msg, 0)?;
        if name != 0 {
            if let Ok(sock) = self.socket_of(fd) {
                let bytes = m.read(name, 16)?.to_vec();
                if let Ok((ip, port)) = net::parse_sockaddr(&bytes) {
                    sock.set_peer(ip, port);
                }
            }
        }
        let iov = self.msg_rd(m, msg, 2)?;
        let iovlen = self.msg_rd(m, msg, 3)?;
        let mut data = Vec::new();
        for i in 0..iovlen.min(1024) {
            let base = self.msg_rd(m, iov, 2 * i)?;
            let len = self.msg_rd(m, iov, 2 * i + 1)? as usize;
            if len > 0 {
                data.extend_from_slice(m.read(base, len.min(1 << 22))?);
            }
        }
        // Ancillary data, which for a unix socket may be descriptors. Staged
        // on the socket rather than passed down, so the send attaches them to
        // the first byte it writes and a send that fails attaches nothing.
        let control = self.msg_rd(m, msg, 4)?;
        let controllen = self.msg_rd(m, msg, 5)?;
        if control != 0 && controllen >= self.cmsg_hdr() {
            if let Ok(sock) = self.unix_of(fd) {
                let files = self.read_scm_rights(m, control, controllen)?;
                sock.stage_send(files);
            }
        }
        self.fds.get(fd)?.write(&data)
    }

    /// One `struct mmsghdr` is a `msghdr` and the length it carried: 64
    /// bytes with the length at 56, or 32 with it at 28 for i386.
    fn mmsghdr(&self) -> (u64, u64) {
        if self.compat {
            (32, 28)
        } else {
            (64, 56)
        }
    }

    /// `sendmmsg`: several datagrams in one call. glibc's resolver sends its A
    /// and AAAA queries this way and does not fall back to `sendto` on
    /// `ENOSYS`, so it must be real or name resolution fails.
    fn sys_sendmmsg(&mut self, m: &mut GuestMem<'_>, fd: i32, vec: u64, n: u64) -> SysResult {
        let (size, len_at) = self.mmsghdr();
        let mut sent = 0u64;
        for i in 0..n.min(1024) {
            let hdr = vec + i * size;
            match self.sys_sendmsg(m, fd, hdr) {
                Ok(len) => {
                    m.write_u32(hdr + len_at, len as u32)?;
                    sent += 1;
                }
                // A failure on the first is the call's failure; after that
                // the count is the answer, as it is for `sendmsg`.
                Err(e) if sent == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(sent)
    }

    fn sys_recvmmsg(
        &mut self,
        m: &mut GuestMem<'_>,
        fd: i32,
        vec: u64,
        n: u64,
        flags: u64,
    ) -> SysResult {
        let (size, len_at) = self.mmsghdr();
        let mut got = 0u64;
        for i in 0..n.min(1024) {
            let hdr = vec + i * size;
            match self.sys_recvmsg(m, fd, hdr, flags) {
                Ok(len) => {
                    m.write_u32(hdr + len_at, len as u32)?;
                    got += 1;
                    // A datagram socket delivers one message per call; if
                    // this one came up short there is nothing else waiting.
                    if len == 0 {
                        break;
                    }
                }
                Err(e) if got == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(got)
    }

    /// `recvmsg` flags. `MSG_DONTWAIT` is decided in `blocking_op`; here it
    /// only matters that an empty socket answers `EAGAIN`, which the stream
    /// already does. `MSG_PEEK` leaves the bytes queued.
    const MSG_PEEK: u64 = 0x2;
    const MSG_DONTWAIT: u64 = 0x40;

    fn sys_recvmsg(&mut self, m: &mut GuestMem<'_>, fd: i32, msg: u64, flags: u64) -> SysResult {
        let peek = flags & Self::MSG_PEEK != 0;
        let iov = self.msg_rd(m, msg, 2)?;
        let iovlen = self.msg_rd(m, msg, 3)?;
        let control = self.msg_rd(m, msg, 4)?;
        let controllen = self.msg_rd(m, msg, 5)?;
        // How many descriptors the caller left room for. A reader that offers
        // no room leaves them queued for the next call rather than losing
        // them, which is what Wayland's two-stage read depends on.
        let room = if control != 0 && controllen >= self.cmsg_hdr() {
            ((controllen - self.cmsg_hdr()) / 4) as usize
        } else {
            0
        };
        let unix_sock = self.unix_of(fd).ok();
        let mut total = 0u64;
        for i in 0..iovlen.min(1024) {
            let base = self.msg_rd(m, iov, 2 * i)?;
            let len = self.msg_rd(m, iov, 2 * i + 1)? as usize;
            if len == 0 {
                continue;
            }
            let mut tmp = vec![0u8; len.min(1 << 22)];
            let n = match &unix_sock {
                Some(sock) => sock.recv(&mut tmp, room, peek)?,
                None => self.fds.get(fd)?.read(&mut tmp)?,
            };
            m.write(base, &tmp[..n as usize])?;
            total += n;
            // A short read means there is no more waiting; asking again would
            // block, and `recvmsg` returns what it has.
            if (n as usize) < tmp.len() {
                break;
            }
        }
        let received = unix_sock.map(|s| s.take_received()).unwrap_or_default();
        let written = self.write_scm_rights(m, control, controllen, received)?;
        self.msg_wr(m, msg, 5, written)?;
        Ok(total)
    }

    // -- process and time ----------------------------------------------------

    /// `dup2`: `dup3` without flags, except that `dup2(fd, fd)` is a no-op
    /// that still validates the descriptor, where `dup3` would refuse.
    fn sys_dup2(&mut self, old: i32, new: i32) -> SysResult {
        if old == new {
            self.fds.get(old)?;
            return Ok(new as u64);
        }
        self.sys_dup3(old, new, 0)
    }

    fn sys_time(&mut self, m: &mut GuestMem<'_>, tloc: u64) -> SysResult {
        let (secs, _) = (self.clock)();
        if tloc != 0 {
            m.write_u64(tloc, secs)?;
        }
        Ok(secs)
    }

    /// `alarm(s)`: `setitimer(ITIMER_REAL, {0, s})`, returning the seconds the
    /// previous timer had left. Both itimervals live in the thread's scratch
    /// area, since the call has none of its own.
    fn sys_alarm(&mut self, m: &mut GuestMem<'_>, secs: u64) -> SysResult {
        let scratch = self.ctx_addr + ctx::SCRATCH as u64;
        for off in 0..4 {
            m.write_u64(scratch + off * 8, 0)?;
        }
        m.write_u64(scratch + 16, secs)?;
        self.sys_itimer(m, 0, scratch, scratch + 32, true)?;
        let left = m.read_u64(scratch + 32 + 16)?;
        let left_us = m.read_u64(scratch + 32 + 24)?;
        Ok(left + u64::from(left_us > 0))
    }

    /// x86-64 `arch_prctl`: the FS and GS segment bases, which is how that
    /// ABI sets its thread pointer.
    fn sys_arch_prctl(&mut self, m: &mut GuestMem<'_>, code: u64, addr: u64) -> SysResult {
        const ARCH_SET_GS: u64 = 0x1001;
        const ARCH_SET_FS: u64 = 0x1002;
        const ARCH_GET_FS: u64 = 0x1003;
        const ARCH_GET_GS: u64 = 0x1004;
        match code {
            ARCH_SET_FS => m.write_u64(self.ctx_addr + ctx::FS_BASE as u64, addr)?,
            ARCH_SET_GS => m.write_u64(self.ctx_addr + ctx::GS_BASE as u64, addr)?,
            ARCH_GET_FS => {
                let v = m.read_u64(self.ctx_addr + ctx::FS_BASE as u64)?;
                m.write_u64(addr, v)?;
            }
            ARCH_GET_GS => {
                let v = m.read_u64(self.ctx_addr + ctx::GS_BASE as u64)?;
                m.write_u64(addr, v)?;
            }
            _ => return Err(Errno::EINVAL),
        }
        Ok(0)
    }

    fn sys_uname(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        let fields = [
            "Linux",
            "gex",
            "6.1.0-gex",
            "#1 SMP gex",
            if self.compat && !gex_abi::PPC { "i686" } else { gex_abi::ARCH_NAME },
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

    fn sys_clock_gettime(&mut self, m: &mut GuestMem<'_>, clk: u64, buf: u64) -> SysResult {
        // The two processor clocks count from when the process started, not
        // from the epoch. A program timing itself with one of these and
        // getting the wall clock instead sees a plausible-looking number that
        // is forty years too large, which subtracts away for a delta and does
        // not for anything else.
        const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
        const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
        let (secs, nanos) = match clk {
            CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
                let t = self.cpu_nanos();
                ((t / 1_000_000_000) as u64, (t % 1_000_000_000) as u32)
            }
            c if is_monotonic_clock(c) => monotonic_from((self.clock)()),
            _ => (self.clock)(),
        };
        m.write_u64(buf, secs)?;
        m.write_u64(buf + 8, nanos as u64)?;
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
        let (secs, nanos) = (self.clock)();
        m.write_u64(buf, secs)?;
        m.write_u64(buf + 8, (nanos / 1000) as u64)?;
        Ok(0)
    }

    fn sys_nanosleep(&mut self, m: &GuestMem<'_>, req: u64) -> SysResult {
        if req == 0 {
            return Err(Errno::EFAULT);
        }
        let secs = m.read_u64(req)? as i64;
        let nanos = m.read_u64(req + 8)?;
        if secs < 0 || nanos >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        // The sleep itself is `BlockOp::Sleep`, run by the host with the
        // kernel unlocked; reaching here means nothing to wait for: a zero
        // request, or a deadline already past.
        Ok(0)
    }

    /// Now on `clk`, in nanoseconds, as `clock_gettime` would answer it.
    fn clock_nanos(&mut self, clk: u64) -> u128 {
        const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
        const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
        match clk {
            CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => self.cpu_nanos(),
            c if is_monotonic_clock(c) => {
                let (secs, nanos) = monotonic_from((self.clock)());
                secs as u128 * 1_000_000_000 + nanos as u128
            }
            _ => self.now_nanos(),
        }
    }

    /// How much time this process has used, in nanoseconds: wall clock since
    /// it started, not true processor time (which there is no portable way to
    /// ask for here). `openssl speed` divides by it, so zero printed every
    /// throughput as `infk`.
    pub fn cpu_nanos(&mut self) -> u128 {
        let now = self.now_nanos();
        if self.started == 0 {
            self.started = now;
        }
        now.saturating_sub(self.started)
    }

    /// `times`. Four `clock_t` -- user, system, and the same for reaped
    /// children -- counted in the hundredths of a second that `USER_HZ` means
    /// on Linux, and a return value on the same scale.
    fn sys_times(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        let ticks = (self.cpu_nanos() / 10_000_000) as u64;
        if buf != 0 {
            m.zero(buf, 32)?;
            m.write_u64(buf, ticks)?;
        }
        Ok(ticks)
    }

    /// `getrusage`. Only the two times are filled in; the page-fault and
    /// context-switch counters stay zero, which is what a program that asks
    /// for them under emulation should expect to see.
    fn sys_getrusage(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf != 0 {
            m.zero(buf, 144)?;
            write_timeval(m, buf, self.cpu_nanos())?;
        }
        Ok(0)
    }

    fn sys_sysinfo(&mut self, m: &mut GuestMem<'_>, buf: u64) -> SysResult {
        if buf == 0 {
            return Err(Errno::EFAULT);
        }
        // struct sysinfo: uptime, loads[3], totalram, freeram, sharedram,
        // bufferram, totalswap, freeswap, procs (u16), totalhigh, freehigh,
        // mem_unit. The 64-bit layout has 8-byte longs (112 bytes); the i386
        // one 4-byte longs (64 bytes), where 4 GiB does not fit and memory
        // is counted in pages through mem_unit. glibc's qsort asks sysconf
        // for the physical pages once a sort has 256 elements, and its
        // caller's frame is exactly this struct's size: the wide layout in a
        // 32-bit process overwrote the stack canary.
        if self.compat {
            m.zero(buf, 64)?;
            m.write_u32(buf, 1000)?; // uptime
            m.write_u32(buf + 16, 1 << 20)?; // totalram: 4 GiB in pages
            m.write_u32(buf + 20, 1 << 19)?; // freeram
            m.write(buf + 40, &1u16.to_ge_bytes())?; // procs
            m.write_u32(buf + 52, 4096)?; // mem_unit
        } else {
            m.zero(buf, 112)?;
            m.write_u64(buf, 1000)?; // uptime
            m.write_u64(buf + 32, 4 * 1024 * 1024 * 1024)?; // totalram
            m.write_u64(buf + 40, 2 * 1024 * 1024 * 1024)?; // freeram
            m.write(buf + 80, &1u16.to_ge_bytes())?; // procs
            m.write_u32(buf + 104, 1)?; // mem_unit
        }
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
#[derive(Default, Clone)]
pub struct ProcState {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    pub maps: String,
    pub cmdline: Vec<u8>,
    pub environ: Vec<u8>,
    pub exe: String,
    pub cwd: String,
    pub fds: Vec<(i32, String)>,
    /// Which version of the memory map `maps` was rendered from.
    ///
    /// Not part of what `/proc` serves -- it is how the rendering is avoided.
    /// See `MemoryMap::generation`.
    pub maps_generation: u64,
    /// Which version of the descriptor table `fds` was listed from.
    pub fds_generation: u64,
}

/// A `/proc` source backed by a snapshot the kernel refreshes.
pub struct KernelProcSource(pub Arc<Mutex<ProcState>>);

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
        // pid, comm, state, ppid, pgrp, session, then the rest, which nothing
        // seen so far reads. `ps` and every "who is my parent" one-liner cut
        // fields out of this by number, so the first six have to be right
        // even when the remainder is zeroes.
        format!(
            "{} ({}) R {} {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0\n",
            s.pid,
            s.exe.rsplit('/').next().unwrap_or("gex"),
            s.ppid,
            s.pgid,
            s.sid
        )
    }
    fn status(&self) -> String {
        let s = self.0.lock().unwrap();
        format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nThreads:\t1\n",
            s.exe.rsplit('/').next().unwrap_or("gex"),
            s.pid,
            s.pid,
            s.ppid
        )
    }
}

impl Kernel {
    /// Refresh the snapshot `/proc` reads from.
    pub fn refresh_proc(&self, state: &Mutex<ProcState>) {
        let mut s = state.lock().unwrap();
        s.pid = self.pid;
        s.ppid = self.procs.ppid(self.pid);
        s.pgid = self.procs.pgid(self.pid, 0).unwrap_or(self.pid);
        s.sid = self.procs.sid(self.pid, 0).unwrap_or(self.pid);
        // Only when the map has actually changed. Formatting the VMA tree is
        // the single most expensive thing this function does -- half of all
        // execution in a browser run of an X server, which polls in a tight
        // loop with several hundred mappings live -- and between two `mmap`s
        // it produces the identical string every time.
        let generation = self.mem.generation();
        if s.maps_generation != generation || s.maps.is_empty() {
            s.maps = self.mem.proc_maps();
            s.maps_generation = generation;
        }
        // Compared before cloning, for the same reason at a smaller scale:
        // these are two allocations per syscall to arrive at the bytes that
        // are already there.
        if s.exe != self.exe_path {
            s.exe = self.exe_path.clone();
        }
        if s.cwd != self.vfs.cwd {
            s.cwd.clone_from(&self.vfs.cwd);
        }
        let fds_generation = self.fds.generation();
        if s.fds_generation != fds_generation || s.fds.is_empty() {
            s.fds = self.fds.list();
            s.fds_generation = fds_generation;
        }
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
/// Whether canonical syscall `n` can change another thread's descriptor
/// readiness (or signal state): what a host parking threads on descriptors
/// must wake them for. Over-approximate (any read or write, of any file);
/// futexes, clocks, memory and the like are left out because they are the
/// hot calls and never make a descriptor ready.
pub fn wakes_fd_waiters(n: u64) -> bool {
    matches!(
        n,
        nr::WRITE
            | nr::WRITEV
            | nr::PWRITE64
            | nr::READ
            | nr::READV
            | nr::PREAD64
            | nr::SENDTO
            | nr::SENDMSG
            | nr::SENDMMSG
            | nr::SENDFILE
            | nr::SPLICE
            | nr::RECVFROM
            | nr::RECVMSG
            | nr::RECVMMSG
            | nr::CLOSE
            | nr::CLOSE_RANGE
            | nr::DUP3
            | nr::SHUTDOWN
            | nr::CONNECT
            | nr::ACCEPT
            | nr::ACCEPT4
            | nr::LISTEN
            | nr::EXIT
            | nr::EXIT_GROUP
            | nr::KILL
            | nr::TKILL
            | nr::TGKILL
            | nr::TIMERFD_SETTIME
    )
}

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
            | nr::MREMAP
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
            | nr::CLONE3
            | nr::EXIT
            | nr::EXIT_GROUP
    )
}

/// Name a syscall for trace output.
pub fn syscall_name(n: u64) -> &'static str {
    match n {
        nr::DUP2 => "dup2",
        nr::TIME => "time",
        nr::ALARM => "alarm",
        nr::ARCH_PRCTL => "arch_prctl",
        nr::NOOP_OK => "advisory-noop",
        nr::GETCWD => "getcwd",
        nr::DUP => "dup",
        nr::DUP3 => "dup3",
        nr::FCNTL => "fcntl",
        nr::IOCTL => "ioctl",
        nr::MKDIRAT => "mkdirat",
        nr::UNLINKAT => "unlinkat",
        nr::LINKAT => "linkat",
        nr::SYMLINKAT => "symlinkat",
        nr::RENAMEAT => "renameat",
        nr::RENAMEAT2 => "renameat2",
        nr::FTRUNCATE => "ftruncate",
        nr::FACCESSAT | nr::FACCESSAT2 => "faccessat",
        nr::CHDIR => "chdir",
        nr::FCHDIR => "fchdir",
        nr::UNSHARE => "unshare",
        nr::FCHMOD => "fchmod",
        nr::FCHMODAT => "fchmodat",
        nr::OPENAT => "openat",
        nr::OPENAT2 => "openat2",
        nr::CLOSE => "close",
        nr::PIPE2 => "pipe2",
        nr::EVENTFD2 => "eventfd2",
        nr::TIMERFD_CREATE => "timerfd_create",
        nr::TIMERFD_SETTIME => "timerfd_settime",
        nr::TIMERFD_GETTIME => "timerfd_gettime",
        nr::GETDENTS64 => "getdents64",
        nr::LSEEK => "lseek",
        nr::READ => "read",
        nr::WRITE => "write",
        nr::READV => "readv",
        nr::WRITEV => "writev",
        nr::PREAD64 => "pread64",
        nr::PWRITE64 => "pwrite64",
        nr::SENDFILE => "sendfile",
        nr::COPY_FILE_RANGE => "copy_file_range",
        nr::SPLICE => "splice",
        nr::PPOLL => "ppoll",
        nr::PSELECT6 => "pselect6",
        nr::UTIMENSAT => "utimensat",
        nr::FCHOWN => "fchown",
        nr::FCHOWNAT => "fchownat",
        nr::STATFS => "statfs",
        nr::FSTATFS => "fstatfs",
        nr::UMASK => "umask",
        nr::READLINKAT => "readlinkat",
        nr::NEWFSTATAT => "newfstatat",
        nr::FSTAT => "fstat",
        nr::FSYNC => "fsync",
        nr::FDATASYNC => "fdatasync",
        nr::EXIT => "exit",
        nr::EXIT_GROUP => "exit_group",
        nr::SET_TID_ADDRESS => "set_tid_address",
        nr::FUTEX => "futex",
        nr::SET_ROBUST_LIST => "set_robust_list",
        nr::NANOSLEEP => "nanosleep",
        nr::CLOCK_GETTIME => "clock_gettime",
        nr::CLOCK_GETRES => "clock_getres",
        nr::CLOCK_NANOSLEEP => "clock_nanosleep",
        nr::SETITIMER => "setitimer",
        nr::GETITIMER => "getitimer",
        nr::SCHED_GETAFFINITY => "sched_getaffinity",
        nr::SCHED_YIELD => "sched_yield",
        nr::SCHED_SETPARAM => "sched_setparam",
        nr::SCHED_SETSCHEDULER => "sched_setscheduler",
        nr::SCHED_GETSCHEDULER => "sched_getscheduler",
        nr::SCHED_GETPARAM => "sched_getparam",
        nr::SCHED_GET_PRIORITY_MAX => "sched_get_priority_max",
        nr::SCHED_GET_PRIORITY_MIN => "sched_get_priority_min",
        nr::SETPRIORITY => "setpriority",
        nr::GETPRIORITY => "getpriority",
        nr::GETCPU => "getcpu",
        nr::KILL => "kill",
        nr::TKILL => "tkill",
        nr::TGKILL => "tgkill",
        nr::RT_SIGACTION => "rt_sigaction",
        nr::RT_SIGPROCMASK => "rt_sigprocmask",
        nr::RT_SIGPENDING => "rt_sigpending",
        nr::RT_SIGRETURN => "rt_sigreturn",
        nr::SIGALTSTACK => "sigaltstack",
        nr::TIMES => "times",
        nr::UNAME => "uname",
        nr::GETRUSAGE => "getrusage",
        nr::GETTIMEOFDAY => "gettimeofday",
        nr::GETPID => "getpid",
        nr::GETPPID => "getppid",
        nr::SETSID => "setsid",
        nr::GETSID => "getsid",
        nr::SETPGID => "setpgid",
        nr::GETPGID => "getpgid",
        nr::SETUID => "setuid",
        nr::SETGID => "setgid",
        nr::SETREUID => "setreuid",
        nr::SETREGID => "setregid",
        nr::SETRESUID => "setresuid",
        nr::SETRESGID => "setresgid",
        nr::GETRESUID => "getresuid",
        nr::GETRESGID => "getresgid",
        nr::GETGROUPS => "getgroups",
        nr::SETGROUPS => "setgroups",
        nr::PRCTL => "prctl",
        nr::SYNC => "sync",
        nr::STATX => "statx",
        nr::MOUNT => "mount",
        nr::UMOUNT2 => "umount2",
        nr::REBOOT => "reboot",
        nr::RT_SIGTIMEDWAIT => "rt_sigtimedwait",
        nr::SOCKET => "socket",
        nr::BIND => "bind",
        nr::LISTEN => "listen",
        nr::ACCEPT4 => "accept4",
        nr::MEMFD_CREATE => "memfd_create",
        nr::FLOCK => "flock",
        nr::FALLOCATE => "fallocate",
        nr::EPOLL_CREATE1 => "epoll_create1",
        nr::EPOLL_CTL => "epoll_ctl",
        nr::EPOLL_PWAIT => "epoll_pwait",
        nr::EPOLL_PWAIT2 => "epoll_pwait2",
        nr::CLOSE_RANGE => "close_range",
        nr::MEMBARRIER => "membarrier",
        nr::ACCEPT => "accept",
        nr::GETSOCKNAME => "getsockname",
        nr::GETPEERNAME => "getpeername",
        nr::RECVFROM => "recvfrom",
        nr::SETSOCKOPT => "setsockopt",
        nr::GETSOCKOPT => "getsockopt",
        nr::SHUTDOWN => "shutdown",
        nr::SENDMSG => "sendmsg",
        nr::RECVMSG => "recvmsg",
        nr::SENDMMSG => "sendmmsg",
        nr::RECVMMSG => "recvmmsg",
        nr::SOCKETPAIR => "socketpair",
        nr::CONNECT => "connect",
        nr::SENDTO => "sendto",
        nr::GETUID => "getuid",
        nr::GETEUID => "geteuid",
        nr::GETGID => "getgid",
        nr::GETEGID => "getegid",
        nr::GETTID => "gettid",
        nr::SYSINFO => "sysinfo",
        nr::BRK => "brk",
        nr::MUNMAP => "munmap",
        nr::MREMAP => "mremap",
        nr::CLONE => "clone",
        nr::CLONE3 => "clone3",
        nr::EXECVE => "execve",
        nr::MMAP => "mmap",
        nr::MPROTECT => "mprotect",
        nr::MADVISE => "madvise",
        nr::MSYNC => "msync",
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

    /// A unix socket that arrived over SCM_RIGHTS is placed straight in the
    /// descriptor table, never through `install_unix`; it must still be one
    /// to the calls that pass descriptors. (A per-process side table made it
    /// ENOTSOCK there, and `sendmsg`/`recvmsg` then carried no descriptors.)
    #[test]
    fn a_received_unix_socket_is_still_a_unix_socket() {
        let (_mem, mut k) = setup();
        let (a, _b) = unix::UnixSocket::pair();
        let fd = k
            .fds
            .insert(OpenFile::new(a.clone(), "socket".into(), 0, true, true), false)
            .unwrap() as i32;
        assert!(Arc::ptr_eq(&k.unix_of(fd).unwrap(), &a));
        // Anything else is not.
        let r = Arc::new(Pipe::new());
        let pfd = k
            .fds
            .insert(
                OpenFile::new(Arc::new(crate::file::PipeEnd { pipe: r, write: false }), "pipe".into(), 0, true, false),
                false,
            )
            .unwrap() as i32;
        assert_eq!(k.unix_of(pfd).err(), Some(Errno::ENOTSOCK));
    }

    /// What `clock_nanosleep` turns into: a sleep of the time left for an
    /// absolute deadline (none if it has passed), EINVAL for a malformed
    /// request instead of a sleep.
    #[test]
    fn the_monotonic_clock_counts_from_boot_not_the_epoch() {
        let (mut mem, mut k) = setup();
        let at = 0x2000u64;
        let mut read = |k: &mut Kernel, mem: &mut Vec<u8>, clk: u64| {
            let mut m = GuestMem::new(mem);
            k.sys_clock_gettime(&mut m, clk, at).unwrap();
            m.read_u64(at).unwrap()
        };
        let wall = read(&mut k, &mut mem, 0);
        let mono = read(&mut k, &mut mem, 1);
        // An uptime: milliseconds fit the 32 bits X server times have.
        assert!(mono >= 1 && mono < 49 * 86400, "monotonic {mono}");
        assert!(wall > 1_000_000_000, "realtime {wall}");
        // BOOTTIME and COARSE agree with it.
        for clk in [6, 7] {
            let other = read(&mut k, &mut mem, clk);
            assert!(other.abs_diff(mono) <= 1, "clock {clk}: {other} vs {mono}");
        }
    }

    #[test]
    fn clock_nanosleep_honours_absolute_deadlines() {
        let (mut mem, mut k) = setup();
        let at = 0x2000u64;
        let ask = |k: &mut Kernel, mem: &mut Vec<u8>, flags: u64, secs: i64, nanos: u64| {
            mem[at as usize..at as usize + 8].copy_from_slice(&secs.to_le_bytes());
            mem[at as usize + 8..at as usize + 16].copy_from_slice(&nanos.to_le_bytes());
            let m = GuestMem::new(mem);
            let op = k.blocking_op(&m, nr::CLOCK_NANOSLEEP, [1, flags, at, 0, 0, 0]);
            let now = match op {
                None => Some(k.sys_nanosleep(&m, at)),
                Some(_) => None,
            };
            (op, now)
        };
        // The request names clock 1, CLOCK_MONOTONIC.
        let now = k.clock_nanos(1) as i64 / 1_000_000_000;
        // Relative: slept as given.
        match ask(&mut k, &mut mem, 0, 0, 5_000_000) {
            (Some(BlockOp::Sleep(d)), _) => assert_eq!(d, std::time::Duration::from_millis(5)),
            other => panic!("{other:?}"),
        }
        // Absolute, two seconds ahead: about two seconds, not the 60 s cap.
        match ask(&mut k, &mut mem, 1, now + 2, 0) {
            (Some(BlockOp::Sleep(d)), _) => assert!(d <= std::time::Duration::from_secs(2), "{d:?}"),
            other => panic!("{other:?}"),
        }
        // Absolute and already past: returns at once.
        assert!(matches!(ask(&mut k, &mut mem, 1, now - 5, 0), (None, Some(Ok(0)))));
        // Malformed: EINVAL, not a sleep.
        assert!(matches!(ask(&mut k, &mut mem, 0, -1, 0), (None, Some(Err(Errno::EINVAL)))));
        assert!(matches!(ask(&mut k, &mut mem, 0, 0, 1_000_000_000), (None, Some(Err(Errno::EINVAL)))));
    }

    /// The terminal, as a program sees it through `ioctl`.
    ///
    /// These are the calls every interactive program makes before it draws
    /// anything, and the ones a shell makes to run a job. They were three --
    /// `TCGETS`, `TIOCGWINSZ`, `TIOCGPGRP` -- and everything else answered
    /// `ENOTTY`, which is how `stty`, `tcflush` and job control quietly did
    /// nothing.
    mod terminal {
        use super::*;
        use crate::tty;

        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TIOCGWINSZ: u64 = 0x5413;
        const TIOCSWINSZ: u64 = 0x5414;
        const TIOCGPGRP: u64 = 0x540f;
        const TIOCSPGRP: u64 = 0x5410;
        const TIOCGSID: u64 = 0x5429;
        const TIOCOUTQ: u64 = 0x5411;
        const TIOCGETD: u64 = 0x5424;
        const TIOCSETD: u64 = 0x5423;
        const TIOCEXCL: u64 = 0x540c;
        const TIOCGEXCL: u64 = 0x8004_5440;
        const TIOCMGET: u64 = 0x5415;
        const TCGETS2: u64 = 0x802c_542a;
        const TIOCGPTN: u64 = 0x8004_5430;
        const AT: u64 = 0x2000;

        /// A kernel whose standard input is a terminal, which is what the
        /// browser and the native host both build for an interactive run.
        ///
        /// Reinstalled rather than asked for: `Kernel::new` copies whether
        /// the *host's* stdin is a terminal, and a test runner's is a pipe.
        fn tty_kernel() -> (Vec<u8>, Kernel) {
            let mem = vec![0u8; 0x40000];
            let mut k = Kernel::new(0x1000, 0x10000, "/prog".into());
            k.vfs.mount("/", Arc::new(TmpFs::new()));
            k.fds = FdTable::new();
            for (stream, name, r, w) in [
                (Stream::In, "/dev/stdin", true, false),
                (Stream::Out, "/dev/stdout", false, true),
                (Stream::Err, "/dev/stderr", false, true),
            ] {
                let h = Arc::new(StdStream::new(stream, true));
                let _ = k.fds.insert(OpenFile::new(h, name.to_string(), 0, r, w), false);
            }
            (mem, k)
        }

        fn ioctl(k: &mut Kernel, mem: &mut Vec<u8>, req: u64, arg: u64) -> Result<u64, Errno> {
            let mut m = GuestMem::new(mem);
            k.sys_ioctl(&mut m, 0, req, arg)
        }

        #[test]
        fn termios_round_trips_through_the_kernel_shape() {
            let (mut mem, mut k) = tty_kernel();
            assert_eq!(ioctl(&mut k, &mut mem, TCGETS, AT), Ok(0));
            // 36 bytes and not one more: glibc's `tcgetattr` passes a
            // `struct __kernel_termios` on the stack, and the 24 bytes past
            // it are the caller's, ending at the stack canary.
            assert_eq!(&mem[AT as usize..AT as usize + 4], &0x6502u32.to_le_bytes());
            assert_eq!(mem[AT as usize + tty::KERNEL_TERMIOS], 0, "wrote past the kernel struct");
            // `intr` is `^C`, which is where a shell reads it from.
            assert_eq!(mem[AT as usize + 17], 3);

            // Raw mode, as a line editor sets it, and read back.
            let mut t = tty::COOKED;
            let l = u32::from_le_bytes([t[12], t[13], t[14], t[15]])
                & !(tty::lflag::ICANON | tty::lflag::ECHO);
            t[12..16].copy_from_slice(&l.to_le_bytes());
            mem[AT as usize..AT as usize + 60].copy_from_slice(&t);
            assert_eq!(ioctl(&mut k, &mut mem, TCSETS, AT), Ok(0));
            assert_eq!(k.tty.termios()[12..16], l.to_le_bytes());
        }

        #[test]
        fn the_window_size_is_read_written_and_shared() {
            let (mut mem, mut k) = tty_kernel();
            let ws = [40u16, 100, 0, 0];
            for (i, v) in ws.iter().enumerate() {
                mem[AT as usize + i * 2..AT as usize + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
            assert_eq!(ioctl(&mut k, &mut mem, TIOCSWINSZ, AT), Ok(0));
            mem[AT as usize..AT as usize + 8].fill(0);
            assert_eq!(ioctl(&mut k, &mut mem, TIOCGWINSZ, AT), Ok(0));
            assert_eq!(u16::from_le_bytes([mem[AT as usize], mem[AT as usize + 1]]), 40);
            assert_eq!(u16::from_le_bytes([mem[AT as usize + 2], mem[AT as usize + 3]]), 100);
        }

        #[test]
        fn the_foreground_group_is_settable_and_is_what_a_shell_reads_back() {
            let (mut mem, mut k) = tty_kernel();
            let mut m = GuestMem::new(&mut mem);
            m.write_u32(AT, 42).unwrap();
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCSPGRP, AT), Ok(0));
            m.write_u32(AT, 0).unwrap();
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCGPGRP, AT), Ok(0));
            assert_eq!(m.read_u32(AT).unwrap(), 42);
            // Zero is not a process group, and accepting it would silently
            // point `^C` at nobody.
            m.write_u32(AT, 0).unwrap();
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCSPGRP, AT), Err(Errno::EINVAL));
        }

        #[test]
        fn the_rest_of_the_surface_answers_rather_than_saying_enotty() {
            let (mut mem, mut k) = tty_kernel();
            // Every one of these was ENOTTY before, and every one of them is
            // asked by something in a Debian rootfs.
            assert_eq!(ioctl(&mut k, &mut mem, TIOCGSID, AT), Ok(0), "TIOCGSID");
            assert_eq!(ioctl(&mut k, &mut mem, TIOCOUTQ, AT), Ok(0), "TIOCOUTQ");
            assert_eq!(ioctl(&mut k, &mut mem, TIOCGETD, AT), Ok(0), "TIOCGETD");
            assert_eq!(ioctl(&mut k, &mut mem, TCGETS2, AT), Ok(0), "TCGETS2");
            assert_eq!(ioctl(&mut k, &mut mem, TIOCMGET, AT), Ok(0), "TIOCMGET");
            assert_eq!(ioctl(&mut k, &mut mem, TIOCEXCL, AT), Ok(0), "TIOCEXCL");
            assert_eq!(ioctl(&mut k, &mut mem, TIOCGEXCL, AT), Ok(0), "TIOCGEXCL");
            let mut m = GuestMem::new(&mut mem);
            assert_eq!(m.read_u32(AT).unwrap(), 1, "TIOCEXCL is read back");
            // A line discipline that is not N_TTY is refused rather than
            // accepted and ignored: a program told yes would then write SLIP
            // frames into a terminal.
            m.write_u32(AT, 1).unwrap();
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCSETD, AT), Err(Errno::EINVAL));
            m.write_u32(AT, 0).unwrap();
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCSETD, AT), Ok(0));
            // And this terminal is a console, not half of a pty pair. Saying
            // so is the honest answer and the one Linux gives.
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCGPTN, AT), Err(Errno::EINVAL));
        }

        #[test]
        fn a_pipe_is_still_not_a_terminal() {
            let (mut mem, mut k) = setup();
            let mut m = GuestMem::new(&mut mem);
            // fd 0 here is a plain stream, not a tty.
            assert_eq!(k.sys_ioctl(&mut m, 0, TCGETS, AT), Err(Errno::ENOTTY));
            assert_eq!(k.sys_ioctl(&mut m, 0, TIOCGWINSZ, AT), Err(Errno::ENOTTY));
        }
    }

    /// Load a system call the way the guest would: `n` is the canonical
    /// number, which an x86-64 guest spells as its own (the highest raw number
    /// `x86::translate` maps to it, so the modern form rather than a legacy
    /// one whose arguments it rewrites; unmapped numbers pass through as-is).
    fn set_args(m: &mut GuestMem<'_>, ctx_addr: u64, n: u64, args: &[u64]) {
        let raw = if gex_abi::X86 {
            (0..512).rev().find(|&r| crate::x86::canonical_number(r) == n).unwrap_or(n)
        } else if gex_abi::PPC {
            // `_llseek` (140) also reaches LSEEK, with its own arguments.
            if n == nr::LSEEK {
                crate::ppc::raw::LSEEK
            } else {
                (0..512).rev().find(|&r| crate::ppc::canonical_number(r) == n).unwrap_or(n)
            }
        } else {
            n
        };
        m.write_u64(ctx_addr + ctx::SYSCALL_NR as u64, raw).unwrap();
        for i in 0..6 {
            let v = args.get(i).copied().unwrap_or(0);
            m.write_u64(ctx_addr + ctx::SYSCALL_ARGS[i] as u64, v).unwrap();
        }
    }

    fn ret(m: &GuestMem<'_>, ctx_addr: u64) -> i64 {
        let v = m.read_u64(ctx_addr + ctx::SYSCALL_RET as u64).unwrap() as i64;
        // PowerPC: a positive errno, flagged by CR0.SO (`set_result`).
        if gex_abi::PPC && m.read(ctx_addr + (ctx::PPC_CR + 3) as u64, 1).unwrap()[0] != 0 {
            return -v;
        }
        v
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
            &[
                (-100i64) as u64,
                0x20000,
                oflags::O_CREAT | oflags::O_RDWR,
                0,
            ],
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
            &[
                (-100i64) as u64,
                0x20100,
                oflags::O_CREAT | oflags::O_RDWR,
                0,
            ],
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
        assert!(matches!(k.syscall(&mut m, 0), SyscallOutcome::Continue));
        assert_eq!(ret(&m, k.ctx_addr), -38);
        assert_eq!(k.unimplemented, vec![9999]);
    }

    #[test]
    fn exit_group_reports_the_status() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::EXIT_GROUP, &[7]);
        assert!(matches!(k.syscall(&mut m, 0), SyscallOutcome::Exit(7)));
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

    /// `recvmsg` honours its flags: `MSG_DONTWAIT` on an empty blocking
    /// socket is `EAGAIN`, not a wait (libwayland drains its socket this way
    /// and hung in its first round-trip when the flag was dropped), and
    /// `MSG_PEEK` leaves the bytes for the next read.
    #[test]
    fn recvmsg_flags() {
        // A 32-bit guest's msghdr is not read yet (i386's gap too), so the
        // call is ENOSYS there.
        if gex_abi::PPC {
            return;
        }
        const MSG_PEEK: u64 = 0x2;
        const MSG_DONTWAIT: u64 = 0x40;
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::SOCKETPAIR, &[1, 1, 0, 0x20000]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 0);
        let a = m.read_u32(0x20000).unwrap() as u64;
        let b = m.read_u32(0x20004).unwrap() as u64;
        // msghdr at 0x21000 with one iovec at 0x21100 -> 64 bytes at 0x22000.
        m.write_u64(0x21000 + 16, 0x21100).unwrap();
        m.write_u64(0x21000 + 24, 1).unwrap();
        m.write_u64(0x21100, 0x22000).unwrap();
        m.write_u64(0x21108, 64).unwrap();

        set_args(&mut m, k.ctx_addr, nr::RECVMSG, &[a, 0x21000, MSG_DONTWAIT]);
        assert!(matches!(k.syscall(&mut m, 0), SyscallOutcome::Continue));
        assert_eq!(ret(&m, k.ctx_addr), -(Errno::EAGAIN.0 as i64));

        set_args(&mut m, k.ctx_addr, nr::RECVMSG, &[a, 0x21000, 0]);
        assert!(
            !matches!(k.syscall(&mut m, 0), SyscallOutcome::Continue),
            "a plain recvmsg on an empty blocking socket must wait"
        );
        k.retry_recvmsg = None;

        m.write(0x23000, b"hello").unwrap();
        set_args(&mut m, k.ctx_addr, nr::WRITE, &[b, 0x23000, 5]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 5);
        set_args(&mut m, k.ctx_addr, nr::RECVMSG, &[a, 0x21000, MSG_PEEK]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 5);
        assert_eq!(m.read(0x22000, 5).unwrap(), b"hello");
        set_args(&mut m, k.ctx_addr, nr::RECVMSG, &[a, 0x21000, MSG_DONTWAIT]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), 5, "peek must leave the bytes queued");
        set_args(&mut m, k.ctx_addr, nr::RECVMSG, &[a, 0x21000, MSG_DONTWAIT]);
        k.syscall(&mut m, 0);
        assert_eq!(ret(&m, k.ctx_addr), -(Errno::EAGAIN.0 as i64));
    }

    #[test]
    fn uname_reports_the_guest_arch() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::UNAME, &[0x20000]);
        k.syscall(&mut m, 0);
        let arch = gex_abi::ARCH_NAME.as_bytes();
        assert_eq!(m.read(0x20000 + 65 * 4, arch.len()).unwrap(), arch);
    }
}
