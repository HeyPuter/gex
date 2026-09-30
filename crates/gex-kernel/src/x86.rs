//! The x86-64 guest ABI where it differs from the asm-generic one the kernel
//! is written against: system-call numbering, the legacy calls that
//! architecture kept, and `cpuid`.
//!
//! The kernel's `syscalls::nr` constants are the canonical numbering (the
//! asm-generic ABI aarch64 uses). An x86-64 guest's raw number is translated
//! to it at entry by [`translate`], which also rewrites the arguments of the
//! legacy forms (`open` → `openat(AT_FDCWD, …)`, `fork` → `clone(SIGCHLD)`,
//! `poll` → `ppoll` with a timespec in the thread's scratch area) so the rest
//! of the kernel never sees them.

use crate::GuestMem;
use crate::syscalls::nr;

/// A raw number with no canonical equivalent. Dispatch answers `ENOSYS`.
pub const UNKNOWN: u64 = u64::MAX;

/// The pseudo system register the lifter writes to ask for `cpuid`
/// (`gex_lift_x86::sysreg::CPUID`; the two must agree).
pub const SYSREG_CPUID: i32 = -100;

/// The pseudo system register the lifter writes to have an x87 instruction
/// computed here (`gex_lift_x86::sysreg::X87`; the two must agree). See
/// [`serve_x87`].
pub const SYSREG_X87: i32 = -101;

/// The operations [`SYSREG_X87`] carries (`gex_lift_x86::x87op`).
pub mod x87op {
    pub const F2XM1: u64 = 1;
    pub const FYL2X: u64 = 2;
    pub const FYL2XP1: u64 = 3;
    pub const FPTAN: u64 = 4;
    pub const FPATAN: u64 = 5;
    pub const FXTRACT: u64 = 6;
    pub const FPREM: u64 = 7;
    pub const FPREM1: u64 = 8;
    pub const FSIN: u64 = 9;
    pub const FCOS: u64 = 10;
    pub const FSINCOS: u64 = 11;
}

const AT_FDCWD: u64 = (-100i64) as u64;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const SIGCHLD: u64 = 17;

