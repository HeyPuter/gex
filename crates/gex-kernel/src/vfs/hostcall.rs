//! `/dev/gex-host`: a program's call to the page it runs in.
//!
//! A program opens the device, writes one request, `NAME\0JSON` (the JSON
//! may be empty, and the request may come in several writes), then reads the
//! answer from the same descriptor until end of file: a status byte, 0 when
//! the page's function returned and 1 when it failed, then the value as
//! JSON or the error's text. The first read makes the call, and waits for
//! the page in the calling thread only. Each open is one call.
//!
//! From a shell, one descriptor for both:
//! `exec 3<>/dev/gex-host; printf 'notify\0{"t":1}' >&3; cat <&3`.
//!
//! The device is there where the host serves calls: a page's runtime
//! (web/runtime System `hostCalls`), over the channel its mounts use
//! (shims/kernel `k_host_call`).

use super::{FileHandle, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::Mutex;

/// The most a request may hold.
pub const REQUEST_MAX: usize = 128 << 10;

/// How the host makes a call.
#[derive(Clone, Copy)]
pub struct HostCalls {
    /// Make the call `req`: the answer's id, its whole length and how much
    /// of it came in `first`, or an errno.
    pub call: fn(req: &[u8], first: &mut [u8]) -> Result<(u32, u64, usize), Errno>,
    /// More of answer `id`, from `offset`: how much was written.
    pub read: fn(id: u32, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>,
    /// The program is done with answer `id`.
    pub done: fn(id: u32),
}

/// The node's name in `/dev`.
pub const NAME: &str = "gex-host";

pub fn metadata() -> Metadata {
    Metadata {
        file_type: FileType::CharDevice,
        size: 0,
        mode: 0o666,
        ino: 1,
        mtime: 0,
        nlink: 1,
        // A misc device's number (10:240, of the local use range).
        rdev: (10 << 8) | 240,
        times: Default::default(),
    }
}

/// A new open of the device, whose call `host` makes.
pub fn open(host: HostCalls) -> std::sync::Arc<dyn FileHandle> {
    std::sync::Arc::new(HostCallFile { host, state: Mutex::new(State::Asking(Vec::new())) })
}

enum State {
    /// The request so far.
    Asking(Vec<u8>),
    /// The answer: its id, whole length, first part, and how far it is read.
    Answered { id: u32, total: u64, first: Vec<u8>, at: u64 },
}

struct HostCallFile {
    host: HostCalls,
    state: Mutex<State>,
}

impl std::fmt::Debug for HostCallFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostCallFile")
    }
}

impl Drop for HostCallFile {
    fn drop(&mut self) {
        if let State::Answered { id, total, first, .. } = &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            // An answer longer than its first part is kept for the rest.
            if *total > first.len() as u64 {
                (self.host.done)(*id);
            }
        }
    }
}

impl FileHandle for HostCallFile {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let State::Asking(req) = &*st {
            if req.is_empty() {
                return Err(Errno::EINVAL);
            }
            let mut first = vec![0u8; REQUEST_MAX];
            let (id, total, n) = (self.host.call)(req, &mut first)?;
            first.truncate(n.min(first.len()));
            *st = State::Answered { id, total, first, at: 0 };
        }
        let State::Answered { id, total, first, at } = &mut *st else { unreachable!() };
        if *at >= *total || buf.is_empty() {
            return Ok(0);
        }
        let n = if (*at as usize) < first.len() {
            let from = &first[*at as usize..];
            let n = from.len().min(buf.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else {
            let want = ((*total - *at) as usize).min(buf.len());
            (self.host.read)(*id, *at, &mut buf[..want])?
        };
        if n == 0 {
            // The page let it go before it was read: what was read is all.
            *at = *total;
        }
        *at += n as u64;
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *st {
            State::Asking(req) => {
                if req.len() + buf.len() > REQUEST_MAX {
                    return Err(Errno::EFBIG);
                }
                req.extend_from_slice(buf);
                Ok(buf.len() as u64)
            }
            // One call per open.
            State::Answered { .. } => Err(Errno::EINVAL),
        }
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(metadata())
    }

    fn is_stream(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 300-byte answer: the status, then the request again and again.
    fn answer() -> Vec<u8> {
        let mut a = vec![0u8];
        while a.len() < 300 {
            a.extend_from_slice(b"add\0{}");
        }
        a.truncate(300);
        a
    }
    fn call(req: &[u8], first: &mut [u8]) -> Result<(u32, u64, usize), Errno> {
        assert_eq!(req, b"add\0{}");
        // Only 100 bytes come with the call; the rest is read.
        let n = first.len().min(100);
        first[..n].copy_from_slice(&answer()[..n]);
        Ok((7, 300, n))
    }
    fn read(id: u32, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        assert_eq!(id, 7);
        let a = answer();
        let from = &a[offset as usize..];
        let n = from.len().min(buf.len());
        buf[..n].copy_from_slice(&from[..n]);
        Ok(n)
    }
    fn done(id: u32) {
        assert_eq!(id, 7);
    }

    #[test]
    fn a_call_and_its_answer_in_parts() {
        let f = open(HostCalls { call, read, done });
        assert_eq!(f.write_at(0, b"add\0").unwrap(), 4);
        assert_eq!(f.write_at(0, b"{}").unwrap(), 2);
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = f.read_at(0, &mut buf).unwrap() as usize;
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, answer());
        assert_eq!(f.write_at(0, b"x"), Err(Errno::EINVAL));
    }

    #[test]
    fn nothing_asked() {
        let f = open(HostCalls { call, read, done });
        assert_eq!(f.read_at(0, &mut [0u8; 8]), Err(Errno::EINVAL));
    }
}
