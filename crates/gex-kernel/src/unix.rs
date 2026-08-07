//! Unix domain sockets, and the descriptor passing that comes with them.
//!
//! These exist because of Wayland. A display server and its clients talk over
//! `AF_UNIX`, and every interesting thing they say involves a file descriptor
//! travelling with the message: a client allocates a shared-memory buffer,
//! draws into it, and sends the *descriptor* so the compositor can look at the
//! same pages. Without `SCM_RIGHTS` there is no Wayland, and without
//! `AF_UNIX` there is no `SCM_RIGHTS`.
//!
//! They are also what a great deal of ordinary Linux software expects: D-Bus,
//! X11, syslog, systemd's notification socket, and `socketpair` as a
//! general-purpose channel between a parent and a child.
//!
//! # What a descriptor is here
//!
//! A passed descriptor is an `Arc<OpenFile>` moved from one process's table to
//! another's. That is what Linux does -- the receiving process gets a
//! reference to the same open file description, not a copy of the file -- and
//! it is what makes shared memory work: two processes mapping the same
//! [`crate::fs::MemFd`] see each other's writes because there is one buffer
//! behind both descriptors.
//!
//! Ancillary data is attached to a *position* in the byte stream rather than
//! to a message, because a stream socket has no messages. Linux delivers a
//! descriptor to whichever `recvmsg` picks up the byte it was sent with, and a
//! reader that reads less than a message leaves the rest of the descriptors
//! for the next call. Wayland depends on this precisely: it reads its
//! fixed-size header first and the descriptors have to still be there.

use crate::errno::{Errno, SysResult};
use crate::file::OpenFile;
use crate::vfs::{FileHandle, FileType, Metadata};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// How much a stream socket buffers before a write has to wait.
///
/// Linux's default is 208 KiB for `AF_UNIX`. The number matters for Wayland:
/// a compositor that stops reading must not deadlock a client that is still
/// sending, and a buffer this size means neither side blocks in practice.
pub const SOCK_BUF: usize = 256 * 1024;

/// One end of a connected pair, or one side of a `socketpair`.
///
/// The two directions are separate queues. Each end writes to one and reads
/// from the other, so a socket is a pair of `Arc<Pipe>` swapped between the
/// ends rather than anything cleverer.
#[derive(Debug, Default)]
struct Stream {
    bytes: VecDeque<u8>,
    /// Descriptors in flight, each with the byte offset they were sent at.
    ///
    /// The offset counts from the start of the connection, not from the head
    /// of `bytes`, so draining the queue does not disturb it.
    fds: VecDeque<(u64, Arc<OpenFile>)>,
    /// Total bytes ever written to this direction.
    written: u64,
    /// Total bytes ever read from it.
    read: u64,
    /// Set when the writing end is gone, so a reader sees end-of-file.
    closed: bool,
}

#[derive(Debug, Default)]
struct Pipe {
    state: Mutex<Stream>,
}

impl Pipe {
    fn send(&self, buf: &[u8], fds: Vec<Arc<OpenFile>>) -> SysResult {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.closed {
            return Err(Errno::EPIPE);
        }
        let room = SOCK_BUF.saturating_sub(s.bytes.len());
        // A send carrying descriptors and no room is not partially done: the
        // descriptors would be attached to a byte that was never written.
        if room == 0 && !buf.is_empty() {
            return Err(Errno::EAGAIN);
        }
        let n = buf.len().min(room);
        // Attach at the first byte of this write, which is where Linux puts
        // them and what a reader taking one byte at a time expects.
        let at = s.written;
        for fd in fds {
            s.fds.push_back((at, fd));
        }
        s.bytes.extend(&buf[..n]);
        s.written += n as u64;
        Ok(n as u64)
    }

