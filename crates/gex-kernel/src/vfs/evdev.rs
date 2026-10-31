//! Game controllers as Linux evdev devices: `/dev/input/event0` to `event3`.
//!
//! The host keeps a table of the controllers it sees ([`PADS`] of them, a
//! browser's Gamepad API) and this device turns each into what the kernel's
//! joystick drivers give a program: `EV_KEY` and `EV_ABS` events, followed by
//! `SYN_REPORT`, and the `EVIOCG*` requests that describe the device. SDL and
//! Wine (through SDL) find controllers this way.
//!
//! A controller the browser maps to the standard layout looks like an Xbox
//! 360 pad as Linux's xpad driver presents one: its ids, its name, its
//! buttons and axes. Programs and SDL's mapping database know that device.
//! Any other controller is a plain joystick under its own name and ids:
//! buttons from `BTN_TRIGGER` (then `BTN_TRIGGER_HAPPY`), axes from `ABS_X`.
//!
//! A pad's node exists while the controller is connected, and only then.
//! The directory's mtime moves at each connect and disconnect, which is
//! what a program without udev watches (SDL rescans when it moves). An
//! inotify watch on the directory is refused, so such a program polls the
//! mtime rather than waiting for events that never come.
//!
//! The table's words, per pad (the host fills them, [`JoyHost::read`]):
//!
//! | word | |
//! |------|-|
//! | 0 | serial, even when the pad is not being written |
//! | 1 | connection number, bumped at each connect (0: never connected) |
//! | 2 | 0 disconnected, 1 standard layout, 2 another layout |
//! | 3 | vendor << 16 \| product |
//! | 4 | buttons \| axes << 8 |
//! | 5 | a bit per button pressed |
//! | 6..14 | a byte per button: how far it is pressed, 0 to 255 |
//! | 14..30 | the axes, -32767 to 32767 |
//! | 30..62 | the name, UTF-8, NUL-padded |
//! | 62, 63 | when it last changed, ms since the epoch, low and high |

