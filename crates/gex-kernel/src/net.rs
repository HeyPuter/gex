//! Sockets.
//!
//! A page cannot open a TCP connection, and neither can wasm. What a page can
//! do is talk to a proxy over a WebSocket, and Wisp is a protocol for
//! multiplexing streams down one. So the kernel does not implement TCP: it
//! implements the *shape* of a socket -- descriptors, `connect`, `send`,
//! `recv`, and the blocking each of those implies -- and hands the carrying
//! to whatever the host has. Natively that is a real socket; in a page it is
//! a Wisp stream.
//!
//! # Names
//!
//! The awkward part is DNS. A Wisp stream is opened *by hostname*, because
//! the proxy is the thing that can resolve; but a guest resolves first and
//! connects to an address, and by then the name is gone. glibc's resolver is
//! not something to reimplement, and neither is DNS-over-UDP-to-a-real-server.
//!
//! So the kernel answers DNS itself. A query for a name gets back an address
//! out of a private range, and the name is remembered against it. When the
//! guest then connects to that address, the name comes back and the host is
//! asked for a stream to it. The guest believes it resolved; the proxy gets
//! the name it needs; nothing in between has to understand DNS beyond
//! reading a question and writing an answer.

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

/// The private range the synthetic answers come from.
///
/// 240.0.0.0/4 is reserved and unroutable, which is the point: an address
/// from here can only ever be one of these, so a guest that somehow reaches
/// the real network with one gets an immediate failure rather than a
/// connection to a stranger.
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
    /// Datagrams waiting to be read. Only the resolver puts anything here.
    dgrams: Mutex<VecDeque<Vec<u8>>>,
    nonblocking: AtomicBool,
}

impl Socket {
    pub fn new(kind: Kind, net: Arc<dyn NetStack>, resolver: Arc<Resolver>) -> Socket {
        Socket {
            kind,
            net,
            resolver,
            transport: Mutex::new(None),
            peer: Mutex::new(None),
            dgrams: Mutex::new(VecDeque::new()),
            nonblocking: AtomicBool::new(false),
        }
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    pub fn peer(&self) -> Option<(u32, u16)> {
        *self.peer.lock().unwrap()
    }

    /// Remember where a datagram went, so the reply can be reported as
    /// having come from there.
    ///
    /// A resolver checks that: it compares the source address of every reply
    /// against the nameserver it asked, and drops anything else as a
    /// spoofing attempt. Reporting 0.0.0.0 made every answer look forged and
    /// `getaddrinfo` timed out with EAI_AGAIN on a reply it had in hand.
    pub fn set_peer(&self, addr: u32, port: u16) {
        *self.peer.lock().unwrap() = Some((addr, port));
    }

    /// `connect`. Returns `EINPROGRESS` while it is still being set up, which
    /// is what a non-blocking connect is supposed to say; the kernel turns
    /// that into a wait for a descriptor that is not non-blocking.
    ///
    /// Idempotent: asking twice does not open a second connection. The kernel
    /// calls this once to start the connection and may reach it again on the
    /// path that reports how it went.
    pub fn connect(&self, addr: u32, port: u16) -> Result<(), Errno> {
        if let Some(t) = self.transport.lock().unwrap().as_ref() {
            return match t.status() {
                Status::Connecting => Err(Errno::EINPROGRESS),
                Status::Open => Ok(()),
                Status::Refused => Err(Errno::ECONNREFUSED),
                Status::Closed => Err(Errno::ECONNRESET),
            };
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

    /// A datagram to the resolver: answer it and queue the reply.
    pub fn send_dgram(&self, data: &[u8]) -> SysResult {
        match answer_dns(data, &self.resolver) {
            Some(reply) => {
                self.dgrams.lock().unwrap().push_back(reply);
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
            Some(d) => {
                let n = buf.len().min(d.len());
                buf[..n].copy_from_slice(&d[..n]);
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
            // Closed and drained is end of file, not "try again".
            //
            // A transport reports "nothing right now" as `EAGAIN` and cannot
            // tell a connection that is empty from one that is over --
            // `status` is what knows. Without this a peer that closes to mark
            // the end of a body leaves the reader polling a descriptor that
            // says ready, reading `EAGAIN`, and going round for ever. That is
            // the failure the note on `available` below describes, on the
            // half of the path that never got the fix, and it is what an
            // `apt-get update` in the browser does thirty times without
            // finishing one index.
            //
            // `send` has always answered `Status::Closed` with `EPIPE`. This
            // is its counterpart.
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
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Whether a read would return something.
    ///
    /// Deliberately not affected by `O_NONBLOCK`: that decides whether the
    /// *caller* waits, which the descriptor answers, and conflating the two
    /// here made `poll` report a socket ready whenever the guest had asked
    /// not to block. A resolver polled, was told yes, read `EAGAIN`, and went
    /// round until it timed out.
    /// For a datagram socket this has to be exact: it is the length of the
    /// next datagram, and a caller sizes its buffer by it.
    fn available(&self) -> Option<u64> {
        if self.kind == Kind::Udp {
            return Some(
                self.dgrams
                    .lock()
                    .unwrap()
                    .front()
                    .map_or(0, |d| d.len() as u64),
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
        match &*self.transport.lock().unwrap() {
            // A connection that has failed or finished is readable: the
            // answer is an error or an end, and either way it is ready.
            Some(t) => t.has_data() || matches!(t.status(), Status::Closed | Status::Refused),
            None => true,
        }
    }

    /// Writable, which for a socket in the middle of `connect` means the
    /// connect has finished -- one way or the other.
    ///
    /// This is the whole of how a non-blocking connect reports itself. The
    /// caller gets `EINPROGRESS`, waits for the descriptor to become
    /// writable, and only then asks `SO_ERROR` whether it worked. Inheriting
    /// the trait's `true` said "finished" the instant the connection was
    /// started, so `apt` asked immediately, was told `EINPROGRESS`, and
    /// reported it as the reason it could not connect:
    ///
    /// ```text
    /// Could not connect to deb.debian.org:80 (240.0.0.1).
    ///   - connect (115: Operation now in progress)
    /// ```
    ///
    /// which reads like a network failure and is really a socket answering a
    /// question before it knew.
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
    }
}

// -- sockaddr ---------------------------------------------------------------

/// Read a `sockaddr_in`, returning the address and port in host order.
pub fn parse_sockaddr(bytes: &[u8]) -> Result<(u32, u16), Errno> {
    if bytes.len() < 8 {
        return Err(Errno::EINVAL);
    }
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
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
    out[0..2].copy_from_slice(&2u16.to_le_bytes());
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