    /// Read bytes, and the descriptors that belong to them.
    ///
    /// `max_fds` is how many the caller left room for; the rest stay queued,
    /// which is what Linux does when the control buffer is too small and
    /// `MSG_CMSG_CLOEXEC` is not in play.
    fn recv(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> Result<(u64, Vec<Arc<OpenFile>>), Errno> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.bytes.is_empty() {
            // A descriptor can arrive with no bytes at all only if the sender
            // sent none, which Linux forbids for a stream socket, so an empty
            // buffer means either end-of-file or nothing yet.
            return if s.closed { Ok((0, Vec::new())) } else { Err(Errno::EAGAIN) };
        }
        let n = buf.len().min(s.bytes.len());
        for (i, slot) in buf[..n].iter_mut().enumerate() {
            *slot = s.bytes[i];
        }
        if peek {
            // A peek must not consume descriptors either; report none rather
            // than handing out a reference the next real read would hand out
            // again.
            return Ok((n as u64, Vec::new()));
        }
        s.bytes.drain(..n);
        let upto = s.read + n as u64;
        s.read = upto;
        let mut out = Vec::new();
        while out.len() < max_fds {
            match s.fds.front() {
                Some(&(at, _)) if at < upto => {
                    out.push(s.fds.pop_front().unwrap().1);
                }
                _ => break,
            }
        }
        Ok((n as u64, out))
    }

    fn readable(&self) -> bool {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !s.bytes.is_empty() || s.closed
    }

    fn available(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).bytes.len() as u64
    }

    fn writable(&self) -> bool {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.closed || s.bytes.len() < SOCK_BUF
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    }
}

/// What a socket is at the moment.
#[derive(Debug)]
enum Role {
    /// Made but not yet bound, connected or listening.
    Fresh,
    /// Bound to a path, waiting for `listen`.
    Bound(String),
    /// Listening, with a queue of connections `accept` has not taken.
    Listening {
        path: String,
        backlog: Mutex<VecDeque<Arc<UnixSocket>>>,
    },
    /// Connected: read from `rx`, write to `tx`.
    Connected {
        rx: Arc<Pipe>,
        tx: Arc<Pipe>,
        /// The path this end connected to, for `getpeername`.
        peer: Option<String>,
    },
}

/// A `AF_UNIX` socket.
#[derive(Debug)]
pub struct UnixSocket {
    role: Mutex<Role>,
    nonblocking: AtomicBool,
    /// Sequenced-packet and datagram sockets exist; only streams are
    /// implemented, and the type is kept so the difference can be reported
    /// rather than silently ignored.
    pub stream: bool,
    /// Descriptors handed to `sendmsg` and not yet given to the stream, so
    /// the syscall layer can move them across in one step.
    pending_send: Mutex<Vec<Arc<OpenFile>>>,
    /// Descriptors the last `recvmsg` took off the stream.
    pending_recv: Mutex<Vec<Arc<OpenFile>>>,
}

impl UnixSocket {
    pub fn new(stream: bool) -> UnixSocket {
        UnixSocket {
            role: Mutex::new(Role::Fresh),
            nonblocking: AtomicBool::new(false),
            stream,
            pending_send: Mutex::new(Vec::new()),
            pending_recv: Mutex::new(Vec::new()),
        }
    }

    /// A connected pair, which is `socketpair` and also what `accept`
    /// produces.
    pub fn pair() -> (Arc<UnixSocket>, Arc<UnixSocket>) {
        let a2b = Arc::new(Pipe::default());
        let b2a = Arc::new(Pipe::default());
        let a = UnixSocket::new(true);
        *a.role.lock().unwrap() = Role::Connected {
            rx: Arc::clone(&b2a),
            tx: Arc::clone(&a2b),
            peer: None,
        };
        let b = UnixSocket::new(true);
        *b.role.lock().unwrap() = Role::Connected {
            rx: a2b,
            tx: b2a,
            peer: None,
        };
        (Arc::new(a), Arc::new(b))
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    pub fn is_nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }

    pub fn bind(&self, path: &str) -> SysResult {
        let mut role = self.role.lock().unwrap();
        match *role {
            Role::Fresh => {
                *role = Role::Bound(path.to_string());
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// The path this socket is bound to or listening on.
    pub fn bound_path(&self) -> Option<String> {
        match &*self.role.lock().unwrap() {
            Role::Bound(p) | Role::Listening { path: p, .. } => Some(p.clone()),
            Role::Connected { peer, .. } => peer.clone(),
            Role::Fresh => None,
        }
    }

    pub fn listen(&self) -> SysResult {
        let mut role = self.role.lock().unwrap();
        match &*role {
            Role::Bound(p) => {
                *role = Role::Listening {
                    path: p.clone(),
                    backlog: Mutex::new(VecDeque::new()),
                };
                Ok(0)
            }
            // Listening twice is allowed and only changes the backlog size,
            // which is not modelled.
            Role::Listening { .. } => Ok(0),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Connect to a listener, producing this end's connected state.
    ///
    /// Returns the far end, which the listener will hand to `accept`.
    fn connect_to(&self, listener: &UnixSocket, path: &str) -> SysResult {
        let (mine, theirs) = UnixSocket::pair();
        {
            let role = listener.role.lock().unwrap();
            let Role::Listening { backlog, .. } = &*role else {
                return Err(Errno::ECONNREFUSED);
            };
            backlog.lock().unwrap().push_back(theirs);
        }
        // Take the pair's plumbing rather than the socket itself, because the
        // descriptor the caller holds is this one.
        let mut role = self.role.lock().unwrap();
        let taken = std::mem::replace(&mut *mine.role.lock().unwrap(), Role::Fresh);
        let Role::Connected { rx, tx, .. } = taken else {
            unreachable!("pair() makes connected sockets")
        };
        *role = Role::Connected {
            rx,
            tx,
            peer: Some(path.to_string()),
        };
        Ok(0)
    }

    pub fn accept(&self) -> Result<Arc<UnixSocket>, Errno> {
        let role = self.role.lock().unwrap();
        let taken = match &*role {
            Role::Listening { backlog, .. } => backlog.lock().unwrap().pop_front(),
            _ => return Err(Errno::EINVAL),
        };
        taken.ok_or(Errno::EAGAIN)
    }

    /// Whether `accept` would return without waiting.
    pub fn has_pending(&self) -> bool {
        match &*self.role.lock().unwrap() {
            Role::Listening { backlog, .. } => !backlog.lock().unwrap().is_empty(),
            _ => false,
        }
    }

    /// Descriptors to attach to the next write.
    pub fn stage_send(&self, fds: Vec<Arc<OpenFile>>) {
        *self.pending_send.lock().unwrap() = fds;
    }

    /// Descriptors the last read collected.
    pub fn take_received(&self) -> Vec<Arc<OpenFile>> {
        std::mem::take(&mut *self.pending_recv.lock().unwrap())
    }

    /// Read, keeping any descriptors for [`Self::take_received`].
    pub fn recv(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> SysResult {
        let role = self.role.lock().unwrap();
        let Role::Connected { rx, .. } = &*role else {
            return Err(Errno::ENOTCONN);
        };
        let (n, fds) = rx.recv(buf, max_fds, peek)?;
        if !fds.is_empty() {
            self.pending_recv.lock().unwrap().extend(fds);
        }
        Ok(n)
    }

    pub fn send(&self, buf: &[u8]) -> SysResult {
        let fds = std::mem::take(&mut *self.pending_send.lock().unwrap());
        let role = self.role.lock().unwrap();
        let Role::Connected { tx, .. } = &*role else {
            return Err(Errno::ENOTCONN);
        };
        tx.send(buf, fds)
    }

    pub fn shutdown(&self) {
        if let Role::Connected { tx, .. } = &*self.role.lock().unwrap() {
            tx.close();
        }
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        // The far end must see end-of-file, or a reader waits forever.
        if let Ok(role) = self.role.lock() {
            if let Role::Connected { tx, .. } = &*role {
                tx.close();
            }
        }
    }
}

impl FileHandle for UnixSocket {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        self.recv(buf, 0, false)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        self.send(buf)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Socket,
            size: 0,
            mode: 0o755,
            ino: 1,
            mtime: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        match &*self.role.lock().unwrap() {
            // A listener is "readable" when a connection is waiting, which is
            // how `poll` reports one.
            Role::Listening { backlog, .. } => !backlog.lock().unwrap().is_empty(),
            Role::Connected { rx, .. } => rx.readable(),
            _ => false,
        }
    }

    fn available(&self) -> Option<u64> {
        match &*self.role.lock().unwrap() {
            Role::Connected { rx, .. } => Some(rx.available()),
            _ => Some(0),
        }
    }

    fn poll_writable(&self) -> bool {
        match &*self.role.lock().unwrap() {
            Role::Connected { tx, .. } => tx.writable(),
            _ => false,
        }
    }
}

/// Where listening sockets are found by name.
///
/// A path in the filesystem is how a client finds a server, and both may be
/// in different processes, so this is shared the way the process table is.
/// It is deliberately *not* the VFS: a bound socket is not a file anything
/// reads, and putting one in the filesystem would make it openable.
#[derive(Debug, Default)]
pub struct SocketNames {
    bound: Mutex<Vec<(String, Arc<UnixSocket>)>>,
    /// Sockets the host, rather than a guest process, is listening on. The
    /// compositor is one of these.
    hosted: Mutex<Vec<(String, Arc<dyn HostListener>)>>,
    next_inode: AtomicUsize,
}

/// A listener the host implements.
///
/// The Wayland compositor lives in the host, so a guest client connecting to
/// `$XDG_RUNTIME_DIR/wayland-0` has to reach something that is not a guest
/// process. The host binds a name, and every guest connection to it produces
/// a socket pair whose far end the host drives.
pub trait HostListener: Send + Sync + std::fmt::Debug {
    /// A guest has connected. The far end of its socket is handed over; the
    /// host keeps it and services it.
    fn accept(&self, peer: Arc<UnixSocket>);
}

impl SocketNames {
    pub fn new() -> SocketNames {
        SocketNames::default()
    }

    pub fn bind(&self, path: &str, sock: Arc<UnixSocket>) -> SysResult {
        let mut bound = self.bound.lock().unwrap();
        if bound.iter().any(|(p, _)| p == path) {
            return Err(Errno::EADDRINUSE);
        }
        bound.push((path.to_string(), sock));
        self.next_inode.fetch_add(1, Ordering::Relaxed);
        Ok(0)
    }

    /// Let the host listen on a name.
    pub fn bind_host(&self, path: &str, listener: Arc<dyn HostListener>) {
        let mut hosted = self.hosted.lock().unwrap();
        hosted.retain(|(p, _)| p != path);
        hosted.push((path.to_string(), listener));
    }

    pub fn unbind(&self, path: &str) {
        self.bound.lock().unwrap().retain(|(p, _)| p != path);
    }

    /// Connect `client` to whatever is listening on `path`.
    pub fn connect(&self, client: &UnixSocket, path: &str) -> SysResult {
        if let Some(listener) = self
            .bound
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| Arc::clone(s))
        {
            return client.connect_to(&listener, path);
        }
        let hosted = self
            .hosted
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, l)| Arc::clone(l));
        if let Some(host) = hosted {
            let (mine, theirs) = UnixSocket::pair();
            let taken = std::mem::replace(&mut *mine.role.lock().unwrap(), Role::Fresh);
            *client.role.lock().unwrap() = match taken {
                Role::Connected { rx, tx, .. } => Role::Connected {
                    rx,
                    tx,
                    peer: Some(path.to_string()),
                },
                _ => unreachable!("pair() makes connected sockets"),
            };
            host.accept(theirs);
            return Ok(0);
        }
        // Linux distinguishes "no such file" from "nothing listening", and a
        // client that gets the wrong one retries differently.
        Err(Errno::ECONNREFUSED)
    }

    /// Whether anything is listening on a name, so the VFS can report a
    /// socket at that path.
    pub fn exists(&self, path: &str) -> bool {
        self.bound.lock().unwrap().iter().any(|(p, _)| p == path)
            || self.hosted.lock().unwrap().iter().any(|(p, _)| p == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_file(handle: Arc<dyn FileHandle>) -> Arc<OpenFile> {
        OpenFile::new(handle, "/test".into(), 0, true, true)
    }

    #[test]
    fn a_pair_carries_bytes_both_ways() {
        let (a, b) = UnixSocket::pair();
        assert_eq!(a.send(b"hello").unwrap(), 5);
        let mut buf = [0u8; 8];
        assert_eq!(b.recv(&mut buf, 0, false).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(b.send(b"hi").unwrap(), 2);
        assert_eq!(a.recv(&mut buf, 0, false).unwrap(), 2);
        assert_eq!(&buf[..2], b"hi");
    }

    #[test]
    fn a_descriptor_arrives_with_the_byte_it_was_sent_with() {
        let (a, b) = UnixSocket::pair();
        let (x, _y) = UnixSocket::pair();
        a.stage_send(vec![open_file(x)]);
        a.send(b"AB").unwrap();
        a.send(b"CD").unwrap();

        // Reading the first byte collects the descriptor sent with it.
        let mut one = [0u8; 1];
        b.recv(&mut one, 1, false).unwrap();
        assert_eq!(b.take_received().len(), 1);
        // And nothing further arrives with the rest.
        let mut rest = [0u8; 8];
        b.recv(&mut rest, 1, false).unwrap();
        assert!(b.take_received().is_empty());
    }

    #[test]
    fn a_reader_with_no_room_for_descriptors_leaves_them_queued() {
        let (a, b) = UnixSocket::pair();
        let (x, _y) = UnixSocket::pair();
        a.stage_send(vec![open_file(x)]);
        a.send(b"Z").unwrap();
        let mut buf = [0u8; 1];
        b.recv(&mut buf, 0, false).unwrap();
        assert!(b.take_received().is_empty(), "no room was offered");
    }

    #[test]
    fn peeking_consumes_nothing() {
        let (a, b) = UnixSocket::pair();
        a.send(b"peek").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(b.recv(&mut buf, 0, true).unwrap(), 4);
        assert_eq!(&buf, b"peek");
        // Still there.
        let mut again = [0u8; 4];
        assert_eq!(b.recv(&mut again, 0, false).unwrap(), 4);
        assert_eq!(&again, b"peek");
    }

    #[test]
    fn a_closed_end_reads_as_end_of_file_not_as_waiting() {
        let (a, b) = UnixSocket::pair();
        let mut buf = [0u8; 4];
        assert_eq!(b.recv(&mut buf, 0, false), Err(Errno::EAGAIN));
        drop(a);
        assert_eq!(b.recv(&mut buf, 0, false).unwrap(), 0);
    }

    #[test]
    fn connect_reaches_a_listener_and_accept_hands_back_the_far_end() {
        let names = SocketNames::new();
        let server = Arc::new(UnixSocket::new(true));
        server.bind("/run/thing").unwrap();
        names.bind("/run/thing", Arc::clone(&server)).unwrap();
        server.listen().unwrap();

        let client = UnixSocket::new(true);
        assert!(!server.has_pending());
        names.connect(&client, "/run/thing").unwrap();
        assert!(server.has_pending());

        let session = server.accept().unwrap();
        client.send(b"ping").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(session.recv(&mut buf, 0, false).unwrap(), 4);
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn connecting_to_nothing_is_refused() {
        let names = SocketNames::new();
        let client = UnixSocket::new(true);
        assert_eq!(names.connect(&client, "/run/absent"), Err(Errno::ECONNREFUSED));
    }

    #[test]
    fn a_second_bind_to_the_same_name_is_refused() {
        let names = SocketNames::new();
        let a = Arc::new(UnixSocket::new(true));
        let b = Arc::new(UnixSocket::new(true));
        names.bind("/run/one", a).unwrap();
        assert_eq!(names.bind("/run/one", b), Err(Errno::EADDRINUSE));
    }

    #[test]
    fn a_host_listener_gets_the_far_end() {
        #[derive(Debug, Default)]
        struct Collect(Mutex<Vec<Arc<UnixSocket>>>);
        impl HostListener for Collect {
            fn accept(&self, peer: Arc<UnixSocket>) {
                self.0.lock().unwrap().push(peer);
            }
        }
        let names = SocketNames::new();
        let host = Arc::new(Collect::default());
        names.bind_host("/run/wayland-0", Arc::clone(&host) as Arc<dyn HostListener>);

        let client = UnixSocket::new(true);
        names.connect(&client, "/run/wayland-0").unwrap();
        client.send(b"WAYLAND").unwrap();

        let got = host.0.lock().unwrap();
        assert_eq!(got.len(), 1);
        let mut buf = [0u8; 7];
        assert_eq!(got[0].recv(&mut buf, 0, false).unwrap(), 7);
        assert_eq!(&buf, b"WAYLAND");
    }

    #[test]
    fn a_full_buffer_stops_a_writer_rather_than_growing() {
        let (a, b) = UnixSocket::pair();
        let big = vec![0u8; SOCK_BUF + 4096];
        assert_eq!(a.send(&big).unwrap(), SOCK_BUF as u64);
        assert!(!a.poll_writable());
        let mut drain = vec![0u8; 4096];
        b.recv(&mut drain, 0, false).unwrap();
        assert!(a.poll_writable());
    }
}

/// An anonymous file that lives only in memory: `memfd_create`.
///
/// This is how a Wayland client allocates a buffer. It creates one, sizes it
/// with `ftruncate`, maps it shared, draws into the mapping, and sends the
/// descriptor to the compositor. Nothing ever gives it a name.
///
/// The bytes live here, and a `MAP_SHARED` mapping of one is recorded in the
/// kernel's shared-map list, which is what lets the compositor find the
/// pixels: it looks up the mapping this descriptor has in the guest's address
/// space and reads them out of guest memory directly, with no copy. The
/// `Vec` below is what `read`, `write` and `ftruncate` see before that
/// mapping exists.
#[derive(Debug, Default)]
pub struct MemFd {
    pub name: String,
    data: Mutex<Vec<u8>>,
    /// Seals applied with `F_ADD_SEALS`. See [`seal`].
    ///
    /// Enforced rather than recorded. A caller seals a buffer precisely so it
    /// can stop checking -- Xwayland seals its shared memory against
    /// shrinking so the compositor cannot be made to read off the end of it --
    /// and accepting the seal while ignoring it hands back the guarantee
    /// without the safety.
    seals: Mutex<u32>,
}

/// `F_ADD_SEALS` bits.
pub mod seal {
    /// No more seals may be added.
    pub const SEAL: u32 = 1;
    pub const SHRINK: u32 = 2;
    pub const GROW: u32 = 4;
    pub const WRITE: u32 = 8;
    pub const FUTURE_WRITE: u32 = 0x10;
    pub const ALL: u32 = SEAL | SHRINK | GROW | WRITE | FUTURE_WRITE;
}

impl MemFd {
    pub fn new(name: &str) -> MemFd {
        MemFd {
            name: name.to_string(),
            data: Mutex::new(Vec::new()),
            seals: Mutex::new(0),
        }
    }

    pub fn seals(&self) -> u32 {
        *self.seals.lock().unwrap()
    }

    /// Add seals. Once `F_SEAL_SEAL` is set nothing more may be added, which
    /// is the point of it.
    pub fn add_seals(&self, add: u32) -> SysResult {
        if add & !seal::ALL != 0 {
            return Err(Errno::EINVAL);
        }
        let mut held = self.seals.lock().unwrap();
        if *held & seal::SEAL != 0 {
            return Err(Errno::EPERM);
        }
        *held |= add;
        Ok(0)
    }

    /// Make sure the file is at least this long, for `fallocate`.
    pub fn reserve(&self, upto: u64) -> SysResult {
        let seals = self.seals();
        let mut data = self.data.lock().unwrap();
        if (upto as usize) <= data.len() {
            return Ok(0);
        }
        if seals & seal::GROW != 0 {
            return Err(Errno::EPERM);
        }
        data.resize(upto as usize, 0);
        Ok(0)
    }

    pub fn len(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FileHandle for MemFd {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let data = self.data.lock().unwrap();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n as u64)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let seals = self.seals();
        if seals & (seal::WRITE | seal::FUTURE_WRITE) != 0 {
            return Err(Errno::EPERM);
        }
        let mut data = self.data.lock().unwrap();
        let start = offset as usize;
        if start + buf.len() > data.len() {
            if seals & seal::GROW != 0 {
                return Err(Errno::EPERM);
            }
            data.resize(start + buf.len(), 0);
        }
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(buf.len() as u64)
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let seals = self.seals();
        let mut data = self.data.lock().unwrap();
        let len = len as usize;
        if len < data.len() && seals & seal::SHRINK != 0 {
            return Err(Errno::EPERM);
        }
        if len > data.len() && seals & seal::GROW != 0 {
            return Err(Errno::EPERM);
        }
        data.resize(len, 0);
        Ok(())
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Regular,
            size: self.len(),
            mode: 0o600,
            ino: 0,
            mtime: 0,
        })
    }
}

/// Whole-file advisory locks: `flock`.
///
/// A lock belongs to the *open file description*, not to the descriptor and
/// not to the process, so `dup` and `fork` share one lock and two separate
/// `open`s of the same path contend. That is the rule that makes `flock`
/// usable as a mutex between a parent and a child, which is what most callers
/// want it for.
///
/// Debconf found the absence of this: perl's `flock` returned ENOSYS, and
/// debconf reported it as "locked by another process", so configuring
/// `x11-common` failed and everything depending on it stayed unconfigured.
///
/// One deviation, deliberate and narrow. A blocking request that cannot be
/// granted returns `EWOULDBLOCK` rather than waiting, because waiting needs
/// the syscall to be suspended and resumed and there is nothing here that
/// contends in practice. A spurious `EWOULDBLOCK` is a caller retrying; the
/// alternative -- granting every lock, which is what "always succeed" does --
/// is two processes writing the same file believing they are alone.
#[derive(Debug, Default)]
pub struct FileLocks {
    held: Mutex<Vec<Held>>,
}

#[derive(Debug)]
struct Held {
    /// What is locked. Two `open`s of one path contend; two paths do not.
    file: String,
    /// Who holds it: the open file description, not the process.
    key: usize,
    exclusive: bool,
}

/// `flock`'s operations.
pub mod lock_op {
    pub const SH: u64 = 1;
    pub const EX: u64 = 2;
    pub const NB: u64 = 4;
    pub const UN: u64 = 8;
}

impl FileLocks {
    pub fn new() -> FileLocks {
        FileLocks::default()
    }

    /// Apply an operation on behalf of one open file description.
    ///
    /// `file` names the resource and `key` the owner. Both are needed and
    /// leaving out the first is a mistake worth naming: with the conflict
    /// test written over owners alone, a lock on one file refused a lock on
    /// every other, and debconf -- which locks four unrelated databases in a
    /// row -- got as far as the second.
    pub fn flock(&self, file: &str, key: usize, op: u64) -> SysResult {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if op & lock_op::UN != 0 {
            held.retain(|h| h.key != key);
            return Ok(0);
        }
        let exclusive = op & lock_op::EX != 0;
        if !exclusive && op & lock_op::SH == 0 {
            return Err(Errno::EINVAL);
        }
        let conflict = held
            .iter()
            .any(|h| h.file == file && h.key != key && (exclusive || h.exclusive));
        if conflict {
            return Err(Errno::EWOULDBLOCK);
        }
        // Re-locking the same description converts the mode, which is what
        // Linux does rather than stacking two locks.
        held.retain(|h| !(h.file == file && h.key == key));
        held.push(Held {
            file: file.to_string(),
            key,
            exclusive,
        });
        Ok(0)
    }

    /// Drop everything a description held, when it is closed.
    pub fn release(&self, key: usize) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|h| h.key != key);
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn an_exclusive_lock_excludes_another_description() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::EX), Ok(0));
        assert_eq!(
            l.flock("/a", 2, lock_op::EX | lock_op::NB),
            Err(Errno::EWOULDBLOCK)
        );
        assert_eq!(
            l.flock("/a", 2, lock_op::SH | lock_op::NB),
            Err(Errno::EWOULDBLOCK)
        );
        l.flock("/a", 1, lock_op::UN).unwrap();
        assert_eq!(l.flock("/a", 2, lock_op::EX), Ok(0));
    }

