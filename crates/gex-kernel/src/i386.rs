//! The i386 guest ABI: what a 32-bit program running in compatibility mode
//! inside the x86-64 guest asks for, translated to the canonical numbering
//! the kernel is written against.
//!
//! This is the kernel's `compat` layer, in miniature. A 32-bit program enters
//! with `int 0x80`, its number in `eax` and its arguments in `ebx, ecx, edx,
//! esi, edi, ebp`; its numbering is the i386 one (`asm/unistd_32.h`); its
//! structures have 32-bit pointers and, for the calls that predate the
//! time64 work, 32-bit times. Everything here rewrites a call into the form
//! the rest of the kernel already handles, the way [`crate::x86`] does for
//! x86-64, and [`Post`] carries the little that must happen after the call
//! returns to hand the result back in the 32-bit layout.
//!
//! What is deliberately not here: the structures the kernel writes back
//! whose layout differs (stat64, the sigaction, the stack for sigaltstack,
//! the iovecs of readv/writev, the argv of execve). Those are converted at
//! the call itself, under the kernel's `compat` flag, because converting
//! them here would need more scratch than a thread has.

use crate::syscalls::nr;
use crate::GuestMem;

/// A raw number with no canonical equivalent. Dispatch answers `ENOSYS`.
pub const UNKNOWN: u64 = u64::MAX;

const AT_FDCWD: u64 = (-100i64) as u64;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const SIGCHLD: u64 = 17;
const O_CREAT: u64 = 0o100;
const O_WRONLY: u64 = 0o1;
const O_TRUNC: u64 = 0o1000;
const CLONE_SETTLS: u64 = 0x80000;

