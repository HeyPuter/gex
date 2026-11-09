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

pub mod data;
mod pointer;
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

    /// Where the memory behind a descriptor *is*, as an address in the address
    /// space the host can read, and its length.
    ///
    /// This is the difference between a compositor that copies and one that
    /// does not. A `wl_shm` pool is guest memory, and in the browser the host
    /// can read guest memory directly -- so a host that implements this gets a
    /// buffer's address and uploads the damaged rows to a texture, and nothing
    /// ever copies a frame on the way out. `map` stays for the host that has no
    /// such view (the native one, which composites from its own copy).
    ///
    /// `size` is the pool's: a process that maps the descriptor more than
    /// once has the pool where a mapping holds all of it.
    ///
    /// 64 bits: a memory64 process's pool may sit above 4 GiB.
    fn locate(&self, _fd: u32, _size: usize) -> Option<(u64, u64)> {
        None
    }
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

/// What a surface is, as far as presentation is concerned. A surface with no
/// role is not displayable; the rest are drawn differently and hit-tested
/// differently, so the role is part of what the host is told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    /// Created, but given no role yet: not a window, and never drawn.
    #[default]
    None,
    /// An `xdg_toplevel`: a window. The compositor places and decorates it.
    Toplevel,
    /// An `xdg_popup`: a menu or a combo box, placed relative to its parent by
    /// the positioner the client sent.
    Popup,
    /// A `wl_subsurface`: part of its parent's window, at a fixed offset.
    Subsurface,
    /// The surface a client handed to `wl_pointer.set_cursor`. Drawn at the
    /// pointer, offset by its hotspot -- never a window, never hit-tested.
    Cursor,
}

impl Role {
    /// The wire value the host sees, and what `Role::from_code` reads back.
    pub fn code(self) -> u32 {
        match self {
            Role::None => 0,
            Role::Toplevel => 1,
            Role::Popup => 2,
            Role::Subsurface => 3,
            Role::Cursor => 4,
        }
    }
}

/// A `wl_region`: the rectangles a client added, less the ones it
/// subtracted, kept as disjoint rectangles in surface coordinates.
///
/// An input region is where a surface takes the pointer. Wine gives the
/// subsurface that shows a window's GL picture an empty one, so the pointer
/// goes to the window under it, in that window's coordinates; a compositor
/// that ignored it handed clicks to the subsurface, offset by the frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    rects: Vec<(i32, i32, i32, i32)>,
}

impl Region {
    /// More rectangles than any real client's region, and a bound on what a
    /// client can make the compositor keep: past it, the region is its bounds.
    const MAX_RECTS: usize = 256;

    fn add(&mut self, r: (i32, i32, i32, i32)) {
        if r.2 <= 0 || r.3 <= 0 {
            return;
        }
        self.subtract(r);
        self.rects.push(r);
        if self.rects.len() > Self::MAX_RECTS {
            let b = self.bounds();
            self.rects = vec![b];
        }
    }

    fn subtract(&mut self, (x, y, w, h): (i32, i32, i32, i32)) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (x2, y2) = (x.saturating_add(w), y.saturating_add(h));
        let mut out = Vec::with_capacity(self.rects.len());
        for &(rx, ry, rw, rh) in &self.rects {
            let (rx2, ry2) = (rx + rw, ry + rh);
            if x >= rx2 || x2 <= rx || y >= ry2 || y2 <= ry {
                out.push((rx, ry, rw, rh));
                continue;
            }
            // What is left of the rectangle: the bands above and below the
            // cut, then the parts beside it.
            let (cy, cy2) = (y.max(ry), y2.min(ry2));
            if ry < cy {
                out.push((rx, ry, rw, cy - ry));
            }
            if cy2 < ry2 {
                out.push((rx, cy2, rw, ry2 - cy2));
            }
            if rx < x {
                out.push((rx, cy, x - rx, cy2 - cy));
            }
            if x2 < rx2 {
                out.push((x2, cy, rx2 - x2, cy2 - cy));
            }
        }
        self.rects = out;
    }

    /// Whether a point in surface coordinates is in the region.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.rects.iter().any(|&(rx, ry, rw, rh)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
    }

    /// The smallest rectangle holding the region; zero-sized when it is empty.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        let mut it = self.rects.iter();
        let Some(&(x, y, w, h)) = it.next() else { return (0, 0, 0, 0) };
        let (mut x1, mut y1, mut x2, mut y2) = (x, y, x + w, y + h);
        for &(rx, ry, rw, rh) in it {
            x1 = x1.min(rx);
            y1 = y1.min(ry);
            x2 = x2.max(rx + rw);
            y2 = y2.max(ry + rh);
        }
        (x1, y1, x2 - x1, y2 - y1)
    }

    /// Whether the region is exactly its bounds, so a host testing the
    /// bounds tests the region.
    pub fn is_rect(&self) -> bool {
        let (_, _, w, h) = self.bounds();
        let area: i64 = self.rects.iter().map(|r| i64::from(r.2) * i64::from(r.3)).sum();
        area == i64::from(w) * i64::from(h)
    }
}

/// A rectangle of a surface that changed since the host last looked, in surface
/// pixels. `w == 0` means "all of it" -- what a client that never sent a damage
/// rectangle is asking for, and what a newly attached buffer of a new size is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Damage {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Damage {
    fn all() -> Damage {
        Damage { x: 0, y: 0, w: 0, h: 0 }
    }

    /// Whether this means "the whole surface", which is what a client that
    /// declared no damage at all is asking for.
    pub fn is_all(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// The smallest rectangle covering both. Damage is a union of rectangles in
    /// the protocol; one bounding box is what an upload wants, and the cost of
    /// being loose is uploading a few rows nothing changed in.
    fn merge(&mut self, o: Damage) {
        if o.is_all() || self.is_all() {
            *self = Damage::all();
            return;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = (self.x + self.w).max(o.x + o.w);
        let b = (self.y + self.h).max(o.y + o.h);
        *self = Damage { x, y, w: r - x, h: b - y };
    }
}

/// One surface, as the host needs it to draw a frame: what it is, where its
/// pixels are, and what changed.
///
/// Deliberately flat and `Copy`: the host mirrors an array of these into shared
/// memory every pump, and a compositor that allocates per surface per frame is
/// a compositor that stutters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceInfo {
    /// The `wl_surface` object id, which is the host's handle for it.
    pub surface: u32,
    pub role: Role,
    /// The parent surface for a popup or a subsurface, else 0.
    pub parent: u32,
    /// Offset from the parent (popup, subsurface), or the cursor's hotspot.
    pub rel_x: i32,
    pub rel_y: i32,
    pub width: i32,
    pub height: i32,
    /// Where the pixels are: an address in the guest's address space, and the
    /// row stride. Zero when the surface has no buffer the host can read.
    /// 64 bits: a memory64 process's pool may sit above 4 GiB.
    pub addr: u64,
    pub stride: i32,
    /// A `wl_shm` format code: 0 is ARGB8888, 1 is XRGB8888.
    pub format: u32,
    /// Bumped on every commit that brings new pixels, so the host can tell a
    /// surface that redrew from one that is merely still there.
    pub serial: u32,
    pub damage: Damage,
    /// The `xdg_toplevel` title, for the decoration the host draws.
    pub title: String,
    /// What application it is: `xdg_toplevel.set_app_id` (by convention its
    /// desktop entry's name), or an X window's `WM_CLASS` class, as a desktop
    /// entry's `StartupWMClass` names it. Empty when the client said nothing.
    pub app_id: String,
    /// The guest pid of the program that made it, when known otherwise than
    /// by its connection: an X window's `_NET_WM_PID` (its connection is the
    /// X server's). 0 when not.
    pub pid: u32,
    /// The client's last window-management request (`xdg_toplevel.move`,
    /// `resize`, `set_maximized`, `unset_maximized`), numbered so the host
    /// acts on each once: a client-decorated window is dragged, resized and
    /// maximized by its own frame this way.
    pub request: Request,
    /// Bumped with every request; the host remembers the last it acted on.
    pub request_seq: u8,
    /// The client asked to be fullscreen (`xdg_toplevel.set_fullscreen`) and
    /// has not taken it back: a state, not a numbered request, because a
    /// client asks before its first commit (a rootful Xwayland, a nested
    /// compositor) and a host takes the first request number it sees as
    /// already handled. A fullscreen window has no frame and fills the output.
    pub fullscreen: bool,
    /// A configure has gone out that the client has not acked yet: a host
    /// resizing interactively sends the next size once this clears, so a
    /// client that redraws slowly is not buried in configures.
    pub configure_pending: bool,
    /// An X window, paired to this surface by the compositor's own X window
    /// manager rather than given a role by its client.
    pub x11: bool,
    /// The X window id, when the window manager has named one.
    pub xid: u32,
    /// The pixels are not at `addr`: the host knows where they are.
    pub external: bool,
    /// `wl_surface.set_buffer_scale`: buffer pixels per surface unit (1 unless
    /// the client said otherwise).
    pub buffer_scale: i32,
    /// The output scale this client was told (`wl_output.scale`): device
    /// pixels per surface unit. The host draws a surface at
    /// `width * client_scale / buffer_scale` device pixels.
    pub client_scale: i32,
    /// Where the client says the surface is and what it is stacked under
    /// (`gex_window_placement_v1`), when it says so.
    pub placement: Placement,
    /// The input region (`wl_surface.set_input_region`): `None` is the
    /// whole surface, as it is until a client sets one, and an empty region
    /// takes no pointer input at all.
    pub input: Option<Region>,
    /// A client's `zwp_locked_pointer_v1` asks for the pointer on this
    /// surface (`flags` bit 31): the host locks it when it can (pointer.rs).
    pub pointer_lock: bool,
}

/// A placed surface's position and stacking, as its client asked for them
/// (`protocols/gex-window-placement-v1.xml`). Its position is `rel_x`/`rel_y`
/// in output pixels.
///
/// The one way a client places a window: a Wine window says it through the
/// protocol, and an X11 override-redirect window (a menu) is given the same
/// state by the X window manager (`adopt_x11`), so a compositor draws,
/// stacks and hit-tests both by the same code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// The surface is placed. Everything below means nothing otherwise.
    pub placed: bool,
    /// The client draws the whole window: no decoration.
    pub unmanaged: bool,
    /// 0 normal, 1 topmost, 2 overlay (menus).
    pub layer: u32,
    /// The client's name for the surface, which other placed surfaces --
    /// of any client in the session -- use to say what they go below.
    pub id: u32,
    /// The last restack asked for: directly below the surface with this
    /// id, 0 the top of the layer, `u32::MAX` its bottom.
    pub below: u32,
    /// Bumped by every restack request, so a host acts on each once.
    pub restack: u32,
    /// Bumped by every activate request: give this surface the keyboard.
    pub activate: u32,
}

impl Placement {
    /// The bottom of a layer, for `below`.
    pub const BOTTOM: u32 = u32::MAX;

    /// The placement words of a surface record (`R_PLACE`.. in
    /// `wl-bridge.mjs`): flags and layer, id, below, restack, activate.
    pub fn record_words(&self) -> [i32; 5] {
        [
            i32::from(self.placed) | (i32::from(self.unmanaged) << 1) | ((self.layer as i32 & 0xff) << 8),
            self.id as i32,
            self.below as i32,
            self.restack as i32,
            self.activate as i32,
        ]
    }
}

impl SurfaceInfo {
    /// `R_FLAGS`, as `wl-bridge.mjs` reads it: fullscreen (bit 0), external (bit 1), a configure
    /// not yet acked (bit 2), the request's number (bits 3..6), the buffer
    /// and client scales (bits 8 and 16), the request itself
    /// ([`Request::code`], bits 24..30), and a pointer lock asked for (bit 31).
    pub fn flags(&self) -> i32 {
        i32::from(self.fullscreen)
            | (i32::from(self.external) << 1)
            | (i32::from(self.configure_pending) << 2)
            | ((i32::from(self.request_seq) & 0xf) << 3)
            | ((self.buffer_scale & 0xff) << 8)
            | ((self.client_scale & 0xff) << 16)
            | (((self.request.code() & 0x7f) as i32) << 24)
            | (i32::from(self.input.is_some()) << 7)
            | (i32::from(self.pointer_lock) << 31)
    }

    /// The input region as a host's record carries it, with `flags` bit 7
    /// saying there is one: its bounds in surface units, x, y, width, height.
    /// Zero-sized is a surface that takes no pointer input. A region with
    /// holes is published as its bounds (a client's input region is in
    /// practice a rectangle: empty, the whole surface, or the window inside
    /// its shadow).
    pub fn input_words(&self) -> [i32; 4] {
        let (x, y, w, h) = self.input.as_ref().map(Region::bounds).unwrap_or_default();
        [x, y, w, h]
    }
}

/// An `EV_CONFIGURE`'s states word, with the older activated word folded in.
pub fn configure_states(states: i32, activated: i32) -> u32 {
    (states as u32 & !STATE_ACTIVATED) | if activated != 0 { STATE_ACTIVATED } else { 0 }
}

/// Words in one published surface record: the layout a host page reads
/// (`hosts/browser/js/wl-bridge.mjs`, the same numbers by name), written by
/// [`push_record`] for a guest's surfaces and the browser X server's alike.
pub const REC: usize = 119;
/// Where the title's UTF-8 bytes start, and how many there are room for.
pub const REC_TITLE: usize = 38;
pub const REC_TITLE_BYTES: usize = 256;
/// The app id's, likewise.
pub const REC_APP_ID: usize = 102;
pub const REC_APP_ID_BYTES: usize = 64;
/// [`SurfaceInfo::pid`].
pub const REC_PID: usize = 118;

/// `text` into `words` as UTF-8, little-endian, cut on a character boundary
/// to fit; the rest zero.
fn put_text(words: &mut [i32], text: &str) {
    let mut n = text.len().min(words.len() * 4);
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    for (i, chunk) in text.as_bytes()[..n].chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        words[i] = i32::from_le_bytes(word);
    }
}

/// One surface's record, appended to `out`, for the host's client `client`.
/// Words 18..25 are free (the title's before it had room); 26, 27 and 29..32
/// beyond the placement's are the host side's own.
pub fn push_record(out: &mut Vec<i32>, client: u32, info: &SurfaceInfo) {
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
    r[8] = info.addr as u32 as i32;
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
    // Where its client placed it, if it did (`gex_window_placement_v1`).
    r[28..33].copy_from_slice(&info.placement.record_words());
    // Where it takes the pointer, when its client said (`flags` bit 7).
    r[33..37].copy_from_slice(&info.input_words());
    // The address's high half: a memory64 process's pool may sit above 4 GiB.
    r[37] = (info.addr >> 32) as u32 as i32;
    put_text(&mut r[REC_TITLE..REC_TITLE + REC_TITLE_BYTES / 4], &info.title);
    put_text(&mut r[REC_APP_ID..REC_APP_ID + REC_APP_ID_BYTES / 4], &info.app_id);
    r[REC_PID] = info.pid as i32;
}

/// A window-management request a client made of the compositor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Request {
    #[default]
    None,
    /// `xdg_toplevel.move`: drag me by the pointer.
    Move,
    /// `xdg_toplevel.resize`: resize me by the pointer, from these edges
    /// (`xdg_toplevel.resize_edge` bits: 1 top, 2 bottom, 4 left, 8 right).
    Resize(u8),
    Maximize,
    Unmaximize,
    /// `xdg_toplevel.set_minimized`: a client's own minimize button.
    Minimize,
}