/// x86-64 raw syscall numbers (`asm/unistd_64.h`).
#[allow(dead_code)]
pub mod raw {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    pub const SELECT: u64 = 23;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MSYNC: u64 = 26;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const PAUSE: u64 = 34;
    pub const NANOSLEEP: u64 = 35;
    pub const GETITIMER: u64 = 36;
    pub const ALARM: u64 = 37;
    pub const SETITIMER: u64 = 38;
    pub const GETPID: u64 = 39;
    pub const SENDFILE: u64 = 40;
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const SENDTO: u64 = 44;
    pub const RECVFROM: u64 = 45;
    pub const SENDMSG: u64 = 46;
    pub const RECVMSG: u64 = 47;
    pub const SHUTDOWN: u64 = 48;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const GETPEERNAME: u64 = 52;
    pub const SOCKETPAIR: u64 = 53;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FLOCK: u64 = 73;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const TRUNCATE: u64 = 76;
    pub const FTRUNCATE: u64 = 77;
    pub const GETDENTS: u64 = 78;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const FCHDIR: u64 = 81;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const CREAT: u64 = 85;
    pub const LINK: u64 = 86;
    pub const UNLINK: u64 = 87;
    pub const SYMLINK: u64 = 88;
    pub const READLINK: u64 = 89;
    pub const CHMOD: u64 = 90;
    pub const FCHMOD: u64 = 91;
    pub const CHOWN: u64 = 92;
    pub const FCHOWN: u64 = 93;
    pub const LCHOWN: u64 = 94;
    pub const UMASK: u64 = 95;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const GETRLIMIT: u64 = 97;
    pub const GETRUSAGE: u64 = 98;
    pub const SYSINFO: u64 = 99;
    pub const TIMES: u64 = 100;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const SETUID: u64 = 105;
    pub const SETGID: u64 = 106;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const SETPGID: u64 = 109;
    pub const GETPPID: u64 = 110;
    pub const GETPGRP: u64 = 111;
    pub const SETSID: u64 = 112;
    pub const SETREUID: u64 = 113;
    pub const SETREGID: u64 = 114;
    pub const GETGROUPS: u64 = 115;
    pub const SETGROUPS: u64 = 116;
    pub const SETRESUID: u64 = 117;
    pub const GETRESUID: u64 = 118;
    pub const SETRESGID: u64 = 119;
    pub const GETRESGID: u64 = 120;
    pub const GETPGID: u64 = 121;
    pub const GETSID: u64 = 124;
    pub const RT_SIGPENDING: u64 = 127;
    pub const RT_SIGTIMEDWAIT: u64 = 128;
    pub const RT_SIGSUSPEND: u64 = 130;
    pub const SIGALTSTACK: u64 = 131;
    pub const STATFS: u64 = 137;
    pub const FSTATFS: u64 = 138;
    pub const GETPRIORITY: u64 = 140;
    pub const SETPRIORITY: u64 = 141;
    pub const PRCTL: u64 = 157;
    pub const ARCH_PRCTL: u64 = 158;
    pub const SETRLIMIT: u64 = 160;
    pub const SYNC: u64 = 162;
    pub const MOUNT: u64 = 165;
    pub const UMOUNT2: u64 = 166;
    pub const REBOOT: u64 = 169;
    pub const GETTID: u64 = 186;
    pub const TKILL: u64 = 200;
    pub const TIME: u64 = 201;
    pub const FUTEX: u64 = 202;
    pub const SCHED_GETAFFINITY: u64 = 204;
    pub const EPOLL_CREATE: u64 = 213;
    pub const GETDENTS64: u64 = 217;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const EPOLL_WAIT: u64 = 232;
    pub const EPOLL_CTL: u64 = 233;
    pub const TGKILL: u64 = 234;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const FCHOWNAT: u64 = 260;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const RENAMEAT: u64 = 264;
    pub const LINKAT: u64 = 265;
    pub const SYMLINKAT: u64 = 266;
    pub const READLINKAT: u64 = 267;
    pub const FCHMODAT: u64 = 268;
    pub const FACCESSAT: u64 = 269;
    pub const PSELECT6: u64 = 270;
    pub const PPOLL: u64 = 271;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const GET_ROBUST_LIST: u64 = 274;
    pub const UTIMENSAT: u64 = 280;
    pub const EPOLL_PWAIT: u64 = 281;
    pub const TIMERFD_CREATE: u64 = 283;
    pub const EVENTFD: u64 = 284;
    pub const FALLOCATE: u64 = 285;
    pub const TIMERFD_SETTIME: u64 = 286;
    pub const TIMERFD_GETTIME: u64 = 287;
    pub const ACCEPT4: u64 = 288;
    pub const EVENTFD2: u64 = 290;
    pub const EPOLL_CREATE1: u64 = 291;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const RECVMMSG: u64 = 299;
    pub const PRLIMIT64: u64 = 302;
    pub const SENDMMSG: u64 = 307;
    pub const GETCPU: u64 = 309;
    pub const RENAMEAT2: u64 = 316;
    pub const GETRANDOM: u64 = 318;
    pub const MEMFD_CREATE: u64 = 319;
    pub const MEMBARRIER: u64 = 324;
    pub const COPY_FILE_RANGE: u64 = 326;
    pub const STATX: u64 = 332;
    pub const RSEQ: u64 = 334;
    pub const CLONE3: u64 = 435;
    pub const CLOSE_RANGE: u64 = 436;
    pub const OPENAT2: u64 = 437;
    pub const FACCESSAT2: u64 = 439;
    pub const EPOLL_PWAIT2: u64 = 441;
}

