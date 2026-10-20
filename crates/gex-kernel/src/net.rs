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
    /// The wait queue a change in this end's readiness is announced on
    /// (`FileHandle::wait_queue`); 0 for one no event announces.
    fn wait_queue(&self) -> u32 {
        0
    }
    /// `shutdown(how)`: 0 reads, 1 writes, 2 both. A transport that cannot
    /// half-close accepts it and does nothing.
    fn shutdown(&self, how: u64) -> Result<(), Errno> {
        let _ = how;
        Ok(())
    }
    /// A write once `status` says Closed: `EPIPE`, unless the transport
    /// knows better (only the reading side went away, as after the peer's
    /// `shutdown(SHUT_WR)`).
    fn send_closed(&self, data: &[u8]) -> Result<usize, Errno> {
        let _ = data;
        Err(Errno::EPIPE)
    }
    /// `recv(MSG_PEEK)`: what `recv` would take, left queued.
    fn peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let _ = buf;
        Err(Errno::EOPNOTSUPP)
    }
    /// This connection as a name another process can hold it by (a socket
    /// passed over `SCM_RIGHTS`, or inherited across a fork, in a page where
    /// each process has its own memory), counted as one more holder: the
    /// connection ends when its last holder closes, as a socket outlives one
    /// of its descriptors. `None` for one that cannot leave this process.
    /// What the name means is the host's (`Socket::carry`).
    fn carry(&self) -> Option<String> {
        None
    }
    /// `carry`'s name without the hold: what another holder takes up with
    /// `NetStack::adopt` (a moved socket's connection out, `Socket::go_out`).
    fn name(&self) -> Option<String> {
        None
    }
}

/// Where a host's connections come from.
pub trait NetStack: Send + Sync + std::fmt::Debug {
    fn tcp(&self) -> Result<Arc<dyn Transport>, Errno>;
    /// A connection by `Transport::name`, held once more here; `None` when
    /// it has ended or this network has no such names.
    fn adopt(&self, name: &str) -> Option<Arc<dyn Transport>> {
        let _ = name;
        None
    }
}

/// No network behind the kernel (the browser without a Wisp relay): inet
/// sockets still work over loopback -- bind, listen, connect and the
/// kernel's own DNS answers -- and a connection out is unreachable.
#[derive(Debug)]
pub struct NoNet;

impl NetStack for NoNet {
    fn tcp(&self) -> Result<Arc<dyn Transport>, Errno> {
        Err(Errno::ENETUNREACH)
    }
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
        if let Some(h) = SHARED_NAMES.get() {
            return (h.address_for)(&key);
        }
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
        if let Some(h) = SHARED_NAMES.get() {
            return (h.name_for)(addr);
        }
        self.by_addr.lock().unwrap().get(&addr).cloned()
    }
}

/// The machine's one table of synthetic answers, where processes do not share
/// a kernel (a page: a Worker and a memory per process). One process resolves
/// a name and another connects to the address it was told -- Wine's clients
/// call getaddrinfo and its wineserver makes the connection -- so every
/// process must give a name the same address and read it back the same way.
#[derive(Clone, Copy)]
pub struct SharedNames {
    pub address_for: fn(&str) -> u32,
    pub name_for: fn(u32) -> Option<String>,
}

static SHARED_NAMES: std::sync::OnceLock<SharedNames> = std::sync::OnceLock::new();

/// Answer every `Resolver`'s questions from `names` (set once, by a host with
/// a global kernel, before any process resolves).
pub fn share_names(names: SharedNames) {
    let _ = SHARED_NAMES.set(names);
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
    /// The reading end closed: a write is `EPIPE` (after the first, which
    /// Linux takes before the peer's reset arrives).
    reader_gone: bool,
    /// A write after `reader_gone` was taken: the next is `EPIPE`.
    reset: bool,
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
            if !tx.reset {
                tx.reset = true;
                return Ok(data.len());
            }
            return Err(Errno::EPIPE);
        }
        if tx.writer_gone {
            // this end shut down its writing
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
    fn shutdown(&self, how: u64) -> Result<(), Errno> {
        // SHUT_WR (or RDWR): the peer reads to end of file. SHUT_RD only
        // changes this end's reads, which Linux does not tell the peer.
        if how == 1 || how == 2 {
            self.tx.lock().unwrap().writer_gone = true;
        }
        Ok(())
    }
    fn send_closed(&self, data: &[u8]) -> Result<usize, Errno> {
        self.send(data)
    }
    fn peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let rx = self.rx.lock().unwrap();
        if rx.data.is_empty() {
            return if rx.writer_gone { Ok(0) } else { Err(Errno::EAGAIN) };
        }
        let n = buf.len().min(rx.data.len());
        for (d, s) in buf.iter_mut().zip(rx.data.iter()) {
            *d = *s;
        }
        Ok(n)
    }
}

/// One end of a TCP connection between guests in different processes of a
/// page: a stream in the global kernel (`unix::Remote`), named by the port
/// (`tcp_name`), each end an id there.
pub struct RemoteEnd {
    id: i32,
    remote: crate::unix::Remote,
    /// A write found the far end closed and was taken anyway, as Linux takes
    /// the first one before the peer's reset arrives; the next is `EPIPE`
    /// (`LoopEnd::reset`; tests/gexcc/tcphalfclose).
    reset: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for RemoteEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteEnd({})", self.id)
    }
}

impl RemoteEnd {
    /// An end by its id in the global kernel: one `carry` handed over.
    pub fn adopt(id: i32, remote: crate::unix::Remote) -> RemoteEnd {
        RemoteEnd { id, remote, reset: Default::default() }
    }
}

