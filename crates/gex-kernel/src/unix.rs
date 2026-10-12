//! Unix domain sockets and `SCM_RIGHTS` descriptor passing (needed by Wayland,
//! D-Bus, X11, and `socketpair`).
//!
//! A passed descriptor is an `Arc<OpenFile>` moved between process tables: the
//! receiver shares the open file description, which is what makes a shared
//! [`crate::fs::MemFd`] show both sides' writes.
//!
//! Ancillary data attaches to a byte *position* in the stream, not to a
//! message: a descriptor is delivered to whichever `recvmsg` takes the byte it
//! was sent with. Wayland depends on this -- it reads its fixed header first and
//! the descriptors must still be queued.

use crate::errno::{Errno, SysResult};
use crate::file::OpenFile;
use crate::vfs::{FileHandle, FileType, Metadata};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// How much a stream socket buffers before a write blocks. Sized so a slow
/// Wayland compositor cannot deadlock a client still sending.
pub const SOCK_BUF: usize = 256 * 1024;

/// One direction of a connection. A socket is a pair of `Arc<Pipe>`, each end
/// writing one and reading the other.
#[derive(Debug, Default)]
struct Stream {
    bytes: VecDeque<u8>,
    /// Descriptors in flight, each tagged with the absolute byte offset it was
    /// sent at (from connection start, so draining `bytes` does not disturb it).
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
    /// The wait queues (`crate::wait`) of the end that reads this direction
    /// and of the end that writes it: bytes arriving or the writer going
    /// wakes the one, room freed or the reader going wakes the other.
    reader_q: AtomicU32,
    writer_q: AtomicU32,
}

impl Pipe {
    fn notify_reader(&self) {
        crate::wait::notify(self.reader_q.load(Ordering::SeqCst));
    }

    fn notify_writer(&self) {
        crate::wait::notify(self.writer_q.load(Ordering::SeqCst));
    }

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
        // Attach at the first byte of this write, as Linux does.
        let at = s.written;
        for fd in fds {
            s.fds.push_back((at, fd));
        }
        s.bytes.extend(&buf[..n]);
        s.written += n as u64;
        drop(s);
        if n > 0 {
            self.notify_reader();
        }
        Ok(n as u64)
    }

    /// Read bytes, and the descriptors that belong to them.
    ///
    /// `max_fds` is the room the caller left; the rest stay queued, as Linux
    /// does with a short control buffer.
    fn recv(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> Result<(u64, Vec<Arc<OpenFile>>), Errno> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.bytes.is_empty() {
            // Empty buffer means end-of-file or nothing yet: a stream socket
            // never carries descriptors without bytes.
            return if s.closed { Ok((0, Vec::new())) } else { Err(Errno::EAGAIN) };
        }
        let n = buf.len().min(s.bytes.len());
        for (i, slot) in buf[..n].iter_mut().enumerate() {
            *slot = s.bytes[i];
        }
        if peek {
            // A peek must not consume descriptors, or the next real read hands
            // out the same reference again.
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
        drop(s);
        // Room, for a writer parked on a full buffer.
        self.notify_writer();
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
        self.notify_reader();
        self.notify_writer();
    }

    fn is_closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed
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
    /// Only stream sockets are implemented; the flag is kept so the others can
    /// be reported rather than silently treated as streams.
    pub stream: bool,
    /// Descriptors handed to `sendmsg` and not yet given to the stream, so
    /// the syscall layer can move them across in one step.
    pending_send: Mutex<Vec<Arc<OpenFile>>>,
    /// Descriptors the last `recvmsg` took off the stream.
    pending_recv: Mutex<Vec<Arc<OpenFile>>>,
    /// Who waits on this end (`crate::wait`): readers, writers, and for a
    /// listener `accept`.
    queue: crate::wait::Queue,
}

impl UnixSocket {
    pub fn new(stream: bool) -> UnixSocket {
        UnixSocket {
            role: Mutex::new(Role::Fresh),
            nonblocking: AtomicBool::new(false),
            stream,
            pending_send: Mutex::new(Vec::new()),
            pending_recv: Mutex::new(Vec::new()),
            queue: crate::wait::Queue::new(),
        }
    }

