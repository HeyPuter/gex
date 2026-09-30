//! The 32-bit PowerPC guest ABI (docs/ppc.md §6): system-call numbering,
//! the argument conventions that differ from i386's, and the flag spaces
//! that differ from the canonical (asm-generic) ones.
//!
//! ppc32 and i386 are both old-numbering 32-bit ABIs with the same
//! structures for almost every call, so most calls are renumbered to their
//! i386 equivalent and handed to [`crate::i386::translate`], which already
//! narrows and widens the 32-bit layouts. What ppc32 does differently is
//! handled here first:
//!
//! - a 64-bit argument travels in an aligned register pair, high word first
//!   (`pread64(fd, buf, n, pad, hi, lo)`);
//! - `mmap` takes its six arguments in registers, with a byte offset;
//! - `clone`'s TLS argument is the thread pointer itself (r2), not an i386
//!   `user_desc`;
//! - `O_DIRECT` and `O_LARGEFILE` swap places, and `MAP_NORESERVE` and
//!   `MAP_LOCKED` have values of their own;
//! - the tty ioctls have ppc numbers (`_IOC_READ` is 2, `_IOC_WRITE` 4).
//!
//! The syscall error convention (CR0.SO with a positive errno) is
//! `syscalls::set_result`'s.

use crate::i386::Post;
use crate::syscalls::nr;
use crate::GuestMem;

/// The pseudo system registers the lifter writes to have the host fail
/// loudly (`gex_lift_ppc::sysreg`; the two must agree): a non-default FPSCR
/// rounding mode, and `mffs` under `GEX_FP_STRICT=1`.
pub const SYSREG_FPSCR_RN: i32 = -110;
pub const SYSREG_MFFS_STRICT: i32 = -111;

/// A raw number with no canonical equivalent. Dispatch answers `ENOSYS`.
pub const UNKNOWN: u64 = u64::MAX;

const AT_FDCWD: u64 = (-100i64) as u64;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;

/// ppc's `O_LARGEFILE`/`O_DIRECT` and the canonical (AArch64) ones: the two
/// bits are swapped between the ABIs.
const PPC_O_LARGEFILE: u64 = 0o200000;
const PPC_O_DIRECT: u64 = 0o400000;
const O_DIRECT: u64 = 0o200000;
const O_LARGEFILE: u64 = 0o400000;

/// ppc's `MAP_NORESERVE`/`MAP_LOCKED`, and the canonical values.
const PPC_MAP_NORESERVE: u64 = 0x40;
const PPC_MAP_LOCKED: u64 = 0x80;
const MAP_NORESERVE: u64 = 0x4000;
const MAP_LOCKED: u64 = 0x2000;

fn sext(v: u64) -> u64 {
    v as u32 as i32 as i64 as u64
}

/// A 64-bit value from a register pair, high word first.
fn pair_hi_lo(hi: u64, lo: u64) -> u64 {
    ((hi & 0xffff_ffff) << 32) | (lo & 0xffff_ffff)
}

/// Open flags, ppc spelling to canonical.
pub fn open_flags(f: u64) -> u64 {
    let mut out = f & !(PPC_O_LARGEFILE | PPC_O_DIRECT);
    if f & PPC_O_LARGEFILE != 0 {
        out |= O_LARGEFILE;
    }
    if f & PPC_O_DIRECT != 0 {
        out |= O_DIRECT;
    }
    out
}

/// Open flags, canonical to ppc spelling (`fcntl(F_GETFL)`).
pub fn open_flags_out(f: u64) -> u64 {
    let mut out = f & !(O_LARGEFILE | O_DIRECT);
    if f & O_LARGEFILE != 0 {
        out |= PPC_O_LARGEFILE;
    }
    if f & O_DIRECT != 0 {
        out |= PPC_O_DIRECT;
    }
    out
}

fn mmap_flags(f: u64) -> u64 {
    let mut out = f & !(PPC_MAP_NORESERVE | PPC_MAP_LOCKED);
    if f & PPC_MAP_NORESERVE != 0 {
        out |= MAP_NORESERVE;
    }
    if f & PPC_MAP_LOCKED != 0 {
        out |= MAP_LOCKED;
    }
    out
}

/// A ppc ioctl request, as the canonical (asm-generic) number the kernel's
/// handlers know, for the requests whose argument layout is the same on
/// both; anything else is passed through, and fails as an unknown request.
fn ioctl_request(r: u64) -> u64 {
    match r & 0xffff_ffff {
        0x4008_7468 => 0x5413, // TIOCGWINSZ
        0x8008_7467 => 0x5414, // TIOCSWINSZ
        0x4004_667f => 0x541b, // FIONREAD
        0x8004_667e => 0x5421, // FIONBIO
        0x8004_667d => 0x5452, // FIOASYNC
        0x2000_6601 => 0x5451, // FIOCLEX
        0x2000_6602 => 0x5450, // FIONCLEX
        0x4004_7477 => 0x540f, // TIOCGPGRP
        0x8004_7476 => 0x5410, // TIOCSPGRP
        0x0000_540e => 0x540e, // TIOCSCTTY
        0x4004_5430 => 0x8004_5430, // TIOCGPTN
        0x8004_5431 => 0x4004_5431, // TIOCSPTLCK
        other => other,
    }
}

/// What to do with a successful result before the guest sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PpcPost {
    None,
    /// `fcntl(F_GETFL)`: the flags back in the ppc spelling.
    GetFl,
}

