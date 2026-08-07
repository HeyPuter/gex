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
    pub const SCHED_GETAFFINITY: u64 = 123;
    pub const SCHED_YIELD: u64 = 124;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
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
    pub const EPOLL_CREATE1: u64 = 20;
    pub const EPOLL_CTL: u64 = 21;
    pub const EPOLL_PWAIT: u64 = 22;
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
    },
    Sleep(std::time::Duration),
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
    AcceptWait { socket: Arc<unix::UnixSocket> },
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
        children: Vec<ChildHandle>,
        /// So the waiter can drop the record once it has collected the child,
        /// and can notice a halt while parked.
        procs: Arc<ProcTable>,
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
    /// The blocked mask an `rt_sigsuspend` has to put back when it returns.
    pub restore_sigmask: Option<u64>,
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
    /// Sockets by handle address, so a descriptor can be got back to the
    /// concrete socket the network syscalls need.
    pub sockets: std::collections::HashMap<usize, Arc<net::Socket>>,
    /// The same trick for unix sockets, which are a different concrete type.
    pub unix_sockets: std::collections::HashMap<usize, Arc<unix::UnixSocket>>,
    /// And for epoll sets.
    pub epolls: std::collections::HashMap<usize, Arc<unix::EpollSet>>,
    /// And for anonymous shared files, which have seals.
    pub memfds: std::collections::HashMap<usize, Arc<unix::MemFd>>,
    /// Which unix socket answers to which path. Shared system-wide, because
    /// a server and its clients are different processes and the whole point
    /// of a named socket is that they find each other by name.
    pub socket_names: Arc<unix::SocketNames>,
    /// Advisory whole-file locks, shared for the same reason.
    pub file_locks: Arc<unix::FileLocks>,
    /// File mappings whose writes are meant to reach the file.
    pub shared_maps: Vec<SharedMap>,
    /// Executable mappings made since the host last looked.
    pub new_code: Vec<CodeMap>,
    /// Serial for the unnamed files `O_TMPFILE` asks for.
    pub next_tmpfile: u64,
    /// The file-creation mask. Recorded rather than applied: nothing here
    /// enforces permissions, but `umask(022)` is expected to hand back what
    /// it replaced, and a build system that does not get one back concludes
    /// the mask was zero and re-runs itself.
    pub umask: u32,
    /// The terminal's size, as rows and columns. A page resizes.
    pub winsize: (u16, u16),
    /// The terminal settings, when there is a terminal.
    ///
    /// A `struct termios` as the guest sees it, kept whole rather than
    /// decoded: nothing here acts on the flags, but the *host* does. A page
    /// with a terminal has to know whether the guest has turned off echo and
    /// canonical input, because if it has then the guest is doing its own
    /// line editing and the page must stop doing its own -- otherwise every
    /// keystroke appears twice. Returning success from `TCSETS` while
    /// discarding what it set is how that goes wrong quietly.
    pub termios: [u8; 60],
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
pub fn system_clock() -> (u64, u32) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (now.as_secs(), now.subsec_nanos())
    }
    // Standing still is wrong but it is not a panic, and a host that cares
    // replaces this.
    #[cfg(target_arch = "wasm32")]
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
        nr::RENAMEAT | nr::LINKAT => a[1],
        // `symlinkat` names the link it creates third, not second.
        nr::SYMLINKAT => a[2],
        _ => return String::new(),
    };
    match m.read_cstr(at, 4096) {
        Ok(raw) => format!(" {:?}", String::from_utf8_lossy(&raw)),
        Err(_) => String::new(),
    }
}

/// A terminal in the mode a shell finds it in: echo on, input line at a time.
///
/// The flag words are what Linux hands back for a fresh pty. libc reads
/// `c_lflag` to decide whether a descriptor is a terminal at all, and musl
/// line-buffers stdout when it is.
/// How much of `struct termios` the kernel actually exchanges.
///
/// Four flag words, `c_line`, and `c_cc[19]`: `NCCS` is 19 in the kernel's
/// asm-generic definition, not the 32 a C library declares for its own
/// structure. Every `TCGETS`/`TCSETS` is this size and no more.
pub const KERNEL_TERMIOS: usize = 4 * 4 + 1 + 19;

pub const COOKED_TERMIOS: [u8; 60] = {
    let mut t = [0u8; 60];
    // c_iflag: ICRNL | IXON | BRKINT | ...
    let iflag = 0x2502u32.to_le_bytes();
    let oflag = 0x5u32.to_le_bytes(); // OPOST | ONLCR
    let cflag = 0xbfu32.to_le_bytes(); // B38400 | CS8 | CREAD
    let lflag = 0x8a3bu32.to_le_bytes(); // ISIG | ICANON | ECHO | ECHOE | ...
    t[0] = iflag[0]; t[1] = iflag[1]; t[2] = iflag[2]; t[3] = iflag[3];
    t[4] = oflag[0]; t[5] = oflag[1]; t[6] = oflag[2]; t[7] = oflag[3];
    t[8] = cflag[0]; t[9] = cflag[1]; t[10] = cflag[2]; t[11] = cflag[3];
    t[12] = lflag[0]; t[13] = lflag[1]; t[14] = lflag[2]; t[15] = lflag[3];
    // c_line is 0 (N_TTY), and then c_cc. Leaving these zero is not
    // harmless: an interactive shell reads its erase, interrupt and
    // end-of-file characters from here, and a zero erase character means
    // backspace does nothing. These are the kernel's own INIT_C_CC.
    //
    //     VINTR ^C   VQUIT ^\   VERASE DEL  VKILL ^U   VEOF ^D
    //     VTIME 0    VMIN 1     VSWTC 0     VSTART ^Q  VSTOP ^S
    //     VSUSP ^Z   VEOL 0     VREPRINT ^R VDISCARD ^O
    //     VWERASE ^W VLNEXT ^V  VEOL2 0
    let cc = [
        3u8, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0,
    ];
    let mut i = 0;
    while i < 19 {
        t[17 + i] = cc[i];
        i += 1;
    }
    t
};