    /// This end's wait queue.
    pub fn queue_id(&self) -> u32 {
        self.queue.id()
    }

    /// Point a connection's two directions at this end's queue: it reads
    /// `rx` and writes `tx`.
    fn adopt(&self, rx: &Pipe, tx: &Pipe) {
        rx.reader_q.store(self.queue.id(), Ordering::SeqCst);
        tx.writer_q.store(self.queue.id(), Ordering::SeqCst);
    }

    /// The queue of the end connected to this one (which reads what this one
    /// writes), or 0.
    pub fn peer_queue(&self) -> u32 {
        match &*self.role.lock().unwrap() {
            Role::Connected { tx, .. } => tx.reader_q.load(Ordering::SeqCst),
            _ => 0,
        }
    }

    /// A host-serviced end (`HostListener`): what its peer writes is moved by
    /// the host on a turn of one of the peer's own waiters (the browser's
    /// compositor pump), so the peer's writes wake the peer's waiters rather
    /// than this end's, which nobody waits on.
    pub fn serviced_by_peer(&self) {
        if let Role::Connected { rx, tx, .. } = &*self.role.lock().unwrap() {
            let peer = tx.reader_q.load(Ordering::SeqCst);
            rx.reader_q.store(peer, Ordering::SeqCst);
            // And room the peer makes by reading is for the host to fill on
            // the peer's next turn: the peer's waiters, again.
            tx.writer_q.store(peer, Ordering::SeqCst);
        }
    }

    /// A connected pair, which is `socketpair` and also what `accept`
    /// produces.
    pub fn pair() -> (Arc<UnixSocket>, Arc<UnixSocket>) {
        let a2b = Arc::new(Pipe::default());
        let b2a = Arc::new(Pipe::default());
        let a = UnixSocket::new(true);
        a.adopt(&b2a, &a2b);
        *a.role.lock().unwrap() = Role::Connected {
            rx: Arc::clone(&b2a),
            tx: Arc::clone(&a2b),
            peer: None,
        };
        let b = UnixSocket::new(true);
        b.adopt(&a2b, &b2a);
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
        // A connection is waiting for `accept`.
        listener.queue.notify();
        // Take the pair's plumbing rather than the socket itself, because the
        // descriptor the caller holds is this one.
        let mut role = self.role.lock().unwrap();
        let taken = std::mem::replace(&mut *mine.role.lock().unwrap(), Role::Fresh);
        let Role::Connected { rx, tx, .. } = taken else {
            unreachable!("pair() makes connected sockets")
        };
        self.adopt(&rx, &tx);
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
        let _ = self.shutdown_how(1);
    }

    /// `shutdown(2)`: 0 stops reading (the peer's writes then fail with
    /// EPIPE), 1 stops writing (the peer reads end-of-file and polls
    /// readable), 2 both. Not connected is `ENOTCONN`.
    pub fn shutdown_how(&self, how: u64) -> SysResult {
        if how > 2 {
            return Err(Errno::EINVAL);
        }
        let Role::Connected { rx, tx, .. } = &*self.role.lock().unwrap() else {
            return Err(Errno::ENOTCONN);
        };
        if how != 1 {
            rx.close();
        }
        if how != 0 {
            tx.close();
        }
        Ok(0)
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        // The far end must see end-of-file, or a reader waits forever; and
        // nobody reads what it sends any more, so its writes fail with EPIPE
        // rather than filling a buffer and then blocking for good. Both
        // directions closed is also what makes it poll as hung up.
        if let Ok(role) = self.role.lock() {
            if let Role::Connected { tx, rx, .. } = &*role {
                tx.close();
                rx.close();
            }
        }
    }
}