impl Request {
    /// As the host reads it: the kind in bits 0..2, the edges in 3..6.
    pub fn code(self) -> u32 {
        match self {
            Request::None => 0,
            Request::Move => 1,
            Request::Resize(edges) => 2 | (u32::from(edges & 0xf) << 3),
            Request::Maximize => 3,
            Request::Unmaximize => 4,
            Request::Minimize => 5,
        }
    }
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

/// A configure's states, as the host passes them (`EV_CONFIGURE`).
pub const STATE_ACTIVATED: u32 = 1;
pub const STATE_MAXIMIZED: u32 = 2;
pub const STATE_RESIZING: u32 = 4;
pub const STATE_FULLSCREEN: u32 = 8;

/// What the host's frame takes from the output, in surface units: a border
/// on each side, and the title bar over a border at the bottom
/// (compositor-worker.js `BORDER` 1 and `TITLE_H` 26, in CSS pixels).
const FRAME_W: i32 = 2;
const FRAME_H: i32 = 27;

/// Pixels of scroll that make one wheel notch (WHEEL_DELTA, 120 value120
/// units): what a browser's mouse wheel scrolls a notch by (Chromium's
/// `deltaY` 100, `deltaMode` pixels).
pub const WHEEL_PX: i32 = 100;

const GLOBALS: &[(&str, u32)] = &[
    ("wl_compositor", 4),
    ("wl_shm", 1),
    // 8 for `wl_pointer.axis_value120`: Wine's winewayland turns only discrete
    // or value120 scrolls into WM_MOUSEWHEEL, never the continuous `axis`.
    ("wl_seat", 8),
    ("wl_output", 3),
    // 4 for `xdg_toplevel.configure_bounds`: the room a window has, so a
    // client that chooses its own size chooses one that fits.
    ("xdg_wm_base", 4),
    // GTK expects these during display init; without them it defers/abandons
    // seat setup (never binds wl_seat) and loops on GDK_IS_SEAT. The data
    // device takes drags from the host (data.rs); there is no clipboard yet.
    ("wl_data_device_manager", 3),
    ("wl_subcompositor", 1),
    // Server-side decorations, which is what makes this a compositor rather
    // than a surface mirror: a Qt or KDE client that sees this protocol stops
    // drawing its own titlebar and lets the compositor do it. GTK 3 has never
    // implemented it and decorates itself regardless -- the same thing it does
    // under KWin, so it is the right answer there too, not a gap here.
    ("zxdg_decoration_manager_v1", 1),
    // Wine's Wayland driver will not start without it: every window gets a
    // viewport, whose destination says the size the surface shows at.
    // Recorded (`SurfaceState::viewport`), not yet scaled by: at scale 1 a
    // destination is the buffer's own size.
    ("wp_viewporter", 1),
    // Windows with real coordinates: a Win32 window (Wine) says where it is
    // and what it is under, and the compositor shows it there rather than
    // cascading it. See `protocols/gex-window-placement-v1.xml`. Version 2
    // adds the `origin` event: where the compositor has the surface, for
    // turning its surface-local pointer positions into Win32's.
    ("gex_window_placement_manager_v1", 2),
    // Mouse look: a lock the host grants (the page's pointer lock) and the
    // movement under it (pointer.rs). Xwayland locks for an X client that
    // grabs the pointer and hides it.
    ("zwp_relative_pointer_manager_v1", 1),
    ("zwp_pointer_constraints_v1", 1),
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
    XdgPopup { xdg_surface: u32 },
    /// `wl_data_device_manager`, and the version it was bound at, which its
    /// devices and their offers have (data.rs).
    DataDeviceManager { version: u32 },
    DataDevice,
    DataSource,
    DataOffer,
    Subcompositor,
    Subsurface { surface: u32 },
    Viewporter,
    Viewport { surface: u32 },
    DecorationManager,
    Decoration { surface: u32 },
    PlacementManager,
    Placement { surface: u32 },
    RelativePointerManager,
    RelativePointer,
    PointerConstraints,
    LockedPointer,
    ConfinedPointer,
}

/// An `xdg_positioner`: where a popup wants to be, expressed the way the
/// protocol expresses it -- a rectangle on the parent, an edge of that
/// rectangle to hang from, and a direction to grow in.
#[derive(Clone, Copy, Debug, Default)]
struct Positioner {
    width: i32,
    height: i32,
    anchor_rect: (i32, i32, i32, i32),
    anchor: u32,
    gravity: u32,
    offset: (i32, i32),
}

impl Positioner {
    /// The popup's top-left corner relative to the parent's window geometry.
    ///
    /// The anchor picks a point on the anchor rectangle; the gravity says which
    /// way the popup grows from it. Both are the same nine-value enum, so both
    /// are read the same way: 0 none (the centre), 1 top, 2 bottom, 3 left,
    /// 4 right, and 5..8 the corners.
    fn place(&self) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        let (mut x, mut y) = (ax + aw / 2, ay + ah / 2);
        match self.anchor {
            1 => y = ay,
            2 => y = ay + ah,
            3 => x = ax,
            4 => x = ax + aw,
            5 => { x = ax; y = ay }
            6 => { x = ax + aw; y = ay }
            7 => { x = ax; y = ay + ah }
            8 => { x = ax + aw; y = ay + ah }
            _ => {}
        }
        let (w, h) = (self.width.max(0), self.height.max(0));
        // Gravity is where the popup goes relative to the anchor point, so it
        // decides which corner of the popup lands there.
        match self.gravity {
            1 => y -= h,
            2 => {}
            3 => x -= w,
            4 => {}
            5 => { x -= w; y -= h }
            6 => { y -= h }
            7 => { x -= w }
            8 => {}
            // "none": centred on the anchor point.
            _ => { x -= w / 2; y -= h / 2 }
        }
        (x + self.offset.0, y + self.offset.1)
    }
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
    app_id: String,
    /// See [`SurfaceInfo::pid`].
    pid: u32,
    /// The `xdg_surface` that gave this surface a role, if any.
    xdg: Option<u32>,
    /// Set once the client has acknowledged a configure, which is when a
    /// toplevel is allowed to show anything.
    configured: bool,
    /// What this surface is for. Set when it takes a role, and the reason an
    /// unroled surface (a cursor image, a scratch buffer) never reaches the
    /// output.
    role: Role,
    /// The parent of a popup or a subsurface, and the offset from it. For a
    /// cursor these carry the hotspot instead.
    parent: u32,
    rel_x: i32,
    rel_y: i32,
    /// Where the last committed buffer's pixels are and what shape they are:
    /// the descriptor the host draws from, rather than a copy of them.
    addr: u64,
    stride: i32,
    width: i32,
    height: i32,
    format: u32,
    /// Bumped on every commit with new pixels.
    serial: u32,
    /// Damage the client has declared and the host has not drawn yet, kept
    /// pending until commit like everything else in Wayland.
    ///
    /// `None` is not the same as an empty rectangle: it means the client
    /// declared nothing, and `Damage::all()` -- an empty rectangle -- means
    /// "everything". Conflating the two made every commit a full upload,
    /// because the accumulator started at the value that means all of it.
    pending_damage: Option<Damage>,
    damage: Option<Damage>,
    /// The window geometry a client set with `xdg_surface.set_window_geometry`:
    /// the part of the surface that is the window, excluding the invisible
    /// margins GTK leaves for its shadows. What the compositor decorates and
    /// hit-tests, when it is set.
    geom: Option<(i32, i32, i32, i32)>,
    /// The last window-management request, and its number (see
    /// [`SurfaceInfo::request`]).
    request: Request,
    request_seq: u8,
    /// See [`SurfaceInfo::fullscreen`].
    fullscreen: bool,
    /// The serial of the last configure sent for this surface's role, and of
    /// the last one the client acked.
    configure_sent: u32,
    configure_acked: u32,
    /// This surface is an X11 window, paired with it by the X window manager.
    /// It has no xdg role and never will: rootless Xwayland hands the
    /// compositor bare surfaces and names them over the X connection.
    x11: bool,
    /// The X window this surface is, when it is one. Published so a host that
    /// knows a drawable -- an OpenGL client naming what it draws into -- can
    /// find the surface without knowing anything about X.
    xid: u32,
    /// The size of a surface whose pixels are somewhere else.
    ///
    /// A client rendering through the host's OpenGL attaches no buffer: there
    /// is nothing to attach, because its frames never come back to the guest.
    /// Such a surface would never be published at all -- no buffer, no size,
    /// nothing to draw -- so it says how big it is and the compositor finds
    /// the pixels by other means.
    external: Option<(i32, i32)>,
    /// The descriptor of the pool behind what this surface shows, while it
    /// shows it. A client may destroy a buffer, and its pool, once the buffer
    /// is released -- which is at commit here -- and the surface still shows
    /// those pixels: the host must not free them while it may read them again.
    shown_fd: Option<u32>,
    /// A NULL buffer was attached: this commit unmaps the surface.
    pending_null: bool,
    /// Subsurfaces of this surface, in bottom-to-top order.
    children: Vec<u32>,
    /// `wp_viewport.set_destination`: the size the surface is shown at, when
    /// the client set one.
    viewport: Option<(i32, i32)>,
    /// `set_buffer_scale`, pending until commit, and the committed value
    /// (0 is 1).
    pending_scale: Option<i32>,
    buffer_scale: i32,
    /// `wp_viewport.set_source`: the part of the buffer that is the surface,
    /// as x, y, width, height in buffer pixels. Wine keeps a window's buffer
    /// when the window shrinks and crops it this way, so without it a 40-pixel
    /// taskbar showed as its old 125-pixel buffer.
    crop: Option<(i32, i32, i32, i32)>,
    /// `gex_window_placement_v1`: where the client put the surface.
    placement: Placement,
    /// The last `gex_window_placement_v1.origin` sent: where the host has the
    /// surface, which the pointer positions after it are relative to.
    origin: Option<(i32, i32)>,
    /// `set_input_region`, pending until commit (`Some(None)`: back to the
    /// whole surface), and the committed region (`None`: the whole surface).
    pending_input: Option<Option<Region>>,
    input: Option<Region>,
}

/// Whether a surface's pixels are the host's. A free function because the
/// caller is holding `self.surfaces` mutably where it asks.
fn state_is_external(surfaces: &HashMap<u32, SurfaceState>, id: u32) -> bool {
    surfaces.get(&id).is_some_and(|s| s.external.is_some())
}

