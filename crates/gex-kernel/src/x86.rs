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
