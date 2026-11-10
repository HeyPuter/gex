//! Unix98 pseudoterminals.
//!
//! A pty is a master/slave pair whose two ends usually live in different
//! processes -- xterm holds the master, the shell it started holds the slave.
//! With a Worker (and a memory) per process that means the pair cannot be an
//! `Arc` shared by pointer, exactly as a pipe cannot: it lives in the global
//! kernel addressed by a number, and bytes cross by being copied between the
//! two processes' memories. See [`crate::pipe`] for the same shape without the
//! terminal.
//!
//! Unlike a pipe, the slave *is* a terminal, so the two directions are not
//! symmetric. Bytes the master writes are terminal input: they run through the
//! line discipline ([`crate::tty::Tty`]) -- canonical line editing, echo, and
//! `^C`-style signal characters -- before the slave may read the cooked result,
//! and the echo goes back to the master so what you type appears. Bytes the
//! slave writes are program output: they get output processing (`OPOST`/
//! `ONLCR`) and go straight to the master. The single [`Tty`] holds the shared
//! termios, window size, foreground process group and session for the pair.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use crate::errno::{Errno, SysResult};
use crate::tty::Tty;

struct State {
    /// Cooked terminal input, waiting for the slave to read. In canonical mode
    /// a whole line lands at once; in raw mode every byte as it arrives.
    to_slave: VecDeque<u8>,
    /// Echoed keystrokes and program output, waiting for the master to read.
    to_master: VecDeque<u8>,
    /// The pts number: the `N` in `/dev/pts/N`.
    ptn: u32,
    /// `TIOCSPTLCK`. A fresh pty is locked; `unlockpt` clears it before the
    /// slave may be opened, which is how glibc guards against a slave opened
    /// before the master has set it up.
    locked: bool,
    /// How many descriptors hold each end, so the last close of one is what the
    /// other sees as end-of-file (the master) or hangup (the slave).
    master_open: u32,
    slave_open: u32,
    /// Whether a slave was ever opened. Before the first open, a master read of
    /// an empty buffer waits rather than seeing a false end-of-file.
    slave_ever: bool,
    /// `^D` on an empty line: the slave's next read returns end-of-file.
    slave_eof: bool,
}

/// A pseudoterminal pair, living wherever a pipe would -- the global kernel in a
/// browser run, this process otherwise.
pub struct Pty {
    /// The shared terminal: termios, window size, foreground group, session and
    /// the line discipline itself.
    pub tty: Tty,
    st: Mutex<State>,
    /// Who waits on either end (`crate::wait`).
    queue: crate::wait::Queue,
}

impl Pty {
    /// A fresh, locked pair with `ptn` as its slave number and the master open.
    pub fn new(ptn: u32) -> Pty {
        Pty {
            tty: Tty::new(),
            st: Mutex::new(State {
                to_slave: VecDeque::new(),
                to_master: VecDeque::new(),
                ptn,
                locked: true,
                master_open: 1,
                slave_open: 0,
                slave_ever: false,
                slave_eof: false,
            }),
            queue: crate::wait::Queue::new(),
        }
    }

    /// The pair's wait queue.
    pub fn queue_id(&self) -> u32 {
        self.queue.id()
    }