/// Translate an x86-64 raw syscall into the canonical number, rewriting the
/// arguments of the legacy forms. `scratch` is a 64-byte per-thread area in
/// guest memory for the structures some conversions must materialise;
/// `blocked` is the thread's current signal mask (for `pause`).
pub fn translate(
    n: u64,
    a: &mut [u64; 6],
    m: &mut GuestMem<'_>,
    scratch: u64,
    blocked: u64,
) -> u64 {
    use raw as x;
    match n {
        x::READ => nr::READ,
        x::WRITE => nr::WRITE,
        x::OPEN => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::OPENAT
        }
        x::CLOSE => nr::CLOSE,
        x::STAT => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::NEWFSTATAT
        }
        x::FSTAT => nr::FSTAT,
        x::LSTAT => {
            *a = [AT_FDCWD, a[0], a[1], AT_SYMLINK_NOFOLLOW, 0, 0];
            nr::NEWFSTATAT
        }
        x::POLL => {
            // poll(fds, n, timeout_ms) -> ppoll(fds, n, &ts, NULL)
            let ms = a[2] as i32;
            let ts = if ms < 0 {
                0
            } else {
                let secs = (ms / 1000) as u64;
                let nanos = ((ms % 1000) as u64) * 1_000_000;
                let _ = m.write_u64(scratch, secs);
                let _ = m.write_u64(scratch + 8, nanos);
                scratch
            };
            *a = [a[0], a[1], ts, 0, 8, 0];
            nr::PPOLL
        }
        x::LSEEK => nr::LSEEK,
        x::MMAP => nr::MMAP,
        x::MPROTECT => nr::MPROTECT,
        x::MUNMAP => nr::MUNMAP,
        x::BRK => nr::BRK,
        x::RT_SIGACTION => nr::RT_SIGACTION,
        x::RT_SIGPROCMASK => nr::RT_SIGPROCMASK,
        x::RT_SIGRETURN => nr::RT_SIGRETURN,
        x::IOCTL => nr::IOCTL,
        x::PREAD64 => nr::PREAD64,
        x::PWRITE64 => nr::PWRITE64,
        x::READV => nr::READV,
        x::WRITEV => nr::WRITEV,
        x::ACCESS => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::FACCESSAT
        }
        x::PIPE => {
            *a = [a[0], 0, 0, 0, 0, 0];
            nr::PIPE2
        }
        x::SELECT => {
            // select(n, r, w, e, &timeval) -> pselect6(n, r, w, e, &ts, NULL).
            let tv = a[4];
            let ts = if tv == 0 {
                0
            } else {
                let secs = m.read_u64(tv).unwrap_or(0);
                let usecs = m.read_u64(tv + 8).unwrap_or(0);
                let _ = m.write_u64(scratch, secs);
                let _ = m.write_u64(scratch + 8, usecs.wrapping_mul(1000));
                scratch
            };
            *a = [a[0], a[1], a[2], a[3], ts, 0];
            nr::PSELECT6
        }
        x::SCHED_YIELD => nr::SCHED_YIELD,
        x::MREMAP => nr::MREMAP,
        x::MSYNC => nr::MSYNC,
        x::MADVISE => nr::MADVISE,
        x::DUP => nr::DUP,
        x::DUP2 => nr::DUP2,
        x::PAUSE => {
            // pause() is rt_sigsuspend with the current mask.
            let _ = m.write_u64(scratch, blocked);
            *a = [scratch, 8, 0, 0, 0, 0];
            nr::RT_SIGSUSPEND
        }
        x::NANOSLEEP => nr::NANOSLEEP,
        x::GETITIMER => nr::GETITIMER,
        x::ALARM => nr::ALARM,
        x::SETITIMER => nr::SETITIMER,
        x::GETPID => nr::GETPID,
        x::SENDFILE => nr::SENDFILE,
        x::SOCKET => nr::SOCKET,
        x::CONNECT => nr::CONNECT,
        x::ACCEPT => nr::ACCEPT,
        x::SENDTO => nr::SENDTO,
        x::RECVFROM => nr::RECVFROM,
        x::SENDMSG => nr::SENDMSG,
        x::RECVMSG => nr::RECVMSG,
        x::SHUTDOWN => nr::SHUTDOWN,
        x::BIND => nr::BIND,
        x::LISTEN => nr::LISTEN,
        x::GETSOCKNAME => nr::GETSOCKNAME,
        x::GETPEERNAME => nr::GETPEERNAME,
        x::SOCKETPAIR => nr::SOCKETPAIR,
        x::SETSOCKOPT => nr::SETSOCKOPT,
        x::GETSOCKOPT => nr::GETSOCKOPT,
        x::CLONE => {
            // x86-64 argument order: flags, stack, parent_tid, child_tid, tls.
            // The canonical (aarch64) order: flags, stack, parent_tid, tls,
            // child_tid.
            *a = [a[0], a[1], a[2], a[4], a[3], 0];
            nr::CLONE
        }
        x::FORK | x::VFORK => {
            *a = [SIGCHLD, 0, 0, 0, 0, 0];
            nr::CLONE
        }
        x::EXECVE => nr::EXECVE,
        x::EXIT => nr::EXIT,
        x::WAIT4 => nr::WAIT4,
        x::KILL => nr::KILL,
        x::UNAME => nr::UNAME,
        x::FCNTL => nr::FCNTL,
        x::FLOCK => nr::FLOCK,
        x::FSYNC => nr::FSYNC,
        x::FDATASYNC => nr::FDATASYNC,
        x::FTRUNCATE => nr::FTRUNCATE,
        x::GETCWD => nr::GETCWD,
        x::CHDIR => nr::CHDIR,
        x::FCHDIR => nr::FCHDIR,
        x::RENAME => {
            *a = [AT_FDCWD, a[0], AT_FDCWD, a[1], 0, 0];
            nr::RENAMEAT
        }
        x::MKDIR => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::MKDIRAT
        }
        x::RMDIR => {
            *a = [AT_FDCWD, a[0], AT_REMOVEDIR, 0, 0, 0];
            nr::UNLINKAT
        }
        x::CREAT => {
            // O_CREAT | O_WRONLY | O_TRUNC
            *a = [AT_FDCWD, a[0], 0x241, a[1], 0, 0];
            nr::OPENAT
        }
        x::LINK => {
            *a = [AT_FDCWD, a[0], AT_FDCWD, a[1], 0, 0];
            nr::LINKAT
        }
        x::UNLINK => {
            *a = [AT_FDCWD, a[0], 0, 0, 0, 0];
            nr::UNLINKAT
        }
        x::SYMLINK => {
            *a = [a[0], AT_FDCWD, a[1], 0, 0, 0];
            nr::SYMLINKAT
        }
        x::READLINK => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::READLINKAT
        }
        x::CHMOD => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::FCHMODAT
        }
        x::FCHMOD => nr::FCHMOD,
        x::CHOWN => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::FCHOWNAT
        }
        x::FCHOWN => nr::FCHOWN,
        x::LCHOWN => {
            *a = [AT_FDCWD, a[0], a[1], a[2], AT_SYMLINK_NOFOLLOW, 0];
            nr::FCHOWNAT
        }
        x::UMASK => nr::UMASK,
        x::GETTIMEOFDAY => nr::GETTIMEOFDAY,
        x::GETRLIMIT => {
            *a = [0, a[0], 0, a[1], 0, 0];
            nr::PRLIMIT64
        }
        x::SETRLIMIT => {
            *a = [0, a[0], a[1], 0, 0, 0];
            nr::PRLIMIT64
        }
        x::GETRUSAGE => nr::GETRUSAGE,
        x::SYSINFO => nr::SYSINFO,
        x::TIMES => nr::TIMES,
        x::GETUID => nr::GETUID,
        x::GETGID => nr::GETGID,
        x::SETUID => nr::SETUID,
        x::SETGID => nr::SETGID,
        x::GETEUID => nr::GETEUID,
        x::GETEGID => nr::GETEGID,
        x::SETPGID => nr::SETPGID,
        x::GETPPID => nr::GETPPID,
        x::GETPGRP => {
            *a = [0, 0, 0, 0, 0, 0];
            nr::GETPGID
        }
        x::SETSID => nr::SETSID,
        x::SETREUID => nr::SETREUID,
        x::SETREGID => nr::SETREGID,
        x::GETGROUPS => nr::GETGROUPS,
        x::SETGROUPS => nr::SETGROUPS,
        x::SETRESUID => nr::SETRESUID,
        x::GETRESUID => nr::GETRESUID,
        x::SETRESGID => nr::SETRESGID,
        x::GETRESGID => nr::GETRESGID,
        x::GETPGID => nr::GETPGID,
        x::GETSID => nr::GETSID,
        x::RT_SIGPENDING => nr::RT_SIGPENDING,
        x::RT_SIGTIMEDWAIT => nr::RT_SIGTIMEDWAIT,
        x::RT_SIGSUSPEND => nr::RT_SIGSUSPEND,
        x::SIGALTSTACK => nr::SIGALTSTACK,
        x::STATFS => nr::STATFS,
        x::FSTATFS => nr::FSTATFS,
        x::GETPRIORITY => nr::GETPRIORITY,
        x::SETPRIORITY => nr::SETPRIORITY,
        x::PRCTL => nr::PRCTL,
        x::ARCH_PRCTL => nr::ARCH_PRCTL,
        x::SYNC => nr::SYNC,
        x::MOUNT => nr::MOUNT,
        x::UMOUNT2 => nr::UMOUNT2,
        x::REBOOT => nr::REBOOT,
        x::GETTID => nr::GETTID,
        x::TKILL => nr::TKILL,
        x::TIME => nr::TIME,
        x::FUTEX => nr::FUTEX,
        x::SCHED_GETAFFINITY => nr::SCHED_GETAFFINITY,
        x::EPOLL_CREATE => {
            *a = [0, 0, 0, 0, 0, 0];
            nr::EPOLL_CREATE1
        }
        x::GETDENTS64 => nr::GETDENTS64,
        x::SET_TID_ADDRESS => nr::SET_TID_ADDRESS,
        x::CLOCK_GETTIME => nr::CLOCK_GETTIME,
        x::CLOCK_GETRES => nr::CLOCK_GETRES,
        x::CLOCK_NANOSLEEP => nr::CLOCK_NANOSLEEP,
        x::EXIT_GROUP => nr::EXIT_GROUP,
        x::EPOLL_WAIT => {
            *a = [a[0], a[1], a[2], a[3], 0, 8];
            nr::EPOLL_PWAIT
        }
        x::EPOLL_CTL => nr::EPOLL_CTL,
        x::TGKILL => nr::TGKILL,
        x::OPENAT => nr::OPENAT,
        x::MKDIRAT => nr::MKDIRAT,
        x::FCHOWNAT => nr::FCHOWNAT,
        x::NEWFSTATAT => nr::NEWFSTATAT,
        x::UNLINKAT => nr::UNLINKAT,
        x::RENAMEAT => nr::RENAMEAT,
        x::LINKAT => nr::LINKAT,
        x::SYMLINKAT => nr::SYMLINKAT,
        x::READLINKAT => nr::READLINKAT,
        x::FCHMODAT => nr::FCHMODAT,
        x::FACCESSAT => nr::FACCESSAT,
        x::PSELECT6 => nr::PSELECT6,
        x::PPOLL => nr::PPOLL,
        x::SET_ROBUST_LIST => nr::SET_ROBUST_LIST,
        x::GET_ROBUST_LIST => nr::GET_ROBUST_LIST,
        x::UTIMENSAT => nr::UTIMENSAT,
        x::EPOLL_PWAIT => nr::EPOLL_PWAIT,
        x::TIMERFD_CREATE => nr::TIMERFD_CREATE,
        x::EVENTFD => {
            *a = [a[0], 0, 0, 0, 0, 0];
            nr::EVENTFD2
        }
        x::FALLOCATE => nr::FALLOCATE,
        x::TIMERFD_SETTIME => nr::TIMERFD_SETTIME,
        x::TIMERFD_GETTIME => nr::TIMERFD_GETTIME,
        x::ACCEPT4 => nr::ACCEPT4,
        x::EVENTFD2 => nr::EVENTFD2,
        x::EPOLL_CREATE1 => nr::EPOLL_CREATE1,
        x::DUP3 => nr::DUP3,
        x::PIPE2 => nr::PIPE2,
        x::RECVMMSG => nr::RECVMMSG,
        x::PRLIMIT64 => nr::PRLIMIT64,
        x::SENDMMSG => nr::SENDMMSG,
        x::GETCPU => nr::GETCPU,
        x::RENAMEAT2 => nr::RENAMEAT2,
        x::GETRANDOM => nr::GETRANDOM,
        x::MEMFD_CREATE => nr::MEMFD_CREATE,
        x::MEMBARRIER => nr::MEMBARRIER,
        x::COPY_FILE_RANGE => nr::COPY_FILE_RANGE,
        x::STATX => nr::STATX,
        x::RSEQ => nr::RSEQ,
        x::CLONE3 => nr::CLONE3,
        x::CLOSE_RANGE => nr::CLOSE_RANGE,
        x::OPENAT2 => nr::OPENAT2,
        x::FACCESSAT2 => nr::FACCESSAT2,
        x::EPOLL_PWAIT2 => nr::EPOLL_PWAIT2,
        // Advisory: fadvise64, readahead, sync_file_range, mlock, munlock,
        // mlockall, munlockall, mincore-free, sched_setaffinity.
        221 | 187 | 277 | 149 | 150 | 151 | 152 | 203 => nr::NOOP_OK,
        _ => UNKNOWN,
    }
}