/// Translate a ppc32 call into the canonical one. `scratch` and `ctx_addr`
/// are as for [`crate::i386::translate`].
pub fn translate(
    n: u64,
    a: &mut [u64; 6],
    m: &mut GuestMem<'_>,
    scratch: u64,
    ctx_addr: u64,
) -> (u64, Post, PpcPost) {
    use raw as p;
    let mut ppost = PpcPost::None;
    let direct = match n {
        p::MMAP => {
            // mmap(addr, len, prot, flags, fd, off): a byte offset, in
            // registers.
            *a = [a[0], a[1], a[2], mmap_flags(a[3]), sext(a[4]), a[5]];
            Some(nr::MMAP)
        }
        p::MMAP2 => {
            *a = [a[0], a[1], a[2], mmap_flags(a[3]), sext(a[4]), (a[5] & 0xffff_ffff) << 12];
            Some(nr::MMAP)
        }
        p::PREAD64 | p::PWRITE64 => {
            *a = [sext(a[0]), a[1], a[2], pair_hi_lo(a[4], a[5]), 0, 0];
            Some(if n == p::PREAD64 { nr::PREAD64 } else { nr::PWRITE64 })
        }
        p::FTRUNCATE64 => {
            *a = [sext(a[0]), pair_hi_lo(a[2], a[3]), 0, 0, 0, 0];
            Some(nr::FTRUNCATE)
        }
        p::FALLOCATE => {
            *a = [sext(a[0]), a[1], pair_hi_lo(a[2], a[3]), pair_hi_lo(a[4], a[5]), 0, 0];
            Some(nr::FALLOCATE)
        }
        // Advice the kernel may ignore: fadvise64(fd, hi, lo, len, advice),
        // fadvise64_64(fd, advice, hi, lo, hi, lo), readahead, sync_file_range2.
        p::FADVISE64 | p::FADVISE64_64 | p::READAHEAD | p::SYNC_FILE_RANGE2 | p::MLOCK
        | p::MUNLOCK | p::MLOCKALL | p::MUNLOCKALL => Some(nr::NOOP_OK),
        p::CLONE => {
            // clone(flags, stack, parent_tid, tls, child_tid): the canonical
            // order, and `tls` is the new thread's r2.
            Some(nr::CLONE)
        }
        p::RT_SIGRETURN => Some(nr::RT_SIGRETURN),
        p::OPEN => {
            *a = [AT_FDCWD, a[0], open_flags(a[1]), a[2], 0, 0];
            Some(nr::OPENAT)
        }
        p::CREAT => {
            *a = [AT_FDCWD, a[0], 0o1101, a[1], 0, 0];
            Some(nr::OPENAT)
        }
        p::OPENAT => {
            *a = [sext(a[0]), a[1], open_flags(a[2]), a[3], 0, 0];
            Some(nr::OPENAT)
        }
        p::FCNTL | p::FCNTL64 => {
            const F_GETFL: u64 = 3;
            const F_SETFL: u64 = 4;
            if a[1] == F_SETFL {
                a[2] = open_flags(a[2]);
            }
            if a[1] == F_GETFL {
                ppost = PpcPost::GetFl;
            }
            a[0] = sext(a[0]);
            Some(nr::FCNTL)
        }
        p::IOCTL => {
            *a = [sext(a[0]), ioctl_request(a[1]), a[2], 0, 0, 0];
            Some(nr::IOCTL)
        }
        p::STAT | p::STAT64 => {
            *a = [AT_FDCWD, a[0], a[1], 0, 0, 0];
            Some(nr::NEWFSTATAT)
        }
        p::LSTAT | p::LSTAT64 => {
            *a = [AT_FDCWD, a[0], a[1], AT_SYMLINK_NOFOLLOW, 0, 0];
            Some(nr::NEWFSTATAT)
        }
        p::FSTAT | p::FSTAT64 => {
            a[0] = sext(a[0]);
            Some(nr::FSTAT)
        }
        p::FSTATAT64 => {
            a[0] = sext(a[0]);
            Some(nr::NEWFSTATAT)
        }
        p::ACCEPT => {
            *a = [sext(a[0]), a[1], a[2], 0, 0, 0];
            Some(nr::ACCEPT)
        }
        p::SEND | p::RECV => {
            *a = [sext(a[0]), a[1], a[2], a[3], 0, 0];
            Some(if n == p::SEND { nr::SENDTO } else { nr::RECVFROM })
        }
        p::EPOLL_CREATE => {
            *a = [0; 6];
            Some(nr::EPOLL_CREATE1)
        }
        p::EPOLL_WAIT => {
            *a = [sext(a[0]), a[1], a[2], a[3], 0, 8];
            Some(nr::EPOLL_PWAIT)
        }
        p::EVENTFD => {
            *a = [a[0], 0, 0, 0, 0, 0];
            Some(nr::EVENTFD2)
        }
        // The old sigreturn and sigaction families, ppc_select, and i386's
        // segment calls have no ppc32 musl or glibc caller.
        p::SIGRETURN | p::SELECT | p::SIGACTION | p::SIGPROCMASK | p::SOCKETCALL => Some(UNKNOWN),
        _ => None,
    };
    if let Some(c) = direct {
        return (c, Post::None, ppost);
    }
    match i386_number(n) {
        Some(x) => {
            let (c, post) = crate::i386::translate(x, a, m, scratch, ctx_addr);
            (c, post, PpcPost::None)
        }
        None => (UNKNOWN, Post::None, PpcPost::None),
    }
}

/// Apply [`PpcPost`] to a successful result.
pub fn finish(post: PpcPost, r: u64) -> u64 {
    match post {
        PpcPost::None => r,
        PpcPost::GetFl if (r as i64) >= 0 => open_flags_out(r),
        PpcPost::GetFl => r,
    }
}

/// The canonical number of a raw ppc call, for tests and traces.
pub fn canonical_number(n: u64) -> u64 {
    let mut a = [0u64; 6];
    let mut buf = [0u8; 0x200];
    let mut m = GuestMem::new(&mut buf);
    translate(n, &mut a, &mut m, 0x100, 0x100).0
}