impl FileHandle for UnixSocket {
    fn as_unix_socket(self: Arc<Self>) -> Option<Arc<UnixSocket>> {
        Some(self)
    }

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
            ino: crate::vfs::anon_ino(self as *const Self as *const u8),
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        match &*self.role.lock().unwrap() {
            // A listener is "readable" when a connection is waiting.
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

    fn debug_state(&self) -> String {
        match &*self.role.lock().unwrap() {
            Role::Fresh => "fresh".into(),
            Role::Bound(p) => format!("bound {p}"),
            Role::Listening { path, backlog } => {
                format!("listening {path}, {} waiting", backlog.lock().unwrap().len())
            }
            Role::Connected { rx, tx, .. } => {
                format!("connected rx={} tx={}", rx.available(), tx.available())
            }
        }
    }

    /// Hung up once both directions are shut: the peer closed (which shuts
    /// both), or each end shut down its writing side.
    fn poll_hup(&self) -> bool {
        match &*self.role.lock().unwrap() {
            Role::Connected { rx, tx, .. } => rx.is_closed() && tx.is_closed(),
            _ => false,
        }
    }

    fn poll_rdhup(&self) -> bool {
        match &*self.role.lock().unwrap() {
            Role::Connected { rx, .. } => rx.is_closed(),
            _ => false,
        }
    }

    fn wait_queue(&self) -> u32 {
        self.queue.id()
    }

    fn poll_writable(&self) -> bool {
        match &*self.role.lock().unwrap() {
            Role::Connected { tx, .. } => tx.writable(),
            // Not-connected has no buffer to block on, and a non-blocking
            // connect polls for POLLOUT to learn it finished -- so report
            // writable rather than stranding it.
            _ => true,
        }
    }
}

/// Where listening sockets are found by name. Deliberately not the VFS: a bound
/// socket is not a file anything reads, and a VFS entry would make it openable.
#[derive(Debug, Default)]
pub struct SocketNames {
    bound: Mutex<Vec<(String, Arc<UnixSocket>)>>,
    /// Sockets the host (e.g. the compositor), not a guest, listens on.
    hosted: Mutex<Vec<(String, Arc<dyn HostListener>)>>,
    next_inode: AtomicUsize,
}

/// A listener the host implements: the Wayland compositor lives in the host, so
/// a guest connecting to `wayland-0` produces a socket pair whose far end the
/// host drives.
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

    /// Unbind `path` if it is `sock`'s (a socket closing gives up an abstract
    /// name, not one something else bound since).
    pub fn unbind_socket(&self, path: &str, sock: &Arc<UnixSocket>) {
        self.bound.lock().unwrap().retain(|(p, s)| !(p == path && Arc::ptr_eq(s, sock)));
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
                Role::Connected { rx, tx, .. } => {
                    client.adopt(&rx, &tx);
                    Role::Connected { rx, tx, peer: Some(path.to_string()) }
                }
                _ => unreachable!("pair() makes connected sockets"),
            };
            theirs.serviced_by_peer();
            host.accept(theirs);
            return Ok(0);
        }
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

    /// A peer that closes hangs the connection up: POLLHUP (unasked-for) on
    /// the survivor, which still reads what was left and then end-of-file,
    /// and whose writes fail with EPIPE instead of filling a buffer nobody
    /// reads. A half-close is not a hang-up until both halves are shut.
    #[test]
    fn a_closed_peer_hangs_up() {
        use crate::syscalls::{poll_revents, PollFd};
        let pollfd = |h: &Arc<UnixSocket>, events: u16| PollFd {
            addr: 0,
            fd: 3,
            events,
            handle: Some(Arc::clone(h) as Arc<dyn FileHandle>),
            writable: true,
        };
        let (a, b) = UnixSocket::pair();
        b.send(b"last").unwrap();
        assert_eq!(poll_revents(&pollfd(&a, 0)), 0);
        drop(b);
        assert!(a.poll_hup());
        assert_eq!(poll_revents(&pollfd(&a, 0)), 0x010);
        assert_eq!(poll_revents(&pollfd(&a, 0x001)), 0x011);
        let mut buf = [0u8; 8];
        assert_eq!(a.recv(&mut buf, 0, false).unwrap(), 4);
        assert_eq!(a.recv(&mut buf, 0, false).unwrap(), 0);
        assert_eq!(a.send(b"x"), Err(Errno::EPIPE));

        let (a, b) = UnixSocket::pair();
        a.shutdown();
        assert!(!a.poll_hup() && !b.poll_hup(), "one half shut is not a hang-up");
        b.shutdown();
        assert!(a.poll_hup() && b.poll_hup());
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

/// An anonymous in-memory file: `memfd_create`. How a Wayland client allocates
/// a buffer -- create, `ftruncate`, map shared, draw, send the descriptor.
///
/// A `MAP_SHARED` mapping is recorded in the kernel's shared-map list so the
/// compositor can read the pixels out of guest memory with no copy. The `Vec`
/// is what `read`/`write`/`ftruncate` see before such a mapping exists.
#[derive(Debug, Default)]
pub struct MemFd {
    pub name: String,
    data: Mutex<Vec<u8>>,
    /// Seals applied with `F_ADD_SEALS`, enforced rather than recorded:
    /// Xwayland seals shared memory against shrinking so the compositor cannot
    /// read off the end, and an ignored seal hands back the guarantee without
    /// the safety.
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
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }
}