/// `c_lflag`'s ICANON and ECHO, the two the host has to follow.
pub const ICANON: u32 = 0x2;
pub const ECHO: u32 = 0x8;

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
    if p.events & POLLOUT != 0 && p.writable {
        r |= POLLOUT;
    }
    r
}

pub fn poll_ready(fds: &[PollFd]) -> bool {
    fds.iter().any(|p| poll_revents(p) != 0)
}

/// Write the answer back and count what is ready. Shared by the immediate
/// reply and the one a parked wait gives when it wakes.
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
                let at = out + n * 16;
                m.write_u32(at, r as u32)?;
                m.write_u32(at + 4, 0)?;
                m.write_u64(at + 8, token)?;
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
            for (i, set) in [*readfds, *writefds].into_iter().enumerate() {
                if set == 0 {
                    continue;
                }
                for w in 0..words.min(16) {
                    m.write_u64(set + (w as u64) * 8, out[i][w])?;
                    ready += out[i][w].count_ones() as u64;
                }
            }
            // Nothing here ever reports an exceptional condition, but a
            // caller that passed the set expects it cleared.
            if *exceptfds != 0 {
                for w in 0..words.min(16) {
                    m.write_u64(*exceptfds + (w as u64) * 8, 0)?;
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
            procs: Arc::new(ProcTable::new()),
            ppid: 0,
            clock: system_clock,
            itimers: [None; 3],
            retry_accept: None,
            restore_sigmask: None,
            started: 0,
            shared_maps: Vec::new(),
            new_code: Vec::new(),
            next_tmpfile: 0,
            umask: 0o022,
            net: None,
            resolver: Arc::new(net::Resolver::new()),
            sockets: Default::default(),
            unix_sockets: Default::default(),
            epolls: Default::default(),
            memfds: Default::default(),
            socket_names: Arc::new(unix::SocketNames::new()),
            file_locks: Arc::new(unix::FileLocks::new()),
            winsize: (24, 80),
            termios: COOKED_TERMIOS,
        }
    }

    /// Read a whole file out of the guest's namespace.
    ///
    /// Used to load an `execve` target and its interpreter, so the program
    /// that runs is the one the guest can see -- the sysroot's `/bin/sh`, not
    /// the host's.
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
        let read_vec = |mut p: u64| -> Result<Vec<String>, Errno> {
            let mut out = Vec::new();
            // A bound, because the vector is the guest's and its terminator
            // may not be there.
            for _ in 0..4096 {
                if p == 0 {
                    break;
                }
                let ptr = m.read_u64(p)?;
                if ptr == 0 {
                    break;
                }
                out.push(bytes_to_string(m.read_cstr(ptr, 4096)?)?);
                p += 8;
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
            ppid: self.pid,
            clock: self.clock,
            // A fork does not inherit its parent's timers.
            itimers: [None; 3],
            retry_accept: None,
            restore_sigmask: None,
            // Nor its accumulated time: the child's clock starts now.
            started: self.now_nanos(),
            shared_maps: Vec::new(),
            new_code: Vec::new(),
            next_tmpfile: self.next_tmpfile,
            umask: self.umask,
            net: self.net.clone(),
            resolver: Arc::clone(&self.resolver),
            sockets: self.sockets.clone(),
            unix_sockets: self.unix_sockets.clone(),
            epolls: self.epolls.clone(),
            memfds: self.memfds.clone(),
            // Named sockets are system-wide: a forked child must still be
            // able to reach a server its parent could.
            socket_names: Arc::clone(&self.socket_names),
            file_locks: Arc::clone(&self.file_locks),
            winsize: self.winsize,
            termios: self.termios,
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
        if child + ctx::SIZE as u64 >= EXCLUSIVE_SCRATCH {
            return Err(Errno::EAGAIN);
        }

        let parent = self.ctx_addr;
        for off in (0..ctx::SIZE).step_by(8) {
            let v = m.read_u64(parent + off as u64)?;
            m.write_u64(child + off as u64, v)?;
        }
        m.write_u64(child + ctx::x(0) as u64, 0)?;
        m.write_u64(child + ctx::SP as u64, child_stack)?;
        m.write_u64(child + ctx::MON_SIZE as u64, 0)?;
        m.write_u64(child + ctx::TID as u64, tid as u64)?;
        if flags & clone_flags::SETTLS != 0 {
            m.write_u64(child + ctx::TPIDR as u64, tls)?;
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
                        eprintln!("[gex:{}] rt_sigreturn -> 0x{pc:x}", self.pid);
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
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, 0);
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
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, e.as_return());
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
            let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, pid as u64);
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
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, req.tid as u64);
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
                    let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, e.as_return());
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
                    eprintln!("[gex:{}] {} blocking", self.pid, syscall_name(n));
                }
            }
            return SyscallOutcome::Blocking(op);
        }

        let outcome = self.dispatch(m, n, a);
        let ret = match &outcome {
            Ok(r) => encode(*r),
            Err(exit) => {
                if self.trace {
                    eprintln!("[gex:{}] exit_group({exit})", self.pid);
                }
                return SyscallOutcome::Exit(*exit);
            }
        };
        if self.trace {
            eprintln!(
                "[gex:{}] pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}){} = {}",
                self.pid,
                syscall_name(n),
                a[0],
                a[1],
                a[2],
                traced_path(m, n, a),
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
                let want = m.read_u64(a[0]).ok()?;
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
            nr::EPOLL_PWAIT => {
                let set = self.epoll_of(a[0] as i32).ok()?;
                let fds = self.epoll_pollfds(&set);
                if poll_ready(&fds) {
                    return None;
                }
                // The timeout is milliseconds here rather than a `timespec`,
                // and negative means wait forever.
                let millis = a[3] as i32;
                if millis == 0 {
                    return None;
                }
                let deadline = if millis < 0 {
                    None
                } else {
                    Some(std::time::Duration::from_millis(millis as u64))
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
                // Only when it cannot be done in one go. A file, a terminal
                // and a socket all say `None` here and take the direct path,
                // which is every write in the common case; a pipe says how
                // much room it has, and only a write past that has to be
                // carried in pieces.
                if a[2] <= file.handle.write_room()? {
                    return None;
                }
                Some(BlockOp::WriteStream {
                    handle: Arc::clone(&file.handle),
                    buf: a[1],
                    len: a[2].min(1 << 24) as usize,
                })
            }
            nr::ACCEPT | nr::ACCEPT4 => {
                let file = self.fds.get(a[0] as i32).ok()?;
                let socket = self.unix_of(a[0] as i32).ok()?;
                if !file.would_block() || socket.has_pending() {
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
            nr::WAIT4 => {
                // Children come from the table, not from a list this process
                // owns, so a child reparented here when its own parent died
                // is one this `wait4` can collect. That is what makes pid 1
                // able to do its job.
                Some(BlockOp::WaitChild {
                    children: self.procs.children(self.pid),
                    procs: Arc::clone(&self.procs),
                    want: a[0] as i32,
                    wstatus: a[1],
                    nohang: a[2] & 1 != 0,
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
        // An `accept` that waited does its real work here, now that the
        // kernel is locked again and there is a descriptor table to put the
        // new socket in.
        let r = match self.retry_accept.take() {
            Some((fd, addr, len, flags)) if r.is_ok() => {
                self.sys_unix_accept(m, fd, addr, len, flags)
            }
            _ => r,
        };
        // `rt_sigsuspend` waited with a mask of the caller's choosing. The
        // old mask goes back *after* the handler runs, which means the frame
        // has to carry it -- restoring it here instead would block the very
        // signal that ended the wait, leaving it pending forever.
        let suspended = self.restore_sigmask.take();
        let ret = encode(r);
        let _ = m.write_u64(self.ctx_addr + ctx::x(0) as u64, ret);
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
                "[gex:{}] after blocking: delivery {:?}, pending {:#x}, blocked {:#x}",
                self.pid,
                delivery,
                self.signals.pending_raw(),
                self.signals.blocked
            );
        }
        match delivery {
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
            nr::CLOSE => self.sys_close(a[0] as i32),
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
            nr::STATX => self.sys_statx(m, a[0] as i32, a[1], a[2], a[4]),
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
            nr::UTIMENSAT => self.fds.get(a[0] as i32).map(|_| 0).or(Ok(0)),
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
            nr::BRK => Ok(self.mem.brk(a[0])),
            nr::MMAP => self.sys_mmap(m, a),
            nr::MUNMAP => {
                let flushed = self.flush_shared(m, a[0], a[1]);
                self.shared_maps
                    .retain(|s| s.start + s.len <= a[0] || s.start >= a[0] + a[1]);
                flushed.and_then(|_| self.mem.unmap(a[0], a[1])).map(|_| 0)
            }
            nr::MSYNC => self.flush_shared(m, a[0], a[1]).map(|_| 0),
            nr::MREMAP => self.sys_mremap(m, a),
            nr::MPROTECT => self.mem.protect(a[0], a[1], (a[2] & 0x7) as u32).map(|_| 0),
            nr::MADVISE => Ok(0),

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
            nr::RECVMSG => self.sys_recvmsg(m, a[0] as i32, a[1]),
            nr::SENDMMSG => self.sys_sendmmsg(m, a[0] as i32, a[1], a[2]),
            nr::RECVMMSG => self.sys_recvmmsg(m, a[0] as i32, a[1], a[2]),
            nr::GETPEERNAME => self.sys_getpeername(m, a[0] as i32, a[1], a[2]),
            // One interface, unnamed. A guest that asks where it is calling
            // from gets 0.0.0.0, which is what an unbound socket reports.
            nr::GETSOCKNAME => self.sys_write_sockaddr(m, 0, 0, a[1], a[2]),
            // Nothing here has an option worth setting -- no timeouts, no
            // buffer sizes that mean anything -- and refusing makes libraries
            // treat the socket as broken.
            nr::SETSOCKOPT => Ok(0),
            nr::GETSOCKOPT => self.sys_getsockopt(m, a[0] as i32, a[2], a[3]),
            nr::SHUTDOWN => self.fds.get(a[0] as i32).map(|_| 0),
            // A local address means nothing when there is one interface and
            // it is a proxy, but a resolver binds its socket before it sends
            // and gives up if that fails.
            nr::BIND => match self.sys_unix_bind(m, a[0] as i32, a[1], a[2]) {
                Err(Errno::ENOTSOCK) => self.fds.get(a[0] as i32).map(|_| 0),
                other => other,
            },
            nr::LISTEN => self.unix_of(a[0] as i32).and_then(|s| s.listen()),
            nr::ACCEPT => self.sys_unix_accept(m, a[0] as i32, a[1], a[2], 0),
            nr::ACCEPT4 => self.sys_unix_accept(m, a[0] as i32, a[1], a[2], a[3]),
            nr::SOCKETPAIR => self.sys_socketpair(m, a[0], a[1], a[3]),
            nr::MEMFD_CREATE => self.sys_memfd_create(m, a[0], a[1]),
            nr::FLOCK => self.sys_flock(a[0] as i32, a[1]),
            nr::FALLOCATE => self.sys_fallocate(a[0] as i32, a[1], a[2], a[3]),
            nr::EPOLL_CREATE1 => self.sys_epoll_create(a[0]),
            nr::EPOLL_CTL => self.sys_epoll_ctl(m, a[0] as i32, a[1], a[2] as i32, a[3]),
            nr::EPOLL_PWAIT => self.sys_epoll_wait(m, a[0] as i32, a[1], a[2] as u32),
            // No listening, though: a page cannot be connected to.
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
            nr::TIMES => self.sys_times(m, a[0]),
            nr::GETRUSAGE => self.sys_getrusage(m, a[1]),
            nr::SYSINFO => self.sys_sysinfo(m, a[0]),
            nr::PRLIMIT64 => self.sys_prlimit(m, a[2], a[3]),

            // -- signals -----------------------------------------------------
            nr::RT_SIGACTION => self.sys_rt_sigaction(m, a[0] as u32, a[1], a[2]),
            nr::RT_SIGTIMEDWAIT => self.sys_rt_sigtimedwait(m, a),
            nr::RT_SIGPROCMASK => self.sys_rt_sigprocmask(m, a[0] as u32, a[1], a[2]),
            nr::SIGALTSTACK => self.sys_sigaltstack(m, a[0], a[1]),
            // One process and one thread for now, so any target is us.
            // `raise` goes through tkill, which is why it needs its own entry
            // rather than falling into the unimplemented bucket.
            // `tkill` and `tgkill` address a thread, and every thread of a
            // process shares this kernel, so they raise here directly.
            nr::TKILL | nr::TGKILL => {
                let signo = if n == nr::TKILL { a[1] } else { a[2] } as u32;
                if signo > 0 {
                    self.signals.raise(signo);
                }
                Ok(0)
            }
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

    fn sys_futex(&mut self, _m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (uaddr, raw_op, val) = (a[0], a[1] as u32, a[2] as u32);
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
        self.collect_signals();
        self.check_timers();
        if !self.signals.has_pending() {
            return Delivery::None;
        }
        signal::dispatch(&mut self.signals, m, self.ctx_addr).unwrap_or(Delivery::None)
    }

    /// Move signals other processes have sent into this one's pending set.
    ///
    /// `kill` cannot reach into another process's kernel -- each has its own
    /// -- so it leaves the signal on a shared per-process word and the target
    /// picks it up here. Nothing did: `ProcState::take_signals` existed, was
    /// tested, and had no caller outside its own tests, so `kill(pid, sig)`
    /// recorded a signal that was never delivered to anything.
    ///
    /// A child asleep in `nanosleep` therefore ran its full thirty seconds
    /// and exited normally when its parent had sent it `SIGTERM` a second in;
    /// a spinning one never stopped at all. That is `timeout`, job control,
    /// and every supervisor.
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
                self.signals.raise(signo);
            }
        }
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
                "[gex:{}] signal pending {:#x} but blocked {:#x}",
                self.pid,
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

    fn sys_pwrite(&mut self, m: &GuestMem<'_>, fd: i32, buf: u64, len: u64, off: u64) -> SysResult {
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
            let data = match m.read(base, len as usize) {
                Ok(d) => d.to_vec(),
                // Same rule as below: bytes already accepted have to be
                // reported, or the caller sends them twice.
                Err(e) => return if total > 0 { Ok(total) } else { Err(e) },
            };
            let n = match file.write(&data) {
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
        let vecs = Self::read_iovecs(m, iov, count)?;
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

    fn sys_openat(
        &mut self,
        m: &GuestMem<'_>,
        dirfd: i32,
        path: u64,
        flags: u64,
        mode: u32,
    ) -> SysResult {
        let p = self.at_path(m, dirfd, path)?;
        // `O_TMPFILE` names a directory and asks for an unnamed file inside
        // it. Ignoring the bit opens the directory, and every write to it
        // then fails with `EISDIR` -- which is what apt saw while writing its
        // package cache.
        //
        // The file is made, opened, and its name removed straight away, which
        // is what "unnamed" means: the handle keeps it alive and nothing else
        // can find it. A later `linkat` to give it a name would fail, and
        // that is honest -- the alternative is leaving litter in the guest's
        // filesystem under a name it never chose.
        // `O_PATH` opens the name, not the file. tar uses it to get a handle
        // on a symlink it has just created so it can set the mode through
        // it, and the symlink's target very often does not exist yet -- so
        // opening it the ordinary way follows the link and answers `ENOENT`,
        // which tar reports as a failure to unpack the archive.
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
            let _ = self.vfs.unlink(&tmp);
            h
        } else {
            self.vfs.open(&p, flags, mode)?
        };
        let acc = flags & oflags::ACCMODE;
        let readable = acc == oflags::O_RDONLY || acc == oflags::O_RDWR;
        let writable = acc == oflags::O_WRONLY || acc == oflags::O_RDWR;
        let file = OpenFile::new(handle, p, flags, readable, writable);
        self.fds.insert(file, flags & oflags::O_CLOEXEC != 0)
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
        let from = self.at_path(m, a[0] as i32, a[1])?;
        let to = self.at_path(m, a[2] as i32, a[3])?;
        self.vfs.link(&from, &to).map(|_| 0)
    }

    fn sys_pipe2(&mut self, m: &mut GuestMem<'_>, fds: u64, flags: u64) -> SysResult {
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

    /// `statx`, which is how glibc stats a file on any kernel new enough to
    /// have it.
    ///
    /// Answering `ENOSYS` is supposed to be safe -- glibc falls back to
    /// `fstatat` -- and it very nearly is: the fallback runs, returns the
    /// right answer, and then `ls` branches into padding. Rather than find
    /// out why a path that is barely exercised on any real system goes wrong,
    /// this is the path glibc wants.
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
            o[at..at + 4].copy_from_slice(&v.to_le_bytes())
        };
        let put64 = |o: &mut [u8; 256], at: usize, v: u64| {
            o[at..at + 8].copy_from_slice(&v.to_le_bytes())
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
        out[28..30].copy_from_slice(&(st.mode as u16).to_le_bytes());
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
            F_SETLK | F_SETLKW => {
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
        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TCSETSW: u64 = 0x5403;
        const TCSETSF: u64 = 0x5404;
        const TIOCGWINSZ: u64 = 0x5413;
        const FIONREAD: u64 = 0x541b;
        const TIOCGPGRP: u64 = 0x540f;
        const TIOCSPGRP: u64 = 0x5410;
        const TIOCSWINSZ: u64 = 0x5414;
        let file = self.fds.get(fd)?;
        // A device the host provides answers its own ioctls, and knows things
        // the kernel does not. Asked first, because the request numbers below
        // are the terminal's and a graphics device's are its own.
        if let Some(r) = file.handle.ioctl(m, req, arg) {
            return r;
        }
        let is_tty = file.is_tty();
        match req {
            TCGETS if is_tty => {
                // Only the kernel's part of the structure. This is not a
                // detail: glibc's `tcgetattr` passes a `struct
                // __kernel_termios` that lives on the caller's stack, and on
                // this architecture that is 36 bytes because NCCS is 19.
                // Writing the 60-byte userspace shape into it overruns by 24
                // and lands on the stack canary -- which is exactly what
                // `bash -i` did, and why it reported stack smashing before it
                // had printed a prompt. musl hides the bug by passing its own
                // larger struct straight through.
                m.write(arg, &self.termios[..KERNEL_TERMIOS])?;
                Ok(0)
            }
            // Kept, not discarded. A shell that turns off ICANON and ECHO is
            // taking over line editing, and the host has to stop doing its
            // own or every keystroke lands twice.
            TCSETS | TCSETSW | TCSETSF if is_tty => {
                // The same shape in the other direction: the caller gave a
                // kernel-sized structure, so reading past it would read the
                // caller's stack.
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                self.termios[..KERNEL_TERMIOS].copy_from_slice(&src);
                Ok(0)
            }
            TIOCGWINSZ if is_tty => {
                let mut ws = [0u8; 8];
                ws[0..2].copy_from_slice(&self.winsize.0.to_le_bytes());
                ws[2..4].copy_from_slice(&self.winsize.1.to_le_bytes());
                m.write(arg, &ws)?;
                Ok(0)
            }
            TIOCGPGRP if is_tty => {
                m.write_u32(arg, self.procs.pgid(self.pid, 0).unwrap_or(self.pid) as u32)?;
                Ok(0)
            }
            // A shell claiming the terminal for a job. There is one terminal
            // and no contest for it, so this only has to not fail.
            TIOCSPGRP if is_tty => Ok(0),
            TIOCSWINSZ if is_tty => Ok(0),
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

    /// `symlinkat(target, newdirfd, linkpath)`.
    ///
    /// The directory descriptor is the third argument and it was being
    /// ignored: every symlink resolved against the working directory
    /// instead. A program that opens its destination and works relative to it
    /// -- which is what tar does, and what anything careful about races does
    /// -- got `ENOENT`, because the parent it had just created is not there
    /// relative to the cwd. Unpacking any Debian package containing a symlink
    /// failed on it.
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

    /// `ppoll` over the descriptors gex can answer for.
    ///
    /// Regular files are always ready, which is what Linux reports; pipes
    /// answer from their buffer state. There is no waiting yet, so a poll
    /// that would block returns zero ready descriptors after the timeout is
    /// checked, which callers handle as a spurious wakeup.
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
                let word = m.read_u64(set + (w as u64) * 8).ok()?;
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
            let bytes = m.read(from, (to - from) as usize)?;
            map.file.pwrite(map.offset + (from - map.start), bytes)?;
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
        // `AT_SYMLINK_NOFOLLOW` on a symlink is not something Linux supports:
        // a symlink has no mode of its own, and the call reports
        // `EOPNOTSUPP`. Following the link instead is both wrong and
        // dangerous -- it changes a different file -- and here it produced a
        // stranger failure, because a symlink is very often created before
        // its target exists.
        //
        // GNU tar unpacking a package does exactly that, and `chmod`
        // following the link answered `ENOENT`. tar reports that as a real
        // error, dpkg-deb reports tar's failure, and a Debian package with a
        // dangling symlink in it -- which `libpython3.11-minimal` has, the
        // last entry in the archive -- fails to install after unpacking
        // perfectly.
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

    /// `setitimer` and `getitimer`.
    ///
    /// A `struct itimerval` is two `timeval`s -- reload interval then time
    /// remaining -- and a `timeval` is two 64-bit words, seconds and
    /// *microseconds*. Thirty-two bytes in all.
    ///
    /// These used to be accepted and dropped, on the reasoning that a program
    /// that cannot arm a timeout is worse off than one whose timeout never
    /// fires. That reasoning is wrong for the programs that actually use
    /// them: `openssl speed` runs its measurement loop until SIGALRM arrives
    /// and nothing else ever stops it, so accepting the call and never firing
    /// turned a two-second benchmark into an infinite one.
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

    /// Fire any timer whose deadline has passed, and reload it if it repeats.
    ///
    /// Called wherever a signal could be delivered, which on this design is
    /// every back-edge safepoint. That is the right granularity for a timer
    /// whose job is to interrupt a loop, and the wrong one for a process
    /// asleep in `read` -- a guest blocked in the host has no back-edges, so
    /// its timer waits for the read to finish. Worth knowing before relying on
    /// one of these as a watchdog.
    ///
    /// `ITIMER_VIRTUAL` and `ITIMER_PROF` are meant to count the process's own
    /// CPU time rather than wall clock. They are driven from the same clock
    /// here, which for a guest that is mostly running is nearly the same
    /// number and for one that is mostly blocked is not.
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
            self.signals.raise(match which {
                ITIMER_VIRTUAL => signal::sig::SIGVTALRM,
                ITIMER_PROF => signal::sig::SIGPROF,
                _ => signal::sig::SIGALRM,
            });
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
        } else if new_len > old_len {
            // Growing in place must expose zeroes, not whatever the guest
            // left behind the last time this range was mapped.
            m.zero(addr + old_len, (new_len - old_len) as usize)?;
        }
        Ok(dst)
    }

    fn sys_mmap(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let (addr, len, prot_bits, flags, fd, offset) =
            (a[0], a[1], a[2] as u32, a[3], a[4] as i32, a[5]);
        if flags & mflags::ANONYMOUS == 0 && fd >= 0 {
            // A private file mapping is an eager copy, which is exactly
            // correct and needs no page faults (PLAN §7.4).
            let len_aligned = crate::mem::page_align_up(len);
            let file = self.fds.get(fd)?;
            let name = file.path.clone();
            // A second `MAP_SHARED` of the same open file must see the first
            // one's writes -- that is what shared means -- and the way to
            // arrange that in a single address space is to hand back the same
            // pages. A Wayland client maps a buffer and sends the descriptor;
            // whoever maps it next has to be looking at the same pixels, and
            // with a copy they were looking at zeroes.
            //
            // The mapping is not reference counted, so unmapping one alias
            // unmaps both. Nothing that does this unmaps early.
            if flags & crate::mem::flags::SHARED != 0 {
                if let Some(existing) = self.shared_maps.iter().find(|s| {
                    Arc::ptr_eq(&s.file, &file) && s.offset == offset && s.len >= len_aligned
                }) {
                    return Ok(existing.start);
                }
            }
            let start = self.mem.mmap(addr, len_aligned, prot_bits, flags, &name)?;
            let mut tmp = vec![0u8; len as usize];
            let n = file.pread(offset, &mut tmp)? as usize;
            m.zero(start, len_aligned as usize)?;
            m.write(start, &tmp[..n])?;
            // A shared mapping is expected to reach the file. The copy above
            // is the whole of the mapping, so nothing does until it is
            // written back -- at `msync`, and at `munmap`, which is where a
            // program that never calls `msync` expects its changes to land.
            // An executable file mapping is a shared library arriving. Tell
            // the host, so the static recompiler gets a whole object at once
            // rather than the runtime translator getting it a miss at a time.
            if prot_bits & crate::mem::prot::EXEC != 0 {
                self.new_code.push(CodeMap {
                    start,
                    end: start + len_aligned,
                    path: name.clone(),
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
        let start = self.mem.mmap(addr, len, prot_bits, flags, "")?;
        // Anonymous memory is defined to read as zero.
        m.zero(start, crate::mem::page_align_up(len) as usize)?;
        Ok(start)
    }


    // -- sessions, credentials and mounts --------------------------------------

    /// Move anything another process has sent into this one's pending set.
    fn take_sent_signals(&mut self) {
        if !self.procs.has_signals(self.pid) {
            return;
        }
        let mut sent = self.procs.take_signals(self.pid);
        while sent != 0 {
            let signo = sent.trailing_zeros() + 1;
            sent &= sent - 1;
            self.signals.raise(signo);
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

    /// `rt_sigtimedwait`, for a signal already pending.
    ///
    /// The blocking case is not implemented: a caller that asks to wait gets
    /// `EAGAIN` as though the timeout had expired. That is a real limitation
    /// and it is visible -- `sigwaitinfo` with no timeout is supposed never to
    /// return empty-handed -- but the alternative is a fifth kind of
    /// [`BlockOp`], and every caller seen so far uses this to drain signals it
    /// has blocked rather than to sleep on them.
    fn sys_rt_sigtimedwait(&mut self, m: &mut GuestMem<'_>, a: [u64; 6]) -> SysResult {
        let set = if a[0] == 0 { 0 } else { m.read_u64(a[0])? };
        match self.signals.take_pending_in(set) {
            Some(signal) => {
                // A `siginfo_t` the caller can read the number out of: si_signo,
                // si_errno, si_code, then the union, which nothing here fills.
                if a[1] != 0 {
                    m.zero(a[1], 128)?;
                    m.write_u32(a[1], signal)?;
                }
                Ok(signal as u64)
            }
            None => Err(Errno::EAGAIN),
        }
    }


    // -- sockets ---------------------------------------------------------------

    /// `socket`. Only AF_INET, and only the two kinds that matter: a stream,
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
        let sock = Arc::new(net::Socket::new(kind, net, Arc::clone(&self.resolver)));
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

    /// The unix socket behind a descriptor, if it is one.
    fn unix_of(&self, fd: i32) -> Result<Arc<unix::UnixSocket>, Errno> {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        self.unix_sockets.get(&key).cloned().ok_or(Errno::ENOTSOCK)
    }

    /// Put a unix socket in the descriptor table and remember the concrete
    /// type, the way `sys_socket` does for an internet one.
    fn install_unix(&mut self, sock: Arc<unix::UnixSocket>, flags: u64, cloexec: bool) -> SysResult {
        let handle: Arc<dyn crate::vfs::FileHandle> = sock.clone();
        let key = Arc::as_ptr(&handle) as *const () as usize;
        let file = OpenFile::new(handle, "socket".to_string(), flags, true, true);
        let fd = self.fds.insert(file, cloexec)?;
        self.unix_sockets.insert(key, sock);
        Ok(fd)
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
            buf[..2].copy_from_slice(&AF_UNIX.to_le_bytes());
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

    /// `socketpair`, which is a connected pair with no name.
    /// `memfd_create`: an anonymous file with no name in any filesystem.
    ///
    /// A Wayland client's buffers are these. The `path` recorded is what
    /// Linux shows in `/proc/self/fd`, which is where a program looks to work
    /// out what a descriptor it was handed actually is.
    /// `close`, which also drops any advisory lock the description held.
    ///
    /// Only when this was the last descriptor onto it: `dup` and `fork` share
    /// one description and one lock, so closing a copy releases nothing. And
    /// it has to happen, because the lock is keyed by the description's
    /// address and a freed one's address is handed straight back to the next
    /// open -- a stale lock would then belong to an unrelated file.
    fn sys_close(&mut self, fd: i32) -> SysResult {
        let file = self.fds.get(fd)?;
        let key = Arc::as_ptr(&file) as *const () as usize;
        self.fds.close(fd)?;
        // One reference in the table is gone; `file` is the other. Anything
        // above that is another descriptor.
        if Arc::strong_count(&file) == 1 {
            self.file_locks.release(key);
        }
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
            let at = out + i as u64 * 16;
            m.write_u32(at, *events)?;
            m.write_u32(at + 4, 0)?;
            m.write_u64(at + 8, *data)?;
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
        let names = Arc::clone(&self.socket_names);
        names.connect(&sock, &path)
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
            let peer = self.socket_of(fd).ok().and_then(|s| s.peer());
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
                net::Status::Connecting => Errno::EINPROGRESS.0,
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

    /// A `struct cmsghdr` is a length, a level, a type, then the payload,
    /// with every field and the whole header aligned to a pointer.
    const CMSG_ALIGN: u64 = 8;
    const CMSG_HDR: u64 = 16;

    fn cmsg_align(n: u64) -> u64 {
        n.div_ceil(Self::CMSG_ALIGN) * Self::CMSG_ALIGN
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
        let mut out = Vec::new();
        let mut at = 0u64;
        while at + Self::CMSG_HDR <= len {
            let cmsg_len = m.read_u64(control + at)?;
            if cmsg_len < Self::CMSG_HDR || at + cmsg_len > len {
                break;
            }
            let level = m.read_u32(control + at + 8)?;
            let ty = m.read_u32(control + at + 12)?;
            if level == SOL_SOCKET && ty == SCM_RIGHTS {
                let n = (cmsg_len - Self::CMSG_HDR) / 4;
                for i in 0..n.min(253) {
                    let fd = m.read_u32(control + at + Self::CMSG_HDR + i * 4)? as i32;
                    out.push(self.fds.get(fd)?);
                }
            }
            at += Self::cmsg_align(cmsg_len);
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
        let payload = files.len() as u64 * 4;
        let cmsg_len = Self::CMSG_HDR + payload;
        if cmsg_len > len {
            return Ok(0);
        }
        m.write_u64(control, cmsg_len)?;
        m.write_u32(control + 8, SOL_SOCKET)?;
        m.write_u32(control + 12, SCM_RIGHTS)?;
        for (i, f) in files.into_iter().enumerate() {
            let fd = self.fds.insert(f, false)?;
            m.write_u32(control + Self::CMSG_HDR + i as u64 * 4, fd as u32)?;
        }
        Ok(cmsg_len)
    }

    /// `sendmsg`, which is how glibc writes to a socket, and the only way to
    /// pass a descriptor to another process.
    fn sys_sendmsg(&mut self, m: &GuestMem<'_>, fd: i32, msg: u64) -> SysResult {
        // `msg_name` is where this one goes, and a reply has to be reported
        // as having come from there or a resolver drops it as spoofed.
        let name = m.read_u64(msg)?;
        if name != 0 {
            if let Ok(sock) = self.socket_of(fd) {
                let bytes = m.read(name, 16)?.to_vec();
                if let Ok((ip, port)) = net::parse_sockaddr(&bytes) {
                    sock.set_peer(ip, port);
                }
            }
        }
        let iov = m.read_u64(msg + 16)?;
        let iovlen = m.read_u64(msg + 24)?;
        let mut data = Vec::new();
        for i in 0..iovlen.min(1024) {
            let base = m.read_u64(iov + i * 16)?;
            let len = m.read_u64(iov + i * 16 + 8)? as usize;
            if len > 0 {
                data.extend_from_slice(m.read(base, len.min(1 << 22))?);
            }
        }
        // Ancillary data, which for a unix socket may be descriptors. Staged
        // on the socket rather than passed down, so the send attaches them to
        // the first byte it writes and a send that fails attaches nothing.
        let control = m.read_u64(msg + 32)?;
        let controllen = m.read_u64(msg + 40)?;
        if control != 0 && controllen >= Self::CMSG_HDR {
            if let Ok(sock) = self.unix_of(fd) {
                let files = self.read_scm_rights(m, control, controllen)?;
                sock.stage_send(files);
            }
        }
        self.fds.get(fd)?.write(&data)
    }

    /// One `struct mmsghdr` is a `msghdr` and the length it carried.
    const MMSGHDR: u64 = 64;

    /// `sendmmsg`: several datagrams in one call.
    ///
    /// glibc's resolver sends its A and AAAA queries this way, and answering
    /// `ENOSYS` does not send it back to `sendto` -- it reports the name as
    /// unresolvable, which is how `apt-get update` failed with "Temporary
    /// failure resolving".
    fn sys_sendmmsg(&mut self, m: &mut GuestMem<'_>, fd: i32, vec: u64, n: u64) -> SysResult {
        let mut sent = 0u64;
        for i in 0..n.min(1024) {
            let hdr = vec + i * Self::MMSGHDR;
            match self.sys_sendmsg(m, fd, hdr) {
                Ok(len) => {
                    m.write_u32(hdr + 56, len as u32)?;
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

    fn sys_recvmmsg(&mut self, m: &mut GuestMem<'_>, fd: i32, vec: u64, n: u64) -> SysResult {
        let mut got = 0u64;
        for i in 0..n.min(1024) {
            let hdr = vec + i * Self::MMSGHDR;
            match self.sys_recvmsg(m, fd, hdr) {
                Ok(len) => {
                    m.write_u32(hdr + 56, len as u32)?;
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

    fn sys_recvmsg(&mut self, m: &mut GuestMem<'_>, fd: i32, msg: u64) -> SysResult {
        let iov = m.read_u64(msg + 16)?;
        let iovlen = m.read_u64(msg + 24)?;
        let control = m.read_u64(msg + 32)?;
        let controllen = m.read_u64(msg + 40)?;
        // How many descriptors the caller left room for. A reader that offers
        // no room leaves them queued for the next call rather than losing
        // them, which is what Wayland's two-stage read depends on.
        let room = if control != 0 && controllen >= Self::CMSG_HDR {
            ((controllen - Self::CMSG_HDR) / 4) as usize
        } else {
            0
        };
        let unix_sock = self.unix_of(fd).ok();
        let mut total = 0u64;
        for i in 0..iovlen.min(1024) {
            let base = m.read_u64(iov + i * 16)?;
            let len = m.read_u64(iov + i * 16 + 8)? as usize;
            if len == 0 {
                continue;
            }
            let mut tmp = vec![0u8; len.min(1 << 22)];
            let n = match &unix_sock {
                Some(sock) => sock.recv(&mut tmp, room, false)?,
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
        m.write_u64(msg + 40, written)?;
        Ok(total)
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

    /// How much time this process has used, in nanoseconds.
    ///
    /// Wall clock since it started, not processor time: nothing here measures
    /// how much of that the guest was actually scheduled for, and on a host
    /// where the guest is one thread of one process there is no portable way
    /// to ask. For a guest that is computing it is very nearly the same
    /// number, and for one that spends its life blocked it is not.
    ///
    /// The alternative was what this used to do, which is report zero.
    /// `openssl speed` divides by it, and printed every throughput as `infk`.
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
        nr::LINKAT => "linkat",
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

    #[test]
    fn uname_reports_aarch64() {
        let (mut buf, mut k) = setup();
        let mut m = GuestMem::new(&mut buf);
        set_args(&mut m, k.ctx_addr, nr::UNAME, &[0x20000]);
        k.syscall(&mut m, 0);
        assert_eq!(m.read(0x20000 + 65 * 4, 7).unwrap(), b"aarch64");
    }
}
