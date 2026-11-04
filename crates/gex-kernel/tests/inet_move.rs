//! Internet sockets moved to the global kernel (`net::Socket::move_out`): one
//! socket held by two processes, as Wine's wineserver and its client hold
//! every socket. The global kernel's side is played by a table here, in this
//! process's memory (`hosts/kernel` keeps the same table in the shared one).

use gex_kernel::net::{op, share_inet, InetRemote, Kind, NoNet, Ports, Resolver, Socket};
use gex_kernel::vfs::FileHandle;
use std::sync::{Arc, Mutex, OnceLock};

mod global {
    use super::*;

    type Table = Mutex<Vec<Option<Arc<Socket>>>>;

    fn table() -> &'static Table {
        static T: OnceLock<Table> = OnceLock::new();
        T.get_or_init(Default::default)
    }

    /// The global kernel's machine: where a moved socket's port is.
    pub fn ports() -> Arc<Ports> {
        static P: OnceLock<Arc<Ports>> = OnceLock::new();
        Arc::clone(P.get_or_init(|| Arc::new(Ports::new())))
    }

    fn keep(s: Arc<Socket>) -> i32 {
        let mut t = table().lock().unwrap();
        t.push(Some(s));
        (t.len() - 1) as i32
    }

    fn at(id: i32) -> Option<Arc<Socket>> {
        table().lock().unwrap().get(id as usize).cloned().flatten()
    }

    fn adopt(state: &[u8]) -> i32 {
        keep(Socket::import(state, Arc::new(NoNet), Arc::new(Resolver::new()), ports(), None).unwrap())
    }

    fn call(id: i32, what: u32, a: u64, b: u64, buf: &mut [u8]) -> i64 {
        at(id).map_or(-9, |s| s.serve(what, a, b, buf))
    }

    fn write(id: i32, data: &[u8]) -> i64 {
        let mut v = data.to_vec();
        call(id, op::WRITE, 0, 0, &mut v)
    }

    fn dup(id: i32) -> i32 {
        at(id).map_or(-9, keep)
    }

    fn close(id: i32) {
        let s = table().lock().unwrap().get_mut(id as usize).and_then(Option::take);
        drop(s);
    }

    pub fn live() -> usize {
        table().lock().unwrap().iter().filter(|e| e.is_some()).count()
    }

    /// The table is the test binary's; one test at a time counts it.
    pub fn install() -> std::sync::MutexGuard<'static, ()> {
        static ONE: Mutex<()> = Mutex::new(());
        let g = ONE.lock().unwrap_or_else(|e| e.into_inner());
        share_inet(InetRemote { adopt, op: call, write, dup, close });
        g
    }
}

fn local_socket(kind: Kind, res: &Arc<Resolver>, ports: &Arc<Ports>) -> Arc<Socket> {
    let s = Arc::new(Socket::new(kind, Arc::new(NoNet), Arc::clone(res), Arc::clone(ports), None));
    s.set_self();
    s
}

#[test]
fn an_unconnected_udp_socket_is_one_socket_in_two_processes() {
    let _one = global::install();
    let res = Arc::new(Resolver::new());
    let mine = Arc::new(Ports::new());
    let before = global::live();
    // The wineserver's socket, handed to a client before anything is done
    // with it (Wine's client asks for the descriptor to set an option).
    let server = local_socket(Kind::Udp, &res, &mine);
    let name = server.move_out().expect("it can leave its process");
    assert!(name.starts_with("inetg:1:"), "{name}");
    let client = Socket::moved_proxy(&name, Arc::new(NoNet), Arc::clone(&res), Arc::clone(&mine), None).unwrap();
    // The server binds it; the client's copy is where it was bound.
    server.bind(0x7f00_0001, 0).unwrap();
    let port = client.local().1;
    assert_ne!(port, 0);
    assert_eq!(server.local(), client.local());
    // A datagram to it, from a socket on the global kernel's machine.
    let sender = local_socket(Kind::Udp, &Arc::new(Resolver::new()), &global::ports());
    sender.set_peer(0x7f00_0001, port);
    assert_eq!(sender.write_at(0, b"ping").unwrap(), 4);
    // The server sees it ready, as its poll does; the client reads it.
    assert!(server.poll_readable(), "readiness is the one socket's");
    let mut b = [0u8; 16];
    assert_eq!(client.read_at(0, &mut b).unwrap(), 4);
    assert_eq!(&b[..4], b"ping");
    assert!(!server.poll_readable(), "and the read was the one socket's too");
    assert_eq!(client.last_from().map(|f| f.1), Some(sender.local().1));
    // A close in either process leaves the other's working.
    drop(server);
    sender.write_at(0, b"two").unwrap();
    assert!(client.poll_readable());
    assert_eq!(client.read_at(0, &mut b).unwrap(), 3);
    // A second hand-on is one more hold, and the last close frees it.
    let again = client.move_out().unwrap();
    let third = Socket::moved_proxy(&again, Arc::new(NoNet), Arc::clone(&res), Arc::clone(&mine), None).unwrap();
    drop(client);
    assert_eq!(third.local().1, port);
    drop(third);
    assert_eq!(global::live(), before, "every hold is given back");
}