/// Whole-file advisory locks: `flock`.
///
/// A lock belongs to the *open file description*, so `dup`/`fork` share one and
/// two separate `open`s of a path contend -- the rule that makes `flock` a
/// parent/child mutex.
///
/// Deliberate deviation: a blocking request that cannot be granted returns
/// `EWOULDBLOCK` rather than waiting (nothing here contends in practice), since
/// granting it anyway would let two writers believe they are alone.
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

    /// Apply an operation for one open file description. `file` names the
    /// resource, `key` the owner -- both matter, or a lock on one file wrongly
    /// conflicts with every other.
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
        // Re-locking the same description converts the mode, not stacks a lock.
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

/// An `epoll` set: the descriptors a program watches and what it wants told
/// about each. `epoll_wait` turns the registrations back into the list `poll`
/// already knows how to answer.
///
/// The set keeps its own wait queue, chained onto the queue of every
/// descriptor it watches (`crate::wait::chain`): a change on a watched
/// descriptor notifies the set, which notifies the threads in `epoll_wait` on
/// it -- and only them. A change to the interest list itself notifies the set
/// too and moves [`EpollSet::generation`], so a waiter knows its list of
/// descriptors is out of date.
#[derive(Debug, Default)]
pub struct EpollSet {
    watched: Mutex<Vec<Watch>>,
    queue: crate::wait::Queue,
    generation: AtomicU32,
}

