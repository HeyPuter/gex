//! Sockets.
//!
//! A page cannot open TCP; it can talk to a proxy over a WebSocket, and Wisp
//! multiplexes streams down one. So the kernel implements the *shape* of a
//! socket -- descriptors, `connect`, `send`, `recv` and their blocking -- and
//! hands the carrying to the host: a real socket natively, a Wisp stream in a
//! page.
//!
//! DNS is the awkward part: a Wisp stream opens by hostname, but a guest
//! resolves first and connects to an address. So the kernel answers DNS
//! itself, handing back an address from a private range and remembering the
//! name against it; when the guest connects to that address the name comes
//! back and the host is asked for a stream to it.

use gex_abi::GuestEndian;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::errno::{Errno, SysResult};
use crate::vfs::{FileHandle, FileType, Metadata};

/// How a connection is getting on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Asked for, not answered yet.
    Connecting,
    Open,
    /// The peer hung up. Reads drain what is left and then return zero.
    Closed,
    /// It never opened.
    Refused,
}

/// One connection, as the host carries it.
///
/// Everything here is non-blocking. The kernel does the waiting, because the
/// kernel is what knows how to park a guest thread; a host that blocked
/// inside `recv` would take the browser's only wasm stack with it.
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// Begin connecting. Returns as soon as the request is placed.
    fn connect(&self, host: &str, port: u16) -> Result<(), Errno>;
    fn status(&self) -> Status;
    /// Queue bytes. A transport that cannot take them all says how many.
    fn send(&self, data: &[u8]) -> Result<usize, Errno>;
    /// Take what has arrived. `Err(EAGAIN)` when nothing has and the
    /// connection is still open; `Ok(0)` once it is closed and drained.
    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno>;
    /// Whether `recv` would return without waiting -- which includes a closed
    /// connection, because end-of-file is an answer.
    fn has_data(&self) -> bool;
    /// How many bytes are waiting, for `FIONREAD`. A stream may under-report
    /// and the caller simply reads again; the number only has to be right
    /// when it is zero.
    fn available(&self) -> usize {
        usize::from(self.has_data())
    }
    fn close(&self);
}

/// Where a host's connections come from.
pub trait NetStack: Send + Sync + std::fmt::Debug {
    fn tcp(&self) -> Result<Arc<dyn Transport>, Errno>;
}

// -- names ------------------------------------------------------------------

/// The private range the synthetic answers come from. 240.0.0.0/4 is reserved
/// and unroutable, so one of these addresses can only ever be a synthetic one.
const FAKE_BASE: u32 = 0xf000_0001;

/// Names the guest has asked about, and what it was told.
#[derive(Debug, Default)]
pub struct Resolver {
    by_name: Mutex<HashMap<String, u32>>,
    by_addr: Mutex<HashMap<u32, String>>,
    next: AtomicU32,
}

impl Resolver {
    pub fn new() -> Resolver {
        Resolver {
            next: AtomicU32::new(FAKE_BASE),
            ..Default::default()
        }
    }

    /// The address to answer a query for `name` with, allocating one if this
    /// is the first time it has been asked.
    pub fn address_for(&self, name: &str) -> u32 {
        let key = name.trim_end_matches('.').to_ascii_lowercase();
        if let Some(&a) = self.by_name.lock().unwrap().get(&key) {
            return a;
        }
        let a = self.next.fetch_add(1, Ordering::AcqRel);
        self.by_name.lock().unwrap().insert(key.clone(), a);
        self.by_addr.lock().unwrap().insert(a, key);
        a
    }

    /// The name an address was handed out for.
    pub fn name_for(&self, addr: u32) -> Option<String> {
        self.by_addr.lock().unwrap().get(&addr).cloned()
    }
}

