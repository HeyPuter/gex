//! A Wayland compositor for the guest in a browser: `$XDG_RUNTIME_DIR/wayland-0`.
//!
//! [`gex_wayland`] is the protocol as a state machine; this joins it to a guest
//! and to the page. It binds a name in the unix-socket namespace (see
//! `mount_devices`), pumps bytes between each client and its compositor state
//! once per scheduler turn (the one place every thread passes through on its
//! way to waiting -- so a client's blocked wait always has a serviced reply),
//! and hands each committed frame's pixels to the page to blit onto the canvas.
//!
//! This mirrors `shims/native/src/wayland.rs`, minus the GPU compositor and the
//! PPM file output: the browser has one surface (the canvas) and composites
//! there. Shared memory only, like the protocol -- a client that insists on a
//! GPU buffer (`linux-dmabuf`/`wl_drm`) finds none and falls back to `wl_shm`.

use gex_kernel::file::OpenFile;
use gex_kernel::syscalls::Kernel;
use gex_kernel::unix::{HostListener, MemFd, UnixSocket};
use gex_kernel::vfs::FileHandle;
use gex_kernel::GuestMem;
use gex_wayland::{Buffers, Client, Frame};
use std::sync::{Arc, Mutex};

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Put a frame on the canvas. `ptr` is an address in this shared memory of
    /// `stride*height` bytes of `wl_shm` pixels (format 0 = ARGB8888, 1 =
    /// XRGB8888); the page reads them and draws. Called synchronously from the
    /// scheduler pump, so the page's `putImageData` happens off a message it
    /// posts to itself -- the pixels are in shared memory either way.
    fn wl_present(ptr: u32, width: i32, height: i32, stride: i32, format: u32);
    /// Pull up to `max` page input events into `ptr`, packed as i32 quintuples
    /// `[kind, a, b, c, d]`; returns how many were written. The reverse of
    /// `wl_present`: the page fills a ring from its mouse/keyboard listeners and
    /// this drains it into wasm memory. `kind` selects the event: 0 motion(x, y,
    /// time), 1 button(code, pressed, time), 2 axis(axis, value, time), 3
    /// key(code, pressed, time), 4 modifiers(depressed, latched, locked, group).
    /// Coordinates are surface-local pixels; button/key codes are Linux
    /// `BTN_*`/evdev numbers, which is what Wayland carries.
    fn wl_poll_input(ptr: u32, max: u32) -> u32;
}

/// A page input event, decoded from the poll buffer and applied to the client.
enum InputEvent {
    Motion { x: i32, y: i32, time: u32 },
    Button { button: u32, pressed: bool, time: u32 },
    Axis { axis: u32, value: i32, time: u32 },
    Key { key: u32, pressed: bool, time: u32 },
    Mods { depressed: u32, latched: u32, locked: u32, group: u32 },
}

/// The most events pulled from the page in one pump. A page cannot generate
/// more than a handful per turn; this is roomy and keeps the buffer on the
/// stack.
const MAX_POLL: usize = 64;

/// Pull and decode this turn's page input.
fn poll_input() -> Vec<InputEvent> {
    if !have_wl() {
        return Vec::new();
    }
    let mut raw = [0i32; MAX_POLL * 5];
    let n = (unsafe { wl_poll_input(raw.as_mut_ptr() as u32, MAX_POLL as u32) } as usize)
        .min(MAX_POLL);
    (0..n)
        .filter_map(|i| {
            let q = &raw[i * 5..i * 5 + 5];
            Some(match q[0] {
                0 => InputEvent::Motion { x: q[1], y: q[2], time: q[3] as u32 },
                1 => InputEvent::Button { button: q[1] as u32, pressed: q[2] != 0, time: q[3] as u32 },
                2 => InputEvent::Axis { axis: q[1] as u32, value: q[2], time: q[3] as u32 },
                3 => InputEvent::Key { key: q[1] as u32, pressed: q[2] != 0, time: q[3] as u32 },
                4 => InputEvent::Mods {
                    depressed: q[1] as u32,
                    latched: q[2] as u32,
                    locked: q[3] as u32,
                    group: q[4] as u32,
                },
                _ => return None,
            })
        })
        .collect()
}

/// Set when the page said it has a canvas for the compositor. Off by default,
/// so a page without one has the guest find no `wayland-0` (a clean connect
/// failure) rather than a compositor whose frames go nowhere.
static mut HAVE_WL: bool = false;