/// One registration.
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    pub fd: i32,
    pub events: u32,
    /// Opaque word handed back to the caller (usually a pointer to its state).
    pub data: u64,
    /// The watched descriptor's wait queue this set is chained onto, 0 none.
    pub source: u32,
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
        self.ctl_queue(op, fd, events, data, 0)
    }

    /// `epoll_ctl`, with the watched descriptor's wait queue (`source`) to
    /// chain this set onto.
    pub fn ctl_queue(&self, op: u64, fd: i32, events: u32, data: u64, source: u32) -> SysResult {
        let mut w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        let me = self.queue.id();
        let r = match op {
            epoll_op::ADD => {
                if w.iter().any(|x| x.fd == fd) {
                    return Err(Errno::EEXIST);
                }
                crate::wait::chain(source, me);
                w.push(Watch { fd, events, data, source });
                Ok(0)
            }
            epoll_op::MOD => match w.iter_mut().find(|x| x.fd == fd) {
                Some(x) => {
                    x.events = events;
                    x.data = data;
                    if source != 0 && x.source != source {
                        crate::wait::unchain(x.source, me);
                        crate::wait::chain(source, me);
                        x.source = source;
                    }
                    Ok(0)
                }
                None => Err(Errno::ENOENT),
            },
            epoll_op::DEL => {
                let before = w.len();
                for x in w.iter().filter(|x| x.fd == fd) {
                    crate::wait::unchain(x.source, me);
                }
                w.retain(|x| x.fd != fd);
                if w.len() == before {
                    return Err(Errno::ENOENT);
                }
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        };
        drop(w);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.queue.notify();
        r
    }

    /// Make sure this set is chained onto `source`, the queue descriptor
    /// `fd` has now (a descriptor closed and its number reused names a new
    /// object). Called as a waiter collects the set's descriptors.
    pub fn rechain(&self, fd: i32, source: u32) {
        let mut w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = w.iter_mut().find(|x| x.fd == fd) {
            if x.source != source {
                crate::wait::unchain(x.source, self.queue.id());
                crate::wait::chain(source, self.queue.id());
                x.source = source;
            }
        }
    }

    /// A set already holding `watches`: a fork's child's copy of its
    /// parent's (see [`EpollSet::describe`]). Chained onto no descriptor's
    /// queue yet: a waiter chains each as it collects them (`rechain`).
    pub fn with_watches(watches: Vec<Watch>) -> EpollSet {
        let set = EpollSet::new();
        *set.watched.lock().unwrap_or_else(|e| e.into_inner()) =
            watches.into_iter().map(|w| Watch { source: 0, ..w }).collect();
        set
    }

    /// The registrations as text, `fd,events,data` joined by `;`, for a set
    /// that travels to another process by name.
    pub fn describe(&self) -> String {
        self.watches()
            .iter()
            .map(|w| format!("{},{},{}", w.fd, w.events, w.data))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// The other half of [`EpollSet::describe`]. `None` for text it did not write.
    pub fn parse(text: &str) -> Option<Vec<Watch>> {
        if text.is_empty() {
            return Some(Vec::new());
        }
        text.split(';')
            .map(|w| {
                let mut it = w.split(',');
                let fd = it.next()?.parse().ok()?;
                let events = it.next()?.parse().ok()?;
                let data = it.next()?.parse().ok()?;
                Some(Watch { fd, events, data, source: 0 })
            })
            .collect()
    }

    pub fn watches(&self) -> Vec<Watch> {
        self.watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The set's own wait queue.
    pub fn queue_id(&self) -> u32 {
        self.queue.id()
    }

    /// Moves on every change to the interest list.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::SeqCst)
    }
}

impl Drop for EpollSet {
    fn drop(&mut self) {
        let me = self.queue.id();
        for x in self.watched.get_mut().unwrap_or_else(|e| e.into_inner()).iter() {
            crate::wait::unchain(x.source, me);
        }
    }
}

impl FileHandle for EpollSet {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn wait_queue(&self) -> u32 {
        self.queue.id()
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Regular,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }
}

#[cfg(test)]
mod epoll_tests {
    use super::*;

    #[test]
    fn a_set_travels_by_its_description() {
        let e = EpollSet::new();
        e.ctl(epoll_op::ADD, 3, 1, 0xaa).unwrap();
        e.ctl(epoll_op::ADD, 9, 5, u64::MAX).unwrap();
        let text = e.describe();
        let copy = EpollSet::with_watches(EpollSet::parse(&text).unwrap());
        let w = copy.watches();
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].fd, w[0].events, w[0].data), (3, 1, 0xaa));
        assert_eq!((w[1].fd, w[1].events, w[1].data), (9, 5, u64::MAX));
        assert!(EpollSet::parse("").unwrap().is_empty());
        assert!(EpollSet::parse("x").is_none());
    }

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

