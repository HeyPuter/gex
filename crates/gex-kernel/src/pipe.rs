//! Pipes.
//!
//! A bounded ring buffer with the blocking rules Linux specifies: a read on
//! an empty pipe waits until data arrives or every writer is gone, a write to
//! a full pipe waits for room, and a write with no readers left returns
//! `EPIPE`; the syscall layer raises SIGPIPE for it, as Linux does.
//!
//! Blocking uses a condition variable, which works because a guest thread is
//! a real host thread (PLAN §2.5). The browser runtime blocks the same way
//! through `Atomics.wait`.

use crate::errno::{Errno, SysResult};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// Linux's pipe capacity.
pub const PIPE_BUF: usize = 65536;

#[derive(Debug, Default)]
struct State {
    buf: VecDeque<u8>,
    readers: usize,
    writers: usize,
    /// Short writes (took less than offered) and refused writes (took
    /// nothing). Counted for the stall census.
    short: u64,
    refused: u64,
    /// Writes that put bytes in, and total bytes. For the census: a `0
    /// buffered` pipe could be one never written or one already drained.
    writes: u64,
    written: u64,
    /// The last thing written, truncated. For the census: apt's method
    /// protocol is text lines, so this names where a stalled conversation
    /// stopped.
    last: Vec<u8>,
    /// Writes that found no reader (EPIPE). Distinct from refused (EAGAIN):
    /// here the message is gone, not merely deferred.
    broken: u64,
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
                short: 0,
                refused: 0,
                writes: 0,
                written: 0,
                last: Vec::new(),
                broken: 0,
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
            // Never wait here: this runs with the kernel locked and the writer
            // that would supply data needs that lock, so waiting is a
            // deadlock. A descriptor that should block is parked by the
            // syscall layer outside the lock (`BlockOp::ReadStream`).
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
                // EPIPE; the syscall layer raises SIGPIPE with it, as Linux does.
                s.broken += 1;
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
                s.writes += 1;
                s.written += n as u64;
                // Bounded, and the *first* bytes rather than the last: a
                // protocol line says what it is in its first word.
                s.last.clear();
                s.last.extend(buf[written..written + n].iter().take(160));
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
            if written > 0 {
                s.short += 1;
                return Ok(written as u64);
            }
            s.refused += 1;
            return Err(Errno::EAGAIN);
        }
    }

    /// Short writes, refused writes and writes that found no reader.
    pub fn write_stats(&self) -> (u64, u64) {
        let s = self.state.lock().unwrap();
        // `broken` rides in the high half: a write with nowhere to go is the
        // one of the three that loses a message outright, so it must not be
        // the one that goes unreported for want of a field.
        (s.short + (s.broken << 8), s.refused)
    }

    /// The last message written, printable characters only.
    pub fn last_message(&self) -> String {
        let s = self.state.lock().unwrap();
        s.last
            .iter()
            .map(|&b| match b {
                b'\n' => '|',
                0x20..=0x7e => b as char,
                _ => '.',
            })
            .collect()
    }

    /// How many writes have put bytes in, and how many bytes in all.
    pub fn write_totals(&self) -> (u64, u64) {
        let s = self.state.lock().unwrap();
        (s.writes, s.written)
    }

    /// Open descriptors on each end, as `(readers, writers)`. For the census:
    /// a reader stuck without EOF means the writer count never hit zero.
    pub fn ends(&self) -> (usize, usize) {
        let s = self.state.lock().unwrap();
        (s.readers, s.writers)
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

    /// True when every write end is closed.
    pub fn hung_up(&self) -> bool {
        self.state.lock().unwrap().writers == 0
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

    /// The read end polls as hung up once the last writer is gone, with
    /// data still in it or not; the write end never does.
    #[test]
    fn the_read_end_hangs_up_with_its_last_writer() {
        use crate::file::PipeEnd;
        use crate::vfs::FileHandle;
        let p = Arc::new(Pipe::new());
        let r = PipeEnd { pipe: Arc::clone(&p), write: false };
        let w = PipeEnd { pipe: Arc::clone(&p), write: true };
        p.write(b"x").unwrap();
        assert!(!r.poll_hup());
        p.close_end(true);
        assert!(r.poll_hup() && r.poll_readable());
        assert!(!w.poll_hup());
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

/// A pipe that lives in another wasm module.
///
/// An `Arc<Pipe>` is a pointer, so both ends of such a pipe must share one
/// memory. Here the pipe is instead a *number*: the buffer lives in the global
/// kernel, each end is an id, and bytes cross by being copied between the two
/// processes' memories. So both ends can be Workers that share nothing else.
#[derive(Clone, Copy)]
pub struct Remote {
    /// Make a pipe with both ends open, held by `pid`. Returns its id.
    ///
    /// The pid lets the global kernel release a process's ends via
    /// `k_pipe_forget` when it leaves by a trap (no unwind, so no `Drop`).
    pub new: extern "C" fn(pid: i32) -> i32,
    /// Another descriptor onto one end, as `dup` and `fork` both make.
    pub open_end: extern "C" fn(id: i32, write: i32),
    /// How many descriptors hold each end, as `(readers << 16) | writers`.
    /// Negative when the pipe is gone. For the stall census only.
    pub ends: extern "C" fn(id: i32) -> i32,
    /// Short writes and refused writes, `(short << 16) | refused`.
    pub write_stats: extern "C" fn(id: i32) -> i32,
    /// How many writes have ever put bytes in. Negative when the pipe is gone.
    pub writes: extern "C" fn(id: i32) -> i32,
    /// The last message written, into the caller's memory. Returns its length.
    pub last_message: extern "C" fn(id: i32, at: crate::PAddr, cap: u32) -> i32,
    /// Drop a descriptor. The last one on an end is what a reader sees as EOF.
    pub close_end: extern "C" fn(id: i32, write: i32),
    /// Read into, or write out of, an address in the caller's memory.
    pub read: extern "C" fn(id: i32, at: crate::PAddr, len: u32) -> i32,
    pub write: extern "C" fn(id: i32, at: crate::PAddr, len: u32) -> i32,
    pub readable: extern "C" fn(id: i32) -> i32,
    pub writable: extern "C" fn(id: i32) -> i32,
    pub buffered: extern "C" fn(id: i32) -> i32,
}

/// One end of a pipe in the global kernel. Dropping closes that end there, so
/// a writer whose Worker exited counts as EOF just like a `close`.
pub struct RemotePipeEnd {
    pub id: i32,
    pub write: bool,
    pub remote: Remote,
}

impl std::fmt::Debug for RemotePipeEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RemotePipeEnd({}, {})",
            self.id,
            if self.write { "w" } else { "r" }
        )
    }
}

