//! Hosting the Wayland compositor for a guest (native debug host).
//!
//! [`gex_wayland`] is the protocol; this joins it to a guest -- binds a name in
//! the unix-socket namespace, pumps bytes between each client and its
//! compositor state, and answers where a client's pixels are. A client maps its
//! buffer `MAP_SHARED`, which in gex aliases a range of guest memory, so the
//! compositor reads the framebuffer directly with no copy. Pumped at the end of
//! every syscall, so a client's wait always has a serviced reply.

use gex_kernel::file::OpenFile;
use gex_kernel::syscalls::Kernel;
use gex_kernel::unix::{HostListener, MemFd, UnixSocket};
use gex_kernel::vfs::FileHandle;
use gex_kernel::GuestMem;
use gex_wayland::{Buffers, Client, Frame};
use std::sync::{Arc, Mutex};

/// One connected client: its socket, its protocol state, and the descriptors
/// it has sent.
struct Session {
    socket: Arc<UnixSocket>,
    client: Client,
    /// Descriptors received over `SCM_RIGHTS`, in arrival order: the protocol
    /// refers to one by arrival index, not the client's descriptor number.
    fds: Vec<Arc<OpenFile>>,
    /// Event bytes the client's socket had no room for yet, and how many have
    /// gone. Usually empty (events are small), but a dropped tail does not fail
    /// -- it desyncs the stream and leaves the client waiting forever.
    out: Vec<u8>,
    sent: usize,
}

/// The compositor, as the guest sees it: a name that accepts connections.
#[derive(Clone)]
pub struct Wayland {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    sessions: Mutex<Vec<Session>>,
    /// Frames committed since the host last collected them.
    frames: Mutex<Vec<Frame>>,
    size: Mutex<(i32, i32)>,
    /// Where to write each frame, and how many have been written.
    out: Mutex<Option<String>>,
    written: Mutex<u32>,
    /// Presents counted for `GEX_FPS` (count since the last report, and when).
    fps: Mutex<Option<(u32, std::time::Instant)>>,
    /// The GPU compositor, when one could be started. Optional: a machine with
    /// no graphics adapter still runs, compositing on the host instead.
    gpu: Mutex<Option<gex_gpu::Compositor>>,
    /// `GEX_WL_INPUT`: scripted input, timed from the first frame.
    input: Mutex<Option<Input>>,
}

/// One scripted input event (`GEX_WL_INPUT`, [`Input::load`]).
#[derive(Debug, Clone, Copy)]
enum Ev {
    Motion(i32, i32),
    Button(u32, bool),
    Key(u32, bool),
}

/// A script of input events at times (seconds) after the compositor's Nth
/// frame (`frames N`, default 1: a game's first frame is often a splash long
/// before its menu; 0: from the compositor's start), delivered to the clients by [`Wayland::pump`] as they fall due:
/// what an automated game test needs to get past menus. One event per line,
/// `#` comments:
///
/// ```text
/// frames 300                 # time from the 300th frame
/// 30    click 100 328        # motion, then BTN_LEFT down and (0.1 s later) up
/// 31.5  move 400 300
/// 32    button 272 1         # a Linux BTN_* code, 1 down / 0 up
/// 40    tap enter            # key down, and up 0.1 s later
/// 41    key z 1              # a key name or Linux KEY_* code, 1 down / 0 up
/// ```
///
/// Coordinates are the focused surface's pixels.
struct Input {
    events: Vec<(f64, Ev, String)>,
    next: usize,
    start: Option<std::time::Instant>,
    /// Frames to see before the clock starts, and how many have been.
    after: u64,
    seen: u64,
}

