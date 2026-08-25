//! A Wayland compositor, as a state machine over the wire protocol.
//!
//! The guest has no display hardware, so the display server lives in the host.
//! A guest client connects to `$XDG_RUNTIME_DIR/wayland-0`, the kernel routes
//! that unix socket here, and out comes a list of surfaces with pixels the host
//! can draw however it likes. This crate knows nothing about drawing: it takes
//! bytes and descriptors in, produces bytes out, and reports commits as
//! [`Frame`]s naming a buffer, which the host resolves through [`Buffers`].
//!
//! # What is here
//!
//! Enough for a real client to map a window and show pixels: `wl_display`,
//! `wl_registry`, `wl_callback`, `wl_compositor`, `wl_shm` with pools and
//! buffers, `wl_surface`, `wl_region`, `wl_seat` (keyboard and pointer),
//! `wl_output`, and the `xdg_shell` trio. Shared memory only: no `linux-dmabuf`
//! or `wl_drm`, so a client insisting on a GPU buffer finds none.

pub mod wire;

use std::collections::HashMap;
use wire::{header, Fixed, Reader, Writer};

/// A US-layout XKB keymap in the `include` form `xkb_keymap_new_from_string`
/// resolves against the client's own `/usr/share/X11/xkb` (which the graphics
/// rootfs carries). Sent once as the `wl_keyboard.keymap` payload; the host
/// attaches it as a readable descriptor the client mmaps and compiles. A flat,
/// fully-resolved keymap would not depend on the client's config at all, but it
/// is ~30 KB of text where this is six lines and every client here has the data.
const KEYMAP: &str = "xkb_keymap {\n\
\txkb_keycodes  { include \"evdev+aliases(qwerty)\" };\n\
\txkb_types     { include \"complete\" };\n\
\txkb_compat    { include \"complete\" };\n\
\txkb_symbols   { include \"pc+us\" };\n\
\txkb_geometry  { include \"pc(pc105)\" };\n\
};\n";

/// Where a buffer's pixels are.
///
/// A `wl_shm_pool` is made from a descriptor the client sent; in gex its memory
/// is inside the guest's address space, so this hands back a slice of it.
pub trait Buffers {
    /// The bytes of the memory behind a descriptor, if it has any.
    fn map(&self, fd: u32, size: usize) -> Option<Vec<u8>>;
}

/// A committed surface: what to draw, and where its pixels are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub surface: u32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    /// A `wl_shm` format code. 0 is ARGB8888, 1 is XRGB8888 -- the two
    /// mandatory formats, and the only ones advertised here.
    pub format: u32,
    /// The pixels, already copied out of the client's pool.
    pub pixels: Vec<u8>,
    /// The window title, when the surface has an `xdg_toplevel` with one.
    pub title: String,
}

