//! Linux error numbers, and the negative-return convention.
//!
//! The aarch64 syscall ABI reports failure by returning `-errno` in `x0`, so
//! errors are values here rather than a separate channel.

/// A Linux errno value.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const ESRCH: Errno = Errno(3);
    pub const EINTR: Errno = Errno(4);
    pub const EIO: Errno = Errno(5);
    pub const ENXIO: Errno = Errno(6);
    pub const E2BIG: Errno = Errno(7);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const EAGAIN: Errno = Errno(11);
    pub const ENOEXEC: Errno = Errno(8);
    pub const ENOMEM: Errno = Errno(12);
    pub const EACCES: Errno = Errno(13);
    pub const EFAULT: Errno = Errno(14);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENODEV: Errno = Errno(19);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const ENFILE: Errno = Errno(23);
    pub const EMFILE: Errno = Errno(24);
    pub const ENOTTY: Errno = Errno(25);
    pub const EFBIG: Errno = Errno(27);
    pub const ENOSPC: Errno = Errno(28);
    pub const ESPIPE: Errno = Errno(29);
    pub const EROFS: Errno = Errno(30);
    pub const EPIPE: Errno = Errno(32);
    pub const ERANGE: Errno = Errno(34);
    pub const ENAMETOOLONG: Errno = Errno(36);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ELOOP: Errno = Errno(40);
    pub const ETIMEDOUT: Errno = Errno(110);
    pub const EOPNOTSUPP: Errno = Errno(95);
    pub const EAFNOSUPPORT: Errno = Errno(97);
    pub const ECONNREFUSED: Errno = Errno(111);
    pub const ENOTCONN: Errno = Errno(107);
    pub const EINPROGRESS: Errno = Errno(115);
    pub const EPROTONOSUPPORT: Errno = Errno(93);
    pub const ENETUNREACH: Errno = Errno(101);
    pub const ENOTSOCK: Errno = Errno(88);
    pub const ECONNRESET: Errno = Errno(104);

    /// The value the guest sees in `x0`.
    pub fn as_return(self) -> u64 {
        (-(self.0 as i64)) as u64
    }

    /// Map a host I/O error onto the closest guest errno.
    pub fn from_io(e: &std::io::Error) -> Errno {
        use std::io::ErrorKind::*;
        match e.kind() {
            NotFound => Errno::ENOENT,
            PermissionDenied => Errno::EACCES,
            AlreadyExists => Errno::EEXIST,
            InvalidInput => Errno::EINVAL,
            BrokenPipe => Errno::EPIPE,
            WouldBlock => Errno::EAGAIN,
            Interrupted => Errno::EINTR,
            _ => match e.raw_os_error() {
                // The host is Linux in every supported native configuration,
                // so a raw errno passes straight through.
                Some(code) if code > 0 => Errno(code),
                _ => Errno::EIO,
            },
        }
    }
}

/// The result of a syscall that produces a value.
pub type SysResult = Result<u64, Errno>;

/// Convert a syscall result to the `x0` the guest expects.
pub fn encode(r: SysResult) -> u64 {
    match r {
        Ok(v) => v,
        Err(e) => e.as_return(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_encode_as_negative() {
        assert_eq!(encode(Err(Errno::ENOENT)) as i64, -2);
        assert_eq!(encode(Err(Errno::ENOSYS)) as i64, -38);
        assert_eq!(encode(Ok(17)), 17);
        // A large successful return must not look like an error.
        assert_eq!(encode(Ok(0x7fff_ffff_ffff_ffff)), 0x7fff_ffff_ffff_ffff);
    }
}