impl Transport for RemoteEnd {
    fn connect(&self, _host: &str, _port: u16) -> Result<(), Errno> {
        Ok(())
    }
    /// Another id on the same end of the global kernel's stream: the end
    /// lives until its last id closes, and `shutdown` on any id is the end's.
    fn carry(&self) -> Option<String> {
        match (self.remote.dup)(self.id) {
            n if n >= 0 => Some(format!("loop:{n}")),
            _ => None,
        }
    }
    fn wait_queue(&self) -> u32 {
        (self.remote.queue)(self.id)
    }
    fn status(&self) -> Status {
        Status::Open
    }
    fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        match (self.remote.write)(self.id, data.as_ptr() as crate::PAddr, data.len() as u32) {
            n if n >= 0 => Ok(n as usize),
            n if Errno(-n) == Errno::EPIPE && !data.is_empty() && !self.reset.swap(true, Ordering::Relaxed) => Ok(data.len()),
            n => Err(Errno(-n)),
        }
    }
    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        match (self.remote.read)(self.id, buf.as_mut_ptr() as crate::PAddr, buf.len() as u32) {
            n if n >= 0 => Ok(n as usize),
            n => Err(Errno(-n)),
        }
    }
    /// The global kernel's `recv` with `MSG_PEEK` (its descriptor-passing
    /// read, asked for none).
    fn peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut out = [0u8; 8];
        let n = (self.remote.read_fds)(
            self.id,
            buf.as_mut_ptr() as crate::PAddr,
            buf.len() as u32,
            0,
            1,
            0,
            out.as_mut_ptr() as crate::PAddr,
            out.len() as u32,
        );
        if n < 0 {
            return Err(Errno(-n));
        }
        Ok(n as usize)
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
    /// The global kernel's shutdown is SHUT_WR's: the far end reads to its
    /// end. SHUT_RD only changes this end, which the far end is not told.
    fn shutdown(&self, how: u64) -> Result<(), Errno> {
        if how == 1 || how == 2 {
            let _ = (self.remote.shutdown)(self.id, 1);
        }
        Ok(())
    }
}

/// The global kernel's name for a TCP port (abstract, so no file appears).
fn tcp_name(port: u16) -> String {
    format!("\0gex-tcp:{port}")
}

/// The global kernel's name for a UDP port. The global kernel has only
/// streams, so datagrams between a page's processes travel framed over one
/// stream per (sender, port): a 4-byte length and a 2-byte source port
/// (little-endian), then the bytes.
fn udp_name(port: u16) -> String {
    format!("\0gex-udp:{port}")
}