/// The canonical number for a raw x86-64 syscall number alone, for a host
/// that wants to classify a call before the kernel sees it. The legacy
/// forms' argument rewriting is not performed (it needs the real arguments).
pub fn canonical_number(raw: u64) -> u64 {
    let mut a = [0u64; 6];
    let mut dummy = [0u8; 128];
    let mut m = GuestMem::new(&mut dummy);
    translate(raw, &mut a, &mut m, 0, 0)
}

/// The processor gex claims to be: an SSSE3-era Intel core (family 6, model
/// 15) with the baseline x86-64 feature set plus SSE3, SSSE3, CMPXCHG16B and
/// POPCNT -- exactly the instruction set the lifter implements, so glibc's
/// IFUNC resolvers pick code gex can run. No AVX, no SSE4, no XSAVE.
pub fn cpuid(leaf: u32, sub: u32) -> [u32; 4] {
    match leaf {
        0 => [7, 0x756e_6547, 0x6c65_746e, 0x4965_6e69], // "GenuineIntel"
        1 => {
            let eax = 0x0000_06fb; // family 6, model 15, stepping 11
            let ebx = 0x0001_0800; // 1 logical processor, 64-byte clflush line
            let ecx = 1 | (1 << 9) | (1 << 13) | (1 << 23); // SSE3 SSSE3 CX16 POPCNT
            let edx = (1 << 0)
                | (1 << 4)
                | (1 << 8)
                | (1 << 15)
                | (1 << 19)
                | (1 << 23)
                | (1 << 24)
                | (1 << 25)
                | (1 << 26); // FPU TSC CX8 CMOV CLFSH MMX FXSR SSE SSE2
            [eax, ebx, ecx, edx]
        }
        // Deterministic cache parameters: a 32 KiB 8-way L1D and a 4 MiB
        // 16-way L2, both with 64-byte lines. glibc sizes its memcpy
        // thresholds from these.
        4 => match sub {
            0 => [0x121, 0x01c0_003f, 63, 0],
            1 => [0x143, 0x03c0_003f, 4095, 0],
            _ => [0, 0, 0, 0],
        },
        0x8000_0000 => [0x8000_0008, 0, 0, 0],
        0x8000_0001 => [0, 0, 1, (1 << 11) | (1 << 20) | (1 << 27) | (1 << 29)], // LAHF; SYSCALL NX RDTSCP LM
        0x8000_0002 => str_regs(b"gex virtual x86-"),
        0x8000_0003 => str_regs(b"64 processor    "),
        0x8000_0004 => str_regs(b"                "),
        0x8000_0008 => [0x3030, 0, 0, 0],
        _ => [0, 0, 0, 0],
    }
}

