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
use gex_wayland::{Buffers, Client, Frame, Placement};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One connected client: its socket, its protocol state, and the descriptors
/// it has sent.
struct Session {
    /// Which connection this is, for the screen's keys: surface ids are only
    /// unique within one client.
    id: u64,
    /// The guest process that connected. Its requests are read only by a
    /// pump running in that process: a buffer's pixels are found through the
    /// process's own shared mappings and memory, and a commit read anywhere
    /// else found neither and was dropped -- most of a Wine session's frames
    /// never reached the screen.
    owner: Option<i32>,
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
    /// Where requests are read into, kept: `pump` runs at every syscall of
    /// every thread, and a fresh 64 KiB zeroed buffer each time (calloc's
    /// memset, and glibc growing and trimming its heap around it) was most
    /// of what a W3 game's threads spent their CPU on. Keeping it took
    /// Retrocycles (edger, gameplay) from 3.0-3.5 fps to 38-46 fps.
    rbuf: Vec<u8>,
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
    /// What is on screen once a client places windows.
    screen: Mutex<Screen>,
    next_session: Mutex<u64>,
}

fn keytrace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_WL_KEYTRACE").is_some())
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
        crate::stall::frame();
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
        // `GEX_PPM_GL_ONLY=1`: only gex-gl's presented frames are written
        // (`present_external`), not client buffers -- with a GL game on
        // Xwayland, the X window's own buffer is black and would take the
        // GEX_PPM_EVERY turns.
        static GL_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *GL_ONLY.get_or_init(|| std::env::var_os("GEX_PPM_GL_ONLY").is_some_and(|v| v != "0")) {
            return;
        }
        // Before the GPU read-back, so a skipped frame costs nothing.
        let Some(path) = self.frame_path(&prefix) else { return };
        let mut gpu = self.inner.gpu.lock().unwrap();
        let mut screen = self.inner.screen.lock().unwrap();
        if screen.active() {
            screen.list();
        }
        let (width, height, rgb) = if let Some(c) = gpu.as_mut() {
            let drawn = if screen.active() { c.draw_at(&screen.items()) } else { c.draw(frames) };
            match drawn.and_then(|()| c.read_back()) {
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
        PROCESS.with(|p| p.set(Some(kernel.pid)));
        // Scripted input waits for a client that can take it (a seat and a
        // surface); a GL-only window gets the keyboard by default.
        if self.inner.input.lock().unwrap().is_some() {
            for s in self.inner.sessions.lock().unwrap().iter_mut() {
                s.client.focus_fallback();
            }
        }
        // Placed windows: the screen knows where each is, so it decides which
        // client an event is for, as a compositor does.
        let placing = self.inner.screen.lock().unwrap().active();
        let ready = self.inner.sessions.lock().unwrap().iter().any(|s| {
            if placing { s.client.wants_input() } else { s.client.input_ready() }
        });
        let due = if ready { self.due_input() } else { Vec::new() };
        let mut routed = if placing && !due.is_empty() {
            self.inner.screen.lock().unwrap().route(&due)
        } else {
            Vec::new()
        };
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut produced = Vec::new();
        sessions.retain_mut(|s| {
            let mut gone = false;
            let mine = s.owner.is_none_or(|o| o == kernel.pid);
            // A big read: a toolkit's startup is a few KB of requests at once.
            let mut buf = std::mem::take(&mut s.rbuf);
            if buf.is_empty() {
                buf = vec![0u8; 64 * 1024];
            }
            while mine {
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
            s.rbuf = buf;
            if !due.is_empty() {
                eprintln!("[gex] input: {}", s.client.describe_input());
            }
            let take = s.client.input_ready() && !placing;
            for &(ev, ms) in due.iter().filter(|_| take) {
                match ev {
                    Ev::Motion(x, y) => s.client.pointer_motion(x, y, ms),
                    Ev::Button(b, down) => s.client.pointer_button(b, down, ms),
                    Ev::Key(k, down) => s.client.keyboard_key(k, down, ms),
                }
            }
            for (_, ev) in routed.iter().filter(|r| r.0 == s.id) {
                match *ev {
                    Routed::Origin(surface, x, y) => s.client.placement_origin(surface, x, y),
                    Routed::PointerTo(surface, x, y, ms) => s.client.pointer_to(surface, x, y, ms),
                    Routed::PointerClear => s.client.pointer_clear(),
                    Routed::Button(b, down, ms) => s.client.pointer_button(b, down, ms),
                    Routed::Key(k, down, ms) => s.client.keyboard_key(k, down, ms),
                    Routed::KeyboardFocus(surface) => s.client.keyboard_focus(surface),
                    Routed::KeyboardLeave => s.client.keyboard_focus(0),
                }
            }
            routed.retain(|r| r.0 != s.id);
            let frames = s.client.take_frames();
            self.inner.screen.lock().unwrap().update(s.id, &mut s.client, &frames);
            produced.extend(frames);
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
            let pending = s.out.len() - s.sent;
            let mut failed = None;
            while s.sent < s.out.len() {
                match s.socket.send(&s.out[s.sent..]) {
                    Ok(0) => break,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                    Ok(n) => s.sent += n as usize,
                }
            }
            if keytrace() && (failed.is_some() || s.sent < s.out.len() || !due.is_empty()) {
                eprintln!("[wl-key] pump: {pending} bytes to send, {} left, error {failed:?}", s.out.len() - s.sent);
            }
            if s.sent == s.out.len() {
                s.out.clear();
                s.sent = 0;
            }
            let keep = !gone && s.client.failed.is_none();
            if !keep {
                self.inner.screen.lock().unwrap().forget(s.id);
            }
            keep
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

thread_local! {
    /// The guest process this host thread runs, as its last pump said: a
    /// connection is accepted inside the connecting process's own `connect`.
    static PROCESS: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// A surface on the screen: its client's connection and its id there.
type Key = (u64, u32);

/// The screen, natively, once a client places windows
/// (`gex_window_placement_v1`): each surface's last frame, and the placed
/// ones where their clients put them, in the order they stacked them. Enough
/// of a window manager to check placement, stacking and click-through on a
/// host with no browser: the browser's compositor (`compositor-worker.js`)
/// is the one that matters, and does the same with the same records.
///
/// Until anything is placed it is not used, and frames are drawn as they
/// always were: each batch of commits, centred.
#[derive(Default)]
struct Screen {
    frames: HashMap<Key, Frame>,
    /// Placed surfaces: position and placement.
    placed: HashMap<Key, (i32, i32, Placement)>,
    /// Placed surfaces bottom to top, before layers are applied.
    order: Vec<Key>,
    /// The restack and activate serials already acted on.
    acted: HashMap<Key, (u32, u32)>,
    /// Who has the keyboard (by `activate`) and the pointer (by hit test).
    focus: Option<Key>,
    pointer: Option<Key>,
    /// Sessions owed a keyboard leave: the keyboard went to another client.
    leaves: Vec<u64>,
    /// The window list last printed.
    listed: String,
}

/// Scripted input, routed by the screen to one client.
enum Routed {
    /// A placed surface's origin, which the pointer positions after it are
    /// relative to (`gex_window_placement_v1.origin`).
    Origin(u32, i32, i32),
    PointerTo(u32, i32, i32, u32),
    PointerClear,
    Button(u32, bool, u32),
    Key(u32, bool, u32),
    /// The keyboard to this surface (the screen chose it, no client did).
    KeyboardFocus(u32),
    KeyboardLeave,
}

impl Screen {
    fn active(&self) -> bool {
        !self.placed.is_empty()
    }

    /// A client hung up: its windows are gone.
    fn forget(&mut self, sid: u64) {
        self.frames.retain(|k, _| k.0 != sid);
        self.placed.retain(|k, _| k.0 != sid);
        self.order.retain(|k| k.0 != sid);
        self.acted.retain(|k, _| k.0 != sid);
        if self.focus.is_some_and(|k| k.0 == sid) {
            self.focus = None;
        }
        if self.pointer.is_some_and(|k| k.0 == sid) {
            self.pointer = None;
        }
    }

    /// The placed surfaces bottom to top, layers kept apart.
    fn stack(&self) -> Vec<Key> {
        let mut v = self.order.clone();
        v.sort_by_key(|k| self.placed.get(k).map(|p| p.2.layer).unwrap_or(0));
        v
    }

    /// Take in one client's state after its requests were read.
    fn update(&mut self, sid: u64, client: &mut Client, produced: &[Frame]) {
        for f in produced {
            self.frames.insert((sid, f.surface), f.clone());
        }
        self.frames.retain(|k, _| k.0 != sid || client.is_mapped(k.1));
        let now = client.placed();
        self.placed.retain(|k, _| k.0 != sid || now.iter().any(|p| p.0 == k.1));
        self.order.retain(|k| self.placed.contains_key(k));
        self.acted.retain(|k, _| k.0 != sid || self.placed.contains_key(k));
        let mut restacks = Vec::new();
        for (surface, x, y, p) in now {
            let k = (sid, surface);
            self.placed.insert(k, (x, y, p));
            if !self.order.contains(&k) {
                self.order.push(k);
            }
            let (restack, activate) = self.acted.get(&k).copied().unwrap_or((0, 0));
            if p.restack != restack && p.restack != 0 {
                restacks.push((k, p.below));
            }
            if p.activate != activate && p.activate != 0 {
                if let Some(old) = self.focus.filter(|o| o.0 != sid) {
                    self.leaves.push(old.0);
                }
                self.focus = Some(k);
                client.keyboard_focus(surface);
            }
            self.acted.insert(k, (p.restack, p.activate));
        }
        for (k, below) in restacks {
            self.restack(k, below);
        }
        if self.focus.is_some_and(|f| f.0 == sid && !self.placed.contains_key(&f)) {
            self.focus = None;
        }
    }

    fn restack(&mut self, k: Key, below: u32) {
        self.order.retain(|o| *o != k);
        let at = if below == Placement::BOTTOM {
            0
        } else if below == 0 {
            self.order.len()
        } else {
            self.order
                .iter()
                .position(|o| self.placed.get(o).is_some_and(|p| p.2.id == below))
                .unwrap_or(self.order.len())
        };
        self.order.insert(at, k);
    }

    /// The topmost placed surface at a point, and the point in it.
    fn hit(&self, x: i32, y: i32) -> Option<(Key, i32, i32)> {
        for k in self.stack().into_iter().rev() {
            let (Some(&(px, py, _)), Some(f)) = (self.placed.get(&k), self.frames.get(&k)) else {
                continue;
            };
            if x >= px && x < px + f.width && y >= py && y < py + f.height {
                return Some((k, x - px, y - py));
            }
        }
        None
    }

    /// Route scripted events: the pointer to whatever placed window is under
    /// it, buttons with it, keys to the window its client activated.
    fn route(&mut self, due: &[(Ev, u32)]) -> Vec<(u64, Routed)> {
        let mut out: Vec<(u64, Routed)> = self.leaves.drain(..).map(|s| (s, Routed::KeyboardLeave)).collect();
        for &(ev, ms) in due {
            match ev {
                Ev::Motion(x, y) => {
                    let hit = self.hit(x, y);
                    if let Some(old) = self.pointer {
                        if hit.map(|h| h.0 .0) != Some(old.0) {
                            out.push((old.0, Routed::PointerClear));
                        }
                    }
                    eprintln!("[gex] input: pointer at {x},{y} is over {:?}", hit.map(|h| h.0));
                    self.pointer = hit.map(|h| h.0);
                    if let Some(((sid, surface), lx, ly)) = hit {
                        out.push((sid, Routed::Origin(surface, x - lx, y - ly)));
                        out.push((sid, Routed::PointerTo(surface, lx, ly, ms)));
                    }
                }
                Ev::Button(b, down) => {
                    if let Some((sid, _)) = self.pointer {
                        out.push((sid, Routed::Button(b, down, ms)));
                    }
                }
                Ev::Key(k, down) => {
                    // No client has activated a window (Wine activates its
                    // own, and a slow start can leave none active when the
                    // script's keys come): the topmost window gets the
                    // keyboard, as a compositor focuses the window on top.
                    if self.focus.is_none() {
                        if let Some(top) = self.stack().last().copied() {
                            eprintln!("[gex] input: no window has the keyboard; it goes to the top one, {}:{}", top.0, top.1);
                            self.focus = Some(top);
                            out.push((top.0, Routed::KeyboardFocus(top.1)));
                        }
                    }
                    if let Some((sid, _)) = self.focus {
                        out.push((sid, Routed::Key(k, down, ms)));
                    }
                }
            }
        }
        out
    }

    /// What to draw, bottom to top: windows nobody placed centred, as
    /// before, then the placed ones where they are.
    fn items(&self) -> Vec<(u64, &Frame, Option<(i32, i32)>)> {
        let key = |k: &Key| (k.0 << 32) | u64::from(k.1);
        let mut loose: Vec<_> = self.frames.iter().filter(|(k, _)| !self.placed.contains_key(k)).collect();
        loose.sort_by_key(|(k, _)| **k);
        let mut out: Vec<_> = loose.into_iter().map(|(k, f)| (key(k), f, None)).collect();
        for k in self.stack() {
            if let (Some(f), Some(&(x, y, _))) = (self.frames.get(&k), self.placed.get(&k)) {
                out.push((key(&k), f, Some((x, y))));
            }
        }
        out
    }

    /// The window list, bottom to top, printed when it changes: what a test
    /// reads to check placement and stacking.
    fn list(&mut self) {
        let mut s = String::new();
        for k in self.stack() {
            let (Some(&(x, y, p)), Some(f)) = (self.placed.get(&k), self.frames.get(&k)) else {
                continue;
            };
            s += &format!(
                " {}:{} id={:#x} {}x{}+{}+{} layer={}{}",
                k.0, k.1, p.id, f.width, f.height, x, y, p.layer,
                if self.focus == Some(k) { " focus" } else { "" }
            );
        }
        if s != self.listed {
            eprintln!("[wl-place]{s}");
            self.listed = s;
        }
    }
}

impl HostListener for Wayland {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut client = Client::new();
        let (w, h) = *self.inner.size.lock().unwrap();
        client.set_default_size(w, h);
        let id = {
            let mut n = self.inner.next_session.lock().unwrap();
            *n += 1;
            *n
        };
        self.inner.sessions.lock().unwrap().push(Session {
            id,
            owner: PROCESS.with(|p| p.get()),
            socket: peer,
            client,
            fds: Vec::new(),
            out: Vec::new(),
            sent: 0,
            rbuf: Vec::new(),
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
            .mem
            .lock()
            .log
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))
            .cloned()?;
        let len = size.min(map.len as usize);
        self.mem.read(map.start, len).ok().map(|b| b.to_vec())
    }
}
