//! Internet sockets moved to the global kernel (`net::Socket::move_out`): one
//! socket held by two processes, as Wine's wineserver and its client hold
//! every socket. The global kernel's side is played by a table here, in this
//! process's memory (`shims/kernel` keeps the same table in the shared one).

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
    }

    fn conns() -> &'static Mutex<Vec<Arc<Conn>>> {
        static C: OnceLock<Mutex<Vec<Arc<Conn>>>> = OnceLock::new();
        C.get_or_init(Default::default)
    }

    pub fn conn(i: usize) -> Arc<Conn> {
        Arc::clone(&conns().lock().unwrap()[i])
    }

    #[derive(Debug)]
    struct End(usize, Arc<Conn>);

    impl Transport for End {
        fn connect(&self, _host: &str, _port: u16) -> Result<(), Errno> {
            Ok(())
        }
        fn status(&self) -> Status {
            Status::Open
        }
        fn send(&self, data: &[u8]) -> Result<usize, Errno> {
            self.1.sent.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn recv(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::EAGAIN)
        }
        fn has_data(&self) -> bool {
            false
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
    assert_eq!(server.connect(0x0a00_0001, 80), Err(gex_kernel::errno::Errno::EINPROGRESS));
    assert_eq!(server.peer(), Some((0x0a00_0001, 80)));
    // The client's next call takes it up, and its bytes go down it.
    assert_eq!(client.write_at(0, b"GET").unwrap(), 3);
    assert_eq!(client.peer(), Some((0x0a00_0001, 80)));
    assert_eq!(server.write_at(0, b" /").unwrap(), 2);
    let c = named::conn(0);
    assert_eq!(&*c.sent.lock().unwrap(), b"GET /");
    assert_eq!(*c.holders.lock().unwrap(), 2, "each holder holds it once");
    drop(server);
    drop(client);
    assert_eq!(*c.holders.lock().unwrap(), 0);
}