/// Build a DNS response for a query.
///
/// Only what a resolver actually sends: one question, class IN, type A or
/// AAAA. An AAAA query is answered with no records rather than an error,
/// which is what tells a dual-stack resolver to ask for A instead -- an
/// error would make it give up on the name entirely.
pub fn answer_dns(query: &[u8], resolver: &Resolver) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }
    // Walk the question's labels.
    let mut i = 12;
    let mut name = String::new();
    loop {
        let len = *query.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        // No compression pointers in a question.
        if len & 0xc0 != 0 || i + len > query.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(&query[i..i + len]).ok()?);
        i += len;
    }
    let qtype = u16::from_be_bytes([*query.get(i)?, *query.get(i + 1)?]);
    let qend = i + 4;
    if qend > query.len() {
        return None;
    }

    let mut out = Vec::with_capacity(qend + 16);
    out.extend_from_slice(&query[..2]); // the id, echoed
    // QR=1 (response), RD copied, RA=1 (recursion available), RCODE=0.
    out.push(0x81);
    out.push(0x80);
    out.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    let answers: u16 = if qtype == 1 { 1 } else { 0 };
    out.extend_from_slice(&answers.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&0u16.to_be_bytes()); // arcount
    out.extend_from_slice(&query[12..qend]); // the question, echoed
    if answers == 1 {
        out.extend_from_slice(&[0xc0, 0x0c]); // a pointer back to the name
        out.extend_from_slice(&1u16.to_be_bytes()); // type A
        out.extend_from_slice(&1u16.to_be_bytes()); // class IN
        out.extend_from_slice(&60u32.to_be_bytes()); // ttl
        out.extend_from_slice(&4u16.to_be_bytes()); // rdlength
        out.extend_from_slice(&resolver.address_for(&name).to_be_bytes());
    }
    Some(out)
}

// -- loopback ---------------------------------------------------------------

/// One direction of a loopback connection.
#[derive(Debug, Default)]
struct LoopBuf {
    data: VecDeque<u8>,
    /// The writing end closed: once drained, the reader sees end of file.
    writer_gone: bool,
    /// The reading end closed: a write is `EPIPE`.
    reader_gone: bool,
}

/// One end of a TCP connection between two guest sockets on this machine
/// (a guest connecting to a port another guest listens on): two byte
/// queues, as a socketpair's.
#[derive(Debug)]
pub struct LoopEnd {
    rx: Arc<Mutex<LoopBuf>>,
    tx: Arc<Mutex<LoopBuf>>,
}

/// Far more than a guest leaves unread in practice; a writer past it gets a
/// short count and waits, as on a full socket buffer.
const LOOP_BUF_MAX: usize = 4 << 20;

impl LoopEnd {
    pub fn pair() -> (Arc<LoopEnd>, Arc<LoopEnd>) {
        let (a, b) = (Arc::new(Mutex::new(LoopBuf::default())), Arc::new(Mutex::new(LoopBuf::default())));
        (Arc::new(LoopEnd { rx: Arc::clone(&a), tx: Arc::clone(&b) }), Arc::new(LoopEnd { rx: b, tx: a }))
    }
}

impl Transport for LoopEnd {
    fn connect(&self, _host: &str, _port: u16) -> Result<(), Errno> {
        Ok(())
    }
    fn status(&self) -> Status {
        if self.rx.lock().unwrap().writer_gone || self.tx.lock().unwrap().reader_gone {
            Status::Closed
        } else {
            Status::Open
        }
    }
    fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        let mut tx = self.tx.lock().unwrap();
        if tx.reader_gone {
            return Err(Errno::EPIPE);
        }
        let n = data.len().min(LOOP_BUF_MAX.saturating_sub(tx.data.len()));
        if n == 0 && !data.is_empty() {
            return Err(Errno::EAGAIN);
        }
        tx.data.extend(&data[..n]);
        Ok(n)
    }
    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut rx = self.rx.lock().unwrap();
        if rx.data.is_empty() {
            return if rx.writer_gone { Ok(0) } else { Err(Errno::EAGAIN) };
        }
        let n = buf.len().min(rx.data.len());
        for (d, s) in buf.iter_mut().zip(rx.data.drain(..n)) {
            *d = s;
        }
        Ok(n)
    }
    fn has_data(&self) -> bool {
        let rx = self.rx.lock().unwrap();
        !rx.data.is_empty() || rx.writer_gone
    }
    fn available(&self) -> usize {
        self.rx.lock().unwrap().data.len()
    }
    fn close(&self) {
        self.tx.lock().unwrap().writer_gone = true;
        self.rx.lock().unwrap().reader_gone = true;
    }
}