#[test]
fn a_bound_socket_moves_with_its_port() {
    let _one = global::install();
    let res = Arc::new(Resolver::new());
    let ports = global::ports();
    let s = local_socket(Kind::Udp, &res, &ports);
    s.bind(0x7f00_0001, 0).unwrap();
    let port = s.local().1;
    let name = s.move_out().unwrap();
    let other = Socket::moved_proxy(&name, Arc::new(NoNet), Arc::clone(&res), Arc::clone(&ports), None).unwrap();
    assert_eq!(other.local().1, port, "the move keeps where it was bound");
    assert_eq!(s.local().1, port);
}

#[test]
fn a_connected_socket_still_goes_as_its_connection() {
    let _one = global::install();
    let (res, ports) = (Arc::new(Resolver::new()), Arc::new(Ports::new()));
    let l = local_socket(Kind::Tcp, &res, &ports);
    l.listen().unwrap();
    let c = local_socket(Kind::Tcp, &res, &ports);
    c.connect(0x7f00_0001, l.local().1).unwrap();
    assert!(c.move_out().is_none(), "a connection is carry's, not moved");
}

/// A network whose connections have names, as the page's Wisp streams do.
mod named {
    use gex_kernel::errno::Errno;
    use gex_kernel::net::{NetStack, Status, Transport};
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Debug, Default)]
    pub struct Conn {
        pub sent: Mutex<Vec<u8>>,
        pub holders: Mutex<u32>,
        /// A UDP flow's: each datagram sent comes back, as from an echo
        /// server.
        pub echo: Option<Mutex<std::collections::VecDeque<Vec<u8>>>>,
        /// The far end ended it.
        pub ended: Mutex<bool>,
    }

    fn conns() -> &'static Mutex<Vec<Arc<Conn>>> {
        static C: OnceLock<Mutex<Vec<Arc<Conn>>>> = OnceLock::new();
        C.get_or_init(Default::default)
    }

    pub fn conn(i: usize) -> Arc<Conn> {
        Arc::clone(&conns().lock().unwrap()[i])
    }

    /// How many connections and flows have been made, in every test.
    pub fn made() -> usize {
        conns().lock().unwrap().len()
    }

    #[derive(Debug)]
    struct End(usize, Arc<Conn>);

    impl Transport for End {
        fn connect(&self, _host: &str, _port: u16) -> Result<(), Errno> {
            Ok(())
        }
        fn status(&self) -> Status {
            if *self.1.ended.lock().unwrap() { Status::Closed } else { Status::Open }
        }
        fn send(&self, data: &[u8]) -> Result<usize, Errno> {
            self.1.sent.lock().unwrap().extend_from_slice(data);
            if let Some(q) = &self.1.echo {
                q.lock().unwrap().push_back(data.to_vec());
            }
            Ok(data.len())
        }
        fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            let d = self.1.echo.as_ref().and_then(|q| q.lock().unwrap().pop_front()).ok_or(Errno::EAGAIN)?;
            let n = d.len().min(buf.len());
            buf[..n].copy_from_slice(&d[..n]);
            Ok(n)
        }
        fn has_data(&self) -> bool {
            self.1.echo.as_ref().is_some_and(|q| !q.lock().unwrap().is_empty())
        }
        fn peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            let q = self.1.echo.as_ref().ok_or(Errno::EAGAIN)?.lock().unwrap();
            let d = q.front().ok_or(Errno::EAGAIN)?;
            let n = d.len().min(buf.len());
            buf[..n].copy_from_slice(&d[..n]);
            Ok(n)
        }
        fn available(&self) -> usize {
            self.1.echo.as_ref().and_then(|q| q.lock().unwrap().front().map(|d| d.len())).unwrap_or(0)
        }
        fn close(&self) {
            *self.1.holders.lock().unwrap() -= 1;
        }
        fn name(&self) -> Option<String> {
            Some(format!("named:{}", self.0))
        }
    }

    #[derive(Debug)]
    pub struct Net;

    impl NetStack for Net {
        fn tcp(&self) -> Result<Arc<dyn Transport>, Errno> {
            let c = Arc::new(Conn { holders: Mutex::new(1), ..Default::default() });
            let mut t = conns().lock().unwrap();
            t.push(Arc::clone(&c));
            Ok(Arc::new(End(t.len() - 1, c)))
        }
        fn udp(&self) -> Result<Arc<dyn Transport>, Errno> {
            let c = Arc::new(Conn { holders: Mutex::new(1), echo: Some(Default::default()), ..Default::default() });
            let mut t = conns().lock().unwrap();
            t.push(Arc::clone(&c));
            Ok(Arc::new(End(t.len() - 1, c)))
        }
        fn adopt(&self, name: &str) -> Option<Arc<dyn Transport>> {
            let i = name.strip_prefix("named:")?.parse::<usize>().ok()?;
            let c = conns().lock().unwrap().get(i)?.clone();
            *c.holders.lock().unwrap() += 1;
            Some(Arc::new(End(i, c)))
        }
    }
}