use super::{FileHandle, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Controllers at once: `event0` to `event3`.
pub const PADS: usize = 4;
/// Words per pad in the host's table.
pub const PAD_WORDS: usize = 64;

const P_GEN: usize = 1;
const P_KIND: usize = 2;
const P_ID: usize = 3;
const P_COUNTS: usize = 4;
const P_PRESSED: usize = 5;
const P_VALUES: usize = 6;
const P_AXES: usize = 14;
const P_NAME: usize = 30;
const P_TIME: usize = 62;
const MAX_BUTTONS: usize = 32;
const MAX_AXES: usize = 16;

const KIND_STANDARD: u32 = 1;

// Event types and codes (linux/input-event-codes.h).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const BTN_TRIGGER: u16 = 0x120;
const BTN_TRIGGER_HAPPY: u16 = 0x2c0;
const BTN_A: u16 = 0x130;
const BTN_B: u16 = 0x131;
const BTN_X: u16 = 0x133;
const BTN_Y: u16 = 0x134;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;

const EV_MAX: u32 = 0x1f;
const KEY_MAX: u32 = 0x2ff;
const ABS_MAX: u32 = 0x3f;
const REL_MAX: u32 = 0x0f;
const MSC_MAX: u32 = 0x07;
const LED_MAX: u32 = 0x0f;
const SND_MAX: u32 = 0x07;
const FF_MAX: u32 = 0x7f;
const SW_MAX: u32 = 0x10;
const PROP_MAX: u32 = 0x1f;

/// `EV_VERSION`.
const VERSION: u32 = 0x01_0001;
const BUS_USB: u16 = 0x03;

/// What the host gives the device.
pub struct JoyHost {
    /// Pad `pad`'s words, read whole (not while the host writes them).
    pub read: fn(pad: u32, out: &mut [u32; PAD_WORDS]) -> bool,
    /// The wait queue the host notifies when any pad changes.
    pub queue: fn() -> u32,
    /// When a pad last connected or went (seconds since the epoch), the
    /// directory's mtime.
    pub changed: fn() -> u32,
    /// Whether the process is on the i386 ABI (an i386 or wasm32 program):
    /// 16-byte events and 4-byte longs, rather than 24 and 8.
    pub compat: fn() -> bool,
}

fn compat() -> bool {
    HOST.get().is_some_and(|h| (h.compat)()) || crate::compat32()
}

static HOST: OnceLock<JoyHost> = OnceLock::new();

/// Give the device its host. The first call wins.
pub fn set_joy_host(host: JoyHost) {
    let _ = HOST.set(host);
}

/// Whether there is a host, so `/dev/input` exists.
pub fn have_joy() -> bool {
    HOST.get().is_some()
}

/// Whether `/dev/input` should be refused an inotify watch (see the module
/// note).
pub fn is_input_dir(path: &str) -> bool {
    have_joy() && path == "input"
}

/// `/dev/input`'s mtime.
pub fn dir_mtime() -> i64 {
    HOST.get().map_or(0, |h| i64::from((h.changed)()))
}

/// A pad's words now, when it is connected.
fn snapshot(pad: usize) -> Option<Pad> {
    let h = HOST.get()?;
    let mut w = [0u32; PAD_WORDS];
    ((h.read)(pad as u32, &mut w) && w[P_KIND] != 0).then_some(Pad(w))
}

/// `inputN`'s node name and number, from a path under `/dev`.
pub fn node(path: &str) -> Option<usize> {
    let n: usize = path.strip_prefix("input/event")?.parse().ok()?;
    (n < PADS).then_some(n)
}

/// The names under `/dev/input` now: the connected pads'.
pub fn names() -> Vec<String> {
    (0..PADS).filter(|&p| snapshot(p).is_some()).map(|p| format!("event{p}")).collect()
}

/// A pad node's metadata, when the pad is connected.
pub fn metadata(pad: usize) -> Option<Metadata> {
    snapshot(pad)?;
    Some(Metadata {
        file_type: FileType::CharDevice,
        size: 0,
        mode: 0o666,
        ino: 0x4000 + pad as u64,
        mtime: dir_mtime(),
        nlink: 1,
        // makedev(13, 64 + N), as evdev's.
        rdev: (13 << 8) | (64 + pad as u64),
        times: Default::default(),
    })
}

/// Open a pad's node: ENODEV when nothing is connected there.
pub fn open(pad: usize) -> Result<std::sync::Arc<dyn FileHandle>, Errno> {
    let p = snapshot(pad).ok_or(Errno::ENODEV)?;
    Ok(std::sync::Arc::new(JoyHandle {
        pad,
        state: Mutex::new(Open { gen: p.0[P_GEN], last: p, queue: VecDeque::new(), gone: false }),
    }))
}

#[derive(Clone)]
struct Pad([u32; PAD_WORDS]);

/// Where an absolute axis's value comes from.
#[derive(Clone, Copy)]
enum AbsSrc {
    Axis(usize),
    /// A button's analog value, 0 to 255 (a trigger).
    Trigger(usize),
    /// Two buttons as -1, 0, 1 (a d-pad's direction: negative, positive).
    Hat(usize, usize),
}

struct Abs {
    code: u16,
    src: AbsSrc,
    min: i32,
    max: i32,
    fuzz: i32,
    flat: i32,
}

impl Pad {
    fn standard(&self) -> bool {
        self.0[P_KIND] == KIND_STANDARD
    }

    fn buttons(&self) -> usize {
        (self.0[P_COUNTS] & 0xff).min(MAX_BUTTONS as u32) as usize
    }

    fn axes(&self) -> usize {
        ((self.0[P_COUNTS] >> 8) & 0xff).min(MAX_AXES as u32) as usize
    }

    fn pressed(&self, b: usize) -> bool {
        b < MAX_BUTTONS && self.0[P_PRESSED] & (1 << b) != 0
    }

    fn value(&self, b: usize) -> i32 {
        if b >= MAX_BUTTONS {
            return 0;
        }
        ((self.0[P_VALUES + b / 4] >> ((b % 4) * 8)) & 0xff) as i32
    }

    fn axis(&self, a: usize) -> i32 {
        if a >= MAX_AXES {
            return 0;
        }
        (self.0[P_AXES + a] as i32).clamp(-32768, 32767)
    }

    fn time_ms(&self) -> u64 {
        u64::from(self.0[P_TIME]) | (u64::from(self.0[P_TIME + 1]) << 32)
    }

    /// `(bus, vendor, product, version)`: an Xbox 360 pad for the standard
    /// layout, as xpad has it.
    fn id(&self) -> [u16; 4] {
        if self.standard() {
            [BUS_USB, 0x045e, 0x028e, 0x0114]
        } else {
            [BUS_USB, (self.0[P_ID] >> 16) as u16, self.0[P_ID] as u16, 0x0100]
        }
    }

    fn name(&self) -> String {
        if self.standard() {
            return "Microsoft X-Box 360 pad".to_string();
        }
        let mut b = Vec::with_capacity(128);
        for w in &self.0[P_NAME..P_NAME + 32] {
            b.extend_from_slice(&w.to_le_bytes());
        }
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let name = String::from_utf8_lossy(&b[..end]).trim().to_string();
        if name.is_empty() { "Gamepad".to_string() } else { name }
    }

    /// Its keys: the evdev code and the button it reads.
    fn keys(&self) -> Vec<(u16, usize)> {
        if self.standard() {
            return vec![
                (BTN_A, 0),
                (BTN_B, 1),
                (BTN_X, 2),
                (BTN_Y, 3),
                (BTN_TL, 4),
                (BTN_TR, 5),
                (BTN_SELECT, 8),
                (BTN_START, 9),
                (BTN_THUMBL, 10),
                (BTN_THUMBR, 11),
                (BTN_MODE, 16),
            ];
        }
        (0..self.buttons())
            .map(|b| (if b < 16 { BTN_TRIGGER + b as u16 } else { BTN_TRIGGER_HAPPY + (b - 16) as u16 }, b))
            .collect()
    }

    /// Its absolute axes.
    fn abs(&self) -> Vec<Abs> {
        let stick = |code, a| Abs { code, src: AbsSrc::Axis(a), min: -32768, max: 32767, fuzz: 16, flat: 128 };
        if self.standard() {
            return vec![
                stick(ABS_X, 0),
                stick(ABS_Y, 1),
                Abs { code: ABS_Z, src: AbsSrc::Trigger(6), min: 0, max: 255, fuzz: 0, flat: 0 },
                stick(ABS_RX, 2),
                stick(ABS_RY, 3),
                Abs { code: ABS_RZ, src: AbsSrc::Trigger(7), min: 0, max: 255, fuzz: 0, flat: 0 },
                Abs { code: ABS_HAT0X, src: AbsSrc::Hat(14, 15), min: -1, max: 1, fuzz: 0, flat: 0 },
                Abs { code: ABS_HAT0Y, src: AbsSrc::Hat(12, 13), min: -1, max: 1, fuzz: 0, flat: 0 },
            ];
        }
        // ABS_X to ABS_BRAKE, then the hats' codes.
        const CODES: [u16; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x10, 0x11, 0x12, 0x13, 0x14];
        (0..self.axes()).map(|a| Abs { code: CODES[a], src: AbsSrc::Axis(a), min: -32768, max: 32767, fuzz: 0, flat: 0 }).collect()
    }

    fn abs_value(&self, src: AbsSrc) -> i32 {
        match src {
            AbsSrc::Axis(a) => self.axis(a),
            AbsSrc::Trigger(b) => self.value(b),
            AbsSrc::Hat(neg, pos) => i32::from(self.pressed(pos)) - i32::from(self.pressed(neg)),
        }
    }
}

/// One `struct input_event`.
#[derive(Clone, Copy)]
struct Event {
    ms: u64,
    kind: u16,
    code: u16,
    value: i32,
}

struct Open {
    /// The connection this open is of: another means the pad went.
    gen: u32,
    last: Pad,
    queue: VecDeque<Event>,
    gone: bool,
}

/// An open `/dev/input/eventN`.
struct JoyHandle {
    pad: usize,
    state: Mutex<Open>,
}

impl std::fmt::Debug for JoyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JoyHandle(event{})", self.pad)
    }
}