impl Drop for RemotePipeEnd {
    fn drop(&mut self) {
        (self.remote.close_end)(self.id, self.write as i32);
    }
}

impl crate::vfs::FileHandle for RemotePipeEnd {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if self.write {
            return Err(Errno::EBADF);
        }
        match (self.remote.read)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if !self.write {
            return Err(Errno::EBADF);
        }
        match (self.remote.write)(self.id, buf.as_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    fn metadata(&self) -> Result<crate::vfs::Metadata, Errno> {
        Ok(crate::vfs::Metadata {
            file_type: crate::vfs::FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
            rdev: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// A pipe with nothing in it is not readable, whatever the default says.
    /// The default is "yes", which is right for a file and wrong here.
    fn poll_readable(&self) -> bool {
        self.write || (self.remote.readable)(self.id) != 0
    }

    fn poll_writable(&self) -> bool {
        !self.write || (self.remote.writable)(self.id) != 0
    }

    fn available(&self) -> Option<u64> {
        Some(if self.write {
            0
        } else {
            (self.remote.buffered)(self.id).max(0) as u64
        })
    }

    fn write_room(&self) -> Option<u64> {
        // Not reported: a number here could be invalidated by another Worker
        // between the claim and the write. Short writes are carried by the
        // syscall layer instead.
        None
    }

    fn debug_state(&self) -> String {
        // With the ends: "0 buffered" alone cannot tell a reader stuck with
        // no writers left (a lost wake or miscounted close) from one whose
        // writer simply has not written.
        let e = (self.remote.ends)(self.id);
        let (r, w) = if e < 0 {
            (-1i32, -1i32)
        } else {
            ((e >> 16) & 0xffff, e & 0xffff)
        };
        let ws = (self.remote.write_stats)(self.id);
        let (short, refused) = if ws < 0 { (-1i32, -1i32) } else { ((ws >> 16) & 0xffff, ws & 0xffff) };
        let broken = (short >> 8) & 0xff;
        let short = short & 0xff;
        let wrote = if short > 0 || refused > 0 || broken > 0 {
            format!(", {short} short {refused} refused {broken} broken")
        } else {
            String::new()
        };
        let n = (self.remote.writes)(self.id);
        let mut buf = [0u8; 180];
        let got = (self.remote.last_message)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32);
        let said = if got > 0 {
            format!(", last {:?}", String::from_utf8_lossy(&buf[..got as usize]))
        } else {
            String::new()
        };
        format!(
            "shared pipe {} {}, {} buffered, {r} readers {w} writers, {n} writes ever{wrote}{said}",
            self.id,
            if self.write { "write" } else { "read" },
            (self.remote.buffered)(self.id)
        )
    }
}