fn str_regs(s: &[u8; 16]) -> [u32; 4] {
    let w = |i: usize| u32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
    [w(0), w(4), w(8), w(12)]
}

/// Serve a `cpuid` the translated code asked for through `sysreg_write`:
/// `value` is `eax | ecx << 32`; the four results go into the context.
pub fn serve_cpuid(m: &mut GuestMem<'_>, ctx_addr: u64, value: u64) {
    use gex_abi::ctx;
    let r = cpuid(value as u32, (value >> 32) as u32);
    // eax, ebx, ecx, edx are GPRs 0, 3, 1, 2; a 32-bit write zero-extends.
    for (reg, v) in [(0u8, r[0]), (3, r[1]), (1, r[2]), (2, r[3])] {
        let _ = m.write_u64(ctx_addr + ctx::x(reg) as u64, v as u64);
    }
}

/// The x87 register stack as the lifter keeps it in the context: eight
/// doubles addressed physically, `TOP`, one valid bit per register, and the
/// status word's condition codes.
struct X87<'m, 'a> {
    m: &'m mut GuestMem<'a>,
    ctx: u64,
}

impl X87<'_, '_> {
    const C0: u16 = 1 << 8;
    const C1: u16 = 1 << 9;
    const C2: u16 = 1 << 10;
    const C3: u16 = 1 << 14;

