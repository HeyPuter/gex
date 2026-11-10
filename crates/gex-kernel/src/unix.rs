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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// How much a stream socket buffers before a write blocks. Sized so a slow
/// Wayland compositor cannot deadlock a client still sending.
pub const SOCK_BUF: usize = 256 * 1024;

/// One direction of a connection. A socket is a pair of `Arc<Pipe>`, each end
/// writing one and reading the other.
#[derive(Debug, Default)]
struct Stream {
    bytes: VecDeque<u8>,
    /// Descriptors in flight, each tagged with the absolute byte offsets of
    /// the write that carried it, start and end (from connection start, so
    /// draining `bytes` does not disturb them).
    fds: VecDeque<(u64, u64, Arc<OpenFile>)>,
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
    /// The host reads this direction (`UnixSocket::serviced_by_peer`): what
    /// the guest writes waits for the host, so a write says so
    /// ([`take_host_sent`]).
    host_reads: AtomicBool,
    /// How many times each end has been woken ([`Self::notify_reader`],
    /// [`Self::notify_writer`]): the ends' `FileHandle::wakes`.
    reader_wakes: AtomicU64,
    writer_wakes: AtomicU64,
}

/// A guest wrote to (or closed) a host-serviced socket since the last
/// [`take_host_sent`]. Per process in the browser, where each process's
/// compositor is in its own shim.
static HOST_SENT: AtomicBool = AtomicBool::new(false);

/// Whether a guest wrote to, or closed, a socket the host services since
/// the last call: the browser's compositor pumps right then, so a client
/// that keeps running (and so reaches no scheduler turn) has its requests
/// served (X's B2: a closed menu stayed on screen, catching clicks, for
/// seconds while explorer ran).
pub fn take_host_sent() -> bool {
    HOST_SENT.load(Ordering::Relaxed) && HOST_SENT.swap(false, Ordering::AcqRel)
}

impl Pipe {
    fn note_host(&self) {
        if self.host_reads.load(Ordering::Relaxed) {
            HOST_SENT.store(true, Ordering::Release);
        }
    }

    fn notify_reader(&self) {
        self.reader_wakes.fetch_add(1, Ordering::SeqCst);
        crate::wait::notify(self.reader_q.load(Ordering::SeqCst));
    }

    fn notify_writer(&self) {
        self.writer_wakes.fetch_add(1, Ordering::SeqCst);
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
            s.fds.push_back((at, at + n as u64, fd));
        }
        s.bytes.extend(&buf[..n]);
        s.written += n as u64;
        drop(s);
        if n > 0 {
            self.notify_reader();
            self.note_host();
        }
        Ok(n as u64)
    }

    /// Read bytes, and the descriptors that belong to them, as Linux reads a
    /// stream socket: bytes of several writes run together, but a read that
    /// reaches a write carrying descriptors ends with that write, so one
    /// read never holds two writes' descriptors. Of those, `max_fds` (the
    /// room the caller's control buffer has) are handed over and the rest
    /// closed; the third value says some were (`MSG_CTRUNC`).
    fn recv(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> Result<(u64, Vec<Arc<OpenFile>>, bool), Errno> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.bytes.is_empty() {
            // Empty buffer means end-of-file or nothing yet: a stream socket
            // never carries descriptors without bytes.
            return if s.closed { Ok((0, Vec::new(), false)) } else { Err(Errno::EAGAIN) };
        }
        let mut n = buf.len().min(s.bytes.len());
        // The first write with descriptors this read reaches: the read
        // goes no further than its end.
        let group = s.fds.front().map(|&(at, end, _)| (at, end)).filter(|&(at, _)| at < s.read + n as u64);
        if let Some((_, end)) = group {
            n = n.min((end - s.read) as usize);
        }
        for (i, slot) in buf[..n].iter_mut().enumerate() {
            *slot = s.bytes[i];
        }
        if peek {
            // A peek must not consume descriptors, or the next real read hands
            // out the same reference again.
            return Ok((n as u64, Vec::new(), false));
        }
        s.bytes.drain(..n);
        s.read += n as u64;
        let mut out = Vec::new();
        let mut cut = false;
        if let Some((at, _)) = group {
            while s.fds.front().is_some_and(|f| f.0 == at) {
                let (_, _, file) = s.fds.pop_front().unwrap();
                if out.len() < max_fds {
                    out.push(file);
                } else {
                    // Closed: this was the last reference the socket held.
                    cut = true;
                }
            }
        }
        drop(s);
        // Room, for a writer parked on a full buffer.
        self.notify_writer();
        Ok((n as u64, out, cut))
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
        self.note_host();
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
        /// The listener's name, on the end `accept` gave out: `getsockname`.
        local: Option<String>,
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
    /// The credentials this end was made with, as `SO_PEERCRED` hands them
    /// to the far end: the process that called `socketpair`, `connect` or
    /// `listen` on it (an accepted end has its listener's).
    cred: Mutex<Option<Cred>>,
    /// The far end's credentials, taken when the two were joined
    /// (`SO_PEERCRED`): None for a peer this kernel did not see made (a
    /// host's end).
    peer_cred: Mutex<Option<Cred>>,
    /// The IPv4 address and port the connecting process gave for this end
    /// (a page's loopback TCP connection, made through a name here): what
    /// the accepted end's `getpeername` reports.
    addr: Mutex<Option<(u32, u16)>>,
    /// The far end's `addr`, taken when the two were joined.
    peer_addr: Mutex<Option<(u32, u16)>>,
    /// Its last descriptor has closed ([`Self::release`]).
    gone: AtomicBool,
    /// `SO_PASSCRED`: each `recvmsg` says who sent it (`SCM_CREDENTIALS`).
    passcred: AtomicBool,
}