/// Events queued at most; a reader that falls further behind loses the
/// oldest whole reports (the kernel's buffer drops too, with SYN_DROPPED).
const QUEUE_MAX: usize = 1024;

impl JoyHandle {
    /// Turn what changed since the last look into events.
    fn refresh(&self, s: &mut Open) {
        if s.gone {
            return;
        }
        let Some(now) = snapshot(self.pad).filter(|p| p.0[P_GEN] == s.gen) else {
            s.gone = true;
            return;
        };
        if now.0[..P_TIME] == s.last.0[..P_TIME] {
            return;
        }
        let ms = now.time_ms();
        let before = s.queue.len();
        for (code, b) in now.keys() {
            let (was, is) = (s.last.pressed(b), now.pressed(b));
            if was != is {
                s.queue.push_back(Event { ms, kind: EV_KEY, code, value: i32::from(is) });
            }
        }
        for a in now.abs() {
            let (was, is) = (s.last.abs_value(a.src), now.abs_value(a.src));
            if was != is {
                s.queue.push_back(Event { ms, kind: EV_ABS, code: a.code, value: is });
            }
        }
        if s.queue.len() != before {
            s.queue.push_back(Event { ms, kind: EV_SYN, code: SYN_REPORT, value: 0 });
        }
        s.last = now;
        while s.queue.len() > QUEUE_MAX {
            // Drop through the oldest report's SYN_REPORT.
            while let Some(e) = s.queue.pop_front() {
                if e.kind == EV_SYN {
                    break;
                }
            }
        }
    }