    fn top(&self) -> u32 {
        self.m.read_u32(self.ctx + gex_abi::ctx::X87_TOP as u64).unwrap_or(0) & 7
    }
    fn slot(&self, i: u32) -> u64 {
        self.ctx + gex_abi::ctx::X87 as u64 + (((self.top() + i) & 7) as u64) * 8
    }
    fn st(&self, i: u32) -> f64 {
        f64::from_bits(self.m.read_u64(self.slot(i)).unwrap_or(0))
    }
    fn set_st(&mut self, i: u32, v: f64) {
        let a = self.slot(i);
        let _ = self.m.write_u64(a, v.to_bits());
    }
    fn tags(&self) -> u32 {
        self.m.read_u32(self.ctx + gex_abi::ctx::X87_TAGS as u64).unwrap_or(0)
    }
    fn set_tags(&mut self, t: u32) {
        let _ = self.m.write_u32(self.ctx + gex_abi::ctx::X87_TAGS as u64, t & 0xff);
    }
    fn set_top(&mut self, t: u32) {
        let _ = self.m.write_u32(self.ctx + gex_abi::ctx::X87_TOP as u64, t & 7);
    }
    fn push(&mut self, v: f64) {
        let t = (self.top() + 7) & 7;
        self.set_top(t);
        self.set_st(0, v);
        let tags = self.tags() | 1 << t;
        self.set_tags(tags);
    }
    fn pop(&mut self) {
        let t = self.top();
        let tags = self.tags() & !(1 << t);
        self.set_tags(tags);
        self.set_top(t + 1);
    }
    /// Replace the condition codes (C0..C3) with `cc`.
    fn set_cc(&mut self, cc: u16) {
        let a = self.ctx + gex_abi::ctx::X87_SW as u64;
        let sw = self.m.read(a, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
        let sw = (sw & !(Self::C0 | Self::C1 | Self::C2 | Self::C3)) | cc;
        let _ = self.m.write(a, &sw.to_le_bytes());
    }
    /// The condition codes `fprem`/`fprem1` report: C2 clear (the reduction
    /// is complete; doubles never need the partial steps), and the quotient's
    /// three low bits in C0 (bit 2), C3 (bit 1) and C1 (bit 0).
    fn quotient_cc(q: u64) -> u16 {
        let mut cc = 0;
        if q & 4 != 0 {
            cc |= Self::C0;
        }
        if q & 2 != 0 {
            cc |= Self::C3;
        }
        if q & 1 != 0 {
            cc |= Self::C1;
        }
        cc
    }
}

/// `x` and the magnitude of `x / y`'s quotient under `fmod`'s truncation, or
/// under IEEE round-to-nearest-even for `nearest` (`fprem1`, C's
/// `remainder`). The quotient is exact while it fits a double's mantissa,
/// which is all its low three bits are ever asked for.
fn x87_remainder(x: f64, y: f64, nearest: bool) -> (f64, u64) {
    let r = x % y;
    if !r.is_finite() || y == 0.0 || !x.is_finite() {
        return (f64::NAN, 0);
    }
    let q = ((x - r) / y).abs();
    let q = if q < 9.007_199_254_740_992e15 { q as u64 } else { 0 };
    if !nearest {
        return (r, q);
    }
    let (ar, half) = (r.abs(), y.abs() / 2.0);
    if ar > half || (ar == half && q & 1 == 1) {
        // Exact: |r| lies within [|y|/2, |y|] (Sterbenz).
        (r - y.abs().copysign(r), q + 1)
    } else {
        (r, q)
    }
}

/// Serve an x87 instruction the translated code handed over through
/// `sysreg_write` (see [`x87op`]). Computed in double precision, like the
/// rest of gex's x87; the operand-range rule of `fsin`/`fcos`/`fptan`
/// (|x| < 2^63, else C2 is set and the operand is left alone) is kept.
pub fn serve_x87(m: &mut GuestMem<'_>, ctx_addr: u64, op: u64) {
    let mut x = X87 { m, ctx: ctx_addr };
    let in_range = |v: f64| v.abs() < 9.223_372_036_854_775_808e18;
    match op {
        x87op::F2XM1 => {
            let v = x.st(0);
            x.set_st(0, (v * std::f64::consts::LN_2).exp_m1());
            x.set_cc(0);
        }
        x87op::FYL2X => {
            let (a, b) = (x.st(0), x.st(1));
            x.set_st(1, b * a.log2());
            x.pop();
            x.set_cc(0);
        }
        x87op::FYL2XP1 => {
            let (a, b) = (x.st(0), x.st(1));
            x.set_st(1, b * (a.ln_1p() / std::f64::consts::LN_2));
            x.pop();
            x.set_cc(0);
        }
        x87op::FPATAN => {
            let (a, b) = (x.st(0), x.st(1));
            x.set_st(1, b.atan2(a));
            x.pop();
            x.set_cc(0);
        }
        x87op::FPTAN | x87op::FSIN | x87op::FCOS | x87op::FSINCOS => {
            let v = x.st(0);
            if !in_range(v) && v.is_finite() {
                x.set_cc(X87::C2);
                return;
            }
            match op {
                x87op::FPTAN => {
                    x.set_st(0, v.tan());
                    x.push(1.0);
                }
                x87op::FSIN => x.set_st(0, v.sin()),
                x87op::FCOS => x.set_st(0, v.cos()),
                _ => {
                    x.set_st(0, v.sin());
                    x.push(v.cos());
                }
            }
            x.set_cc(0);
        }
        x87op::FXTRACT => {
            // st0 becomes the unbiased exponent, and the significand, in
            // [1, 2) with st0's sign, is pushed on top of it.
            let v = x.st(0);
            let (exp, sig) = if v == 0.0 {
                (f64::NEG_INFINITY, v)
            } else if v.is_nan() {
                (v, v)
            } else if v.is_infinite() {
                (f64::INFINITY, v)
            } else {
                // Normalise a subnormal first, so the exponent field is real.
                let (v2, adj) = if v.abs() < f64::MIN_POSITIVE { (v * 2f64.powi(64), 64) } else { (v, 0) };
                let bits = v2.to_bits();
                let e = ((bits >> 52) & 0x7ff) as i64 - 1023 - adj;
                let s = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1023u64 << 52));
                (e as f64, s)
            };
            x.set_st(0, exp);
            x.push(sig);
            x.set_cc(0);
        }
        x87op::FPREM | x87op::FPREM1 => {
            let (a, b) = (x.st(0), x.st(1));
            let (r, q) = x87_remainder(a, b, op == x87op::FPREM1);
            x.set_st(0, r);
            x.set_cc(X87::quotient_cc(q));
        }
        // Only the lifter writes this register, with one of the codes above.
        _ => panic!("sysreg_write: unknown x87 operation {op}"),
    }
}

