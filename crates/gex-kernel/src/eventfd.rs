//! `eventfd`.
//!
//! A counter with a descriptor, and the one primitive an event loop reaches
//! for that gex did not have: libuv (so Node) creates an
//! `EFD_NONBLOCK|EFD_CLOEXEC` eventfd, adds it to its epoll set, and pokes it
//! to wake the loop from another thread. A daemon that multiplexes with
//! libevent does the same. Without it every one of them fails at
//! initialisation with `ENOSYS`.
//!
//! The object is a single `u64`. A `write` adds its argument to the counter; a
//! `read` returns the counter and clears it, or -- in semaphore mode --
//! returns 1 and decrements. A read of a zero counter waits (or answers
//! `EAGAIN` when non-blocking), and a write that would carry the counter past
//! `u64::MAX - 1` waits likewise. Both waits are the ordinary stream-blocking
//! path: [`poll_readable`]/[`poll_writable`] gate them, so the syscall layer
//! parks on the same machinery a pipe uses and another thread's write releases
//! it.
//!
//! [`poll_readable`]: crate::vfs::FileHandle::poll_readable
//! [`poll_writable`]: crate::vfs::FileHandle::poll_writable

use crate::errno::{Errno, SysResult};
use crate::vfs::{FileHandle, FileType, Metadata};
use std::sync::Mutex;

/// `eventfd2` flag bits, in the guest's `EFD_*` spelling. `EFD_NONBLOCK` and
/// `EFD_CLOEXEC` share their values with `O_NONBLOCK`/`O_CLOEXEC` and are
/// handled as open flags by the caller; only the semaphore bit is the
/// object's own.
pub mod flags {
    /// `EFD_SEMAPHORE`: a read returns 1 and decrements, rather than draining.
    pub const SEMAPHORE: u64 = 1;
}

/// The largest value the counter may hold; a write is refused if it would go
/// past this, exactly as Linux does.
const MAX: u64 = u64::MAX - 1;

/// An `eventfd` object.
#[derive(Debug)]
pub struct EventFd {
    count: Mutex<u64>,
    semaphore: bool,
}

impl EventFd {
    pub fn new(initval: u64, semaphore: bool) -> EventFd {
        EventFd {
            count: Mutex::new(initval),
            semaphore,
        }
    }
}

impl FileHandle for EventFd {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        // The read must take a whole `u64`. A shorter buffer is `EINVAL`, not
        // a short read: the object has no bytes to give in pieces.
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let mut c = self.count.lock().unwrap();
        if *c == 0 {
            // The layer above only calls this once `poll_readable` said yes, or
            // on a non-blocking descriptor; either way an empty counter now is
            // `EAGAIN`. Another thread drained it between the poll and here.
            return Err(Errno::EAGAIN);
        }
        let val = if self.semaphore {
            *c -= 1;
            1
        } else {
            std::mem::replace(&mut *c, 0)
        };
        buf[..8].copy_from_slice(&val.to_le_bytes());
        Ok(8)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let val = u64::from_le_bytes(buf[..8].try_into().unwrap());
        // `u64::MAX` is the one value a write may never carry -- Linux uses it
        // as the "would block forever" sentinel and rejects it outright.
        if val == u64::MAX {
            return Err(Errno::EINVAL);
        }
        let mut c = self.count.lock().unwrap();
        if val > MAX - *c {
            // Adding this would pass the ceiling; the write waits for a reader
            // to drain, or answers `EAGAIN` when non-blocking.
            return Err(Errno::EAGAIN);
        }
        *c += val;
        Ok(8)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        // An anonymous inode. Reported as a fifo because that is the closest
        // real type -- a nameless, stream-like object another thread feeds --
        // and because the browser scheduler keys "a wait another Worker will
        // satisfy" off exactly that, rather than off "a person will type".
        Ok(Metadata {
            file_type: FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        *self.count.lock().unwrap() > 0
    }

    fn poll_writable(&self) -> bool {
        *self.count.lock().unwrap() < MAX
    }

    fn available(&self) -> Option<u64> {
        Some(if *self.count.lock().unwrap() > 0 { 8 } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(e: &EventFd) -> Result<u64, Errno> {
        let mut buf = [0u8; 8];
        e.read_at(0, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn write(e: &EventFd, v: u64) -> SysResult {
        e.write_at(0, &v.to_le_bytes())
    }

    #[test]
    fn a_read_drains_the_counter() {
        let e = EventFd::new(5, false);
        assert!(e.poll_readable());
        assert_eq!(read(&e).unwrap(), 5);
        assert!(!e.poll_readable());
        assert_eq!(read(&e).err(), Some(Errno::EAGAIN));
    }

    #[test]
    fn a_write_adds_to_the_counter() {
        let e = EventFd::new(0, false);
        assert_eq!(write(&e, 3).unwrap(), 8);
        assert_eq!(write(&e, 4).unwrap(), 8);
        assert_eq!(read(&e).unwrap(), 7);
    }

    #[test]
    fn semaphore_mode_returns_one_at_a_time() {
        let e = EventFd::new(3, true);
        assert_eq!(read(&e).unwrap(), 1);
        assert_eq!(read(&e).unwrap(), 1);
        assert_eq!(read(&e).unwrap(), 1);
        assert_eq!(read(&e).err(), Some(Errno::EAGAIN));
    }

    #[test]
    fn writing_u64_max_is_rejected() {
        let e = EventFd::new(0, false);
        assert_eq!(write(&e, u64::MAX).err(), Some(Errno::EINVAL));
    }

    #[test]
    fn a_write_past_the_ceiling_would_block() {
        let e = EventFd::new(MAX, false);
        assert!(!e.poll_writable());
        assert_eq!(write(&e, 1).err(), Some(Errno::EAGAIN));
        // But a write that exactly reaches the ceiling is fine.
        let e2 = EventFd::new(MAX - 1, false);
        assert_eq!(write(&e2, 1).unwrap(), 8);
        assert!(!e2.poll_writable());
    }

    #[test]
    fn a_short_buffer_is_einval_both_ways() {
        let e = EventFd::new(1, false);
        let mut small = [0u8; 4];
        assert_eq!(e.read_at(0, &mut small).err(), Some(Errno::EINVAL));
        assert_eq!(e.write_at(0, &[0u8; 4]).err(), Some(Errno::EINVAL));
    }

    #[test]
    fn it_is_a_stream_and_reports_readability() {
        let e = EventFd::new(0, false);
        assert!(e.is_stream());
        assert_eq!(e.available(), Some(0));
        write(&e, 2).unwrap();
        assert_eq!(e.available(), Some(8));
        assert_eq!(e.metadata().unwrap().file_type, FileType::Fifo);
    }
}