#[test]
fn a_moved_socket_connected_out_is_that_connection_in_every_holder() {
    let _one = global::install();
    let (res, ports) = (Arc::new(Resolver::new()), Arc::new(Ports::new()));
    let net = || Arc::new(named::Net) as Arc<dyn gex_kernel::net::NetStack>;
    let server = Arc::new(Socket::new(Kind::Tcp, net(), Arc::clone(&res), Arc::clone(&ports), None));
    server.set_self();
    let name = server.move_out().unwrap();
    let client = Socket::moved_proxy(&name, net(), Arc::clone(&res), Arc::clone(&ports), None).unwrap();
    // The server connects it out (the global kernel has no network): the
    // connection is made in the server's process.
    let _ = named::made();
    assert_eq!(server.connect(0x0a00_0001, 80), Err(gex_kernel::errno::Errno::EINPROGRESS));
    assert_eq!(server.peer(), Some((0x0a00_0001, 80)));
    // The client's next call takes it up, and its bytes go down it.
    assert_eq!(client.write_at(0, b"GET").unwrap(), 3);
    assert_eq!(client.peer(), Some((0x0a00_0001, 80)));
    assert_eq!(server.write_at(0, b" /").unwrap(), 2);
    let c = named::conn(named::made() - 1);
    assert_eq!(&*c.sent.lock().unwrap(), b"GET /");
    assert_eq!(*c.holders.lock().unwrap(), 2, "each holder holds it once");
    drop(server);
    drop(client);
    assert_eq!(*c.holders.lock().unwrap(), 0);
}

#[test]
fn udp_off_the_machine_is_a_flow_of_the_network() {
    use gex_kernel::errno::Errno;
    let _one = global::install();
    let (res, ports) = (Arc::new(Resolver::new()), Arc::new(Ports::new()));
    let s = Arc::new(Socket::new(Kind::Udp, Arc::new(named::Net), Arc::clone(&res), Arc::clone(&ports), None));
    s.set_self();
    s.set_peer(0x0a00_0001, 7);
    assert_eq!(s.write_at(0, b"ping").unwrap(), 4);
    let flow = named::conn(named::made() - 1);
    assert!(s.poll_readable(), "the echo is there to read");
    let mut b = [0u8; 16];
    assert_eq!(s.read_at(0, &mut b).unwrap(), 4);
    assert_eq!((&b[..4], s.last_from()), (&b"ping"[..], Some((0x0a00_0001, 7))));
    assert_ne!(s.local().1, 0, "the first datagram bound it");
    // A second datagram there goes down the same flow; another address is
    // another flow.
    let n = named::made();
    s.write_at(0, b"two").unwrap();
    assert_eq!(named::made(), n);
    s.set_peer(0x0a00_0002, 9);
    s.write_at(0, b"three").unwrap();
    assert_eq!(named::made(), n + 1);
    assert_eq!(s.read_at(0, &mut b).unwrap(), 3);
    assert_eq!(s.last_from(), Some((0x0a00_0001, 7)));
    assert_eq!(s.read_at(0, &mut b).unwrap(), 5);
    assert_eq!(s.last_from(), Some((0x0a00_0002, 9)));
    assert_eq!(s.read_at(0, &mut b), Err(Errno::EAGAIN));
    drop(s);
    assert_eq!(*flow.holders.lock().unwrap(), 0, "its flows close with it");
    // A network with no datagrams: unreachable, but DNS is still the
    // kernel's.
    let none = local_socket(Kind::Udp, &res, &ports);
    none.set_peer(0x0a00_0001, 7);
    assert_eq!(none.write_at(0, b"x"), Err(Errno::ENETUNREACH));
    none.set_peer(0x0101_0101, 53);
    let q = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x04test\x00\x00\x01\x00\x01";
    none.write_at(0, q).unwrap();
    let mut a = [0u8; 512];
    let n = none.read_at(0, &mut a).unwrap() as usize;
    assert!(n > q.len() && a[..2] == q[..2], "the kernel answers it");
}

