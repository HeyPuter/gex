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
use gex_wayland::{Buffers, Client, SurfaceInfo};
use std::sync::{Arc, Mutex};

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Hand the host this pump's scene: `count` fixed-size records at `ptr`,
    /// one per mapped surface, laid out as [`REC`] words each.
    ///
    /// The records are descriptors, not pixels. A `wl_shm` buffer lives in the
    /// guest's memory and the host can read that memory directly, so what
    /// crosses here is where each surface's pixels are -- an address, a stride,
    /// a size, and the rectangle that changed -- and the compositor uploads the
    /// damaged rows straight out of the guest. Nothing copies a frame.
    fn wl_publish(ptr: u32, count: u32);
    /// Pull up to `max` input and control events into `ptr`, packed as i32
    /// records of [`EV`] words: `[kind, a, b, c, d, client, surface, pad]`.
    /// Returns how many were written.
    ///
    /// The reverse of `wl_publish`: the compositor knows where every window is,
    /// so it does the hit test and names the surface each event is for. `kind`
    /// selects the event -- 0 motion(x, y, time), 1 button(code, pressed,
    /// time), 2 axis(axis, value, time), 3 key(code, pressed, time),
    /// 4 modifiers(depressed, latched, locked, group), 5 the pointer left this
    /// client, 6 keyboard focus, 7 configure(width, height, activated),
    /// 8 close, 9 dismiss popup. Coordinates are surface-local pixels;
    /// button/key codes are Linux `BTN_*`/evdev numbers.
    fn wl_poll_input(ptr: u32, max: u32) -> u32;
    /// Whether this process's input ring holds an event, without taking it.
    fn wl_input_pending() -> u32;
}

/// The floor under [`WaylandDevice::needs_pump`]: whatever it does not look
/// at still gets a pump at least this often.
const PUMP_FLOOR_NS: u64 = 50_000_000;

/// Words in one published surface record. Fixed so both sides can index it
/// without a schema: see `wl-bridge.mjs`, which reads the same numbers.
const REC: usize = 72;
/// Words in one input event record.
const EV: usize = 8;
/// The most surfaces published in one pump. A desktop session with a dozen
/// windows, their menus and their tooltips fits several times over.
const MAX_SURFACES: usize = 64;
/// Bytes of UTF-8 title carried in a record, which is what a titlebar shows.
const TITLE_BYTES: usize = 32;

/// A page input or control event, decoded from the poll buffer.
enum InputEvent {
    Motion { client: u32, surface: u32, x: i32, y: i32, time: u32 },
    Button { client: u32, button: u32, pressed: bool, time: u32 },
    Axis { client: u32, axis: u32, value: i32, time: u32 },
    Key { client: u32, key: u32, pressed: bool, time: u32 },
    Mods { client: u32, depressed: u32, latched: u32, locked: u32, group: u32 },
    PointerClear { client: u32 },
    KeyboardFocus { client: u32, surface: u32 },
    Configure { client: u32, surface: u32, w: i32, h: i32, states: u32 },
    Close { client: u32, surface: u32 },
    DismissPopup { client: u32, surface: u32 },
    /// The compositor moved a window. For an X window this has to reach the X
    /// server, so that a menu the client positions in X coordinates lands
    /// where the window now is.
    Moved { client: u32, surface: u32, x: i32, y: i32 },
}

impl InputEvent {
    /// Which client this is addressed to. A process can host more than one
    /// (Xwayland and a native client both connect), and an event for one must
    /// not reach the other.
    fn client(&self) -> u32 {
        match *self {
            InputEvent::Motion { client, .. }
            | InputEvent::Button { client, .. }
            | InputEvent::Axis { client, .. }
            | InputEvent::Key { client, .. }
            | InputEvent::Mods { client, .. }
            | InputEvent::PointerClear { client }
            | InputEvent::KeyboardFocus { client, .. }
            | InputEvent::Configure { client, .. }
            | InputEvent::Close { client, .. }
            | InputEvent::DismissPopup { client, .. }
            | InputEvent::Moved { client, .. } => client,
        }
    }
}