/// `/proc/cpuinfo` for the processor [`cpuid`] describes.
pub const CPUINFO: &str = concat!(
    "processor\t: 0\n",
    "vendor_id\t: GenuineIntel\n",
    "cpu family\t: 6\n",
    "model\t\t: 15\n",
    "model name\t: gex virtual x86-64 processor\n",
    "stepping\t: 11\n",
    "cpu MHz\t\t: 1000.000\n",
    "cache size\t: 4096 KB\n",
    "physical id\t: 0\n",
    "siblings\t: 1\n",
    "core id\t\t: 0\n",
    "cpu cores\t: 1\n",
    "fpu\t\t: yes\n",
    "fpu_exception\t: yes\n",
    "cpuid level\t: 7\n",
    "wp\t\t: yes\n",
    "flags\t\t: fpu tsc cx8 cmov clflush mmx fxsr sse sse2 syscall nx rdtscp lm pni ssse3 cx16 popcnt lahf_lm\n",
    "bogomips\t: 2000.00\n",
    "clflush size\t: 64\n",
    "cache_alignment\t: 64\n",
    "address sizes\t: 48 bits physical, 48 bits virtual\n\n",
);

#[cfg(test)]
mod tests {
    use super::*;
    use gex_abi::ctx;

    const CTX: u64 = 0x1000;