/// i386 raw syscall numbers (`asm/unistd_32.h`).
#[allow(dead_code)]
pub mod raw {
    pub const EXIT: u64 = 1;
    pub const FORK: u64 = 2;
    pub const READ: u64 = 3;
    pub const WRITE: u64 = 4;
    pub const OPEN: u64 = 5;
    pub const CLOSE: u64 = 6;
    pub const WAITPID: u64 = 7;
    pub const CREAT: u64 = 8;
    pub const LINK: u64 = 9;
    pub const UNLINK: u64 = 10;
    pub const EXECVE: u64 = 11;
    pub const CHDIR: u64 = 12;
    pub const TIME: u64 = 13;
    pub const CHMOD: u64 = 15;
    pub const LCHOWN16: u64 = 16;
    pub const LSEEK: u64 = 19;
    pub const GETPID: u64 = 20;
    pub const MOUNT: u64 = 21;
    pub const SETUID16: u64 = 23;
    pub const GETUID16: u64 = 24;
    pub const ALARM: u64 = 27;
    pub const ACCESS: u64 = 33;
    pub const SYNC: u64 = 36;
    pub const KILL: u64 = 37;
    pub const RENAME: u64 = 38;
    pub const MKDIR: u64 = 39;
    pub const RMDIR: u64 = 40;
    pub const DUP: u64 = 41;
    pub const PIPE: u64 = 42;
    pub const TIMES: u64 = 43;
    pub const BRK: u64 = 45;
    pub const SETGID16: u64 = 46;
    pub const GETGID16: u64 = 47;
    pub const GETEUID16: u64 = 49;
    pub const GETEGID16: u64 = 50;
    pub const UMOUNT2: u64 = 52;
    pub const IOCTL: u64 = 54;
    pub const FCNTL: u64 = 55;
    pub const SETPGID: u64 = 57;
    pub const UMASK: u64 = 60;
    pub const DUP2: u64 = 63;
    pub const GETPPID: u64 = 64;
    pub const GETPGRP: u64 = 65;
    pub const SETSID: u64 = 66;
    pub const SETREUID16: u64 = 70;
    pub const SETREGID16: u64 = 71;
    pub const GETRLIMIT: u64 = 76;
    pub const GETRUSAGE: u64 = 77;
    pub const GETTIMEOFDAY: u64 = 78;
    pub const GETGROUPS16: u64 = 80;
    pub const SETGROUPS16: u64 = 81;
    pub const SYMLINK: u64 = 83;
    pub const READLINK: u64 = 85;
    pub const REBOOT: u64 = 88;
    pub const MMAP: u64 = 90;
    pub const MUNMAP: u64 = 91;
    pub const TRUNCATE: u64 = 92;
    pub const FTRUNCATE: u64 = 93;
    pub const FCHMOD: u64 = 94;
    pub const FCHOWN16: u64 = 95;
    pub const GETPRIORITY: u64 = 96;
    pub const SETPRIORITY: u64 = 97;
    pub const STATFS: u64 = 99;
    pub const FSTATFS: u64 = 100;
    pub const SOCKETCALL: u64 = 102;
    pub const SETITIMER: u64 = 104;
    pub const GETITIMER: u64 = 105;
    pub const STAT: u64 = 106;
    pub const LSTAT: u64 = 107;
    pub const FSTAT: u64 = 108;
    pub const WAIT4: u64 = 114;
    pub const SYSINFO: u64 = 116;
    pub const FSYNC: u64 = 118;
    pub const CLONE: u64 = 120;
    pub const UNAME: u64 = 122;
    pub const MPROTECT: u64 = 125;
    pub const GETPGID: u64 = 132;
    pub const FCHDIR: u64 = 133;
    pub const PERSONALITY: u64 = 136;
    pub const LLSEEK: u64 = 140;
    pub const GETDENTS: u64 = 141;
    pub const NEWSELECT: u64 = 142;
    pub const FLOCK: u64 = 143;
    pub const MSYNC: u64 = 144;
    pub const READV: u64 = 145;
    pub const WRITEV: u64 = 146;
    pub const GETSID: u64 = 147;
    pub const FDATASYNC: u64 = 148;
    pub const SCHED_YIELD: u64 = 158;
    pub const NANOSLEEP: u64 = 162;
    pub const MREMAP: u64 = 163;
    pub const POLL: u64 = 168;
    pub const PRCTL: u64 = 172;
    pub const SIGRETURN: u64 = 119;
    pub const RT_SIGRETURN: u64 = 173;
    pub const RT_SIGACTION: u64 = 174;
    pub const RT_SIGPROCMASK: u64 = 175;
    pub const RT_SIGPENDING: u64 = 176;
    pub const RT_SIGTIMEDWAIT: u64 = 177;
    pub const RT_SIGSUSPEND: u64 = 179;
    pub const PREAD64: u64 = 180;
    pub const PWRITE64: u64 = 181;
    pub const GETCWD: u64 = 183;
    pub const SIGALTSTACK: u64 = 186;
    pub const SENDFILE: u64 = 187;
    pub const VFORK: u64 = 190;
    pub const UGETRLIMIT: u64 = 191;
    pub const MMAP2: u64 = 192;
    pub const TRUNCATE64: u64 = 193;
    pub const FTRUNCATE64: u64 = 194;
    pub const STAT64: u64 = 195;
    pub const LSTAT64: u64 = 196;
    pub const FSTAT64: u64 = 197;
    pub const LCHOWN: u64 = 198;
    pub const GETUID: u64 = 199;
    pub const GETGID: u64 = 200;
    pub const GETEUID: u64 = 201;
    pub const GETEGID: u64 = 202;
    pub const SETREUID: u64 = 203;
    pub const SETREGID: u64 = 204;
    pub const GETGROUPS: u64 = 205;
    pub const SETGROUPS: u64 = 206;
    pub const FCHOWN: u64 = 207;
    pub const SETRESUID: u64 = 208;
    pub const GETRESUID: u64 = 209;
    pub const SETRESGID: u64 = 210;
    pub const GETRESGID: u64 = 211;
    pub const CHOWN: u64 = 212;
    pub const SETUID: u64 = 213;
    pub const SETGID: u64 = 214;
    pub const MADVISE: u64 = 219;
    pub const GETDENTS64: u64 = 220;
    pub const FCNTL64: u64 = 221;
    pub const GETTID: u64 = 224;
    pub const TKILL: u64 = 238;
    pub const SENDFILE64: u64 = 239;
    pub const FUTEX: u64 = 240;
    pub const SCHED_GETAFFINITY: u64 = 242;
    pub const SET_THREAD_AREA: u64 = 243;
    pub const GET_THREAD_AREA: u64 = 244;
    pub const EXIT_GROUP: u64 = 252;
    pub const EPOLL_CTL: u64 = 255;
    pub const SET_TID_ADDRESS: u64 = 258;
    pub const CLOCK_GETTIME: u64 = 265;
    pub const CLOCK_GETRES: u64 = 266;
    pub const CLOCK_NANOSLEEP: u64 = 267;
    pub const STATFS64: u64 = 268;
    pub const FSTATFS64: u64 = 269;
    pub const TGKILL: u64 = 270;
    pub const OPENAT: u64 = 295;
    pub const MKDIRAT: u64 = 296;
    pub const FCHOWNAT: u64 = 298;
    pub const FSTATAT64: u64 = 300;
    pub const UNLINKAT: u64 = 301;
    pub const RENAMEAT: u64 = 302;
    pub const LINKAT: u64 = 303;
    pub const SYMLINKAT: u64 = 304;
    pub const READLINKAT: u64 = 305;
    pub const FCHMODAT: u64 = 306;
    pub const FACCESSAT: u64 = 307;
    pub const PSELECT6: u64 = 308;
    pub const PPOLL: u64 = 309;
    pub const SET_ROBUST_LIST: u64 = 311;
    pub const GET_ROBUST_LIST: u64 = 312;
    pub const GETCPU: u64 = 318;
    pub const EPOLL_PWAIT: u64 = 319;
    pub const UTIMENSAT: u64 = 320;
    pub const TIMERFD_CREATE: u64 = 322;
    pub const FALLOCATE: u64 = 324;
    pub const EVENTFD2: u64 = 328;
    pub const EPOLL_CREATE1: u64 = 329;
    pub const DUP3: u64 = 330;
    pub const PIPE2: u64 = 331;
    pub const RECVMMSG: u64 = 337;
    pub const PRLIMIT64: u64 = 340;
    pub const SENDMMSG: u64 = 345;
    pub const RENAMEAT2: u64 = 353;
    pub const GETRANDOM: u64 = 355;
    pub const MEMFD_CREATE: u64 = 356;
    pub const SOCKET: u64 = 359;
    pub const SOCKETPAIR: u64 = 360;
    pub const BIND: u64 = 361;
    pub const CONNECT: u64 = 362;
    pub const LISTEN: u64 = 363;
    pub const ACCEPT4: u64 = 364;
    pub const GETSOCKOPT: u64 = 365;
    pub const SETSOCKOPT: u64 = 366;
    pub const GETSOCKNAME: u64 = 367;
    pub const GETPEERNAME: u64 = 368;
    pub const SENDTO: u64 = 369;
    pub const SENDMSG: u64 = 370;
    pub const RECVFROM: u64 = 371;
    pub const RECVMSG: u64 = 372;
    pub const SHUTDOWN: u64 = 373;
    pub const MEMBARRIER: u64 = 375;
    pub const COPY_FILE_RANGE: u64 = 377;
    pub const STATX: u64 = 383;
    pub const ARCH_PRCTL: u64 = 384;
    pub const RSEQ: u64 = 386;
    pub const CLOCK_GETTIME64: u64 = 403;
    pub const CLOCK_GETRES_TIME64: u64 = 406;
    pub const CLOCK_NANOSLEEP_TIME64: u64 = 407;
    pub const TIMERFD_GETTIME64: u64 = 410;
    pub const TIMERFD_SETTIME64: u64 = 411;
    pub const UTIMENSAT_TIME64: u64 = 412;
    pub const PSELECT6_TIME64: u64 = 413;
    pub const PPOLL_TIME64: u64 = 414;
    pub const RECVMMSG_TIME64: u64 = 417;
    pub const RT_SIGTIMEDWAIT_TIME64: u64 = 421;
    pub const FUTEX_TIME64: u64 = 422;
    pub const CLONE3: u64 = 435;
    pub const CLOSE_RANGE: u64 = 436;
    pub const OPENAT2: u64 = 437;
    pub const FACCESSAT2: u64 = 439;
    pub const EPOLL_PWAIT2: u64 = 441;
}