/// The most events pulled from the page in one pump.
const MAX_POLL: usize = 64;

/// Pull and decode this turn's page input.
fn poll_input() -> Vec<InputEvent> {
    if !have_wl() {
        return Vec::new();
    }
    let mut raw = [0i32; MAX_POLL * EV];
    let n = (unsafe { wl_poll_input(raw.as_mut_ptr() as u32, MAX_POLL as u32) } as usize)
        .min(MAX_POLL);
    (0..n)
        .filter_map(|i| {
            let q = &raw[i * EV..i * EV + EV];
            let (client, surface) = (q[5] as u32, q[6] as u32);
            Some(match q[0] {
                0 => InputEvent::Motion { client, surface, x: q[1], y: q[2], time: q[3] as u32 },
                1 => InputEvent::Button {
                    client,
                    button: q[1] as u32,
                    pressed: q[2] != 0,
                    time: q[3] as u32,
                },
                2 => InputEvent::Axis {
                    client,
                    axis: q[1] as u32,
                    value: q[2],
                    time: q[3] as u32,
                },
                3 => InputEvent::Key {
                    client,
                    key: q[1] as u32,
                    pressed: q[2] != 0,
                    time: q[3] as u32,
                },
                4 => InputEvent::Mods {
                    client,
                    depressed: q[1] as u32,
                    latched: q[2] as u32,
                    locked: q[3] as u32,
                    group: q[4] as u32,
                },
                5 => InputEvent::PointerClear { client },
                6 => InputEvent::KeyboardFocus { client, surface },
                7 => InputEvent::Configure {
                    client,
                    surface,
                    w: q[1],
                    h: q[2],
                    states: gex_wayland::configure_states(q[3], q[4]),
                },
                8 => InputEvent::Close { client, surface },
                9 => InputEvent::DismissPopup { client, surface },
                10 => InputEvent::Moved { client, surface, x: q[1], y: q[2] },
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

/// The output size, as the page's compositor canvas. A toplevel is configured
/// to this and `wl_output` reports it, so a client sizes its window to the
/// space there actually is.
static mut OUTPUT: (i32, i32) = DEFAULT_SIZE;

/// Say how large the compositor's output is.
///
/// # Safety
/// Called from JS before the guest runs, on the one thread a page has.
#[no_mangle]
pub extern "C" fn gex_set_wl_output(w: i32, h: i32) {
    unsafe { OUTPUT = (w.max(1), h.max(1)) }
}

fn output_size() -> (i32, i32) {
    unsafe { OUTPUT }
}

static OUTPUT_SCALE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// Device pixels per surface unit (`wl_output.scale`): the page's
/// devicePixelRatio, rounded, when its output is in device pixels.
#[no_mangle]
pub extern "C" fn gex_set_wl_scale(scale: i32) {
    OUTPUT_SCALE.store(scale.clamp(1, 8), std::sync::atomic::Ordering::Relaxed);
}

/// This process's name in the compositor's scene arena: its pid.
///
/// It has to be the *process's* and not the Worker's. A process's threads are
/// each a Worker with their own shim instance and their own JS side, and every
/// one of them can run the scheduler pump and publish this process's surfaces.
/// A token per Worker therefore showed one window once per thread -- four
/// stacked copies of KCalc, the newest of them empty -- and a random number
/// per process cannot be agreed on by Workers that share no statics. The pid
/// is the one name every Worker of a process already knows.
///
/// Zero until the process has a pid (`gex_load` or `gex_adopt_child` sets it),
/// which is why the JS side asks again rather than caching a zero.
/// This process's compositor, for a device that has to reach it.
///
/// The GL device's `GEX_GL_SURFACE` is the one: a client that renders on the
/// host has to tell the compositor which of its surfaces that is, and the two
/// devices know nothing of each other. Process-wide rather than per-thread
/// because the ioctl arrives on whichever thread the client called it from.
static COMPOSITOR: std::sync::Mutex<Option<WaylandDevice>> = std::sync::Mutex::new(None);

/// Whether this process's compositor has anything to move (`needs_pump`),
/// for a scheduler turn that takes no lock (`waitq::idle_turn`): if so, the
/// turn takes the lock and pumps. A compositor busy elsewhere is a yes.
pub fn pump_wanted() -> bool {
    let device = match COMPOSITOR.try_lock() {
        Ok(d) => d.clone(),
        Err(_) => return true,
    };
    device.is_some_and(|d| d.needs_pump())
}

/// Remember it at mount, so the GL device can find it later.
pub fn set_compositor(device: &WaylandDevice) {
    *COMPOSITOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(device.clone());
}

/// Say that a surface of this process's client is drawn by the host.
///
/// Surface ids are per-client, and a process with a GL client has one;
/// marking every session is what makes that true without the caller having to
/// know its own client number.
pub fn attach_external(surface: u32, w: i32, h: i32) {
    let device = COMPOSITOR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(device) = device else { return };
    {
        let mut sessions = device
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            crate::write_stderr(&format!(
                "[wl] external: surface {surface} {w}x{h} to {} session(s)\n",
                sessions.len()
            ));
        }
        for session in sessions.iter_mut() {
            session.client.attach_external(surface, w, h);
        }
    }
    // And publish it now rather than waiting for a pump. A client that has
    // just said this is about to stop talking to its compositor: its frames
    // go to the host from here on, and nothing it does will drive another
    // pump. Waiting for one means the window never appears at all.
    device.publish_scene();
}

#[no_mangle]
pub extern "C" fn gex_wl_token() -> u32 {
    crate::my_pid().max(0) as u32
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
    /// Pumps since this session started, for rate-limiting the attempt to
    /// reach an X server that may not be listening yet.
    xwm_tries: u32,
    /// The X window manager, once this client has turned out to be an X server.
    /// The compositor *is* the window manager -- there is no second one in the
    /// guest -- so it lives here, beside the Wayland half of the same client.
    xwm: Option<crate::xwm::Xwm>,
    /// This client's number within this process, which is how the compositor
    /// addresses it: surface ids are per-client, so two clients in one process
    /// can both own a surface 3.
    idx: u32,
    fds: Vec<Arc<OpenFile>>,
    /// Event bytes the socket had no room for yet, and how many have gone.
    out: Vec<u8>,
    sent: usize,
    /// Where requests are read into, kept rather than a fresh zeroed 64 KiB
    /// each pump (as the native host's `wayland.rs`).
    rbuf: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    sessions: Mutex<Vec<Session>>,
    size: Mutex<(i32, i32)>,
    /// Clients numbered so far, so an index is never reused by a later
    /// connection -- the compositor keys its window state on it.
    next_idx: Mutex<u32>,
    /// How many clients were live at the last publish, so a client going away
    /// is a scene change even when no one drew.
    published: Mutex<usize>,
    /// When a pump last ran (host ns), for `PUMP_FLOOR_NS`.
    last_pump: std::sync::atomic::AtomicU64,
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
        // The compositor's own diagnostics -- a key with nowhere to go, a
        // buffer that could not be placed -- go to this process's stderr
        // whether or not protocol tracing is on. The verbose per-message trace
        // stays behind `?wltrace=1`; these are the lines that answer "why is
        // nothing happening", and they should not need a flag.
        fn sink(s: &str) {
            unsafe { crate::write_stderr(&format!("{s}\n")) };
        }
        gex_wayland::set_trace_sink(sink);
        // The window manager's notes go the same way: it is a crate now, and
        // its notes belong beside the compositor's in the page's terminal.
        gex_xwm::set_trace_sink(sink);
        let dev = WaylandDevice {
            inner: Arc::new(Inner::default()),
        };
        *dev.inner.size.lock().unwrap() = output_size();
        dev
    }

    /// Move bytes both ways for every client, hand the compositor this turn's
    /// scene, and take its input. Run once per scheduler turn. Says whether it
    /// sent any client anything: a reply written here is readiness no
    /// syscall announced, so the caller wakes the parked Workers.
    ///
    /// `kernel`/`mem` are how a buffer is found: a client maps its `wl_shm`
    /// pool `MAP_SHARED`, which aliases a range of guest memory, so what the
    /// compositor is told is the address of that range -- nothing is copied
    /// here, and nothing is composited here either.
    pub fn pump(&self, kernel: &Kernel, mem: &GuestMem<'_>) -> bool {
        self.inner.last_pump.store(crate::now_ns(), std::sync::atomic::Ordering::Relaxed);
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut sent_any = false;
        // Compositor input and control for this turn, addressed by client: a
        // process can host Xwayland and a native client at once, and an event
        // for one must not reach the other.
        //
        // Drained ONLY when a client is there to take it, and that is the whole
        // difference between a working pointer and the bug this replaced. There
        // is one input ring per process on the page and a `WaylandDevice` in
        // EVERY guest process -- `mount_devices` gives one to pid 1 and to every
        // child it adopts -- so a pump with no client would drain events meant
        // for the process that has one and drop them on the floor. Motion
        // survived that (hundreds of events, and losing most still tracks the
        // cursor); a click did not, because press and release are one event
        // each. The queue is the right place for an event no one can take yet.
        let wanted = sessions.iter().any(|s| s.client.wants_input());
        let input = if wanted { poll_input() } else { Vec::new() };
        let mut scene: Vec<i32> = Vec::new();
        let mut changed = false;
        sessions.retain_mut(|s| {
            let mut gone = false;
            let mut buf = std::mem::take(&mut s.rbuf);
            if buf.is_empty() {
                buf = vec![0u8; 64 * 1024];
            }
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
            s.rbuf = buf;
            // Deliver this turn's events now that any `get_pointer`/
            // `get_keyboard` from this turn's bytes has been processed.
            if s.client.wants_input() {
                // A self-limiting note that input reached a client end to end
                // (compositor hit test -> ring -> poll -> this client), for the
                // first few events only so a busy session does not narrate
                // every move.
                let mine = input.iter().filter(|e| e.client() == s.idx).count();
                if mine > 0 {
                    use std::sync::atomic::{AtomicU32, Ordering};
                    static SEEN: AtomicU32 = AtomicU32::new(0);
                    if SEEN.fetch_add(mine as u32, Ordering::Relaxed) < 24 {
                        unsafe {
                            crate::write_stderr(&format!(
                                "[wl] delivered {mine} event(s) to client {}\n",
                                s.idx
                            ));
                        }
                    }
                }
                for ev in input.iter().filter(|e| e.client() == s.idx) {
                    match *ev {
                        InputEvent::Motion { surface, x, y, time, .. } => {
                            s.client.pointer_to(surface, x, y, time)
                        }
                        InputEvent::Button { button, pressed, time, .. } => {
                            s.client.pointer_button(button, pressed, time)
                        }
                        InputEvent::Axis { axis, value, time, .. } => {
                            s.client.pointer_axis(axis, value, time)
                        }
                        InputEvent::Key { key, pressed, time, .. } => {
                            s.client.keyboard_key(key, pressed, time)
                        }
                        InputEvent::Mods { depressed, latched, locked, group, .. } => {
                            s.client.keyboard_modifiers(depressed, latched, locked, group)
                        }
                        InputEvent::PointerClear { .. } => s.client.pointer_clear(),
                        InputEvent::KeyboardFocus { surface, .. } => {
                            s.client.keyboard_focus(surface);
                            if surface != 0 && s.client.is_x11(surface) {
                                if let Some(xwm) = s.xwm.as_mut() {
                                    xwm.focus(surface);
                                }
                            }
                        }
                        InputEvent::Configure { surface, w, h, states, .. } => {
                            // An X window is sized by the X server, through
                            // the manager.
                            let x11 = surface != 0 && s.client.is_x11(surface);
                            match (x11, s.xwm.as_mut()) {
                                (true, Some(xwm)) => {
                                    xwm.resized(surface, w, h);
                                }
                                _ => s.client.configure(surface, w, h, states),
                            }
                        }
                        InputEvent::Close { surface, .. } => {
                            // An X window has no xdg_toplevel to close: the
                            // manager asks the client with WM_DELETE_WINDOW.
                            let x11 = s.client.is_x11(surface);
                            match (x11, s.xwm.as_mut()) {
                                (true, Some(xwm)) => {
                                    xwm.close(surface);
                                }
                                _ => s.client.close(surface),
                            }
                        }
                        InputEvent::DismissPopup { surface, .. } => {
                            s.client.dismiss_popup(surface)
                        }
                        // Where the page put a window: an X window's
                        // position, through the manager; a placed
                        // window's origin, which its client adds to the
                        // pointer positions that follow.
                        InputEvent::Moved { surface, x, y, .. } => {
                            if let Some(xwm) = s.xwm.as_mut() {
                                xwm.moved(surface, x, y);
                            }
                            if !s.client.is_x11(surface) {
                                s.client.placement_origin(surface, x, y);
                            }
                        }
                    }
                }
            }
            // The X window manager, if this client is an X server. Connected
            // lazily: the socket only exists once Xwayland has bound it, which
            // is after its Wayland connection is up.
            // Not every pump: the X socket does not exist until Xwayland has
            // bound it, and asking the global kernel for a name that is not
            // there yet, thousands of times a second, is a lock and a scan
            // each time for nothing.
            s.xwm_tries = s.xwm_tries.wrapping_add(1);
            if s.xwm_tries == 1 {
                // Which program this client is, and so whether this process is
                // the X server whose windows the compositor must manage.
                unsafe {
                    crate::write_stderr(&format!(
                        "[wl] client {} is {} (x server: {})\n",
                        s.idx,
                        kernel.exe_path,
                        is_x_server(kernel)
                    ));
                }
            }
            if s.xwm.is_none() && s.xwm_tries % 64 == 1 && is_x_server(kernel) {
                s.xwm = crate::xwm::connect(kernel);
                if s.xwm.is_none() {
                    // Once, and only from the process that is the X server: a
                    // manager that cannot reach the server is the difference
                    // between windows and an empty output, and it should not be
                    // silent about it.
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static SAID: AtomicBool = AtomicBool::new(false);
                    if !SAID.swap(true, Ordering::Relaxed) {
                        unsafe {
                            crate::write_stderr(
                                "[xwm] the X server's socket is not connectable yet\n",
                            );
                        }
                    }
                }
            }
            if let Some(xwm) = s.xwm.as_mut() {
                xwm.pump(&mut s.client);
                if xwm.failed() {
                    s.xwm = None;
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
                    Ok(n) => {
                        s.sent += n as usize;
                        sent_any = true;
                    }
                }
            }
            if s.sent == s.out.len() {
                s.out.clear();
                s.sent = 0;
            }
            // This client's contribution to the scene. Published every pump it
            // changed anything, which a client that is not drawing does not.
            // Frames are the copy-out path, for a host that cannot read guest
            // memory. This one can, so a frame here means a buffer that could
            // not be placed (one that claimed more than its pool holds) --
            // taken and dropped rather than left to accumulate.
            let refused = s.client.take_frames();
            if !refused.is_empty() && Self::trace_refusals() {
                unsafe {
                    crate::write_stderr(&format!(
                        "[wl] {} buffer(s) could not be placed and were dropped\n",
                        refused.len()
                    ));
                }
            }
            changed |= s.client.take_dirty();
            for info in s.client.scene() {
                if scene.len() / REC >= MAX_SURFACES {
                    break;
                }
                push_record(&mut scene, s.idx, &info);
                s.client.clear_damage(info.surface);
            }
            // A client that is gone, or one this compositor threw out. The
            // second is worth saying out loud: a dropped session takes its X
            // window manager with it, and then every X client waits forever
            // for a MapRequest nobody will answer.
            if let Some(why) = s.client.failed.clone() {
                unsafe {
                    crate::write_stderr(&format!(
                        "[wl] client {} was dropped: {why}\n",
                        s.idx
                    ));
                }
            }
            if gone {
                unsafe {
                    crate::write_stderr(&format!("[wl] client {} disconnected\n", s.idx));
                }
            }
            !gone && s.client.failed.is_none()
        });
        // A client that went away changes the scene by its absence.
        let live = sessions.len();
        let mut last = self.inner.published.lock().unwrap();
        changed |= *last != live;
        *last = live;
        drop(sessions);
        if changed {
            let count = (scene.len() / REC) as u32;
            unsafe { wl_publish(scene.as_ptr() as u32, count) };
        }
        sent_any
    }

    /// Whether a pump now would do anything: a client socket with bytes or a
    /// hang-up to read, bytes still owed to one, an X window manager to
    /// drive, or input in the ring for a client that takes it -- or the floor
    /// (`PUMP_FLOOR_NS`) has passed. Asked on every scheduler turn of every
    /// Worker, under the lock as the pump was: Undertale's game process pumped 7,000
    /// times a second, nearly all with nothing to move, and the pump was 311
    /// of the 546 ms a 10 s window held the lock for the turn (census,
    /// worldmachine) -- the turn that is 70% of the game's lock wait.
    pub fn needs_pump(&self) -> bool {
        let now = crate::now_ns();
        if now.saturating_sub(self.inner.last_pump.load(std::sync::atomic::Ordering::Relaxed)) >= PUMP_FLOOR_NS {
            return true;
        }
        // Held elsewhere (an accept, a GL attach): pump, as before.
        let Ok(sessions) = self.inner.sessions.try_lock() else {
            return true;
        };
        let mut wants = false;
        for s in sessions.iter() {
            if s.xwm.is_some() || s.sent < s.out.len() || s.socket.poll_readable() || s.socket.poll_hup() {
                return true;
            }
            wants |= s.client.wants_input();
        }
        // SAFETY: a host import taking nothing.
        wants && unsafe { wl_input_pending() } != 0
    }

    /// Write every client's surfaces into the scene arena.
    ///
    /// The tail of `pump`, callable on its own for a change that arrives from
    /// somewhere other than the Wayland socket -- a GL client naming the
    /// surface the host draws for it.
    pub fn publish_scene(&self) {
        let mut scene: Vec<i32> = Vec::new();
        {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for s in sessions.iter_mut() {
                for info in s.client.scene() {
                    if scene.len() / REC >= MAX_SURFACES {
                        break;
                    }
                    push_record(&mut scene, s.idx, &info);
                }
            }
        }
        let count = (scene.len() / REC) as u32;
        unsafe { wl_publish(scene.as_ptr() as u32, count) };
    }

    pub fn client_count(&self) -> usize {
        self.inner.sessions.lock().unwrap().len()
    }

    /// The output size the compositor is drawing at, which is what a toplevel
    /// is configured to and what `wl_output` reports.
    pub fn set_output(&self, w: i32, h: i32) {
        *self.inner.size.lock().unwrap() = (w.max(1), h.max(1));
    }
}

impl WaylandDevice {
    /// Say once that a buffer was refused, and not again: a client that does it
    /// does it every frame.
    fn trace_refusals() -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAID: AtomicBool = AtomicBool::new(false);
        !SAID.swap(true, Ordering::Relaxed)
    }
}

/// Whether this process is the X server, which is how the compositor knows to
/// be its window manager as well.
///
/// sway knows because it spawns Xwayland itself; here the guest's session
/// starts it, so the program's own name is what says so. The manager has to run
/// in *this* process: the pairing between an X window and a `wl_surface` names
/// a surface on this process's Wayland connection.
fn is_x_server(kernel: &Kernel) -> bool {
    if !kernel.exe_path.contains("Xwayland") {
        return false;
    }
    // And only a *rootless* one. Rootless Xwayland hands each X window over as
    // its own surface and needs a window manager to do it; a rootful one
    // presents the whole X screen as one surface and manages its own windows
    // inside it -- and a manager that redirects them with Composite there sends
    // every window offscreen, leaving a black root. Which is exactly what
    // happened to the single-app card until this test was here.
    kernel.argv.iter().any(|a| a == "-rootless")
}

/// One surface's record, in the layout `wl-bridge.mjs` reads.
fn push_record(out: &mut Vec<i32>, client: u32, info: &SurfaceInfo) {
    let base = out.len();
    out.resize(base + REC, 0);
    let r = &mut out[base..base + REC];
    r[0] = client as i32;
    r[1] = info.surface as i32;
    r[2] = info.role.code() as i32;
    r[3] = info.parent as i32;
    r[4] = info.rel_x;
    r[5] = info.rel_y;
    r[6] = info.width;
    r[7] = info.height;
    r[8] = info.addr as i32;
    r[9] = info.stride;
    r[10] = info.format as i32;
    r[11] = info.serial as i32;
    r[12] = info.damage.x;
    r[13] = info.damage.y;
    r[14] = info.damage.w;
    r[15] = info.damage.h;
    r[16] = info.flags();
    // Which X window this is, for a host that knows a drawable and not a
    // surface -- an OpenGL client naming what it draws into.
    r[17] = info.xid as i32;
    // The title as UTF-8 in the record's tail, truncated on a character
    // boundary so a multi-byte title cannot be cut into invalid bytes.
    let mut n = info.title.len().min(TITLE_BYTES);
    while n > 0 && !info.title.is_char_boundary(n) {
        n -= 1;
    }
    let bytes = &info.title.as_bytes()[..n];
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        r[18 + i] = i32::from_le_bytes(word);
    }
    // Where its client placed it, if it did (`gex_window_placement_v1`).
    r[28..33].copy_from_slice(&info.placement.record_words());
    // Where it takes the pointer, when its client said (`flags` bit 7).
    r[33..37].copy_from_slice(&info.input_words());
    // Its glass, if its client asked for some (`gex_glass_v1`).
    r[37..72].copy_from_slice(&info.glass.record_words());
}

impl HostListener for WaylandDevice {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut client = Client::new();
        let (w, h) = output_size();
        client.set_default_size(w, h);
        client.set_scale(OUTPUT_SCALE.load(std::sync::atomic::Ordering::Relaxed));
        let idx = {
            let mut n = self.inner.next_idx.lock().unwrap();
            *n += 1;
            *n
        };
        unsafe {
            crate::write_stderr(&format!(
                "[wl] client {idx} connected to this process's compositor\n"
            ));
        }
        self.inner.sessions.lock().unwrap().push(Session {
            socket: peer,
            client,
            xwm_tries: 0,
            xwm: None,
            idx,
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
    fn locate(&self, fd: u32) -> Option<(u32, u32)> {
        let file = self.fds.get(fd as usize)?;
        // The client mapped its pool MAP_SHARED, so the mapping *is* the
        // pool: its address in guest memory is where the pixels are, live,
        // and the compositor reads them from there. This is why a frame is
        // never copied on the way to the screen.
        let map = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))?;
        Some((u32::try_from(map.start).ok()?, u32::try_from(map.len).ok()?))
    }

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