    #[test]
    fn a_lock_on_one_file_says_nothing_about_another() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::EX), Ok(0));
        assert_eq!(l.flock("/b", 2, lock_op::EX), Ok(0));
        assert_eq!(l.flock("/c", 3, lock_op::EX), Ok(0));
    }

    #[test]
    fn shared_locks_coexist() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::SH), Ok(0));
        assert_eq!(l.flock("/a", 2, lock_op::SH), Ok(0));
        assert_eq!(
            l.flock("/a", 3, lock_op::EX | lock_op::NB),
            Err(Errno::EWOULDBLOCK)
        );
    }

    #[test]
    fn the_same_description_may_relock_and_convert() {
        let l = FileLocks::new();
        assert_eq!(l.flock("/a", 1, lock_op::SH), Ok(0));
        assert_eq!(
            l.flock("/a", 1, lock_op::EX),
            Ok(0),
            "converting is not contention"
        );
        assert_eq!(
            l.flock("/a", 2, lock_op::SH | lock_op::NB),
            Err(Errno::EWOULDBLOCK)
        );
    }

    #[test]
    fn closing_releases() {
        let l = FileLocks::new();
        l.flock("/a", 1, lock_op::EX).unwrap();
        l.release(1);
        assert_eq!(l.flock("/a", 2, lock_op::EX), Ok(0));
    }
}