/// Unix sockets that live in the global kernel module. With a Worker (and its
/// own memory) per process, the two ends of a socket sit in two memories where
/// the `Arc<Pipe>` above means nothing, so the socket lives in the shared
/// kernel and each end is just an id; only the id and the bytes cross.
#[derive(Clone, Copy, Debug)]
pub struct Remote {
    /// Bind and listen in one. Returns a listener id or a negated errno.
    pub bind: extern "C" fn(path: crate::PAddr, path_len: u32) -> i32,
    /// Connect to a name. Returns this end's id, or a negated errno.
    pub connect: extern "C" fn(path: crate::PAddr, path_len: u32) -> i32,
    /// Take a waiting connection. Returns the server end's id, or -EAGAIN.
    pub accept: extern "C" fn(listener: i32) -> i32,
    pub pending: extern "C" fn(listener: i32) -> i32,
    pub read: extern "C" fn(id: i32, at: crate::PAddr, len: u32) -> i32,
    pub write: extern "C" fn(id: i32, at: crate::PAddr, len: u32) -> i32,
    pub readable: extern "C" fn(id: i32) -> i32,
    pub writable: extern "C" fn(id: i32) -> i32,
    pub available: extern "C" fn(id: i32) -> i32,
    pub close: extern "C" fn(id: i32),
    /// Hang-ups on a connected end: bit 0 both directions shut (`POLLHUP`),
    /// bit 1 the peer's sending side shut (`POLLRDHUP`).
    pub hup: extern "C" fn(id: i32) -> i32,
    /// `shutdown(2)`'s `how` on a connected socket: 0 or a negative errno.
    pub shutdown: extern "C" fn(id: i32, how: u32) -> i32,
    pub exists: extern "C" fn(path: crate::PAddr, path_len: u32) -> i32,
    /// A connected pair, as `socketpair` makes: both ids written as two
    /// little-endian `u32`s at `out`. Zero or a negated errno.
    pub pair: extern "C" fn(out: crate::PAddr) -> i32,
    /// Another id on the same end, for a descriptor that is going to another
    /// process (a fork, or `SCM_RIGHTS`) and must outlive this one closing.
    pub dup: extern "C" fn(id: i32) -> i32,
    /// `send` with descriptors attached: `descs` is one description per
    /// descriptor, newline-separated, as [`Remote::describe`] wrote them. The
    /// global kernel holds each until the far end takes it, and releases what
    /// it holds if the bytes are never read.
    pub write_fds:
        extern "C" fn(id: i32, at: crate::PAddr, len: u32, descs: crate::PAddr, descs_len: u32) -> i32,
    /// `recv` that also takes up to `max_fds` descriptors. Their descriptions
    /// go to `out` as a `u32` length and the newline-separated text; the
    /// in-flight references they stood for are now `pid`'s. Bytes or a
    /// negated errno.
    pub read_fds: extern "C" fn(
        id: i32,
        at: crate::PAddr,
        len: u32,
        max_fds: u32,
        peek: i32,
        pid: i32,
        out: crate::PAddr,
        out_cap: u32,
    ) -> i32,
    /// A descriptor as text another process can rebuild it from, taking
    /// whatever reference keeps it alive in flight; `None` when it is
    /// something that cannot leave this process.
    pub describe: fn(&crate::syscalls::Kernel, &Arc<OpenFile>) -> Option<String>,
    /// The other half of [`Remote::describe`], in the receiving process.
    pub rebuild: fn(&mut crate::syscalls::Kernel, &str) -> Option<Arc<OpenFile>>,
    /// The end's wait queue (`crate::wait`).
    pub queue: extern "C" fn(id: i32) -> u32,
}

/// One end of a socket in the global kernel.
///
/// Dropping closes it there, which is what the far end needs: a reader whose
/// writer's Worker has gone must see end-of-file, not wait forever.
pub struct RemoteUnix {
    pub id: i32,
    /// Set for a listener, so `accept` and `poll` know which question to ask.
    pub listening: bool,
    pub peer: Option<String>,
    pub remote: Remote,
    nonblocking: AtomicBool,
}

impl std::fmt::Debug for RemoteUnix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RemoteUnix({}, {})",
            self.id,
            if self.listening { "listening" } else { "connected" }
        )
    }
}

impl RemoteUnix {
    pub fn new(id: i32, listening: bool, peer: Option<String>, remote: Remote) -> RemoteUnix {
        RemoteUnix {
            id,
            listening,
            peer,
            remote,
            nonblocking: AtomicBool::new(false),
        }
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    pub fn is_nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }

    /// Take a waiting connection, as a handle of the same kind.
    pub fn accept(&self) -> Result<Arc<RemoteUnix>, Errno> {
        if !self.listening {
            return Err(Errno::EINVAL);
        }
        match (self.remote.accept)(self.id) {
            n if n >= 0 => Ok(Arc::new(RemoteUnix::new(n, false, None, self.remote))),
            n => Err(Errno(-n)),
        }
    }

    pub fn has_pending(&self) -> bool {
        self.listening && (self.remote.pending)(self.id) != 0
    }