/// One connected client.
pub struct Client {
    objects: HashMap<u32, Object>,
    surfaces: HashMap<u32, SurfaceState>,
    /// Surfaces named as drawn-on-the-host before they existed. See
    /// [`Client::attach_external`].
    external_pending: HashMap<u32, (i32, i32)>,
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
    /// The size a toplevel is told it is, in device pixels.
    default_size: (i32, i32),
    /// Surfaces whose fullscreen state the client changed and the host has
    /// not taken yet ([`Client::take_fullscreen_changes`]), one entry each.
    fullscreen_changes: Vec<(u32, bool)>,
    /// Device pixels per surface unit: what `wl_output.scale` says. A
    /// toplevel's configure is in surface units, so `default_size / scale`.
    scale: i32,
    /// The version this client bound `xdg_wm_base` at, which its toplevels
    /// share: `configure_bounds` is only for 4 and up.
    wm_base_version: u32,
    /// The version this client bound `gex_window_placement_manager_v1` at,
    /// which its placement objects share: `origin` is for 2 and up.
    placement_version: u32,
    /// Positioner state, by object id: what a popup asked to be placed against.
    positioners: HashMap<u32, Positioner>,
    /// What each `wl_region` holds.
    regions: HashMap<u32, Region>,
    /// Surfaces whose scene state changed since the host last published it, so
    /// a pump that changed nothing costs nothing.
    dirty: bool,
    /// Set when the protocol has been violated and the connection is over.
    pub failed: Option<String>,
    /// The `wl_pointer` and `wl_keyboard` object ids once the client asks for
    /// them, so injected input has somewhere to go.
    pointer: Option<u32>,
    keyboard: Option<u32>,
    /// The `wl_seat` version the client bound: which scroll events its
    /// pointer takes (`axis_discrete` from 5, `axis_value120` from 8).
    seat_version: u32,
    /// Scroll not yet sent as wheel units, per axis: in 1/(256 * WHEEL_PX)
    /// of a value120 unit for `axis_value120`, and value120 units short of a
    /// whole notch for `axis_discrete`.
    scroll_rest: [i64; 2],
    scroll_notch: [i32; 2],
    /// The surface input is delivered to when the host has not said which:
    /// the last one to commit a buffer. What a one-window host wants, and the
    /// fallback for the native host, which composites from copies and has no
    /// hit test of its own.
    focus_surface: Option<u32>,
    /// The surface the host's hit test says the pointer is over, and the one it
    /// gave the keyboard to. A real compositor decides both, so these win over
    /// `focus_surface` when set.
    pointer_target: Option<u32>,
    keyboard_target: Option<u32>,
    /// The surface the client last handed to `set_cursor`, so a new one
    /// replaces it rather than leaving two cursors in the scene.
    cursor_surface: Option<u32>,
    /// The last position the host injected, in surface pixels. An `enter`
    /// carries it, so a click that arrives without a motion in front of it --
    /// or right after the focus moved -- lands where the pointer is rather
    /// than at the origin.
    pointer_at: (i32, i32),
    /// Which surface `enter` was last sent for, per device (`None` == not
    /// currently over/focused), so motion and keys only follow an enter.
    pointer_on: Option<u32>,
    keyboard_on: Option<u32>,
    /// A keyboard was created and owes a `keymap` event whose fd the host must
    /// attach: this is the payload, taken once by [`Client::take_keymap`].
    pending_keymap: Option<Vec<u8>>,
    /// Data devices, sources and offers (data.rs).
    data: data::DataState,
    /// `zwp_relative_pointer_v1` objects, and pointer locks and
    /// confinements by object id (pointer.rs).
    relative_pointers: Vec<u32>,
    constraints: HashMap<u32, pointer::Constraint>,
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
            external_pending: HashMap::new(),
            inbox: Vec::new(),
            fds: Vec::new(),
            outbox: Vec::new(),
            frames: Vec::new(),
            next_serial: 1,
            default_size: (640, 480),
            fullscreen_changes: Vec::new(),
            scale: 1,
            wm_base_version: 0,
            placement_version: 0,
            positioners: HashMap::new(),
            regions: HashMap::new(),
            dirty: false,
            failed: None,
            pointer: None,
            keyboard: None,
            seat_version: 0,
            scroll_rest: [0; 2],
            scroll_notch: [0; 2],
            focus_surface: None,
            pointer_target: None,
            keyboard_target: None,
            cursor_surface: None,
            pointer_at: (0, 0),
            pointer_on: None,
            keyboard_on: None,
            pending_keymap: None,
            data: data::DataState::default(),
            relative_pointers: Vec::new(),
            constraints: HashMap::new(),
        }
    }

    /// The output's size in device pixels: what `wl_output` advertises, and
    /// (less the frame) the room a toplevel is given.
    pub fn set_default_size(&mut self, w: i32, h: i32) {
        self.default_size = (w, h);
    }

    /// The output was resized after this client connected: the new size, and
    /// `mode` then `done` on every `wl_output` it has bound, as a compositor
    /// does when a monitor's mode changes. A client that sizes itself to the
    /// output (Wine's desktop, Xwayland's root) learns of it no other way.
    pub fn set_output_size(&mut self, w: i32, h: i32) {
        if self.default_size == (w, h) {
            return;
        }
        self.default_size = (w, h);
        let mut outputs: Vec<u32> = self
            .objects
            .iter()
            .filter_map(|(o, k)| matches!(k, Object::Output).then_some(*o))
            .collect();
        outputs.sort_unstable();
        for id in outputs {
            // mode(flags=current|preferred, width, height, refresh mHz), done
            self.send(Writer::new(id, 1).uint(0b11).int(w).int(h).int(60000));
            self.send(Writer::new(id, 2));
        }
    }

    /// `configure_bounds`: the output in surface units, less the frame the
    /// host draws around a window (a title bar and a border, `FRAME_*`).
    fn bounds(&self) -> (i32, i32) {
        let (w, h) = self.default_size;
        let k = self.scale.max(1);
        ((w - FRAME_W * k).max(k) / k, (h - FRAME_H * k).max(k) / k)
    }

    /// The surfaces whose fullscreen state changed since the last call, and
    /// the state each is in now: for a host that configures on the change
    /// rather than reading every scene (the native one).
    pub fn take_fullscreen_changes(&mut self) -> Vec<(u32, bool)> {
        std::mem::take(&mut self.fullscreen_changes)
    }

    /// The output scale this client is told (`wl_output.scale`), for a host
    /// whose output is in device pixels on a high-density display. Set before
    /// the client binds the output.
    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.clamp(1, 8);
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

    /// Every descriptor the compositor still needs: a live `wl_shm_pool` or
    /// `wl_buffer` names it, or a surface shows pixels from it. A host that
    /// holds a reference per descriptor received (the page's X server) drops
    /// it once its descriptor is no longer here.
    pub fn fds_in_use(&self) -> std::collections::HashSet<u32> {
        let mut out = std::collections::HashSet::new();
        for o in self.objects.values() {
            match o {
                Object::ShmPool { fd, .. } => {
                    out.insert(*fd);
                }
                Object::Buffer(spec) => {
                    out.insert(spec.fd);
                }
                _ => {}
            }
        }
        out.extend(self.surfaces.values().filter_map(|s| s.shown_fd));
        out.extend(self.receive_fds());
        out
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

    /// `GEX_WL_KEYTRACE=1`: every keyboard object made or released, and every
    /// enter, leave and key sent (a scripted key that a client never acts on).
    fn keytrace(what: &str) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ON.get_or_init(|| std::env::var_os("GEX_WL_KEYTRACE").is_some()) {
            eprintln!("[wl-key] {what}");
        }
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
                Object::XdgPopup { xdg_surface } => {
                    self.xdg_popup(object, xdg_surface, opcode, &mut r)
                }
                Object::Subsurface { surface } => self.subsurface(object, surface, opcode, &mut r),
                Object::DecorationManager => self.decoration_manager(opcode, &mut r),
                Object::Decoration { surface } => self.decoration(object, surface, opcode, &mut r),
                Object::PlacementManager => self.placement_manager(opcode, &mut r),
                Object::Placement { surface } => self.placement(object, surface, opcode, &mut r),
                Object::RelativePointerManager => self.relative_pointer_manager(object, opcode, &mut r),
                Object::RelativePointer => self.relative_pointer(object, opcode),
                Object::PointerConstraints => self.pointer_constraints(object, opcode, &mut r),
                Object::LockedPointer | Object::ConfinedPointer => self.constrained_pointer(object, opcode, &mut r),
                Object::Seat => self.seat(opcode, &mut r),
                Object::DataDeviceManager { version } => self.data_device_manager(version, opcode, &mut r),
                Object::DataDevice => self.data_device(object, opcode, &mut r),
                Object::DataSource => self.data_source(object, opcode, &mut r),
                Object::DataOffer => self.data_offer(object, opcode, &mut r),
                Object::Subcompositor => self.subcompositor(opcode, &mut r),
                Object::Viewporter => self.viewporter(object, opcode, &mut r),
                Object::Viewport { surface } => self.viewport(object, surface, opcode, &mut r),
                Object::Positioner => self.positioner(object, opcode, &mut r),
                Object::Region => self.region(object, opcode, &mut r),
                Object::Buffer(_) => {
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
                        // set_cursor(serial, surface, hotspot_x, hotspot_y).
                        // The compositor draws it: a real pointer image, from
                        // the client, at the hotspot the client chose.
                        0 => {
                            let (_serial, cursor, hx, hy) =
                                (r.uint(), r.object(), r.int(), r.int());
                            let cursor = cursor.unwrap_or(0);
                            if let Some(prev) = self.cursor_surface.take() {
                                if prev != cursor {
                                    if let Some(s) = self.surfaces.get_mut(&prev) {
                                        s.role = Role::None;
                                    }
                                }
                            }
                            if cursor != 0 {
                                if let Some(s) = self.surfaces.get_mut(&cursor) {
                                    s.role = Role::Cursor;
                                    s.rel_x = hx.unwrap_or(0);
                                    s.rel_y = hy.unwrap_or(0);
                                }
                                self.cursor_surface = Some(cursor);
                            }
                            self.dirty = true;
                            true
                        }
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
                        Self::keytrace(&format!("release keyboard {object} (current {:?})", self.keyboard));
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
        let (Some(name), Some(interface), Some(version), Some(id)) =
            (r.uint(), r.string(), r.uint(), r.new_id())
        else {
            return false;
        };
        if Self::tracing() {
            trace_out(&format!("[wl] bind {interface} v{version} as {id}"));
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
                self.seat_version = self.seat_version.max(version);
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
                let ev = Writer::new(id, 3).int(self.scale);
                self.send(ev);
                let ev = Writer::new(id, 2);
                self.send(ev);
                // Every surface is on this output, the only one: said with
                // `wl_surface.enter`, which is how a client learns which
                // output's scale to draw at. Without it foot at scale 2 drew
                // at 1 and was shown at twice the size it meant.
                let surfaces: Vec<u32> = self.surfaces.keys().copied().collect();
                for surface in surfaces {
                    self.send(Writer::new(surface, 0).object(id));
                }
            }
            "xdg_wm_base" => {
                self.objects.insert(id, Object::WmBase);
                self.wm_base_version = version.min(4);
            }
            "wl_data_device_manager" => {
                self.objects.insert(id, Object::DataDeviceManager { version: version.min(3) });
            }
            "wl_subcompositor" => {
                self.objects.insert(id, Object::Subcompositor);
            }
            "wp_viewporter" => {
                self.objects.insert(id, Object::Viewporter);
            }
            "zxdg_decoration_manager_v1" => {
                self.objects.insert(id, Object::DecorationManager);
            }
            "gex_window_placement_manager_v1" => {
                self.objects.insert(id, Object::PlacementManager);
                self.placement_version = version.min(2);
            }
            "zwp_relative_pointer_manager_v1" => {
                self.objects.insert(id, Object::RelativePointerManager);
            }
            "zwp_pointer_constraints_v1" => {
                self.objects.insert(id, Object::PointerConstraints);
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
                let mut state = SurfaceState::default();
                // A client that already said this surface is drawn on the host
                // said so before the request that creates it arrived. The word
                // is for this one creation: ids are reused, and a later surface
                // with this id (a dialog after a GL probe window) is not drawn
                // on the host unless the client says so again.
                state.external = self.external_pending.remove(&id);
                self.surfaces.insert(id, state);
                if state_is_external(&self.surfaces, id) {
                    self.dirty = true;
                }
                // On the output from the start (see the `wl_output` bind).
                let outputs: Vec<u32> = self
                    .objects
                    .iter()
                    .filter_map(|(o, k)| matches!(k, Object::Output).then_some(*o))
                    .collect();
                for output in outputs {
                    self.send(Writer::new(id, 0).object(output));
                }
                true
            }
            // create_region(id)
            1 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Region);
                self.regions.insert(id, Region::default());
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
                // Every place that remembers this surface forgets it, and the
                // input focus most of all: a `leave` naming a destroyed object
                // is a fatal protocol error, and it killed Xwayland the moment
                // a second window appeared -- "unknown object (15), message
                // leave(uo)", and with the X server went the whole session.
                // No leave is sent here: the object is already gone, and the
                // client knows it destroyed it.
                if self.pointer_on == Some(object) {
                    self.pointer_on = None;
                }
                if self.keyboard_on == Some(object) {
                    self.keyboard_on = None;
                }
                if self.pointer_target == Some(object) {
                    self.pointer_target = None;
                }
                if self.keyboard_target == Some(object) {
                    self.keyboard_target = None;
                }
                if self.focus_surface == Some(object) {
                    self.focus_surface = None;
                }
                if self.cursor_surface == Some(object) {
                    self.cursor_surface = None;
                }
                // Out of its parent's child list too, or a destroyed tooltip
                // stays in the scene as a dangling id.
                let parent = self.surfaces.get(&object).map(|s| s.parent).unwrap_or(0);
                if let Some(p) = self.surfaces.get_mut(&parent) {
                    p.children.retain(|c| *c != object);
                }
                self.objects.remove(&object);
                self.surfaces.remove(&object);
                self.delete_id(object);
                self.dirty = true;
                true
            }
            // attach(buffer, x, y). A NULL buffer is not "no change": it
            // unmaps the surface, and it is how a client hides a window
            // without destroying it. Kept apart from "attached nothing this
            // commit", or a hidden window stays on screen forever.
            1 => {
                let Some(buffer) = r.object() else {
                    return false;
                };
                let (_x, _y) = (r.int(), r.int());
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_buffer = if buffer == 0 { None } else { Some(buffer) };
                    s.pending_null = buffer == 0;
                }
                true
            }
            // damage(x, y, width, height), in surface coordinates. What makes
            // an upload cheap: a text cursor blinking in a terminal is a
            // 8x16 rectangle, and uploading the whole window for it is the
            // difference between a compositor that idles and one that does not.
            2 => {
                let (Some(x), Some(y), Some(w), Some(h)) = (r.int(), r.int(), r.int(), r.int())
                else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&object) {
                    // Surface units, and the host's damage is in buffer pixels.
                    let k = s.pending_scale.unwrap_or(s.buffer_scale).max(1);
                    let rect = Damage { x: x * k, y: y * k, w: w * k, h: h * k };
                    s.pending_damage = Some(match s.pending_damage {
                        Some(mut held) => {
                            held.merge(rect);
                            held
                        }
                        None => rect,
                    });
                }
                true
            }
            // frame(callback)
            3 => {
                let Some(id) = r.new_id() else { return false };
                self.objects.insert(id, Object::Callback);
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_callbacks.push(id);
                }
                true
            }
            // set_opaque_region: only an optimisation for a compositor that
            // skips drawing what is under an opaque surface.
            4 => true,
            // set_input_region(region): a copy of the region as it is now (the
            // client may destroy it at once), taking effect at commit. Null
            // is the whole surface again.
            5 => {
                let Some(region) = r.object() else { return false };
                let input = if region == 0 { None } else { Some(self.regions.get(&region).cloned().unwrap_or_default()) };
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_input = Some(input);
                }
                true
            }
            // commit
            6 => {
                self.commit(object, buffers);
                true
            }
            // set_buffer_scale(scale): buffer pixels per surface unit, from
            // the next commit.
            8 => {
                let Some(k) = r.int() else { return false };
                if let Some(s) = self.surfaces.get_mut(&object) {
                    s.pending_scale = Some(k.clamp(1, 8));
                }
                true
            }
            // set_buffer_transform, offset
            7 | 10 => true,
            // damage_buffer(x, y, width, height), in buffer coordinates. The
            // same thing as `damage` at scale 1 and no transform, which is
            // every surface here -- and it is the one toolkits actually use.
            9 => {
                let (Some(x), Some(y), Some(w), Some(h)) = (r.int(), r.int(), r.int(), r.int())
                else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&object) {
                    let rect = Damage { x, y, w, h };
                    s.pending_damage = Some(match s.pending_damage {
                        Some(mut held) => {
                            held.merge(rect);
                            held
                        }
                        None => rect,
                    });
                }
                true
            }
            _ => false,
        }
    }

    /// Apply a surface's pending state, the only moment anything becomes visible.
    fn commit(&mut self, surface: u32, buffers: &dyn Buffers) {
        // The buffer scale is double-buffered state like the rest.
        if let Some(s) = self.surfaces.get_mut(&surface) {
            if let Some(k) = s.pending_scale.take() {
                if k != s.buffer_scale.max(1) {
                    self.dirty = true;
                }
                s.buffer_scale = k;
            }
            if let Some(input) = s.pending_input.take() {
                if input != s.input {
                    self.dirty = true;
                }
                s.input = input;
            }
        }
        let Some(state) = self.surfaces.get(&surface).cloned() else {
            return;
        };
        if state.pending_null {
            // Unmapped: no pixels, so nothing to draw and nothing to hit-test.
            // The role is kept -- the surface is still a toplevel, it is just
            // not on screen -- so a later attach maps it again.
            if let Some(s) = self.surfaces.get_mut(&surface) {
                s.pending_null = false;
                s.committed_buffer = None;
                s.pending_buffer = None;
                s.addr = 0;
                s.shown_fd = None;
            }
            self.dirty = true;
            return;
        }
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
        // A surface with no role is not displayable, and no compositor shows
        // one: a cursor image, a scratch buffer, a surface whose popup was just
        // dismissed. Its buffer is still released below -- a client whose
        // buffer never comes back stops drawing.
        let mut produced = None;
        // The descriptor is recorded whatever the surface's role is -- even
        // none. Rootless Xwayland attaches and commits before its window
        // manager has paired the surface with an X window, and a client's
        // cursor surface is committed before `set_cursor` names it: a
        // descriptor dropped because the role had not arrived yet is a window
        // that never appears. `scene()` is where the role decides what is
        // drawn.
        //
        // The *copy* path below is different: it exists for a host with no
        // view into guest memory, and copying a frame for a surface that is
        // not a window is pure waste.
        let showable = state.role != Role::None;
        if let Some(buffer_id) = buffer_id {
            if let Some(Object::Buffer(spec)) = self.objects.get(&buffer_id).cloned() {
                // The surface is the viewport's source rectangle of the buffer,
                // when the client gave one: the same rows at the same stride,
                // starting further in and fewer of them.
                let spec = Self::crop(spec, state.crop);
                // Where the pixels are, not what they are. `locate` hands back
                // the address of the client's pool in an address space the host
                // can read, so the frame is never copied: the host uploads the
                // damaged rows straight out of the guest's memory. A host with
                // no such view (the native one) says None, and gets a `Frame`
                // with the pixels in it as before.
                let placed = buffers.locate(spec.fd, spec.pool_size).and_then(|(base, len)| {
                    let off = u64::try_from(spec.offset).ok()?;
                    let need = u64::try_from(spec.stride)
                        .ok()?
                        .checked_mul(u64::try_from(spec.height).ok()?)?;
                    // A client that claims more than its pool holds is refused
                    // rather than read: this is a length the guest chose.
                    if spec.stride < spec.width.checked_mul(4)? || off.checked_add(need)? > len {
                        return None;
                    }
                    base.checked_add(off)
                });
                if Self::tracing() {
                    trace_out(&format!(
                        "[wl] buffer {buffer_id}: fd {} pool {} -> addr {:?}, {}x{} stride {}",
                        spec.fd, spec.pool_size, placed, spec.width, spec.height, spec.stride
                    ));
                }
                match placed {
                    Some(addr) => {
                        // The whole surface is damaged when its size changed --
                        // the host's texture is remade -- or when it had no
                        // pixels at all before. NOT when the address changed:
                        // damage is relative to the *surface*, and a
                        // double-buffered client alternates between two
                        // buffers every frame, so treating a new address as
                        // full damage uploads a whole window per frame and
                        // makes damage tracking do nothing.
                        let resized =
                            state.width != spec.width || state.height != spec.height;
                        // A client that attached a buffer and declared no
                        // damage gets a full upload: it is the only safe
                        // reading, and shm clients do it.
                        //
                        // A commit that attaches nothing keeps the buffer it
                        // had, and its pixels are new only where the client
                        // says so: no damage, no change. Taking it as a full
                        // reattach re-uploaded the whole buffer on every
                        // commit that only moved a subsurface or set a
                        // viewport -- a game through winewayland commits its
                        // toplevel twice a frame for its GL subsurface, and
                        // each one was all 4 MiB of the window again.
                        let attached = state.pending_buffer.is_some();
                        let mut damage = match state.pending_damage {
                            Some(d) => Some(d),
                            None if attached => Some(Damage::all()),
                            None => None,
                        };
                        if resized || state.addr == 0 || (!attached && addr != state.addr) {
                            damage = Some(Damage::all());
                        }
                        if let Some(s) = self.surfaces.get_mut(&surface) {
                            s.pending_damage = None;
                        }
                        if let Some(damage) = damage {
                            if let Some(s) = self.surfaces.get_mut(&surface) {
                                s.addr = addr;
                                s.shown_fd = Some(spec.fd);
                                s.stride = spec.stride;
                                s.width = spec.width;
                                s.height = spec.height;
                                s.format = spec.format;
                                s.serial = s.serial.wrapping_add(1);
                                s.damage = Some(match s.damage {
                                    Some(mut held) => {
                                        held.merge(damage);
                                        held
                                    }
                                    None => damage,
                                });
                                s.pending_damage = None;
                            }
                        }
                        // The scene may still have changed (a subsurface
                        // moved, a viewport): it is re-sent, but with the
                        // serial unmoved the host uploads nothing.
                        self.dirty = true;
                    }
                    None if showable => {
                        let mapped = buffers.map(spec.fd, spec.pool_size);
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
                    None => {}
                }
            }
            // A client may not draw into a buffer again until it is released,
            // and most will not draw a second frame without this. Once per
            // attach: a commit that attaches nothing new (a subsurface
            // position, a viewport, an input region) shows the committed
            // buffer again but does not release it again. Wine's
            // winewayland counts releases, and fourteen for one attach
            // freed a buffer still in its queue, which it then destroyed a
            // second time (two games on 32-bit Wine: libwayland's
            // `proxy->refcount > 0` assertion at the first swap-chain resize).
            if state.pending_buffer.is_some() {
                let w = Writer::new(buffer_id, 0);
                self.send(w);
            }
        }
        if let Some(frame) = produced {
            // The host that composites from copies has one window, and it is
            // whichever last drew -- of the windows: a cursor or a subsurface
            // drawing must not take the keyboard (Xwayland's cursor surface
            // did, and every scripted key went to it).
            // Not a placed one: its client says when it has the keyboard
            // (`activate`), and Wine takes a keyboard enter as "make this the
            // foreground window" -- every repaint of a taskbar would.
            //
            // And only when the window maps (its first buffer, or the first
            // after an unmap), or when no window has the keyboard: a window
            // that loses it repaints itself as inactive, and following that
            // repaint handed the keyboard back, so two of Wine's windows
            // passed it between them for ever, each repaint making the other
            // repaint (with an older winewayland).
            let maps = state.committed_buffer.is_none() || self.focus_surface.is_none();
            if matches!(state.role, Role::Toplevel | Role::Popup) && !state.placement.placed && maps {
                self.focus_surface = Some(surface);
                self.ensure_keyboard_enter();
            }
            self.frames.push(frame);
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

    /// A buffer cut down to a `wp_viewport` source rectangle, clipped to the
    /// buffer. A rectangle that leaves nothing is ignored, as the protocol
    /// says a client may not send one.
    fn crop(spec: BufferSpec, crop: Option<(i32, i32, i32, i32)>) -> BufferSpec {
        let Some((x, y, w, h)) = crop else { return spec };
        let (x, y) = (x.clamp(0, spec.width), y.clamp(0, spec.height));
        let (w, h) = (w.min(spec.width - x), h.min(spec.height - y));
        if w <= 0 || h <= 0 || (x, y, w, h) == (0, 0, spec.width, spec.height) {
            return spec;
        }
        BufferSpec { offset: spec.offset + y * spec.stride + x * 4, width: w, height: h, ..spec }
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
            // destroy: the surface loses its role, and a surface with no role
            // is not on screen. Without this a client that recreates its
            // window -- which Qt does on a resize -- left the old one drawn,
            // and a session accumulated empty stacked frames.
            0 => {
                self.unmap(surface);
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
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.role = Role::Toplevel;
                }
                self.dirty = true;
                // A toplevel may not draw until configured, so a configure goes
                // out at once: 0x0, "you choose", within the bounds it carries
                // -- a window opens at the size its program wants, as on any
                // floating desktop. It used to be told the whole output, so
                // every window opened screen-sized.
                self.send_toplevel_configure(id, object, 0, 0, STATE_ACTIVATED);
                true
            }
            // get_popup(id, parent, positioner)
            2 => {
                let (Some(id), parent, Some(pos)) = (r.new_id(), r.object(), r.object()) else {
                    return false;
                };
                self.objects.insert(id, Object::XdgPopup { xdg_surface: object });
                let placed = self.positioners.get(&pos).copied().unwrap_or_default();
                let (x, y) = placed.place();
                // The parent is an xdg_surface, and what the compositor places
                // against is its wl_surface.
                let parent_surface = match parent.and_then(|p| self.objects.get(&p)) {
                    Some(Object::XdgSurface { surface }) => *surface,
                    _ => 0,
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.role = Role::Popup;
                    s.parent = parent_surface;
                    s.rel_x = x;
                    s.rel_y = y;
                }
                self.dirty = true;
                // configure(x, y, width, height), then the xdg_surface configure
                // that makes the pair complete. A popup that gets no configure
                // never attaches a buffer -- which is why menus never appeared.
                let ev = Writer::new(id, 0)
                    .int(x)
                    .int(y)
                    .int(placed.width)
                    .int(placed.height);
                self.send(ev);
                let serial = self.serial();
                let ev = Writer::new(object, 0).uint(serial);
                self.send(ev);
                true
            }
            // set_window_geometry(x, y, width, height): the part of the surface
            // that is the window. GTK leaves invisible margins for its shadows
            // and says so here, and a compositor that decorates or hit-tests
            // the whole surface instead draws a frame around the shadow.
            3 => {
                let (Some(x), Some(y), Some(w), Some(h)) = (r.int(), r.int(), r.int(), r.int())
                else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.geom = Some((x, y, w, h));
                }
                self.dirty = true;
                true
            }
            // ack_configure(serial)
            4 => {
                let serial = r.uint().unwrap_or(0);
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.configured = true;
                    if serial != 0 && serial != s.configure_acked {
                        s.configure_acked = serial;
                        self.dirty = true;
                    }
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
            // destroy: the window is gone, whatever the client does with the
            // wl_surface next.
            0 => {
                if let Some(surface) = self.surface_of_xdg(xdg_surface) {
                    self.unmap(surface);
                }
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_parent(parent)
            1 => true,
            // set_title(title)
            2 => {
                let Some(title) = r.string() else { return false };
                let surface = self.surface_of_xdg(xdg_surface);
                if let Some(s) = surface.and_then(|k| self.surfaces.get_mut(&k)) {
                    s.title = title;
                }
                self.dirty = true;
                true
            }
            // set_app_id(app_id)
            3 => {
                let Some(app_id) = r.string() else { return false };
                let surface = self.surface_of_xdg(xdg_surface);
                if let Some(s) = surface.and_then(|k| self.surfaces.get_mut(&k)) {
                    s.app_id = app_id;
                }
                self.dirty = true;
                true
            }
            // show_window_menu(seat, serial, x, y)
            4 => true,
            // move(seat, serial): drag me. A client that draws its own frames
            // (every GTK app) reports a titlebar drag this way, and it is the
            // only way the compositor can know a CSD window is being moved.
            5 => {
                self.request(xdg_surface, Request::Move);
                true
            }
            // resize(seat, serial, edges): the same for an interactive resize,
            // from the edge or corner of its own frame the client says.
            6 => {
                let (Some(_seat), Some(_serial), Some(edges)) = (r.object(), r.uint(), r.uint()) else {
                    return false;
                };
                self.request(xdg_surface, Request::Resize(edges as u8));
                true
            }
            // set_maximized, unset_maximized: a client's own maximize button.
            9 => {
                self.request(xdg_surface, Request::Maximize);
                true
            }
            10 => {
                self.request(xdg_surface, Request::Unmaximize);
                true
            }
            // set_fullscreen(output), unset_fullscreen: a state the host reads
            // (`SurfaceInfo::fullscreen`). The output is ignored: there is one.
            11 | 12 => {
                let on = opcode == 11;
                if let Some(k) = self.surface_of_xdg(xdg_surface) {
                    if let Some(s) = self.surfaces.get_mut(&k) {
                        s.fullscreen = on;
                    }
                    self.fullscreen_changes.retain(|c| c.0 != k);
                    self.fullscreen_changes.push((k, on));
                }
                self.dirty = true;
                true
            }
            // set_minimized: a client's own minimize button, for the host.
            13 => {
                self.request(xdg_surface, Request::Minimize);
                true
            }
            // set_max_size, set_min_size -- acknowledged and not acted on.
            7 | 8 => true,
            _ => false,
        }
    }


    /// Take a surface off the screen: no role, no pixels, no place in the
    /// scene. What a destroyed role and an unmapped window have in common.
    fn unmap(&mut self, surface: u32) {
        // The pointer and the keyboard leave a surface that stops being
        // mapped -- said now, while the object still exists to be named. A
        // client that unmaps commonly destroys next, and after that naming it
        // is a protocol error.
        if self.pointer_on == Some(surface) {
            if let Some(ptr) = self.pointer {
                let serial = self.serial();
                self.send(Writer::new(ptr, 1).uint(serial).object(surface));
                self.pointer_frame();
            }
            self.pointer_on = None;
        }
        if self.keyboard_on == Some(surface) {
            if let Some(kbd) = self.keyboard {
                let serial = self.serial();
                self.send(Writer::new(kbd, 2).uint(serial).object(surface));
            }
            self.keyboard_on = None;
        }
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.role = Role::None;
            s.addr = 0;
            s.committed_buffer = None;
            s.pending_buffer = None;
            s.xdg = None;
            s.configured = false;
        }
        if self.pointer_target == Some(surface) {
            self.pointer_target = None;
        }
        if self.keyboard_target == Some(surface) {
            self.keyboard_target = None;
        }
        if self.focus_surface == Some(surface) {
            self.focus_surface = None;
        }
        self.dirty = true;
    }

    /// The `wl_surface` an `xdg_surface` gave a role to.
    fn surface_of_xdg(&self, xdg_surface: u32) -> Option<u32> {
        match self.objects.get(&xdg_surface) {
            Some(Object::XdgSurface { surface }) => Some(*surface),
            _ => None,
        }
    }

    /// The `xdg_toplevel` (and its `xdg_surface`) of a surface, if it has one.
    fn toplevel_of_surface(&self, surface: u32) -> Option<(u32, u32)> {
        let xdg = self.surfaces.get(&surface)?.xdg?;
        let toplevel = self.objects.iter().find_map(|(id, o)| match o {
            Object::XdgToplevel { xdg_surface } if *xdg_surface == xdg => Some(*id),
            _ => None,
        })?;
        Some((toplevel, xdg))
    }

    /// `xdg_toplevel.configure` and the `xdg_surface.configure` that completes
    /// it: the size the compositor has decided, and whether this window has the
    /// keyboard. A toplevel may not draw until it has had one.
    fn send_toplevel_configure(
        &mut self,
        toplevel: u32,
        xdg_surface: u32,
        w: i32,
        h: i32,
        states_in: u32,
    ) {
        // The states array is a list of little-endian u32s: 1 maximized,
        // 2 fullscreen, 3 resizing, 4 activated. A window with none of them
        // draws itself as inactive, which is why an app that never gets
        // `activated` looks greyed out and takes no keyboard focus of its own.
        let mut states = Vec::new();
        for (bit, state) in [(STATE_MAXIMIZED, 1u32), (STATE_FULLSCREEN, 2), (STATE_RESIZING, 3), (STATE_ACTIVATED, 4)] {
            if states_in & bit != 0 {
                states.extend_from_slice(&state.to_le_bytes());
            }
        }
        // The room there is, in surface units, before the configure it goes
        // with: a client choosing its own size keeps within it.
        if self.wm_base_version >= 4 {
            let (bw, bh) = self.bounds();
            self.send(Writer::new(toplevel, 2).int(bw).int(bh));
        }
        let ev = Writer::new(toplevel, 0).int(w).int(h).array(&states);
        self.send(ev);
        let serial = self.serial();
        let ev = Writer::new(xdg_surface, 0).uint(serial);
        self.send(ev);
        if let Some(s) = self.surface_of_xdg(xdg_surface).and_then(|k| self.surfaces.get_mut(&k)) {
            s.configure_sent = serial;
        }
        self.dirty = true;
    }

    // -- xdg_positioner -----------------------------------------------------

    /// Where a popup wants to go. Every request here is a field of the
    /// placement, applied when `get_popup` reads it -- so a positioner is
    /// state, not behaviour.
    fn positioner(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        let p = self.positioners.entry(object).or_default();
        match opcode {
            // destroy
            0 => {
                self.positioners.remove(&object);
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_size(width, height)
            1 => match (r.int(), r.int()) {
                (Some(w), Some(h)) => {
                    p.width = w;
                    p.height = h;
                    true
                }
                _ => false,
            },
            // set_anchor_rect(x, y, width, height)
            2 => match (r.int(), r.int(), r.int(), r.int()) {
                (Some(x), Some(y), Some(w), Some(h)) => {
                    p.anchor_rect = (x, y, w, h);
                    true
                }
                _ => false,
            },
            // set_anchor(anchor)
            3 => match r.uint() {
                Some(a) => {
                    p.anchor = a;
                    true
                }
                None => false,
            },
            // set_gravity(gravity)
            4 => match r.uint() {
                Some(g) => {
                    p.gravity = g;
                    true
                }
                None => false,
            },
            // set_constraint_adjustment(adjustment): which way a popup may be
            // flipped or slid to stay on screen. The compositor clamps popups
            // to the output itself, so this needs no state of its own.
            5 => true,
            // set_offset(x, y)
            6 => match (r.int(), r.int()) {
                (Some(x), Some(y)) => {
                    p.offset = (x, y);
                    true
                }
                _ => false,
            },
            // set_reactive, set_parent_size, set_parent_configure (v3)
            7..=9 => true,
            _ => false,
        }
    }

    // -- xdg_popup ----------------------------------------------------------

    /// A menu, a combo box drop-down, a tooltip: a surface placed against its
    /// parent rather than by the compositor.
    fn xdg_popup(
        &mut self,
        object: u32,
        xdg_surface: u32,
        opcode: u16,
        r: &mut Reader<'_>,
    ) -> bool {
        let surface = match self.objects.get(&xdg_surface) {
            Some(Object::XdgSurface { surface }) => *surface,
            _ => return false,
        };
        match opcode {
            // destroy: the popup is gone, and so is the surface's role. Its
            // parent stays mapped -- a menu closing must not take the window
            // with it.
            0 => {
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.role = Role::None;
                    s.committed_buffer = None;
                    s.addr = 0;
                }
                self.dirty = true;
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // grab(seat, serial): the client asks for an implicit grab, so that
            // a click outside the popup dismisses it. Accepted -- the
            // compositor dismisses popups on an outside click anyway, which is
            // the behaviour the grab is for.
            1 => true,
            // reposition(positioner, token)
            2 => {
                let (Some(pos), Some(token)) = (r.object(), r.uint()) else {
                    return false;
                };
                let placed = self.positioners.get(&pos).copied().unwrap_or_default();
                let (x, y) = placed.place();
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.rel_x = x;
                    s.rel_y = y;
                }
                self.dirty = true;
                // repositioned(token), then the configure pair a client waits
                // for before it redraws.
                let w = Writer::new(object, 2).uint(token);
                self.send(w);
                let ev = Writer::new(object, 0)
                    .int(x)
                    .int(y)
                    .int(placed.width)
                    .int(placed.height);
                self.send(ev);
                let serial = self.serial();
                let w = Writer::new(xdg_surface, 0).uint(serial);
                self.send(w);
                true
            }
            _ => false,
        }
    }

    // -- wl_subcompositor / wl_subsurface -----------------------------------

    /// `wp_viewporter`: destroy(0), get_viewport(1).
    // -- wl_region ----------------------------------------------------------

    fn region(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => {
                self.objects.remove(&object);
                self.regions.remove(&object);
                self.delete_id(object);
                true
            }
            // add(x, y, width, height), subtract(x, y, width, height)
            1 | 2 => {
                let (Some(x), Some(y), Some(w), Some(h)) = (r.int(), r.int(), r.int(), r.int()) else {
                    return false;
                };
                let region = self.regions.entry(object).or_default();
                if opcode == 1 {
                    region.add((x, y, w, h));
                } else {
                    region.subtract((x, y, w, h));
                }
                true
            }
            _ => false,
        }
    }

    fn viewporter(&mut self, object: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            1 => {
                let (Some(id), Some(surface)) = (r.new_id(), r.object()) else {
                    return false;
                };
                self.objects.insert(id, Object::Viewport { surface });
                true
            }
            _ => false,
        }
    }

    /// `wp_viewport`: destroy(0), set_source(1), set_destination(2). The
    /// destination is recorded on the surface (-1, -1 unsets it).
    fn viewport(&mut self, object: u32, surface: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            0 => {
                // Destroying the viewport removes its source and destination
                // both (applied at the next commit, as they were set).
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.viewport = None;
                    s.crop = None;
                }
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_source(x, y, width, height), wl_fixed: -1 everywhere unsets.
            1 => {
                let (Some(x), Some(y), Some(w), Some(h)) = (r.int(), r.int(), r.int(), r.int())
                else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.crop = if w < 0 || h < 0 {
                        None
                    } else {
                        Some((x >> 8, y >> 8, w >> 8, h >> 8))
                    };
                }
                true
            }
            2 => {
                let (Some(w), Some(h)) = (r.int(), r.int()) else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.viewport = if w < 0 { None } else { Some((w, h)) };
                }
                true
            }
            _ => false,
        }
    }

    fn subcompositor(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => true,
            // get_subsurface(id, surface, parent)
            1 => {
                let (Some(id), Some(surface), Some(parent)) =
                    (r.new_id(), r.object(), r.object())
                else {
                    return false;
                };
                self.objects.insert(id, Object::Subsurface { surface });
                if let Some(sub) = self.surfaces.get_mut(&surface) {
                    sub.role = Role::Subsurface;
                    sub.parent = parent;
                }
                if let Some(p) = self.surfaces.get_mut(&parent) {
                    if !p.children.contains(&surface) {
                        p.children.push(surface);
                    }
                }
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    /// Part of another surface's window: a tooltip, a GTK popover, the
    /// video plane of a player. Position is relative to the parent, and
    /// stacking is relative to its siblings.
    fn subsurface(
        &mut self,
        object: u32,
        surface: u32,
        opcode: u16,
        r: &mut Reader<'_>,
    ) -> bool {
        match opcode {
            // destroy
            0 => {
                let parent = self.surfaces.get(&surface).map(|s| s.parent).unwrap_or(0);
                if let Some(p) = self.surfaces.get_mut(&parent) {
                    p.children.retain(|c| *c != surface);
                }
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.role = Role::None;
                    s.parent = 0;
                }
                self.dirty = true;
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_position(x, y)
            1 => {
                let (Some(x), Some(y)) = (r.int(), r.int()) else {
                    return false;
                };
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.rel_x = x;
                    s.rel_y = y;
                }
                self.dirty = true;
                true
            }
            // place_above(sibling) / place_below(sibling)
            2 | 3 => {
                let Some(sibling) = r.object() else { return false };
                let parent = self.surfaces.get(&surface).map(|s| s.parent).unwrap_or(0);
                if let Some(p) = self.surfaces.get_mut(&parent) {
                    p.children.retain(|c| *c != surface);
                    let at = p.children.iter().position(|c| *c == sibling);
                    match (at, opcode) {
                        (Some(i), 2) => p.children.insert(i + 1, surface),
                        (Some(i), _) => p.children.insert(i, surface),
                        (None, _) => p.children.push(surface),
                    }
                }
                self.dirty = true;
                true
            }
            // set_sync / set_desync: whether the child's commits wait for the
            // parent's. Every commit here is applied when it arrives, which is
            // what desync means and is indistinguishable for a client that
            // commits both in the same frame -- which is all of them.
            4 | 5 => true,
            _ => false,
        }
    }

    // -- xdg_decoration -----------------------------------------------------

    fn decoration_manager(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => true,
            // get_toplevel_decoration(id, toplevel)
            1 => {
                let (Some(id), Some(toplevel)) = (r.new_id(), r.object()) else {
                    return false;
                };
                let surface = match self.objects.get(&toplevel) {
                    Some(Object::XdgToplevel { xdg_surface }) => {
                        match self.objects.get(xdg_surface) {
                            Some(Object::XdgSurface { surface }) => *surface,
                            _ => return false,
                        }
                    }
                    _ => return false,
                };
                self.objects.insert(id, Object::Decoration { surface });
                // configure(mode): server_side (2). Said immediately, because a
                // client that asked for the decoration is waiting for the
                // answer before it lays anything out.
                let w = Writer::new(id, 0).uint(2);
                self.send(w);
                true
            }
            _ => false,
        }
    }

    fn decoration(&mut self, object: u32, _surface: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => {
                self.objects.remove(&object);
                self.delete_id(object);
                true
            }
            // set_mode(mode) / unset_mode: the answer is server_side either
            // way. A client may ask for client_side and this compositor says no
            // -- which is allowed, and is the point of drawing the frames here.
            1 | 2 => {
                let _ = r.uint();
                let w = Writer::new(object, 0).uint(2);
                self.send(w);
                true
            }
            _ => false,
        }
    }

    // -- gex_window_placement_v1 --------------------------------------------

    fn placement_manager(&mut self, opcode: u16, r: &mut Reader<'_>) -> bool {
        match opcode {
            // destroy
            0 => true,
            // get_placement(id, surface)
            1 => {
                let (Some(id), Some(surface)) = (r.new_id(), r.object()) else {
                    return false;
                };
                let Some(s) = self.surfaces.get_mut(&surface) else {
                    return false;
                };
                s.placement.placed = true;
                // A new placement object has been told no origin yet.
                s.origin = None;
                self.objects.insert(id, Object::Placement { surface });
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    fn placement(&mut self, object: u32, surface: u32, opcode: u16, r: &mut Reader<'_>) -> bool {
        // The surface may be gone before its placement object: the requests
        // are then accepted and change nothing.
        let mut ok = true;
        let apply = |c: &mut Self, f: &mut dyn FnMut(&mut SurfaceState)| {
            if let Some(s) = c.surfaces.get_mut(&surface) {
                f(s);
                c.dirty = true;
            }
        };
        match opcode {
            // destroy: back to the compositor's own layout.
            0 => {
                apply(self, &mut |s| {
                    s.placement = Placement::default();
                    s.origin = None;
                });
                self.objects.remove(&object);
                self.delete_id(object);
            }
            // set_position(x, y)
            1 => match (r.int(), r.int()) {
                (Some(x), Some(y)) => apply(self, &mut |s| {
                    s.rel_x = x;
                    s.rel_y = y;
                }),
                _ => ok = false,
            },
            // set_layer(layer)
            2 => match r.uint() {
                Some(layer) => apply(self, &mut |s| s.placement.layer = layer.min(2)),
                None => ok = false,
            },
            // set_stack_id(id)
            3 => match r.uint() {
                Some(id) => apply(self, &mut |s| s.placement.id = id),
                None => ok = false,
            },
            // place_below(sibling)
            4 => match r.uint() {
                Some(sibling) => apply(self, &mut |s| {
                    s.placement.below = sibling;
                    s.placement.restack = s.placement.restack.wrapping_add(1);
                }),
                None => ok = false,
            },
            // set_unmanaged
            5 => apply(self, &mut |s| s.placement.unmanaged = true),
            // activate
            6 => apply(self, &mut |s| s.placement.activate = s.placement.activate.wrapping_add(1)),
            _ => ok = false,
        }
        ok
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
                Self::keytrace(&format!("get_keyboard {id} (was {:?})", self.keyboard));
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


    // -- what the host draws ------------------------------------------------

    /// Every surface the host should draw, with where its pixels are.
    ///
    /// Only surfaces that have a role and a buffer: a toplevel that has not
    /// drawn yet, a dismissed popup and a scratch surface are all absent, which
    /// is what "not mapped" means in this protocol.
    pub fn scene(&self) -> Vec<SurfaceInfo> {
        let mut out = Vec::new();
        for (id, s) in &self.surfaces {
            let external = s.external.filter(|(w, h)| *w > 0 && *h > 0);
            if s.role == Role::None {
                continue;
            }
            if external.is_none() && (s.addr == 0 || s.width <= 0 || s.height <= 0) {
                continue;
            }
            let (width, height) = external.unwrap_or((s.width, s.height));
            out.push(SurfaceInfo {
                surface: *id,
                role: s.role,
                parent: s.parent,
                rel_x: s.rel_x,
                rel_y: s.rel_y,
                width,
                height,
                addr: s.addr,
                stride: s.stride,
                format: s.format,
                serial: s.serial,
                // With nothing outstanding the value does not matter: the
                // host only reads damage when the serial has moved on.
                damage: s.damage.unwrap_or_else(Damage::all),
                title: s.title.clone(),
                app_id: s.app_id.clone(),
                pid: s.pid,
                request: s.request,
                request_seq: s.request_seq,
                fullscreen: s.fullscreen,
                configure_pending: s.configure_sent != 0 && s.configure_sent != s.configure_acked,
                x11: s.x11,
                xid: s.xid,
                external: external.is_some(),
                buffer_scale: s.buffer_scale.max(1),
                client_scale: self.scale,
                placement: s.placement,
                input: s.input.clone(),
                pointer_lock: self.wants_lock(*id),
            });
        }
        // A stable order, so a host that keys textures by index rather than by
        // surface id is not quietly wrong. HashMap iteration is not an order.
        out.sort_by_key(|s| s.surface);
        out
    }

    /// Whether the scene changed since this was last asked. A pump that
    /// changed nothing should cost the host nothing.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The damage a host has now drawn. Cleared per surface, because a host
    /// that draws a frame and leaves the damage behind uploads it forever.
    pub fn clear_damage(&mut self, surface: u32) {
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.damage = None;
        }
    }

    /// A client asked something of the compositor for this window: recorded
    /// and numbered for the host, which acts on each number once. (It was a
    /// flag nobody cleared, so the host started a drag on every scene read
    /// after a client-decorated window's first move.)
    fn request(&mut self, xdg_surface: u32, request: Request) {
        let surface = self.surface_of_xdg(xdg_surface);
        if let Some(s) = surface.and_then(|k| self.surfaces.get_mut(&k)) {
            s.request = request;
            s.request_seq = s.request_seq.wrapping_add(1);
        }
        self.dirty = true;
    }

    /// Tell a window what size to be, and its states (`STATE_*`: active,
    /// maximized, being resized). This is how a compositor resizes: it
    /// configures, and the client redraws to fit.
    pub fn configure(&mut self, surface: u32, w: i32, h: i32, states: u32) {
        let Some((toplevel, xdg)) = self.toplevel_of_surface(surface) else {
            return;
        };
        self.send_toplevel_configure(toplevel, xdg, w, h, states);
    }

    /// Ask a window to close, the way a titlebar's close button does. The
    /// client decides what that means -- a text editor may put up a dialog --
    /// which is why this is a request and not a kill.
    pub fn close(&mut self, surface: u32) {
        if let Some((toplevel, _)) = self.toplevel_of_surface(surface) {
            let w = Writer::new(toplevel, 1);
            self.send(w);
        }
    }

    /// Dismiss a popup: `xdg_popup.popup_done`, which is what a compositor
    /// sends when a click lands outside an open menu.
    pub fn dismiss_popup(&mut self, surface: u32) {
        let Some(xdg) = self.surfaces.get(&surface).and_then(|s| s.xdg) else {
            return;
        };
        let popup = self.objects.iter().find_map(|(id, o)| match o {
            Object::XdgPopup { xdg_surface } if *xdg_surface == xdg => Some(*id),
            _ => None,
        });
        if let Some(popup) = popup {
            let w = Writer::new(popup, 1);
            self.send(w);
        }
    }

    // -- X11 windows --------------------------------------------------------
    //
    // Rootless Xwayland does not use xdg_shell. It creates a plain
    // `wl_surface` per X window and tells the compositor's X window manager
    // which is which (`WL_SURFACE_ID`), so a surface becomes a window on the
    // manager's word rather than on a role it requested. These are how that
    // word arrives.

    /// Whether this client has a surface with this id at all -- the pairing
    /// message can arrive before the surface does.
    pub fn has_surface(&self, surface: u32) -> bool {
        self.surfaces.contains_key(&surface)
    }

    /// This surface is an X window: a toplevel the compositor decorates, or --
    /// if the X window is override-redirect, which is what a menu is -- a
    /// popup positioned in output coordinates by the client itself.
    /// A client's frames for this surface are drawn by the host, not put in a
    /// buffer. `w`/`h` are what size it draws at; zero forgets the
    /// arrangement.
    pub fn attach_external(&mut self, surface: u32, w: i32, h: i32) {
        // Remembered, not just applied: this arrives by a route of its own --
        // the client tells the compositor directly, through the GL device --
        // and it can beat the `create_surface` request still sitting in the
        // client's own output buffer. So the surface may not exist yet, and
        // the mark waits for it.
        if w > 0 && h > 0 {
            self.external_pending.insert(surface, (w, h));
        } else {
            self.external_pending.remove(&surface);
        }
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.external = if w > 0 && h > 0 { Some((w, h)) } else { None };
            self.dirty = true;
        }
    }

    /// Which X window a surface is, for a host that knows only the drawable.
    pub fn set_xid(&mut self, surface: u32, xid: u32) {
        match self.surfaces.get_mut(&surface) {
            Some(s) => {
                s.xid = xid;
                if s.x11 && s.placement.placed {
                    s.placement.id = xid;
                }
                self.dirty = true;
            }
            None => trace_out(&format!("[wl] xid {xid:#x}: no surface {surface}")),
        }
    }

    pub fn adopt_x11(&mut self, surface: u32, title: &str, override_redirect: bool, x: i32, y: i32) {
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        s.role = if override_redirect { Role::Popup } else { Role::Toplevel };
        s.x11 = true;
        s.parent = 0;
        s.rel_x = x;
        s.rel_y = y;
        // An override-redirect window is placed by its client: where it is,
        // undecorated, above everything -- the same state a client asks for
        // with `gex_window_placement_v1`, so it is drawn and hit-tested by the
        // same code. Named by its X window id, as other X windows would name it.
        s.placement = if override_redirect {
            Placement { placed: true, unmanaged: true, layer: 2, id: s.xid, ..s.placement }
        } else {
            Placement::default()
        };
        if !title.is_empty() {
            s.title = title.to_string();
        } else if s.title.is_empty() {
            s.title = "X11".to_string();
        }
        self.dirty = true;
    }

    /// An X window's application and program, as its client named them:
    /// `WM_CLASS`'s class (`app_id`) and `_NET_WM_PID` (`pid`, 0 unknown).
    pub fn set_x11_identity(&mut self, surface: u32, app_id: &str, pid: u32) {
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        if s.app_id != app_id || s.pid != pid {
            s.app_id = app_id.to_string();
            s.pid = pid;
            self.dirty = true;
        }
    }

    /// The X window is gone or unmapped.
    pub fn forget_x11(&mut self, surface: u32) {
        if self.surfaces.get(&surface).map(|s| s.x11) == Some(true) {
            self.unmap(surface);
            if let Some(s) = self.surfaces.get_mut(&surface) {
                s.x11 = false;
            }
        }
    }

    /// Whether a surface is an X window, which decides where a close or a move
    /// has to be sent: to the X server, not to an xdg_toplevel that does not
    /// exist.
    pub fn is_x11(&self, surface: u32) -> bool {
        self.surfaces.get(&surface).map(|s| s.x11).unwrap_or(false)
    }

    /// Whether a surface is on screen: it has a role and a buffer. For a host
    /// that composites from copied frames (`take_frames`), which `scene` does
    /// not describe.
    pub fn is_mapped(&self, surface: u32) -> bool {
        self.surfaces
            .get(&surface)
            .is_some_and(|s| s.role != Role::None && s.committed_buffer.is_some())
    }

    /// Every placed surface that is on screen: its id, position and placement.
    /// The same state `scene` publishes, for a host without a view into guest
    /// memory.
    pub fn placed(&self) -> Vec<(u32, i32, i32, Placement)> {
        let mut out: Vec<_> = self
            .surfaces
            .iter()
            .filter(|(id, s)| s.placement.placed && self.is_mapped(**id))
            .map(|(id, s)| (*id, s.rel_x, s.rel_y, s.placement))
            .collect();
        out.sort_by_key(|p| p.0);
        out
    }

    /// Where the host has put a placed surface, in output pixels: the point
    /// its surface-local pointer positions are relative to from now on.
    ///
    /// The client places the window, but the host shows it -- a frame or more
    /// after the client asked, and pointer events go on arriving meanwhile,
    /// relative to where the window was *shown*. A client that turns them into
    /// screen positions with the place it last asked for (Wine does: its
    /// window rectangle) gets them wrong by however far the window moved in
    /// between, and a window that follows the pointer (Win32's own caption
    /// drag) overshoots by its last step, then back: it shakes. Told the
    /// origin in order with the pointer events, the client adds the right
    /// one. Only sent when it changes, and only to a client that bound
    /// version 2 (`gex_window_placement_v1.origin`).
    pub fn placement_origin(&mut self, surface: u32, x: i32, y: i32) {
        if self.placement_version < 2 {
            return;
        }
        let Some(obj) = self.objects.iter().find_map(|(id, o)| match o {
            Object::Placement { surface: s } if *s == surface => Some(*id),
            _ => None,
        }) else {
            return;
        };
        let Some(s) = self.surfaces.get_mut(&surface) else { return };
        if s.origin == Some((x, y)) {
            return;
        }
        s.origin = Some((x, y));
        self.send(Writer::new(obj, 0).int(x).int(y));
    }

    /// Whether input can be delivered: the client has a pointer and a
    /// keyboard, and a surface to give them to.
    /// Give the keyboard to a window when none has it.
    ///
    /// Focus follows the last surface that committed a buffer, and a window
    /// whose pixels are all the host's GL never commits one: an X11 GL game
    /// drew for minutes and scripted keys had nowhere to go.
    /// The newest toplevel -- an `xdg_toplevel`, or an X11 window -- takes
    /// focus then.
    pub fn focus_fallback(&mut self) {
        if self.pointer_target.or(self.focus_surface).is_some() {
            return;
        }
        let pick = self
            .surfaces
            .iter()
            .filter(|(_, s)| !s.placement.placed && (s.x11 || matches!(s.role, Role::Toplevel)))
            .map(|(id, _)| *id)
            .max();
        if let Some(id) = pick {
            self.focus_surface = Some(id);
            self.ensure_keyboard_enter();
        }
    }

    pub fn input_ready(&self) -> bool {
        self.pointer.is_some() && self.keyboard.is_some() && self.pointer_target.or(self.focus_surface).is_some()
    }

    /// Where pointer input would go now, and the surfaces there are: for a
    /// host explaining scripted input (`GEX_WL_INPUT`).
    pub fn describe_input(&self) -> String {
        let mut ids: Vec<_> = self.surfaces.keys().copied().collect();
        ids.sort();
        let list: Vec<String> = ids
            .iter()
            .map(|id| {
                let s = &self.surfaces[id];
                format!("{id}:{:?}{}{}{}", s.role, if s.x11 { " x11" } else { "" },
                    if s.committed_buffer.is_some() { " drawn" } else { "" },
                    s.external.map(|(w, h)| format!(" ext {w}x{h}")).unwrap_or_default())
            })
            .collect();
        format!(
            "pointer {:?} on {:?}, target {:?}, focus {:?}; surfaces [{}]",
            self.pointer.is_some(),
            self.pointer_on,
            self.pointer_target,
            self.focus_surface,
            list.join(", ")
        )
    }

    /// Move the pointer to a position on a particular surface, entering it (and
    /// leaving whatever it was on) if the surface changed.
    ///
    /// This is the compositor's hit test speaking: the host knows where every
    /// window is, so it -- not this state machine -- decides which surface the
    /// pointer is over.
    pub fn pointer_to(&mut self, surface: u32, x: i32, y: i32, time: u32) {
        self.pointer_target = Some(surface);
        self.pointer_motion(x, y, time);
    }

    /// The pointer left every surface of this client (it is over another
    /// client's window, or over the compositor's own decorations).
    pub fn pointer_clear(&mut self) {
        self.pointer_target = None;
        let (Some(ptr), Some(old)) = (self.pointer, self.pointer_on.take()) else {
            return;
        };
        let serial = self.serial();
        self.send(Writer::new(ptr, 1).uint(serial).object(old)); // leave
        self.pointer_frame();
    }

    /// Give the keyboard to a surface, or to none.
    pub fn keyboard_focus(&mut self, surface: u32) {
        if surface == 0 {
            self.keyboard_target = None;
            let (Some(kbd), Some(old)) = (self.keyboard, self.keyboard_on.take()) else {
                return;
            };
            let serial = self.serial();
            Self::keytrace(&format!("leave keyboard {kbd} surface {old}"));
            self.send(Writer::new(kbd, 2).uint(serial).object(old)); // leave
            return;
        }
        self.keyboard_target = Some(surface);
        self.ensure_keyboard_enter();
    }

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
        let target = self.pointer_target.or(self.focus_surface);
        let (Some(ptr), Some(surface)) = (self.pointer, target) else {
            return false;
        };
        // A surface the client has destroyed cannot be entered, and the host's
        // hit test can be a frame behind the client.
        if !self.surfaces.contains_key(&surface) {
            self.pointer_target = None;
            return false;
        }
        if self.pointer_on == Some(surface) {
            return true;
        }
        if let Some(old) = self.pointer_on.take() {
            let serial = self.serial();
            self.send(Writer::new(ptr, 1).uint(serial).object(old)); // leave
        }
        let serial = self.serial();
        let (x, y) = self.pointer_at;
        // enter(serial, surface, surface_x, surface_y)
        self.send(
            Writer::new(ptr, 0)
                .uint(serial)
                .object(surface)
                .fixed(Fixed::from_int(x))
                .fixed(Fixed::from_int(y)),
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
        let Some(ptr) = self.pointer else {
            trace_out("[wl] pointer motion before the client asked for a pointer");
            return;
        };
        // Before the enter, so the enter it may trigger carries this position.
        self.pointer_at = (x, y);
        if !self.ensure_pointer_enter() {
            trace_out(&format!(
                "[wl] pointer motion dropped: no surface to enter (target {:?}, focus {:?})",
                self.pointer_target, self.focus_surface
            ));
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

    /// Scroll. `axis` 0 is vertical, 1 horizontal; `value` is a wl_fixed delta
    /// in surface pixels (1/256 px), positive down or right.
    ///
    /// Besides the continuous `axis`, the scroll goes out in wheel units, which
    /// is all Wine's winewayland reads (its `axis` handler is empty): WHEEL_PX
    /// pixels are one notch, 120 value120 units, Windows' WHEEL_DELTA. Pixels
    /// short of a unit are kept for the next scroll, so a trackpad's stream of
    /// small deltas adds up instead of each rounding to nothing. A v8 client
    /// gets `axis_value120` (partial notches, as a Windows precision touchpad
    /// sends); a v5-v7 one `axis_discrete`, once a whole notch has built up.
    pub fn pointer_axis(&mut self, axis: u32, value: i32, time: u32) {
        // A scroll with nowhere to go is lost for good (a wheel test's sum
        // comes up short), so it is said, not dropped quietly.
        let Some(ptr) = self.pointer else {
            trace_out("[wl] scroll dropped: the client has no pointer");
            return;
        };
        if axis > 1 {
            return;
        }
        if self.pointer_on.is_none() {
            trace_out(&format!("[wl] scroll dropped: the pointer is on no surface (axis {axis}, {value}/256 px)"));
            return;
        }
        let a = axis as usize;
        let unit = 256 * WHEEL_PX as i64;
        let rest = self.scroll_rest[a] + value as i64 * 120;
        let v120 = (rest / unit) as i32;
        self.scroll_rest[a] = rest - v120 as i64 * unit;
        if v120 != 0 && self.seat_version >= 8 {
            // axis_value120(axis, value120): before the axis event of its frame.
            self.send(Writer::new(ptr, 9).uint(axis).int(v120));
        } else if v120 != 0 && self.seat_version >= 5 {
            let notch = self.scroll_notch[a] + v120;
            let n = notch / 120;
            self.scroll_notch[a] = notch - n * 120;
            if n != 0 {
                // axis_discrete(axis, discrete)
                self.send(Writer::new(ptr, 8).uint(axis).int(n));
            }
        }
        // axis(time, axis, value)
        self.send(Writer::new(ptr, 4).uint(time).uint(axis).fixed(Fixed(value)));
        self.pointer_frame();
    }

    /// `wl_keyboard.enter` the focus surface if not already focused, priming the
    /// client's xkb state with an empty modifiers event. Returns whether focused.
    fn ensure_keyboard_enter(&mut self) -> bool {
        let target = self.keyboard_target.or(self.focus_surface);
        let (Some(kbd), Some(surface)) = (self.keyboard, target) else {
            return false;
        };
        // A surface the client has destroyed cannot be given the keyboard, and the host's
        // hit test can be a frame behind the client.
        if !self.surfaces.contains_key(&surface) {
            self.keyboard_target = None;
            return false;
        }
        if self.keyboard_on == Some(surface) {
            return true;
        }
        if let Some(old) = self.keyboard_on.take() {
            let serial = self.serial();
            Self::keytrace(&format!("leave keyboard {kbd} surface {old}"));
            self.send(Writer::new(kbd, 2).uint(serial).object(old)); // leave
        }
        let serial = self.serial();
        Self::keytrace(&format!("enter keyboard {kbd} surface {surface}"));
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
        let Some(kbd) = self.keyboard else {
            trace_out("[wl] a key arrived before the client asked for a keyboard");
            return;
        };
        if !self.ensure_keyboard_enter() {
            // Nowhere to send it: the host has not said which surface has the
            // keyboard, and nothing has drawn. Said out loud because a key that
            // vanishes here looks exactly like a client ignoring it.
            trace_out(&format!(
                "[wl] key {key} dropped: no focused surface (target {:?})",
                self.keyboard_target
            ));
            return;
        }
        let serial = self.serial();
        Self::keytrace(&format!("key {key} {} to keyboard {kbd} on {:?} serial {serial} time {time}", pressed as u32, self.keyboard_on));
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

    /// A client with a pointer (13) on the window, its seat bound at `version`.
    fn pointer_at_version(version: u32) -> Client {
        let (mut c, _) = draw(vec![0u8; 16], 2, 2, 8);
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(version).new_id(12).finish(),
            &[],
            &no_pool(),
        );
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &no_pool());
        c.pointer_motion(1, 1, 1);
        let _ = c.take_outgoing();
        c
    }

    /// Scroll as the pointer's (opcode, axis, int) events: value120 (9) and
    /// discrete (8) by their int, axis (4) by its fixed value.
    fn scrolls(c: &mut Client) -> Vec<(u16, u32, i32)> {
        messages(&c.take_outgoing())
            .into_iter()
            .filter(|(o, op, _)| *o == 13 && matches!(*op, 4 | 8 | 9))
            .map(|(_, op, body)| {
                let mut fds = Vec::new();
                let mut r = Reader::new(&body, &mut fds);
                if op == 4 {
                    let _time = r.uint();
                    (op, r.uint().unwrap(), r.fixed().unwrap().0)
                } else {
                    (op, r.uint().unwrap(), r.int().unwrap())
                }
            })
            .collect()
    }

    #[test]
    fn a_scroll_is_wheel_units_and_small_ones_add_up() {
        // v8: a notch (100 px) is value120 120, before its axis event.
        let mut c = pointer_at_version(8);
        c.pointer_axis(0, 100 << 8, 5);
        assert_eq!(scrolls(&mut c), vec![(9, 0, 120), (4, 0, 100 << 8)]);
        // 40 trackpad deltas of 2.5 px up: 100 px, so -120 in all, none lost.
        let mut sum = 0;
        let mut axes = 0;
        for i in 0..40 {
            c.pointer_axis(0, -640, 10 + i);
            for (op, axis, v) in scrolls(&mut c) {
                assert_eq!(axis, 0);
                match op {
                    9 => sum += v,
                    4 => { assert_eq!(v, -640); axes += 1 }
                    _ => panic!("discrete to a v8 client"),
                }
            }
        }
        assert_eq!((sum, axes), (-120, 40));
        // Horizontal keeps its own remainder.
        c.pointer_axis(1, 50 << 8, 60);
        c.pointer_axis(1, 50 << 8, 61);
        let h: i32 = scrolls(&mut c).iter().filter(|e| e.0 == 9).map(|e| { assert_eq!(e.1, 1); e.2 }).sum();
        assert_eq!(h, 120);
        // v5: whole notches only, as axis_discrete, once 100 px have built up.
        let mut c = pointer_at_version(5);
        let mut notches = Vec::new();
        for i in 0..40 {
            c.pointer_axis(0, 640, i);
            notches.extend(scrolls(&mut c).into_iter().filter(|e| e.0 != 4));
        }
        assert_eq!(notches, vec![(8, 0, 1)]);
        // v4: the continuous axis alone.
        let mut c = pointer_at_version(4);
        c.pointer_axis(0, 100 << 8, 5);
        assert_eq!(scrolls(&mut c), vec![(4, 0, 100 << 8)]);
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

    /// A window that never commits a buffer (its pixels are the host's GL)
    /// gets the keyboard from `focus_fallback`, so scripted keys reach it.
    #[test]
    fn a_window_that_never_draws_gets_the_keyboard_by_default() {
        let pool = no_pool();
        let mut c = Client::new();
        let mut go = |c: &mut Client, bytes: Vec<u8>| c.receive(&bytes, &[], &pool);
        go(&mut c, Writer::new(1, 1).new_id(2).finish());
        go(&mut c, Writer::new(2, 0).uint(1).string("wl_compositor").uint(4).new_id(3).finish());
        go(&mut c, Writer::new(2, 0).uint(5).string("xdg_wm_base").uint(3).new_id(5).finish());
        go(&mut c, Writer::new(3, 0).new_id(6).finish());
        go(&mut c, Writer::new(5, 2).new_id(7).object(6).finish());
        go(&mut c, Writer::new(7, 1).new_id(8).finish());
        go(&mut c, Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(12).finish());
        go(&mut c, Writer::new(12, 0).new_id(13).finish()); // get_pointer
        go(&mut c, Writer::new(12, 1).new_id(14).finish()); // get_keyboard
        let _ = c.take_outgoing();
        assert!(!c.input_ready(), "no window has drawn, so none has focus");
        c.focus_fallback();
        assert!(c.input_ready(), "the toplevel takes focus");
        c.keyboard_key(28, true, 10);
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == 14 && *op == 1), "keyboard enter");
        assert!(msgs.iter().any(|(o, op, _)| *o == 14 && *op == 3), "the key");
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
    fn a_commit_with_nothing_attached_does_not_release_the_buffer_again() {
        let px = vec![0u8; 16];
        let (mut c, _) = draw(px.clone(), 2, 2, 8);
        let _ = c.take_outgoing();
        // wl_surface.commit (6) on surface 6, with no attach since the last.
        c.receive(&Writer::new(6, 6).finish(), &[], &Pool(px));
        let msgs = messages(&c.take_outgoing());
        assert!(
            !msgs.iter().any(|(o, op, _)| *o == 10 && *op == 0),
            "no second wl_buffer.release for one attach: {msgs:?}"
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

    /// A pool whose memory the host can *place* rather than copy, which is what
    /// the browser does: the descriptor path, and the one the compositor uses.
    struct Placed(u64, u64);
    impl Buffers for Placed {
        fn map(&self, _fd: u32, size: usize) -> Option<Vec<u8>> {
            Some(vec![0u8; size])
        }
        fn locate(&self, _fd: u32, _size: usize) -> Option<(u64, u64)> {
            Some((self.0, self.1))
        }
    }

    /// A client with a toplevel that has drawn, on a pool the host located
    /// rather than copied. Returns the client and the surface id.
    fn placed_window(w: i32, h: i32, stride: i32) -> (Client, u32) {
        placed_window_at(0x10_0000, w, h, stride)
    }

    /// `placed_window`, its pool at `base`.
    fn placed_window_at(base: u64, w: i32, h: i32, stride: i32) -> (Client, u32) {
        let pool = Placed(base, (stride * h) as u64 + 64);
        let mut c = Client::new();
        let mut go = |c: &mut Client, bytes: Vec<u8>, fds: &[u32]| {
            c.receive(&bytes, fds, &pool);
        };
        go(&mut c, Writer::new(1, 1).new_id(2).finish(), &[]);
        for (name, iface, ver, id) in [
            (1u32, "wl_compositor", 4u32, 3u32),
            (2, "wl_shm", 1, 4),
            (5, "xdg_wm_base", 3, 5),
        ] {
            go(
                &mut c,
                Writer::new(2, 0).uint(name).string(iface).uint(ver).new_id(id).finish(),
                &[],
            );
        }
        go(&mut c, Writer::new(3, 0).new_id(6).finish(), &[]);
        go(&mut c, Writer::new(5, 2).new_id(7).object(6).finish(), &[]);
        go(&mut c, Writer::new(7, 1).new_id(8).finish(), &[]);
        go(&mut c, Writer::new(8, 2).string("a window").finish(), &[]);
        go(&mut c, Writer::new(7, 4).uint(2).finish(), &[]);
        go(&mut c, Writer::new(4, 0).new_id(9).int(stride * h).finish(), &[77]);
        go(
            &mut c,
            Writer::new(9, 0).new_id(10).int(0).int(w).int(h).int(stride).uint(1).finish(),
            &[],
        );
        go(&mut c, Writer::new(6, 1).object(10).int(0).int(0).finish(), &[]);
        go(&mut c, Writer::new(6, 6).finish(), &[]);
        (c, 6)
    }

    #[test]
    fn a_located_buffer_becomes_a_descriptor_and_not_a_copy() {
        let (c, surface) = placed_window(64, 32, 256);
        // No `Frame`: nothing was copied out of the pool.
        assert!(c.frames.is_empty(), "a frame was copied out of the pool");
        let scene = c.scene();
        assert_eq!(scene.len(), 1);
        let s = &scene[0];
        assert_eq!(s.surface, surface);
        assert_eq!((s.width, s.height, s.stride), (64, 32, 256));
        assert_eq!(s.addr, 0x10_0000, "the descriptor points at the pool");
        assert_eq!(s.role, Role::Toplevel);
        assert_eq!(s.title, "a window");
    }

    #[test]
    fn a_pool_above_4_gib_keeps_its_whole_address() {
        // A memory64 process's pool: its low half alone is somewhere else.
        let (c, _) = placed_window_at(0x1_4000_0000, 64, 32, 256);
        assert!(c.frames.is_empty(), "a frame was copied out of the pool");
        let scene = c.scene();
        assert_eq!(scene.len(), 1);
        assert_eq!(scene[0].addr, 0x1_4000_0000);
    }

    #[test]
    fn a_descriptor_is_in_use_while_a_surface_shows_its_pixels() {
        let (mut c, _) = placed_window(64, 32, 256);
        let pool = Placed(0x20_0000, 256 * 32 + 64);
        assert!(c.fds_in_use().contains(&77), "pool 9 and buffer 10 hold it");
        // What Xwayland does: the pool goes at once, the buffer later, while
        // the surface still shows it.
        c.receive(&Writer::new(9, 1).finish(), &[], &pool); // wl_shm_pool.destroy
        c.receive(&Writer::new(10, 0).finish(), &[], &pool); // wl_buffer.destroy
        assert!(c.fds_in_use().contains(&77), "the surface still shows it");
        // A second pool and buffer, attached and committed: the first is free.
        c.receive(&Writer::new(4, 0).new_id(11).int(256 * 32).finish(), &[78], &pool);
        c.receive(
            &Writer::new(11, 0).new_id(12).int(0).int(64).int(32).int(256).uint(1).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(6, 1).object(12).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let used = c.fds_in_use();
        assert!(!used.contains(&77), "nothing needs the first pool: {used:?}");
        assert!(used.contains(&78));
        // Unmapped: a NULL attach and a commit, and the buffer destroyed.
        c.receive(&Writer::new(11, 1).finish(), &[], &pool);
        c.receive(&Writer::new(6, 1).object(0).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        c.receive(&Writer::new(12, 0).finish(), &[], &pool);
        assert!(c.fds_in_use().is_empty(), "{:?}", c.fds_in_use());
    }

    #[test]
    fn a_buffer_past_the_end_of_its_pool_is_not_placed() {
        // The pool is one byte short of what the buffer claims, so there is
        // nothing safe to hand the compositor: a guest chose this length.
        let pool = Placed(0x1000, 63);
        let mut c = Client::new();
        let mut go = |c: &mut Client, bytes: Vec<u8>, fds: &[u32]| c.receive(&bytes, fds, &pool);
        go(&mut c, Writer::new(1, 1).new_id(2).finish(), &[]);
        for (name, iface, ver, id) in [
            (1u32, "wl_compositor", 4u32, 3u32),
            (2, "wl_shm", 1, 4),
            (5, "xdg_wm_base", 3, 5),
        ] {
            go(
                &mut c,
                Writer::new(2, 0).uint(name).string(iface).uint(ver).new_id(id).finish(),
                &[],
            );
        }
        go(&mut c, Writer::new(3, 0).new_id(6).finish(), &[]);
        go(&mut c, Writer::new(5, 2).new_id(7).object(6).finish(), &[]);
        go(&mut c, Writer::new(7, 1).new_id(8).finish(), &[]);
        go(&mut c, Writer::new(4, 0).new_id(9).int(64).finish(), &[77]);
        go(
            &mut c,
            Writer::new(9, 0).new_id(10).int(0).int(4).int(4).int(16).uint(1).finish(),
            &[],
        );
        go(&mut c, Writer::new(6, 1).object(10).int(0).int(0).finish(), &[]);
        go(&mut c, Writer::new(6, 6).finish(), &[]);
        assert!(c.scene().is_empty(), "a buffer larger than its pool was placed");
    }

    #[test]
    fn damage_accumulates_until_the_host_has_drawn_it() {
        // The same pool the window was placed in: a different address would be
        // a different buffer, which is a thing that happens and is not what
        // this test is about.
        let pool = Placed(0x10_0000, 8192);
        let (mut c, surface) = placed_window(32, 32, 128);
        // The first frame of a window is all of it, and until the host has
        // drawn that there is nothing smaller to say.
        assert!(c.scene()[0].damage.is_all());
        c.clear_damage(surface);
        // Two damage rectangles and a commit: the union is what to upload.
        c.receive(&Writer::new(surface, 2).int(2).int(3).int(4).int(5).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 9).int(10).int(10).int(2).int(2).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        let d = c.scene()[0].damage;
        assert_eq!((d.x, d.y, d.w, d.h), (2, 3, 10, 9), "the damage union");
        c.clear_damage(surface);
        assert_eq!(c.scene()[0].damage.w, 0, "drawn damage is not uploaded twice");
    }

    #[test]
    fn a_commit_with_no_attach_and_no_damage_changes_no_pixels() {
        let pool = Placed(0x10_0000, 8192);
        let (mut c, surface) = placed_window(32, 32, 128);
        let first = c.scene()[0].serial;
        c.clear_damage(surface);
        // A commit for a subsurface's position or a viewport: no attach, no
        // damage. The host must have nothing new to upload.
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        assert_eq!(c.scene()[0].serial, first, "a bare commit moved the serial");
        // Damage without an attach: the same buffer, new pixels there.
        c.receive(&Writer::new(surface, 2).int(1).int(1).int(4).int(4).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        let s = &c.scene()[0];
        assert_eq!(s.serial, first.wrapping_add(1));
        assert_eq!((s.damage.x, s.damage.y, s.damage.w, s.damage.h), (1, 1, 4, 4));
        // An attach with no damage is still all of it.
        c.clear_damage(surface);
        c.receive(&Writer::new(surface, 1).object(10).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        let s = &c.scene()[0];
        assert_eq!(s.serial, first.wrapping_add(2));
        assert!(s.damage.is_all());
    }

    #[test]
    fn a_null_buffer_takes_the_window_off_the_screen() {
        let pool = Placed(0x3000, 4096);
        let (mut c, surface) = placed_window(16, 16, 64);
        assert_eq!(c.scene().len(), 1);
        // attach(NULL) then commit: hidden, but still a toplevel.
        c.receive(&Writer::new(surface, 1).object(0).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(surface, 6).finish(), &[], &pool);
        assert!(c.scene().is_empty(), "an unmapped window stayed on screen");
    }

    #[test]
    fn a_host_drawn_mark_is_for_one_surface_not_its_id() {
        let pool = no_pool();
        let mut c = Client::new();
        c.receive(&Writer::new(1, 1).new_id(2).finish(), &[], &pool);
        c.receive(&Writer::new(2, 0).uint(1).string("wl_compositor").uint(4).new_id(3).finish(), &[], &pool);
        // The GL device's word can beat the create_surface it is about.
        c.attach_external(6, 133, 1);
        c.receive(&Writer::new(3, 0).new_id(6).finish(), &[], &pool);
        assert!(state_is_external(&c.surfaces, 6), "the mark that came first was lost");
        // A GL probe window goes away; a dialog's surface gets its id again.
        c.receive(&Writer::new(6, 0).finish(), &[], &pool);
        c.receive(&Writer::new(3, 0).new_id(6).finish(), &[], &pool);
        assert!(!state_is_external(&c.surfaces, 6), "a new surface took an old one's host frames by its id");
        // Marked after its creation and unmarked when its EGL surface goes
        // (eglDestroySurface): the next surface with the id is the client's.
        c.attach_external(6, 133, 1);
        assert!(state_is_external(&c.surfaces, 6));
        c.attach_external(6, 0, 0);
        c.receive(&Writer::new(6, 0).finish(), &[], &pool);
        c.receive(&Writer::new(3, 0).new_id(6).finish(), &[], &pool);
        assert!(!state_is_external(&c.surfaces, 6), "an unmarked id came back marked");
    }

    #[test]
    fn a_destroyed_toplevel_leaves_no_window_behind() {
        let pool = Placed(0x4000, 4096);
        let (mut c, _) = placed_window(16, 16, 64);
        // xdg_toplevel.destroy. Qt does this on a resize, and a window that
        // survived it was drawn forever as an empty frame.
        c.receive(&Writer::new(8, 0).finish(), &[], &pool);
        assert!(c.scene().is_empty(), "a destroyed window stayed in the scene");
    }

    #[test]
    fn a_popup_is_placed_by_its_positioner_and_configured() {
        let pool = Placed(0x5000, 8192);
        let (mut c, _) = placed_window(200, 100, 800);
        c.take_outgoing();
        // create_positioner(20); size 40x20; anchor rect at (5,10) 30x8;
        // anchor = bottom (2); gravity = bottom-right (8).
        c.receive(&Writer::new(5, 1).new_id(20).finish(), &[], &pool);
        c.receive(&Writer::new(20, 1).int(40).int(20).finish(), &[], &pool);
        c.receive(&Writer::new(20, 2).int(5).int(10).int(30).int(8).finish(), &[], &pool);
        c.receive(&Writer::new(20, 3).uint(2).finish(), &[], &pool);
        c.receive(&Writer::new(20, 4).uint(8).finish(), &[], &pool);
        // A second surface with an xdg_surface, made a popup of the first.
        c.receive(&Writer::new(3, 0).new_id(21).finish(), &[], &pool);
        c.receive(&Writer::new(5, 2).new_id(22).object(21).finish(), &[], &pool);
        c.receive(&Writer::new(22, 2).new_id(23).object(7).object(20).finish(), &[], &pool);
        let out = messages(&c.take_outgoing());
        // xdg_popup.configure(x, y, w, h) then xdg_surface.configure(serial).
        let configure = out.iter().find(|(id, op, _)| *id == 23 && *op == 0);
        let (_, _, body) = configure.expect("the popup was never configured");
        let x = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let y = i32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        // Anchor "bottom" is the middle of the bottom edge: (5 + 30/2, 10 + 8);
        // gravity "bottom_right" grows away from it, so the corner is there.
        assert_eq!((x, y), (20, 18), "the popup went somewhere else");
        assert!(
            out.iter().any(|(id, op, _)| *id == 22 && *op == 0),
            "the popup's xdg_surface was not configured, so it will never draw",
        );
        // And it draws: attach a buffer to the popup and it is in the scene.
        c.receive(&Writer::new(9, 0).new_id(24).int(0).int(40).int(20).int(160).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(21, 1).object(24).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(21, 6).finish(), &[], &pool);
        let popup = c.scene().into_iter().find(|s| s.surface == 21).expect("no popup surface");
        assert_eq!(popup.role, Role::Popup);
        assert_eq!(popup.parent, 6, "a popup is placed against its parent");
        assert_eq!((popup.rel_x, popup.rel_y), (20, 18));
    }

    #[test]
    fn a_subsurface_follows_its_parent() {
        let pool = Placed(0x6000, 8192);
        let (mut c, _) = placed_window(200, 100, 800);
        // bind wl_subcompositor (name 7), get_subsurface(child of the window)
        c.receive(
            &Writer::new(2, 0).uint(7).string("wl_subcompositor").uint(1).new_id(30).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(3, 0).new_id(31).finish(), &[], &pool);
        c.receive(&Writer::new(30, 1).new_id(32).object(31).object(6).finish(), &[], &pool);
        c.receive(&Writer::new(32, 1).int(12).int(7).finish(), &[], &pool);
        c.receive(&Writer::new(9, 0).new_id(33).int(0).int(20).int(10).int(80).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(31, 1).object(33).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(31, 6).finish(), &[], &pool);
        let sub = c.scene().into_iter().find(|s| s.surface == 31).expect("no subsurface");
        assert_eq!(sub.role, Role::Subsurface);
        assert_eq!(sub.parent, 6);
        assert_eq!((sub.rel_x, sub.rel_y), (12, 7));
    }

    #[test]
    fn an_input_region_takes_effect_at_commit() {
        let pool = Placed(0x6000, 8192);
        let (mut c, _) = placed_window(200, 100, 800);
        // Wine's GL subsurface: an empty region (create_region, no adds),
        // set as the input region, destroyed at once, then committed.
        c.receive(&Writer::new(3, 1).new_id(40).finish(), &[], &pool);
        c.receive(&Writer::new(6, 5).object(40).finish(), &[], &pool);
        c.receive(&Writer::new(40, 0).finish(), &[], &pool);
        let input = |c: &Client| c.scene().into_iter().find(|s| s.surface == 6).expect("no window").input;
        assert_eq!(input(&c), None, "the region applied before the commit");
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let got = input(&c).expect("no input region after the commit");
        assert!(!got.contains(0, 0));
        let info = c.scene().into_iter().find(|s| s.surface == 6).unwrap();
        assert_eq!(info.flags() & 0x80, 0x80);
        assert_eq!(info.input_words(), [0, 0, 0, 0]);
        // A region with a hole: 0..100 square less a 20x20 in the middle.
        c.receive(&Writer::new(3, 1).new_id(41).finish(), &[], &pool);
        c.receive(&Writer::new(41, 1).int(0).int(0).int(100).int(100).finish(), &[], &pool);
        c.receive(&Writer::new(41, 2).int(40).int(40).int(20).int(20).finish(), &[], &pool);
        c.receive(&Writer::new(6, 5).object(41).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let got = input(&c).unwrap();
        assert!(got.contains(10, 10) && got.contains(99, 50) && !got.contains(50, 50) && !got.contains(100, 0));
        assert_eq!(got.bounds(), (0, 0, 100, 100));
        assert!(!got.is_rect());
        // Null: the whole surface again.
        c.receive(&Writer::new(6, 5).object(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        assert_eq!(input(&c), None);
        assert_eq!(c.scene().into_iter().find(|s| s.surface == 6).unwrap().flags() & 0x80, 0);
    }

    #[test]
    fn a_viewport_records_its_destination() {
        let pool = Placed(0x6000, 8192);
        let (mut c, _) = placed_window(200, 100, 800);
        // bind wp_viewporter (name 9), get_viewport for the window (6),
        // set_destination, then destroy the viewport: Wine's driver does all
        // three, and the connection must survive them.
        c.receive(&Writer::new(2, 0).uint(9).string("wp_viewporter").uint(1).new_id(50).finish(), &[], &pool);
        c.receive(&Writer::new(50, 1).new_id(51).object(6).finish(), &[], &pool);
        c.receive(&Writer::new(51, 2).int(200).int(100).finish(), &[], &pool);
        assert_eq!(c.surfaces.get(&6).and_then(|s| s.viewport), Some((200, 100)));
        c.receive(&Writer::new(51, 2).int(-1).int(-1).finish(), &[], &pool);
        assert_eq!(c.surfaces.get(&6).and_then(|s| s.viewport), None);
        c.receive(&Writer::new(51, 0).finish(), &[], &pool);
        assert!(!c.objects.contains_key(&51), "the viewport outlived its destroy");
        assert!(c.scene().iter().any(|s| s.surface == 6), "the window went away");
    }

    #[test]
    fn a_viewport_source_crops_the_buffer() {
        let pool = Placed(0x10_0000, 800 * 125 + 64);
        // A 200x125 buffer of which Wine shows the top 40 rows: the surface
        // the host draws is 200x40, at the same address and stride.
        let (mut c, _) = placed_window(200, 125, 800);
        c.receive(&Writer::new(2, 0).uint(9).string("wp_viewporter").uint(1).new_id(50).finish(), &[], &pool);
        c.receive(&Writer::new(50, 1).new_id(51).object(6).finish(), &[], &pool);
        c.receive(&Writer::new(51, 1).int(0).int(0).int(200 << 8).int(40 << 8).finish(), &[], &pool);
        c.receive(&Writer::new(6, 1).object(10).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let s = c.scene().into_iter().find(|s| s.surface == 6).expect("the window");
        assert_eq!((s.width, s.height, s.stride), (200, 40, 800));
        assert_eq!(s.addr, 0x10_0000);
        // Off the origin: the address moves in by rows and columns.
        c.receive(&Writer::new(51, 1).int(4 << 8).int(2 << 8).int(100 << 8).int(10 << 8).finish(), &[], &pool);
        c.receive(&Writer::new(6, 1).object(10).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let s = c.scene().into_iter().find(|s| s.surface == 6).expect("the window");
        assert_eq!((s.width, s.height), (100, 10));
        assert_eq!(s.addr, 0x10_0000 + 2 * 800 + 4 * 4);
        // Destroying the viewport drops the crop: the whole buffer again.
        c.receive(&Writer::new(51, 0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 1).object(10).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(6, 6).finish(), &[], &pool);
        let s = c.scene().into_iter().find(|s| s.surface == 6).expect("the window");
        assert_eq!((s.width, s.height, s.addr), (200, 125, 0x10_0000));
    }

    #[test]
    fn a_placed_window_carries_its_position_and_stacking() {
        let pool = Placed(0x6000, 8192);
        let (mut c, _) = placed_window(64, 32, 256);
        assert!(!c.scene()[0].placement.placed, "placed before it asked");
        // bind gex_window_placement_manager_v1 (name 10), get_placement(60, 6).
        c.receive(
            &Writer::new(2, 0).uint(10).string("gex_window_placement_manager_v1").uint(1).new_id(59).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(59, 1).new_id(60).object(6).finish(), &[], &pool);
        c.receive(&Writer::new(60, 1).int(300).int(-20).finish(), &[], &pool);
        c.receive(&Writer::new(60, 2).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(60, 3).uint(0x1004a).finish(), &[], &pool);
        c.receive(&Writer::new(60, 4).uint(0x20010).finish(), &[], &pool);
        c.receive(&Writer::new(60, 5).finish(), &[], &pool);
        c.receive(&Writer::new(60, 6).finish(), &[], &pool);
        assert!(c.failed.is_none(), "{:?}", c.failed);
        let s = &c.scene()[0];
        assert_eq!((s.rel_x, s.rel_y), (300, -20));
        let p = s.placement;
        assert!(p.placed && p.unmanaged);
        assert_eq!((p.layer, p.id, p.below, p.restack, p.activate), (1, 0x1004a, 0x20010, 1, 1));
        assert_eq!(p.record_words(), [0x103, 0x1004a, 0x20010, 1, 1]);
        // Destroyed: back to the compositor's layout.
        c.receive(&Writer::new(60, 0).finish(), &[], &pool);
        assert_eq!(c.scene()[0].placement, Placement::default());
    }

    #[test]
    fn a_placed_window_is_told_its_origin_once_per_change_at_version_2() {
        let pool = Placed(0x6000, 8192);
        for version in [1u32, 2] {
            let (mut c, _) = placed_window(64, 32, 256);
            c.receive(
                &Writer::new(2, 0).uint(10).string("gex_window_placement_manager_v1").uint(version).new_id(59).finish(),
                &[],
                &pool,
            );
            c.receive(&Writer::new(59, 1).new_id(60).object(6).finish(), &[], &pool);
            let _ = c.take_outgoing();
            c.placement_origin(6, 120, 40);
            c.placement_origin(6, 120, 40);
            c.placement_origin(6, 160, 70);
            let origins: Vec<(i32, i32)> = messages(&c.take_outgoing())
                .iter()
                .filter(|(o, op, _)| *o == 60 && *op == 0)
                .map(|m| {
                    let mut fds = Vec::new();
                    let mut r = Reader::new(&m.2, &mut fds);
                    (r.int().unwrap(), r.int().unwrap())
                })
                .collect();
            let want: Vec<(i32, i32)> = if version >= 2 { vec![(120, 40), (160, 70)] } else { vec![] };
            assert_eq!(origins, want, "bound at version {version}");
            // A surface without a placement object is told nothing.
            c.placement_origin(7, 1, 1);
            assert!(messages(&c.take_outgoing()).is_empty());
        }
    }

    #[test]
    fn an_override_redirect_window_is_placed_like_any_other() {
        let (mut c, _) = placed_window(64, 32, 256);
        c.adopt_x11(6, "menu", true, 30, 40);
        c.set_xid(6, 0x400007);
        let s = &c.scene()[0];
        assert_eq!((s.rel_x, s.rel_y), (30, 40));
        assert_eq!(
            s.placement,
            Placement { placed: true, unmanaged: true, layer: 2, id: 0x400007, ..Placement::default() }
        );
        c.adopt_x11(6, "xterm", false, 0, 0);
        assert!(!c.scene()[0].placement.placed);
    }

    #[test]
    fn a_toplevel_decoration_is_answered_server_side() {
        let pool = Placed(0x7000, 4096);
        let (mut c, _) = placed_window(64, 64, 256);
        c.take_outgoing();
        // bind zxdg_decoration_manager_v1 (name 8), get_toplevel_decoration.
        c.receive(
            &Writer::new(2, 0)
                .uint(8)
                .string("zxdg_decoration_manager_v1")
                .uint(1)
                .new_id(40)
                .finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(40, 1).new_id(41).object(8).finish(), &[], &pool);
        let out = messages(&c.take_outgoing());
        let (_, _, body) = out
            .iter()
            .find(|(id, op, _)| *id == 41 && *op == 0)
            .expect("the decoration was never configured");
        let mode = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(mode, 2, "the compositor draws the frames, so: server_side");
    }

    #[test]
    fn a_cursor_drawing_does_not_take_the_keyboard() {
        // Xwayland draws its cursor surface after its window; on a host that
        // composites from copies, the keyboard stayed on the cursor and every
        // key went nowhere.
        let pool = Pool(vec![0u8; 16]);
        let mut c = with_seat();
        c.receive(&Writer::new(12, 0).new_id(13).finish(), &[], &pool); // get_pointer
        c.receive(&Writer::new(12, 1).new_id(14).finish(), &[], &pool); // get_keyboard
        assert_eq!(c.focus_surface, Some(6), "the window has the keyboard");
        c.receive(&Writer::new(3, 0).new_id(20).finish(), &[], &pool); // a surface
        c.receive(&Writer::new(9, 0).new_id(21).int(0).int(2).int(2).int(8).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(13, 0).uint(1).object(20).int(0).int(0).finish(), &[], &pool); // set_cursor
        c.receive(&Writer::new(20, 1).object(21).int(0).int(0).finish(), &[], &pool); // attach
        c.receive(&Writer::new(20, 6).finish(), &[], &pool); // commit
        assert!(c.failed.is_none(), "{:?}", c.failed);
        assert_eq!(c.focus_surface, Some(6), "the cursor took the keyboard");
    }

    #[test]
    fn a_cursor_surface_is_a_cursor_and_not_a_window() {
        let pool = Placed(0x8000, 4096);
        let (mut c, _) = placed_window(64, 64, 256);
        // A seat, a pointer, and a surface handed to set_cursor with a hotspot.
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(50).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(50, 0).new_id(51).finish(), &[], &pool);
        c.receive(&Writer::new(3, 0).new_id(52).finish(), &[], &pool);
        c.receive(&Writer::new(9, 0).new_id(53).int(0).int(24).int(24).int(96).uint(0).finish(), &[], &pool);
        c.receive(&Writer::new(51, 0).uint(1).object(52).int(4).int(3).finish(), &[], &pool);
        c.receive(&Writer::new(52, 1).object(53).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(52, 6).finish(), &[], &pool);
        let cursor = c.scene().into_iter().find(|s| s.surface == 52).expect("no cursor surface");
        assert_eq!(cursor.role, Role::Cursor);
        assert_eq!((cursor.rel_x, cursor.rel_y), (4, 3), "the hotspot");
        // And the window is still the window: a cursor never becomes one.
        assert_eq!(
            c.scene().iter().filter(|s| s.role == Role::Toplevel).count(),
            1,
            "the cursor was counted as a window",
        );
    }

    #[test]
    fn a_surface_carries_its_buffer_scale_and_its_clients_output_scale() {
        // The cursor surface from above, drawn at 2x for a 2x output: the host
        // shows it at width * client_scale / buffer_scale, 1:1 here.
        let pool = Placed(0x8000, 4096);
        let (mut c, _) = placed_window(64, 64, 256);
        c.set_scale(2);
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(50).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(50, 0).new_id(51).finish(), &[], &pool);
        c.receive(&Writer::new(3, 0).new_id(52).finish(), &[], &pool);
        c.receive(&Writer::new(9, 0).new_id(53).int(0).int(24).int(24).int(96).uint(0).finish(), &[], &pool);
        c.receive(&Writer::new(51, 0).uint(1).object(52).int(4).int(3).finish(), &[], &pool);
        c.receive(&Writer::new(52, 1).object(53).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(52, 8).int(2).finish(), &[], &pool); // set_buffer_scale(2)
        let before = c.scene().into_iter().find(|s| s.surface == 52);
        assert!(before.is_none() || before.unwrap().buffer_scale == 1, "pending until commit");
        c.receive(&Writer::new(52, 6).finish(), &[], &pool);
        let s = c.scene().into_iter().find(|s| s.surface == 52).expect("no cursor surface");
        assert_eq!((s.buffer_scale, s.client_scale), (2, 2));
    }

    #[test]
    fn an_x11_window_is_a_window_on_the_managers_word() {
        let pool = Placed(0x9000, 8192);
        let mut c = Client::new();
        let mut go = |c: &mut Client, bytes: Vec<u8>| c.receive(&bytes, &[], &pool);
        go(&mut c, Writer::new(1, 1).new_id(2).finish());
        for (name, iface, ver, id) in
            [(1u32, "wl_compositor", 4u32, 3u32), (2, "wl_shm", 1, 4)]
        {
            go(
                &mut c,
                Writer::new(2, 0).uint(name).string(iface).uint(ver).new_id(id).finish(),
            );
        }
        // A bare surface with a buffer: what rootless Xwayland creates. No
        // role, so it is not on screen -- until the window manager says what
        // it is.
        go(&mut c, Writer::new(3, 0).new_id(6).finish());
        c.receive(&Writer::new(4, 0).new_id(9).int(4096).finish(), &[77], &pool);
        go(&mut c, Writer::new(9, 0).new_id(10).int(0).int(50).int(20).int(200).uint(1).finish());
        go(&mut c, Writer::new(6, 1).object(10).int(0).int(0).finish());
        go(&mut c, Writer::new(6, 6).finish());
        assert!(c.scene().is_empty(), "a surface with no role was on screen");
        assert!(c.has_surface(6));
        c.adopt_x11(6, "xterm", false, 30, 40);
        let s = c.scene();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].role, Role::Toplevel, "an X window is a window");
        assert_eq!(s[0].title, "xterm");
        assert!(s[0].x11);
        assert!(c.is_x11(6));
        // An override-redirect X window is a menu: no frame, placed by the
        // client in output coordinates.
        c.adopt_x11(6, "menu", true, 30, 40);
        assert_eq!(c.scene()[0].role, Role::Popup);
        assert_eq!((c.scene()[0].rel_x, c.scene()[0].rel_y), (30, 40));
        c.forget_x11(6);
        assert!(c.scene().is_empty(), "an unmapped X window stayed on screen");
    }

    #[test]
    fn a_destroyed_surface_is_never_named_in_an_event() {
        let pool = Placed(0xb000, 8192);
        let (mut c, first) = placed_window(64, 64, 256);
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(80).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(80, 0).new_id(81).finish(), &[], &pool);
        // The pointer is on the first window.
        c.pointer_to(first, 5, 5, 10);
        assert!(messages(&c.take_outgoing()).iter().any(|(id, op, _)| *id == 81 && *op == 0));
        // A second window, and the first destroyed while the pointer is on it.
        c.receive(&Writer::new(3, 0).new_id(90).finish(), &[], &pool);
        c.receive(&Writer::new(5, 2).new_id(91).object(90).finish(), &[], &pool);
        c.receive(&Writer::new(91, 1).new_id(92).finish(), &[], &pool);
        c.receive(&Writer::new(9, 0).new_id(93).int(0).int(16).int(16).int(64).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(90, 1).object(93).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(90, 6).finish(), &[], &pool);
        c.receive(&Writer::new(first, 0).finish(), &[], &pool); // wl_surface.destroy
        c.take_outgoing();
        // Moving to the new window must not send a `leave` for the dead one.
        // Xwayland answers that with "unknown object, message leave(uo)" and
        // exits, which took the whole session with it.
        c.pointer_to(90, 3, 3, 20);
        for (id, op, body) in messages(&c.take_outgoing()) {
            if id == 81 && op == 1 {
                let named = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                assert_ne!(named, first, "a leave named a destroyed surface");
            }
        }
        // And the pointer did enter the surviving window.
        assert_eq!(c.pointer_on, Some(90));
    }

    #[test]
    fn a_positioner_anchors_and_grows_the_way_the_protocol_says() {
        // Anchor top-left of a 100x50 rectangle at (10, 20), popup 30x10,
        // gravity top-left: the popup's bottom-right corner lands on the
        // anchor point.
        let p = Positioner {
            width: 30,
            height: 10,
            anchor_rect: (10, 20, 100, 50),
            anchor: 5,
            gravity: 5,
            offset: (0, 0),
        };
        assert_eq!(p.place(), (10 - 30, 20 - 10));
        // No anchor and no gravity: centred on the middle of the rectangle.
        let p = Positioner {
            anchor: 0,
            gravity: 0,
            ..p
        };
        assert_eq!(p.place(), (10 + 50 - 15, 20 + 25 - 5));
        // And the offset is added last.
        let p = Positioner {
            offset: (3, -4),
            ..p
        };
        assert_eq!(p.place(), (10 + 50 - 15 + 3, 20 + 25 - 5 - 4));
    }

    #[test]
    fn input_goes_to_the_surface_the_host_names() {
        let pool = Placed(0xa000, 8192);
        let (mut c, _) = placed_window(64, 64, 256);
        c.receive(
            &Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(60).finish(),
            &[],
            &pool,
        );
        c.receive(&Writer::new(60, 0).new_id(61).finish(), &[], &pool);
        c.take_outgoing();
        // A second window, and the pointer put on it by name: the enter must
        // be for that surface, not for whichever drew last.
        c.receive(&Writer::new(3, 0).new_id(70).finish(), &[], &pool);
        c.receive(&Writer::new(5, 2).new_id(71).object(70).finish(), &[], &pool);
        c.receive(&Writer::new(71, 1).new_id(72).finish(), &[], &pool);
        c.receive(&Writer::new(9, 0).new_id(73).int(0).int(10).int(10).int(40).uint(1).finish(), &[], &pool);
        c.receive(&Writer::new(70, 1).object(73).int(0).int(0).finish(), &[], &pool);
        c.receive(&Writer::new(70, 6).finish(), &[], &pool);
        c.take_outgoing();
        c.pointer_to(70, 3, 4, 100);
        let out = messages(&c.take_outgoing());
        let (_, _, enter) = out
            .iter()
            .find(|(id, op, _)| *id == 61 && *op == 0)
            .expect("no pointer enter");
        let surface = u32::from_le_bytes([enter[4], enter[5], enter[6], enter[7]]);
        assert_eq!(surface, 70, "the enter named the wrong surface");
        // The enter carries the position, so a click with no motion in front
        // of it lands where the pointer is.
        let x = i32::from_le_bytes([enter[8], enter[9], enter[10], enter[11]]);
        assert_eq!(x, 3 * 256, "the enter's x, in 24.8 fixed point");
    }

    #[test]
    fn a_locked_pointer_waits_for_the_host_and_takes_relative_motion() {
        let pool = Placed(0xa000, 8192);
        let (mut c, surface) = placed_window(64, 64, 256);
        let bind = |c: &mut Client, name: u32, iface: &str, id: u32| {
            c.receive(&Writer::new(2, 0).uint(name).string(iface).uint(1).new_id(id).finish(), &[], &pool);
        };
        let idx = |iface: &str| GLOBALS.iter().position(|g| g.0 == iface).unwrap() as u32 + 1;
        c.receive(&Writer::new(2, 0).uint(3).string("wl_seat").uint(5).new_id(60).finish(), &[], &pool);
        c.receive(&Writer::new(60, 0).new_id(61).finish(), &[], &pool);
        bind(&mut c, idx("zwp_relative_pointer_manager_v1"), "zwp_relative_pointer_manager_v1", 80);
        bind(&mut c, idx("zwp_pointer_constraints_v1"), "zwp_pointer_constraints_v1", 81);
        c.receive(&Writer::new(80, 1).new_id(82).object(61).finish(), &[], &pool);
        assert!(!c.scene().iter().any(|s| s.pointer_lock), "no lock asked yet");
        // lock_pointer(id, surface, pointer, region, lifetime oneshot)
        c.receive(&Writer::new(81, 1).new_id(83).object(surface).object(61).object(0).uint(1).finish(), &[], &pool);
        assert!(c.failed.is_none(), "{:?}", c.failed);
        c.take_outgoing();
        let s = c.scene().into_iter().find(|s| s.surface == surface).unwrap();
        assert!(s.pointer_lock && s.flags() < 0, "the lock is in the scene, flags bit 31");
        c.pointer_locked(surface, true);
        c.pointer_relative(3 * 256, -2 * 256, 1000);
        let out = messages(&c.take_outgoing());
        assert!(out.iter().any(|(id, op, _)| *id == 83 && *op == 0), "locked");
        let (_, _, m) = out.iter().find(|(id, op, _)| *id == 82 && *op == 0).expect("relative_motion");
        let w = |i: usize| i32::from_le_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
        assert_eq!((w(4) as u32, w(8), w(12)), (1_000_000, 3 * 256, -2 * 256));
        // A oneshot lock released is done with.
        c.pointer_locked(surface, false);
        assert!(messages(&c.take_outgoing()).iter().any(|(id, op, _)| *id == 83 && *op == 1), "unlocked");
        assert!(!c.scene().iter().any(|s| s.pointer_lock), "a oneshot lock asks no more");
    }

    #[test]
    fn a_new_window_chooses_its_size_within_the_room_there_is() {
        // foot opened four screens wide at scale 2: every toplevel was told
        // the whole output, and no surface was ever said to be on the output,
        // so foot never learned its scale. Now: wl_surface.enter, then a
        // first configure of 0x0 ("you choose") with the room as its bounds.
        let pool = Placed(0x5000, 4096);
        let mut c = Client::new();
        c.set_default_size(2560, 1254);
        c.set_scale(2);
        let mut go = |c: &mut Client, bytes: Vec<u8>| c.receive(&bytes, &[], &pool);
        go(&mut c, Writer::new(1, 1).new_id(2).finish());
        for (name, iface, ver, id) in [
            (1u32, "wl_compositor", 4u32, 3u32),
            (4, "wl_output", 3, 4),
            (5, "xdg_wm_base", 4, 5),
        ] {
            go(&mut c, Writer::new(2, 0).uint(name).string(iface).uint(ver).new_id(id).finish());
        }
        c.take_outgoing();
        go(&mut c, Writer::new(3, 0).new_id(6).finish());
        go(&mut c, Writer::new(5, 2).new_id(7).object(6).finish());
        go(&mut c, Writer::new(7, 1).new_id(8).finish());
        let out = messages(&c.take_outgoing());
        let enter = out.iter().find(|(id, op, _)| *id == 6 && *op == 0).expect("no wl_surface.enter");
        assert_eq!(u32::from_le_bytes(enter.2[..4].try_into().unwrap()), 4, "entered the wrong output");
        let at = |op| out.iter().position(|(id, o, _)| *id == 8 && *o == op);
        let (bounds, configure) = (at(2).expect("no configure_bounds"), at(0).expect("no configure"));
        assert!(bounds < configure, "the bounds come before the configure they go with");
        let word = |m: &[u8], i: usize| i32::from_le_bytes(m[i * 4..i * 4 + 4].try_into().unwrap());
        let b = &out[bounds].2;
        assert_eq!((word(b, 0), word(b, 1)), (1280 - 2, 627 - 27), "the output less the frame, in surface units");
        let cfg = &out[configure].2;
        assert_eq!((word(cfg, 0), word(cfg, 1)), (0, 0), "a new window was given a size instead of choosing one");
    }

    #[test]
    fn a_windows_whole_title_app_id_and_minimize_reach_its_record() {
        // placed_window: toplevel 8 on xdg_surface 7, surface 6.
        let pool = Placed(0x6000, 8192);
        let (mut c, surface) = placed_window(64, 64, 256);
        let title = "Document 1 \u{2014} a title past the record's old 32 bytes";
        c.receive(&Writer::new(8, 2).string(title).finish(), &[], &pool);
        c.receive(&Writer::new(8, 3).string("org.kde.kate").finish(), &[], &pool);
        // set_minimized
        c.receive(&Writer::new(8, 13).finish(), &[], &pool);
        let info = c.scene().into_iter().find(|s| s.surface == surface).unwrap();
        assert_eq!(info.request, Request::Minimize);
        let mut rec = Vec::new();
        push_record(&mut rec, 3, &info);
        assert_eq!(rec.len(), REC);
        let text = |at: usize, n: usize| {
            let b: Vec<u8> = rec[at..at + n / 4].iter().flat_map(|w| w.to_le_bytes()).collect();
            String::from_utf8(b.into_iter().take_while(|&x| x != 0).collect()).unwrap()
        };
        assert_eq!(text(REC_TITLE, REC_TITLE_BYTES), title);
        assert_eq!(text(REC_APP_ID, REC_APP_ID_BYTES), "org.kde.kate");
        assert_eq!(rec[REC_PID], 0, "a Wayland window's program is its connection's");
        assert_eq!(((rec[16] >> 24) & 0x7f) as u32, Request::Minimize.code());
        // An X window's identity, from its window manager.
        c.set_x11_identity(surface, "XTerm", 42);
        let info = c.scene().into_iter().find(|s| s.surface == surface).unwrap();
        assert_eq!((info.app_id.as_str(), info.pid), ("XTerm", 42));
    }

    #[test]
    fn a_title_cut_to_fit_stays_whole_characters() {
        let mut w = [0i32; 2];
        put_text(&mut w, "abcdefg\u{e9}");
        let b: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(&b, b"abcdefg\0", "the two-byte character is left out, not cut");
    }

    #[test]
    fn a_window_asks_to_be_resized_and_the_resize_is_acked() {
        // placed_window: toplevel 8 on xdg_surface 7, surface 6.
        let pool = Placed(0x6000, 8192);
        let (mut c, surface) = placed_window(64, 64, 256);
        let info = |c: &Client| c.scene().into_iter().find(|s| s.surface == surface).unwrap();
        let before = info(&c).request_seq;
        // resize(seat, serial, edges = bottom | right)
        c.receive(&Writer::new(8, 6).object(1).uint(1).uint(2 | 8).finish(), &[], &pool);
        let i = info(&c);
        assert_eq!(i.request, Request::Resize(10));
        assert_eq!(i.request_seq, before.wrapping_add(1), "each request is numbered");
        // set_maximized
        c.receive(&Writer::new(8, 9).finish(), &[], &pool);
        assert_eq!(info(&c).request, Request::Maximize);
        // The host answers with a size, being resized: pending until acked.
        c.take_outgoing();
        c.configure(surface, 300, 200, STATE_ACTIVATED | STATE_RESIZING);
        let out = messages(&c.take_outgoing());
        let (_, _, cfg) = out.iter().find(|(id, op, _)| *id == 8 && *op == 0).expect("no configure");
        let word = |i: usize| u32::from_le_bytes(cfg[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!((word(0), word(1)), (300, 200));
        assert_eq!(word(2), 8, "two states");
        assert_eq!((word(3), word(4)), (3, 4), "resizing, then activated");
        let (_, _, serial) = out.iter().find(|(id, op, _)| *id == 7 && *op == 0).expect("no xdg_surface.configure");
        assert!(info(&c).configure_pending, "sent and not acked");
        c.receive(&Writer::new(7, 4).uint(u32::from_le_bytes(serial[..4].try_into().unwrap())).finish(), &[], &pool);
        assert!(!info(&c).configure_pending, "acked");
    }

    #[test]
    fn a_fullscreen_window_is_a_state_the_host_reads_and_configures() {
        // placed_window: toplevel 8 on xdg_surface 7, surface 6.
        let pool = Placed(0x6000, 8192);
        let (mut c, surface) = placed_window(64, 64, 256);
        let info = |c: &Client| c.scene().into_iter().find(|s| s.surface == surface).unwrap();
        assert!(!info(&c).fullscreen);
        assert_eq!(info(&c).flags() & 1, 0, "a new window is not fullscreen");
        let seq = info(&c).request_seq;
        // set_fullscreen(output = null)
        c.receive(&Writer::new(8, 11).uint(0).finish(), &[], &pool);
        let i = info(&c);
        assert!(i.fullscreen);
        assert_eq!(i.flags() & 1, 1, "flags bit 0");
        assert_eq!(i.request_seq, seq, "a state, not a numbered request");
        // The host answers with the whole output and the fullscreen state.
        c.take_outgoing();
        c.configure(surface, 1280, 720, STATE_ACTIVATED | STATE_FULLSCREEN);
        let out = messages(&c.take_outgoing());
        let (_, _, cfg) = out.iter().find(|(id, op, _)| *id == 8 && *op == 0).expect("no configure");
        let word = |i: usize| u32::from_le_bytes(cfg[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!((word(0), word(1)), (1280, 720));
        assert_eq!(word(2), 8, "two states");
        assert_eq!((word(3), word(4)), (2, 4), "fullscreen, then activated");
        assert_eq!(c.take_fullscreen_changes(), vec![(surface, true)]);
        // unset_fullscreen
        c.receive(&Writer::new(8, 12).finish(), &[], &pool);
        assert!(!info(&c).fullscreen);
        assert_eq!(c.take_fullscreen_changes(), vec![(surface, false)]);
        assert!(c.take_fullscreen_changes().is_empty(), "taken once");
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

    /// After `with_seat`, a data device (21) from a manager bound at `version`.
    fn with_data_device(version: u32) -> Client {
        let mut c = with_seat();
        c.receive(
            &Writer::new(2, 0).uint(6).string("wl_data_device_manager").uint(version).new_id(20).finish(),
            &[],
            &no_pool(),
        );
        c.receive(&Writer::new(20, 1).new_id(21).object(12).finish(), &[], &no_pool());
        let _ = c.take_outgoing();
        c
    }

    #[test]
    fn a_host_drag_offers_its_types_enters_and_drops_once_accepted() {
        let mut c = with_data_device(2);
        assert!(c.has_data_device());
        assert!(c.drag_enter(6, 3, 4, &["text/uri-list"], data::DND_COPY));
        let msgs = messages(&c.take_outgoing());
        let offer = 0xff00_0000;
        assert_eq!((msgs[0].0, msgs[0].1), (21, 0), "data_offer first");
        assert!(msgs.iter().any(|(o, op, _)| *o == offer && *op == 0), "offer(mime)");
        assert!(!msgs.iter().any(|(o, op, _)| *o == offer && *op == 1), "no source_actions below v3");
        let enter = msgs.iter().find(|(o, op, _)| *o == 21 && *op == 1).expect("enter");
        let mut fds = Vec::new();
        let mut r = Reader::new(&enter.2, &mut fds);
        let serial = r.uint().unwrap();
        assert_eq!(r.object(), Some(6));
        assert_eq!(r.fixed().map(|f| f.to_int()), Some(3));
        assert_eq!(r.fixed().map(|f| f.to_int()), Some(4));
        assert_eq!(r.object(), Some(offer));
        assert!(!c.drag_status().unwrap().takes());
        c.receive(&Writer::new(offer, 0).uint(serial).string("text/uri-list").finish(), &[], &no_pool());
        assert!(c.drag_status().unwrap().takes());
        c.receive(&Writer::new(offer, 1).string("text/uri-list").finish(), &[55], &no_pool());
        assert!(c.fds_in_use().contains(&55), "a receive's fd is held until the host takes it");
        assert_eq!(c.take_receives(), vec![data::Receive { offer, mime: "text/uri-list".into(), fd: 55 }]);
        c.drag_motion(5, 6, 7);
        assert!(messages(&c.take_outgoing()).iter().any(|(o, op, _)| *o == 21 && *op == 3), "motion");
        assert!(c.drag_drop());
        assert!(messages(&c.take_outgoing()).iter().any(|(o, op, _)| *o == 21 && *op == 4), "drop");
        c.receive(&Writer::new(offer, 2).finish(), &[], &no_pool());
        let st = c.drag_status().unwrap();
        assert!(st.dropped && st.finished, "{st:?}");
        assert!(c.failed.is_none(), "{:?}", c.failed);
    }

    #[test]
    fn a_drop_the_client_does_not_take_is_a_leave() {
        let mut c = with_data_device(2);
        c.drag_enter(6, 0, 0, &["text/uri-list"], data::DND_COPY);
        let serial = c.drag_serial().unwrap();
        c.receive(&Writer::new(0xff00_0000, 0).uint(serial).string("").finish(), &[], &no_pool());
        let _ = c.take_outgoing();
        assert!(!c.drag_drop());
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == 21 && *op == 2), "leave");
        assert!(!msgs.iter().any(|(o, op, _)| *o == 21 && *op == 4), "no drop");
        assert!(c.drag_status().is_none());
    }

    #[test]
    fn a_version_3_client_chooses_an_action() {
        let mut c = with_data_device(3);
        c.drag_enter(6, 0, 0, &["text/uri-list"], data::DND_COPY | data::DND_MOVE);
        let offer = 0xff00_0000;
        let msgs = messages(&c.take_outgoing());
        assert!(msgs.iter().any(|(o, op, _)| *o == offer && *op == 1), "source_actions");
        let serial = c.drag_serial().unwrap();
        c.receive(&Writer::new(offer, 0).uint(serial).string("text/uri-list").finish(), &[], &no_pool());
        assert!(!c.drag_status().unwrap().takes(), "no action yet");
        c.receive(&Writer::new(offer, 4).uint(data::DND_COPY | data::DND_MOVE).uint(data::DND_MOVE).finish(), &[], &no_pool());
        let msgs = messages(&c.take_outgoing());
        let action = msgs.iter().find(|(o, op, _)| *o == offer && *op == 2).expect("action");
        assert_eq!(u32::from_le_bytes(action.2[..4].try_into().unwrap()), data::DND_MOVE);
        assert!(c.drag_status().unwrap().takes());
        assert!(c.drag_drop());
        c.receive(&Writer::new(offer, 3).finish(), &[], &no_pool());
        assert!(c.drag_status().unwrap().finished);
    }

    #[test]
    fn a_source_and_the_selection_are_kept() {
        let mut c = with_data_device(3);
        c.receive(&Writer::new(20, 0).new_id(22).finish(), &[], &no_pool());
        c.receive(&Writer::new(22, 0).string("text/plain").finish(), &[], &no_pool());
        c.receive(&Writer::new(21, 1).object(22).uint(1).finish(), &[], &no_pool());
        let (sources, sel) = c.data_sources();
        assert_eq!(sources[&22].mimes, vec!["text/plain".to_string()]);
        assert_eq!(sel, Some(22));
        c.receive(&Writer::new(22, 1).finish(), &[], &no_pool());
        assert_eq!(c.data_sources().1, None);
        assert!(c.failed.is_none());
    }

    #[test]
    fn an_output_resize_reaches_every_bound_output() {
        let mut c = Client::new();
        c.set_default_size(1300, 713);
        let mut go = |c: &mut Client, bytes: Vec<u8>| c.receive(&bytes, &[], &no_pool());
        go(&mut c, Writer::new(1, 1).new_id(2).finish());
        go(&mut c, Writer::new(2, 0).uint(4).string("wl_output").uint(3).new_id(4).finish());
        go(&mut c, Writer::new(2, 0).uint(4).string("wl_output").uint(3).new_id(9).finish());
        c.take_outgoing();
        let mode = |out: &[(u32, u16, Vec<u8>)], id: u32| -> Vec<(i32, i32)> {
            out.iter()
                .filter(|(o, op, _)| *o == id && *op == 1)
                .map(|(_, _, b)| {
                    let w = |i: usize| i32::from_le_bytes(b[i..i + 4].try_into().unwrap());
                    (w(4), w(8))
                })
                .collect()
        };
        c.set_output_size(1000, 650);
        let out = messages(&c.take_outgoing());
        for id in [4, 9] {
            assert_eq!(mode(&out, id), vec![(1000, 650)], "output {id}'s mode");
            let m = out.iter().position(|(o, op, _)| *o == id && *op == 1).unwrap();
            let d = out.iter().position(|(o, op, _)| *o == id && *op == 2).expect("no done");
            assert!(d > m, "done before the mode");
        }
        c.set_output_size(1000, 650);
        assert!(c.take_outgoing().is_empty(), "the same size again says nothing");
        // A released output is told nothing; one bound later is told the new size.
        c.receive(&Writer::new(9, 0).finish(), &[], &no_pool());
        c.take_outgoing();
        c.set_output_size(1300, 800);
        let out = messages(&c.take_outgoing());
        assert_eq!(mode(&out, 4), vec![(1300, 800)]);
        assert!(mode(&out, 9).is_empty(), "a released output was told");
        c.receive(&Writer::new(2, 0).uint(4).string("wl_output").uint(3).new_id(10).finish(), &[], &no_pool());
        assert_eq!(mode(&messages(&c.take_outgoing()), 10), vec![(1300, 800)]);
    }
}
