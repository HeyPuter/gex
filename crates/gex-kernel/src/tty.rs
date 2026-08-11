//! The terminal.
//!
//! Everything a `struct termios` describes and nothing a page can decide for
//! itself: the flags, the window size, the foreground process group, and the
//! line discipline that turns keystrokes into what a program reads.
//!
//! It lives here rather than in the page because of what it has to reach. A
//! `^C` is not an input event -- it is `SIGINT` to the *foreground process
//! group*, and only the process table knows which processes those are. A
//! `^D` on an empty line is end of input to whoever is reading, which is not
//! necessarily the process that set the flags. And `stty` in one process must
//! be visible to the next, because there is one terminal and its settings
//! outlive any of them: that is why the page's copy of `c_lflag`, sent along
//! with each write, could never be more than a guess about echo.
//!
//! So there is one of these for the whole system, in the memory every process
//! shares, and both ends talk to it: the page pushes keystrokes in with
//! [`Tty::input`] and displays what comes back, and the guest's `ioctl` reads
//! and writes the same flags the discipline is obeying.
//!
//! What is deliberately not modelled: parity and framing errors (`PARMRK`,
//! `INPCK`, `IGNPAR` -- there is no wire to get them wrong on), the delay bits
//! in `c_oflag`, modem control beyond answering that the line is up, and
//! `pty` pairs. `xterm.js` is the terminal, and it is always connected.

use std::sync::Mutex;

/// Indices into `c_cc`. The kernel's `NCCS` is 19 on this architecture, not
/// the 32 a C library declares for its own larger structure.
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSWTC: usize = 7;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VDISCARD: usize = 13;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
    /// How many the kernel's structure has.
    pub const NCCS: usize = 19;
}

/// `c_iflag`.
pub mod iflag {
    pub const IGNBRK: u32 = 0x0001;
    pub const BRKINT: u32 = 0x0002;
    pub const IGNPAR: u32 = 0x0004;
    pub const PARMRK: u32 = 0x0008;
    pub const INPCK: u32 = 0x0010;
    pub const ISTRIP: u32 = 0x0020;
    pub const INLCR: u32 = 0x0040;
    pub const IGNCR: u32 = 0x0080;
    pub const ICRNL: u32 = 0x0100;
    pub const IUCLC: u32 = 0x0200;
    pub const IXON: u32 = 0x0400;
    pub const IXANY: u32 = 0x0800;
    pub const IXOFF: u32 = 0x1000;
    pub const IMAXBEL: u32 = 0x2000;
    pub const IUTF8: u32 = 0x4000;
}

/// `c_oflag`.
pub mod oflag {
    pub const OPOST: u32 = 0x0001;
    pub const OLCUC: u32 = 0x0002;
    pub const ONLCR: u32 = 0x0004;
    pub const OCRNL: u32 = 0x0008;
    pub const ONOCR: u32 = 0x0010;
    pub const ONLRET: u32 = 0x0020;
}

/// `c_lflag`.
pub mod lflag {
    pub const ISIG: u32 = 0x0001;
    pub const ICANON: u32 = 0x0002;
    pub const ECHO: u32 = 0x0008;
    pub const ECHOE: u32 = 0x0010;
    pub const ECHOK: u32 = 0x0020;
    pub const ECHONL: u32 = 0x0040;
    pub const NOFLSH: u32 = 0x0080;
    pub const TOSTOP: u32 = 0x0100;
    pub const ECHOCTL: u32 = 0x0200;
    pub const ECHOPRT: u32 = 0x0400;
    pub const ECHOKE: u32 = 0x0800;
    pub const FLUSHO: u32 = 0x1000;
    pub const PENDIN: u32 = 0x4000;
    pub const IEXTEN: u32 = 0x8000;
}

/// How much of `struct termios` the kernel exchanges: four flag words,
/// `c_line`, and `c_cc[19]`.
pub const KERNEL_TERMIOS: usize = 4 * 4 + 1 + cc::NCCS;

/// Where `c_cc` starts inside the structure.
const CC_AT: usize = 17;