/// PowerPC (32-bit) raw system-call numbers: `arch/powerpc/kernel/syscalls/syscall.tbl`,
/// the `common`, `32` and `nospu` entries.
#[allow(dead_code)]
pub mod raw {
    pub const RESTART_SYSCALL: u64 = 0;
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
    pub const MKNOD: u64 = 14;
    pub const CHMOD: u64 = 15;
    pub const LCHOWN: u64 = 16;
    pub const BREAK: u64 = 17;
    pub const OLDSTAT: u64 = 18;
    pub const LSEEK: u64 = 19;
    pub const GETPID: u64 = 20;
    pub const MOUNT: u64 = 21;
    pub const UMOUNT: u64 = 22;
    pub const SETUID: u64 = 23;
    pub const GETUID: u64 = 24;
    pub const STIME: u64 = 25;
    pub const PTRACE: u64 = 26;
    pub const ALARM: u64 = 27;
    pub const OLDFSTAT: u64 = 28;
    pub const PAUSE: u64 = 29;
    pub const UTIME: u64 = 30;
    pub const STTY: u64 = 31;
    pub const GTTY: u64 = 32;
    pub const ACCESS: u64 = 33;
    pub const NICE: u64 = 34;
    pub const FTIME: u64 = 35;
    pub const SYNC: u64 = 36;
    pub const KILL: u64 = 37;
    pub const RENAME: u64 = 38;
    pub const MKDIR: u64 = 39;
    pub const RMDIR: u64 = 40;
    pub const DUP: u64 = 41;
    pub const PIPE: u64 = 42;
    pub const TIMES: u64 = 43;
    pub const PROF: u64 = 44;
    pub const BRK: u64 = 45;
    pub const SETGID: u64 = 46;
    pub const GETGID: u64 = 47;
    pub const SIGNAL: u64 = 48;
    pub const GETEUID: u64 = 49;
    pub const GETEGID: u64 = 50;
    pub const ACCT: u64 = 51;
    pub const UMOUNT2: u64 = 52;
    pub const LOCK: u64 = 53;
    pub const IOCTL: u64 = 54;
    pub const FCNTL: u64 = 55;
    pub const MPX: u64 = 56;
    pub const SETPGID: u64 = 57;
    pub const ULIMIT: u64 = 58;
    pub const OLDOLDUNAME: u64 = 59;
    pub const UMASK: u64 = 60;
    pub const CHROOT: u64 = 61;
    pub const USTAT: u64 = 62;
    pub const DUP2: u64 = 63;
    pub const GETPPID: u64 = 64;
    pub const GETPGRP: u64 = 65;
    pub const SETSID: u64 = 66;
    pub const SIGACTION: u64 = 67;
    pub const SGETMASK: u64 = 68;
    pub const SSETMASK: u64 = 69;
    pub const SETREUID: u64 = 70;
    pub const SETREGID: u64 = 71;
    pub const SIGSUSPEND: u64 = 72;
    pub const SIGPENDING: u64 = 73;
    pub const SETHOSTNAME: u64 = 74;
    pub const SETRLIMIT: u64 = 75;
    pub const GETRLIMIT: u64 = 76;
    pub const GETRUSAGE: u64 = 77;
    pub const GETTIMEOFDAY: u64 = 78;
    pub const SETTIMEOFDAY: u64 = 79;
    pub const GETGROUPS: u64 = 80;
    pub const SETGROUPS: u64 = 81;
    pub const SELECT: u64 = 82;
    pub const SYMLINK: u64 = 83;
    pub const OLDLSTAT: u64 = 84;
    pub const READLINK: u64 = 85;
    pub const USELIB: u64 = 86;
    pub const SWAPON: u64 = 87;
    pub const REBOOT: u64 = 88;
    pub const READDIR: u64 = 89;
    pub const MMAP: u64 = 90;
    pub const MUNMAP: u64 = 91;
    pub const TRUNCATE: u64 = 92;
    pub const FTRUNCATE: u64 = 93;
    pub const FCHMOD: u64 = 94;
    pub const FCHOWN: u64 = 95;
    pub const GETPRIORITY: u64 = 96;
    pub const SETPRIORITY: u64 = 97;
    pub const PROFIL: u64 = 98;
    pub const STATFS: u64 = 99;
    pub const FSTATFS: u64 = 100;
    pub const IOPERM: u64 = 101;
    pub const SOCKETCALL: u64 = 102;
    pub const SYSLOG: u64 = 103;
    pub const SETITIMER: u64 = 104;
    pub const GETITIMER: u64 = 105;
    pub const STAT: u64 = 106;
    pub const LSTAT: u64 = 107;
    pub const FSTAT: u64 = 108;
    pub const OLDUNAME: u64 = 109;
    pub const IOPL: u64 = 110;
    pub const VHANGUP: u64 = 111;
    pub const IDLE: u64 = 112;
    pub const VM86: u64 = 113;
    pub const WAIT4: u64 = 114;
    pub const SWAPOFF: u64 = 115;
    pub const SYSINFO: u64 = 116;
    pub const IPC: u64 = 117;
    pub const FSYNC: u64 = 118;
    pub const SIGRETURN: u64 = 119;
    pub const CLONE: u64 = 120;
    pub const SETDOMAINNAME: u64 = 121;
    pub const UNAME: u64 = 122;
    pub const MODIFY_LDT: u64 = 123;
    pub const ADJTIMEX: u64 = 124;
    pub const MPROTECT: u64 = 125;
    pub const SIGPROCMASK: u64 = 126;
    pub const CREATE_MODULE: u64 = 127;
    pub const INIT_MODULE: u64 = 128;
    pub const DELETE_MODULE: u64 = 129;
    pub const GET_KERNEL_SYMS: u64 = 130;
    pub const QUOTACTL: u64 = 131;
    pub const GETPGID: u64 = 132;
    pub const FCHDIR: u64 = 133;
    pub const BDFLUSH: u64 = 134;
    pub const SYSFS: u64 = 135;
    pub const PERSONALITY: u64 = 136;
    pub const AFS_SYSCALL: u64 = 137;
    pub const SETFSUID: u64 = 138;
    pub const SETFSGID: u64 = 139;
    pub const _LLSEEK: u64 = 140;
    pub const GETDENTS: u64 = 141;
    pub const _NEWSELECT: u64 = 142;
    pub const FLOCK: u64 = 143;
    pub const MSYNC: u64 = 144;
    pub const READV: u64 = 145;
    pub const WRITEV: u64 = 146;
    pub const GETSID: u64 = 147;
    pub const FDATASYNC: u64 = 148;
    pub const _SYSCTL: u64 = 149;
    pub const MLOCK: u64 = 150;
    pub const MUNLOCK: u64 = 151;
    pub const MLOCKALL: u64 = 152;
    pub const MUNLOCKALL: u64 = 153;
    pub const SCHED_SETPARAM: u64 = 154;
    pub const SCHED_GETPARAM: u64 = 155;
    pub const SCHED_SETSCHEDULER: u64 = 156;
    pub const SCHED_GETSCHEDULER: u64 = 157;
    pub const SCHED_YIELD: u64 = 158;
    pub const SCHED_GET_PRIORITY_MAX: u64 = 159;
    pub const SCHED_GET_PRIORITY_MIN: u64 = 160;
    pub const SCHED_RR_GET_INTERVAL: u64 = 161;
    pub const NANOSLEEP: u64 = 162;
    pub const MREMAP: u64 = 163;
    pub const SETRESUID: u64 = 164;
    pub const GETRESUID: u64 = 165;
    pub const QUERY_MODULE: u64 = 166;
    pub const POLL: u64 = 167;
    pub const NFSSERVCTL: u64 = 168;
    pub const SETRESGID: u64 = 169;
    pub const GETRESGID: u64 = 170;
    pub const PRCTL: u64 = 171;
    pub const RT_SIGRETURN: u64 = 172;
    pub const RT_SIGACTION: u64 = 173;
    pub const RT_SIGPROCMASK: u64 = 174;
    pub const RT_SIGPENDING: u64 = 175;
    pub const RT_SIGTIMEDWAIT: u64 = 176;
    pub const RT_SIGQUEUEINFO: u64 = 177;
    pub const RT_SIGSUSPEND: u64 = 178;
    pub const PREAD64: u64 = 179;
    pub const PWRITE64: u64 = 180;
    pub const CHOWN: u64 = 181;
    pub const GETCWD: u64 = 182;
    pub const CAPGET: u64 = 183;
    pub const CAPSET: u64 = 184;
    pub const SIGALTSTACK: u64 = 185;
    pub const SENDFILE: u64 = 186;
    pub const GETPMSG: u64 = 187;
    pub const PUTPMSG: u64 = 188;
    pub const VFORK: u64 = 189;
    pub const UGETRLIMIT: u64 = 190;
    pub const READAHEAD: u64 = 191;
    pub const MMAP2: u64 = 192;
    pub const TRUNCATE64: u64 = 193;
    pub const FTRUNCATE64: u64 = 194;
    pub const STAT64: u64 = 195;
    pub const LSTAT64: u64 = 196;
    pub const FSTAT64: u64 = 197;
    pub const PCICONFIG_READ: u64 = 198;
    pub const PCICONFIG_WRITE: u64 = 199;
    pub const PCICONFIG_IOBASE: u64 = 200;
    pub const MULTIPLEXER: u64 = 201;
    pub const GETDENTS64: u64 = 202;
    pub const PIVOT_ROOT: u64 = 203;
    pub const FCNTL64: u64 = 204;
    pub const MADVISE: u64 = 205;
    pub const MINCORE: u64 = 206;
    pub const GETTID: u64 = 207;
    pub const TKILL: u64 = 208;
    pub const SETXATTR: u64 = 209;
    pub const LSETXATTR: u64 = 210;
    pub const FSETXATTR: u64 = 211;
    pub const GETXATTR: u64 = 212;
    pub const LGETXATTR: u64 = 213;
    pub const FGETXATTR: u64 = 214;
    pub const LISTXATTR: u64 = 215;
    pub const LLISTXATTR: u64 = 216;
    pub const FLISTXATTR: u64 = 217;
    pub const REMOVEXATTR: u64 = 218;
    pub const LREMOVEXATTR: u64 = 219;
    pub const FREMOVEXATTR: u64 = 220;
    pub const FUTEX: u64 = 221;
    pub const SCHED_SETAFFINITY: u64 = 222;
    pub const SCHED_GETAFFINITY: u64 = 223;
    pub const TUXCALL: u64 = 225;
    pub const SENDFILE64: u64 = 226;
    pub const IO_SETUP: u64 = 227;
    pub const IO_DESTROY: u64 = 228;
    pub const IO_GETEVENTS: u64 = 229;
    pub const IO_SUBMIT: u64 = 230;
    pub const IO_CANCEL: u64 = 231;
    pub const SET_TID_ADDRESS: u64 = 232;
    pub const FADVISE64: u64 = 233;
    pub const EXIT_GROUP: u64 = 234;
    pub const LOOKUP_DCOOKIE: u64 = 235;
    pub const EPOLL_CREATE: u64 = 236;
    pub const EPOLL_CTL: u64 = 237;
    pub const EPOLL_WAIT: u64 = 238;
    pub const REMAP_FILE_PAGES: u64 = 239;
    pub const TIMER_CREATE: u64 = 240;
    pub const TIMER_SETTIME: u64 = 241;
    pub const TIMER_GETTIME: u64 = 242;
    pub const TIMER_GETOVERRUN: u64 = 243;
    pub const TIMER_DELETE: u64 = 244;
    pub const CLOCK_SETTIME: u64 = 245;
    pub const CLOCK_GETTIME: u64 = 246;
    pub const CLOCK_GETRES: u64 = 247;
    pub const CLOCK_NANOSLEEP: u64 = 248;
    pub const SWAPCONTEXT: u64 = 249;
    pub const TGKILL: u64 = 250;
    pub const UTIMES: u64 = 251;
    pub const STATFS64: u64 = 252;
    pub const FSTATFS64: u64 = 253;
    pub const FADVISE64_64: u64 = 254;
    pub const RTAS: u64 = 255;
    pub const SYS_DEBUG_SETCONTEXT: u64 = 256;
    pub const MIGRATE_PAGES: u64 = 258;
    pub const MBIND: u64 = 259;
    pub const GET_MEMPOLICY: u64 = 260;
    pub const SET_MEMPOLICY: u64 = 261;
    pub const MQ_OPEN: u64 = 262;
    pub const MQ_UNLINK: u64 = 263;
    pub const MQ_TIMEDSEND: u64 = 264;
    pub const MQ_TIMEDRECEIVE: u64 = 265;
    pub const MQ_NOTIFY: u64 = 266;
    pub const MQ_GETSETATTR: u64 = 267;
    pub const KEXEC_LOAD: u64 = 268;
    pub const ADD_KEY: u64 = 269;
    pub const REQUEST_KEY: u64 = 270;
    pub const KEYCTL: u64 = 271;
    pub const WAITID: u64 = 272;
    pub const IOPRIO_SET: u64 = 273;
    pub const IOPRIO_GET: u64 = 274;
    pub const INOTIFY_INIT: u64 = 275;
    pub const INOTIFY_ADD_WATCH: u64 = 276;
    pub const INOTIFY_RM_WATCH: u64 = 277;
    pub const SPU_RUN: u64 = 278;
    pub const SPU_CREATE: u64 = 279;
    pub const PSELECT6: u64 = 280;
    pub const PPOLL: u64 = 281;
    pub const UNSHARE: u64 = 282;
    pub const SPLICE: u64 = 283;
    pub const TEE: u64 = 284;
    pub const VMSPLICE: u64 = 285;
    pub const OPENAT: u64 = 286;
    pub const MKDIRAT: u64 = 287;
    pub const MKNODAT: u64 = 288;
    pub const FCHOWNAT: u64 = 289;
    pub const FUTIMESAT: u64 = 290;
    pub const FSTATAT64: u64 = 291;
    pub const UNLINKAT: u64 = 292;
    pub const RENAMEAT: u64 = 293;
    pub const LINKAT: u64 = 294;
    pub const SYMLINKAT: u64 = 295;
    pub const READLINKAT: u64 = 296;
    pub const FCHMODAT: u64 = 297;
    pub const FACCESSAT: u64 = 298;
    pub const GET_ROBUST_LIST: u64 = 299;
    pub const SET_ROBUST_LIST: u64 = 300;
    pub const MOVE_PAGES: u64 = 301;
    pub const GETCPU: u64 = 302;
    pub const EPOLL_PWAIT: u64 = 303;
    pub const UTIMENSAT: u64 = 304;
    pub const SIGNALFD: u64 = 305;
    pub const TIMERFD_CREATE: u64 = 306;
    pub const EVENTFD: u64 = 307;
    pub const SYNC_FILE_RANGE2: u64 = 308;
    pub const FALLOCATE: u64 = 309;
    pub const SUBPAGE_PROT: u64 = 310;
    pub const TIMERFD_SETTIME: u64 = 311;
    pub const TIMERFD_GETTIME: u64 = 312;
    pub const SIGNALFD4: u64 = 313;
    pub const EVENTFD2: u64 = 314;
    pub const EPOLL_CREATE1: u64 = 315;
    pub const DUP3: u64 = 316;
    pub const PIPE2: u64 = 317;
    pub const INOTIFY_INIT1: u64 = 318;
    pub const PERF_EVENT_OPEN: u64 = 319;
    pub const PREADV: u64 = 320;
    pub const PWRITEV: u64 = 321;
    pub const RT_TGSIGQUEUEINFO: u64 = 322;
    pub const FANOTIFY_INIT: u64 = 323;
    pub const FANOTIFY_MARK: u64 = 324;
    pub const PRLIMIT64: u64 = 325;
    pub const SOCKET: u64 = 326;
    pub const BIND: u64 = 327;
    pub const CONNECT: u64 = 328;
    pub const LISTEN: u64 = 329;
    pub const ACCEPT: u64 = 330;
    pub const GETSOCKNAME: u64 = 331;
    pub const GETPEERNAME: u64 = 332;
    pub const SOCKETPAIR: u64 = 333;
    pub const SEND: u64 = 334;
    pub const SENDTO: u64 = 335;
    pub const RECV: u64 = 336;
    pub const RECVFROM: u64 = 337;
    pub const SHUTDOWN: u64 = 338;
    pub const SETSOCKOPT: u64 = 339;
    pub const GETSOCKOPT: u64 = 340;
    pub const SENDMSG: u64 = 341;
    pub const RECVMSG: u64 = 342;
    pub const RECVMMSG: u64 = 343;
    pub const ACCEPT4: u64 = 344;
    pub const NAME_TO_HANDLE_AT: u64 = 345;
    pub const OPEN_BY_HANDLE_AT: u64 = 346;
    pub const CLOCK_ADJTIME: u64 = 347;
    pub const SYNCFS: u64 = 348;
    pub const SENDMMSG: u64 = 349;
    pub const SETNS: u64 = 350;
    pub const PROCESS_VM_READV: u64 = 351;
    pub const PROCESS_VM_WRITEV: u64 = 352;
    pub const FINIT_MODULE: u64 = 353;
    pub const KCMP: u64 = 354;
    pub const SCHED_SETATTR: u64 = 355;
    pub const SCHED_GETATTR: u64 = 356;
    pub const RENAMEAT2: u64 = 357;
    pub const SECCOMP: u64 = 358;
    pub const GETRANDOM: u64 = 359;
    pub const MEMFD_CREATE: u64 = 360;
    pub const BPF: u64 = 361;
    pub const EXECVEAT: u64 = 362;
    pub const SWITCH_ENDIAN: u64 = 363;
    pub const USERFAULTFD: u64 = 364;
    pub const MEMBARRIER: u64 = 365;
    pub const MLOCK2: u64 = 378;
    pub const COPY_FILE_RANGE: u64 = 379;
    pub const PREADV2: u64 = 380;
    pub const PWRITEV2: u64 = 381;
    pub const KEXEC_FILE_LOAD: u64 = 382;
    pub const STATX: u64 = 383;
    pub const PKEY_ALLOC: u64 = 384;
    pub const PKEY_FREE: u64 = 385;
    pub const PKEY_MPROTECT: u64 = 386;
    pub const RSEQ: u64 = 387;
    pub const IO_PGETEVENTS: u64 = 388;
    pub const SEMGET: u64 = 393;
    pub const SEMCTL: u64 = 394;
    pub const SHMGET: u64 = 395;
    pub const SHMCTL: u64 = 396;
    pub const SHMAT: u64 = 397;
    pub const SHMDT: u64 = 398;
    pub const MSGGET: u64 = 399;
    pub const MSGSND: u64 = 400;
    pub const MSGRCV: u64 = 401;
    pub const MSGCTL: u64 = 402;
    pub const CLOCK_GETTIME64: u64 = 403;
    pub const CLOCK_SETTIME64: u64 = 404;
    pub const CLOCK_ADJTIME64: u64 = 405;
    pub const CLOCK_GETRES_TIME64: u64 = 406;
    pub const CLOCK_NANOSLEEP_TIME64: u64 = 407;
    pub const TIMER_GETTIME64: u64 = 408;
    pub const TIMER_SETTIME64: u64 = 409;
    pub const TIMERFD_GETTIME64: u64 = 410;
    pub const TIMERFD_SETTIME64: u64 = 411;
    pub const UTIMENSAT_TIME64: u64 = 412;
    pub const PSELECT6_TIME64: u64 = 413;
    pub const PPOLL_TIME64: u64 = 414;
    pub const IO_PGETEVENTS_TIME64: u64 = 416;
    pub const RECVMMSG_TIME64: u64 = 417;
    pub const MQ_TIMEDSEND_TIME64: u64 = 418;
    pub const MQ_TIMEDRECEIVE_TIME64: u64 = 419;
    pub const SEMTIMEDOP_TIME64: u64 = 420;
    pub const RT_SIGTIMEDWAIT_TIME64: u64 = 421;
    pub const FUTEX_TIME64: u64 = 422;
    pub const SCHED_RR_GET_INTERVAL_TIME64: u64 = 423;
    pub const PIDFD_SEND_SIGNAL: u64 = 424;
    pub const IO_URING_SETUP: u64 = 425;
    pub const IO_URING_ENTER: u64 = 426;
    pub const IO_URING_REGISTER: u64 = 427;
    pub const OPEN_TREE: u64 = 428;
    pub const MOVE_MOUNT: u64 = 429;
    pub const FSOPEN: u64 = 430;
    pub const FSCONFIG: u64 = 431;
    pub const FSMOUNT: u64 = 432;
    pub const FSPICK: u64 = 433;
    pub const PIDFD_OPEN: u64 = 434;
    pub const CLONE3: u64 = 435;
    pub const CLOSE_RANGE: u64 = 436;
    pub const OPENAT2: u64 = 437;
    pub const PIDFD_GETFD: u64 = 438;
    pub const FACCESSAT2: u64 = 439;
    pub const PROCESS_MADVISE: u64 = 440;
    pub const EPOLL_PWAIT2: u64 = 441;
    pub const MOUNT_SETATTR: u64 = 442;
    pub const QUOTACTL_FD: u64 = 443;
    pub const LANDLOCK_CREATE_RULESET: u64 = 444;
    pub const LANDLOCK_ADD_RULE: u64 = 445;
    pub const LANDLOCK_RESTRICT_SELF: u64 = 446;
    pub const PROCESS_MRELEASE: u64 = 448;
    pub const FUTEX_WAITV: u64 = 449;
    pub const SET_MEMPOLICY_HOME_NODE: u64 = 450;
    pub const CACHESTAT: u64 = 451;
    pub const FCHMODAT2: u64 = 452;
    pub const MAP_SHADOW_STACK: u64 = 453;
    pub const FUTEX_WAKE: u64 = 454;
    pub const FUTEX_WAIT: u64 = 455;
    pub const FUTEX_REQUEUE: u64 = 456;
    pub const STATMOUNT: u64 = 457;
    pub const LISTMOUNT: u64 = 458;
    pub const LSM_GET_SELF_ATTR: u64 = 459;
    pub const LSM_SET_SELF_ATTR: u64 = 460;
    pub const LSM_LIST_MODULES: u64 = 461;
    pub const MSEAL: u64 = 462;
    pub const SETXATTRAT: u64 = 463;
    pub const GETXATTRAT: u64 = 464;
    pub const LISTXATTRAT: u64 = 465;
    pub const REMOVEXATTRAT: u64 = 466;
    pub const OPEN_TREE_ATTR: u64 = 467;
    pub const FILE_GETATTR: u64 = 468;
    pub const FILE_SETATTR: u64 = 469;
    pub const LISTNS: u64 = 470;
    pub const RSEQ_SLICE_YIELD: u64 = 471;
    pub const FCHROOT: u64 = 472;
}