/// Interfaces the registry advertises, with the version offered. A toolkit
/// binds at the highest shared version and uses requests only that version has,
/// so offering one whose requests are unimplemented is worse than a lower one:
/// these are deliberately conservative.
/// Runtime switch for wl tracing (the wasm shim has no env, so `GEX_WL_TRACE`
/// never fires there; the host sets this via `gex_set_wl_trace`).
pub static WL_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn wl protocol tracing on/off at runtime.
pub fn set_trace(on: bool) {
    WL_TRACE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Where trace lines go. In the wasm shim `eprintln!`/stderr is a no-op (no
/// wasi), so the host installs a sink that writes to `console.error`. Stored as
/// a `fn(&str)` pointer in a usize (0 = none, fall back to `eprintln!`).
pub static TRACE_SINK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Install the sink the shim points at its console writer.
pub fn set_trace_sink(f: fn(&str)) {
    TRACE_SINK.store(f as usize, std::sync::atomic::Ordering::Relaxed);
}

fn trace_out(s: &str) {
    let p = TRACE_SINK.load(std::sync::atomic::Ordering::Relaxed);
    if p != 0 {
        // SAFETY: only ever set by `set_trace_sink` to a valid `fn(&str)`.
        let f: fn(&str) = unsafe { std::mem::transmute::<usize, fn(&str)>(p) };
        f(s);
    } else {
        eprintln!("{s}");
    }
}

const GLOBALS: &[(&str, u32)] = &[
    ("wl_compositor", 4),
    ("wl_shm", 1),
    ("wl_seat", 5),
    ("wl_output", 3),
    ("xdg_wm_base", 3),
    // GTK expects these during display init; without them it defers/abandons
    // seat setup (never binds wl_seat) and loops on GDK_IS_SEAT. Minimal: their
    // requests are accepted and ignored (no real clipboard/DnD or subsurfaces).
    ("wl_data_device_manager", 3),
    ("wl_subcompositor", 1),
];

/// What an object id currently is.
#[derive(Clone, Debug)]
enum Object {
    Display,
    Registry,
    Callback,
    Compositor,
    Shm,
    ShmPool { fd: u32, size: usize },
    Buffer(BufferSpec),
    Surface,
    Region,
    Seat,
    Keyboard,
    Pointer,
    Output,
    WmBase,
    XdgSurface { surface: u32 },
    XdgToplevel { xdg_surface: u32 },
    Positioner,
    DataDeviceManager,
    Subcompositor,
    /// An object whose requests we accept and ignore (data devices/sources,
    /// subsurfaces) -- present so GTK's init completes, does nothing observable.
    Stub,
}

#[derive(Clone, Debug)]
struct BufferSpec {
    /// The pool's descriptor and size, held here rather than looked up through
    /// the pool: destroying a `wl_shm_pool` does not destroy its buffers, and
    /// Xwayland destroys the pool immediately after making one buffer.
    fd: u32,
    pool_size: usize,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
}

/// One surface's pending and current state: everything a client says takes
/// effect at `commit`, per Wayland's double-buffered model.
#[derive(Clone, Debug, Default)]
struct SurfaceState {
    pending_buffer: Option<u32>,
    committed_buffer: Option<u32>,
    /// Frame callbacks, delivered after the commit they belong to. A client
    /// that does not get these back stops drawing.
    pending_callbacks: Vec<u32>,
    title: String,
    /// The `xdg_surface` that gave this surface a role, if any.
    xdg: Option<u32>,
    /// Set once the client has acknowledged a configure, which is when a
    /// toplevel is allowed to show anything.
    configured: bool,
}

/// One connected client.
pub struct Client {
    objects: HashMap<u32, Object>,
    surfaces: HashMap<u32, SurfaceState>,
    /// Bytes received and not yet forming a whole message.
    inbox: Vec<u8>,
    /// Descriptors received and not yet claimed by an `fd` argument.
    fds: Vec<u32>,
    /// Bytes to send back.
    outbox: Vec<u8>,
    /// Commits since the host last looked.
    frames: Vec<Frame>,
    /// Event serials, handed out with sync/configure/frame callbacks, counting
    /// up.
    next_serial: u32,
    /// The size a toplevel is told it is.
    default_size: (i32, i32),
    /// Set when the protocol has been violated and the connection is over.
    pub failed: Option<String>,
    /// The `wl_pointer` and `wl_keyboard` object ids once the client asks for
    /// them, so injected input has somewhere to go.
    pointer: Option<u32>,
    keyboard: Option<u32>,
    /// The surface input is delivered to: the last one to commit a buffer, i.e.
    /// the visible window.
    focus_surface: Option<u32>,
    /// Which surface `enter` was last sent for, per device (`None` == not
    /// currently over/focused), so motion and keys only follow an enter.
    pointer_on: Option<u32>,
    keyboard_on: Option<u32>,
    /// A keyboard was created and owes a `keymap` event whose fd the host must
    /// attach: this is the payload, taken once by [`Client::take_keymap`].
    pending_keymap: Option<Vec<u8>>,
}

/// Wayland's error codes on `wl_display`.
mod display_error {
    pub const INVALID_OBJECT: u32 = 0;
    pub const INVALID_METHOD: u32 = 1;
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Client {
        let mut objects = HashMap::new();
        // Object 1 is the display, and it is the only id a client may assume.
        objects.insert(1, Object::Display);
        Client {
            objects,
            surfaces: HashMap::new(),
            inbox: Vec::new(),
            fds: Vec::new(),
            outbox: Vec::new(),
            frames: Vec::new(),
            next_serial: 1,
            default_size: (640, 480),
            failed: None,
            pointer: None,
            keyboard: None,
            focus_surface: None,
            pointer_on: None,
            keyboard_on: None,
            pending_keymap: None,
        }
    }

    /// The size a toplevel is told to be, fixed and configurable so a test can
    /// ask for a particular window.
    pub fn set_default_size(&mut self, w: i32, h: i32) {
        self.default_size = (w, h);
    }

    /// Feed bytes and descriptors that arrived together.
    pub fn receive(&mut self, bytes: &[u8], fds: &[u32], buffers: &dyn Buffers) {
        self.inbox.extend_from_slice(bytes);
        self.fds.extend_from_slice(fds);
        while self.failed.is_none() {
            let Some(h) = header(&self.inbox) else { break };
            let size = h.size as usize;
            let body: Vec<u8> = self.inbox[8..size].to_vec();
            self.inbox.drain(..size);
            self.dispatch(h.object, h.opcode, &body, buffers);
        }
    }

    /// Bytes the host should send to the client.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outbox)
    }

    /// Surfaces committed since this was last called.
    pub fn take_frames(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.frames)
    }

    fn serial(&mut self) -> u32 {
        self.next_serial += 1;
        self.next_serial
    }

    fn send(&mut self, w: Writer) {
        self.outbox.extend_from_slice(&w.finish());
    }

    fn error(&mut self, object: u32, code: u32, message: &str) {
        let w = Writer::new(1, 0).object(object).uint(code).string(message);
        self.send(w);
        self.failed = Some(message.to_string());
    }

    /// `wl_display.delete_id`, which a client needs before it will reuse an
    /// id. libwayland leaks ids without it.
    fn delete_id(&mut self, id: u32) {
        let w = Writer::new(1, 1).uint(id);
        self.send(w);
    }

    /// Whether to report every request and every refusal. A stalled client has
    /// usually asked for something it did not get; this is how Xwayland's
    /// requirements were found.
    fn tracing() -> bool {
        std::env::var_os("GEX_WL_TRACE").is_some()
            || WL_TRACE.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn dispatch(&mut self, object: u32, opcode: u16, body: &[u8], buffers: &dyn Buffers) {
        if Self::tracing() {
            trace_out(&format!(
                "[wl] -> object {object} opcode {opcode} ({} bytes) {}",
                body.len(),
                match self.objects.get(&object) {
                    Some(o) => format!("{o:?}"),
                    None => "UNKNOWN".into(),
                }
            ));
        }
        let Some(target) = self.objects.get(&object).cloned() else {
            self.error(
                object,
                display_error::INVALID_OBJECT,
                &format!("no object {object}"),
            );
            return;
        };
        let mut fds = std::mem::take(&mut self.fds);
        let ok = {
            let mut r = Reader::new(body, &mut fds);
            match target {
                Object::Display => self.display(object, opcode, &mut r),
                Object::Registry => self.registry(opcode, &mut r),
                Object::Compositor => self.compositor(opcode, &mut r),
                Object::Shm => self.shm(opcode, &mut r),
                Object::ShmPool { fd, size } => self.shm_pool(object, fd, size, opcode, &mut r),
                Object::Surface => self.surface(object, opcode, &mut r, buffers),
                Object::WmBase => self.wm_base(opcode, &mut r),
                Object::XdgSurface { surface } => self.xdg_surface(object, surface, opcode, &mut r),
                Object::XdgToplevel { xdg_surface } => {
                    self.xdg_toplevel(object, xdg_surface, opcode, &mut r)
                }
                Object::Seat => self.seat(opcode, &mut r),
                Object::DataDeviceManager => {
                    // create_data_source(id) = 0, get_data_device(id, seat) = 1;
                    // both make a new object whose requests we then ignore.
                    match opcode {
                        0 | 1 => {
                            if let Some(id) = r.new_id() {
                                self.objects.insert(id, Object::Stub);
                            }
                            true
                        }
                        _ => true,
                    }
                }
                Object::Subcompositor => {
                    // destroy = 0; get_subsurface(id, surface, parent) = 1.
                    match opcode {
                        1 => {
                            if let Some(id) = r.new_id() {
                                self.objects.insert(id, Object::Stub);
                            }
                            true
                        }
                        _ => true,
                    }
                }
                // Data devices/sources and subsurfaces: none of their requests
                // creates a child object, so just accept and ignore everything.
                Object::Stub => true,
                Object::Buffer(_) | Object::Region | Object::Positioner => {
                    // Every one of these has `destroy` at opcode 0 and
                    // requests that change nothing observable here.
                    if opcode == 0 {
                        self.objects.remove(&object);
                        self.delete_id(object);
                    }
                    true
                }
                Object::Pointer => {
                    // wl_pointer requests are set_cursor(0) and release(1) -- NOT
                    // destroy-at-0 like most objects. A client points its cursor
                    // the instant the pointer enters, and treating set_cursor as
                    // a destroy takes the pointer object out from under the next
                    // event ("no object N", then a fatal). We draw no client
                    // cursor (the page's own shows), so set_cursor is accepted
                    // and ignored; release destroys.
                    match opcode {
                        0 => true,
                        1 => {
                            self.objects.remove(&object);
                            if self.pointer == Some(object) {
                                self.pointer = None;
                                self.pointer_on = None;
                            }
                            self.delete_id(object);
                            true
                        }
                        _ => false,
                    }
                }
                Object::Keyboard => {
                    // wl_keyboard's only request is release(0).
                    if opcode == 0 {
                        self.objects.remove(&object);
                        if self.keyboard == Some(object) {
                            self.keyboard = None;
                            self.keyboard_on = None;
                        }
                        self.delete_id(object);
                    }
                    true
                }
                Object::Output | Object::Callback => {
                    // `release`/`destroy` and nothing else.
                    if opcode == 0 {
                        self.objects.remove(&object);
                        self.delete_id(object);
                    }
                    true
                }
            }
        };
        self.fds = fds;
        if !ok && self.failed.is_none() {
            if Self::tracing() {
                trace_out(&format!("[wl] REFUSED object {object} opcode {opcode}"));
            }
            self.error(
                object,
                display_error::INVALID_METHOD,
                &format!("bad request {opcode} on object {object}"),
            );
        }
    }

    // -- wl_display ---------------------------------------------------------

    fn display(&mut self, _object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // sync(callback): answer at once; a client uses it as a round-trip
            // barrier.
            0 => {
                let Some(id) = r.new_id() else { return false };
                let serial = self.serial();
                let w = Writer::new(id, 0).uint(serial);
                self.send(w);
                self.delete_id(id);
                true
            }
            // get_registry(registry)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Registry);
                for (name, (interface, version)) in GLOBALS.iter().enumerate() {
                    if Self::tracing() {
                        trace_out(&format!(
                            "[wl] advertise global {} = {interface} v{version}",
                            name as u32 + 1
                        ));
                    }
                    let w = Writer::new(id, 0)
                        .uint(name as u32 + 1)
                        .string(interface)
                        .uint(*version);
                    self.send(w);
                }
                true
            }
            _ => false,
        }
    }

    // -- wl_registry --------------------------------------------------------

    fn registry(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        if opcode != 0 {
            return false;
        }
        let (Some(name), Some(interface), Some(_version), Some(id)) =
            (r.uint(), r.string(), r.uint(), r.new_id())
        else {
            return false;
        };
        if Self::tracing() {
            trace_out(&format!("[wl] bind {interface} v{_version} as {id}"));
        }
        let Some((expected, _)) = GLOBALS.get(name as usize - 1) else {
            return false;
        };
        if *expected != interface {
            return false;
        }
        match interface.as_str() {
            "wl_compositor" => {
                self.objects.insert(id, Object::Compositor);
            }
            "wl_shm" => {
                self.objects.insert(id, Object::Shm);
                // The two mandatory formats: ARGB8888 and XRGB8888.
                for fmt in [0u32, 1] {
                    let w = Writer::new(id, 0).uint(fmt);
                    self.send(w);
                }
            }
            "wl_seat" => {
                self.objects.insert(id, Object::Seat);
                // Pointer (bit 0) and keyboard (bit 1). The keyboard's `keymap`
                // event needs a real fd for its middle `h` argument, which the
                // host attaches from the socket's ancillary channel (see
                // `take_keymap`); with that in place both capabilities are real.
                let w = Writer::new(id, 0).uint(0b11);
                self.send(w);
                let w = Writer::new(id, 1).string("gex-seat0");
                self.send(w);
            }
            "wl_output" => {
                self.objects.insert(id, Object::Output);
                // Keep the advertised output in sync with the size used for
                // xdg_toplevel configure events.  Xwayland sizes its root
                // framebuffer from wl_output, so a fixed 1920x1080 mode made
                // every commit copy 8 MiB even when the compositor and page
                // were only presenting a 640x480 surface.
                let (w_px, h_px) = self.default_size;
                let ev = Writer::new(id, 0)
                    .int(0)
                    .int(0)
                    .int(510)
                    .int(287)
                    .int(0)
                    .string("gex")
                    .string("virtual")
                    .int(0);
                self.send(ev);
                // mode(flags=current|preferred, width, height, refresh mHz)
                let ev = Writer::new(id, 1).uint(0b11).int(w_px).int(h_px).int(60000);
                self.send(ev);
                let ev = Writer::new(id, 3).int(1);
                self.send(ev);
                let ev = Writer::new(id, 2);
                self.send(ev);
            }
            "xdg_wm_base" => {
                self.objects.insert(id, Object::WmBase);
            }
            "wl_data_device_manager" => {
                self.objects.insert(id, Object::DataDeviceManager);
            }
            "wl_subcompositor" => {
                self.objects.insert(id, Object::Subcompositor);
            }
            _ => return false,
        }
        true
    }

    // -- wl_compositor ------------------------------------------------------

    fn compositor(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // create_surface(id)
            0 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Surface);
                self.surfaces.insert(id, SurfaceState::default());
                true
            }
            // create_region(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Region);
                true
            }
            _ => false,
        }
    }

    // -- wl_shm -------------------------------------------------------------

    fn shm(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        if opcode != 0 {
            return false;
        }
        // create_pool(id, fd, size)
        let (Some(id), Some(fd), Some(size)) = (r.new_id(), r.fd(), r.int()) else {
            return false;
        };
        if size <= 0 {
            return false;
        }
        self.objects.insert(
            id,
            Object::ShmPool {
                fd,
                size: size as usize,
            },
        );
        true
    }

    fn shm_pool(
        &mut self,
        object: u32,
        fd: u32,
        size: usize,
        opcode: u16,
        r: &mut Reader<'_>,
    ) -> bool {
        match opcode {
            // create_buffer(id, offset, width, height, stride, format)
            0 => {
                let (Some(id), Some(offset), Some(width), Some(height), Some(stride), Some(format)) =
                    (r.new_id(), r.int(), r.int(), r.int(), r.int(), r.uint())
                else {
                    return false;
                };
                let _ = object;
                self.objects.insert(
                    id,
                    Object::Buffer(BufferSpec {
                        fd,
                        pool_size: size,
                        offset,
                        width,
                        height,
                        stride,
                        format,
                    }),
                );
                true
            }
            // destroy
            1 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // resize(size)
            2 => {
                let Some(new_size) = r.int() else { return false };
                if new_size as usize > size {
                    self.objects.insert(
                        object,
                        Object::ShmPool {
                            fd,
                            size: new_size as usize,
                        },
                    );
                }
                true
            }
            _ => false,
        }
    }

    // -- wl_surface ---------------------------------------------------------

    fn surface(
        &mut self,
        object: u32,
        opcode: u16,
        r: &mut Reader<'_>,
        buffers: &dyn Buffers,
    ) -> bool {
        match opcode {
            // destroy
            0 => {
                self.objects.remove(&object);
                self.surfaces.remove(&object);
                self.delete_id(object);
                true
            }
            // attach(buffer, x, y)
            1 => {
                let Some(buffer) = r.object() else {
                    return false;
                };
                let (_x, _y) = (r.int(), r.int());
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_buffer = if buffer == 0 { None } else { Some(buffer) };
                }
                true
            }
            // damage(x, y, width, height): irrelevant to a compositor that
            // always redraws.
            2 => true,
            // frame(callback)
            3 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Callback);
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_callbacks.push(id);
                }
                true
            }
            // set_opaque_region, set_input_region
            4 | 5 => true,
            // commit
            6 => {
                self.commit(object, buffers);
                true
            }
            // set_buffer_transform, set_buffer_scale, damage_buffer, offset
            7 | 8 | 9 | 10 => true,
            _ => false,
        }
    }

    /// Apply a surface's pending state, the only moment anything becomes visible.
    fn commit(&mut self, surface: u32, buffers: &dyn Buffers) {
        let Some(state) = self.surfaces.get(&surface).cloned() else {
            return;
        };
        let buffer_id = state.pending_buffer.or(state.committed_buffer);
        // The initial, bufferless commit of a surface that has an xdg role: the
        // xdg_shell contract is that the compositor answers THIS commit with an
        // `xdg_surface.configure`, and libwayland (es2gears) will not attach its
        // first buffer until one arrives after its own initial commit. Sending
        // it eagerly at `get_toplevel` is too early -- the client is still
        // waiting for the post-commit one and parks forever. Idempotent: a
        // client acks the latest serial, so an extra configure is harmless.
        if buffer_id.is_none() && !state.configured {
            if let Some(xdg) = state.xdg {
                let serial = self.serial();
                let w = Writer::new(xdg, 0).uint(serial);
                self.send(w);
            }
        }
        let mut produced = None;
        if let Some(buffer_id) = buffer_id {
            if let Some(Object::Buffer(spec)) = self.objects.get(&buffer_id).cloned() {
                let mapped = buffers.map(spec.fd, spec.pool_size);
                if Self::tracing() {
                    trace_out(&format!(
                        "[wl] buffer {buffer_id}: fd {} pool {} -> {:?} bytes, {}x{} stride {}",
                        spec.fd,
                        spec.pool_size,
                        mapped.as_ref().map(|p| p.len()),
                        spec.width,
                        spec.height,
                        spec.stride
                    ));
                }
                if let Some(pool) = mapped {
                    produced = Self::cut(&pool, &spec).map(|pixels| Frame {
                        surface,
                        width: spec.width,
                        height: spec.height,
                        stride: spec.stride,
                        format: spec.format,
                        pixels,
                        title: state.title.clone(),
                    });
                }
            }
            // A client may not draw into a buffer again until it is released,
            // and most will not draw a second frame without this.
            let w = Writer::new(buffer_id, 0);
            self.send(w);
        }
        match produced {
            Some(frame) => {
                // The visible window, and so where injected input is delivered.
                self.focus_surface = Some(surface);
                self.frames.push(frame);
            }
            None if buffer_id.is_some() && Self::tracing() => {
                trace_out(&format!("[wl] commit on surface {surface} produced no frame"));
            }
            None => {}
        }
        // Frame callbacks belong to the commit that carried them. The argument
        // is meant to be a monotonic millisecond timestamp, used only for
        // deltas; a serial counts up the same way.
        let serial = self.serial();
        for cb in &state.pending_callbacks {
            let w = Writer::new(*cb, 0).uint(serial);
            self.send(w);
            let id = *cb;
            self.objects.remove(&id);
            self.delete_id(id);
        }
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.committed_buffer = buffer_id;
            s.pending_buffer = None;
            s.pending_callbacks.clear();
        }
    }

    /// Copy a buffer's rectangle out of its pool. `None` when the pool does not
    /// hold what the client claimed: reading past the end lets a client crash
    /// the compositor.
    fn cut(pool: &[u8], spec: &BufferSpec) -> Option<Vec<u8>> {
        if spec.width <= 0 || spec.height <= 0 || spec.stride < spec.width.checked_mul(4)? {
            return None;
        }
        let offset = usize::try_from(spec.offset).ok()?;
        let stride = usize::try_from(spec.stride).ok()?;
        let height = usize::try_from(spec.height).ok()?;
        let need = stride.checked_mul(height)?;
        let end = offset.checked_add(need)?;
        if end > pool.len() {
            return None;
        }
        Some(pool[offset..end].to_vec())
    }

    // -- xdg_shell ----------------------------------------------------------

    fn wm_base(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => true,
            // create_positioner(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Positioner);
                true
            }
            // get_xdg_surface(id, surface)
            2 => {
                let (Some(id), Some(surface)) = (r.new_id(), r.object()) else {
                    return false;
                };
                if !matches!(self.objects.get(&surface), Some(Object::Surface)) {
                    return false;
                }
                self.objects.insert(id, Object::XdgSurface { surface });
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.xdg = Some(id);
                }
                true
            }
            // pong(serial)
            3 => true,
            _ => false,
        }
    }

    fn xdg_surface(
        &mut self,
        object: u32,
        surface: u32,
        opcode: u16,
        r: &mut Reader<'_>,
    ) -> bool {
        match opcode {
            // destroy
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // get_toplevel(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(
                    id,
                    Object::XdgToplevel {
                        xdg_surface: object,
                    },
                );
                // A toplevel may not draw until configured, so the size goes out
                // at once. A real size (not 0x0, "you choose") is what a tiling
                // compositor sends.
                let (w, h) = self.default_size;
                let ev = Writer::new(id, 0).int(w).int(h).array(&[]);
                self.send(ev);
                let serial = self.serial();
                let ev = Writer::new(object, 0).uint(serial);
                self.send(ev);
                let _ = surface;
                true
            }
            // get_popup(id, parent, positioner)
            2 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Positioner);
                true
            }
            // set_window_geometry(x, y, width, height)
            3 => true,
            // ack_configure(serial)
            4 => {
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.configured = true;
                }
                true
            }
            _ => false,
        }
    }

    fn xdg_toplevel(
        &mut self,
        object: u32,
        xdg_surface: u32,
        opcode: u16,
        r: &mut Reader<'_>,
    ) -> bool {
        match opcode {
            // destroy
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_parent(parent)
            1 => true,
            // set_title(title)
            2 => {
                let Some(title) = r.string() else { return false };
                let surface = match self.objects.get(&xdg_surface) {
                    Some(Object::XdgSurface { surface }) => *surface,
                    _ => return false,
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.title = title;
                }
                true
            }
            // set_app_id, show_window_menu, move, resize, set_max_size,
            // set_min_size, set_maximized, unset_maximized, set_fullscreen,
            // unset_fullscreen, set_minimized -- all layout, none of which
            // changes what the host is handed.
            3..=13 => true,
            _ => false,
        }
    }

    // -- wl_seat ------------------------------------------------------------

    fn seat(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // get_pointer(id)
            0 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Pointer);
                self.pointer = Some(id);
                true
            }
            // get_keyboard(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Keyboard);
                self.keyboard = Some(id);
                // keymap(format = XKB_V1 (1), fd, size). The `h` fd argument
                // occupies no word in the body -- it rides the socket's ancillary
                // control message -- so the body is just format and size, and the
                // payload the host must attach is recorded for `take_keymap`. The
                // size counts the NUL the client's `mmap`+compile expects.
                let mut km = KEYMAP.as_bytes().to_vec();
                km.push(0);
                let w = Writer::new(id, 0).uint(1).uint(km.len() as u32);
                self.send(w);
                self.pending_keymap = Some(km);
                // repeat_info(rate keys/s, delay ms) for v4+, so the client does
                // not have to assume a repeat rate.
                let w = Writer::new(id, 5).int(25).int(600);
                self.send(w);
                true
            }
            // get_touch(id)
            2 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Pointer);
                true
            }
            // release
            3 => true,
            _ => false,
        }
    }

    // -- injected input -----------------------------------------------------
    //
    // The host turns a page's mouse and keyboard events into these calls; they
    // append `wl_pointer`/`wl_keyboard` events to the outbox, delivered on the
    // next pump. Every one is a no-op until the client has asked for the device
    // (so events before `get_pointer`/`get_keyboard` are dropped, not queued)
    // and a surface is visible to enter.

    /// The keymap payload the host must hand the client as an SCM_RIGHTS fd,
    /// taken once. `Some` exactly after a `get_keyboard`.
    pub fn take_keymap(&mut self) -> Option<Vec<u8>> {
        self.pending_keymap.take()
    }

    /// Whether the client has bound a pointer or keyboard: no point delivering
    /// input to one that has not.
    pub fn wants_input(&self) -> bool {
        self.pointer.is_some() || self.keyboard.is_some()
    }

    /// `wl_pointer.enter` the focus surface if the pointer is not already on it,
    /// leaving the previous one first. Returns whether the pointer is now on a
    /// surface.
    fn ensure_pointer_enter(&mut self) -> bool {
        let (Some(ptr), Some(surface)) = (self.pointer, self.focus_surface) else {
            return false;
        };
        if self.pointer_on == Some(surface) {
            return true;
        }
        if let Some(old) = self.pointer_on.take() {
            let serial = self.serial();
            self.send(Writer::new(ptr, 1).uint(serial).object(old)); // leave
        }
        let serial = self.serial();
        // enter(serial, surface, surface_x, surface_y)
        self.send(
            Writer::new(ptr, 0)
                .uint(serial)
                .object(surface)
                .fixed(Fixed::from_int(0))
                .fixed(Fixed::from_int(0)),
        );
        self.pointer_on = Some(surface);
        true
    }

    /// `wl_pointer.frame` (v5): closes an event group. Harmless one-per-event.
    fn pointer_frame(&mut self) {
        if let Some(ptr) = self.pointer {
            self.send(Writer::new(ptr, 5));
        }
    }

    /// Pointer moved to surface-local `(x, y)` in pixels.
    pub fn pointer_motion(&mut self, x: i32, y: i32, time: u32) {
        let Some(ptr) = self.pointer else { return };
        if !self.ensure_pointer_enter() {
            return;
        }
        // motion(time, surface_x, surface_y)
        self.send(
            Writer::new(ptr, 2)
                .uint(time)
                .fixed(Fixed::from_int(x))
                .fixed(Fixed::from_int(y)),
        );
        self.pointer_frame();
    }

    /// A pointer button (Linux `BTN_*` code) went down or up.
    pub fn pointer_button(&mut self, button: u32, pressed: bool, time: u32) {
        let Some(ptr) = self.pointer else { return };
        if !self.ensure_pointer_enter() {
            return;
        }
        let serial = self.serial();
        // button(serial, time, button, state): 1 pressed, 0 released.
        self.send(
            Writer::new(ptr, 3)
                .uint(serial)
                .uint(time)
                .uint(button)
                .uint(pressed as u32),
        );
        self.pointer_frame();
    }

    /// Scroll. `axis` 0 is vertical, 1 horizontal; `value` is a pixel-ish delta.
    pub fn pointer_axis(&mut self, axis: u32, value: i32, time: u32) {
        let Some(ptr) = self.pointer else { return };
        if self.pointer_on.is_none() {
            return;
        }
        // axis(time, axis, value)
        self.send(
            Writer::new(ptr, 4)
                .uint(time)
                .uint(axis)
                .fixed(Fixed::from_int(value)),
        );
        self.pointer_frame();
    }

    /// `wl_keyboard.enter` the focus surface if not already focused, priming the
    /// client's xkb state with an empty modifiers event. Returns whether focused.
    fn ensure_keyboard_enter(&mut self) -> bool {
        let (Some(kbd), Some(surface)) = (self.keyboard, self.focus_surface) else {
            return false;
        };
        if self.keyboard_on == Some(surface) {
            return true;
        }
        if let Some(old) = self.keyboard_on.take() {
            let serial = self.serial();
            self.send(Writer::new(kbd, 2).uint(serial).object(old)); // leave
        }
        let serial = self.serial();
        // enter(serial, surface, keys): no keys held.
        self.send(
            Writer::new(kbd, 1)
                .uint(serial)
                .object(surface)
                .array(&[]),
        );
        self.keyboard_on = Some(surface);
        // modifiers(serial, depressed, latched, locked, group): all clear, so
        // the client starts from a known state rather than guessing.
        let serial = self.serial();
        self.send(
            Writer::new(kbd, 4)
                .uint(serial)
                .uint(0)
                .uint(0)
                .uint(0)
                .uint(0),
        );
        true
    }

    /// A key (Linux evdev code, i.e. X keycode minus 8) went down or up.
    pub fn keyboard_key(&mut self, key: u32, pressed: bool, time: u32) {
        let Some(kbd) = self.keyboard else { return };
        if !self.ensure_keyboard_enter() {
            return;
        }
        let serial = self.serial();
        // key(serial, time, key, state): 1 pressed, 0 released.
        self.send(
            Writer::new(kbd, 3)
                .uint(serial)
                .uint(time)
                .uint(key)
                .uint(pressed as u32),
        );
    }

    /// The modifier state (xkb masks), sent after a modifier key changes it.
    pub fn keyboard_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(kbd) = self.keyboard else { return };
        if !self.ensure_keyboard_enter() {
            return;
        }
        let serial = self.serial();
        self.send(
            Writer::new(kbd, 4)
                .uint(serial)
                .uint(depressed)
                .uint(latched)
                .uint(locked)
                .uint(group),
        );
    }
}