/// Say that the page has a canvas behind [`wl_present`].
///
/// # Safety
/// Called from JS before the guest runs, on the one thread a page has.
#[no_mangle]
pub extern "C" fn gex_set_wl(on: i32) {
    unsafe { HAVE_WL = on != 0 }
}

pub fn have_wl() -> bool {
    unsafe { HAVE_WL }
}

/// The default screen size, until a client asks for its own.
pub const DEFAULT_SIZE: (i32, i32) = (640, 480);

/// Where the compositor binds. The demo sets `XDG_RUNTIME_DIR=/tmp` (which
/// always exists in the root, unlike `/run`), so the well-known name is
/// `/tmp/wayland-0` -- what libwayland forms from `$XDG_RUNTIME_DIR/wayland-0`.
pub const DEFAULT_SOCKET: &str = "/tmp/wayland-0";

/// One connected client: its socket, its protocol state, and the descriptors
/// it has sent (referred to by arrival index, not the client's fd number).
struct Session {
    socket: Arc<UnixSocket>,
    client: Client,
    fds: Vec<Arc<OpenFile>>,
    /// Event bytes the socket had no room for yet, and how many have gone.
    out: Vec<u8>,
    sent: usize,
}

#[derive(Default)]
struct Inner {
    sessions: Mutex<Vec<Session>>,
    size: Mutex<(i32, i32)>,
    /// Pixels of the last frame presented, kept alive across the `wl_present`
    /// call so the address handed to the page stays valid until it has read.
    held: Mutex<Vec<u8>>,
}

/// The compositor as the guest sees it: a name that accepts connections.
#[derive(Clone)]
pub struct WaylandDevice {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for WaylandDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WaylandDevice")
    }
}

impl WaylandDevice {
    pub fn new() -> WaylandDevice {
        let dev = WaylandDevice {
            inner: Arc::new(Inner::default()),
        };
        *dev.inner.size.lock().unwrap() = DEFAULT_SIZE;
        dev
    }