/// Linux `KEY_*` codes for the names a script may use (else a number).
fn key_code(name: &str) -> Option<u32> {
    let k = match name.to_ascii_lowercase().as_str() {
        "esc" | "escape" => 1,
        "1" => 2, "2" => 3, "3" => 4, "4" => 5, "5" => 6, "6" => 7, "7" => 8, "8" => 9, "9" => 10, "0" => 11,
        "backspace" => 14, "tab" => 15,
        "q" => 16, "w" => 17, "e" => 18, "r" => 19, "t" => 20, "y" => 21, "u" => 22, "i" => 23, "o" => 24, "p" => 25,
        "enter" | "return" => 28, "ctrl" | "leftctrl" => 29,
        "a" => 30, "s" => 31, "d" => 32, "f" => 33, "g" => 34, "h" => 35, "j" => 36, "k" => 37, "l" => 38,
        "shift" | "leftshift" => 42,
        "z" => 44, "x" => 45, "c" => 46, "v" => 47, "b" => 48, "n" => 49, "m" => 50,
        "alt" | "leftalt" => 56, "space" => 57,
        "f1" => 59, "f2" => 60, "f3" => 61, "f4" => 62, "f5" => 63,
        "up" => 103, "left" => 105, "right" => 106, "down" => 108,
        n => return n.parse().ok(),
    };
    Some(k)
}

impl Input {
    fn load(path: &str) -> Result<Input, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut events = Vec::new();
        let mut after = 1;
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let w: Vec<&str> = line.split_whitespace().collect();
            let bad = || format!("{path}:{}: cannot read `{line}`", n + 1);
            if w[0] == "frames" {
                after = w.get(1).and_then(|v| v.parse().ok()).ok_or_else(bad)?;
                continue;
            }
            let t: f64 = w[0].parse().map_err(|_| bad())?;
            let num = |i: usize| -> Result<i32, String> { w.get(i).and_then(|v| v.parse().ok()).ok_or_else(bad) };
            let key = |i: usize| -> Result<u32, String> { w.get(i).and_then(|v| key_code(v)).ok_or_else(bad) };
            let down = |i: usize| -> Result<bool, String> { Ok(num(i)? != 0) };
            let l = line.to_string();
            match w.get(1).copied() {
                Some("move") => events.push((t, Ev::Motion(num(2)?, num(3)?), l)),
                Some("click") => {
                    events.push((t, Ev::Motion(num(2)?, num(3)?), l.clone()));
                    events.push((t, Ev::Button(0x110, true), l.clone()));
                    events.push((t + 0.1, Ev::Button(0x110, false), l));
                }
                Some("button") => events.push((t, Ev::Button(num(2)? as u32, down(3)?), l)),
                Some("key") => events.push((t, Ev::Key(key(2)?, down(3)?), l)),
                Some("tap") => {
                    events.push((t, Ev::Key(key(2)?, true), l.clone()));
                    events.push((t + 0.1, Ev::Key(key(2)?, false), l));
                }
                _ => return Err(bad()),
            }
        }
        events.sort_by(|a, b| a.0.total_cmp(&b.0));
        // `frames 0`: from the compositor's start.
        let start = if after == 0 { Some(std::time::Instant::now()) } else { None };
        Ok(Input { events, next: 0, start, after, seen: 0 })
    }
}

impl std::fmt::Debug for Wayland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wayland")
    }
}