/// A terminal in the state a shell expects to find one: echo on, input a line
/// at a time, `^C` meaning something.
pub const COOKED: [u8; 60] = {
    let mut t = [0u8; 60];
    // ICRNL | IXON | BRKINT | IMAXBEL | IUTF8.
    let i = (iflag::BRKINT | iflag::ICRNL | iflag::IXON | iflag::IMAXBEL | iflag::IUTF8)
        .to_le_bytes();
    let o = (oflag::OPOST | oflag::ONLCR).to_le_bytes();
    let c = 0xbfu32.to_le_bytes(); // B38400 | CS8 | CREAD
    let l = (lflag::ISIG
        | lflag::ICANON
        | lflag::ECHO
        | lflag::ECHOE
        | lflag::ECHOK
        | lflag::ECHOCTL
        | lflag::ECHOKE
        | lflag::IEXTEN)
        .to_le_bytes();
    t[0] = i[0]; t[1] = i[1]; t[2] = i[2]; t[3] = i[3];
    t[4] = o[0]; t[5] = o[1]; t[6] = o[2]; t[7] = o[3];
    t[8] = c[0]; t[9] = c[1]; t[10] = c[2]; t[11] = c[3];
    t[12] = l[0]; t[13] = l[1]; t[14] = l[2]; t[15] = l[3];
    // `c_line` is 0 (N_TTY), and then `c_cc`. Leaving these zero is not
    // harmless: an interactive shell reads its erase, interrupt and
    // end-of-file characters from here, and a zero erase character means
    // backspace does nothing. These are the kernel's own `INIT_C_CC`.
    //
    //     VINTR ^C   VQUIT ^\   VERASE DEL  VKILL ^U   VEOF ^D
    //     VTIME 0    VMIN 1     VSWTC 0     VSTART ^Q  VSTOP ^S
    //     VSUSP ^Z   VEOL 0     VREPRINT ^R VDISCARD ^O
    //     VWERASE ^W VLNEXT ^V  VEOL2 0
    let init = [
        3u8, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0,
    ];
    let mut n = 0;
    while n < cc::NCCS {
        t[CC_AT + n] = init[n];
        n += 1;
    }
    t
};

/// Which queue a `TCFLSH` or `tcflush` names.
pub const TCIFLUSH: i32 = 0;
pub const TCOFLUSH: i32 = 1;
pub const TCIOFLUSH: i32 = 2;

/// What one push of keystrokes produced.
///
/// Returned rather than acted on, because two of the four things a keystroke
/// can do reach outside this file: a signal goes to the process table, and
/// what is echoed goes to whatever is drawing the terminal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// Bytes a reader may now take.
    pub to_reader: Vec<u8>,
    /// Bytes the terminal should display.
    pub echo: Vec<u8>,
    /// Signals for the foreground process group, in the order they were
    /// typed.
    pub signals: Vec<u32>,
    /// A reader should see end of input -- `^D` on an empty line.
    pub eof: bool,
    /// Throw away everything already queued for a reader before delivering
    /// `to_reader`. A signal character flushes the input queue unless
    /// `NOFLSH`.
    pub flush: bool,
}

struct State {
    termios: [u8; 60],
    /// `TIOCGLCKTRMIOS`: which bits a caller may not change. All-zero, which
    /// is "nothing is locked", and is what anybody who asks wants to hear.
    locked: [u8; 60],
    /// Rows, columns, and the pixel dimensions nobody fills in but `resize`
    /// asks for.
    winsize: (u16, u16, u16, u16),
    /// The foreground process group, or 0 before a shell has claimed one.
    fg: i32,
    /// The session that owns the terminal, or 0 for none.
    sid: i32,
    /// The line being edited, in canonical mode, with how many columns each
    /// byte took to echo -- so that erasing one control character rubs out
    /// the two columns `^C` occupies rather than one.
    line: Vec<(u8, u8)>,
    /// `VLNEXT` was typed: the next byte is data whatever it is.
    literal: bool,
    /// `^S` stopped output; `^Q` starts it again.
    stopped: bool,
    /// `TIOCGETD`/`TIOCSETD`. Only `N_TTY` is implemented, and setting
    /// another is refused rather than accepted and ignored.
    ldisc: i32,
    /// `TIOCEXCL`. Nothing enforces it -- there is one terminal and one page
    /// -- but a program that sets it expects to read it back.
    exclusive: bool,
    /// `TIOCSSOFTCAR`.
    softcar: bool,
}

/// The system's terminal.
pub struct Tty {
    st: Mutex<State>,
}

impl Default for Tty {
    fn default() -> Tty {
        Tty::new()
    }
}

fn word(t: &[u8], n: usize) -> u32 {
    u32::from_le_bytes([t[n * 4], t[n * 4 + 1], t[n * 4 + 2], t[n * 4 + 3]])
}

impl State {
    fn iflag(&self) -> u32 {
        word(&self.termios, 0)
    }
    fn oflag(&self) -> u32 {
        word(&self.termios, 1)
    }
    fn lflag(&self) -> u32 {
        word(&self.termios, 3)
    }
    fn cc(&self, n: usize) -> u8 {
        self.termios[CC_AT + n]
    }
    /// Whether `b` is the control character `c_cc[n]` names. A `c_cc` entry
    /// of zero is disabled, and must not match a NUL that was typed.
    fn is(&self, n: usize, b: u8) -> bool {
        let want = self.cc(n);
        want != 0 && want == b
    }