/// The i386 number of a PowerPC call whose arguments and structures are
/// the same on both 32-bit ABIs, so `i386::translate` can take it from there.
fn i386_number(n: u64) -> Option<u64> {
    use crate::i386::raw as x;
    use raw as p;
    Some(match n {
        p::EXIT => x::EXIT,
        p::FORK => x::FORK,
        p::READ => x::READ,
        p::WRITE => x::WRITE,
        p::CLOSE => x::CLOSE,
        p::WAITPID => x::WAITPID,
        p::LINK => x::LINK,
        p::UNLINK => x::UNLINK,
        p::EXECVE => x::EXECVE,
        p::CHDIR => x::CHDIR,
        p::TIME => x::TIME,
        p::CHMOD => x::CHMOD,
        p::LCHOWN => x::LCHOWN16,
        p::LSEEK => x::LSEEK,
        p::GETPID => x::GETPID,
        p::MOUNT => x::MOUNT,
        p::SETUID => x::SETUID16,
        p::GETUID => x::GETUID16,
        p::ALARM => x::ALARM,
        p::ACCESS => x::ACCESS,
        p::SYNC => x::SYNC,
        p::KILL => x::KILL,
        p::RENAME => x::RENAME,
        p::MKDIR => x::MKDIR,
        p::RMDIR => x::RMDIR,
        p::DUP => x::DUP,
        p::PIPE => x::PIPE,
        p::TIMES => x::TIMES,
        p::BRK => x::BRK,
        p::SETGID => x::SETGID16,
        p::GETGID => x::GETGID16,
        p::GETEUID => x::GETEUID16,
        p::GETEGID => x::GETEGID16,
        p::UMOUNT2 => x::UMOUNT2,
        p::SETPGID => x::SETPGID,
        p::UMASK => x::UMASK,
        p::DUP2 => x::DUP2,
        p::GETPPID => x::GETPPID,
        p::GETPGRP => x::GETPGRP,
        p::SETSID => x::SETSID,
        p::SETREUID => x::SETREUID16,
        p::SETREGID => x::SETREGID16,
        p::GETRLIMIT => x::GETRLIMIT,
        p::GETRUSAGE => x::GETRUSAGE,
        p::GETTIMEOFDAY => x::GETTIMEOFDAY,
        p::GETGROUPS => x::GETGROUPS16,
        p::SETGROUPS => x::SETGROUPS16,
        p::SYMLINK => x::SYMLINK,
        p::READLINK => x::READLINK,
        p::REBOOT => x::REBOOT,
        p::MUNMAP => x::MUNMAP,
        p::TRUNCATE => x::TRUNCATE,
        p::FTRUNCATE => x::FTRUNCATE,
        p::FCHMOD => x::FCHMOD,
        p::FCHOWN => x::FCHOWN16,
        p::GETPRIORITY => x::GETPRIORITY,
        p::SETPRIORITY => x::SETPRIORITY,
        p::STATFS => x::STATFS,
        p::FSTATFS => x::FSTATFS,
        p::SETITIMER => x::SETITIMER,
        p::GETITIMER => x::GETITIMER,
        p::WAIT4 => x::WAIT4,
        p::SYSINFO => x::SYSINFO,
        p::FSYNC => x::FSYNC,
        p::UNAME => x::UNAME,
        p::MPROTECT => x::MPROTECT,
        p::GETPGID => x::GETPGID,
        p::FCHDIR => x::FCHDIR,
        p::PERSONALITY => x::PERSONALITY,
        p::_LLSEEK => x::LLSEEK,
        p::GETDENTS => x::GETDENTS,
        p::_NEWSELECT => x::NEWSELECT,
        p::FLOCK => x::FLOCK,
        p::MSYNC => x::MSYNC,
        p::READV => x::READV,
        p::WRITEV => x::WRITEV,
        p::GETSID => x::GETSID,
        p::FDATASYNC => x::FDATASYNC,
        p::SCHED_YIELD => x::SCHED_YIELD,
        p::NANOSLEEP => x::NANOSLEEP,
        p::MREMAP => x::MREMAP,
        p::SETRESUID => x::SETRESUID,
        p::GETRESUID => x::GETRESUID,
        p::POLL => x::POLL,
        p::SETRESGID => x::SETRESGID,
        p::GETRESGID => x::GETRESGID,
        p::PRCTL => x::PRCTL,
        p::RT_SIGACTION => x::RT_SIGACTION,
        p::RT_SIGPROCMASK => x::RT_SIGPROCMASK,
        p::RT_SIGPENDING => x::RT_SIGPENDING,
        p::RT_SIGTIMEDWAIT => x::RT_SIGTIMEDWAIT,
        p::RT_SIGSUSPEND => x::RT_SIGSUSPEND,
        p::CHOWN => x::CHOWN,
        p::GETCWD => x::GETCWD,
        p::SIGALTSTACK => x::SIGALTSTACK,
        p::SENDFILE => x::SENDFILE,
        p::VFORK => x::VFORK,
        p::UGETRLIMIT => x::UGETRLIMIT,
        p::GETDENTS64 => x::GETDENTS64,
        p::MADVISE => x::MADVISE,
        p::GETTID => x::GETTID,
        p::TKILL => x::TKILL,
        p::FUTEX => x::FUTEX,
        p::SCHED_GETAFFINITY => x::SCHED_GETAFFINITY,
        p::SENDFILE64 => x::SENDFILE64,
        p::SET_TID_ADDRESS => x::SET_TID_ADDRESS,
        p::EXIT_GROUP => x::EXIT_GROUP,
        p::EPOLL_CTL => x::EPOLL_CTL,
        p::CLOCK_GETTIME => x::CLOCK_GETTIME,
        p::CLOCK_GETRES => x::CLOCK_GETRES,
        p::CLOCK_NANOSLEEP => x::CLOCK_NANOSLEEP,
        p::TGKILL => x::TGKILL,
        p::STATFS64 => x::STATFS64,
        p::FSTATFS64 => x::FSTATFS64,
        p::PSELECT6 => x::PSELECT6,
        p::PPOLL => x::PPOLL,
        p::MKDIRAT => x::MKDIRAT,
        p::FCHOWNAT => x::FCHOWNAT,
        p::UNLINKAT => x::UNLINKAT,
        p::RENAMEAT => x::RENAMEAT,
        p::LINKAT => x::LINKAT,
        p::SYMLINKAT => x::SYMLINKAT,
        p::READLINKAT => x::READLINKAT,
        p::FCHMODAT => x::FCHMODAT,
        p::FACCESSAT => x::FACCESSAT,
        p::GET_ROBUST_LIST => x::GET_ROBUST_LIST,
        p::SET_ROBUST_LIST => x::SET_ROBUST_LIST,
        p::GETCPU => x::GETCPU,
        p::EPOLL_PWAIT => x::EPOLL_PWAIT,
        p::UTIMENSAT => x::UTIMENSAT,
        p::TIMERFD_CREATE => x::TIMERFD_CREATE,
        p::EVENTFD2 => x::EVENTFD2,
        p::EPOLL_CREATE1 => x::EPOLL_CREATE1,
        p::DUP3 => x::DUP3,
        p::PIPE2 => x::PIPE2,
        p::PRLIMIT64 => x::PRLIMIT64,
        p::SOCKET => x::SOCKET,
        p::BIND => x::BIND,
        p::CONNECT => x::CONNECT,
        p::LISTEN => x::LISTEN,
        p::GETSOCKNAME => x::GETSOCKNAME,
        p::GETPEERNAME => x::GETPEERNAME,
        p::SOCKETPAIR => x::SOCKETPAIR,
        p::SENDTO => x::SENDTO,
        p::RECVFROM => x::RECVFROM,
        p::SHUTDOWN => x::SHUTDOWN,
        p::SETSOCKOPT => x::SETSOCKOPT,
        p::GETSOCKOPT => x::GETSOCKOPT,
        p::SENDMSG => x::SENDMSG,
        p::RECVMSG => x::RECVMSG,
        p::RECVMMSG => x::RECVMMSG,
        p::ACCEPT4 => x::ACCEPT4,
        p::SENDMMSG => x::SENDMMSG,
        p::RENAMEAT2 => x::RENAMEAT2,
        p::GETRANDOM => x::GETRANDOM,
        p::MEMFD_CREATE => x::MEMFD_CREATE,
        p::MEMBARRIER => x::MEMBARRIER,
        p::COPY_FILE_RANGE => x::COPY_FILE_RANGE,
        p::STATX => x::STATX,
        p::RSEQ => x::RSEQ,
        p::CLOCK_GETTIME64 => x::CLOCK_GETTIME64,
        p::CLOCK_GETRES_TIME64 => x::CLOCK_GETRES_TIME64,
        p::CLOCK_NANOSLEEP_TIME64 => x::CLOCK_NANOSLEEP_TIME64,
        p::TIMERFD_GETTIME64 => x::TIMERFD_GETTIME64,
        p::TIMERFD_SETTIME64 => x::TIMERFD_SETTIME64,
        p::UTIMENSAT_TIME64 => x::UTIMENSAT_TIME64,
        p::PSELECT6_TIME64 => x::PSELECT6_TIME64,
        p::PPOLL_TIME64 => x::PPOLL_TIME64,
        p::RECVMMSG_TIME64 => x::RECVMMSG_TIME64,
        p::RT_SIGTIMEDWAIT_TIME64 => x::RT_SIGTIMEDWAIT_TIME64,
        p::FUTEX_TIME64 => x::FUTEX_TIME64,
        p::CLONE3 => x::CLONE3,
        p::CLOSE_RANGE => x::CLOSE_RANGE,
        p::FACCESSAT2 => x::FACCESSAT2,
        p::EPOLL_PWAIT2 => x::EPOLL_PWAIT2,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_calls_reach_their_canonical_numbers() {
        assert_eq!(canonical_number(raw::WRITE), nr::WRITE);
        assert_eq!(canonical_number(raw::EXIT_GROUP), nr::EXIT_GROUP);
        assert_eq!(canonical_number(raw::CLOCK_GETTIME64), nr::CLOCK_GETTIME);
        assert_eq!(canonical_number(raw::MMAP2), nr::MMAP);
        assert_eq!(canonical_number(raw::STATX), nr::STATX);
    }

    #[test]
    fn sixty_four_bit_pairs_are_high_word_first() {
        let mut buf = vec![0u8; 0x1000];
        let mut m = GuestMem::new(&mut buf);
        // pread64(3, buf, 10, pad, hi=1, lo=2)
        let mut a = [3, 0x2000, 10, 0, 1, 2];
        let (n, _, _) = translate(raw::PREAD64, &mut a, &mut m, 0x100, 0x100);
        assert_eq!(n, nr::PREAD64);
        assert_eq!(a[3], 0x1_0000_0002);
    }

    #[test]
    fn open_flags_swap_direct_and_largefile() {
        assert_eq!(open_flags(PPC_O_LARGEFILE | 2), O_LARGEFILE | 2);
        assert_eq!(open_flags_out(O_LARGEFILE), PPC_O_LARGEFILE);
    }
}