/// What remains to be done once the canonical call has returned: the
/// 32-bit ABI reports a few results through memory in layouts of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Post {
    None,
    /// `_llseek`: the 64-bit result goes to `result` and the call returns 0.
    Llseek { result: u64 },
    /// A `struct timespec` with 32-bit fields: the canonical call wrote the
    /// 16-byte one at `scratch`; copy it to `user` as two 32-bit words.
    Timespec32 { user: u64, scratch: u64 },
    /// `gettimeofday`'s 32-bit timeval, likewise.
    Timeval32 { user: u64, scratch: u64 },
    /// `getrlimit`'s 32-bit rlimit: the canonical `prlimit64` wrote 16 bytes
    /// at `scratch`; each half saturates to the 32-bit `RLIM_INFINITY`.
    Rlimit32 { user: u64, scratch: u64 },
}

impl Post {
    /// Apply to a successful result `r`, giving the value the program sees.
    pub fn finish(self, m: &mut GuestMem<'_>, r: u64) -> u64 {
        let ok = (r as i64) >= 0;
        match self {
            Post::None => r,
            Post::Llseek { result } => {
                if ok {
                    let _ = m.write_u64(result, r);
                    0
                } else {
                    r
                }
            }
            Post::Rlimit32 { user, scratch } => {
                if ok && user != 0 {
                    for i in 0..2 {
                        let v = m.read_u64(scratch + i * 8).unwrap_or(0);
                        let v32 = if v > 0xffff_ffff { 0xffff_ffff } else { v as u32 };
                        let _ = m.write_u32(user + i * 4, v32);
                    }
                }
                r
            }
            Post::Timespec32 { user, scratch } | Post::Timeval32 { user, scratch } => {
                if ok && user != 0 {
                    let s = m.read_u64(scratch).unwrap_or(0);
                    let f = m.read_u64(scratch + 8).unwrap_or(0);
                    let _ = m.write_u32(user, s as u32);
                    let _ = m.write_u32(user + 4, f as u32);
                }
                r
            }
        }
    }
}

/// The 32-bit value in `v`, sign-extended: an `int` argument such as a file
/// descriptor or a pid arrives zero-extended from a 32-bit register and must
/// become a negative 64-bit one when it was negative (`-1`, `AT_FDCWD`).
#[inline]
fn sext(v: u64) -> u64 {
    v as u32 as i32 as i64 as u64
}