#[test]
fn a_moved_udp_socket_reads_every_holders_flows() {
    let _one = global::install();
    let (res, ports) = (Arc::new(Resolver::new()), Arc::new(Ports::new()));
    let net = || Arc::new(named::Net) as Arc<dyn gex_kernel::net::NetStack>;
    let before = global::live();
    // A flow before the move stays the first holder's, and the others read
    // it too.
    let server = Arc::new(Socket::new(Kind::Udp, net(), Arc::clone(&res), Arc::clone(&ports), None));
    server.set_self();
    server.set_peer(0x0a00_0001, 7);
    server.write_at(0, b"early").unwrap();
    let early = named::conn(named::made() - 1);
    let name = server.move_out().unwrap();
    let client = Socket::moved_proxy(&name, net(), Arc::clone(&res), Arc::clone(&ports), None).unwrap();
    // As the wineserver polls and the client reads.
    assert!(client.poll_readable());
    let mut b = [0u8; 16];
    assert_eq!(client.read_at(0, &mut b).unwrap(), 5);
    assert_eq!(client.last_from(), Some((0x0a00_0001, 7)));
    // The client sends elsewhere: the global kernel has no network, so the
    // flow is the client's, and the server sees the reply.
    client.set_peer(0x0a00_0002, 9);
    assert_eq!(client.write_at(0, b"hi").unwrap(), 2);
    let flow = named::conn(named::made() - 1);
    assert!(server.poll_readable(), "every holder sees the flows ready");
    assert!(client.poll_readable(), "and a poll reads nothing");
    assert_eq!(server.read_at(0, &mut b).unwrap(), 2);
    assert_eq!((&b[..2], server.last_from()), (&b"hi"[..], Some((0x0a00_0002, 9))));
    assert!(!client.poll_readable(), "a datagram is read once");
    // The server's next datagram there goes down the same flow.
    let n = named::made();
    server.set_peer(0x0a00_0002, 9);
    server.write_at(0, b"again").unwrap();
    assert_eq!(named::made(), n);
    assert_eq!(&*flow.sent.lock().unwrap(), b"hiagain");
    assert_eq!(*flow.holders.lock().unwrap(), 2);
    drop(server);
    drop(client);
    assert_eq!((*flow.holders.lock().unwrap(), *early.holders.lock().unwrap()), (0, 0));
    assert_eq!(global::live(), before, "every hold is given back");
}

#[test]
fn a_moved_udp_sockets_flow_opened_again_reaches_every_holder() {
    let _one = global::install();
    let (res, ports) = (Arc::new(Resolver::new()), Arc::new(Ports::new()));
    let net = || Arc::new(named::Net) as Arc<dyn gex_kernel::net::NetStack>;
    let before = global::live();
    let server = Arc::new(Socket::new(Kind::Udp, net(), Arc::clone(&res), Arc::clone(&ports), None));
    server.set_self();
    let name = server.move_out().unwrap();
    let client = Socket::moved_proxy(&name, net(), Arc::clone(&res), Arc::clone(&ports), None).unwrap();
    let mut b = [0u8; 16];
    // The far end ends the flow, again and again: each time the client's
    // next datagram there opens another, and the server reads its reply off
    // it, the table one entry for the one far address.
    let mut flows = Vec::new();
    for round in 0..40u8 {
        client.set_peer(0x0a00_0003, 5300);
        let n = named::made();
        assert_eq!(client.write_at(0, &[round]).unwrap(), 1);
        assert_eq!(named::made(), n + 1, "round {round}: a flow opened");
        let flow = named::conn(n);
        if round % 2 == 0 {
            assert!(server.poll_readable(), "round {round}: the server sees the new flow");
        } else {
            assert_eq!(server.available(), Some(1), "round {round}: FIONREAD sees the new flow");
        }
        assert_eq!(server.read_at(0, &mut b).unwrap(), 1);
        assert_eq!((b[0], server.last_from()), (round, Some((0x0a00_0003, 5300))));
        assert!(!client.poll_readable(), "round {round}: read once");
        *flow.ended.lock().unwrap() = true;
        flows.push(flow);
    }
    // Two holders at once: each opens its own flow there, and whatever
    // comes back on either is read, once.
    let n = named::made();
    server.set_peer(0x0a00_0004, 7);
    client.set_peer(0x0a00_0004, 7);
    server.write_at(0, b"s").unwrap();
    client.write_at(0, b"c").unwrap();
    let mut got = Vec::new();
    for _ in 0..4 {
        for h in [&server, &client] {
            if h.poll_readable() {
                let k = h.read_at(0, &mut b).unwrap() as usize;
                got.push(b[..k].to_vec());
            }
        }
    }
    got.sort();
    assert_eq!(got, vec![b"c".to_vec(), b"s".to_vec()]);
    assert!(named::made() <= n + 2);
    drop(server);
    drop(client);
    for f in &flows {
        assert_eq!(*f.holders.lock().unwrap(), 0);
    }
    assert_eq!(global::live(), before, "every hold is given back");
}