/// Convert a frame to 8-bit RGB rows. Wayland's two mandatory formats put the
/// blue byte first in memory, which is why this looks byte-swapped and is not.
pub fn to_rgb(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity((frame.width * frame.height * 3) as usize);
    for y in 0..frame.height {
        let row = (y * frame.stride) as usize;
        for x in 0..frame.width {
            let at = row + (x * 4) as usize;
            if at + 3 < frame.pixels.len() {
                out.push(frame.pixels[at + 2]);
                out.push(frame.pixels[at + 1]);
                out.push(frame.pixels[at]);
            } else {
                out.extend_from_slice(&[0, 0, 0]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Pool(Vec<u8>);
    impl Buffers for Pool {
        fn map(&self, _fd: u32, size: usize) -> Option<Vec<u8>> {
            let mut v = self.0.clone();
            v.resize(size, 0);
            Some(v)
        }
    }

    /// Read every message out of what the compositor sent.
    fn messages(bytes: &[u8]) -> Vec<(u32, u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(h) = header(&bytes[at..]) {
            let size = h.size as usize;
            out.push((h.object, h.opcode, bytes[at + 8..at + size].to_vec()));
            at += size;
        }
        out
    }

    fn no_pool() -> Pool {
        Pool(Vec::new())
    }

    #[test]
    fn get_registry_advertises_every_global() {
        let mut c = Client::new();
        c.receive(&Writer::new(1, 1).new_id(2).finish(), &[], &no_pool());
        let out = c.take_outgoing();
        let msgs = messages(&out);
        assert_eq!(msgs.len(), GLOBALS.len());
        for (i, (object, opcode, body)) in msgs.iter().enumerate() {
            assert_eq!(*object, 2);
            assert_eq!(*opcode, 0);
            let mut fds = Vec::new();
            let mut r = Reader::new(body, &mut fds);
            assert_eq!(r.uint(), Some(i as u32 + 1));
            assert_eq!(r.string().as_deref(), Some(GLOBALS[i].0));
        }
    }

    #[test]
    fn sync_answers_immediately_and_retires_the_callback() {
        let mut c = Client::new();
        c.receive(&Writer::new(1, 0).new_id(2).finish(), &[], &no_pool());
        let msgs = messages(&c.take_outgoing());
        assert_eq!(msgs[0].0, 2, "the callback answers");
        assert_eq!(msgs[0].1, 0, "done");
        assert_eq!(msgs[1].0, 1, "then the display retires the id");
        assert_eq!(msgs[1].1, 1, "delete_id");
    }

    #[test]
    fn a_request_on_an_unknown_object_is_a_protocol_error() {
        let mut c = Client::new();
        c.receive(&Writer::new(99, 0).finish(), &[], &no_pool());
        assert!(c.failed.is_some());
        let msgs = messages(&c.take_outgoing());
        assert_eq!(msgs[0].0, 1);
        assert_eq!(msgs[0].1, 0, "wl_display.error");
    }

    /// Drive a client all the way to a committed frame, the way a real one
    /// does: registry, bind, surface, xdg role, pool, buffer, attach, commit.
    fn draw(pixels: Vec<u8>, w: i32, h: i32, stride: i32) -> (Client, Vec<Frame>) {
        let pool = Pool(pixels);
        let mut c = Client::new();
        let mut go = |c: &mut Client, bytes: Vec<u8>, fds: &[u32]| {
            c.receive(&bytes, fds, &pool);
        };
        go(&mut c, Writer::new(1, 1).new_id(2).finish(), &[]);
        // bind wl_compositor (name 1) and wl_shm (name 2) and xdg_wm_base (5)
        go(
            &mut c,
            Writer::new(2, 0).uint(1).string("wl_compositor").uint(4).new_id(3).finish(),
            &[],
        );
        go(
            &mut c,
            Writer::new(2, 0).uint(2).string("wl_shm").uint(1).new_id(4).finish(),
            &[],
        );
        go(
            &mut c,
            Writer::new(2, 0).uint(5).string("xdg_wm_base").uint(3).new_id(5).finish(),
            &[],
        );
        // create_surface -> 6
        go(&mut c, Writer::new(3, 0).new_id(6).finish(), &[]);
        // get_xdg_surface(7, surface 6), get_toplevel(8)
        go(&mut c, Writer::new(5, 2).new_id(7).object(6).finish(), &[]);
        go(&mut c, Writer::new(7, 1).new_id(8).finish(), &[]);
        go(&mut c, Writer::new(8, 2).string("a window").finish(), &[]);
        go(&mut c, Writer::new(7, 4).uint(2).finish(), &[]);
        // create_pool(9, fd, size) -- the fd travels in the control message
        go(
            &mut c,
            Writer::new(4, 0).new_id(9).int((stride * h) as i32).finish(),
            &[77],
        );
        // create_buffer(10, offset 0, w, h, stride, XRGB8888)
        go(
            &mut c,
            Writer::new(9, 0)
                .new_id(10)
                .int(0)
                .int(w)
                .int(h)
                .int(stride)
                .uint(1)
                .finish(),
            &[],
        );
        go(&mut c, Writer::new(6, 1).object(10).int(0).int(0).finish(), &[]);
        go(&mut c, Writer::new(6, 3).new_id(11).finish(), &[]);
        go(&mut c, Writer::new(6, 6).finish(), &[]);
        let frames = c.take_frames();
        (c, frames)
    }

    #[test]
    fn a_client_that_attaches_and_commits_produces_a_frame() {
        // Two by two, XRGB, distinguishable pixels.
        let px: Vec<u8> = vec![
            1, 2, 3, 255, 4, 5, 6, 255, // row 0
            7, 8, 9, 255, 10, 11, 12, 255, // row 1
        ];
        let (mut c, frames) = draw(px, 2, 2, 8);
        assert!(c.failed.is_none(), "{:?}", c.failed);
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!((f.width, f.height, f.stride), (2, 2, 8));
        assert_eq!(f.title, "a window");
        assert_eq!(f.pixels.len(), 16);
        // Blue is first in memory, so the first pixel is rgb(3,2,1).
        assert_eq!(to_rgb(f)[..3], [3, 2, 1]);
    }

    /// After `draw`, bind `wl_seat` (global name 3) as object 12 and return the
    /// client with the focus surface (6) already set by its committed frame.
    fn with_seat() -> Client {
        let (mut c, _) = draw(vec![0u8; 16], 2, 2, 8);
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(12).finish(),
            &[],
            &no_pool(),
        );
        c
    }

    #[test]
    fn a_pointer_enters_the_window_then_moves_and_frames() {
        let mut c = with_seat();
        // get_pointer -> 13
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &no_pool());
        let _ = c.take_outgoing();
        c.pointer_motion(10, 20, 100);
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == 13 && *op == 0), "enter");
        let motion = msgs.iter().find(|(o, op, _)| *o == 13 && *op == 2).expect("motion");
        let mut fds = Vec::new();
        let mut r = Reader::new(&motion.2, &mut fds);
        assert_eq!(r.uint(), Some(100), "time");
        assert_eq!(r.fixed().map(|f| f.to_int()), Some(10), "x");
        assert_eq!(r.fixed().map(|f| f.to_int()), Some(20), "y");
        assert!(msgs.iter().any(|(o, op, _)| *o == 13 && *op == 5), "frame");
    }

    #[test]
    fn a_button_enters_first_and_carries_state() {
        let mut c = with_seat();
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &no_pool());
        let _ = c.take_outgoing();
        c.pointer_button(0x110, true, 200); // BTN_LEFT
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == 13 && *op == 0), "enter before button");
        let btn = msgs.iter().find(|(o, op, _)| *o == 13 && *op == 3).expect("button");
        let mut fds = Vec::new();
        let mut r = Reader::new(&btn.2, &mut fds);
        let _serial = r.uint();
        assert_eq!(r.uint(), Some(200), "time");
        assert_eq!(r.uint(), Some(0x110), "button");
        assert_eq!(r.uint(), Some(1), "pressed");
    }

    #[test]
    fn a_keyboard_gets_a_keymap_payload_then_enter_and_keys() {
        let mut c = with_seat();
        // get_keyboard -> 13
        c.receive(&Writer::new(12, 1).new_id(13).finish(), &[], &no_pool());
        let msgs = messages(&c.take_outgoing());
        let keymap = msgs.iter().find(|(o, op, _)| *o == 13 && *op == 0).expect("keymap event");
        let mut fds = Vec::new();
        let mut r = Reader::new(&keymap.2, &mut fds);
        assert_eq!(r.uint(), Some(1), "XKB_V1 format");
        let size = r.uint().expect("size");
        // The host must have a payload of exactly `size` bytes to hand over.
        let km = c.take_keymap().expect("keymap payload");
        assert_eq!(km.len() as u32, size, "size matches the payload");
        assert!(km.starts_with(b"xkb_keymap"), "a real keymap");
        assert_eq!(km.last(), Some(&0), "NUL-terminated");
        // A key delivers enter + modifiers + the key itself.
        c.keyboard_key(30, true, 300); // KEY_A
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == 13 && *op == 1), "keyboard enter");
        assert!(msgs.iter().any(|(o, op, _)| *o == 13 && *op == 4), "modifiers primed");
        let key = msgs.iter().find(|(o, op, _)| *o == 13 && *op == 3).expect("key");
        let mut fds = Vec::new();
        let mut r = Reader::new(&key.2, &mut fds);
        let _serial = r.uint();
        assert_eq!(r.uint(), Some(300), "time");
        assert_eq!(r.uint(), Some(30), "key");
        assert_eq!(r.uint(), Some(1), "pressed");
    }

    #[test]
    fn set_cursor_is_accepted_and_leaves_the_pointer_usable() {
        let mut c = with_seat();
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &no_pool()); // get_pointer
        // The client sets its cursor the moment it is entered: set_cursor is
        // wl_pointer opcode 0 -- NOT a destroy. (serial, surface, hotspot x, y)
        c.receive(
            &Writer::new(13, 0).uint(1).object(0).int(0).int(0).finish(),
            &[],
            &no_pool(),
        );
        assert!(c.failed.is_none(), "set_cursor accepted: {:?}", c.failed);
        let _ = c.take_outgoing();
        c.pointer_motion(5, 6, 10);
        let msgs = messages(&c.take_outgoing());
        assert!(
            msgs.iter().any(|(o, op, _)| *o == 13 && *op == 2),
            "the pointer still delivers motion after set_cursor"
        );
    }

    #[test]
    fn a_pointer_release_destroys_it_and_stops_events() {
        let mut c = with_seat();
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &no_pool());
        c.receive(&Writer::new(13, 1).finish(), &[], &no_pool()); // release is opcode 1
        let _ = c.take_outgoing();
        c.pointer_motion(5, 6, 10);
        assert!(c.take_outgoing().is_empty(), "no events after the pointer is released");
    }

    #[test]
    fn input_before_any_device_is_bound_is_dropped() {
        let mut c = with_seat();
        let _ = c.take_outgoing();
        c.pointer_motion(1, 2, 3);
        c.keyboard_key(30, true, 4);
        assert!(c.take_outgoing().is_empty(), "no pointer/keyboard bound, no events");
    }

    #[test]
    fn a_commit_releases_the_buffer_and_answers_the_frame_callback() {
        let px = vec![0u8; 16];
        let (mut c, _) = draw(px, 2, 2, 8);
        let msgs = messages(&c.take_outgoing());
        assert!(
            msgs.iter().any(|(o, op, _)| *o == 10 && *op == 0),
            "wl_buffer.release"
        );
        assert!(
            msgs.iter().any(|(o, op, _)| *o == 11 && *op == 0),
            "wl_callback.done"
        );
    }

    #[test]
    fn a_toplevel_is_configured_before_it_may_draw() {
        let px = vec![0u8; 16];
        let (mut c, _) = draw(px, 2, 2, 8);
        let _ = &mut c;
        // The configure pair goes out at get_toplevel; the test above then
        // acks it, and the surface records that.
        assert!(c.surfaces.values().any(|s| s.configured));
    }

    #[test]
    fn a_buffer_larger_than_its_pool_is_refused_rather_than_read() {
        // The pool is one pixel; the buffer claims sixteen.
        let px = vec![0u8; 4];
        let pool = Pool(px);
        let spec = BufferSpec {
            fd: 77,
            pool_size: 4,
            offset: 0,
            width: 4,
            height: 4,
            stride: 16,
            format: 1,
        };
        let mapped = pool.map(77, 4).unwrap();
        assert!(Client::cut(&mapped, &spec).is_none());
    }

    #[test]
    fn a_stride_narrower_than_the_row_is_refused() {
        let spec = BufferSpec {
            fd: 77,
            pool_size: 64,
            offset: 0,
            width: 4,
            height: 1,
            stride: 8,
            format: 1,
        };
        assert!(Client::cut(&vec![0u8; 64], &spec).is_none());
    }

    #[test]
    fn messages_split_across_two_reads_are_reassembled() {
        let mut c = Client::new();
        let bytes = Writer::new(1, 1).new_id(2).finish();
        c.receive(&bytes[..5], &[], &no_pool());
        assert!(c.take_outgoing().is_empty(), "nothing until it is whole");
        c.receive(&bytes[5..], &[], &no_pool());
        assert!(!c.take_outgoing().is_empty());
    }
}