/// One end of a TCP connection between guests in different processes of a
/// page: a stream in the global kernel (`unix::Remote`), named by the port
/// (`tcp_name`), each end an id there.
pub struct RemoteEnd {
    id: i32,
    remote: crate::unix::Remote,
}

impl std::fmt::Debug for RemoteEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteEnd({})", self.id)
    }
}

impl Transport for RemoteEnd {
    fn connect(&self, _host: &str, _port: u16) -> Result<(), Errno> {
        Ok(())
    }
    fn status(&self) -> Status {
        Status::Open
    }
    fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        match (self.remote.write)(self.id, data.as_ptr() as crate::PAddr, data.len() as u32) {
            n if n >= 0 => Ok(n as usize),
            n => Err(Errno(-n)),
        }
    }
    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        match (self.remote.read)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as usize),
            n => Err(Errno(-n)),
        }
    }
    fn has_data(&self) -> bool {
        (self.remote.readable)(self.id) != 0
    }
    fn available(&self) -> usize {
        (self.remote.available)(self.id).max(0) as usize
    }
    fn close(&self) {
        (self.remote.close)(self.id);
    }
}

/// The global kernel's name for a TCP port (abstract, so no file appears).
fn tcp_name(port: u16) -> String {
    format!("\0gex-tcp:{port}")
}

/// The TCP ports guests on this machine listen on. Shared across `fork`, as
/// the resolver is: a child connects to its parent's listener.
#[derive(Debug, Default)]
pub struct Ports {
    listeners: Mutex<HashMap<u16, std::sync::Weak<Socket>>>,
    /// Bound UDP sockets, by port: where a datagram to the loopback goes.
    udp: Mutex<HashMap<u16, std::sync::Weak<Socket>>>,
    next_ephemeral: AtomicU32,
}

impl Ports {
    pub fn new() -> Ports {
        Ports::default()
    }

    /// A port nothing listens on, from Linux's ephemeral range.
    fn ephemeral(&self) -> u16 {
        loop {
            let n = self.next_ephemeral.fetch_add(1, Ordering::Relaxed);
            let port = 32768 + (n % 28232) as u16;
            if !self.listening(port) {
                return port;
            }
        }
    }

    fn listening(&self, port: u16) -> bool {
        self.listeners.lock().unwrap().get(&port).is_some_and(|w| w.strong_count() > 0)
    }

    fn listener(&self, port: u16) -> Option<Arc<Socket>> {
        self.listeners.lock().unwrap().get(&port).and_then(|w| w.upgrade())
    }

    fn udp_at(&self, port: u16) -> Option<Arc<Socket>> {
        self.udp.lock().unwrap().get(&port).and_then(|w| w.upgrade())
    }

    /// A UDP port nothing is bound to.
    fn udp_ephemeral(&self) -> u16 {
        loop {
            let p = self.ephemeral();
            if self.udp_at(p).is_none() {
                return p;
            }
        }
    }
}

/// Whether an address is this machine: the loopback network, or the
/// unspecified address, which Linux takes to mean it too.
pub fn is_local(addr: u32) -> bool {
    addr >> 24 == 127 || addr == 0
}

// -- sockets ----------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Tcp,
    /// Only ever used to talk to the resolver, which is this kernel.
    Udp,
}

