//! `socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)`: the dumps that list the
//! machine's network interfaces and their addresses, and nothing else.
//!
//! musl's `if_nameindex`, `if_nametoindex` and `getifaddrs` read them (its
//! `__rtnetlink_enumerate`): an `RTM_GETLINK` dump, then an `RTM_GETADDR`
//! one. With no route socket every one failed, `if_nameindex` with
//! `ENOBUFS`, and Wine's nsi (the table behind `GetAdaptersAddresses`,
//! `GetIfTable` and the rest of iphlpapi) logged
//! `if_nameindex failed, errno 105` and knew no interface at all -- not even
//! the loopback one every Windows machine has.
//!
//! The machine here has two: `lo` (127.0.0.1/8) and `eth0` (10.0.2.15/24,
//! the address QEMU's user network gives a guest). The guest's network is
//! the kernel's own in both modes (crate::net): it answers DNS with
//! synthetic addresses and hands each connection to the host by name -- a
//! host TCP socket natively, a Wisp stream in a page -- so the guest never
//! sees the host's interfaces or addresses, natively either. `eth0` is that
//! network's interface, the address a connected socket reports
//! (`getsockname`, [`local_for`]). IPv4 only, as that network is. glibc's `getaddrinfo` (`AI_ADDRCONFIG`)
//! reads the same dump and counts a non-loopback IPv4 address as "IPv4 is
//! there", so `eth0` keeps its A lookups, and with no IPv6 address it now
//! asks for no AAAA either.
//!
//! A request is answered as it is written: the dump, as one datagram ended by
//! `NLMSG_DONE`, waits for the next read. Any other request gets
//! `NLMSG_ERROR` with `EOPNOTSUPP` (when it asked for an acknowledgement or a
//! dump); nothing on this socket changes anything.

use crate::errno::{Errno, SysResult};
use crate::vfs::{FileHandle, FileType, Metadata};
use std::collections::VecDeque;
use std::sync::Mutex;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const IFF_LOWER_UP: u32 = 0x10000;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_HOST: u8 = 254;
const IFA_F_PERMANENT: u8 = 0x80;

/// One interface: its index, name, type, flags, MTU, hardware address, and
/// its IPv4 address with prefix length.
struct Iface {
    index: u32,
    name: &'static str,
    kind: u16,
    flags: u32,
    mtu: u32,
    mac: [u8; 6],
    addr: [u8; 4],
    prefix: u8,
}

/// eth0's address: where a connected socket says it is (`getsockname`).
pub const ETH0_ADDR: [u8; 4] = [10, 0, 2, 15];

/// The address a socket connected to `peer` (host order) has: lo's for a
/// loopback peer, eth0's for any other. What `getsockname` reports for a
/// connected socket not bound to an address of its own.
pub fn local_for(peer: u32) -> u32 {
    if peer >> 24 == 127 {
        0x7f00_0001
    } else {
        u32::from_be_bytes(ETH0_ADDR)
    }
}

const IFACES: [Iface; 2] = [
    Iface {
        index: 1,
        name: "lo",
        kind: ARPHRD_LOOPBACK,
        flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
        mtu: 65536,
        mac: [0; 6],
        addr: [127, 0, 0, 1],
        prefix: 8,
    },
    Iface {
        index: 2,
        name: "eth0",
        kind: ARPHRD_ETHER,
        flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST | IFF_LOWER_UP,
        mtu: 1500,
        mac: [0x02, 0, 0, 0, 0, 0x01],
        addr: ETH0_ADDR,
        prefix: 24,
    },
];

/// A route socket: the replies written requests are owed, a datagram each.
#[derive(Debug)]
pub struct NetlinkRoute {
    replies: Mutex<VecDeque<Vec<u8>>>,
    /// Who waits on it (`crate::wait`): a request makes a reply.
    queue: crate::wait::Queue,
}

impl Default for NetlinkRoute {
    fn default() -> Self {
        Self::new()
    }
}