    /// `shutdown(2)`: as [`UnixSocket::shutdown_how`], in the global kernel.
    pub fn shutdown_how(&self, how: u64) -> SysResult {
        match (self.remote.shutdown)(self.id, how.min(u32::MAX as u64) as u32) {
            n if n < 0 => Err(Errno(-n)),
            _ => Ok(0),
        }
    }

    pub fn bound_path(&self) -> Option<String> {
        self.peer.clone()
    }

    /// Send `buf` with descriptors described as `descs` (newline-separated).
    pub fn send_described(&self, buf: &[u8], descs: &str) -> SysResult {
        if self.listening {
            return Err(Errno::ENOTCONN);
        }
        let n = (self.remote.write_fds)(
            self.id,
            buf.as_ptr() as crate::PAddr,
            buf.len() as u32,
            descs.as_ptr() as crate::PAddr,
            descs.len() as u32,
        );
        if n < 0 {
            return Err(Errno(-n));
        }
        Ok(n as u64)
    }

    /// Receive into `buf` and up to `max_fds` descriptors, which come back
    /// as their descriptions, now held for `pid`.
    pub fn recv_described(
        &self,
        buf: &mut [u8],
        max_fds: usize,
        peek: bool,
        pid: i32,
    ) -> Result<(u64, Vec<String>), Errno> {
        if self.listening {
            return Err(Errno::ENOTCONN);
        }
        let mut out = vec![0u8; 4 + 4096usize.max(300 * max_fds.min(253))];
        let n = (self.remote.read_fds)(
            self.id,
            buf.as_mut_ptr() as crate::PAddr,
            buf.len() as u32,
            max_fds as u32,
            peek as i32,
            pid,
            out.as_mut_ptr() as crate::PAddr,
            out.len() as u32,
        );
        if n < 0 {
            return Err(Errno(-n));
        }
        let len = (u32::from_le_bytes(out[..4].try_into().unwrap()) as usize).min(out.len() - 4);
        let text = String::from_utf8_lossy(&out[4..4 + len]);
        let descs = text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
        Ok((n as u64, descs))
    }

    /// Another handle on the same end, for a descriptor going to another
    /// process; see [`Remote::dup`].
    pub fn dup_id(&self) -> Option<i32> {
        match (self.remote.dup)(self.id) {
            n if n >= 0 => Some(n),
            _ => None,
        }
    }
}

impl Drop for RemoteUnix {
    fn drop(&mut self) {
        (self.remote.close)(self.id);
    }
}

impl crate::vfs::FileHandle for RemoteUnix {
    fn is_global_socket(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if self.listening {
            return Err(Errno::ENOTCONN);
        }
        match (self.remote.read)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if self.listening {
            return Err(Errno::ENOTCONN);
        }
        match (self.remote.write)(self.id, buf.as_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    fn metadata(&self) -> Result<crate::vfs::Metadata, Errno> {
        Ok(crate::vfs::Metadata {
            file_type: crate::vfs::FileType::Socket,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        if self.listening {
            return self.has_pending();
        }
        (self.remote.readable)(self.id) != 0
    }

    fn poll_writable(&self) -> bool {
        self.listening || (self.remote.writable)(self.id) != 0
    }

    fn wait_queue(&self) -> u32 {
        (self.remote.queue)(self.id)
    }

    // Without these a peer's close showed in `poll` and `epoll` only as
    // readable: no `POLLHUP`, no `EPOLLRDHUP`.
    fn poll_hup(&self) -> bool {
        !self.listening && (self.remote.hup)(self.id) & 1 != 0
    }

    fn poll_rdhup(&self) -> bool {
        !self.listening && (self.remote.hup)(self.id) & 2 != 0
    }

    fn available(&self) -> Option<u64> {
        if self.listening {
            return Some(0);
        }
        Some((self.remote.available)(self.id).max(0) as u64)
    }

    fn debug_state(&self) -> String {
        if self.listening {
            format!(
                "shared socket {} listening, {} waiting",
                self.id,
                (self.remote.pending)(self.id)
            )
        } else {
            format!(
                "shared socket {}, {} buffered",
                self.id,
                (self.remote.available)(self.id)
            )
        }
    }
}