    /// Something changed that a reader on either end may be waiting for: the
    /// terminal's foreground group, say, which decides whose read may go on.
    pub fn notify(&self) {
        self.queue.notify();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ptn(&self) -> u32 {
        self.lock().ptn
    }
    pub fn locked(&self) -> bool {
        self.lock().locked
    }
    pub fn set_locked(&self, v: bool) {
        self.lock().locked = v;
    }

    /// The master wrote: terminal input. Runs the line discipline, queues the
    /// cooked result for the slave and the echo for the master, and returns the
    /// signals the caller must deliver to the foreground group (`^C` etc.).
    pub fn master_write(&self, bytes: &[u8]) -> Vec<u32> {
        let r = self.tty.input(bytes);
        let mut s = self.lock();
        // A signal character discards the half-line typed before it, so the
        // abandoned bytes are not handed to the slave after the `^C`.
        if r.flush {
            s.to_slave.clear();
        }
        s.to_slave.extend(r.to_reader);
        s.to_master.extend(r.echo);
        if r.eof {
            s.slave_eof = true;
        }
        drop(s);
        self.queue.notify();
        r.signals
    }

    /// Set the terminal's termios. A line part-typed in canonical mode stays
    /// input when the mode is left (`Tty::set_termios_keeping`): the slave
    /// reads it next.
    pub fn set_termios(&self, src: &[u8]) {
        let kept = self.tty.set_termios_keeping(src);
        if !kept.is_empty() {
            self.lock().to_slave.extend(kept);
            self.queue.notify();
        }
    }

    /// The slave wrote: program output. Output-processed, then queued for the
    /// master.
    pub fn slave_write(&self, bytes: &[u8]) {
        let out = self.tty.output(bytes);
        let mut s = self.lock();
        match out {
            Some(v) => s.to_master.extend(v),
            None => s.to_master.extend(bytes.iter().copied()),
        }
        drop(s);
        self.queue.notify();
    }

    /// The master reads echo and program output. Empty and every slave gone is
    /// end-of-file; empty with a slave still open waits.
    pub fn master_read(&self, buf: &mut [u8]) -> SysResult {
        let mut s = self.lock();
        if s.to_master.is_empty() {
            // Every slave closed: a hangup, which Linux reports to the master
            // as EIO rather than end-of-file (`os.read` on a pty.fork child
            // that has exited, xterm noticing its shell is gone).
            return if s.slave_ever && s.slave_open == 0 {
                Err(Errno::EIO)
            } else {
                Err(Errno::EAGAIN)
            };
        }
        Ok(drain(&mut s.to_master, buf))
    }

    /// The slave reads cooked input. Empty with the master gone, or after `^D`,
    /// is end-of-file; empty otherwise waits.
    pub fn slave_read(&self, buf: &mut [u8]) -> SysResult {
        let mut s = self.lock();
        if s.to_slave.is_empty() {
            return if s.slave_eof || s.master_open == 0 {
                s.slave_eof = false;
                Ok(0)
            } else {
                Err(Errno::EAGAIN)
            };
        }
        Ok(drain(&mut s.to_slave, buf))
    }

    pub fn master_readable(&self) -> bool {
        let s = self.lock();
        !s.to_master.is_empty() || (s.slave_ever && s.slave_open == 0)
    }
    pub fn slave_readable(&self) -> bool {
        let s = self.lock();
        !s.to_slave.is_empty() || s.slave_eof || s.master_open == 0
    }
    pub fn master_available(&self) -> u64 {
        self.lock().to_master.len() as u64
    }
    pub fn slave_available(&self) -> u64 {
        self.lock().to_slave.len() as u64
    }

    /// `TCFLSH`: drop queued bytes in one or both directions.
    pub fn flush(&self, which: i32) {
        let mut s = self.lock();
        // TCIFLUSH 0 = input, TCOFLUSH 1 = output, TCIOFLUSH 2 = both.
        if which != 1 {
            s.to_slave.clear();
        }
        if which != 0 {
            s.to_master.clear();
        }
        drop(s);
        self.tty.flush_line();
        self.queue.notify();
    }

    pub fn open_master(&self) {
        self.lock().master_open += 1;
    }
    pub fn open_slave(&self) {
        let mut s = self.lock();
        s.slave_open += 1;
        s.slave_ever = true;
    }
    /// Drop a master; returns true when it was the last -- the slave then hangs
    /// up, and the caller signals `SIGHUP` to its foreground group.
    pub fn close_master(&self) -> bool {
        let mut s = self.lock();
        s.master_open = s.master_open.saturating_sub(1);
        let last = s.master_open == 0;
        drop(s);
        self.queue.notify();
        last
    }
    pub fn close_slave(&self) {
        let mut s = self.lock();
        s.slave_open = s.slave_open.saturating_sub(1);
        drop(s);
        self.queue.notify();
    }
    pub fn master_count(&self) -> u32 {
        self.lock().master_open
    }
    pub fn slave_count(&self) -> u32 {
        self.lock().slave_open
    }
}

fn drain(q: &mut VecDeque<u8>, buf: &mut [u8]) -> u64 {
    let n = buf.len().min(q.len());
    for b in buf.iter_mut().take(n) {
        *b = q.pop_front().unwrap();
    }
    n as u64
}

/// `st_rdev` of `/dev/pts/N` (major 136), in Linux's encoding.
pub const fn pts_rdev(n: u32) -> u64 {
    let (major, minor) = (136u64, n as u64);
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// `st_rdev` of `/dev/ptmx` (5:2).
pub const PTMX_RDEV: u64 = (5 << 8) | 2;

/// `st_ino` of `/dev/pts/N`: the same from `stat` on the name and `fstat` on
/// a descriptor, which is how `ttyname` recognises the one as the other.
pub const fn pts_ino(n: u32) -> u64 {
    0x7074_0000 + n as u64
}

/// `st_ino` of `/dev/ptmx`.
pub const PTMX_INO: u64 = 0x7074_ffff;

/// What `/proc/<pid>/fd/N` says for a pty end gex names `pty:[id:m|s]`
/// internally: `/dev/ptmx` for a master, `/dev/pts/N` for a slave (the id is
/// the pts number). Anything else as it is.
pub fn link_name(path: &str) -> String {
    let parsed = path
        .strip_prefix("pty:[")
        .and_then(|v| v.strip_suffix(']'))
        .and_then(|v| v.split_once(':'))
        .and_then(|(id, side)| Some((id.parse::<u32>().ok()?, side)));
    match parsed {
        Some((_, "m")) => "/dev/ptmx".to_string(),
        Some((n, "s")) => format!("/dev/pts/{n}"),
        _ => path.to_string(),
    }
}

/// `/proc/<pid>/stat`'s `tty_nr` for a controlling terminal that is
/// `/dev/pts/N`: the device number, as an int.
pub const fn tty_nr(n: u32) -> i32 {
    pts_rdev(n) as i32
}

/// A pty pair that lives in another wasm module, reached by number and a table
/// of `extern "C"` calls -- the [`crate::pipe::Remote`] shape, with the terminal
/// added (natively, the pairs live in the host process: `hosts/native`'s
/// `pty.rs`). `id` names the pair; `master` picks the end. Data calls take the
/// kernel's own buffer and return bytes or `-errno`.
#[derive(Clone, Copy)]
pub struct Remote {
    /// Open `/dev/ptmx`: make a fresh locked pair with the master open. Returns
    /// `(id << 16) | ptn`, or `-1` if none can be made.
    pub open_master: extern "C" fn() -> i32,
    /// Open `/dev/pts/N`: take a slave end. Returns the pair's id, `-1` if there
    /// is no such pts, or `-EIO` if it is still locked.
    pub open_slave: extern "C" fn(ptn: i32) -> i32,
    /// Another descriptor onto one end, as `dup` and `fork` make.
    pub open_end: extern "C" fn(id: i32, master: i32),
    /// Drop a descriptor. The last on an end is the other end's EOF/hangup.
    pub close_end: extern "C" fn(id: i32, master: i32),
    pub read: fn(id: i32, master: i32, buf: &mut [u8]) -> i32,
    pub write: fn(id: i32, master: i32, buf: &[u8]) -> i32,
    pub readable: extern "C" fn(id: i32, master: i32) -> i32,
    pub available: extern "C" fn(id: i32, master: i32) -> i32,
    pub ptn: extern "C" fn(id: i32) -> i32,
    pub get_lock: extern "C" fn(id: i32) -> i32,
    pub set_lock: extern "C" fn(id: i32, locked: i32),
    /// The shared terminal state, in the byte shapes the ioctls exchange.
    pub get_termios: fn(id: i32, buf: &mut [u8]) -> i32,
    pub set_termios: fn(id: i32, src: &[u8], flush: i32) -> i32,
    pub winsize: extern "C" fn(id: i32) -> u64,
    /// Sets the window size and, when it changed, delivers `SIGWINCH` to the
    /// foreground group itself (only the global kernel can, being next to the
    /// process table). Returns 1 when it changed.
    pub set_winsize: extern "C" fn(id: i32, packed: u64) -> i32,
    pub fg: extern "C" fn(id: i32) -> i32,
    /// Which pty a session has claimed as its controlling terminal, or -1.
    /// This is what `/dev/tty` resolves to inside a terminal emulator.
    pub for_sid: extern "C" fn(sid: i32) -> i32,
    pub set_fg: extern "C" fn(id: i32, pgrp: i32),
    pub sid: extern "C" fn(id: i32) -> i32,
    pub set_sid: extern "C" fn(id: i32, sid: i32),
    pub flush: extern "C" fn(id: i32, which: i32),
    /// The pair's wait queue (`crate::wait`).
    pub queue: extern "C" fn(id: i32) -> u32,
}

/// One end of a pty in the global kernel. Dropping closes that end there, so a
/// Worker that exits releases its end just as a `close` would.
pub struct RemotePty {
    pub id: i32,
    pub master: bool,
    pub remote: Remote,
}

impl std::fmt::Debug for RemotePty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RemotePty({}, {})",
            self.id,
            if self.master { "m" } else { "s" }
        )
    }
}

