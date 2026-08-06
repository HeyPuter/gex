//! Pipes.
//!
//! A bounded ring buffer with the blocking rules Linux specifies: a read on
//! an empty pipe waits until data arrives or every writer is gone, a write to
//! a full pipe waits for room, and a write with no readers left raises
//! `EPIPE` (the kernel also sends SIGPIPE, which the signal layer does once
//! it exists).
//!
//! Blocking uses a condition variable, which works because a guest thread is
//! a real host thread (PLAN §2.5). The browser runtime blocks the same way
//! through `Atomics.wait`.

use crate::errno::{Errno, SysResult};
use crate::fs::oflags;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// Linux's pipe capacity.
pub const PIPE_BUF: usize = 65536;

#[derive(Debug, Default)]
struct State {
    buf: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

/// One pipe, shared by both ends.
#[derive(Debug)]
pub struct Pipe {
    state: Mutex<State>,
    ready: Condvar,

}

impl Default for Pipe {
    fn default() -> Self {
        Self::new()
    }
}

impl Pipe {
    pub fn new() -> Pipe {
        Pipe {
            state: Mutex::new(State {
                buf: VecDeque::new(),
                readers: 1,
                writers: 1,
            }),
            ready: Condvar::new(),
        }
    }



    /// Note that another descriptor now refers to one end.
    pub fn open_end(&self, write: bool) {
        let mut s = self.state.lock().unwrap();
        if write {
            s.writers += 1;
        } else {
            s.readers += 1;
        }
    }

    /// Note that a descriptor for one end has closed.
    pub fn close_end(&self, write: bool) {
        let mut s = self.state.lock().unwrap();
        if write {
            s.writers = s.writers.saturating_sub(1);
        } else {
            s.readers = s.readers.saturating_sub(1);
        }
        // Waking both sides is what turns the last close into an EOF or an
        // EPIPE rather than a hang.
        self.ready.notify_all();
    }

    pub fn read(&self, buf: &mut [u8]) -> SysResult {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut s = self.state.lock().unwrap();
        loop {
            if !s.buf.is_empty() {
                let n = buf.len().min(s.buf.len());
                for slot in buf.iter_mut().take(n) {
                    *slot = s.buf.pop_front().unwrap();
                }
                self.ready.notify_all();
                return Ok(n as u64);
            }
            // No data: end of file once no writer can ever supply any.
            if s.writers == 0 {
                return Ok(0);
            }
            // Never wait here. This runs with the kernel locked, and the
            // process that would supply the data needs that lock to write --
            // so waiting is not slow, it is a deadlock. A descriptor that
            // should block is parked by the syscall layer instead, outside
            // the lock, which is what `BlockOp::ReadStream` is for.
            //
            // apt's download helper found this: it read its control pipe,
            // the pipe was momentarily empty, and the whole guest stopped.
            return Err(Errno::EAGAIN);
        }
    }

    pub fn write(&self, buf: &[u8]) -> SysResult {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut s = self.state.lock().unwrap();
        let mut written = 0usize;
        loop {
            if s.readers == 0 {
                // Linux also raises SIGPIPE; with the default disposition the
                // process dies before it ever sees this return value.
                return if written > 0 {
                    Ok(written as u64)
                } else {
                    Err(Errno::EPIPE)
                };
            }
            let room = PIPE_BUF.saturating_sub(s.buf.len());
            if room > 0 {
                let n = room.min(buf.len() - written);
                s.buf.extend(&buf[written..written + n]);
                written += n;
                self.ready.notify_all();
                if written == buf.len() {
                    return Ok(written as u64);
                }
            }
            // Never wait here, for the same reason a read does not: this
            // runs with the kernel locked and the reader that would drain
            // the pipe needs that lock. A partial write is what the caller
            // gets, or `EAGAIN` if nothing fitted, and the syscall layer
            // parks a descriptor that should block.
            return if written > 0 {
                Ok(written as u64)
            } else {
                Err(Errno::EAGAIN)
            };
        }
    }

    /// Bytes available to read, for `poll` and `FIONREAD`.
    pub fn available(&self) -> usize {
        self.state.lock().unwrap().buf.len()
    }

    /// How many bytes a write can take before the pipe is full.
    pub fn room(&self) -> usize {
        PIPE_BUF.saturating_sub(self.state.lock().unwrap().buf.len())
    }

    /// How many bytes are waiting to be read.
    pub fn buffered(&self) -> usize {
        self.state.lock().unwrap().buf.len()
    }

    /// True when a reader would not block: data present, or no writers left.
    pub fn readable(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.buf.is_empty() || s.writers == 0
    }

    /// True when a writer would not block.
    pub fn writable(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.buf.len() < PIPE_BUF || s.readers == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn write_then_read_roundtrips() {
        let p = Pipe::new();
        assert_eq!(p.write(b"hello").unwrap(), 5);
        let mut buf = [0u8; 8];
        assert_eq!(p.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn closing_the_writer_makes_reads_return_eof() {
        let p = Pipe::new();
        p.write(b"ab").unwrap();
        p.close_end(true);
        let mut buf = [0u8; 8];
        assert_eq!(p.read(&mut buf).unwrap(), 2);
        // Drained and no writers: end of file, not a block.
        assert_eq!(p.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn writing_with_no_readers_is_epipe() {
        let p = Pipe::new();
        p.close_end(false);
        assert_eq!(p.write(b"x").err(), Some(Errno::EPIPE));
    }

    #[test]
    fn reads_report_eagain_rather_than_waiting() {
        let p = Pipe::new();
        let mut buf = [0u8; 4];
        assert_eq!(p.read(&mut buf).err(), Some(Errno::EAGAIN));
    }

    #[test]
    fn an_empty_pipe_reports_rather_than_waits() {
        // A read here runs with the kernel locked, and whoever would supply
        // the data needs that lock to write. So an empty pipe says so, and
        // the syscall layer parks the caller outside the lock.
        let p = Arc::new(Pipe::new());
        let mut buf = [0u8; 8];
        assert!(!p.readable());
        assert_eq!(p.read(&mut buf).err(), Some(Errno::EAGAIN));
        p.write(b"late").unwrap();
        assert!(p.readable());
        let n = p.read(&mut buf).unwrap();
        assert_eq!(&buf[..n as usize], b"late");
    }

    #[test]
    fn a_full_pipe_takes_what_fits_and_says_so() {
        // As with a read, waiting here would be a deadlock: the reader that
        // would drain this needs the kernel lock the writer is holding. A
        // short write is what the caller gets, and the syscall layer parks a
        // descriptor that is supposed to block.
        let p = Pipe::new();
        let data = vec![7u8; PIPE_BUF + 1];
        assert_eq!(p.write(&data).unwrap(), PIPE_BUF as u64);
        assert_eq!(p.write(b"x").err(), Some(Errno::EAGAIN));
        let mut buf = vec![0u8; PIPE_BUF];
        assert_eq!(p.read(&mut buf).unwrap(), PIPE_BUF as u64);
        assert_eq!(p.write(b"x").unwrap(), 1);
    }
}