    /// The pad as this open last saw it.
    fn pad(&self) -> Pad {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut s);
        s.last.clone()
    }
}

/// A bitmap of `codes`, as `EVIOCGBIT` and `EVIOCGKEY` return it: `max`
/// bits in longs, at most `len` bytes of it.
fn bitmap(codes: impl Iterator<Item = u16>, max: u32, len: usize) -> Vec<u8> {
    let long = if compat() { 4 } else { 8 };
    let bytes = (max as usize + 1).div_ceil(long * 8) * long;
    let mut b = vec![0u8; bytes];
    for c in codes {
        if u32::from(c) <= max {
            b[c as usize / 8] |= 1 << (c % 8);
        }
    }
    b.truncate(len);
    b
}

/// `b` to the caller's `arg`: its length, as the `EVIOCG*` that return
/// bytes answer.
fn put(m: &mut crate::GuestMem<'_>, arg: u64, b: &[u8]) -> SysResult {
    m.write(arg, b)?;
    Ok(b.len() as u64)
}

/// A string as `EVIOCGNAME` returns one: NUL-terminated, at most `len`
/// bytes, the count copied.
fn string(m: &mut crate::GuestMem<'_>, arg: u64, s: &str, len: usize) -> SysResult {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b.truncate(len);
    m.write(arg, &b)?;
    Ok(b.len() as u64)
}