    /// How `b` appears on screen, and how many columns that takes.
    fn echo_of(&self, b: u8, out: &mut Vec<u8>) -> u8 {
        let l = self.lflag();
        if b == b'\n' || b == b'\r' {
            out.push(b);
            return 1;
        }
        if b == b'\t' {
            out.push(b'\t');
            return 1;
        }
        // A control character is shown as `^X` when `ECHOCTL` says so, and
        // otherwise sent through -- which for `^[` is how a program that
        // wants the escape sequence on screen gets it.
        if (b < 0x20 || b == 0x7f) && l & lflag::ECHOCTL != 0 {
            out.push(b'^');
            out.push(if b == 0x7f { b'?' } else { b + b'@' });
            return 2;
        }
        out.push(b);
        1
    }

    /// Rub out `n` columns.
    fn rub(out: &mut Vec<u8>, n: u8) {
        for _ in 0..n {
            out.extend_from_slice(b"\x08 \x08");
        }
    }
}

impl Tty {
    pub fn new() -> Tty {
        Tty {
            st: Mutex::new(State {
                termios: COOKED,
                locked: [0u8; 60],
                winsize: (24, 80, 0, 0),
                fg: 0,
                sid: 0,
                line: Vec::new(),
                literal: false,
                stopped: false,
                ldisc: 0,
                exclusive: false,
                softcar: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic in here would poison the terminal for the whole run and
        // there is nothing to recover to, so carry on with what is there.
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The whole structure, as `TCGETS` hands it over.
    pub fn termios(&self) -> [u8; 60] {
        self.lock().termios
    }

    /// `TCSETS`. Short input sets only what it covers, which is how a
    /// kernel-sized `struct termios` of 36 bytes leaves the rest alone.
    pub fn set_termios(&self, src: &[u8]) {
        let mut s = self.lock();
        let n = src.len().min(60);
        s.termios[..n].copy_from_slice(&src[..n]);
        // Leaving canonical mode abandons a part-typed line, as Linux does:
        // the bytes belong to a discipline that is no longer running.
        if s.lflag() & lflag::ICANON == 0 {
            s.line.clear();
        }
    }

    /// `TIOCGLCKTRMIOS` and its setter, which nothing enforces.
    pub fn locked_termios(&self) -> [u8; 60] {
        self.lock().locked
    }
    pub fn set_locked_termios(&self, src: &[u8]) {
        let mut s = self.lock();
        let n = src.len().min(60);
        s.locked[..n].copy_from_slice(&src[..n]);
    }

    /// `c_lflag`, which is what a page needs to know whether the guest has
    /// taken over echo and line editing.
    pub fn lflag(&self) -> u32 {
        self.lock().lflag()
    }

    pub fn winsize(&self) -> (u16, u16, u16, u16) {
        self.lock().winsize
    }

    /// Resize. Returns whether anything changed, because `SIGWINCH` is owed
    /// only when it did -- a page that reports its size on every animation
    /// frame would otherwise signal the foreground group sixty times a
    /// second, and `bash` redraws its prompt for each one.
    pub fn set_winsize(&self, ws: (u16, u16, u16, u16)) -> bool {
        let mut s = self.lock();
        if s.winsize == ws {
            return false;
        }
        s.winsize = ws;
        true
    }

    /// `TIOCGPGRP`.
    pub fn fg(&self) -> i32 {
        self.lock().fg
    }
    /// `TIOCSPGRP`.
    pub fn set_fg(&self, pgrp: i32) {
        self.lock().fg = pgrp;
    }

    /// `TIOCGSID`.
    pub fn sid(&self) -> i32 {
        self.lock().sid
    }
    /// `TIOCSCTTY`, and `TIOCNOTTY` with a session of zero.
    pub fn set_sid(&self, sid: i32) {
        let mut s = self.lock();
        s.sid = sid;
        // A new session leader takes the terminal and is its own foreground
        // group, which is what makes `setsid; bash -i` able to read.
        if sid != 0 {
            s.fg = sid;
        }
    }

    pub fn ldisc(&self) -> i32 {
        self.lock().ldisc
    }
    /// Only `N_TTY` exists here. Refusing the others is the honest answer:
    /// a program that sets `N_SLIP` and is told yes will then write frames
    /// into a terminal.
    pub fn set_ldisc(&self, n: i32) -> bool {
        if n != 0 {
            return false;
        }
        self.lock().ldisc = 0;
        true
    }

    pub fn exclusive(&self) -> bool {
        self.lock().exclusive
    }
    pub fn set_exclusive(&self, on: bool) {
        self.lock().exclusive = on;
    }

    pub fn softcar(&self) -> bool {
        self.lock().softcar
    }
    pub fn set_softcar(&self, on: bool) {
        self.lock().softcar = on;
    }

    /// Whether `^S` has stopped output.
    pub fn stopped(&self) -> bool {
        self.lock().stopped
    }
    /// `tcflow(TCOOFF)`, which is the same switch `^S` throws.
    pub fn set_stopped(&self, on: bool) {
        self.lock().stopped = on;
    }

    /// Abandon the part-typed line. `TCFLSH` with `TCIFLUSH` empties the
    /// input queue, which the caller does, and the line being edited, which
    /// is here.
    pub fn flush_line(&self) {
        self.lock().line.clear();
    }

    /// `c_oflag` output processing, on the way from a program to the screen.
    ///
    /// `None` when there is nothing to do, so that the overwhelmingly common
    /// case -- a program writing a block of text with `OPOST | ONLCR` and no
    /// bare carriage returns -- does not copy it.
    ///
    /// This is not cosmetic. A terminal is a device that moves a cursor, and
    /// `\n` moves it down and not back; `ONLCR` is why a Unix program can
    /// write `\n` and not stair-step down the screen. Every other emulator
    /// hard-codes the translation, which is fine until a program clears
    /// `ONLCR` -- `less` and `vi` do, precisely so that they can place the
    /// cursor themselves.
    pub fn output(&self, buf: &[u8]) -> Option<Vec<u8>> {
        let o = self.lock().oflag();
        output_with(o, buf)
    }
}

/// [`Tty::output`], against `c_oflag` on its own.
///
/// Separate because the caller that matters has the flags and not the
/// terminal: in the browser the flags are in the global kernel and every
/// `write` to a terminal comes through here, so building a `Tty` to ask would
/// be a mutex and two heap allocations per `write` -- for a `printf`.
pub fn output_with(o: u32, buf: &[u8]) -> Option<Vec<u8>> {
    {
        if o & oflag::OPOST == 0 {
            return None;
        }
        let nl = o & oflag::ONLCR != 0;
        let cr = o & oflag::OCRNL != 0;
        let uc = o & oflag::OLCUC != 0;
        let nocr = o & oflag::ONOCR != 0;
        if !nl && !cr && !uc && !nocr {
            return None;
        }
        let mut out = Vec::with_capacity(buf.len() + buf.len() / 8);
        // Column zero is where `ONOCR` suppresses a carriage return. Only
        // tracked coarsely -- what matters is whether the last thing written
        // ended a line.
        let mut at_col0 = true;
        for &b in buf {
            match b {
                b'\n' if nl => {
                    out.extend_from_slice(b"\r\n");
                    at_col0 = true;
                }
                b'\n' => {
                    out.push(b'\n');
                    at_col0 = false;
                }
                b'\r' if cr => {
                    out.push(b'\n');
                    at_col0 = true;
                }
                b'\r' if nocr && at_col0 => {}
                b'\r' => {
                    out.push(b'\r');
                    at_col0 = true;
                }
                b if uc && b.is_ascii_lowercase() => {
                    out.push(b.to_ascii_uppercase());
                    at_col0 = false;
                }
                b => {
                    out.push(b);
                    at_col0 = false;
                }
            }
        }
        Some(out)
    }
}

impl Tty {
    /// Keystrokes, through the line discipline.
    ///
    /// The whole of N_TTY that a terminal on a screen can mean: input
    /// mapping, flow control, signal characters, and -- in canonical mode --
    /// a line the person can edit before any program sees a byte of it.
    pub fn input(&self, bytes: &[u8]) -> Input {
        let mut out = Input::default();
        let mut s = self.lock();
        for &raw in bytes {
            let l = s.lflag();
            let i = s.iflag();
            let mut b = raw;
            if i & iflag::ISTRIP != 0 {
                b &= 0x7f;
            }
            if i & iflag::IUCLC != 0 {
                b = b.to_ascii_lowercase();
            }

            // `VLNEXT` took the last byte's meaning away from this one.
            if s.literal {
                s.literal = false;
                Self::take(&mut s, b, &mut out, true);
                continue;
            }

            // Newline mapping happens before anything looks at the byte, so
            // that `ICRNL` makes Return end a line the way `\n` does.
            match b {
                b'\r' if i & iflag::IGNCR != 0 => continue,
                b'\r' if i & iflag::ICRNL != 0 => b = b'\n',
                b'\n' if i & iflag::INLCR != 0 => b = b'\r',
                _ => {}
            }

            // Flow control, which consumes the character.
            if i & iflag::IXON != 0 {
                if s.is(cc::VSTOP, b) {
                    s.stopped = true;
                    continue;
                }
                if s.is(cc::VSTART, b) {
                    s.stopped = false;
                    continue;
                }
                // `IXANY`: any key restarts stopped output.
                if s.stopped && i & iflag::IXANY != 0 {
                    s.stopped = false;
                }
            }

            // Signal characters. The line goes with them: a `^C` at the end
            // of a half-typed command must not leave that command waiting for
            // the next Return.
            if l & lflag::ISIG != 0 {
                let sig = if s.is(cc::VINTR, b) {
                    Some(crate::signal::sig::SIGINT)
                } else if s.is(cc::VQUIT, b) {
                    Some(crate::signal::sig::SIGQUIT)
                } else if s.is(cc::VSUSP, b) {
                    Some(crate::signal::sig::SIGTSTP)
                } else {
                    None
                };
                if let Some(sig) = sig {
                    if l & lflag::ECHO != 0 {
                        s.echo_of(b, &mut out.echo);
                        // Onto a line of its own, as a terminal does: what
                        // follows is a shell's next prompt, not the rest of
                        // the line that was abandoned.
                        out.echo.extend_from_slice(b"\r\n");
                    }
                    if l & lflag::NOFLSH == 0 {
                        s.line.clear();
                        out.to_reader.clear();
                        out.flush = true;
                    }
                    s.stopped = false;
                    out.signals.push(sig);
                    continue;
                }
            }

            if l & lflag::ICANON == 0 {
                // Raw. The program is doing its own editing, so every byte
                // goes through untouched -- and is still echoed if the
                // program left `ECHO` on, which `read -n1` does.
                if l & lflag::ECHO != 0 {
                    s.echo_of(b, &mut out.echo);
                }
                out.to_reader.push(b);
                continue;
            }

            // Canonical mode: the editing keys.
            if s.is(cc::VERASE, b) {
                if let Some((_, w)) = s.line.pop() {
                    if l & lflag::ECHOE != 0 {
                        State::rub(&mut out.echo, w);
                    } else if l & lflag::ECHO != 0 {
                        s.echo_of(b, &mut out.echo);
                    }
                }
                continue;
            }
            if s.is(cc::VKILL, b) {
                let cols: u32 = s.line.iter().map(|&(_, w)| w as u32).sum();
                s.line.clear();
                if l & lflag::ECHOKE != 0 {
                    for _ in 0..cols {
                        out.echo.extend_from_slice(b"\x08 \x08");
                    }
                } else if l & lflag::ECHOK != 0 {
                    out.echo.extend_from_slice(b"\r\n");
                } else if l & lflag::ECHO != 0 {
                    s.echo_of(b, &mut out.echo);
                }
                continue;
            }
            if l & lflag::IEXTEN != 0 {
                if s.is(cc::VWERASE, b) {
                    // Back over trailing blanks, then over the word.
                    let mut cols = 0u32;
                    while matches!(s.line.last(), Some(&(c, _)) if c == b' ' || c == b'\t') {
                        cols += s.line.pop().map_or(0, |(_, w)| w as u32);
                    }
                    while matches!(s.line.last(), Some(&(c, _)) if c != b' ' && c != b'\t') {
                        cols += s.line.pop().map_or(0, |(_, w)| w as u32);
                    }
                    if l & lflag::ECHOE != 0 {
                        for _ in 0..cols {
                            out.echo.extend_from_slice(b"\x08 \x08");
                        }
                    }
                    continue;
                }
                if s.is(cc::VREPRINT, b) {
                    if l & lflag::ECHO != 0 {
                        s.echo_of(b, &mut out.echo);
                        out.echo.extend_from_slice(b"\r\n");
                        let line: Vec<u8> = s.line.iter().map(|&(c, _)| c).collect();
                        for c in line {
                            s.echo_of(c, &mut out.echo);
                        }
                    }
                    continue;
                }
                if s.is(cc::VLNEXT, b) {
                    s.literal = true;
                    if l & lflag::ECHO != 0 {
                        // A caret with nothing after it yet, and the cursor
                        // put back on it, is what Linux shows.
                        out.echo.extend_from_slice(b"^\x08");
                    }
                    continue;
                }
            }
            if s.is(cc::VEOF, b) {
                // End of input if the line is empty, and otherwise the same
                // as Return without the newline -- which is what makes
                // `printf x | cat` see an `x` with no trailing `\n`.
                if s.line.is_empty() {
                    out.eof = true;
                } else {
                    out.to_reader.extend(s.line.drain(..).map(|(c, _)| c));
                }
                continue;
            }
            if b == b'\n' || s.is(cc::VEOL, b) || s.is(cc::VEOL2, b) {
                if l & lflag::ECHO != 0 || (b == b'\n' && l & lflag::ECHONL != 0) {
                    out.echo.extend_from_slice(b"\r\n");
                }
                out.to_reader.extend(s.line.drain(..).map(|(c, _)| c));
                out.to_reader.push(b);
                continue;
            }
            Self::take(&mut s, b, &mut out, false);
        }
        out
    }

    /// An ordinary data byte: into the line in canonical mode, straight
    /// through otherwise, echoed either way if `ECHO` is on.
    fn take(s: &mut State, b: u8, out: &mut Input, literal: bool) {
        let l = s.lflag();
        let mut w = 1;
        if l & lflag::ECHO != 0 {
            w = s.echo_of(b, &mut out.echo);
        }
        if l & lflag::ICANON != 0 && !(literal && b == b'\n') {
            s.line.push((b, w));
        } else {
            out.to_reader.push(b);
        }
    }
}

/// The terminal when it is not in this process's memory.
///
/// The same arrangement as [`crate::proc::Remote`] and for the same reason:
/// in the browser every guest process is a Worker with its own memory, and
/// the one terminal they share lives in a module all of them instantiated
/// against. It is reached by calling, not by pointing, so everything here is
/// a scalar and a buffer is an address in the *caller's* memory that the
/// other side copies across.
#[derive(Clone, Copy)]
pub struct Remote {
    /// Write the 60-byte structure to `at`.
    pub get_termios: extern "C" fn(at: u32, len: u32) -> i32,
    pub set_termios: extern "C" fn(at: u32, len: u32) -> i32,
    pub get_locked: extern "C" fn(at: u32, len: u32) -> i32,
    pub set_locked: extern "C" fn(at: u32, len: u32) -> i32,
    /// `rows << 48 | cols << 32 | xpixel << 16 | ypixel`.
    pub winsize: extern "C" fn() -> u64,
    /// Returns 1 if it changed, so the caller knows to raise `SIGWINCH`.
    pub set_winsize: extern "C" fn(packed: u64) -> i32,
    pub fg: extern "C" fn() -> i32,
    pub set_fg: extern "C" fn(pgrp: i32),
    pub sid: extern "C" fn() -> i32,
    pub set_sid: extern "C" fn(sid: i32),
    pub ldisc: extern "C" fn() -> i32,
    pub set_ldisc: extern "C" fn(n: i32) -> i32,
    /// Packed bits, because three booleans are not worth three imports:
    /// 1 exclusive, 2 soft carrier, 4 output stopped by `^S`.
    pub bits: extern "C" fn() -> i32,
    pub set_bits: extern "C" fn(which: i32, on: i32),
    /// Empty the queues `which` names -- `TCIFLUSH` and the rest.
    pub flush: extern "C" fn(which: i32) -> i32,
    /// Bytes waiting to be read, for `FIONREAD`.
    pub queued: extern "C" fn() -> i32,
    /// Push bytes into the input queue as though typed, for `TIOCSTI`.
    pub push: extern "C" fn(at: u32, len: u32) -> i32,
}

/// The terminal, wherever it is.
///
/// A run with no global kernel keeps one in its own memory and behaves as it
/// always did; a browser run points every process at the shared one.
pub struct TtyRef {
    local: Tty,
    remote: Option<Remote>,
}

impl Default for TtyRef {
    fn default() -> TtyRef {
        TtyRef::new()
    }
}

/// Bits for [`Remote::bits`].
pub const BIT_EXCL: i32 = 1;
pub const BIT_SOFTCAR: i32 = 2;
pub const BIT_STOPPED: i32 = 4;

impl TtyRef {
    pub fn new() -> TtyRef {
        TtyRef { local: Tty::new(), remote: None }
    }

    pub fn shared(remote: Remote) -> TtyRef {
        TtyRef { local: Tty::new(), remote: Some(remote) }
    }

    /// The local terminal, for a host that drives the discipline itself.
    /// `None` once there is a shared one, which drives its own.
    pub fn local(&self) -> Option<&Tty> {
        self.remote.is_none().then_some(&self.local)
    }

    pub fn termios(&self) -> [u8; 60] {
        match self.remote {
            Some(r) => {
                let mut t = [0u8; 60];
                (r.get_termios)(t.as_mut_ptr() as u32, 60);
                t
            }
            None => self.local.termios(),
        }
    }

    pub fn set_termios(&self, src: &[u8]) {
        match self.remote {
            Some(r) => {
                (r.set_termios)(src.as_ptr() as u32, src.len() as u32);
            }
            None => self.local.set_termios(src),
        }
    }

    pub fn locked_termios(&self) -> [u8; 60] {
        match self.remote {
            Some(r) => {
                let mut t = [0u8; 60];
                (r.get_locked)(t.as_mut_ptr() as u32, 60);
                t
            }
            None => self.local.locked_termios(),
        }
    }

    pub fn set_locked_termios(&self, src: &[u8]) {
        match self.remote {
            Some(r) => {
                (r.set_locked)(src.as_ptr() as u32, src.len() as u32);
            }
            None => self.local.set_locked_termios(src),
        }
    }

    pub fn winsize(&self) -> (u16, u16, u16, u16) {
        match self.remote {
            Some(r) => unpack_ws((r.winsize)()),
            None => self.local.winsize(),
        }
    }

    pub fn set_winsize(&self, ws: (u16, u16, u16, u16)) -> bool {
        match self.remote {
            Some(r) => (r.set_winsize)(pack_ws(ws)) != 0,
            None => self.local.set_winsize(ws),
        }
    }

    pub fn fg(&self) -> i32 {
        match self.remote {
            Some(r) => (r.fg)(),
            None => self.local.fg(),
        }
    }
    pub fn set_fg(&self, pgrp: i32) {
        match self.remote {
            Some(r) => (r.set_fg)(pgrp),
            None => self.local.set_fg(pgrp),
        }
    }
    pub fn sid(&self) -> i32 {
        match self.remote {
            Some(r) => (r.sid)(),
            None => self.local.sid(),
        }
    }
    pub fn set_sid(&self, sid: i32) {
        match self.remote {
            Some(r) => (r.set_sid)(sid),
            None => self.local.set_sid(sid),
        }
    }
    pub fn ldisc(&self) -> i32 {
        match self.remote {
            Some(r) => (r.ldisc)(),
            None => self.local.ldisc(),
        }
    }
    pub fn set_ldisc(&self, n: i32) -> bool {
        match self.remote {
            Some(r) => (r.set_ldisc)(n) != 0,
            None => self.local.set_ldisc(n),
        }
    }
    fn bit(&self, which: i32) -> bool {
        match self.remote {
            Some(r) => (r.bits)() & which != 0,
            None => match which {
                BIT_EXCL => self.local.exclusive(),
                BIT_SOFTCAR => self.local.softcar(),
                _ => self.local.stopped(),
            },
        }
    }
    fn set_bit(&self, which: i32, on: bool) {
        match self.remote {
            Some(r) => (r.set_bits)(which, on as i32),
            None => match which {
                BIT_EXCL => self.local.set_exclusive(on),
                BIT_SOFTCAR => self.local.set_softcar(on),
                _ => self.local.set_stopped(on),
            },
        }
    }
    pub fn exclusive(&self) -> bool {
        self.bit(BIT_EXCL)
    }
    pub fn set_exclusive(&self, on: bool) {
        self.set_bit(BIT_EXCL, on)
    }
    pub fn softcar(&self) -> bool {
        self.bit(BIT_SOFTCAR)
    }
    pub fn set_softcar(&self, on: bool) {
        self.set_bit(BIT_SOFTCAR, on)
    }
    pub fn stopped(&self) -> bool {
        self.bit(BIT_STOPPED)
    }
    pub fn set_stopped(&self, on: bool) {
        self.set_bit(BIT_STOPPED, on)
    }

    /// `TCFLSH`. Returns how many bytes were thrown away.
    pub fn flush(&self, which: i32) -> i32 {
        match self.remote {
            Some(r) => (r.flush)(which),
            None => {
                if which != TCOFLUSH {
                    self.local.flush_line();
                }
                0
            }
        }
    }

    /// Bytes a reader could take right now, for `FIONREAD`. `None` when
    /// there is no shared terminal to ask, and the caller should fall back to
    /// the descriptor's own answer.
    pub fn queued(&self) -> Option<i32> {
        self.remote.map(|r| (r.queued)())
    }

    /// `TIOCSTI`: bytes into the input queue as though they had been typed.
    pub fn push(&self, bytes: &[u8]) -> i32 {
        match self.remote {
            Some(r) => (r.push)(bytes.as_ptr() as u32, bytes.len() as u32),
            None => 0,
        }
    }

    /// `c_oflag` processing on the way out. `None` leaves the bytes alone.
    ///
    /// The bytes stay here: only the flag word crosses, and the transform
    /// runs against the caller's own buffer.
    pub fn output(&self, buf: &[u8]) -> Option<Vec<u8>> {
        match self.remote {
            Some(_) => output_with(self.oflag(), buf),
            None => self.local.output(buf),
        }
    }

    /// `c_oflag`, which is all a write needs.
    pub fn oflag(&self) -> u32 {
        let t = self.termios();
        u32::from_le_bytes([t[4], t[5], t[6], t[7]])
    }
}

pub fn pack_ws(ws: (u16, u16, u16, u16)) -> u64 {
    (ws.0 as u64) << 48 | (ws.1 as u64) << 32 | (ws.2 as u64) << 16 | ws.3 as u64
}

pub fn unpack_ws(v: u64) -> (u16, u16, u16, u16) {
    ((v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(t: &Tty, s: &str) -> Input {
        t.input(s.as_bytes())
    }

    #[test]
    fn a_cooked_line_arrives_whole_on_return() {
        let t = Tty::new();
        let a = typed(&t, "ec");
        assert!(a.to_reader.is_empty(), "nothing before Return");
        assert_eq!(a.echo, b"ec");
        let b = typed(&t, "ho\r");
        assert_eq!(b.to_reader, b"echo\n");
        assert_eq!(b.echo, b"ho\r\n");
    }

    #[test]
    fn backspace_rubs_out_one_column_and_a_caret_two() {
        let t = Tty::new();
        typed(&t, "ab");
        let e = typed(&t, "\x7f");
        assert_eq!(e.echo, b"\x08 \x08");
        // `^A` echoes as two columns, so erasing it must rub out two.
        typed(&t, "\x01");
        let e = typed(&t, "\x7f");
        assert_eq!(e.echo, b"\x08 \x08\x08 \x08");
        let done = typed(&t, "\r");
        assert_eq!(done.to_reader, b"a\n");
    }

    #[test]
    fn ctrl_c_signals_and_throws_the_line_away() {
        let t = Tty::new();
        typed(&t, "rm -rf /");
        let c = typed(&t, "\x03");
        assert_eq!(c.signals, vec![crate::signal::sig::SIGINT]);
        assert!(c.flush);
        assert_eq!(c.echo, b"^C\r\n");
        // And the abandoned line is not waiting to be delivered later.
        assert_eq!(typed(&t, "\r").to_reader, b"\n");
    }

    #[test]
    fn noflsh_keeps_the_line() {
        let t = Tty::new();
        let mut tm = COOKED;
        tm[12] |= lflag::NOFLSH as u8;
        t.set_termios(&tm);
        typed(&t, "keep");
        let c = typed(&t, "\x03");
        assert_eq!(c.signals, vec![crate::signal::sig::SIGINT]);
        assert!(!c.flush);
        assert_eq!(typed(&t, "\r").to_reader, b"keep\n");
    }

    #[test]
    fn raw_mode_delivers_every_key_at_once() {
        let t = Tty::new();
        let mut tm = COOKED;
        // Clear ICANON and ECHO, which is what a shell's line editor does.
        let l = u32::from_le_bytes([tm[12], tm[13], tm[14], tm[15]]) & !(lflag::ICANON | lflag::ECHO);
        tm[12..16].copy_from_slice(&l.to_le_bytes());
        t.set_termios(&tm);
        let a = typed(&t, "hi");
        assert_eq!(a.to_reader, b"hi");
        assert!(a.echo.is_empty(), "the program echoes, not the terminal");
        // `^C` still signals: ISIG is independent of ICANON, which is why
        // `bash`'s own line editor still gets interrupted.
        assert_eq!(typed(&t, "\x03").signals, vec![crate::signal::sig::SIGINT]);
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_end_of_input() {
        let t = Tty::new();
        assert!(typed(&t, "\x04").eof);
        // And on a part-typed line it delivers what is there, unterminated.
        typed(&t, "x");
        let d = typed(&t, "\x04");
        assert!(!d.eof);
        assert_eq!(d.to_reader, b"x");
    }

    #[test]
    fn word_erase_takes_a_word_and_the_blanks_after_it() {
        let t = Tty::new();
        typed(&t, "ls -l /usr  ");
        typed(&t, "\x17");
        assert_eq!(typed(&t, "\r").to_reader, b"ls -l \n");
    }

    #[test]
    fn kill_erases_the_whole_line() {
        let t = Tty::new();
        typed(&t, "nonsense");
        typed(&t, "\x15");
        assert_eq!(typed(&t, "\r").to_reader, b"\n");
    }

    #[test]
    fn lnext_makes_the_next_key_data() {
        let t = Tty::new();
        typed(&t, "\x16\x03");
        let done = typed(&t, "\r");
        assert_eq!(done.to_reader, b"\x03\n");
    }

    #[test]
    fn flow_control_swallows_the_keys() {
        let t = Tty::new();
        let s = typed(&t, "\x13");
        assert!(s.to_reader.is_empty() && s.echo.is_empty());
        assert!(t.stopped());
        typed(&t, "\x11");
        assert!(!t.stopped());
    }

    #[test]
    fn onlcr_is_what_stops_the_staircase() {
        let t = Tty::new();
        assert_eq!(t.output(b"a\nb\n").unwrap(), b"a\r\nb\r\n");
        // A program that clears OPOST -- `less`, `vi` -- gets its bytes
        // through untouched.
        let mut tm = COOKED;
        tm[4] = 0;
        t.set_termios(&tm);
        assert!(t.output(b"a\nb\n").is_none());
    }

    #[test]
    fn a_resize_to_the_same_size_is_not_a_signal() {
        let t = Tty::new();
        assert!(t.set_winsize((40, 100, 0, 0)));
        assert!(!t.set_winsize((40, 100, 0, 0)));
        assert_eq!(t.winsize(), (40, 100, 0, 0));
    }

    #[test]
    fn leaving_canonical_mode_abandons_the_line() {
        let t = Tty::new();
        typed(&t, "half");
        let mut tm = COOKED;
        let l = u32::from_le_bytes([tm[12], tm[13], tm[14], tm[15]]) & !lflag::ICANON;
        tm[12..16].copy_from_slice(&l.to_le_bytes());
        t.set_termios(&tm);
        assert_eq!(typed(&t, "x").to_reader, b"x");
    }
}