/// A process's credentials as `struct ucred` has them: pid, uid, gid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
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
            cred: Mutex::new(None),
            peer_cred: Mutex::new(None),
            addr: Mutex::new(None),
            peer_addr: Mutex::new(None),
            gone: AtomicBool::new(false),
            passcred: AtomicBool::new(false),
        }
    }

    /// `SO_PASSCRED`, set and read.
    pub fn set_passcred(&self, on: bool) {
        self.passcred.store(on, Ordering::Relaxed);
    }

    pub fn passcred(&self) -> bool {
        self.passcred.load(Ordering::Relaxed)
    }

    /// Record the credentials of the process making this end (before its
    /// `connect` or `listen`, and on both ends of a `socketpair`).
    pub fn set_cred(&self, cred: Cred) {
        *self.cred.lock().unwrap() = Some(cred);
    }

    /// The far end's credentials (`SO_PEERCRED`), when they are known.
    pub fn peer_cred(&self) -> Option<Cred> {
        *self.peer_cred.lock().unwrap()
    }

    /// Record the IPv4 address and port this end connects from (before its
    /// `connect`), for the accepted end's `peer_addr`.
    pub fn set_addr(&self, addr: (u32, u16)) {
        *self.addr.lock().unwrap() = Some(addr);
    }

    /// The far end's IPv4 address and port, when its process gave one.
    pub fn peer_addr(&self) -> Option<(u32, u16)> {
        *self.peer_addr.lock().unwrap()
    }

    /// Both ends of a `socketpair` made by `cred`: each end's peer is that
    /// process, as Linux has it.
    pub fn set_pair_cred(a: &UnixSocket, b: &UnixSocket, cred: Cred) {
        for s in [a, b] {
            s.set_cred(cred);
            *s.peer_cred.lock().unwrap() = Some(cred);
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
            rx.host_reads.store(true, Ordering::SeqCst);
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
            local: None,
        };
        let b = UnixSocket::new(true);
        b.adopt(&a2b, &b2a);
        *b.role.lock().unwrap() = Role::Connected {
            rx: a2b,
            tx: b2a,
            peer: None,
            local: None,
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

    /// Undo a `bind` whose name was taken: a failed bind leaves the socket
    /// unbound, free to bind again.
    pub fn unbind_failed(&self, path: &str) {
        let mut role = self.role.lock().unwrap();
        if matches!(&*role, Role::Bound(p) if p == path) {
            *role = Role::Fresh;
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

    /// The name this end connected to, for `getpeername`: None for an
    /// accepted end (its peer is unnamed); ENOTCONN when not connected.
    pub fn peer_name(&self) -> Result<Option<String>, Errno> {
        match &*self.role.lock().unwrap() {
            Role::Connected { peer, .. } => Ok(peer.clone()),
            _ => Err(Errno::ENOTCONN),
        }
    }

    /// This end's own name, for `getsockname`: the one it is bound to or
    /// listening on, or (an accepted end) its listener's. None when unnamed.
    pub fn local_name(&self) -> Option<String> {
        match &*self.role.lock().unwrap() {
            Role::Bound(p) | Role::Listening { path: p, .. } => Some(p.clone()),
            Role::Connected { local, .. } => local.clone(),
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

    /// Its last descriptor closed (a forked child's copy is one of them): a
    /// listener listens no more, so a connect to a name that stays (a path,
    /// until unlinked) is refused and the connections still waiting for
    /// `accept` go with it, and an abstract name it had is free to bind.
    pub fn release(&self) {
        self.gone.store(true, Ordering::Release);
        let mut role = self.role.lock().unwrap();
        if let Role::Listening { path, .. } = &*role {
            *role = Role::Bound(path.clone());
        }
    }

    /// Whether [`Self::release`] has run.
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
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
            if let Role::Connected { local, .. } = &mut *theirs.role.lock().unwrap() {
                *local = Some(path.to_string());
            }
            // Each end has the other's credentials as they were now: the
            // connecting process's for the end `accept` gives out, the
            // listener's (from its `listen`) for this one.
            let client = *self.cred.lock().unwrap();
            let server = *listener.cred.lock().unwrap();
            *theirs.cred.lock().unwrap() = server;
            *theirs.peer_cred.lock().unwrap() = client;
            *self.peer_cred.lock().unwrap() = server;
            *theirs.peer_addr.lock().unwrap() = *self.addr.lock().unwrap();
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
            local: None,
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

    /// Descriptors to attach to the next write. For a socket with one owner
    /// (the compositor's end of a Wayland connection): between this and the
    /// `send`, anyone else writing the socket takes them. The guest's
    /// `sendmsg` uses [`Self::send_with`], one step.
    pub fn stage_send(&self, fds: Vec<Arc<OpenFile>>) {
        *self.pending_send.lock().unwrap() = fds;
    }

    /// Descriptors the last read collected. One owner only, as
    /// [`Self::stage_send`]; the guest's `recvmsg` uses [`Self::recv_with`].
    pub fn take_received(&self) -> Vec<Arc<OpenFile>> {
        std::mem::take(&mut *self.pending_recv.lock().unwrap())
    }

    /// Read, keeping any descriptors for [`Self::take_received`].
    pub fn recv(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> SysResult {
        let role = self.role.lock().unwrap();
        let Role::Connected { rx, .. } = &*role else {
            return Err(Errno::ENOTCONN);
        };
        let (n, fds, _) = rx.recv(buf, max_fds, peek)?;
        if !fds.is_empty() {
            self.pending_recv.lock().unwrap().extend(fds);
        }
        Ok(n)
    }

    /// Read, and hand back the descriptors that came with the bytes, in one
    /// step: two threads of a process reading one socket (every Wine thread
    /// shares the process's server socket) each get their own message's
    /// descriptors, which [`Self::recv`] then [`Self::take_received`] could
    /// swap.
    pub fn recv_with(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> Result<(u64, Vec<Arc<OpenFile>>), Errno> {
        self.recv_msg(buf, max_fds, peek).map(|(n, fds, _)| (n, fds))
    }

    /// [`Self::recv_with`], also saying whether descriptors that did not
    /// fit in `max_fds` were closed (`recvmsg`'s `MSG_CTRUNC`).
    pub fn recv_msg(&self, buf: &mut [u8], max_fds: usize, peek: bool) -> Result<(u64, Vec<Arc<OpenFile>>, bool), Errno> {
        let role = self.role.lock().unwrap();
        let Role::Connected { rx, .. } = &*role else {
            return Err(Errno::ENOTCONN);
        };
        rx.recv(buf, max_fds, peek)
    }

    /// Write `buf` with `fds` attached to its first byte, in one step: the
    /// guest's `sendmsg`. Staging them first ([`Self::stage_send`]) let a
    /// second thread's `sendmsg` on the same socket replace them or carry them
    /// off with its own bytes in between -- Wine's threads all send their
    /// reply and wait pipes on the one server socket, and the server then
    /// found a thread's `init_thread` with no reply fd ("request 6 without a
    /// reply fd: killed") or with another thread's.
    pub fn send_with(&self, buf: &[u8], fds: Vec<Arc<OpenFile>>) -> SysResult {
        let role = self.role.lock().unwrap();
        let Role::Connected { tx, .. } = &*role else {
            return Err(Errno::ENOTCONN);
        };
        tx.send(buf, fds)
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

    fn wakes(&self) -> Option<u64> {
        // Its own queue (a connection queued, a shutdown) and, connected,
        // the two directions' ends it holds (bytes in, room out).
        let own = self.queue.wakes();
        Some(match &*self.role.lock().unwrap() {
            Role::Connected { rx, tx, .. } => {
                own + rx.reader_wakes.load(Ordering::SeqCst) + tx.writer_wakes.load(Ordering::SeqCst)
            }
            _ => own,
        })
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
    /// Datagram sockets by the name they are bound to ([`UnixDgram`]).
    dgram: Mutex<std::collections::HashMap<String, Weak<UnixDgram>>>,
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
        // An abstract name goes with its socket's last descriptor.
        bound.retain(|(p, s)| !(p.starts_with('\0') && s.is_gone()));
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

    /// The socket file at `from` unlinked (`to` none) or renamed to `to`:
    /// its name goes with the file, as on Linux, where the name is the
    /// file. A socket bound there keeps its connections and its own address
    /// (`getsockname`), and the path can be bound again. A name `to` had is
    /// gone with the file the rename replaced. A renamed directory takes
    /// the names of the sockets under it along.
    pub fn rename_name(&self, from: &str, to: Option<&str>) {
        // `from`'s path in `to`, if it is `from` or under it.
        let moved = |p: &str| -> Option<String> {
            let to = to?;
            if p == from {
                return Some(to.to_string());
            }
            let rest = p.strip_prefix(from)?;
            (rest.starts_with('/') && !from.is_empty()).then(|| format!("{to}{rest}"))
        };
        {
            let mut bound = self.bound.lock().unwrap();
            if let Some(to) = to {
                bound.retain(|(p, _)| p != to);
            }
            match to {
                Some(_) => bound.iter_mut().for_each(|(p, _)| {
                    if let Some(n) = moved(p) {
                        *p = n;
                    }
                }),
                None => bound.retain(|(p, _)| p != from),
            }
        }
        let mut d = self.dgram.lock().unwrap();
        if let Some(to) = to {
            d.remove(to);
            let keys: Vec<String> = d.keys().filter(|k| moved(k).is_some()).cloned().collect();
            for k in keys {
                if let Some(s) = d.remove(&k) {
                    d.insert(moved(&k).unwrap(), s);
                }
            }
        } else {
            d.remove(from);
        }
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
                    Role::Connected { rx, tx, peer: Some(path.to_string()), local: None }
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

    /// The datagram socket bound to `path`: `ECONNREFUSED` when the name is
    /// a stream socket's or its socket has closed, `ENOENT` when nothing has
    /// it (Linux's answers for a name with no socket file).
    pub fn dgram_target(&self, path: &str) -> Result<Arc<UnixDgram>, Errno> {
        let mut d = self.dgram.lock().unwrap();
        match d.get(path).map(|w| w.upgrade()) {
            Some(Some(s)) => Ok(s),
            Some(None) => {
                d.remove(path);
                Err(Errno::ECONNREFUSED)
            }
            None if self.exists(path) => Err(Errno::EPROTOTYPE),
            None => Err(Errno::ENOENT),
        }
    }
}

/// The most a datagram socket holds unread: datagrams, and bytes. A send
/// past either is `EAGAIN` (Linux would block a blocking sender; nothing here
/// fills one that fast).
const DGRAM_QUEUE: usize = 1024;
const DGRAM_BYTES: usize = 4 << 20;

/// An `AF_UNIX` `SOCK_DGRAM` socket: whole messages, each with the name of
/// the socket that sent it, to a name (`sendto`) or to the one socket it is
/// connected to (`connect`, `socketpair`). musl's `if_nametoindex` opens one
/// only to ask an interface ioctl; syslog sends to `/dev/log` through one.
///
/// Names are this kernel's ([`SocketNames`]): natively every process's, in a
/// page those of the process's own Worker, so a page's datagram sockets do
/// not reach another process (its stream sockets do, through the global
/// kernel).
#[derive(Debug)]
pub struct UnixDgram {
    me: Weak<UnixDgram>,
    /// The names it binds in and sends to.
    names: Arc<SocketNames>,
    /// The name it is bound to.
    name: Mutex<Option<String>>,
    /// Where `send` goes: a socketpair's other end, or the socket bound to
    /// the name `connect` gave (the name too, for `getpeername`).
    peer: Mutex<Option<(Weak<UnixDgram>, Option<String>)>>,
    /// Datagrams waiting to be read, each with its sender's name.
    inbox: Mutex<VecDeque<(Vec<u8>, Option<String>)>>,
    nonblocking: AtomicBool,
    /// Who waits on it (`crate::wait`): readers.
    queue: crate::wait::Queue,
}

impl UnixDgram {
    pub fn new(names: &Arc<SocketNames>) -> Arc<UnixDgram> {
        Arc::new_cyclic(|me| UnixDgram {
            me: me.clone(),
            names: Arc::clone(names),
            name: Mutex::new(None),
            peer: Mutex::new(None),
            inbox: Mutex::new(VecDeque::new()),
            nonblocking: AtomicBool::new(false),
            queue: crate::wait::Queue::new(),
        })
    }

    /// `socketpair(AF_UNIX, SOCK_DGRAM)`: two sockets connected to each other.
    pub fn pair(names: &Arc<SocketNames>) -> (Arc<UnixDgram>, Arc<UnixDgram>) {
        let (a, b) = (UnixDgram::new(names), UnixDgram::new(names));
        *a.peer.lock().unwrap() = Some((Arc::downgrade(&b), None));
        *b.peer.lock().unwrap() = Some((Arc::downgrade(&a), None));
        (a, b)
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    pub fn name(&self) -> Option<String> {
        self.name.lock().unwrap().clone()
    }

    /// The name `connect` gave, for `getpeername`; `ENOTCONN` when it was
    /// not connected (a socketpair's end has an unnamed peer: `Ok(None)`).
    pub fn peer_name(&self) -> Result<Option<String>, Errno> {
        match &*self.peer.lock().unwrap() {
            Some((_, name)) => Ok(name.clone()),
            None => Err(Errno::ENOTCONN),
        }
    }

    pub fn bind(&self, path: &str) -> SysResult {
        let names = &self.names;
        let mut name = self.name.lock().unwrap();
        if name.is_some() {
            return Err(Errno::EINVAL);
        }
        let mut d = names.dgram.lock().unwrap();
        d.retain(|_, w| w.strong_count() > 0);
        if d.contains_key(path) || names.exists(path) {
            return Err(Errno::EADDRINUSE);
        }
        d.insert(path.to_string(), self.me.clone());
        *name = Some(path.to_string());
        Ok(0)
    }

    pub fn connect(&self, path: &str) -> SysResult {
        let target = self.names.dgram_target(path)?;
        *self.peer.lock().unwrap() = Some((Arc::downgrade(&target), Some(path.to_string())));
        Ok(0)
    }

    /// Queue `data` for this socket from `from`.
    fn deliver(&self, data: &[u8], from: Option<String>) -> SysResult {
        let mut inbox = self.inbox.lock().unwrap();
        let held: usize = inbox.iter().map(|(d, _)| d.len()).sum();
        if inbox.len() >= DGRAM_QUEUE || held + data.len() > DGRAM_BYTES {
            return Err(Errno::EAGAIN);
        }
        inbox.push_back((data.to_vec(), from));
        drop(inbox);
        self.queue.notify();
        Ok(data.len() as u64)
    }

    /// `sendto`: to the socket bound to `to`, or (`None`) to the connected one.
    pub fn send_to(&self, data: &[u8], to: Option<&str>) -> SysResult {
        if data.len() > DGRAM_BYTES {
            return Err(Errno::EMSGSIZE);
        }
        let target = match to {
            Some(path) => self.names.dgram_target(path)?,
            None => match &*self.peer.lock().unwrap() {
                Some((w, _)) => w.upgrade().ok_or(Errno::ECONNREFUSED)?,
                None => return Err(Errno::ENOTCONN),
            },
        };
        target.deliver(data, self.name())
    }

    /// `recvfrom`: the next datagram (what does not fit in `buf` is lost) and
    /// who sent it; its whole length too, for `MSG_TRUNC`.
    pub fn recv_from(&self, buf: &mut [u8], peek: bool) -> Result<(usize, usize, Option<String>), Errno> {
        let mut inbox = self.inbox.lock().unwrap();
        let (data, from) = if peek {
            inbox.front().cloned().ok_or(Errno::EAGAIN)?
        } else {
            inbox.pop_front().ok_or(Errno::EAGAIN)?
        };
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, data.len(), from))
    }
}

impl Drop for UnixDgram {
    fn drop(&mut self) {
        // Its name is free again (a filesystem name's node stays, as on Linux).
        if let Some(n) = self.name.get_mut().unwrap().take() {
            self.names.dgram.lock().unwrap().retain(|p, w| !(p == &n && w.strong_count() == 0));
        }
        // A socketpair's other end is woken: a reader waiting on it is told
        // the far end is gone (poll_hup) rather than waiting for good.
        if let Some((w, _)) = self.peer.get_mut().unwrap().take() {
            if let Some(p) = w.upgrade() {
                p.queue.notify();
            }
        }
    }
}

impl FileHandle for UnixDgram {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        self.recv_from(buf, false).map(|(n, _, _)| n as u64)
    }

    /// `write` and `send` with no address: to the connected socket. The
    /// syscalls that carry a name call [`UnixDgram::send_to`] themselves.
    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if buf.len() > DGRAM_BYTES {
            return Err(Errno::EMSGSIZE);
        }
        let target = match &*self.peer.lock().unwrap() {
            Some((w, _)) => w.upgrade().ok_or(Errno::ECONNREFUSED)?,
            None => return Err(Errno::ENOTCONN),
        };
        target.deliver(buf, self.name())
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: crate::vfs::FileType::Socket,
            size: 0,
            mode: 0o777,
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
        !self.inbox.lock().unwrap().is_empty()
    }

    fn poll_writable(&self) -> bool {
        true
    }

    fn wait_queue(&self) -> u32 {
        self.queue.id()
    }

    fn wakes(&self) -> Option<u64> {
        Some(self.queue.wakes())
    }

    fn available(&self) -> Option<u64> {
        Some(self.inbox.lock().unwrap().front().map_or(0, |(d, _)| d.len() as u64))
    }

    fn as_unix_dgram(self: Arc<Self>) -> Option<Arc<UnixDgram>> {
        Some(self)
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
            epoll: None,
            edge: false,
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

    /// Descriptors past the room a read leaves are closed and the read says
    /// so (`MSG_CTRUNC`), as Linux does; none stay queued for a later read.
    #[test]
    fn descriptors_without_room_are_closed_not_kept() {
        let (a, b) = UnixSocket::pair();
        let files: Vec<Arc<OpenFile>> = (0..3).map(|_| open_file(UnixSocket::pair().0)).collect();
        let weak: Vec<_> = files.iter().map(Arc::downgrade).collect();
        a.send_with(b"AB", files).unwrap();
        let mut buf = [0u8; 8];
        let (n, got, cut) = b.recv_msg(&mut buf, 1, false).unwrap();
        assert_eq!((n, got.len(), cut), (2, 1, true));
        assert!(weak[1].upgrade().is_none() && weak[2].upgrade().is_none(), "the two without room were kept");
        // A plain read (no room at all) closes what came with its bytes too.
        let lone = open_file(UnixSocket::pair().0);
        let w = Arc::downgrade(&lone);
        a.send_with(b"CD", vec![lone]).unwrap();
        assert_eq!(b.recv(&mut buf, 0, false).unwrap(), 2);
        assert!(w.upgrade().is_none());
        a.send(b"EF").unwrap();
        let (_, got, cut) = b.recv_msg(&mut buf, 4, false).unwrap();
        assert!(got.is_empty() && !cut, "a later read got an earlier write's descriptors");
    }

    /// A read runs plain writes together but ends with the first write that
    /// carries descriptors, so two writes' descriptors never share a read.
    #[test]
    fn a_read_ends_with_the_write_that_carries_descriptors() {
        let (a, b) = UnixSocket::pair();
        a.send(b"AB").unwrap();
        a.send_with(b"CD", vec![open_file(UnixSocket::pair().0)]).unwrap();
        a.send_with(b"EF", vec![open_file(UnixSocket::pair().0)]).unwrap();
        a.send(b"GH").unwrap();
        let mut buf = [0u8; 16];
        let (n, got, _) = b.recv_msg(&mut buf, 4, false).unwrap();
        assert_eq!((&buf[..n as usize], got.len()), (&b"ABCD"[..], 1));
        let (n, got, _) = b.recv_msg(&mut buf, 4, false).unwrap();
        assert_eq!((&buf[..n as usize], got.len()), (&b"EF"[..], 1));
        let (n, got, _) = b.recv_msg(&mut buf, 4, false).unwrap();
        assert_eq!((&buf[..n as usize], got.len()), (&b"GH"[..], 0));
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
    fn concurrent_senders_each_keep_their_own_descriptors() {
        // Wine's shape: many threads, one socket, each sending a message
        // naming itself with its own descriptor.
        let (a, b) = UnixSocket::pair();
        let a = Arc::new(a);
        let threads: Vec<_> = (0..8u8)
            .map(|t| {
                let a = Arc::clone(&a);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        let (x, _y) = UnixSocket::pair();
                        let f = open_file(x);
                        let tag = Arc::as_ptr(&f) as usize;
                        let mut msg = vec![t];
                        msg.extend_from_slice(&tag.to_le_bytes());
                        assert_eq!(a.send_with(&msg, vec![f]).unwrap(), msg.len() as u64);
                    }
                })
            })
            .collect();
        let per = 1 + std::mem::size_of::<usize>();
        let mut got = 0;
        while got < 8 * 500 {
            let mut buf = vec![0u8; per];
            match b.recv_with(&mut buf, 1, false) {
                Ok((n, fds)) => {
                    assert_eq!(n as usize, per, "a message split");
                    assert_eq!(fds.len(), 1, "message {got} came with {} descriptors", fds.len());
                    let tag = usize::from_le_bytes(buf[1..].try_into().unwrap());
                    assert_eq!(Arc::as_ptr(&fds[0]) as usize, tag, "another sender's descriptor");
                    got += 1;
                }
                Err(Errno::EAGAIN) => std::thread::yield_now(),
                Err(e) => panic!("{e:?}"),
            }
        }
        for t in threads {
            t.join().unwrap();
        }
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
    fn as_memfd(self: Arc<Self>) -> Option<Arc<MemFd>> {
        Some(self)
    }

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

/// Advisory locks, `flock`'s and `fcntl`'s ([`crate::locks`]).
pub use crate::locks::{lock_op, FileLocks};

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
    /// The open file each watch is of (and whether it was opened for
    /// writing): the one `epoll_ctl` added, or for a set that came without
    /// them (a fork's copy), the one a waiter first found under the number.
    /// A watch is of that file and not of its number, as on Linux: it goes
    /// once the file is closed, and a new file given the number is not it.
    files: Mutex<Vec<(i32, Weak<dyn FileHandle>, bool)>>,
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
    /// Edge-triggered (`EPOLLET`): the descriptor's wakeup count
    /// (`FileHandle::wakes`) when this watch last reported, `None` when it
    /// has not since it was added or changed. It reports again only once
    /// that count has moved.
    pub seen: Option<u64>,
    /// `EPOLLONESHOT`: false once it has reported, until `EPOLL_CTL_MOD`
    /// arms it again.
    pub armed: bool,
}

/// `epoll_event.events` bits that are not readiness.
pub mod epoll_flag {
    pub const ET: u32 = 1 << 31;
    pub const ONESHOT: u32 = 1 << 30;
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
                w.push(Watch { fd, events, data, source, seen: None, armed: true });
                Ok(0)
            }
            epoll_op::MOD => match w.iter_mut().find(|x| x.fd == fd) {
                Some(x) => {
                    x.events = events;
                    x.data = data;
                    x.seen = None;
                    x.armed = true;
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
                self.files.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.0 != fd);
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

    /// `epoll_ctl` on `fd`, which is `file` now (`None`: not open). A
    /// watch under the number that is of another file, or of one since
    /// closed, is gone first: adding the number again is a new watch, and
    /// changing or deleting it finds none.
    pub fn ctl_file(&self, op: u64, fd: i32, events: u32, data: u64, file: Option<(&Arc<dyn FileHandle>, bool)>) -> SysResult {
        if let Some((now, _)) = file {
            if let Some(Some(was)) = self.owner_weak(fd) {
                let same = was.upgrade().is_some_and(|h| core::ptr::eq(Arc::as_ptr(&h) as *const (), Arc::as_ptr(now) as *const ()));
                if !same {
                    self.forget(fd);
                }
            }
        }
        let source = file.map_or(0, |(f, _)| f.wait_queue());
        let r = self.ctl_queue(op, fd, events, data, source)?;
        if let (epoll_op::ADD, Some((f, writable))) = (op, file) {
            let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
            files.retain(|x| x.0 != fd);
            files.push((fd, Arc::downgrade(f), writable));
        }
        Ok(r)
    }

    /// The file `fd`'s watch is of: `None` when the set has not been told
    /// one, `Some(None)` when it was closed.
    fn owner_weak(&self, fd: i32) -> Option<Option<Weak<dyn FileHandle>>> {
        let files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        files.iter().find(|x| x.0 == fd).map(|x| Some(x.1.clone()))
    }

    /// The open file `fd`'s watch is of and whether it is writable: `None`
    /// when the set has not been told one, `Some(None)` once it is closed.
    pub fn owner(&self, fd: i32) -> Option<Option<(Arc<dyn FileHandle>, bool)>> {
        let files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        files.iter().find(|x| x.0 == fd).map(|x| x.1.upgrade().map(|h| (h, x.2)))
    }

    /// Drop `fd`'s watch: its file is closed, or the number is another's.
    pub fn forget(&self, fd: i32) {
        let mut w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        let me = self.queue.id();
        for x in w.iter().filter(|x| x.fd == fd) {
            crate::wait::unchain(x.source, me);
        }
        w.retain(|x| x.fd != fd);
        drop(w);
        self.files.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.0 != fd);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.queue.notify();
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

    /// `fd`'s open file, for a watch the set was not told the file of (a
    /// fork's copy); chains the set onto its queue as [`EpollSet::rechain`]
    /// does. A watch that has a file keeps it.
    pub fn note_file(&self, fd: i32, file: &Arc<dyn FileHandle>, writable: bool) {
        let mut f = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if f.iter().any(|x| x.0 == fd) {
            return;
        }
        f.push((fd, Arc::downgrade(file), writable));
        drop(f);
        self.rechain(fd, file.wait_queue());
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
                Some(Watch { fd, events, data, source: 0, seen: None, armed: true })
            })
            .collect()
    }

    pub fn watches(&self) -> Vec<Watch> {
        self.watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether `fd`'s watch may report what its descriptor has ready now
    /// (`wakes` its wakeup count, read before its readiness): a disarmed
    /// one-shot watch may not; an edge-triggered one only if the count moved
    /// since it last reported (or it never has), or the count is unknown
    /// (`None`: answered as level-triggered); any other may.
    pub fn may_report(&self, fd: i32, wakes: Option<u64>) -> bool {
        let w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        let Some(x) = w.iter().find(|x| x.fd == fd) else {
            return true;
        };
        if !x.armed {
            return false;
        }
        if x.events & epoll_flag::ET == 0 {
            return true;
        }
        match wakes {
            None => true,
            Some(k) => x.seen != Some(k),
        }
    }

    /// `fd`'s watch has reported, at wakeup count `wakes`.
    pub fn reported(&self, fd: i32, wakes: Option<u64>) {
        let mut w = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = w.iter_mut().find(|x| x.fd == fd) {
            if x.events & epoll_flag::ET != 0 {
                x.seen = wakes;
            }
            if x.events & epoll_flag::ONESHOT != 0 {
                x.armed = false;
            }
        }
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
    fn as_epoll(self: Arc<Self>) -> Option<Arc<EpollSet>> {
        Some(self)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn wait_queue(&self) -> u32 {
        self.queue.id()
    }

    /// Readable when a watch has something to report, as `epoll_wait` would
    /// return it, not always: a set in a `poll` loop (GLib's main loop
    /// polling an epoll descriptor, nested sets) otherwise spins a core.
    /// The poll path tells the set its files before it asks
    /// (`note_epoll_sets`), so a watch with none is one whose descriptor is
    /// not open, and it reports nothing.
    fn poll_readable(&self) -> bool {
        let files = self.files.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.watches().iter().any(|x| {
            let Some((_, file, writable)) = files.iter().find(|f| f.0 == x.fd) else {
                return false;
            };
            let Some(h) = file.upgrade() else {
                return false;
            };
            let wakes = if x.events & epoll_flag::ET != 0 { h.wakes() } else { None };
            let p = crate::syscalls::PollFd {
                addr: 0,
                fd: x.fd,
                events: (x.events & 0xffff) as u16,
                handle: Some(h),
                writable: *writable,
                epoll: None,
                edge: false,
            };
            crate::syscalls::poll_revents(&p) != 0 && self.may_report(x.fd, wakes)
        })
    }

    /// Never writable, as on Linux.
    fn poll_writable(&self) -> bool {
        false
    }

    fn wakes(&self) -> Option<u64> {
        Some(self.queue.wakes())
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
    /// Bind and listen in one, as the process `pid` with ids `uid` and
    /// `gid` (what a client's `SO_PEERCRED` reports). Returns a listener id
    /// or a negated errno.
    pub bind: extern "C" fn(path: crate::PAddr, path_len: u32, pid: i32, uid: u32, gid: u32) -> i32,
    /// Connect to a name, as `pid`, `uid` and `gid` (what the server's
    /// `SO_PEERCRED` reports), from IPv4 address `addr` and port `port` (a
    /// loopback TCP connection's own end, what the accepted end's
    /// `peer_addr` reports; both 0 for none). Returns this end's id, or a
    /// negated errno.
    pub connect: extern "C" fn(path: crate::PAddr, path_len: u32, pid: i32, uid: u32, gid: u32, addr: u32, port: u32) -> i32,
    /// The far end's `struct ucred` as it was when the two were joined:
    /// pid, uid and gid as three little-endian words at `out`. Zero, or a
    /// negated errno (`ENOTCONN`: no peer, or none known).
    pub peer_cred: extern "C" fn(id: i32, out: crate::PAddr) -> i32,
    /// The far end's IPv4 address and port as its `connect` gave them: two
    /// little-endian words at `out`. Zero, or a negated errno (`ENOTCONN`:
    /// none given).
    pub peer_addr: extern "C" fn(id: i32, out: crate::PAddr) -> i32,
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
    /// A connected pair, as `socketpair` makes, by `pid`, `uid` and `gid`
    /// (each end's peer): both ids written as two little-endian `u32`s at
    /// `out`. Zero or a negated errno.
    pub pair: extern "C" fn(out: crate::PAddr, pid: i32, uid: u32, gid: u32) -> i32,
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
    pub describe: fn(&Arc<OpenFile>) -> Option<String>,
    /// The other half of [`Remote::describe`], in the receiving process.
    pub rebuild: fn(&crate::vfs::Namespace, &str) -> Option<Arc<OpenFile>>,
    /// The end's wait queue (`crate::wait`).
    pub queue: extern "C" fn(id: i32) -> u32,
    /// The socket file at a name unlinked (`to_len` 0) or renamed
    /// ([`SocketNames::rename_name`]).
    pub rename: extern "C" fn(from: crate::PAddr, from_len: u32, to: crate::PAddr, to_len: u32),
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
    /// `SO_PASSCRED` (this process's, as the option is the descriptor's).
    passcred: AtomicBool,
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
    /// The far end's credentials ([`Remote::peer_cred`]), when known.
    pub fn peer_cred(&self) -> Option<Cred> {
        let mut b = [0u8; 12];
        if (self.remote.peer_cred)(self.id, b.as_mut_ptr() as crate::PAddr) != 0 {
            return None;
        }
        let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        Some(Cred { pid: w(0) as i32, uid: w(1), gid: w(2) })
    }

    pub fn new(id: i32, listening: bool, peer: Option<String>, remote: Remote) -> RemoteUnix {
        RemoteUnix {
            id,
            listening,
            peer,
            remote,
            nonblocking: AtomicBool::new(false),
            passcred: AtomicBool::new(false),
        }
    }

    pub fn set_passcred(&self, on: bool) {
        self.passcred.store(on, Ordering::Relaxed);
    }

    pub fn passcred(&self) -> bool {
        self.passcred.load(Ordering::Relaxed)
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

    fn as_remote_unix(self: Arc<Self>) -> Option<Arc<RemoteUnix>> {
        Some(self)
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