/// Appends a `struct nlmsghdr` and `body`, padded to 4 bytes.
fn message(out: &mut Vec<u8>, kind: u16, flags: u16, seq: u32, body: &[u8]) {
    let len = 16 + body.len();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // from the kernel
    out.extend_from_slice(body);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// Appends a `struct rtattr` holding `data`, padded to 4 bytes.
fn attr(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
    out.extend_from_slice(&((4 + data.len()) as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// `RTM_NEWLINK` for each interface: `struct ifinfomsg` and its attributes.
fn links(out: &mut Vec<u8>, seq: u32) {
    for i in &IFACES {
        let mut b = vec![AF_UNSPEC, 0];
        b.extend_from_slice(&i.kind.to_le_bytes());
        b.extend_from_slice(&(i.index as i32).to_le_bytes());
        b.extend_from_slice(&i.flags.to_le_bytes());
        b.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // ifi_change
        attr(&mut b, IFLA_IFNAME, &cstr(i.name));
        attr(&mut b, IFLA_MTU, &i.mtu.to_le_bytes());
        attr(&mut b, IFLA_ADDRESS, &i.mac);
        let bcast = if i.kind == ARPHRD_ETHER { [0xff; 6] } else { [0; 6] };
        attr(&mut b, IFLA_BROADCAST, &bcast);
        message(out, RTM_NEWLINK, NLM_F_MULTI, seq, &b);
    }
}

/// `RTM_NEWADDR` for each interface's address: `struct ifaddrmsg` and its
/// attributes.
fn addrs(out: &mut Vec<u8>, seq: u32) {
    for i in &IFACES {
        let scope = if i.flags & IFF_LOOPBACK != 0 { RT_SCOPE_HOST } else { RT_SCOPE_UNIVERSE };
        let mut b = vec![AF_INET, i.prefix, IFA_F_PERMANENT, scope];
        b.extend_from_slice(&i.index.to_le_bytes());
        attr(&mut b, IFA_ADDRESS, &i.addr);
        attr(&mut b, IFA_LOCAL, &i.addr);
        if i.flags & IFF_BROADCAST != 0 {
            let host = u32::MAX >> i.prefix;
            let bcast = (u32::from_be_bytes(i.addr) | host).to_be_bytes();
            attr(&mut b, IFA_BROADCAST, &bcast);
        }
        attr(&mut b, IFA_LABEL, &cstr(i.name));
        message(out, RTM_NEWADDR, NLM_F_MULTI, seq, &b);
    }
}

impl NetlinkRoute {
    pub fn new() -> Self {
        NetlinkRoute { replies: Mutex::new(VecDeque::new()), queue: crate::wait::Queue::new() }
    }

    /// The reply to one request (`nlmsghdr` + body), if it is owed one.
    fn answer(req: &[u8]) -> Option<Vec<u8>> {
        let kind = u16::from_le_bytes(req[4..6].try_into().unwrap());
        let flags = u16::from_le_bytes(req[6..8].try_into().unwrap());
        let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
        // rtgenmsg / ifinfomsg / ifaddrmsg all start with the family.
        let family = req.get(16).copied().unwrap_or(AF_UNSPEC);
        let mut out = Vec::new();
        let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
        match kind {
            RTM_GETLINK if dump => links(&mut out, seq),
            // No IPv6 addresses: a dump for AF_INET6 is only its end.
            RTM_GETADDR if dump && (family == AF_UNSPEC || family == AF_INET) => addrs(&mut out, seq),
            RTM_GETADDR if dump => {}
            _ => {
                if flags & (NLM_F_ACK | NLM_F_DUMP) == 0 {
                    return None;
                }
                let mut b = (-(Errno::EOPNOTSUPP.0 as i32)).to_le_bytes().to_vec();
                b.extend_from_slice(&req[..16]);
                message(&mut out, NLMSG_ERROR, 0, seq, &b);
                return Some(out);
            }
        }
        message(&mut out, NLMSG_DONE, NLM_F_MULTI, seq, &0i32.to_le_bytes());
        Some(out)
    }
}

impl FileHandle for NetlinkRoute {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        let mut q = self.replies.lock().unwrap();
        let Some(d) = q.pop_front() else {
            return Err(Errno::EAGAIN);
        };
        // A datagram: what does not fit is dropped (the caller sees a short,
        // whole-buffer read, as with MSG_TRUNC).
        let n = d.len().min(buf.len());
        buf[..n].copy_from_slice(&d[..n]);
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        let mut at = 0;
        let mut out = Vec::new();
        while at + 16 <= buf.len() {
            let len = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
            if len < 16 || at + len > buf.len() {
                return Err(Errno::EINVAL);
            }
            let flags = u16::from_le_bytes(buf[at + 6..at + 8].try_into().unwrap());
            if flags & NLM_F_REQUEST != 0 {
                if let Some(r) = Self::answer(&buf[at..at + len]) {
                    out.extend_from_slice(&r);
                }
            }
            at += (len + 3) & !3;
        }
        if !out.is_empty() {
            self.replies.lock().unwrap().push_back(out);
            self.queue.notify();
        }
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Fifo,
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
        !self.replies.lock().unwrap().is_empty()
    }

    fn wait_queue(&self) -> u32 {
        self.queue.id()
    }

    fn wakes(&self) -> Option<u64> {
        Some(self.queue.wakes())
    }

    fn poll_writable(&self) -> bool {
        true
    }

    fn available(&self) -> Option<u64> {
        Some(self.replies.lock().unwrap().front().map_or(0, |d| d.len() as u64))
    }
}

/// The interface requests asked of any socket (`struct ifreq`): by name
/// (`SIOCGIFINDEX`, the flags, MTU, hardware and IPv4 addresses) or by index
/// (`SIOCGIFNAME`). musl's and glibc's `if_nametoindex` and `if_indextoname`
/// are these, and Wine's nsi asks the flags, MTU and hardware address of
/// each interface `if_nameindex` gave it.
pub mod ioctl {
    pub const SIOCGIFNAME: u64 = 0x8910;
    pub const SIOCGIFFLAGS: u64 = 0x8913;
    pub const SIOCGIFADDR: u64 = 0x8915;
    pub const SIOCGIFBRDADDR: u64 = 0x8919;
    pub const SIOCGIFNETMASK: u64 = 0x891b;
    pub const SIOCGIFMTU: u64 = 0x8921;
    pub const SIOCGIFHWADDR: u64 = 0x8927;
    pub const SIOCGIFINDEX: u64 = 0x8933;
}

/// Whether `req` is one of [`ioctl`]'s.
pub fn is_iface_ioctl(req: u64) -> bool {
    use ioctl::*;
    matches!(req, SIOCGIFNAME | SIOCGIFFLAGS | SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFINDEX)
}

/// Answers one of [`ioctl`]'s in `ifr`, the first 32 bytes of a `struct
/// ifreq` (the name, then the union's first 16 bytes: the same on 32- and
/// 64-bit guests). `ENODEV` for an interface that is not here.
pub fn iface_ioctl(req: u64, ifr: &mut [u8; 32]) -> Result<(), Errno> {
    use ioctl::*;
    let iface = if req == SIOCGIFNAME {
        let index = i32::from_le_bytes(ifr[16..20].try_into().unwrap());
        IFACES.iter().find(|i| i.index as i32 == index)
    } else {
        let name = ifr[..16].split(|&b| b == 0).next().unwrap_or(&[]);
        IFACES.iter().find(|i| i.name.as_bytes() == name)
    };
    let i = iface.ok_or(Errno::ENODEV)?;
    let inet = |ifr: &mut [u8; 32], a: [u8; 4]| {
        ifr[16..32].fill(0);
        ifr[16..18].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        ifr[20..24].copy_from_slice(&a);
    };
    let mask = (u32::MAX << (32 - i.prefix as u32)).to_be_bytes();
    match req {
        SIOCGIFNAME => {
            ifr[..16].fill(0);
            ifr[..i.name.len()].copy_from_slice(i.name.as_bytes());
        }
        SIOCGIFINDEX => ifr[16..20].copy_from_slice(&(i.index as i32).to_le_bytes()),
        // ifr_flags is a short: the low 16 bits (IFF_LOWER_UP is not among them).
        SIOCGIFFLAGS => ifr[16..18].copy_from_slice(&(i.flags as u16).to_le_bytes()),
        SIOCGIFMTU => ifr[16..20].copy_from_slice(&(i.mtu as i32).to_le_bytes()),
        SIOCGIFHWADDR => {
            ifr[16..32].fill(0);
            ifr[16..18].copy_from_slice(&i.kind.to_le_bytes());
            ifr[18..24].copy_from_slice(&i.mac);
        }
        SIOCGIFADDR => inet(ifr, i.addr),
        SIOCGIFNETMASK => inet(ifr, mask),
        SIOCGIFBRDADDR => {
            if i.flags & IFF_BROADCAST == 0 {
                return Err(Errno::EINVAL);
            }
            let b = u32::from_be_bytes(i.addr) | !u32::from_be_bytes(mask);
            inet(ifr, b.to_be_bytes());
        }
        _ => return Err(Errno::ENOTTY),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: u16, flags: u16, seq: u32, family: u8) -> Vec<u8> {
        let mut v = Vec::new();
        message(&mut v, kind, flags, seq, &[family, 0, 0, 0]);
        v
    }

    /// (type, seq, body) of each message in a datagram.
    fn parse(d: &[u8]) -> Vec<(u16, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 16 <= d.len() {
            let len = u32::from_le_bytes(d[at..at + 4].try_into().unwrap()) as usize;
            let kind = u16::from_le_bytes(d[at + 4..at + 6].try_into().unwrap());
            let seq = u32::from_le_bytes(d[at + 8..at + 12].try_into().unwrap());
            out.push((kind, seq, d[at + 16..at + len].to_vec()));
            at += (len + 3) & !3;
        }
        out
    }

    /// The rtattrs after a fixed header of `skip` bytes, as (type, data).
    fn attrs(body: &[u8], skip: usize) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = skip;
        while at + 4 <= body.len() {
            let len = u16::from_le_bytes(body[at..at + 2].try_into().unwrap()) as usize;
            let kind = u16::from_le_bytes(body[at + 2..at + 4].try_into().unwrap());
            out.push((kind, body[at + 4..at + len].to_vec()));
            at += (len + 3) & !3;
        }
        out
    }

    fn reply(s: &NetlinkRoute) -> Vec<u8> {
        let mut buf = vec![0u8; 8192];
        let n = s.read_at(0, &mut buf).unwrap() as usize;
        buf.truncate(n);
        buf
    }

    /// What musl's `if_nameindex` does: a link dump, then an address dump;
    /// it wants every interface by index and name, and each dump's end.
    #[test]
    fn a_link_dump_names_lo_and_eth0() {
        let s = NetlinkRoute::new();
        assert!(!s.poll_readable());
        let req = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 7, AF_UNSPEC);
        assert_eq!(s.write_at(0, &req).unwrap(), req.len() as u64);
        assert!(s.poll_readable());
        let msgs = parse(&reply(&s));
        assert_eq!(msgs.len(), 3);
        assert!(msgs.iter().all(|m| m.1 == 7));
        assert_eq!(msgs[2].0, NLMSG_DONE);
        let names: Vec<(i32, Vec<u8>)> = msgs[..2]
            .iter()
            .map(|(k, _, b)| {
                assert_eq!(*k, RTM_NEWLINK);
                let index = i32::from_le_bytes(b[4..8].try_into().unwrap());
                let name = attrs(b, 16).into_iter().find(|a| a.0 == IFLA_IFNAME).unwrap().1;
                (index, name)
            })
            .collect();
        assert_eq!(names, vec![(1, b"lo\0".to_vec()), (2, b"eth0\0".to_vec())]);
        assert!(!s.poll_readable());
        assert_eq!(s.read_at(0, &mut [0u8; 64]).err(), Some(Errno::EAGAIN));
    }

    #[test]
    fn an_address_dump_has_the_ipv4_addresses_and_no_ipv6() {
        let s = NetlinkRoute::new();
        s.write_at(0, &request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 2, AF_UNSPEC)).unwrap();
        let msgs = parse(&reply(&s));
        assert_eq!(msgs.len(), 3);
        let got: Vec<(u8, u8, u32, Vec<u8>)> = msgs[..2]
            .iter()
            .map(|(k, _, b)| {
                assert_eq!(*k, RTM_NEWADDR);
                let local = attrs(b, 8).into_iter().find(|a| a.0 == IFA_LOCAL).unwrap().1;
                (b[0], b[1], u32::from_le_bytes(b[4..8].try_into().unwrap()), local)
            })
            .collect();
        assert_eq!(got, vec![(AF_INET, 8, 1, vec![127, 0, 0, 1]), (AF_INET, 24, 2, vec![10, 0, 2, 15])]);
        s.write_at(0, &request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 3, 10)).unwrap();
        let msgs = parse(&reply(&s));
        assert_eq!(msgs.iter().map(|m| m.0).collect::<Vec<_>>(), vec![NLMSG_DONE]);
    }

    #[test]
    fn anything_else_is_refused_when_it_wants_an_answer() {
        let s = NetlinkRoute::new();
        s.write_at(0, &request(24, NLM_F_REQUEST | NLM_F_ACK, 9, AF_INET)).unwrap();
        let msgs = parse(&reply(&s));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, NLMSG_ERROR);
        assert_eq!(i32::from_le_bytes(msgs[0].2[..4].try_into().unwrap()), -(Errno::EOPNOTSUPP.0 as i32));
        s.write_at(0, &request(24, NLM_F_REQUEST, 10, AF_INET)).unwrap();
        assert!(!s.poll_readable());
    }

    fn ifr(name: &str) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[..name.len()].copy_from_slice(name.as_bytes());
        r
    }

    #[test]
    fn interface_ioctls_answer_by_name_and_index() {
        let mut r = ifr("eth0");
        iface_ioctl(ioctl::SIOCGIFINDEX, &mut r).unwrap();
        assert_eq!(i32::from_le_bytes(r[16..20].try_into().unwrap()), 2);
        let mut r = [0u8; 32];
        r[16..20].copy_from_slice(&1i32.to_le_bytes());
        iface_ioctl(ioctl::SIOCGIFNAME, &mut r).unwrap();
        assert_eq!(&r[..3], b"lo\0");
        let mut r = ifr("lo");
        iface_ioctl(ioctl::SIOCGIFFLAGS, &mut r).unwrap();
        assert_ne!(u16::from_le_bytes(r[16..18].try_into().unwrap()) as u32 & IFF_LOOPBACK, 0);
        let mut r = ifr("eth0");
        iface_ioctl(ioctl::SIOCGIFADDR, &mut r).unwrap();
        assert_eq!(&r[20..24], &[10, 0, 2, 15]);
        iface_ioctl(ioctl::SIOCGIFNETMASK, &mut r).unwrap();
        assert_eq!(&r[20..24], &[255, 255, 255, 0]);
        iface_ioctl(ioctl::SIOCGIFBRDADDR, &mut r).unwrap();
        assert_eq!(&r[20..24], &[10, 0, 2, 255]);
        let mut r = ifr("eth0");
        iface_ioctl(ioctl::SIOCGIFHWADDR, &mut r).unwrap();
        assert_eq!(u16::from_le_bytes(r[16..18].try_into().unwrap()), ARPHRD_ETHER);
        assert_eq!(&r[18..24], &[2, 0, 0, 0, 0, 1]);
        let mut r = ifr("lo");
        iface_ioctl(ioctl::SIOCGIFMTU, &mut r).unwrap();
        assert_eq!(i32::from_le_bytes(r[16..20].try_into().unwrap()), 65536);
        assert_eq!(iface_ioctl(ioctl::SIOCGIFINDEX, &mut ifr("wlan0")).err(), Some(Errno::ENODEV));
    }

    #[test]
    fn a_connected_socket_is_on_lo_or_eth0() {
        assert_eq!(local_for(0x7f00_0001), 0x7f00_0001);
        assert_eq!(local_for(0x7f12_3456), 0x7f00_0001);
        assert_eq!(local_for(0xf000_0001).to_be_bytes(), ETH0_ADDR);
    }
}