/// A 64-bit value passed as two 32-bit halves.
#[inline]
fn pair(lo: u64, hi: u64) -> u64 {
    (lo & 0xffff_ffff) | ((hi & 0xffff_ffff) << 32)
}

/// A 32-bit `struct timespec`/`timeval` at `p`, widened into the 16 bytes
/// at `scratch`, with the second field scaled by `scale` (1000 for a timeval).
fn widen_time(m: &mut GuestMem<'_>, p: u64, scratch: u64, scale: u64) -> u64 {
    if p == 0 {
        return 0;
    }
    let s = m.read_u32(p).unwrap_or(0) as i32 as i64 as u64;
    let f = m.read_u32(p + 4).unwrap_or(0) as i32 as i64 as u64;
    let _ = m.write_u64(scratch, s);
    let _ = m.write_u64(scratch + 8, f.wrapping_mul(scale));
    scratch
}

/// Translate an i386 call into the canonical one. `scratch` is the thread's
/// 64-byte scratch area in its context (the same one [`crate::x86`] uses);
/// `ctx_addr` is the context itself, for `set_thread_area`.
pub fn translate(
    n: u64,
    a: &mut [u64; 6],
    m: &mut GuestMem<'_>,
    scratch: u64,
    ctx_addr: u64,
) -> (u64, Post) {
    use raw as x;
    let mut post = Post::None;
    let n = match n {
        x::EXIT => nr::EXIT,
        x::EXIT_GROUP => nr::EXIT_GROUP,
        x::FORK | x::VFORK => {
            *a = [SIGCHLD, 0, 0, 0, 0, 0];
            nr::CLONE
        }
        x::READ | x::WRITE => {
            a[0] = sext(a[0]);
            if n == x::READ { nr::READ } else { nr::WRITE }
        }
        x::OPEN => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::OPENAT
        }
        x::CREAT => {
            *a = [AT_FDCWD, a[0], O_CREAT | O_WRONLY | O_TRUNC, a[1], 0, 0];
            nr::OPENAT
        }
        x::OPENAT => {
            a[0] = sext(a[0]);
            nr::OPENAT
        }
        x::OPENAT2 => {
            a[0] = sext(a[0]);
            nr::OPENAT2
        }
        x::CLOSE => {
            a[0] = sext(a[0]);
            nr::CLOSE
        }
        x::CLOSE_RANGE => nr::CLOSE_RANGE,
        x::WAITPID => {
            *a = [sext(a[0]), a[1], a[2], 0, 0, 0];
            nr::WAIT4
        }
        x::WAIT4 => {
            // The 32-bit rusage has a layout of its own; nothing here asks
            // for it, so it is not filled rather than filled wrong.
            *a = [sext(a[0]), a[1], a[2], 0, 0, 0];
            nr::WAIT4
        }
        x::LINK => {
            *a = [AT_FDCWD, a[0], AT_FDCWD, a[1], 0, 0];
            nr::LINKAT
        }
        x::UNLINK => {
            *a = [AT_FDCWD, a[0], 0, 0, 0, 0];
            nr::UNLINKAT
        }
        x::RMDIR => {
            *a = [AT_FDCWD, a[0], AT_REMOVEDIR, 0, 0, 0];
            nr::UNLINKAT
        }
        x::EXECVE => nr::EXECVE,
        x::CHDIR => nr::CHDIR,
        x::FCHDIR => {
            a[0] = sext(a[0]);
            nr::FCHDIR
        }
        x::TIME => nr::TIME,
        x::CHMOD => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::FCHMODAT
        }
        x::FCHMOD => {
            a[0] = sext(a[0]);
            nr::FCHMOD
        }
        x::FCHMODAT => {
            a[0] = sext(a[0]);
            nr::FCHMODAT
        }
        x::LCHOWN | x::LCHOWN16 => {
            *a = [AT_FDCWD, a[0], a[1], a[2], AT_SYMLINK_NOFOLLOW, 0];
            nr::FCHOWNAT
        }
        x::CHOWN => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::FCHOWNAT
        }
        x::FCHOWN | x::FCHOWN16 => {
            a[0] = sext(a[0]);
            nr::FCHOWN
        }
        x::FCHOWNAT => {
            a[0] = sext(a[0]);
            nr::FCHOWNAT
        }
        x::LSEEK => {
            *a = [sext(a[0]), sext(a[1]), a[2], 0, 0, 0];
            nr::LSEEK
        }
        x::LLSEEK => {
            // _llseek(fd, off_hi, off_lo, &result, whence)
            post = Post::Llseek { result: a[3] };
            *a = [sext(a[0]), pair(a[2], a[1]), a[4], 0, 0, 0];
            nr::LSEEK
        }
        x::GETPID => nr::GETPID,
        x::GETPPID => nr::GETPPID,
        x::GETTID => nr::GETTID,
        x::GETPGRP => {
            *a = [0; 6];
            nr::GETPGID
        }
        x::GETPGID => {
            a[0] = sext(a[0]);
            nr::GETPGID
        }
        x::SETPGID => {
            a[0] = sext(a[0]);
            a[1] = sext(a[1]);
            nr::SETPGID
        }
        x::SETSID => nr::SETSID,
        x::GETSID => {
            a[0] = sext(a[0]);
            nr::GETSID
        }
        x::GETUID | x::GETUID16 => nr::GETUID,
        x::GETEUID | x::GETEUID16 => nr::GETEUID,
        x::GETGID | x::GETGID16 => nr::GETGID,
        x::GETEGID | x::GETEGID16 => nr::GETEGID,
        x::SETUID | x::SETUID16 => nr::SETUID,
        x::SETGID | x::SETGID16 => nr::SETGID,
        x::SETREUID | x::SETREUID16 => nr::SETREUID,
        x::SETREGID | x::SETREGID16 => nr::SETREGID,
        x::SETRESUID => nr::SETRESUID,
        x::GETRESUID => nr::GETRESUID,
        x::SETRESGID => nr::SETRESGID,
        x::GETRESGID => nr::GETRESGID,
        x::GETGROUPS | x::GETGROUPS16 => nr::GETGROUPS,
        x::SETGROUPS | x::SETGROUPS16 => nr::SETGROUPS,
        x::MOUNT => nr::MOUNT,
        x::UMOUNT2 => nr::UMOUNT2,
        x::ALARM => nr::ALARM,
        x::ACCESS => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::FACCESSAT
        }
        x::FACCESSAT => {
            a[0] = sext(a[0]);
            nr::FACCESSAT
        }
        x::FACCESSAT2 => {
            a[0] = sext(a[0]);
            nr::FACCESSAT2
        }
        x::SYNC => nr::SYNC,
        x::FSYNC => {
            a[0] = sext(a[0]);
            nr::FSYNC
        }
        x::FDATASYNC => {
            a[0] = sext(a[0]);
            nr::FDATASYNC
        }
        x::KILL => {
            a[0] = sext(a[0]);
            nr::KILL
        }
        x::TKILL => {
            a[0] = sext(a[0]);
            nr::TKILL
        }
        x::TGKILL => {
            a[0] = sext(a[0]);
            a[1] = sext(a[1]);
            nr::TGKILL
        }
        x::RENAME => {
            *a = [AT_FDCWD, a[0], AT_FDCWD, a[1], 0, 0];
            nr::RENAMEAT
        }
        x::RENAMEAT => {
            a[0] = sext(a[0]);
            a[2] = sext(a[2]);
            nr::RENAMEAT
        }
        x::RENAMEAT2 => {
            a[0] = sext(a[0]);
            a[2] = sext(a[2]);
            nr::RENAMEAT2
        }
        x::MKDIR => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::MKDIRAT
        }
        x::MKDIRAT => {
            a[0] = sext(a[0]);
            nr::MKDIRAT
        }
        x::UNLINKAT => {
            a[0] = sext(a[0]);
            nr::UNLINKAT
        }
        x::LINKAT => {
            a[0] = sext(a[0]);
            a[2] = sext(a[2]);
            nr::LINKAT
        }
        x::SYMLINK => {
            *a = [a[0], AT_FDCWD, a[1], 0, 0, 0];
            nr::SYMLINKAT
        }
        x::SYMLINKAT => {
            a[1] = sext(a[1]);
            nr::SYMLINKAT
        }
        x::READLINK => {
            *a = [AT_FDCWD, a[0], a[1], a[2], 0, 0];
            nr::READLINKAT
        }
        x::READLINKAT => {
            a[0] = sext(a[0]);
            nr::READLINKAT
        }
        x::DUP => {
            a[0] = sext(a[0]);
            nr::DUP
        }
        x::DUP2 => {
            a[0] = sext(a[0]);
            a[1] = sext(a[1]);
            nr::DUP2
        }
        x::DUP3 => {
            a[0] = sext(a[0]);
            a[1] = sext(a[1]);
            nr::DUP3
        }
        x::PIPE => {
            *a = [a[0], 0, 0, 0, 0, 0];
            nr::PIPE2
        }
        x::PIPE2 => nr::PIPE2,
        x::BRK => nr::BRK,
        x::IOCTL => {
            a[0] = sext(a[0]);
            nr::IOCTL
        }
        x::FCNTL | x::FCNTL64 => {
            // F_GETLK64/F_SETLK64/F_SETLKW64 carry a flock64 the kernel does
            // not read in its 32-bit layout; they fall through as the lock
            // commands they are and fail loudly on the struct if anything
            // uses them.
            a[0] = sext(a[0]);
            nr::FCNTL
        }
        x::UMASK => nr::UMASK,
        x::GETCWD => nr::GETCWD,
        x::GETTIMEOFDAY => {
            // gettimeofday(&tv32, tz): let the kernel fill the 64-bit one in
            // scratch and narrow it afterwards.
            post = Post::Timeval32 { user: a[0], scratch };
            a[0] = if a[0] != 0 { scratch } else { 0 };
            nr::GETTIMEOFDAY
        }
        x::CLOCK_GETTIME | x::CLOCK_GETRES => {
            post = Post::Timespec32 { user: a[1], scratch };
            a[1] = if a[1] != 0 { scratch } else { 0 };
            if n == x::CLOCK_GETTIME { nr::CLOCK_GETTIME } else { nr::CLOCK_GETRES }
        }
        x::CLOCK_GETTIME64 => nr::CLOCK_GETTIME,
        x::CLOCK_GETRES_TIME64 => nr::CLOCK_GETRES,
        x::CLOCK_NANOSLEEP_TIME64 => nr::CLOCK_NANOSLEEP,
        x::CLOCK_NANOSLEEP => {
            // clock_nanosleep(clk, flags, &req32, &rem32): the remainder is
            // not reported back in the 32-bit layout.
            a[2] = widen_time(m, a[2], scratch, 1);
            a[3] = 0;
            nr::CLOCK_NANOSLEEP
        }
        x::NANOSLEEP => {
            a[0] = widen_time(m, a[0], scratch, 1);
            a[1] = 0;
            nr::NANOSLEEP
        }
        x::TIMERFD_CREATE => nr::TIMERFD_CREATE,
        x::TIMERFD_SETTIME64 => {
            a[0] = sext(a[0]);
            nr::TIMERFD_SETTIME
        }
        x::TIMERFD_GETTIME64 => {
            a[0] = sext(a[0]);
            nr::TIMERFD_GETTIME
        }
        x::UTIMENSAT_TIME64 => {
            a[0] = sext(a[0]);
            nr::UTIMENSAT
        }
        x::MMAP2 => {
            // mmap2(addr, len, prot, flags, fd, pgoff)
            a[4] = sext(a[4]);
            a[5] = (a[5] & 0xffff_ffff) << 12;
            nr::MMAP
        }
        x::MMAP => {
            // The old form takes a pointer to six words.
            let p = a[0];
            let w = |i: u64| m.read_u32(p + i * 4).unwrap_or(0) as u64;
            *a = [w(0), w(1), w(2), w(3), sext(w(4)), w(5)];
            nr::MMAP
        }
        x::MUNMAP => nr::MUNMAP,
        x::MPROTECT => nr::MPROTECT,
        x::MREMAP => nr::MREMAP,
        x::MADVISE => nr::MADVISE,
        x::MSYNC => nr::MSYNC,
        x::FTRUNCATE => {
            a[0] = sext(a[0]);
            nr::FTRUNCATE
        }
        x::FTRUNCATE64 => {
            *a = [sext(a[0]), pair(a[1], a[2]), 0, 0, 0, 0];
            nr::FTRUNCATE
        }
        x::PREAD64 => {
            *a = [sext(a[0]), a[1], a[2], pair(a[3], a[4]), 0, 0];
            nr::PREAD64
        }
        x::PWRITE64 => {
            *a = [sext(a[0]), a[1], a[2], pair(a[3], a[4]), 0, 0];
            nr::PWRITE64
        }
        x::READV => {
            a[0] = sext(a[0]);
            nr::READV
        }
        x::WRITEV => {
            a[0] = sext(a[0]);
            nr::WRITEV
        }
        x::SENDFILE | x::SENDFILE64 => {
            a[0] = sext(a[0]);
            a[1] = sext(a[1]);
            nr::SENDFILE
        }
        x::COPY_FILE_RANGE => {
            a[0] = sext(a[0]);
            a[2] = sext(a[2]);
            nr::COPY_FILE_RANGE
        }
        x::FALLOCATE => {
            // fallocate(fd, mode, off_lo, off_hi, len_lo, len_hi)
            *a = [sext(a[0]), a[1], pair(a[2], a[3]), pair(a[4], a[5]), 0, 0];
            nr::FALLOCATE
        }
        x::STAT | x::STAT64 => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            nr::NEWFSTATAT
        }
        x::LSTAT | x::LSTAT64 => {
            *a = [AT_FDCWD, a[0], a[1], AT_SYMLINK_NOFOLLOW, 0, 0];
            nr::NEWFSTATAT
        }
        x::FSTAT | x::FSTAT64 => {
            a[0] = sext(a[0]);
            nr::FSTAT
        }
        x::FSTATAT64 => {
            a[0] = sext(a[0]);
            nr::NEWFSTATAT
        }
        x::STATX => {
            a[0] = sext(a[0]);
            nr::STATX
        }
        x::GETDENTS64 => {
            a[0] = sext(a[0]);
            nr::GETDENTS64
        }
        x::FLOCK => {
            a[0] = sext(a[0]);
            nr::FLOCK
        }
        x::POLL => {
            // poll(fds, n, timeout_ms) -> ppoll(fds, n, &ts, NULL)
            let ms = a[2] as u32 as i32;
            let ts = if ms < 0 {
                0
            } else {
                let _ = m.write_u64(scratch, (ms / 1000) as u64);
                let _ = m.write_u64(scratch + 8, ((ms % 1000) as u64) * 1_000_000);
                scratch
            };
            *a = [a[0], a[1], ts, 0, 8, 0];
            nr::PPOLL
        }
        x::PPOLL_TIME64 => nr::PPOLL,
        x::PPOLL => {
            // The 32-bit timespec form: only the timeout needs widening.
            a[2] = widen_time(m, a[2], scratch, 1);
            nr::PPOLL
        }
        x::NEWSELECT => {
            // select(n, r, w, e, &timeval32) -> pselect6(n, r, w, e, &ts, NULL)
            a[4] = widen_time(m, a[4], scratch, 1000);
            a[5] = 0;
            nr::PSELECT6
        }
        x::PSELECT6_TIME64 | x::PSELECT6 => {
            if n == x::PSELECT6 {
                a[4] = widen_time(m, a[4], scratch, 1);
            }
            // The sixth argument points at { sigset_t *ss; size_t len } with
            // 32-bit fields; the kernel reads 64-bit ones.
            if a[5] != 0 {
                let ss = m.read_u32(a[5]).unwrap_or(0) as u64;
                let len = m.read_u32(a[5] + 4).unwrap_or(0) as u64;
                let at = scratch + 16;
                let _ = m.write_u64(at, ss);
                let _ = m.write_u64(at + 8, len);
                a[5] = at;
            }
            nr::PSELECT6
        }
        x::EPOLL_CREATE1 => nr::EPOLL_CREATE1,
        x::EPOLL_CTL => {
            a[0] = sext(a[0]);
            a[2] = sext(a[2]);
            nr::EPOLL_CTL
        }
        x::EPOLL_PWAIT => {
            a[0] = sext(a[0]);
            nr::EPOLL_PWAIT
        }
        x::EPOLL_PWAIT2 => {
            a[0] = sext(a[0]);
            nr::EPOLL_PWAIT2
        }
        x::EVENTFD2 => nr::EVENTFD2,
        x::MEMFD_CREATE => nr::MEMFD_CREATE,
        x::SCHED_YIELD => nr::SCHED_YIELD,
        x::SCHED_GETAFFINITY => nr::SCHED_GETAFFINITY,
        x::GETCPU => nr::GETCPU,
        x::GETPRIORITY => nr::GETPRIORITY,
        x::SETPRIORITY => nr::SETPRIORITY,
        x::PRCTL => nr::PRCTL,
        x::ARCH_PRCTL => nr::ARCH_PRCTL,
        x::PERSONALITY => nr::NOOP_OK,
        x::UNAME => nr::UNAME,
        x::SYSINFO => nr::SYSINFO,
        x::GETRANDOM => nr::GETRANDOM,
        x::RT_SIGACTION => nr::RT_SIGACTION,
        x::RT_SIGPROCMASK => nr::RT_SIGPROCMASK,
        x::RT_SIGRETURN | x::SIGRETURN => nr::RT_SIGRETURN,
        x::RT_SIGPENDING => nr::RT_SIGPENDING,
        x::RT_SIGSUSPEND => nr::RT_SIGSUSPEND,
        x::RT_SIGTIMEDWAIT_TIME64 => nr::RT_SIGTIMEDWAIT,
        x::RT_SIGTIMEDWAIT => {
            a[2] = widen_time(m, a[2], scratch, 1);
            nr::RT_SIGTIMEDWAIT
        }
        x::SIGALTSTACK => nr::SIGALTSTACK,
        x::SETITIMER => {
            // setitimer(which, &new32, &old32): the itimerval's two timevals
            // are widened into scratch; the old value is not reported back.
            let p = a[1];
            let new = if p == 0 {
                0
            } else {
                widen_time(m, p, scratch, 1000);
                widen_time(m, p + 8, scratch + 16, 1000);
                scratch
            };
            *a = [a[0], new, 0, 0, 0, 0];
            nr::SETITIMER
        }
        x::GETRLIMIT | x::UGETRLIMIT => {
            // getrlimit(resource, &rlimit32) -> prlimit64(0, resource, NULL, &rlimit64 in scratch)
            post = Post::Rlimit32 { user: a[1], scratch };
            *a = [0, a[0], 0, scratch, 0, 0];
            nr::PRLIMIT64
        }
        x::GETITIMER | x::GETRUSAGE | x::TIMES => {
            // Their 32-bit result layouts are not written yet. ENOSYS is
            // loud where a silently wrong struct would not be.
            UNKNOWN
        }
        x::PRLIMIT64 => {
            a[0] = sext(a[0]);
            nr::PRLIMIT64
        }
        x::FUTEX_TIME64 => nr::FUTEX,
        x::FUTEX => {
            // futex(uaddr, op, val, &timespec32 | val2, uaddr2, val3): the
            // fourth word is a timeout only for the waiting operations.
            const FUTEX_CMD_MASK: u64 = 0x7f;
            let cmd = a[1] & FUTEX_CMD_MASK;
            // WAIT (0), LOCK_PI (6), WAIT_BITSET (9), WAIT_REQUEUE_PI (11)
            if matches!(cmd, 0 | 6 | 9 | 11) {
                a[3] = widen_time(m, a[3], scratch, 1);
            }
            nr::FUTEX
        }
        x::SET_TID_ADDRESS => nr::SET_TID_ADDRESS,
        x::SET_ROBUST_LIST => nr::SET_ROBUST_LIST,
        x::GET_ROBUST_LIST => nr::GET_ROBUST_LIST,
        x::RSEQ => nr::RSEQ,
        x::MEMBARRIER => nr::MEMBARRIER,
        x::CLONE => {
            // clone(flags, stack, parent_tid, tls, child_tid): the canonical
            // order already. `tls` is a `struct user_desc *`, not a base.
            if a[0] & CLONE_SETTLS != 0 && a[3] != 0 {
                a[3] = m.read_u32(a[3] + 4).unwrap_or(0) as u64;
            }
            nr::CLONE
        }
        x::CLONE3 => nr::CLONE3,
        x::SET_THREAD_AREA => {
            // struct user_desc { int entry_number; u32 base_addr; u32 limit;
            // u32 bitfields }. Linux allots GDT entries 6..=8 to TLS; -1
            // asks for a free one. There is one segment base register for
            // %gs, which is the register i386 programs load the entry into,
            // so the base is installed there directly (see `sys_clone` for
            // the same shortcut on a new thread).
            let p = a[0];
            let entry = m.read_u32(p).unwrap_or(0) as i32;
            let base = m.read_u32(p + 4).unwrap_or(0) as u64;
            if entry == -1 {
                let _ = m.write_u32(p, 6);
            }
            let _ = m.write_u64(ctx_addr + gex_abi::ctx::GS_BASE as u64, base);
            *a = [0; 6];
            nr::NOOP_OK
        }
        x::GET_THREAD_AREA => UNKNOWN,
        x::SOCKET => nr::SOCKET,
        x::SOCKETPAIR => nr::SOCKETPAIR,
        x::BIND => {
            a[0] = sext(a[0]);
            nr::BIND
        }
        x::CONNECT => {
            a[0] = sext(a[0]);
            nr::CONNECT
        }
        x::LISTEN => {
            a[0] = sext(a[0]);
            nr::LISTEN
        }
        x::ACCEPT4 => {
            a[0] = sext(a[0]);
            nr::ACCEPT4
        }
        x::GETSOCKOPT => {
            a[0] = sext(a[0]);
            nr::GETSOCKOPT
        }
        x::SETSOCKOPT => {
            a[0] = sext(a[0]);
            nr::SETSOCKOPT
        }
        x::GETSOCKNAME => {
            a[0] = sext(a[0]);
            nr::GETSOCKNAME
        }
        x::GETPEERNAME => {
            a[0] = sext(a[0]);
            nr::GETPEERNAME
        }
        x::SENDTO => {
            a[0] = sext(a[0]);
            nr::SENDTO
        }
        x::RECVFROM => {
            a[0] = sext(a[0]);
            nr::RECVFROM
        }
        x::SHUTDOWN => {
            a[0] = sext(a[0]);
            nr::SHUTDOWN
        }
        // msghdr and its iovecs are 32-bit structures the kernel does not
        // read yet.
        x::SENDMSG | x::RECVMSG | x::SENDMMSG | x::RECVMMSG | x::RECVMMSG_TIME64 | x::SOCKETCALL => {
            UNKNOWN
        }
        x::REBOOT => nr::REBOOT,
        x::STATFS | x::FSTATFS | x::STATFS64 | x::FSTATFS64 => UNKNOWN,
        x::TRUNCATE | x::TRUNCATE64 | x::GETDENTS => UNKNOWN,
        _ => UNKNOWN,
    };
    (n, post)
}

/// The i386 program's argument registers: `ebx, ecx, edx, esi, edi, ebp`,
/// as context slots.
pub const ARG_SLOTS: [usize; 6] = [
    gex_abi::ctx::x(3),
    gex_abi::ctx::x(1),
    gex_abi::ctx::x(2),
    gex_abi::ctx::x(6),
    gex_abi::ctx::x(7),
    gex_abi::ctx::x(5),
];