/// A UDP socket's side of the global kernel (`Socket::udp_remote`).
#[derive(Debug, Default)]
struct UdpRemote {
    /// The listener its port is bound as.
    listener: Option<i32>,
    /// Accepted streams from senders, each with the bytes read from it that
    /// do not yet make a whole datagram.
    inbound: Vec<(i32, Vec<u8>)>,
    /// Streams to the ports this socket has sent to.
    outbound: HashMap<u16, i32>,
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
    /// Held by a `recvfrom`/`recvmsg` from its read until it has read
    /// `last_from` ([`Self::recv_turn`]): the address is set by the read and
    /// asked for after it, so two threads receiving on one socket could each
    /// report the other's sender.
    recv_turn: Mutex<()>,
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
    /// A UDP socket's streams in the global kernel.
    udp_remote: Mutex<UdpRemote>,
    /// The id of this socket's hold in the global kernel once it has moved
    /// there (`move_out`), else -1: every call is then that socket's, and the
    /// fields above are unused, until a connection out brings it back
    /// (`go_out`, `come_back`). For a socket that never leaves its process
    /// it costs one load per call.
    moved: std::sync::atomic::AtomicI32,
    /// In the global kernel: the connection a holder made out of it, by
    /// name (`op::GO_OUT`). Every holder then holds that connection itself.
    went_out: Mutex<Option<String>>,
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
            recv_turn: Mutex::new(()),
            me: Mutex::new(std::sync::Weak::new()),
            nonblocking: AtomicBool::new(false),
            ports,
            local: Mutex::new(None),
            backlog: Mutex::new(None),
            remote,
            remote_listener: Mutex::new(None),
            udp_remote: Mutex::new(UdpRemote::default()),
            moved: std::sync::atomic::AtomicI32::new(-1),
            went_out: Mutex::new(None),
        }
    }

    /// Remember the `Arc` this socket lives in (made right after `new`).
    pub fn set_self(self: &Arc<Self>) {
        *self.me.lock().unwrap() = Arc::downgrade(self);
    }

    /// One receiver at a time, from its read to its `last_from`: a guard
    /// the syscall layer holds across both.
    pub fn recv_turn(&self) -> std::sync::MutexGuard<'_, ()> {
        self.recv_turn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the last datagram read came from.
    pub fn last_from(&self) -> Option<(u32, u16)> {
        if let Some(v) = self.fwd(op::LAST_FROM, 0, 0, &mut []) {
            return unpack_addr(v);
        }
        *self.last_from.lock().unwrap()
    }

    /// Give this UDP socket a port (`bind`, or the first send unbound),
    /// registered so guests here can send to it.
    fn udp_take(&self, addr: u32, mut port: u16) -> Result<(), Errno> {
        // In a page: the port is a name in the global kernel, where every
        // process sees it.
        if let Some(r) = self.remote {
            let fixed = port != 0;
            for _ in 0..1000 {
                if !fixed {
                    port = self.ports.udp_ephemeral();
                }
                let n = udp_name(port);
                let id = (r.bind)(n.as_ptr() as crate::PAddr, n.len() as u32);
                if id >= 0 {
                    self.udp_remote.lock().unwrap().listener = Some(id);
                    *self.local.lock().unwrap() = Some((addr, port));
                    return Ok(());
                }
                if fixed || -id != Errno::EADDRINUSE.0 {
                    return Err(Errno(-id));
                }
            }
            return Err(Errno::EADDRINUSE);
        }
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
        if let Some(v) = self.fwd(op::BIND, addr as u64, port as u64, &mut []) {
            return Socket::unit(v);
        }
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
        // Port 0 is a port of the kernel's choosing, chosen now, as Linux
        // does: a program reads it back with getsockname before listening
        // (Wine's server does, for every bind), and connects to it.
        let mut port = port;
        if port == 0 {
            port = self.ports.ephemeral();
            while self.remote_taken(port) {
                port = self.ports.ephemeral();
            }
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
        if let Some(v) = self.fwd(op::LOCAL, 0, 0, &mut []) {
            return unpack_addr(v).unwrap_or((0, 0));
        }
        self.local.lock().unwrap().unwrap_or((0, 0))
    }

    /// `listen`: take the bound port (an ephemeral one if none), and
    /// connections from this machine's guests.
    pub fn listen(self: &Arc<Self>) -> Result<(), Errno> {
        if let Some(v) = self.fwd(op::LISTEN, 0, 0, &mut []) {
            return Socket::unit(v);
        }
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
        if let Some(v) = self.fwd(op::LISTENING, 0, 0, &mut []) {
            return v > 0;
        }
        self.backlog.lock().unwrap().is_some()
    }

    /// `accept`: the next waiting connection as a socket of its own, or
    /// `EAGAIN`; `EINVAL` when not listening.
    pub fn accept(&self) -> Result<Arc<Socket>, Errno> {
        let mut name = [0u8; 160];
        if let Some(n) = self.moved().and_then(|_| self.fwd(op::ACCEPT, 0, 0, &mut name)) {
            // The connection comes back as its `carry` name: an end in the
            // global kernel, held by this process now.
            if n < 0 {
                return Err(errno_of(n));
            }
            let name = std::str::from_utf8(&name[..n as usize]).map_err(|_| Errno::EINVAL)?;
            let r = self.remote.ok_or(Errno::EINVAL)?;
            let id = Socket::carried_transport(name)
                .and_then(|t| t.strip_prefix("loop:"))
                .and_then(|t| t.parse::<i32>().ok())
                .ok_or(Errno::EINVAL)?;
            let t: Arc<dyn Transport> = Arc::new(RemoteEnd::adopt(id, r));
            return Socket::adopt(name, t, Arc::clone(&self.net), Arc::clone(&self.resolver), Arc::clone(&self.ports), self.remote)
                .ok_or(Errno::EINVAL);
        }
        let remote_listener = *self.remote_listener.lock().unwrap();
        let (t, from): (Arc<dyn Transport>, u16) = match (remote_listener, self.remote) {
            (Some(id), Some(r)) => match (r.accept)(id) {
                // The far end's port is the other process's to know.
                n if n >= 0 => (Arc::new(RemoteEnd { id: n, remote: r, reset: Default::default() }), 0),
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
        if let Some(v) = self.fwd(op::PEER, 0, 0, &mut []) {
            return unpack_addr(v);
        }
        *self.peer.lock().unwrap()
    }

    /// Remember where a datagram went, so the reply can be reported as coming
    /// from there: a resolver drops replies whose source is not the nameserver
    /// it asked, so 0.0.0.0 made every answer look spoofed.
    pub fn set_peer(&self, addr: u32, port: u16) {
        if self.fwd(op::SET_PEER, addr as u64, port as u64, &mut []).is_some() {
            return;
        }
        *self.peer.lock().unwrap() = Some((addr, port));
    }

    /// This socket as a name another process can rebuild it from (a page's
    /// `SCM_RIGHTS` and fork): `inet:PEER:PORT:LOCAL:PORT:TRANSPORT`, the last
    /// part the transport's own (`Transport::carry`), counted as one more
    /// holder of the connection. Only a TCP socket with a connection (open or
    /// still connecting) can go: the two copies are then one connection, as
    /// two descriptors are one socket. One not yet connected cannot -- its
    /// copies would connect apart -- nor can a listener or a UDP socket.
    pub fn carry(&self) -> Option<String> {
        // A moved socket goes as another hold on it (`move_out`).
        if self.moved().is_some() {
            return None;
        }
        if self.kind != Kind::Tcp || self.backlog.lock().unwrap().is_some() {
            return None;
        }
        let t = self.transport.lock().unwrap().as_ref()?.carry()?;
        let (pa, pp) = self.peer().unwrap_or((0, 0));
        let (la, lp) = self.local();
        Some(format!("inet:{pa}:{pp}:{la}:{lp}:{t}"))
    }

    /// The other half of `carry`: a TCP socket on `transport` (what the
    /// name's last part was rebuilt into), where the name says it was.
    pub fn adopt(
        desc: &str,
        transport: Arc<dyn Transport>,
        net: Arc<dyn NetStack>,
        resolver: Arc<Resolver>,
        ports: Arc<Ports>,
        remote: Option<crate::unix::Remote>,
    ) -> Option<Arc<Socket>> {
        let mut p = desc.strip_prefix("inet:")?.splitn(5, ':');
        let pa = p.next()?.parse::<u32>().ok()?;
        let pp = p.next()?.parse::<u16>().ok()?;
        let la = p.next()?.parse::<u32>().ok()?;
        let lp = p.next()?.parse::<u16>().ok()?;
        let sock = Arc::new(Socket::new(Kind::Tcp, net, resolver, ports, remote));
        sock.set_self();
        if pa != 0 || pp != 0 {
            *sock.peer.lock().unwrap() = Some((pa, pp));
        }
        if la != 0 || lp != 0 {
            *sock.local.lock().unwrap() = Some((la, lp));
        }
        *sock.transport.lock().unwrap() = Some(transport);
        Some(sock)
    }

    /// The transport part of a `carry` name (`wisp:N`, `loop:N`).
    pub fn carried_transport(desc: &str) -> Option<&str> {
        desc.strip_prefix("inet:")?.splitn(5, ':').nth(4)
    }

    /// `connect`. Returns `EINPROGRESS` while still connecting; the kernel
    /// turns that into a wait for a blocking descriptor. Idempotent: asking
    /// twice does not open a second connection.
    pub fn connect(&self, addr: u32, port: u16) -> Result<(), Errno> {
        if let Some(v) = self.fwd(op::CONNECT, addr as u64, port as u64, &mut []) {
            // The global kernel has no network of its own: a connection
            // out is made here, and the socket comes back to this process
            // (`go_out`).
            if v != -(Errno::ENETUNREACH.0 as i64) || self.kind != Kind::Tcp {
                return Socket::unit(v);
            }
            return self.go_out(addr, port);
        }
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
                    *self.transport.lock().unwrap() = Some(Arc::new(RemoteEnd { id, remote: r, reset: Default::default() }));
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

    /// `shutdown` on a connected stream.
    pub fn shutdown(&self, how: u64) -> Result<u64, Errno> {
        if let Some(v) = self.fwd(op::SHUTDOWN, how, 0, &mut []) {
            return Socket::count(v);
        }
        if how > 2 {
            return Err(Errno::EINVAL);
        }
        match &*self.transport.lock().unwrap() {
            Some(t) => t.shutdown(how).map(|_| 0),
            None => Err(Errno::ENOTCONN),
        }
    }

    pub fn status(&self) -> Status {
        if let Some(v) = self.fwd(op::STATUS, 0, 0, &mut []) {
            return match v {
                0 => Status::Connecting,
                1 => Status::Open,
                3 => Status::Refused,
                _ => Status::Closed,
            };
        }
        match &*self.transport.lock().unwrap() {
            Some(t) => t.status(),
            None => Status::Closed,
        }
    }

    /// A datagram: to a guest on this machine bound to the port it is for,
    /// else to the resolver, which answers it and queues the reply.
    pub fn send_dgram(&self, data: &[u8]) -> SysResult {
        if let Some(v) = self.fwd_write(data) {
            return Socket::count(v);
        }
        let to = self.peer();
        if let (Some((_, port)), Some(r)) = (to.filter(|&(a, _)| is_local(a)), self.remote) {
            if self.local.lock().unwrap().is_none() {
                self.udp_take(0, 0)?;
            }
            if let Some(id) = self.udp_stream_to(r, port) {
                let mut frame = Vec::with_capacity(6 + data.len());
                frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
                frame.extend_from_slice(&self.local().1.to_le_bytes());
                frame.extend_from_slice(data);
                let mut at = 0;
                // A stream's buffer is far larger than a datagram; a full one
                // (a receiver that stopped reading) drops it, as UDP may.
                for _ in 0..64 {
                    match (r.write)(id, frame[at..].as_ptr() as crate::PAddr, (frame.len() - at) as u32) {
                        n if n > 0 => at += n as usize,
                        _ => {}
                    }
                    if at == frame.len() {
                        break;
                    }
                }
                return Ok(data.len() as u64);
            }
            // Nothing bound there in any process: lost, as UDP is.
            if answer_dns(data, &self.resolver).is_none() {
                return Ok(data.len() as u64);
            }
        }
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

    /// The stream to UDP port `port` in the global kernel, opened once.
    fn udp_stream_to(&self, r: crate::unix::Remote, port: u16) -> Option<i32> {
        let mut u = self.udp_remote.lock().unwrap();
        if let Some(&id) = u.outbound.get(&port) {
            return Some(id);
        }
        let n = udp_name(port);
        let id = (r.connect)(n.as_ptr() as crate::PAddr, n.len() as u32);
        (id >= 0).then(|| {
            u.outbound.insert(port, id);
            id
        })
    }

    /// Take what senders in other processes wrote: accept new streams, read
    /// each, and queue every whole datagram.
    fn udp_pump(&self) {
        let Some(r) = self.remote else { return };
        let mut u = self.udp_remote.lock().unwrap();
        let Some(l) = u.listener else { return };
        while (r.pending)(l) != 0 {
            let id = (r.accept)(l);
            if id < 0 {
                break;
            }
            u.inbound.push((id, Vec::new()));
        }
        let mut q = self.dgrams.lock().unwrap();
        let mut chunk = [0u8; 4096];
        let mut gone = Vec::new();
        for (id, buf) in u.inbound.iter_mut() {
            while (r.readable)(*id) != 0 {
                let n = (r.read)(*id, chunk.as_mut_ptr() as crate::PAddr, chunk.len() as u32);
                if n == 0 {
                    // The sender closed (its socket or its process went).
                    gone.push(*id);
                }
                if n <= 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n as usize]);
            }
            while buf.len() >= 6 {
                let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if buf.len() < 6 + len {
                    break;
                }
                let from = u16::from_le_bytes([buf[4], buf[5]]);
                q.push_back((buf[6..6 + len].to_vec(), (0x7f00_0001, from)));
                buf.drain(..6 + len);
            }
        }
        for id in gone {
            (r.close)(id);
            u.inbound.retain(|(i, _)| *i != id);
        }
    }

    pub fn recv_dgram(&self, buf: &mut [u8]) -> SysResult {
        if let Some(v) = self.fwd(op::READ, 0, 0, buf) {
            return Socket::count(v);
        }
        self.udp_pump();
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

impl Socket {
    /// `recv` with `MSG_PEEK`: the bytes (or the next datagram) left queued
    /// for the next read. Wine's server peeks a byte to tell a socket that
    /// has data from one that was closed.
    pub fn peek(&self, buf: &mut [u8]) -> SysResult {
        if let Some(v) = self.fwd(op::PEEK, 0, 0, buf) {
            return Socket::count(v);
        }
        if self.kind == Kind::Udp {
            self.udp_pump();
            return match self.dgrams.lock().unwrap().front() {
                Some((d, from)) => {
                    let n = buf.len().min(d.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    *self.last_from.lock().unwrap() = Some(*from);
                    Ok(n as u64)
                }
                None => Err(Errno::EAGAIN),
            };
        }
        let t = self.transport.lock().unwrap();
        let Some(t) = t.as_ref() else {
            return Err(Errno::ENOTCONN);
        };
        match t.status() {
            Status::Refused => Err(Errno::ECONNREFUSED),
            Status::Closed => match t.peek(buf) {
                Ok(n) => Ok(n as u64),
                Err(Errno::EAGAIN) => Ok(0),
                Err(e) => Err(e),
            },
            _ => t.peek(buf).map(|n| n as u64),
        }
    }
}

// -- sockets in the global kernel ----------------------------------------------

/// Internet sockets that live in the global kernel module (a page: a Worker
/// and a memory per process). A socket that has to be in two processes at
/// once -- Wine's wineserver makes every socket, binds it and polls it, while
/// the client sends, receives and sets options on the same one -- moves there
/// the first time it leaves its process unconnected (`Socket::move_out`), and
/// each process's descriptor forwards to it by id. A connected TCP socket
/// still travels as its connection (`Socket::carry`), and one that never
/// leaves stays where it is.
#[derive(Clone, Copy, Debug)]
pub struct InetRemote {
    /// A socket rebuilt from `Socket::export`'s bytes: its id, or a negated
    /// errno.
    pub adopt: fn(state: &[u8]) -> i32,
    /// One call on socket `id` (`op::*`), `buf` filled by those `op::fills`
    /// names: the result, or a negated errno.
    pub op: fn(id: i32, op: u32, a: u64, b: u64, buf: &mut [u8]) -> i64,
    /// `op::WRITE`, whose bytes are the caller's to send.
    pub write: fn(id: i32, data: &[u8]) -> i64,
    /// Another holder of the same socket (a descriptor on its way to another
    /// process): its id, or a negated errno.
    pub dup: fn(id: i32) -> i32,
    /// One holder gone; the socket goes with its last.
    pub close: fn(id: i32),
}

static INET_REMOTE: std::sync::OnceLock<InetRemote> = std::sync::OnceLock::new();

/// Let unconnected sockets leave their process (set once, by a host with a
/// global kernel).
pub fn share_inet(remote: InetRemote) {
    let _ = INET_REMOTE.set(remote);
}

/// A socket's place in the global kernel.
#[derive(Clone, Copy, Debug)]
struct Moved {
    id: i32,
    r: InetRemote,
}

/// The calls `InetRemote::op` carries, one per `Socket` method.
pub mod op {
    pub const BIND: u32 = 1;
    pub const LISTEN: u32 = 2;
    /// The connection's `carry` name into `buf`; its length.
    pub const ACCEPT: u32 = 3;
    pub const CONNECT: u32 = 4;
    pub const SHUTDOWN: u32 = 5;
    pub const STATUS: u32 = 6;
    pub const LOCAL: u32 = 7;
    pub const PEER: u32 = 8;
    pub const SET_PEER: u32 = 9;
    pub const LAST_FROM: u32 = 10;
    /// `buf` is the bytes to send.
    pub const WRITE: u32 = 11;
    /// `buf` is filled; the bytes read.
    pub const READ: u32 = 12;
    pub const PEEK: u32 = 13;
    pub const AVAILABLE: u32 = 14;
    pub const READABLE: u32 = 15;
    pub const WRITABLE: u32 = 16;
    pub const LISTENING: u32 = 17;
    pub const QUEUE: u32 = 18;
    /// A holder connected it out (`Socket::go_out`): `buf` is the
    /// connection's `carry` name. `EISCONN` if another holder did first.
    pub const GO_OUT: u32 = 19;
    /// That name into `buf`; its length. Every other call on such a socket
    /// is `EREMCHG`, which tells its holder to take the connection up.
    pub const WENT_OUT: u32 = 20;

    /// Whether the call reads `buf` (copied from the caller first).
    pub fn takes(op: u32) -> bool {
        matches!(op, WRITE | GO_OUT)
    }

    /// Whether the call fills `buf` (copied back to the caller).
    pub fn fills(op: u32) -> bool {
        matches!(op, ACCEPT | READ | PEEK | WENT_OUT)
    }
}

fn pack_addr(a: Option<(u32, u16)>) -> i64 {
    a.map_or(-1, |(ip, port)| (ip as i64) << 16 | port as i64)
}

fn unpack_addr(v: i64) -> Option<(u32, u16)> {
    (v >= 0).then(|| ((v >> 16) as u32, v as u16))
}

fn errno_of(v: i64) -> Errno {
    Errno((-v) as i32)
}

/// The little-endian writer and reader of `Socket::export`'s bytes.
#[derive(Default)]
struct StateOut(Vec<u8>);

impl StateOut {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn addr(&mut self, a: Option<(u32, u16)>) {
        match a {
            Some((ip, port)) => {
                self.u32(1);
                self.u32(ip);
                self.u32(port as u32);
            }
            None => self.u32(0),
        }
    }
    fn id(&mut self, v: Option<i32>) {
        self.u32(v.map_or(u32::MAX, |v| v as u32));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct StateIn<'a>(&'a [u8]);

impl StateIn<'_> {
    fn u32(&mut self) -> Option<u32> {
        let (h, t) = self.0.split_at_checked(4)?;
        self.0 = t;
        Some(u32::from_le_bytes(h.try_into().ok()?))
    }
    fn addr(&mut self) -> Option<Option<(u32, u16)>> {
        Some(match self.u32()? {
            0 => None,
            _ => Some((self.u32()?, self.u32()? as u16)),
        })
    }
    fn id(&mut self) -> Option<Option<i32>> {
        Some(match self.u32()? {
            u32::MAX => None,
            v => Some(v as i32),
        })
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        let (h, t) = self.0.split_at_checked(n)?;
        self.0 = t;
        Some(h.to_vec())
    }
}

impl Socket {
    /// The global kernel's socket this one forwards to, if it has moved.
    fn moved(&self) -> Option<Moved> {
        match self.moved.load(Ordering::Acquire) {
            id if id < 0 => None,
            id => Some(Moved { id, r: *INET_REMOTE.get()? }),
        }
    }

    /// One call on the moved socket, if this one has moved: its result.
    /// `None` too once the socket has come back to this process as a
    /// connection another holder made (`come_back`): the caller then goes
    /// on as for a socket that never left.
    fn fwd(&self, call: u32, a: u64, b: u64, buf: &mut [u8]) -> Option<i64> {
        let m = self.moved()?;
        let v = (m.r.op)(m.id, call, a, b, buf);
        if v == -(Errno::EREMCHG.0 as i64) {
            return match self.come_back(m) {
                Ok(()) => None,
                Err(e) => Some(-(e.0 as i64)),
            };
        }
        Some(v)
    }

    fn fwd_write(&self, data: &[u8]) -> Option<i64> {
        let m = self.moved()?;
        let v = (m.r.write)(m.id, data);
        if v == -(Errno::EREMCHG.0 as i64) {
            return match self.come_back(m) {
                Ok(()) => None,
                Err(e) => Some(-(e.0 as i64)),
            };
        }
        Some(v)
    }

    /// A moved TCP socket connecting out: the connection is this process's
    /// (a Wisp stream), made here; the global kernel keeps its name for the
    /// other holders, each of which takes it up at its next call, and this
    /// one stops forwarding.
    fn go_out(&self, addr: u32, port: u16) -> Result<(), Errno> {
        let Some(m) = self.moved() else { return self.connect(addr, port) };
        let host = self.resolver.name_for(addr).unwrap_or_else(|| {
            let b = addr.to_be_bytes();
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        });
        let t = self.net.tcp()?;
        t.connect(&host, port)?;
        let Some(tname) = t.name() else {
            t.close();
            return Err(Errno::ENETUNREACH);
        };
        let mut name = format!("inet:{addr}:{port}:0:0:{tname}").into_bytes();
        let v = (m.r.op)(m.id, op::GO_OUT, 0, 0, &mut name);
        if v < 0 {
            // Another holder got there first: its connection is this one's.
            t.close();
            self.come_back(m)?;
            return self.connect(addr, port);
        }
        *self.peer.lock().unwrap() = Some((addr, port));
        *self.transport.lock().unwrap() = Some(t);
        if self.moved.compare_exchange(m.id, -1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            (m.r.close)(m.id);
        }
        Err(Errno::EINPROGRESS)
    }

    /// The connection another holder made out of this socket, held here
    /// now (`go_out`): this holder stops forwarding.
    fn come_back(&self, m: Moved) -> Result<(), Errno> {
        let mut name = [0u8; 160];
        let n = (m.r.op)(m.id, op::WENT_OUT, 0, 0, &mut name);
        if n < 0 {
            return Err(errno_of(n));
        }
        let name = std::str::from_utf8(&name[..n as usize]).map_err(|_| Errno::EINVAL)?;
        let t = Socket::carried_transport(name).and_then(|t| self.net.adopt(t)).ok_or(Errno::ECONNRESET)?;
        let mut p = name.strip_prefix("inet:").unwrap_or("").splitn(3, ':');
        let pa = p.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        let pp = p.next().and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
        let _guard = self.udp_remote.lock().unwrap();
        if self.moved.compare_exchange(m.id, -1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            *self.peer.lock().unwrap() = Some((pa, pp));
            *self.transport.lock().unwrap() = Some(t);
            (m.r.close)(m.id);
        } else {
            // Another thread here took it up first.
            t.close();
        }
        Ok(())
    }

    fn unit(v: i64) -> Result<(), Errno> {
        if v < 0 { Err(errno_of(v)) } else { Ok(()) }
    }

    fn count(v: i64) -> SysResult {
        if v < 0 { Err(errno_of(v)) } else { Ok(v as u64) }
    }

    /// Everything this socket is, as bytes `import` rebuilds it from, taken
    /// out of it: its ids in the global kernel are the new socket's now.
    fn export(&self, u: UdpRemote) -> Vec<u8> {
        let mut o = StateOut::default();
        o.u32(match self.kind {
            Kind::Tcp => 0,
            Kind::Udp => 1,
        });
        o.addr(*self.local.lock().unwrap());
        o.addr(*self.peer.lock().unwrap());
        o.addr(*self.last_from.lock().unwrap());
        o.id(self.remote_listener.lock().unwrap().take());
        // A listener's waiting connections are the global kernel's already
        // (in a page nothing queues here).
        o.u32(self.backlog.lock().unwrap().take().is_some() as u32);
        o.id(u.listener);
        o.u32(u.inbound.len() as u32);
        for (id, buf) in &u.inbound {
            o.u32(*id as u32);
            o.bytes(buf);
        }
        o.u32(u.outbound.len() as u32);
        for (port, id) in &u.outbound {
            o.u32(*port as u32);
            o.u32(*id as u32);
        }
        let q = std::mem::take(&mut *self.dgrams.lock().unwrap());
        o.u32(q.len() as u32);
        for (d, from) in &q {
            o.bytes(d);
            o.addr(Some(*from));
        }
        o.0
    }

    /// The other half of `export`: the socket, here (the global kernel).
    pub fn import(
        state: &[u8],
        net: Arc<dyn NetStack>,
        resolver: Arc<Resolver>,
        ports: Arc<Ports>,
        remote: Option<crate::unix::Remote>,
    ) -> Option<Arc<Socket>> {
        let mut i = StateIn(state);
        let kind = match i.u32()? {
            0 => Kind::Tcp,
            _ => Kind::Udp,
        };
        let s = Socket::new(kind, net, resolver, ports, remote);
        *s.local.lock().unwrap() = i.addr()?;
        *s.peer.lock().unwrap() = i.addr()?;
        *s.last_from.lock().unwrap() = i.addr()?;
        *s.remote_listener.lock().unwrap() = i.id()?;
        if i.u32()? != 0 {
            *s.backlog.lock().unwrap() = Some(VecDeque::new());
        }
        {
            let mut u = s.udp_remote.lock().unwrap();
            u.listener = i.id()?;
            for _ in 0..i.u32()? {
                let id = i.u32()? as i32;
                u.inbound.push((id, i.bytes()?));
            }
            for _ in 0..i.u32()? {
                let port = i.u32()? as u16;
                u.outbound.insert(port, i.u32()? as i32);
            }
        }
        {
            let mut q = s.dgrams.lock().unwrap();
            for _ in 0..i.u32()? {
                let d = i.bytes()?;
                q.push_back((d, i.addr()??));
            }
        }
        let s = Arc::new(s);
        s.set_self();
        Some(s)
    }

    /// This socket as one another process can hold too: moved to the global
    /// kernel if it is not there yet, then held once more. The name it goes
    /// by (`inetg:KIND:ID`), or `None` where there is no global kernel to
    /// move to, or for a connected TCP socket (`carry`'s).
    pub fn move_out(&self) -> Option<String> {
        let r = *INET_REMOTE.get()?;
        let kind = match self.kind {
            Kind::Tcp => 0,
            Kind::Udp => 1,
        };
        // The UDP state's lock is the move's: two threads handing the socket
        // on at once move it once.
        let mut u = self.udp_remote.lock().unwrap();
        if self.moved().is_none() {
            if self.transport.lock().unwrap().is_some() {
                return None;
            }
            let state = self.export(std::mem::take(&mut *u));
            let id = (r.adopt)(&state);
            if id < 0 {
                return None;
            }
            self.moved.store(id, Ordering::Release);
        }
        drop(u);
        let m = self.moved()?;
        let dup = (m.r.dup)(m.id);
        (dup >= 0).then(|| format!("inetg:{kind}:{dup}"))
    }

    /// A descriptor's socket from `move_out`'s name: the global kernel's,
    /// held by this process now.
    pub fn moved_proxy(
        name: &str,
        net: Arc<dyn NetStack>,
        resolver: Arc<Resolver>,
        ports: Arc<Ports>,
        remote: Option<crate::unix::Remote>,
    ) -> Option<Arc<Socket>> {
        let r = *INET_REMOTE.get()?;
        let (kind, id) = name.strip_prefix("inetg:")?.split_once(':')?;
        let kind = if kind == "1" { Kind::Udp } else { Kind::Tcp };
        let id = id.parse::<i32>().ok()?;
        let s = Socket::new(kind, net, resolver, ports, remote);
        let _ = r;
        s.moved.store(id, Ordering::Release);
        let s = Arc::new(s);
        s.set_self();
        Some(s)
    }

    /// `InetRemote::op`'s other end, run by the global kernel on its own
    /// socket. `buf` is the caller's bytes for `op::WRITE`, and what the
    /// `op::fills` calls fill.
    pub fn serve(self: &Arc<Self>, call: u32, a: u64, b: u64, buf: &mut [u8]) -> i64 {
        let unit = |r: Result<(), Errno>| r.map_or_else(|e| -(e.0 as i64), |_| 0);
        let count = |r: SysResult| r.map_or_else(|e| -(e.0 as i64), |n| n as i64);
        {
            let mut out = self.went_out.lock().unwrap();
            match (call, out.as_ref()) {
                (op::WENT_OUT, Some(name)) if name.len() <= buf.len() => {
                    buf[..name.len()].copy_from_slice(name.as_bytes());
                    return name.len() as i64;
                }
                (op::WENT_OUT, _) => return -(Errno::EINVAL.0 as i64),
                (op::GO_OUT, Some(_)) => return -(Errno::EISCONN.0 as i64),
                (op::GO_OUT, None) => {
                    if self.transport.lock().unwrap().is_some() || self.is_listening() {
                        return -(Errno::EISCONN.0 as i64);
                    }
                    *out = Some(String::from_utf8_lossy(buf).into_owned());
                    return 0;
                }
                (_, Some(_)) => return -(Errno::EREMCHG.0 as i64),
                _ => {}
            }
        }
        match call {
            op::BIND => unit(self.bind(a as u32, b as u16)),
            op::LISTEN => unit(self.listen()),
            op::ACCEPT => match self.accept() {
                Ok(s) => match s.carry() {
                    Some(name) if name.len() <= buf.len() => {
                        buf[..name.len()].copy_from_slice(name.as_bytes());
                        name.len() as i64
                    }
                    _ => -(Errno::ENOMEM.0 as i64),
                },
                Err(e) => -(e.0 as i64),
            },
            op::CONNECT => unit(self.connect(a as u32, b as u16)),
            op::SHUTDOWN => count(self.shutdown(a)),
            op::STATUS => match self.status() {
                Status::Connecting => 0,
                Status::Open => 1,
                Status::Closed => 2,
                Status::Refused => 3,
            },
            op::LOCAL => pack_addr(Some(self.local())),
            op::PEER => pack_addr(self.peer()),
            op::SET_PEER => {
                self.set_peer(a as u32, b as u16);
                0
            }
            op::LAST_FROM => pack_addr(self.last_from()),
            op::WRITE => count(self.write_at(0, buf)),
            op::READ => count(self.read_at(0, buf)),
            op::PEEK => count(self.peek(buf)),
            op::AVAILABLE => self.available().unwrap_or(0) as i64,
            op::READABLE => self.poll_readable() as i64,
            op::WRITABLE => self.poll_writable() as i64,
            op::LISTENING => self.is_listening() as i64,
            op::QUEUE => self.wait_queue() as i64,
            _ => -(Errno::ENOSYS.0 as i64),
        }
    }
}

impl FileHandle for Socket {
    fn as_inet_socket(self: Arc<Self>) -> Option<Arc<Socket>> {
        Some(self)
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        if let Some(v) = self.fwd(op::READ, 0, 0, buf) {
            return Socket::count(v);
        }
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
        if let Some(v) = self.fwd_write(buf) {
            return Socket::count(v);
        }
        if self.kind == Kind::Udp {
            return self.send_dgram(buf);
        }
        let t = self.transport.lock().unwrap();
        let Some(t) = t.as_ref() else {
            return Err(Errno::ENOTCONN);
        };
        match t.status() {
            Status::Refused => Err(Errno::ECONNREFUSED),
            Status::Closed => t.send_closed(buf).map(|n| n as u64),
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
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Bytes available to read, for `FIONREAD`. For a datagram socket this
    /// must be exact -- the length of the next datagram, which a caller uses
    /// to size its buffer.
    fn available(&self) -> Option<u64> {
        if let Some(v) = self.fwd(op::AVAILABLE, 0, 0, &mut []) {
            return Some(v.max(0) as u64);
        }
        if self.kind == Kind::Udp {
            self.udp_pump();
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
        if let Some(v) = self.fwd(op::READABLE, 0, 0, &mut []) {
            return v != 0;
        }
        if self.kind == Kind::Udp {
            self.udp_pump();
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

    /// In a page, a listener's and a loopback connection's ends are in the
    /// global kernel, and so is the queue their changes are announced on: a
    /// thread parked in `accept`, `poll` or `read` on one had none, and slept
    /// through the connection or the bytes (tests/x86-programs/tcp-loopback,
    /// a forked child connecting to its parent).
    fn wait_queue(&self) -> u32 {
        if let Some(v) = self.fwd(op::QUEUE, 0, 0, &mut []) {
            return v.max(0) as u32;
        }
        if let (Some(id), Some(r)) = (*self.remote_listener.lock().unwrap(), self.remote) {
            return (r.queue)(id);
        }
        self.transport.lock().unwrap().as_ref().map_or(0, |t| t.wait_queue())
    }

    /// Writable, which for a socket mid-`connect` means the connect has
    /// finished either way. This is how a non-blocking connect reports itself:
    /// the caller waits for writable, then asks `SO_ERROR`. The trait's
    /// default `true` would signal "finished" before it was.
    fn poll_writable(&self) -> bool {
        if let Some(v) = self.fwd(op::WRITABLE, 0, 0, &mut []) {
            return v != 0;
        }
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
        // A moved socket's state is the global kernel's: this was one hold.
        if let Some(m) = self.moved() {
            (m.r.close)(m.id);
            return;
        }
        if let Some(t) = self.transport.lock().unwrap().take() {
            t.close();
        }
        if let (Some(id), Some(r)) = (self.remote_listener.lock().unwrap().take(), self.remote) {
            (r.close)(id);
        }
        // A UDP socket's streams in the global kernel close (its name goes
        // with the listener).
        if let Some(r) = self.remote {
            let u = std::mem::take(&mut *self.udp_remote.lock().unwrap());
            for id in u.listener.into_iter().chain(u.inbound.into_iter().map(|(i, _)| i)).chain(u.outbound.into_values()) {
                (r.close)(id);
            }
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

    /// No network: loopback between guests only.
    #[derive(Debug)]
    struct NoNet;
    impl NetStack for NoNet {
        fn tcp(&self) -> Result<Arc<dyn Transport>, Errno> {
            Err(Errno::ENETUNREACH)
        }
    }

    #[test]
    fn a_tcp_bind_to_port_zero_gets_its_port_at_once() {
        let (net, res, ports) = (Arc::new(NoNet) as Arc<dyn NetStack>, Arc::new(Resolver::new()), Arc::new(Ports::new()));
        let l = Arc::new(Socket::new(Kind::Tcp, Arc::clone(&net), Arc::clone(&res), Arc::clone(&ports), None));
        l.set_self();
        l.bind(0x7f00_0001, 0).unwrap();
        let port = l.local().1;
        assert_ne!(port, 0, "getsockname after bind has the port");
        l.listen().unwrap();
        assert_eq!(l.local().1, port, "listen keeps it");
        let c = Socket::new(Kind::Tcp, net, res, ports, None);
        c.connect(0x7f00_0001, port).unwrap();
        let s = l.accept().expect("a connection to it arrives");
        assert_eq!(c.write_at(0, b"hi").unwrap(), 2);
        let mut b = [0u8; 8];
        assert_eq!(s.peek(&mut b[..1]).unwrap(), 1, "a peek sees the first byte");
        assert_eq!(s.read_at(0, &mut b).unwrap(), 2, "and leaves it for the read");
        assert_eq!(&b[..2], b"hi");
        c.shutdown(1).unwrap();
        assert_eq!(s.status(), Status::Closed, "and its shutdown reaches the far end");
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