    /// A context whose x87 stack holds `vals`, `vals[0]` in st0.
    fn stack(buf: &mut [u8], vals: &[f64]) {
        let mut m = GuestMem::new(buf);
        let top = (8 - vals.len() as u32) & 7;
        m.write_u32(CTX + ctx::X87_TOP as u64, top).unwrap();
        let mut tags = 0u32;
        for (i, v) in vals.iter().enumerate() {
            let phys = (top + i as u32) & 7;
            m.write_u64(CTX + ctx::X87 as u64 + phys as u64 * 8, v.to_bits()).unwrap();
            tags |= 1 << phys;
        }
        m.write_u32(CTX + ctx::X87_TAGS as u64, tags).unwrap();
    }

    fn run(vals: &[f64], op: u64) -> (Vec<f64>, u16) {
        let mut buf = vec![0u8; 0x4000];
        stack(&mut buf, vals);
        let mut m = GuestMem::new(&mut buf);
        serve_x87(&mut m, CTX, op);
        let top = m.read_u32(CTX + ctx::X87_TOP as u64).unwrap() & 7;
        let tags = m.read_u32(CTX + ctx::X87_TAGS as u64).unwrap();
        let mut out = Vec::new();
        for i in 0..8 {
            let phys = (top + i) & 7;
            if tags & (1 << phys) == 0 {
                break;
            }
            out.push(f64::from_bits(m.read_u64(CTX + ctx::X87 as u64 + phys as u64 * 8).unwrap()));
        }
        let sw = u16::from_le_bytes(m.read(CTX + ctx::X87_SW as u64, 2).unwrap().try_into().unwrap());
        (out, sw)
    }

    #[test]
    fn x87_transcendentals_push_and_pop_like_the_hardware() {
        // fpatan: st1 = atan2(st1, st0), pop.
        assert_eq!(run(&[1.0, 1.0], x87op::FPATAN).0, vec![1f64.atan2(1.0)]);
        // fyl2x: st1 = st1 * log2(st0), pop.
        assert_eq!(run(&[8.0, 3.0], x87op::FYL2X).0, vec![9.0]);
        // f2xm1 in place.
        assert_eq!(run(&[1.0], x87op::F2XM1).0, vec![1.0]);
        // fptan pushes 1.0; fsincos leaves cos on top of sin.
        assert_eq!(run(&[0.5], x87op::FPTAN).0, vec![1.0, 0.5f64.tan()]);
        assert_eq!(run(&[0.5], x87op::FSINCOS).0, vec![0.5f64.cos(), 0.5f64.sin()]);
        // Out of range: C2 set, operand untouched.
        let (v, sw) = run(&[1e19], x87op::FSIN);
        assert_eq!((v, sw & (1 << 10)), (vec![1e19], 1 << 10));
        // fxtract: significand on top of the exponent.
        assert_eq!(run(&[-12.0], x87op::FXTRACT).0, vec![-1.5, 3.0]);
        assert_eq!(run(&[0.0], x87op::FXTRACT).0, vec![0.0, f64::NEG_INFINITY]);
        assert_eq!(run(&[f64::MIN_POSITIVE / 4.0], x87op::FXTRACT).0, vec![1.0, -1024.0]);
    }

    #[test]
    fn x87_remainders_report_the_quotient_bits() {
        // 7 / 2: fprem truncates (q = 3, r = 1); fprem1 rounds to even
        // (q = 4, r = -1). C0 = q bit 2, C3 = bit 1, C1 = bit 0; C2 clear.
        let (c0, c1, c3) = (1u16 << 8, 1u16 << 9, 1u16 << 14);
        let (v, sw) = run(&[7.0, 2.0], x87op::FPREM);
        assert_eq!(v, vec![1.0, 2.0]);
        assert_eq!(sw & (c0 | c1 | c3 | 1 << 10), c3 | c1);
        let (v, sw) = run(&[7.0, 2.0], x87op::FPREM1);
        assert_eq!(v, vec![-1.0, 2.0]);
        assert_eq!(sw & (c0 | c1 | c3 | 1 << 10), c0);
        // Signs follow the dividend.
        assert_eq!(run(&[-7.5, 2.0], x87op::FPREM).0, vec![-1.5, 2.0]);
        assert!(run(&[1.0, 0.0], x87op::FPREM).0[0].is_nan());
    }
}