impl FileHandle for JoyHandle {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        let size = if compat() { 16 } else { 24 };
        if buf.len() < size {
            return Err(Errno::EINVAL);
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut s);
        if s.queue.is_empty() {
            return Err(if s.gone { Errno::ENODEV } else { Errno::EAGAIN });
        }
        let mut n = 0;
        while n + size <= buf.len() {
            let Some(e) = s.queue.pop_front() else { break };
            let (sec, usec) = (e.ms / 1000, (e.ms % 1000) * 1000);
            let out = &mut buf[n..n + size];
            if size == 16 {
                out[0..4].copy_from_slice(&(sec as u32).to_le_bytes());
                out[4..8].copy_from_slice(&(usec as u32).to_le_bytes());
            } else {
                out[0..8].copy_from_slice(&sec.to_le_bytes());
                out[8..16].copy_from_slice(&usec.to_le_bytes());
            }
            let t = size - 8;
            out[t..t + 2].copy_from_slice(&e.kind.to_le_bytes());
            out[t + 2..t + 4].copy_from_slice(&e.code.to_le_bytes());
            out[t + 4..t + 8].copy_from_slice(&e.value.to_le_bytes());
            n += size;
        }
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        // Force feedback and LEDs: taken and dropped.
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(metadata(self.pad).unwrap_or(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 0x4000 + self.pad as u64,
            mtime: 0,
            nlink: 1,
            rdev: (13 << 8) | (64 + self.pad as u64),
            times: Default::default(),
        }))
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut s);
        !s.queue.is_empty() || s.gone
    }

    fn poll_hup(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut s);
        s.gone
    }

    fn wait_queue(&self) -> u32 {
        HOST.get().map_or(0, |h| (h.queue)())
    }

    /// The page's writes wake a reader through the shared wait words.
    fn peer_wakes(&self) -> bool {
        true
    }

    fn ioctl(&self, m: &mut crate::GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        let request = request as u32;
        if (request >> 8) & 0xff != u32::from(b'E') {
            return None;
        }
        let nr = request & 0xff;
        let len = ((request >> 16) & 0x3fff) as usize;
        let dir = request >> 30;
        let p = self.pad();
        let r = match (dir, nr) {
            // EVIOCGVERSION
            (2, 0x01) => m.write_u32(arg, VERSION).map(|_| 0),
            // EVIOCGID: struct input_id, four u16s.
            (2, 0x02) => {
                let mut b = [0u8; 8];
                for (i, v) in p.id().iter().enumerate() {
                    b[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
                m.write(arg, &b).map(|_| 0)
            }
            // EVIOCGNAME, EVIOCGPHYS
            (2, 0x06) => string(m, arg, &p.name(), len),
            (2, 0x07) => string(m, arg, &format!("gex/input{}", self.pad), len),
            // EVIOCGUNIQ: none, as for most pads.
            (2, 0x08) => Err(Errno::ENOENT),
            // EVIOCGPROP
            (2, 0x09) => put(m, arg, &bitmap(std::iter::empty(), PROP_MAX, len)),
            // EVIOCGKEY: the keys down now.
            (2, 0x18) => {
                let down: Vec<u16> = p.keys().into_iter().filter(|&(_, b)| p.pressed(b)).map(|(c, _)| c).collect();
                put(m, arg, &bitmap(down.into_iter(), KEY_MAX, len))
            }
            // EVIOCGLED, EVIOCGSND, EVIOCGSW: none on.
            (2, 0x19) => put(m, arg, &bitmap(std::iter::empty(), LED_MAX, len)),
            (2, 0x1a) => put(m, arg, &bitmap(std::iter::empty(), SND_MAX, len)),
            (2, 0x1b) => put(m, arg, &bitmap(std::iter::empty(), SW_MAX, len)),
            // EVIOCGBIT(type, len)
            (2, 0x20..=0x3f) => {
                let b = match (nr - 0x20) as u16 {
                    0 => bitmap([EV_SYN, EV_KEY, EV_ABS].into_iter(), EV_MAX, len),
                    EV_KEY => bitmap(p.keys().into_iter().map(|(c, _)| c), KEY_MAX, len),
                    EV_ABS => bitmap(p.abs().into_iter().map(|a| a.code), ABS_MAX, len),
                    0x02 => bitmap(std::iter::empty(), REL_MAX, len),
                    0x04 => bitmap(std::iter::empty(), MSC_MAX, len),
                    0x11 => bitmap(std::iter::empty(), LED_MAX, len),
                    0x12 => bitmap(std::iter::empty(), SND_MAX, len),
                    0x15 => bitmap(std::iter::empty(), FF_MAX, len),
                    0x05 => bitmap(std::iter::empty(), SW_MAX, len),
                    _ => return Some(Err(Errno::EINVAL)),
                };
                put(m, arg, &b)
            }
            // EVIOCGABS(abs): struct input_absinfo, six s32s.
            (2, 0x40..=0x7f) => {
                let code = (nr - 0x40) as u16;
                match p.abs().into_iter().find(|a| a.code == code) {
                    Some(a) => {
                        let v = [p.abs_value(a.src), a.min, a.max, a.fuzz, a.flat, 0];
                        let mut b = [0u8; 24];
                        for (i, x) in v.iter().enumerate() {
                            b[i * 4..i * 4 + 4].copy_from_slice(&x.to_le_bytes());
                        }
                        m.write(arg, &b).map(|_| 0)
                    }
                    None => Err(Errno::EINVAL),
                }
            }
            // EVIOCSABS: taken (calibration a program sets is not kept).
            (1, 0xc0..=0xff) => Ok(0),
            // EVIOCGEFFECTS: no force feedback.
            (2, 0x84) => m.write_u32(arg, 0).map(|_| 0),
            // EVIOCGRAB, EVIOCREVOKE, EVIOCSCLOCKID: accepted.
            (1, 0x90) | (1, 0x91) | (1, 0xa0) => Ok(0),
            _ => Err(Errno::EINVAL),
        };
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard() -> [u32; PAD_WORDS] {
        let mut w = [0u32; PAD_WORDS];
        w[P_GEN] = 1;
        w[P_KIND] = KIND_STANDARD;
        w[P_COUNTS] = 17 | (4 << 8);
        w
    }

    #[test]
    fn a_standard_pad_is_an_xbox_360_pad() {
        let p = Pad(standard());
        assert_eq!(p.id(), [3, 0x045e, 0x028e, 0x0114]);
        assert_eq!(p.name(), "Microsoft X-Box 360 pad");
        assert!(p.keys().iter().any(|&(c, b)| c == BTN_A && b == 0));
        let mut w = standard();
        w[P_PRESSED] = 1 << 13; // d-pad down
        w[P_VALUES + 1] = 200 << 16; // button 6, the left trigger
        w[P_AXES + 1] = (-32767i32) as u32;
        let p = Pad(w);
        let abs = p.abs();
        let v = |code| p.abs_value(abs.iter().find(|a| a.code == code).unwrap().src);
        assert_eq!(v(ABS_HAT0Y), 1);
        assert_eq!(v(ABS_Z), 200);
        assert_eq!(v(ABS_Y), -32767);
    }

    #[test]
    fn another_pad_keeps_its_name_and_counts() {
        let mut w = [0u32; PAD_WORDS];
        w[P_KIND] = 2;
        w[P_ID] = 0x054c_0ce6;
        w[P_COUNTS] = 20 | (6 << 8);
        w[P_NAME..P_NAME + 2].copy_from_slice(&[u32::from_le_bytes(*b"Dual"), u32::from_le_bytes(*b"Sens")]);
        let p = Pad(w);
        assert_eq!(p.name(), "DualSens");
        assert_eq!(p.id()[1..3], [0x054c, 0x0ce6]);
        assert_eq!(p.keys().len(), 20);
        assert_eq!(p.keys()[16].0, BTN_TRIGGER_HAPPY);
        assert_eq!(p.abs().len(), 6);
    }

    #[test]
    fn bitmaps_are_longs_and_truncated() {
        let b = bitmap([BTN_A].into_iter(), KEY_MAX, 1000);
        assert_eq!(b.len(), 96);
        assert_eq!(b[BTN_A as usize / 8], 1 << (BTN_A % 8));
        assert_eq!(bitmap([EV_KEY].into_iter(), EV_MAX, 1), vec![2]);
    }
}