/// An `epoll` set: the descriptors a program is watching, and what it wants
/// to be told about each.
///
/// `poll` hands the kernel its whole list on every call; `epoll` keeps the
/// list in the kernel and the call only asks what has happened. For a program
/// watching a handful of descriptors the two are the same amount of work, and
/// this implementation makes that literal -- an `epoll_wait` turns the
/// registrations back into the list `poll` already knows how to answer.
///
/// It exists because every serious event loop uses it. Xwayland's was the one
/// that forced the issue: `epoll_create1` returned ENOSYS and it stopped with
/// "failed to allocate poll structure" before printing anything else.
#[derive(Debug, Default)]
pub struct EpollSet {
    watched: Mutex<Vec<Watch>>,
}

/// One registration.
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    pub fd: i32,
    pub events: u32,
    /// The opaque word the caller wants handed back, which is how it finds
    /// its own state again. Usually a pointer.
    pub data: u64,
}

/// `epoll_ctl`'s operations.
pub mod epoll_op {
    pub const ADD: u64 = 1;
    pub const DEL: u64 = 2;
    pub const MOD: u64 = 3;
}

impl EpollSet {
    pub fn new() -> EpollSet {
        EpollSet::default()
    }

    pub fn ctl(&self, op: u64, fd: i32, events: u32, data: u64) -> SysResult {
        let mut w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        match op {
            epoll_op::ADD => {
                if w.iter().any(|x| x.fd == fd) {
                    return Err(Errno::EEXIST);
                }
                w.push(Watch { fd, events, data });
                Ok(0)
            }
            epoll_op::MOD => match w.iter_mut().find(|x| x.fd == fd) {
                Some(x) => {
                    x.events = events;
                    x.data = data;
                    Ok(0)
                }
                None => Err(Errno::ENOENT),
            },
            epoll_op::DEL => {
                let before = w.len();
                w.retain(|x| x.fd != fd);
                if w.len() == before {
                    return Err(Errno::ENOENT);
                }
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    pub fn watches(&self) -> Vec<Watch> {
        self.watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl FileHandle for EpollSet {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Regular,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
        })
    }
}

#[cfg(test)]
mod epoll_tests {
    use super::*;

    #[test]
    fn add_modify_and_delete() {
        let e = EpollSet::new();
        assert_eq!(e.ctl(epoll_op::ADD, 3, 1, 0xaa), Ok(0));
        assert_eq!(e.ctl(epoll_op::ADD, 3, 1, 0xbb), Err(Errno::EEXIST));
        assert_eq!(e.ctl(epoll_op::MOD, 3, 4, 0xcc), Ok(0));
        assert_eq!(e.watches()[0].events, 4);
        assert_eq!(e.watches()[0].data, 0xcc);
        assert_eq!(e.ctl(epoll_op::DEL, 3, 0, 0), Ok(0));
        assert!(e.watches().is_empty());
        assert_eq!(e.ctl(epoll_op::DEL, 3, 0, 0), Err(Errno::ENOENT));
        assert_eq!(e.ctl(epoll_op::MOD, 3, 0, 0), Err(Errno::ENOENT));
    }
}