    /// Move bytes both ways for every client and present what committed. Run
    /// once per scheduler turn. `kernel`/`mem` are how a buffer's pixels are
    /// found: a client maps its `wl_shm` pool `MAP_SHARED`, which aliases a
    /// range of guest memory, so the compositor reads the framebuffer with no
    /// copy of its own.
    pub fn pump(&self, kernel: &Kernel, mem: &GuestMem<'_>) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut produced: Vec<Frame> = Vec::new();
        // Page input for this turn, applied below to every client that has bound
        // a device. One event set reaches each such client once; the demo has a
        // single input client (Xwayland or the native gears), so this is exact.
        //
        // Drained ONLY when a client is there to take it, and that is the whole
        // difference between a working pointer and the bug this replaced. There
        // is one input ring on the page and a `WaylandDevice` in EVERY guest
        // process -- `mount_devices` gives one to pid 1 and to every child it
        // adopts -- so every process's pump was draining the same ring. The
        // events a process with no client drained were dropped on the floor,
        // and which process won each event was a scheduler race: the shell, or
        // Xwayland, or the client. Motion survived it (there are hundreds of
        // events and losing most of them still tracks the cursor), and a click
        // did not -- press and release are one event each, so a click landed
        // only when both won the race, and a lone surviving press left a button
        // stuck down. The queue is the right place for an event no one can take
        // yet: it stays in the ring, and the pump that has the client gets it.
        let wanted = sessions.iter().any(|s| s.client.wants_input());
        let input = if wanted { poll_input() } else { Vec::new() };
        sessions.retain_mut(|s| {
            let mut gone = false;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match s.socket.recv(&mut buf, 32, false) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => n as usize,
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
            // Deliver the page's input now that any `get_pointer`/`get_keyboard`
            // from this turn's bytes has been processed.
            if s.client.wants_input() && !input.is_empty() {
                // A self-limiting note that input reached a client end to end
                // (page ring -> poll -> compositor -> this client), for the first
                // few events only so a busy session does not narrate every move.
                use std::sync::atomic::{AtomicU32, Ordering};
                static SEEN: AtomicU32 = AtomicU32::new(0);
                if SEEN.fetch_add(input.len() as u32, Ordering::Relaxed) < 24 {
                    unsafe {
                        crate::write_stderr(&format!(
                            "[wl] delivered {} input event(s) to a client\n",
                            input.len()
                        ));
                    }
                }
                for ev in &input {
                    match *ev {
                        InputEvent::Motion { x, y, time } => s.client.pointer_motion(x, y, time),
                        InputEvent::Button { button, pressed, time } => {
                            s.client.pointer_button(button, pressed, time)
                        }
                        InputEvent::Axis { axis, value, time } => {
                            s.client.pointer_axis(axis, value, time)
                        }
                        InputEvent::Key { key, pressed, time } => {
                            s.client.keyboard_key(key, pressed, time)
                        }
                        InputEvent::Mods { depressed, latched, locked, group } => {
                            s.client.keyboard_modifiers(depressed, latched, locked, group)
                        }
                    }
                }
            }
            s.out.extend_from_slice(&s.client.take_outgoing());
            // A `wl_keyboard.keymap` event owes a readable descriptor for its
            // `h` argument. The compositor state machine cannot make one (it has
            // no kernel); here we back it with an in-memory file holding the xkb
            // keymap and stage it on the socket, so it rides the next send as
            // ancillary data and satisfies the first `h` the client reads.
            if let Some(km) = s.client.take_keymap() {
                let memfd = Arc::new(MemFd::new("gex-keymap"));
                let _ = memfd.write_at(0, &km);
                let handle: Arc<dyn FileHandle> = memfd;
                // A name that is not a path: the client reads the keymap through
                // this fd (mmap resolves it by descriptor, not by name) and then
                // closes it, so it never needs reopening -- and `fork`'s fd
                // rebuild skips a non-`/` name silently instead of warning that
                // it "left the fd behind", which a `/memfd:` name would trigger
                // every time a client holding it forked.
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
            let frames = s.client.take_frames();
            // A self-limiting note of what actually committed -- surface and
            // size -- so a client that draws nothing to the compositor can be
            // told from one whose frames simply do not reach the canvas.
            if !frames.is_empty() {
                use std::sync::atomic::{AtomicU32, Ordering};
                static F: AtomicU32 = AtomicU32::new(0);
                if F.fetch_add(frames.len() as u32, Ordering::Relaxed) < 16 {
                    for fr in &frames {
                        unsafe {
                            crate::write_stderr(&format!(
                                "[wl] commit: surface {} {}x{}\n",
                                fr.surface, fr.width, fr.height
                            ));
                        }
                    }
                }
            }
            produced.extend(frames);
            !gone && s.client.failed.is_none()
        });
        drop(sessions);
        // Present the newest commit. A frame already carries its pixels (copied
        // out of the client's pool during `receive`), so there is nothing left
        // to resolve here -- just swizzle to the RGBA the canvas wants and hand
        // the page the address. `held` keeps the bytes alive across the call.
        if let Some(frame) = produced.last() {
            let rgba = to_rgba(frame);
            let (w, h) = (frame.width, frame.height);
            let mut held = self.inner.held.lock().unwrap();
            *held = rgba;
            unsafe { wl_present(held.as_ptr() as u32, w, h, w * 4, frame.format) };
        }
    }

    pub fn client_count(&self) -> usize {
        self.inner.sessions.lock().unwrap().len()
    }
}

/// A frame's pixels as tightly-packed RGBA for a canvas `ImageData`. The
/// `wl_shm` formats are ARGB8888/XRGB8888, little-endian, so a pixel's bytes
/// are B,G,R,A; the canvas wants R,G,B,A. Alpha is forced opaque -- XRGB has
/// none, and a partially-transparent toplevel over a blank page is not wanted.
fn to_rgba(frame: &Frame) -> Vec<u8> {
    let (w, h) = (frame.width.max(0) as usize, frame.height.max(0) as usize);
    let stride = frame.stride.max(0) as usize;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = y * stride;
        for x in 0..w {
            let at = row + x * 4;
            let o = (y * w + x) * 4;
            if at + 3 < frame.pixels.len() {
                out[o] = frame.pixels[at + 2];
                out[o + 1] = frame.pixels[at + 1];
                out[o + 2] = frame.pixels[at];
                out[o + 3] = 255;
            } else {
                out[o + 3] = 255;
            }
        }
    }
    out
}

impl HostListener for WaylandDevice {
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
        // Mapped shared, so the live pixels are in the guest's memory; the
        // file's own bytes are what was there before it drew.
        let map = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))?;
        let len = size.min(map.len as usize);
        self.mem.read(map.start, len).ok().map(|b| b.to_vec())
    }
}