/// A socket, as a descriptor.
#[derive(Debug)]
pub struct Socket {
    pub kind: Kind,
    net: Arc<dyn NetStack>,
    resolver: Arc<Resolver>,
    transport: Mutex<Option<Arc<dyn Transport>>>,
    /// Where `connect` pointed, for `getpeername`.
    peer: Mutex<Option<(u32, u16)>>,
    /// Datagrams waiting to be read, with where each came from: the
    /// resolver's answers, and datagrams from guests on this machine.
    dgrams: Mutex<VecDeque<(Vec<u8>, (u32, u16))>>,
    /// Where the last datagram read came from, for `recvfrom`.
    last_from: Mutex<Option<(u32, u16)>>,
    /// This socket, for registering itself (a UDP port's table entry).
    me: Mutex<std::sync::Weak<Socket>>,
    nonblocking: AtomicBool,
    /// The machine's listening ports.
    ports: Arc<Ports>,
    /// Where `bind` (or a loopback `connect`) put this end, for
    /// `getsockname`; the port `listen` listens on.
    local: Mutex<Option<(u32, u16)>>,
    /// A listener's connections waiting for `accept`: the far end of each,
    /// and the port it came from. `None` unless listening.
    backlog: Mutex<Option<VecDeque<(Arc<dyn Transport>, u16)>>>,
    /// The global kernel, when there is one (a page: a Worker per process).
    /// Listeners and loopback connections live there, so a guest in another
    /// process can reach them.
    remote: Option<crate::unix::Remote>,
    /// A listener's id in the global kernel.
    remote_listener: Mutex<Option<i32>>,
}

impl Socket {
    pub fn new(kind: Kind, net: Arc<dyn NetStack>, resolver: Arc<Resolver>, ports: Arc<Ports>, remote: Option<crate::unix::Remote>) -> Socket {
        Socket {
            kind,
            net,
            resolver,
            transport: Mutex::new(None),
            peer: Mutex::new(None),
            dgrams: Mutex::new(VecDeque::new()),
            last_from: Mutex::new(None),
            me: Mutex::new(std::sync::Weak::new()),
            nonblocking: AtomicBool::new(false),
            ports,
            local: Mutex::new(None),
            backlog: Mutex::new(None),
            remote,
            remote_listener: Mutex::new(None),
        }
    }

    /// Remember the `Arc` this socket lives in (made right after `new`).
    pub fn set_self(self: &Arc<Self>) {
        *self.me.lock().unwrap() = Arc::downgrade(self);
    }

    /// Where the last datagram read came from.
    pub fn last_from(&self) -> Option<(u32, u16)> {
        *self.last_from.lock().unwrap()
    }

    /// Give this UDP socket a port (`bind`, or the first send unbound),
    /// registered so guests here can send to it.
    fn udp_take(&self, addr: u32, mut port: u16) -> Result<(), Errno> {
        let mut udp = self.ports.udp.lock().unwrap();
        if port == 0 {
            drop(udp);
            port = self.ports.udp_ephemeral();
            udp = self.ports.udp.lock().unwrap();
        }
        if udp.get(&port).is_some_and(|w| w.strong_count() > 0) {
            return Err(Errno::EADDRINUSE);
        }
        udp.insert(port, self.me.lock().unwrap().clone());
        *self.local.lock().unwrap() = Some((addr, port));
        Ok(())
    }

    /// `bind`: where this socket is. A port another listener holds is
    /// `EADDRINUSE` (SO_REUSEADDR is not modelled: a listener that went away
    /// frees its port at once).
    pub fn bind(&self, addr: u32, port: u16) -> Result<(), Errno> {
        if self.local.lock().unwrap().is_some() {
            return Err(Errno::EINVAL);
        }
        if self.kind == Kind::Udp {
            return self.udp_take(addr, port);
        }
        // (In a page the global kernel answers at `listen`: its name table
        // keeps a closed listener's name until the name is bound again.)
        if port != 0 && self.kind == Kind::Tcp && self.remote.is_none() && self.ports.listening(port) {
            return Err(Errno::EADDRINUSE);
        }
        *self.local.lock().unwrap() = Some((addr, port));
        Ok(())
    }