impl Drop for RemotePty {
    fn drop(&mut self) {
        (self.remote.close_end)(self.id, self.master as i32);
    }
}

impl RemotePty {
    fn m(&self) -> i32 {
        self.master as i32
    }
}

impl crate::vfs::FileHandle for RemotePty {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        match (self.remote.read)(self.id, self.m(), buf) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        match (self.remote.write)(self.id, self.m(), buf) {
            n if n >= 0 => Ok(n as u64),
            n => Err(Errno(-n)),
        }
    }

    /// The node `/dev/pts/N` (or `/dev/ptmx`) as `stat` on its name gives
    /// it: the same inode and device number, or `ttyname` finds no name.
    fn metadata(&self) -> Result<crate::vfs::Metadata, Errno> {
        let n = (self.remote.ptn)(self.id).max(0) as u32;
        let (ino, rdev, mode) = if self.master { (PTMX_INO, PTMX_RDEV, 0o666) } else { (pts_ino(n), pts_rdev(n), 0o620) };
        Ok(crate::vfs::Metadata {
            file_type: crate::vfs::FileType::CharDevice,
            size: 0,
            mode,
            ino,
            mtime: 0,
            nlink: 1,
            rdev,
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn is_tty(&self) -> bool {
        true
    }

    /// The slave in non-canonical mode with `VMIN` 0 and `VTIME` 0: btop's
    /// input loop reads until a read gives 0, and parked on the second read of
    /// every key instead, so it froze on the first key.
    fn empty_read_returns(&self) -> bool {
        if self.master {
            return false;
        }
        let mut t = [0u8; 60];
        (self.remote.get_termios)(self.id, &mut t);
        crate::tty::polling_read(&t)
    }

    /// Both ends of this pair live in the global kernel, and the other one is
    /// held by a program in another Worker -- a terminal emulator. Its write is
    /// what ends a wait here, and it wakes on its way out of the syscall.
    fn peer_wakes(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        (self.remote.readable)(self.id, self.m()) != 0
    }

    fn wait_queue(&self) -> u32 {
        (self.remote.queue)(self.id)
    }

    fn available(&self) -> Option<u64> {
        Some((self.remote.available)(self.id, self.m()).max(0) as u64)
    }

    fn pty_peer(&self) -> Option<i32> {
        if self.master { Some((self.remote.ptn)(self.id)) } else { None }
    }

    fn claim_session(&self, sid: i32) {
        // What `TIOCSCTTY` means for a pty that lives in the global kernel.
        //
        // The session is recorded there, where the hangup rule can ask whether
        // it is still alive (see `hangup_is_real` in hosts/kernel) -- and so is
        // the foreground process group, because claiming a controlling terminal
        // is what makes the claimer's group the foreground one. Only a session
        // leader may claim, and a session leader's group is its own pid, which
        // is the session id: so the group is the session.
        //
        // Without that second half every read from the terminal is a background
        // read: the kernel answers a shell's `read` with SIGTTIN, and a terminal
        // shows a shell that echoes what is typed (the line discipline does
        // that) and never runs any of it.
        (self.remote.set_sid)(self.id, sid);
        (self.remote.set_fg)(self.id, sid);
    }

    fn ioctl(&self, m: &mut crate::GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        // The descriptor's own ioctls are the kernel's to answer, as they are
        // for any file. Answering them here with ENOTTY failed libuv's
        // `uv__nonblock` (FIONBIO) on a pty: nvim's input stream never opened,
        // so it drew in an xterm and took no keys.
        if descriptor_level(request) {
            return None;
        }
        Some(self.do_ioctl(m, request, arg))
    }

    fn debug_state(&self) -> String {
        format!(
            "pty {} {}, {} to read",
            self.id,
            if self.master { "master" } else { "slave" },
            (self.remote.available)(self.id, self.m()).max(0),
        )
    }
}

/// FIONBIO, FIONCLEX, FIOCLEX, FIOASYNC: properties of the descriptor, not
/// the device.
fn descriptor_level(request: u64) -> bool {
    matches!(request, 0x5421 | 0x5450 | 0x5451 | 0x5452)
}

#[cfg(test)]
mod tests {
    /// The master sees the slave side going away as EIO, as on Linux: once
    /// its output is read, not before, and not before a slave was ever open.
    #[test]
    fn the_master_reads_eio_once_every_slave_has_closed() {
        let p = super::Pty::new(0);
        let mut buf = [0u8; 8];
        assert_eq!(p.master_read(&mut buf), Err(crate::errno::Errno::EAGAIN), "no slave yet: wait");
        p.open_slave();
        p.slave_write(b"bye");
        p.close_slave();
        assert_eq!(p.master_read(&mut buf), Ok(3), "what it wrote first");
        assert_eq!(p.master_read(&mut buf), Err(crate::errno::Errno::EIO));
        assert!(p.master_readable(), "a hangup is readable, so poll wakes the reader");
    }

    /// Keys typed while the terminal is cooked (a command running) and before
    /// the shell's line editor makes it raw again are read by the line
    /// editor, not dropped: `ls` typed after `clear` ran as `s`.
    #[test]
    fn a_part_typed_line_is_input_once_canonical_mode_is_left() {
        let p = super::Pty::new(0);
        p.open_slave();
        p.master_write(b"l");
        let mut buf = [0u8; 8];
        assert_eq!(p.slave_read(&mut buf), Err(crate::errno::Errno::EAGAIN), "cooked: no line yet");
        let mut t = p.tty.termios();
        let l = u32::from_le_bytes([t[12], t[13], t[14], t[15]]) & !(crate::tty::lflag::ICANON | crate::tty::lflag::ECHO);
        t[12..16].copy_from_slice(&l.to_le_bytes());
        p.set_termios(&t);
        p.master_write(b"s");
        assert_eq!(p.slave_read(&mut buf), Ok(2));
        assert_eq!(&buf[..2], b"ls");
    }

    #[test]
    fn a_pty_end_has_the_name_a_program_can_stat() {
        assert_eq!(super::link_name("pty:[4:s]"), "/dev/pts/4");
        assert_eq!(super::link_name("pty:[4:m]"), "/dev/ptmx");
        assert_eq!(super::link_name("pipe:[2]"), "pipe:[2]");
        assert_eq!(super::pts_rdev(4), 0x8804);
        assert_eq!(super::pts_rdev(300), (300 & 0xff) | (136 << 8) | ((300 & !0xff) << 12));
    }

    #[test]
    fn the_descriptor_ioctls_are_left_to_the_kernel() {
        for req in [0x5421, 0x5450, 0x5451, 0x5452] {
            assert!(super::descriptor_level(req), "{req:#x}");
        }
        // TCGETS, TIOCGWINSZ, TIOCGPTN are the terminal's.
        for req in [0x5401, 0x5413, 0x8004_5430] {
            assert!(!super::descriptor_level(req), "{req:#x}");
        }
    }
}

// ioctl numbers, the terminal subset a pty answers. The `_IOC`-encoded ones
// (pty and termios2) carry direction and struct size in the number.
mod ioc {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TCSETSW: u64 = 0x5403;
    pub const TCSETSF: u64 = 0x5404;
    pub const TCSBRK: u64 = 0x5409;
    pub const TCXONC: u64 = 0x540a;
    pub const TCFLSH: u64 = 0x540b;
    pub const TIOCEXCL: u64 = 0x540c;
    pub const TIOCNXCL: u64 = 0x540d;
    pub const TIOCSCTTY: u64 = 0x540e;
    pub const TIOCGPGRP: u64 = 0x540f;
    pub const TIOCSPGRP: u64 = 0x5410;
    pub const TIOCOUTQ: u64 = 0x5411;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const TIOCMGET: u64 = 0x5415;
    pub const TIOCMBIS: u64 = 0x5416;
    pub const TIOCMBIC: u64 = 0x5417;
    pub const TIOCMSET: u64 = 0x5418;
    pub const FIONREAD: u64 = 0x541b;
    pub const TIOCNOTTY: u64 = 0x5422;
    pub const TCSBRKP: u64 = 0x5425;
    pub const TIOCGSID: u64 = 0x5429;
    pub const TCGETS2: u64 = 0x802c_542a;
    pub const TCSETS2: u64 = 0x402c_542b;
    pub const TCSETSW2: u64 = 0x402c_542c;
    pub const TCSETSF2: u64 = 0x402c_542d;
    pub const TIOCGPTN: u64 = 0x8004_5430;
    pub const TIOCSPTLCK: u64 = 0x4004_5431;
    pub const TIOCSIG: u64 = 0x4004_5436;
    pub const TIOCGPTLCK: u64 = 0x8004_5439;
    pub const TIOCGPTPEER: u64 = 0x5441;
    pub const TIOCGLCKTRMIOS: u64 = 0x5456;
    pub const TIOCSLCKTRMIOS: u64 = 0x5457;
}

impl RemotePty {
    fn do_ioctl(&self, m: &mut crate::GuestMem<'_>, req: u64, arg: u64) -> SysResult {
        use crate::tty::KERNEL_TERMIOS;
        let id = self.id;
        let r = &self.remote;
        match req {
            ioc::TCGETS => {
                let mut buf = [0u8; 60];
                (r.get_termios)(id, &mut buf);
                m.write(arg, &buf[..KERNEL_TERMIOS])?;
                Ok(0)
            }
            ioc::TCGETS2 => {
                let mut buf = [0u8; 60];
                (r.get_termios)(id, &mut buf);
                let mut out = [0u8; KERNEL_TERMIOS + 8];
                out[..KERNEL_TERMIOS].copy_from_slice(&buf[..KERNEL_TERMIOS]);
                out[KERNEL_TERMIOS..KERNEL_TERMIOS + 4].copy_from_slice(&38400u32.to_le_bytes());
                out[KERNEL_TERMIOS + 4..].copy_from_slice(&38400u32.to_le_bytes());
                m.write(arg, &out)?;
                Ok(0)
            }
            ioc::TCSETS | ioc::TCSETSW | ioc::TCSETSF => {
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                let flush = (req == ioc::TCSETSF) as i32;
                (r.set_termios)(id, &src, flush);
                Ok(0)
            }
            ioc::TCSETS2 | ioc::TCSETSW2 | ioc::TCSETSF2 => {
                let src = m.read(arg, KERNEL_TERMIOS)?.to_vec();
                let flush = (req == ioc::TCSETSF2) as i32;
                (r.set_termios)(id, &src, flush);
                Ok(0)
            }
            ioc::TIOCGWINSZ => {
                let (row, col, x, y) = crate::tty::unpack_ws((r.winsize)(id));
                let mut ws = [0u8; 8];
                ws[0..2].copy_from_slice(&row.to_le_bytes());
                ws[2..4].copy_from_slice(&col.to_le_bytes());
                ws[4..6].copy_from_slice(&x.to_le_bytes());
                ws[6..8].copy_from_slice(&y.to_le_bytes());
                m.write(arg, &ws)?;
                Ok(0)
            }
            ioc::TIOCSWINSZ => {
                let ws = m.read(arg, 8)?.to_vec();
                let g = |n: usize| u16::from_le_bytes([ws[n], ws[n + 1]]);
                (r.set_winsize)(id, crate::tty::pack_ws((g(0), g(2), g(4), g(6))));
                Ok(0)
            }
            ioc::TIOCGPGRP => {
                m.write_u32(arg, (r.fg)(id) as u32)?;
                Ok(0)
            }
            ioc::TIOCSPGRP => {
                let pgrp = m.read_u32(arg)? as i32;
                if pgrp <= 0 {
                    return Err(Errno::EINVAL);
                }
                (r.set_fg)(id, pgrp);
                Ok(0)
            }
            ioc::TIOCGSID => {
                m.write_u32(arg, (r.sid)(id) as u32)?;
                Ok(0)
            }
            // Claiming the terminal: the caller's session takes it. `arg` is the
            // process's session id, which only the syscall layer knows, so it is
            // passed the pid-derived value through `set_sid` when the guest asks
            // -- but a plain `TIOCSCTTY` just adopts whatever the process brings.
            // Handled at the syscall layer for the session; here it is a no-op
            // success so the slave's own `TIOCSCTTY` after `setsid` works.
            ioc::TIOCSCTTY => Ok(0),
            ioc::TIOCNOTTY => Ok(0),
            ioc::TIOCGPTN => {
                if !self.master {
                    return Err(Errno::EINVAL);
                }
                m.write_u32(arg, (r.ptn)(id) as u32)?;
                Ok(0)
            }
            ioc::TIOCSPTLCK => {
                if !self.master {
                    return Err(Errno::EINVAL);
                }
                let v = m.read_u32(arg)? as i32;
                (r.set_lock)(id, (v != 0) as i32);
                Ok(0)
            }
            ioc::TIOCGPTLCK => {
                m.write_u32(arg, (r.get_lock)(id) as u32)?;
                Ok(0)
            }
            // The lock termios is answered all-zero and not enforced: a program
            // reading it back is asking whether it may change a setting, and
            // all-zero says yes.
            ioc::TIOCGLCKTRMIOS => {
                m.write(arg, &[0u8; KERNEL_TERMIOS])?;
                Ok(0)
            }
            ioc::TIOCSLCKTRMIOS => Ok(0),
            ioc::TCFLSH => {
                (r.flush)(id, arg as i32);
                Ok(0)
            }
            ioc::FIONREAD | ioc::TIOCOUTQ => {
                let n = if req == ioc::FIONREAD {
                    (r.available)(id, self.m()).max(0)
                } else {
                    0
                };
                m.write_u32(arg, n as u32)?;
                Ok(0)
            }
            // No-op successes: nothing to drain or break, no modem lines, line
            // discipline is fixed at N_TTY, exclusive mode is not enforced.
            ioc::TCSBRK | ioc::TCSBRKP | ioc::TCXONC | ioc::TIOCEXCL | ioc::TIOCNXCL
            | ioc::TIOCMBIS | ioc::TIOCMBIC | ioc::TIOCMSET | ioc::TIOCSIG => Ok(0),
            ioc::TIOCMGET => {
                m.write_u32(arg, 0)?;
                Ok(0)
            }
            // Opening the peer: the syscall layer installs the descriptor
            // (`pty_peer`), before this handle is asked; here only for the
            // slave, which has no peer to open.
            ioc::TIOCGPTPEER => Err(Errno::ENOTTY),
            // The descriptor-level ioctls (FIOCLEX, FIONBIO, ...) are the same
            // for any file; let the kernel's own handling take them.
            _ => Err(Errno::ENOTTY),
        }
    }
}