impl Wayland {
    pub fn new(width: i32, height: i32) -> Wayland {
        let input = match std::env::var("GEX_WL_INPUT") {
            Ok(p) => match Input::load(&p) {
                Ok(i) => Some(i),
                Err(e) => {
                    eprintln!("[gex] GEX_WL_INPUT: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        Wayland {
            inner: Arc::new(Inner {
                size: Mutex::new((width, height)),
                input: Mutex::new(input),
                ..Default::default()
            }),
        }
    }

    /// A frame was shown (a client's commit, or GL's own present): the
    /// input script's clock starts at its `frames N`th.
    fn note_frame(&self) {
        if let Some(inp) = self.inner.input.lock().unwrap().as_mut() {
            inp.seen += 1;
            if inp.seen >= inp.after && inp.start.is_none() {
                eprintln!("[gex] input: the script's clock starts at frame {}", inp.seen);
                inp.start = Some(std::time::Instant::now());
            }
        }
    }

    /// The scripted events now due (`GEX_WL_INPUT`), with their time in ms.
    fn due_input(&self) -> Vec<(Ev, u32)> {
        let mut g = self.inner.input.lock().unwrap();
        let Some(inp) = g.as_mut() else { return Vec::new() };
        let Some(start) = inp.start else { return Vec::new() };
        let now = start.elapsed().as_secs_f64();
        let mut due = Vec::new();
        while inp.next < inp.events.len() && inp.events[inp.next].0 <= now {
            let (t, ev, line) = &inp.events[inp.next];
            eprintln!("[gex] input at {now:.1} s: {ev:?} ({line})");
            due.push((*ev, (t * 1000.0) as u32));
            inp.next += 1;
        }
        due
    }

    /// Composite on the GPU rather than handing single frames to the host.
    /// Returns the adapter, or the reason there is none; either way the
    /// compositor keeps working.
    pub fn enable_gpu(&self, width: i32, height: i32) -> Result<String, String> {
        match gex_gpu::Compositor::new(width.max(1) as u32, height.max(1) as u32) {
            Ok(c) => {
                let name = c.adapter.clone();
                *self.inner.gpu.lock().unwrap() = Some(c);
                Ok(name)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Write every committed frame (every Nth with `GEX_PPM_EVERY=N`) as a
    /// PPM at `prefix-NNN.ppm` -- a file is what a test can compare and what
    /// works with no display.
    pub fn write_frames_to(&self, prefix: &str) {
        *self.inner.out.lock().unwrap() = Some(prefix.to_string());
    }

    /// `GEX_PPM_EVERY=N` (crate::ppm_every): count a frame that is not
    /// written, so the ones that are keep their real index.
    fn skip_frame(&self) -> bool {
        let mut n = self.inner.written.lock().unwrap();
        if *n % crate::ppm_every() == 0 {
            return false;
        }
        *n += 1;
        true
    }

    /// Write out what was drawn: with a GPU, the whole composited screen;
    /// without one, a single client buffer (nothing here would combine them).
    fn save(&self, frames: &[Frame]) {
        let Some(prefix) = self.inner.out.lock().unwrap().clone() else {
            return;
        };
        // Before the GPU read-back, so a skipped frame costs nothing.
        let Some(path) = self.frame_path(&prefix) else { return };
        let mut gpu = self.inner.gpu.lock().unwrap();
        let (width, height, rgb) = if let Some(c) = gpu.as_mut() {
            match c.draw(frames).and_then(|()| c.read_back()) {
                Ok(rgba) => {
                    let (w, h) = c.size();
                    (w as i32, h as i32, gex_gpu::rgba_to_rgb(&rgba))
                }
                Err(e) => {
                    eprintln!("gex: the GPU compositor failed: {e}");
                    return;
                }
            }
        } else {
            let Some(frame) = frames.last() else { return };
            (frame.width, frame.height, gex_wayland::to_rgb(frame))
        };
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend_from_slice(&rgb);
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("gex: could not write {path}: {e}");
        }
    }

    /// Count a presented frame and say where to write it, if anywhere:
    /// `GEX_PPM_EVERY=N` keeps every Nth ([`Self::skip_frame`]), and the
    /// prefix `null` keeps none. `GEX_FPS=1` prints presents per second to
    /// stderr, which is what a measurement script reads.
    fn frame_path(&self, prefix: &str) -> Option<String> {
        if std::env::var_os("GEX_FPS").is_some_and(|v| v != "0") {
            let mut fps = self.inner.fps.lock().unwrap();
            let (count, since) = fps.get_or_insert((0, std::time::Instant::now()));
            *count += 1;
            let dt = since.elapsed().as_secs_f64();
            if dt >= 1.0 {
                eprintln!("[gex] fps {:.1}", *count as f64 / dt);
                *count = 0;
                *since = std::time::Instant::now();
            }
        }
        if prefix == "null" || self.skip_frame() {
            return None;
        }
        let mut n = self.inner.written.lock().unwrap();
        let path = format!("{prefix}-{:03}.ppm", *n);
        *n += 1;
        Some(path)
    }

    /// Write a frame drawn somewhere other than a client buffer (a GL
    /// client's host surface): RGBA rows, top first.
    pub fn present_external(&self, width: i32, height: i32, rgba: &[u8]) {
        self.note_frame();
        let Some(prefix) = self.inner.out.lock().unwrap().clone() else {
            return;
        };
        let Some(path) = self.frame_path(&prefix) else { return };
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend(rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("gex: could not write {path}: {e}");
        }
    }

    /// Everything committed since the last call.
    pub fn take_frames(&self) -> Vec<Frame> {
        std::mem::take(&mut self.inner.frames.lock().unwrap())
    }

    pub fn client_count(&self) -> usize {
        self.inner.sessions.lock().unwrap().len()
    }

    /// Move bytes in both directions for every client, and collect commits. The
    /// kernel locates a buffer's pixels through its shared mappings; guest
    /// memory is where they are.
    pub fn pump(&self, kernel: &Kernel, mem: &GuestMem<'_>) {
        // Scripted input waits for a client that can take it (a seat and a surface).
        let ready = self.inner.sessions.lock().unwrap().iter().any(|s| s.client.input_ready());
        let due = if ready { self.due_input() } else { Vec::new() };
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut produced = Vec::new();
        sessions.retain_mut(|s| {
            let mut gone = false;
            // A big read: a toolkit's startup is a few KB of requests at once.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match s.socket.recv(&mut buf, 32, false) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => n as usize,
                    // Nothing more waiting.
                    Err(_) => break,
                };
                let first = s.fds.len();
                s.fds.extend(s.socket.take_received());
                let tokens: Vec<u32> = (first..s.fds.len()).map(|i| i as u32).collect();
                let view = PoolView {
                    kernel,
                    mem,
                    fds: &s.fds,
                };
                s.client.receive(&buf[..n], &tokens, &view);
                if n < buf.len() {
                    break;
                }
            }
            if !due.is_empty() {
                eprintln!("[gex] input: {}", s.client.describe_input());
            }
            let take = s.client.input_ready();
            for &(ev, ms) in due.iter().filter(|_| take) {
                match ev {
                    Ev::Motion(x, y) => s.client.pointer_motion(x, y, ms),
                    Ev::Button(b, down) => s.client.pointer_button(b, down, ms),
                    Ev::Key(k, down) => s.client.keyboard_key(k, down, ms),
                }
            }
            s.out.extend_from_slice(&s.client.take_outgoing());
            // A `wl_keyboard.keymap` event owes a descriptor for its `h`
            // argument: an in-memory file holding the keymap, staged on the
            // socket so it rides the next send (as the browser host does).
            if let Some(km) = s.client.take_keymap() {
                let memfd = Arc::new(MemFd::new("gex-keymap"));
                let _ = memfd.write_at(0, &km);
                let handle: Arc<dyn FileHandle> = memfd;
                let file = OpenFile::new(handle, "anon:wl-keymap".into(), 0, true, false);
                s.socket.stage_send(vec![file]);
            }
            while s.sent < s.out.len() {
                match s.socket.send(&s.out[s.sent..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => s.sent += n as usize,
                }
            }
            if s.sent == s.out.len() {
                s.out.clear();
                s.sent = 0;
            }
            produced.extend(s.client.take_frames());
            !gone && s.client.failed.is_none()
        });
        drop(sessions);
        if !produced.is_empty() {
            self.note_frame();
            // One screen per batch of commits, not one per surface.
            self.save(&produced);
            self.inner.frames.lock().unwrap().extend(produced);
        }
    }
}

impl HostListener for Wayland {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut client = Client::new();
        let (w, h) = *self.inner.size.lock().unwrap();
        client.set_default_size(w, h);
        self.inner.sessions.lock().unwrap().push(Session {
            socket: peer,
            client,
            fds: Vec::new(),
            out: Vec::new(),
            sent: 0,
        });
    }
}

/// Resolves a descriptor the client sent to the pixels behind it.
struct PoolView<'a, 'm> {
    kernel: &'a Kernel,
    mem: &'a GuestMem<'m>,
    fds: &'a [Arc<OpenFile>],
}

impl Buffers for PoolView<'_, '_> {
    fn map(&self, fd: u32, size: usize) -> Option<Vec<u8>> {
        let file = self.fds.get(fd as usize)?;
        // The client mapped this shared, so the live pixels are in the guest's
        // memory; the file's own bytes are what was there before it drew (zero).
        let map = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))?;
        let len = size.min(map.len as usize);
        self.mem.read(map.start, len).ok().map(|b| b.to_vec())
    }
}