    /// Whether a guest in any process of the page listens on `port`.
    fn remote_taken(&self, port: u16) -> bool {
        self.remote.is_some_and(|r| {
            let n = tcp_name(port);
            (r.exists)(n.as_ptr() as crate::PAddr, n.len() as u32) > 0
        })
    }

    /// Where this end is, for `getsockname`: 0.0.0.0:0 when unbound.
    pub fn local(&self) -> (u32, u16) {
        self.local.lock().unwrap().unwrap_or((0, 0))
    }

    /// `listen`: take the bound port (an ephemeral one if none), and
    /// connections from this machine's guests.
    pub fn listen(self: &Arc<Self>) -> Result<(), Errno> {
        if self.kind != Kind::Tcp || self.transport.lock().unwrap().is_some() {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut backlog = self.backlog.lock().unwrap();
        if backlog.is_some() {
            return Ok(());
        }
        let (addr, mut port) = self.local();
        if let Some(r) = self.remote {
            // In the global kernel: bound and listening in one, as a unix
            // socket there is.
            if port == 0 {
                port = self.ports.ephemeral();
                while self.remote_taken(port) {
                    port = self.ports.ephemeral();
                }
            }
            let n = tcp_name(port);
            let id = (r.bind)(n.as_ptr() as crate::PAddr, n.len() as u32);
            if id < 0 {
                return Err(if -id == Errno::EADDRINUSE.0 || -id == 98 { Errno::EADDRINUSE } else { Errno(-id) });
            }
            *self.remote_listener.lock().unwrap() = Some(id);
            *self.local.lock().unwrap() = Some((addr, port));
            *backlog = Some(VecDeque::new());
            return Ok(());
        }
        let mut listeners = self.ports.listeners.lock().unwrap();
        if port == 0 {
            drop(listeners);
            port = self.ports.ephemeral();
            listeners = self.ports.listeners.lock().unwrap();
        }
        if listeners.get(&port).is_some_and(|w| w.strong_count() > 0) {
            return Err(Errno::EADDRINUSE);
        }
        listeners.insert(port, Arc::downgrade(self));
        *self.local.lock().unwrap() = Some((addr, port));
        *backlog = Some(VecDeque::new());
        Ok(())
    }

    pub fn is_listening(&self) -> bool {
        self.backlog.lock().unwrap().is_some()
    }

    /// `accept`: the next waiting connection as a socket of its own, or
    /// `EAGAIN`; `EINVAL` when not listening.
    pub fn accept(&self) -> Result<Arc<Socket>, Errno> {
        let remote_listener = *self.remote_listener.lock().unwrap();
        let (t, from): (Arc<dyn Transport>, u16) = match (remote_listener, self.remote) {
            (Some(id), Some(r)) => match (r.accept)(id) {
                // The far end's port is the other process's to know.
                n if n >= 0 => (Arc::new(RemoteEnd { id: n, remote: r }), 0),
                n => return Err(Errno(-n)),
            },
            _ => match &mut *self.backlog.lock().unwrap() {
                Some(q) => q.pop_front().ok_or(Errno::EAGAIN)?,
                None => return Err(Errno::EINVAL),
            },
        };
        let s = Socket::new(Kind::Tcp, Arc::clone(&self.net), Arc::clone(&self.resolver), Arc::clone(&self.ports), self.remote);
        *s.transport.lock().unwrap() = Some(t);
        *s.peer.lock().unwrap() = Some((0x7f00_0001, from));
        *s.local.lock().unwrap() = Some((0x7f00_0001, self.local().1));
        let s = Arc::new(s);
        s.set_self();
        Ok(s)
    }

    /// A connection to a port a guest on this machine listens on: both ends
    /// made here, the far one queued for the listener's `accept`.
    fn connect_local(&self, listener: &Socket, port: u16) -> Result<(), Errno> {
        let (mine, theirs) = LoopEnd::pair();
        let from = self.ports.ephemeral();
        match &mut *listener.backlog.lock().unwrap() {
            Some(q) => q.push_back((theirs, from)),
            None => return Err(Errno::ECONNREFUSED),
        }
        *self.peer.lock().unwrap() = Some((0x7f00_0001, port));
        *self.local.lock().unwrap() = Some((0x7f00_0001, from));
        *self.transport.lock().unwrap() = Some(mine);
        Ok(())
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    pub fn peer(&self) -> Option<(u32, u16)> {
        *self.peer.lock().unwrap()
    }

    /// Remember where a datagram went, so the reply can be reported as coming
    /// from there: a resolver drops replies whose source is not the nameserver
    /// it asked, so 0.0.0.0 made every answer look spoofed.
    pub fn set_peer(&self, addr: u32, port: u16) {
        *self.peer.lock().unwrap() = Some((addr, port));
    }

    /// `connect`. Returns `EINPROGRESS` while still connecting; the kernel
    /// turns that into a wait for a blocking descriptor. Idempotent: asking
    /// twice does not open a second connection.
    pub fn connect(&self, addr: u32, port: u16) -> Result<(), Errno> {
        if let Some(t) = self.transport.lock().unwrap().as_ref() {
            return match t.status() {
                Status::Connecting => Err(Errno::EINPROGRESS),
                Status::Open => Ok(()),
                Status::Refused => Err(Errno::ECONNREFUSED),
                Status::Closed => Err(Errno::ECONNRESET),
            };
        }
        // A port a guest here listens on: connected at once, no network
        // involved. (One nothing here listens on still goes out: the host
        // may have a service on its own loopback.)
        if self.kind == Kind::Tcp && is_local(addr) {
            if let Some(r) = self.remote {
                let n = tcp_name(port);
                let id = (r.connect)(n.as_ptr() as crate::PAddr, n.len() as u32);
                if id >= 0 {
                    *self.peer.lock().unwrap() = Some((0x7f00_0001, port));
                    *self.local.lock().unwrap() = Some((0x7f00_0001, self.ports.ephemeral()));
                    *self.transport.lock().unwrap() = Some(Arc::new(RemoteEnd { id, remote: r }));
                    return Ok(());
                }
            } else if let Some(listener) = self.ports.listener(port) {
                return self.connect_local(&listener, port);
            }
        }
        *self.peer.lock().unwrap() = Some((addr, port));
        if self.kind == Kind::Udp {
            // A connected UDP socket only fixes the default destination, and
            // the only destination that means anything here is the resolver.
            return Ok(());
        }
        // A name if this address came from an answer we gave, otherwise the
        // dotted quad -- a guest connecting to a literal address is asking
        // the proxy for exactly that.
        let host = self.resolver.name_for(addr).unwrap_or_else(|| {
            let b = addr.to_be_bytes();
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        });
        let t = self.net.tcp()?;
        t.connect(&host, port)?;
        *self.transport.lock().unwrap() = Some(t);
        Err(Errno::EINPROGRESS)
    }

    pub fn status(&self) -> Status {
        match &*self.transport.lock().unwrap() {
            Some(t) => t.status(),
            None => Status::Closed,
        }
    }

    /// A datagram: to a guest on this machine bound to the port it is for,
    /// else to the resolver, which answers it and queues the reply.
    pub fn send_dgram(&self, data: &[u8]) -> SysResult {
        let to = self.peer();
        if let Some((addr, port)) = to.filter(|&(a, _)| is_local(a)) {
            if let Some(dest) = self.ports.udp_at(port) {
                if self.local.lock().unwrap().is_none() {
                    self.udp_take(0, 0)?;
                }
                let from = (0x7f00_0001, self.local().1);
                let _ = addr;
                dest.dgrams.lock().unwrap().push_back((data.to_vec(), from));
                return Ok(data.len() as u64);
            }
        }
        match answer_dns(data, &self.resolver) {
            Some(reply) => {
                self.dgrams.lock().unwrap().push_back((reply, to.unwrap_or((0, 0))));
                Ok(data.len() as u64)
            }
            // Not a question this can answer. Accepting it and never
            // replying is what a lost packet looks like, and a resolver
            // handles that.
            None => Ok(data.len() as u64),
        }
    }

    pub fn recv_dgram(&self, buf: &mut [u8]) -> SysResult {
        match self.dgrams.lock().unwrap().pop_front() {
            Some((d, from)) => {
                let n = buf.len().min(d.len());
                buf[..n].copy_from_slice(&d[..n]);
                *self.last_from.lock().unwrap() = Some(from);
                Ok(n as u64)
            }
            None => Err(Errno::EAGAIN),
        }
    }
}

impl FileHandle for Socket {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if self.kind == Kind::Udp {
            return self.recv_dgram(buf);
        }
        let t = self.transport.lock().unwrap();
        let Some(t) = t.as_ref() else {
            return Err(Errno::ENOTCONN);
        };
        match t.status() {
            Status::Refused => Err(Errno::ECONNREFUSED),
            // Closed and drained is end of file, not "try again": a transport
            // reports "nothing right now" as EAGAIN and cannot tell an empty
            // connection from a finished one, so `status` decides. Without
            // this a reader spins on EAGAIN forever. Counterpart of `send`
            // answering Closed with EPIPE.
            Status::Closed => match t.recv(buf) {
                Ok(n) => Ok(n as u64),
                Err(Errno::EAGAIN) => Ok(0),
                Err(e) => Err(e),
            },
            _ => t.recv(buf).map(|n| n as u64),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        if self.kind == Kind::Udp {
            return self.send_dgram(buf);
        }
        let t = self.transport.lock().unwrap();
        let Some(t) = t.as_ref() else {
            return Err(Errno::ENOTCONN);
        };
        match t.status() {
            Status::Refused => Err(Errno::ECONNREFUSED),
            Status::Closed => Err(Errno::EPIPE),
            // Still connecting: the bytes cannot go yet, and a caller that
            // waited for the connection would not be here.
            Status::Connecting => Err(Errno::EAGAIN),
            Status::Open => t.send(buf).map(|n| n as u64),
        }
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Socket,
            size: 0,
            mode: 0o600,
            ino: 3,
            mtime: 0,
            nlink: 1,
            rdev: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Bytes available to read, for `FIONREAD`. For a datagram socket this
    /// must be exact -- the length of the next datagram, which a caller uses
    /// to size its buffer.
    fn available(&self) -> Option<u64> {
        if self.kind == Kind::Udp {
            return Some(
                self.dgrams
                    .lock()
                    .unwrap()
                    .front()
                    .map_or(0, |d| d.0.len() as u64),
            );
        }
        Some(
            self.transport
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |t| t.available() as u64),
        )
    }

    fn poll_readable(&self) -> bool {
        if self.kind == Kind::Udp {
            return !self.dgrams.lock().unwrap().is_empty();
        }
        // A listener is readable when a connection waits.
        if let (Some(id), Some(r)) = (*self.remote_listener.lock().unwrap(), self.remote) {
            return (r.pending)(id) != 0;
        }
        if let Some(q) = &*self.backlog.lock().unwrap() {
            return !q.is_empty();
        }
        match &*self.transport.lock().unwrap() {
            // A connection that has failed or finished is readable: the
            // answer is an error or an end, and either way it is ready.
            Some(t) => t.has_data() || matches!(t.status(), Status::Closed | Status::Refused),
            None => true,
        }
    }

    /// Writable, which for a socket mid-`connect` means the connect has
    /// finished either way. This is how a non-blocking connect reports itself:
    /// the caller waits for writable, then asks `SO_ERROR`. The trait's
    /// default `true` would signal "finished" before it was.
    fn poll_writable(&self) -> bool {
        if self.kind == Kind::Udp {
            return true;
        }
        match &*self.transport.lock().unwrap() {
            Some(t) => !matches!(t.status(), Status::Connecting),
            // No `connect` yet. A fresh TCP socket is writable on Linux too,
            // and a caller polling one has not asked about a connection.
            None => true,
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(t) = self.transport.lock().unwrap().take() {
            t.close();
        }
        if let (Some(id), Some(r)) = (self.remote_listener.lock().unwrap().take(), self.remote) {
            (r.close)(id);
        }
        // A UDP socket gives its port back.
        if self.kind == Kind::Udp {
            if let Some((_, port)) = *self.local.lock().unwrap() {
                let mut udp = self.ports.udp.lock().unwrap();
                if udp.get(&port).is_some_and(|w| w.strong_count() == 0) {
                    udp.remove(&port);
                }
            }
        }
        // A listener gives its port back, and the connections nobody
        // accepted see their peer gone.
        if let Some(q) = self.backlog.lock().unwrap().take() {
            let port = self.local.lock().unwrap().map_or(0, |l| l.1);
            let mut listeners = self.ports.listeners.lock().unwrap();
            if listeners.get(&port).is_some_and(|w| w.strong_count() == 0) {
                listeners.remove(&port);
            }
            for (t, _) in q {
                t.close();
            }
        }
    }
}

// -- sockaddr ---------------------------------------------------------------

/// Read a `sockaddr_in`, returning the address and port in host order.
pub fn parse_sockaddr(bytes: &[u8]) -> Result<(u32, u16), Errno> {
    if bytes.len() < 8 {
        return Err(Errno::EINVAL);
    }
    let family = u16::from_ge_bytes([bytes[0], bytes[1]]);
    if family != 2 {
        return Err(Errno::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    let addr = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    Ok((addr, port))
}

/// Write a `sockaddr_in`.
pub fn build_sockaddr(addr: u32, port: u16) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..2].copy_from_slice(&2u16.to_ge_bytes());
    out[2..4].copy_from_slice(&port.to_be_bytes());
    out[4..8].copy_from_slice(&addr.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn an_a_query_gets_an_address_back() {
        let r = Resolver::new();
        let a = answer_dns(&query("deb.debian.org", 1), &r).unwrap();
        assert_eq!(&a[0..2], &[0x12, 0x34], "the id is echoed");
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 1, "one answer");
        let addr = u32::from_be_bytes(a[a.len() - 4..].try_into().unwrap());
        assert_eq!(r.name_for(addr).as_deref(), Some("deb.debian.org"));
    }

    #[test]
    fn the_same_name_keeps_the_same_address() {
        let r = Resolver::new();
        let first = r.address_for("example.com");
        assert_eq!(r.address_for("example.com"), first);
        assert_eq!(r.address_for("EXAMPLE.COM."), first, "and is case-folded");
        assert_ne!(r.address_for("other.com"), first);
    }

    #[test]
    fn an_aaaa_query_is_answered_with_nothing_rather_than_an_error() {
        let r = Resolver::new();
        let a = answer_dns(&query("deb.debian.org", 28), &r).unwrap();
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 0, "no answers");
        assert_eq!(a[3] & 0x0f, 0, "but not an error, or the name is given up on");
    }

    #[test]
    fn a_truncated_query_is_not_answered() {
        let r = Resolver::new();
        assert!(answer_dns(&[0x12, 0x34], &r).is_none());
        let mut q = query("example.com", 1);
        q.truncate(15);
        assert!(answer_dns(&q, &r).is_none());
    }

    #[test]
    fn sockaddr_round_trips() {
        let bytes = build_sockaddr(0xf0000001, 80);
        assert_eq!(parse_sockaddr(&bytes), Ok((0xf0000001, 80)));
    }

    #[test]
    fn only_ipv4_is_understood() {
        let mut bytes = build_sockaddr(1, 1);
        bytes[0] = 10; // AF_INET6
        assert_eq!(parse_sockaddr(&bytes), Err(Errno::EAFNOSUPPORT));
    }
}
