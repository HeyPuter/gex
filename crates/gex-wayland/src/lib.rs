//! A Wayland compositor, as a state machine over the wire protocol.
//!
//! The guest is a Linux userspace with no display hardware, so the display
//! server cannot live inside it: there is no DRM device to drive and no
//! keyboard to read. It lives in the host instead. A guest client connects to
//! `$XDG_RUNTIME_DIR/wayland-0`, the kernel routes that unix socket to here,
//! and what comes out the other side is a list of surfaces with pixels in
//! them that the host can draw however it likes -- a window natively, a canvas
//! in a browser, a PNG in a test.
//!
//! Nothing in this crate knows how to draw or where the pixels are. It takes
//! bytes and descriptors in, produces bytes out, and reports commits as
//! [`Frame`]s naming a buffer. The host resolves a buffer to memory through
//! [`Buffers`], which is the only thing it has to implement.
//!
//! # What is here
//!
//! Enough of the protocol for a real client to map a window and show pixels:
//! `wl_display`, `wl_registry`, `wl_callback`, `wl_compositor`, `wl_shm` and
//! its pools and buffers, `wl_surface`, `wl_region`, `wl_seat` with a keyboard
//! and a pointer, `wl_output`, and the `xdg_shell` trio that every toolkit
//! now requires. Shared memory only: there is no `linux-dmabuf` and no
//! `wl_drm`, so a client that insists on a GPU buffer will not find one. See
//! the note on acceleration in the README.

pub mod wire;

use std::collections::HashMap;
use wire::{header, Fixed, Reader, Writer};

/// Where a buffer's pixels are.
///
/// A `wl_shm_pool` is made from a descriptor the client sent, and the pixels
/// live wherever the host put that descriptor's memory. In gex that is inside
/// the guest's own address space, so this hands back a slice of it and no
/// copy happens anywhere.
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
    /// A `wl_shm` format code. 0 is ARGB8888, 1 is XRGB8888; those two are
    /// the only ones every client is required to support and the only ones
    /// advertised here.
    pub format: u32,
    /// The pixels, already copied out of the client's pool.
    pub pixels: Vec<u8>,
    /// The window title, when the surface has an `xdg_toplevel` with one.
    pub title: String,
}

/// Interfaces the registry advertises, with the version offered.
///
/// Version numbers matter: a toolkit binds at the highest version it and the
/// compositor share and then uses requests that only exist at that version.
/// Offering a version whose requests are not implemented is worse than
/// offering a lower one, so these are deliberately conservative.
const GLOBALS: &[(&str, u32)] = &[
    ("wl_compositor", 4),
    ("wl_shm", 1),
    ("wl_seat", 5),
    ("wl_output", 3),
    ("xdg_wm_base", 3),
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
}

#[derive(Clone, Debug)]
struct BufferSpec {
    pool: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
}

/// The pending and current state of one surface, as Wayland's double-buffered
/// model requires: everything a client says takes effect at `commit` and not
/// before.
#[derive(Clone, Debug, Default)]
struct SurfaceState {
    pending_buffer: Option<u32>,
    committed_buffer: Option<u32>,
    /// Frame callbacks the client asked for, delivered after the commit they
    /// belong to. A client that does not get these back stops drawing.
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
    /// Ids the server allocates for itself, counting down from the top of the
    /// server range so they cannot collide with a client's.
    next_serial: u32,
    /// Whether the registry has been advertised, so `bind` can be checked
    /// against something.
    default_size: (i32, i32),
    /// Set when the protocol has been violated and the connection is over.
    pub failed: Option<String>,
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
        }
    }

    /// The size a toplevel is told to be. A real compositor decides this from
    /// its layout; here it is fixed and configurable so a test can ask for a
    /// particular window.
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

    fn dispatch(&mut self, object: u32, opcode: u16, body: &[u8], buffers: &dyn Buffers) {
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
                Object::Buffer(_) | Object::Region | Object::Positioner => {
                    // Every one of these has `destroy` at opcode 0 and
                    // requests that change nothing observable here.
                    if opcode == 0 {
                        self.objects.remove(&object);
                        self.delete_id(object);
                    }
                    true
                }
                Object::Output | Object::Keyboard | Object::Pointer | Object::Callback => {
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
            // sync(callback): answer at once. A client uses this as a
            // round-trip barrier and blocks until the `done` comes back.
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
                // A client picks its format from what the compositor says it
                // can show. Both of these are mandatory for every compositor.
                for fmt in [0u32, 1] {
                    let w = Writer::new(id, 0).uint(fmt);
                    self.send(w);
                }
            }
            "wl_seat" => {
                self.objects.insert(id, Object::Seat);
                // A pointer and a keyboard, which is what a toolkit checks
                // for before it will build input handling at all.
                let w = Writer::new(id, 0).uint(0b11);
                self.send(w);
                let w = Writer::new(id, 1).string("gex-seat0");
                self.send(w);
            }
            "wl_output" => {
                self.objects.insert(id, Object::Output);
                let (w_px, h_px) = (1920, 1080);
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
                self.objects.insert(
                    id,
                    Object::Buffer(BufferSpec {
                        pool: object,
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
            // damage(x, y, width, height): what changed, which matters to a
            // compositor that avoids redrawing and not to one that does not.
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

    /// Apply a surface's pending state, which is the only moment anything a
    /// client said becomes visible.
    fn commit(&mut self, surface: u32, buffers: &dyn Buffers) {
        let Some(state) = self.surfaces.get(&surface).cloned() else {
            return;
        };
        let buffer_id = state.pending_buffer.or(state.committed_buffer);
        let mut produced = None;
        if let Some(buffer_id) = buffer_id {
            if let Some(Object::Buffer(spec)) = self.objects.get(&buffer_id).cloned() {
                if let Some(Object::ShmPool { fd, size }) = self.objects.get(&spec.pool).cloned() {
                    if let Some(pool) = buffers.map(fd, size) {
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
            }
            // A client may not draw into a buffer again until it is released,
            // and most will not draw a second frame without this.
            let w = Writer::new(buffer_id, 0);
            self.send(w);
        }
        if let Some(frame) = produced {
            self.frames.push(frame);
        }
        // The frame callbacks belong to the commit that carried them. The
        // timestamp is meant to be milliseconds of some monotonic clock; a
        // client uses it only for deltas, and a serial counts up the same way.
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

    /// Copy a buffer's rectangle out of its pool.
    ///
    /// Returns `None` rather than a short buffer when the pool does not hold
    /// what the client said it does; a compositor that reads past the end of a
    /// client's pool is a compositor a client can crash.
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
                // A toplevel is not allowed to draw until it has been
                // configured, so the size goes out immediately. Zero width and
                // height would mean "you choose"; a real size is friendlier
                // and is what a tiling compositor sends.
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
                true
            }
            // get_keyboard(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Keyboard);
                // A keyboard with no keymap is legal only at version 7 and
                // above; below that a client waits for one. Format 0 means
                // "no keymap", which every client handles.
                let w = Writer::new(id, 0).uint(0).uint(0).uint(0);
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
}

/// Convert a frame to 8-bit RGB rows, for a host that wants to write an image.
///
/// Wayland's two mandatory formats are both little-endian 32-bit words with
/// the blue byte first in memory, which is why this looks byte-swapped and is
/// not.
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
            pool: 9,
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
            pool: 9,
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
