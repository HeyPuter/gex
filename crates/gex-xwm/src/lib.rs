//! The X window manager, as protocol.
//!
//! Rootless Xwayland hands each X window to its compositor as a `wl_surface`,
//! but only once a window manager has taken `SubstructureRedirect` *and*
//! redirected the window with the Composite extension -- so a compositor that
//! wants X clients has to be their window manager too. This is that manager,
//! spoken directly over the X11 wire protocol: no Xlib, no xcb, no second
//! process. It is a crate rather than part of a shim because there are two
//! places it has to run from -- a guest process's shim, when Xwayland is a
//! guest program, and the page's Worker, when Xwayland is a wasm module beside
//! the compositor.

use gex_kernel::vfs::FileHandle;
use gex_wayland::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Where this manager's notes go. The browser shim points it at the process's
/// stderr (so they land in the page's terminal beside the compositor's own
/// lines); the host's Worker points it at the console. Unset, they go nowhere,
/// which is what a unit test wants.
static SINK: std::sync::Mutex<Option<fn(&str)>> = std::sync::Mutex::new(None);

/// Say where the manager's notes should go.
pub fn set_trace_sink(f: fn(&str)) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// A note from the manager. `unsafe` and named for the shim's own writer,
/// because every call site here was written against that and this is not the
/// place to churn them.
///
/// # Safety
/// None required; the signature matches the shim's writer it replaced.
pub unsafe fn write_stderr(s: &str) {
    if let Some(f) = *SINK.lock().unwrap_or_else(|e| e.into_inner()) {
        f(s);
    }
}

/// Something a person must see whatever the tracing settings.
pub fn warn_line(s: &str) {
    unsafe { write_stderr(s) }
}

/// Where Xwayland binds, and so where the window manager connects. Both names:
/// an X server binds the filesystem socket and the abstract one, and which of
/// them the name table holds depends on which the server got to first.
/// The abstract name first, because that is the one an X client tries first
/// (libxcb asks for `@/tmp/.X11-unix/X0` before the filesystem path) and so the
/// one the server is certainly listening on.
const X_SOCKETS: &[&str] = &["\0/tmp/.X11-unix/X0", "/tmp/.X11-unix/X0"];

// -- the protocol, as bytes -------------------------------------------------

mod op {
    pub const CREATE_WINDOW: u8 = 1;
    pub const CHANGE_WINDOW_ATTRIBUTES: u8 = 2;
    pub const MAP_WINDOW: u8 = 8;
    pub const CONFIGURE_WINDOW: u8 = 12;
    pub const INTERN_ATOM: u8 = 16;
    pub const CHANGE_PROPERTY: u8 = 18;
    pub const GET_PROPERTY: u8 = 20;
    pub const SET_SELECTION_OWNER: u8 = 22;
    pub const SEND_EVENT: u8 = 25;
    pub const SET_INPUT_FOCUS: u8 = 42;
    pub const QUERY_EXTENSION: u8 = 98;
}

/// The Composite extension's requests, by minor opcode.
mod composite {
    pub const QUERY_VERSION: u8 = 0;
    pub const REDIRECT_WINDOW: u8 = 1;
    /// `CompositeRedirectManual`: the window's contents go to an offscreen
    /// pixmap and the compositor is responsible for putting them on screen.
    pub const MANUAL: u8 = 1;
}

mod ev {
    pub const CREATE_NOTIFY: u8 = 16;
    pub const DESTROY_NOTIFY: u8 = 17;
    pub const UNMAP_NOTIFY: u8 = 18;
    pub const MAP_NOTIFY: u8 = 19;
    pub const MAP_REQUEST: u8 = 20;
    pub const CONFIGURE_NOTIFY: u8 = 22;
    pub const CONFIGURE_REQUEST: u8 = 23;
    pub const PROPERTY_NOTIFY: u8 = 28;
    pub const CLIENT_MESSAGE: u8 = 33;
}

/// `SubstructureRedirect | SubstructureNotify | PropertyChange`: be the window
/// manager, hear about every child of the root, and hear title changes.
const ROOT_EVENT_MASK: u32 = 0x0010_0000 | 0x0008_0000 | 0x0040_0000;

/// `ConfigureWindow` value bits.
const CW_X: u16 = 1;
const CW_Y: u16 = 2;
const CW_WIDTH: u16 = 4;
const CW_HEIGHT: u16 = 8;

/// The atoms this manager needs, interned in this order at startup.
const ATOM_NAMES: &[&str] = &[
    "WL_SURFACE_ID",
    "WM_PROTOCOLS",
    "WM_DELETE_WINDOW",
    "WM_TAKE_FOCUS",
    "WM_STATE",
    "_NET_WM_NAME",
    "UTF8_STRING",
    "_NET_SUPPORTING_WM_CHECK",
    "_NET_SUPPORTED",
    "_NET_WM_WINDOW_TYPE",
    "_NET_WM_WINDOW_TYPE_NORMAL",
    // The window manager's manager selection. Owning it is how a manager
    // announces itself under ICCCM -- and for Xwayland started with `-wm` it is
    // the *gate*: the server does not listen for X clients at all until its
    // manager takes this selection, so that no client ever meets an unmanaged
    // server. Without it the display exists and accepts nothing, which looks
    // exactly like a display nobody connected to.
    "WM_S0",
];

#[derive(Default, Clone, Copy)]
struct Atoms {
    wl_surface_id: u32,
    wm_protocols: u32,
    wm_delete_window: u32,
    wm_take_focus: u32,
    wm_state: u32,
    net_wm_name: u32,
    utf8_string: u32,
    net_supporting_wm_check: u32,
    net_supported: u32,
    net_wm_window_type: u32,
    net_wm_window_type_normal: u32,
    wm_s0: u32,
}

impl Atoms {
    fn set(&mut self, i: usize, atom: u32) {
        match i {
            0 => self.wl_surface_id = atom,
            1 => self.wm_protocols = atom,
            2 => self.wm_delete_window = atom,
            3 => self.wm_take_focus = atom,
            4 => self.wm_state = atom,
            5 => self.net_wm_name = atom,
            6 => self.utf8_string = atom,
            7 => self.net_supporting_wm_check = atom,
            8 => self.net_supported = atom,
            9 => self.net_wm_window_type = atom,
            10 => self.net_wm_window_type_normal = atom,
            11 => self.wm_s0 = atom,
            _ => {}
        }
    }
}

/// What a reply belongs to. X11 answers in order, but only *some* requests have
/// replies, so a queue of the ones that do is how a reply is identified.
#[derive(Clone, Copy)]
enum Awaiting {
    Atom(usize),
    /// The `WM_PROTOCOLS` list of a window: which ICCCM messages it accepts.
    Protocols(u32),
    /// The Composite extension's major opcode.
    Composite,
    /// An `_NET_WM_NAME` read for this window.
    Title(u32),
    /// A `WM_NAME` read, tried when `_NET_WM_NAME` was not set. xterm and the
    /// Athena programs set only this one.
    LegacyTitle(u32),
}

/// One X window the manager knows about.
#[derive(Default, Clone)]
struct XWindow {
    /// Whether `WM_PROTOCOLS` lists `WM_TAKE_FOCUS`. Sending that message to a
    /// client that did not ask for it is not merely useless: xterm answers one
    /// by tearing down its session -- it kills its process group, which from
    /// the outside looks like a terminal whose shell died one command in. The
    /// ICCCM is explicit that a manager sends it only to clients that
    /// advertise it, and this is why.
    takes_focus: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    override_redirect: bool,
    mapped: bool,
    /// The `wl_surface` Xwayland told us this window draws into.
    surface: Option<u32>,
    title: String,
    /// Told to the compositor already, so a pump that changes nothing is quiet.
    announced: bool,
}

pub struct Xwm {
    sock: Arc<dyn FileHandle>,
    /// Bytes read and not yet a whole message.
    inbox: Vec<u8>,
    /// Bytes to write that the socket would not take yet.
    outbox: Vec<u8>,
    /// The setup reply is variable length and arrives before anything else.
    setup_done: bool,
    root: u32,
    id_base: u32,
    id_mask: u32,
    next_id: u32,
    atoms: Atoms,
    atoms_done: usize,
    /// Replies still owed, each with the sequence number of the request that
    /// will answer it. Matched by that number and not by arrival order: a
    /// request whose reply nobody wanted (Composite's QueryVersion, sent for
    /// the version handshake) would otherwise be handed to the next waiter and
    /// every reply after it would belong to the wrong request.
    awaiting: std::collections::VecDeque<(u16, Awaiting)>,
    /// The sequence number of the last request sent. X numbers them from one,
    /// per connection, and a reply carries the low 16 bits of its request's.
    seq: u16,
    windows: HashMap<u32, XWindow>,
    /// `WL_SURFACE_ID` can arrive before the surface exists on the Wayland
    /// connection; the pairing is retried until it does.
    pending_pairs: Vec<(u32, u32)>,
    /// Windows already redirected. Redirecting twice is `BadAccess`, and a
    /// window can be met twice (CreateNotify, and again at MapRequest for one
    /// that existed before the manager did). A window id leaves this set when
    /// the window is destroyed: the X server hands the same id to the next
    /// client that takes that client's slot, and a new window wrongly taken
    /// for redirected is never redirected -- and rootless Xwayland makes no
    /// `wl_surface` for a window that is not. That was a second glxgears,
    /// started after the first had exited, running at full rate with no
    /// window.
    redirected: std::collections::HashSet<u32>,
    check_window: u32,
    /// Composite's major opcode, once the server has answered for it. Zero
    /// until then, and redirection is what makes rootless Xwayland produce a
    /// surface at all -- see `redirect`.
    composite: u8,
    /// Pumps spent waiting for the setup reply.
    waited: u32,
    failed: bool,
}

impl Xwm {
    /// Start managing the X server on an already-open connection.
    ///
    /// The caller opens the socket because where it is depends on the shape of
    /// the run: in a guest process it is a unix socket in the global kernel that
    /// Xwayland bound, and in the page's Worker -- where the X server is a wasm
    /// module in that same Worker -- it is a queue with no kernel in it at all.
    /// What arrives is the X protocol either way.
    pub fn start(sock: Arc<dyn FileHandle>) -> Option<Xwm> {
        let mut xwm = Xwm {
            sock,
            inbox: Vec::new(),
            outbox: Vec::new(),
            setup_done: false,
            root: 0,
            id_base: 0,
            id_mask: 0,
            next_id: 0,
            atoms: Atoms::default(),
            atoms_done: 0,
            awaiting: std::collections::VecDeque::new(),
            seq: 0,
            windows: HashMap::new(),
            pending_pairs: Vec::new(),
            redirected: std::collections::HashSet::new(),
            check_window: 0,
            composite: 0,
            waited: 0,
            failed: false,
        };
        // The connection setup: little-endian, X11 revision 0, no
        // authorisation -- Xwayland is started `-ac`, which is what a
        // compositor that owns its X server does.
        let mut setup = Vec::with_capacity(12);
        setup.push(b'l');
        setup.push(0);
        setup.extend_from_slice(&11u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        xwm.write(setup);
        Some(xwm)
    }


    pub fn failed(&self) -> bool {
        self.failed
    }

    fn write(&mut self, bytes: Vec<u8>) {
        self.outbox.extend_from_slice(&bytes);
    }

    fn flush(&mut self) {
        while !self.outbox.is_empty() {
            match self.sock.write_at(0, &self.outbox) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    self.outbox.drain(..n as usize);
                }
            }
        }
    }

    fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.id_base | (self.next_id & self.id_mask)
    }

    // -- requests ----------------------------------------------------------

    fn request(&mut self, opcode: u8, detail: u8, body: &[u8]) {
        // Length counts 4-byte units, header included, and the body is padded.
        let mut msg = Vec::with_capacity(4 + body.len() + 3);
        let padded = (body.len() + 3) & !3;
        let units = (4 + padded) / 4;
        msg.push(opcode);
        msg.push(detail);
        msg.extend_from_slice(&(units as u16).to_le_bytes());
        msg.extend_from_slice(body);
        msg.resize(4 + padded, 0);
        self.seq = self.seq.wrapping_add(1);
        self.write(msg);
    }

    fn query_extension(&mut self, name: &str) {
        let mut body = Vec::new();
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name.as_bytes());
        self.request(op::QUERY_EXTENSION, 0, &body);
        let seq = self.seq;
        self.awaiting.push_back((seq, Awaiting::Composite));
    }

    /// Redirect a window's drawing to an offscreen pixmap.
    ///
    /// This is the request that makes rootless Xwayland hand a window over:
    /// `xwl_realize_window` creates a `wl_surface` only for a window whose
    /// `redirectDraw` is manual, so a window manager that does not redirect
    /// gets MapRequest, MapNotify, a client happily rendering -- and no
    /// surface, no `WL_SURFACE_ID`, and an empty screen. wlroots, mutter and
    /// kwin all redirect for the same reason.
    fn redirect(&mut self, window: u32) {
        if self.composite == 0 || !self.redirected.insert(window) {
            return;
        }
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.push(composite::MANUAL);
        body.extend_from_slice(&[0u8; 3]);
        self.request(self.composite, composite::REDIRECT_WINDOW, &body);
    }

    fn intern_atom(&mut self, name: &str, i: usize) {
        let mut body = Vec::new();
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name.as_bytes());
        self.request(op::INTERN_ATOM, 0, &body);
        let seq = self.seq;
        self.awaiting.push_back((seq, Awaiting::Atom(i)));
    }

    fn change_window_attributes(&mut self, window: u32, mask: u32, values: &[u32]) {
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        self.request(op::CHANGE_WINDOW_ATTRIBUTES, 0, &body);
    }

    fn map_window(&mut self, window: u32) {
        let body = window.to_le_bytes();
        self.request(op::MAP_WINDOW, 0, &body);
    }

    /// Grant a client's configure request, or impose a position of our own.
    fn configure_window(&mut self, window: u32, mask: u16, values: &[u32]) {
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        self.request(op::CONFIGURE_WINDOW, 0, &body);
    }

    fn change_property(&mut self, window: u32, property: u32, ptype: u32, format: u8, data: &[u8]) {
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&property.to_le_bytes());
        body.extend_from_slice(&ptype.to_le_bytes());
        body.push(format);
        body.extend_from_slice(&[0u8; 3]);
        let units = match format {
            8 => data.len(),
            16 => data.len() / 2,
            _ => data.len() / 4,
        };
        body.extend_from_slice(&(units as u32).to_le_bytes());
        body.extend_from_slice(data);
        self.request(op::CHANGE_PROPERTY, 0, &body);
    }

    fn get_property(&mut self, window: u32, property: u32, what: Awaiting) {
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&property.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
        body.extend_from_slice(&0u32.to_le_bytes()); // long_offset
        body.extend_from_slice(&256u32.to_le_bytes()); // long_length
        self.request(op::GET_PROPERTY, 0, &body);
        let seq = self.seq;
        self.awaiting.push_back((seq, what));
    }

    fn set_input_focus(&mut self, window: u32) {
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
        // revert_to = PointerRoot, so losing the focus does not leave the
        // server with none at all.
        self.request(op::SET_INPUT_FOCUS, 1, &body);
    }

    /// `WM_DELETE_WINDOW`: ask the client to close, which is what a titlebar's
    /// close button means. A client may put up a dialog instead, and that is
    /// its right.
    fn send_delete(&mut self, window: u32) {
        let (protocols, delete) = (self.atoms.wm_protocols, self.atoms.wm_delete_window);
        if protocols == 0 || delete == 0 {
            return;
        }
        let mut event = [0u8; 32];
        event[0] = ev::CLIENT_MESSAGE;
        event[1] = 32; // format
        event[4..8].copy_from_slice(&window.to_le_bytes());
        event[8..12].copy_from_slice(&protocols.to_le_bytes());
        event[12..16].copy_from_slice(&delete.to_le_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // no event mask: to the client
        body.extend_from_slice(&event);
        self.request(op::SEND_EVENT, 0, &body);
    }

    /// `WM_TAKE_FOCUS`: "you have the keyboard now", for a client that asked to
    /// be told rather than watching the input focus itself.
    fn send_take_focus(&mut self, window: u32) {
        let (protocols, take) = (self.atoms.wm_protocols, self.atoms.wm_take_focus);
        if protocols == 0 || take == 0 {
            return;
        }
        let mut event = [0u8; 32];
        event[0] = ev::CLIENT_MESSAGE;
        event[1] = 32;
        event[4..8].copy_from_slice(&window.to_le_bytes());
        event[8..12].copy_from_slice(&protocols.to_le_bytes());
        event[12..16].copy_from_slice(&take.to_le_bytes());
        // The timestamp. A real one would be the time of the event that caused
        // the focus change, which this manager does not select for; CurrentTime
        // is what a compositor without one sends.
        event[16..20].copy_from_slice(&0u32.to_le_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&event);
        self.request(op::SEND_EVENT, 0, &body);
    }

    /// `WM_STATE = NormalState`, which is how a window manager says "managed".
    /// Xwayland waits for it before it treats the window as mapped.
    fn set_wm_state_normal(&mut self, window: u32) {
        let atom = self.atoms.wm_state;
        if atom == 0 {
            return;
        }
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_le_bytes()); // NormalState
        data.extend_from_slice(&0u32.to_le_bytes()); // no icon window
        self.change_property(window, atom, atom, 32, &data);
    }

    // -- the pump ----------------------------------------------------------

    /// Move bytes both ways and act on whatever arrived. Run once per
    /// scheduler turn, beside the compositor's own pump.
    pub fn pump(&mut self, client: &mut Client) {
        if self.failed {
            return;
        }
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.sock.read_at(0, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.inbox.extend_from_slice(&buf[..n as usize]),
            }
        }
        if !self.setup_done {
            self.parse_setup(client);
            // A server that never answers the setup is the difference between
            // a managed session and an empty screen, and it looks exactly like
            // a manager that was never started.
            self.waited = self.waited.wrapping_add(1);
            if self.waited == 2000 {
                unsafe {
                    crate::write_stderr(&format!(
                        "[xwm] no setup reply from the X server yet ({} bytes in)\n",
                        self.inbox.len()
                    ));
                }
            }
        }
        if self.setup_done {
            while self.parse_message(client) {}
            self.retry_pairs(client);
        }
        self.flush();
    }

    fn parse_setup(&mut self, _client: &mut Client) {
        if self.inbox.len() < 8 {
            return;
        }
        let status = self.inbox[0];
        let extra = u16::from_le_bytes([self.inbox[6], self.inbox[7]]) as usize * 4;
        if self.inbox.len() < 8 + extra {
            return;
        }
        if status != 1 {
            crate::warn_line("gex: the X window manager was refused by the X server\n");
            self.failed = true;
            return;
        }
        let body = &self.inbox[8..8 + extra];
        // The fields the manager needs: where to allocate ids, and the root.
        self.id_base = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        self.id_mask = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let vendor_len = u16::from_le_bytes([body[16], body[17]]) as usize;
        let formats = body[21] as usize;
        let mut at = 32 + ((vendor_len + 3) & !3) + formats * 8;
        if at + 36 <= body.len() {
            self.root = u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
            at += 4;
            let _ = at;
        }
        self.inbox.drain(..8 + extra);
        self.setup_done = true;
        // Become the window manager, then learn the atoms.
        let root = self.root;
        self.change_window_attributes(root, 0x0000_0800, &[ROOT_EVENT_MASK]);
        for (i, name) in ATOM_NAMES.iter().enumerate() {
            self.intern_atom(name, i);
        }
        self.query_extension("Composite");
        unsafe {
            crate::write_stderr(&format!(
                "[xwm] managing the X server: root 0x{root:x}\n"
            ));
        }
    }

    /// One reply, error or event. Returns whether more may follow.
    fn parse_message(&mut self, client: &mut Client) -> bool {
        if self.inbox.len() < 32 {
            return false;
        }
        let kind = self.inbox[0];
        // A reply carries extra words; an event and an error are always 32.
        let total = if kind == 1 {
            let extra = u32::from_le_bytes([
                self.inbox[4],
                self.inbox[5],
                self.inbox[6],
                self.inbox[7],
            ]) as usize;
            32 + extra * 4
        } else {
            32
        };
        if self.inbox.len() < total {
            return false;
        }
        let msg: Vec<u8> = self.inbox.drain(..total).collect();
        match kind {
            0 => {
                // An error. Reported once with its code, because a manager that
                // gets BadAccess on the root is not managing anything and
                // should say so rather than sit quietly.
                let code = msg[1];
                let major = msg[10];
                unsafe {
                    crate::write_stderr(&format!(
                        "[xwm] X error {code} on request {major}\n"
                    ));
                }
                // The reply to *this* request will never come. Named by its
                // sequence number, which the error carries.
                let seq = u16::from_le_bytes([msg[2], msg[3]]);
                self.awaiting.retain(|(s, _)| *s != seq);
            }
            1 => self.reply(&msg, client),
            _ => self.event(&msg, client),
        }
        true
    }

    fn reply(&mut self, msg: &[u8], client: &mut Client) {
        // Matched by sequence number: replies arrive in request order, but not
        // every request that produces one is waited for. Anything older than
        // this reply is never coming.
        let seq = u16::from_le_bytes([msg[2], msg[3]]);
        while self
            .awaiting
            .front()
            .is_some_and(|(s, _)| s.wrapping_sub(seq) > 0x8000)
        {
            self.awaiting.pop_front();
        }
        let Some(what) = self
            .awaiting
            .front()
            .is_some_and(|(s, _)| *s == seq)
            .then(|| self.awaiting.pop_front().map(|(_, w)| w))
            .flatten()
        else {
            return;
        };
        match what {
            Awaiting::Protocols(window) => {
                // A list of atoms, 32 bits each: `value_len` counts items.
                let items = u32::from_le_bytes([msg[16], msg[17], msg[18], msg[19]]) as usize;
                let take = self.atoms.wm_take_focus;
                let mut takes_focus = false;
                for i in 0..items.min((msg.len().saturating_sub(32)) / 4) {
                    let at = 32 + i * 4;
                    let atom = u32::from_le_bytes([msg[at], msg[at + 1], msg[at + 2], msg[at + 3]]);
                    if atom != 0 && atom == take {
                        takes_focus = true;
                    }
                }
                if let Some(w) = self.windows.get_mut(&window) {
                    w.takes_focus = takes_focus;
                }
            }
            Awaiting::Atom(i) => {
                let atom = u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]);
                self.atoms.set(i, atom);
                self.atoms_done += 1;
                if self.atoms_done == ATOM_NAMES.len() {
                    self.announce_wm();
                }
            }
            Awaiting::Composite => {
                let present = msg[8];
                self.composite = if present != 0 { msg[9] } else { 0 };
                unsafe {
                    crate::write_stderr(&format!(
                        "[xwm] Composite is {}\n",
                        if self.composite != 0 { "available" } else { "MISSING" }
                    ));
                }
                if self.composite != 0 {
                    // QueryVersion first, which is how an extension is
                    // supposed to be entered, and redirect whatever already
                    // exists: windows created before the manager arrived get
                    // no CreateNotify.
                    let mut body = Vec::new();
                    body.extend_from_slice(&0u32.to_le_bytes());
                    body.extend_from_slice(&4u32.to_le_bytes());
                    self.request(self.composite, composite::QUERY_VERSION, &body);
                    let known: Vec<u32> = self.windows.keys().copied().collect();
                    for window in known {
                        self.redirect(window);
                    }
                }
            }
            Awaiting::Title(window) | Awaiting::LegacyTitle(window) => {
                let len = u32::from_le_bytes([msg[16], msg[17], msg[18], msg[19]]) as usize;
                let title = String::from_utf8_lossy(&msg[32..(32 + len).min(msg.len())]).to_string();
                if title.is_empty() && matches!(what, Awaiting::Title(_)) {
                    // Nothing under _NET_WM_NAME: ask for WM_NAME (atom 39,
                    // one of X11's predefined ones).
                    self.get_property(window, 39, Awaiting::LegacyTitle(window));
                    return;
                }
                if let Some(w) = self.windows.get_mut(&window) {
                    if !title.is_empty() && w.title != title {
                        w.title = title;
                        w.announced = false;
                    }
                }
                self.announce(window, client);
            }
        }
    }

    /// The two EWMH properties that make a client believe there is a window
    /// manager: a check window owning `_NET_SUPPORTING_WM_CHECK`, and a
    /// `_NET_SUPPORTED` list. Toolkits ask, and some behave oddly when the
    /// answer is nothing.
    fn announce_wm(&mut self) {
        let root = self.root;
        let id = self.new_id();
        self.check_window = id;
        let mut body = Vec::new();
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&root.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes()); // x
        body.extend_from_slice(&0i16.to_le_bytes()); // y
        body.extend_from_slice(&1u16.to_le_bytes()); // width
        body.extend_from_slice(&1u16.to_le_bytes()); // height
        body.extend_from_slice(&0u16.to_le_bytes()); // border
        body.extend_from_slice(&2u16.to_le_bytes()); // class: InputOnly
        body.extend_from_slice(&0u32.to_le_bytes()); // visual: CopyFromParent
        body.extend_from_slice(&0u32.to_le_bytes()); // value mask
        self.request(op::CREATE_WINDOW, 0, &body);
        let check = self.atoms.net_supporting_wm_check;
        let supported = self.atoms.net_supported;
        let normal = self.atoms.net_wm_window_type_normal;
        if check != 0 {
            let data = id.to_le_bytes();
            self.change_property(root, check, 33 /* WINDOW */, 32, &data);
            self.change_property(id, check, 33, 32, &data);
        }
        if supported != 0 && normal != 0 {
            let mut data = Vec::new();
            data.extend_from_slice(&self.atoms.net_wm_window_type.to_le_bytes());
            data.extend_from_slice(&normal.to_le_bytes());
            self.change_property(root, supported, 4 /* ATOM */, 32, &data);
        }
        // And take the manager selection. See the note beside "WM_S0" in
        // ATOM_NAMES: for a `-wm` server this is what opens the display.
        if self.atoms.wm_s0 != 0 {
            let mut body = Vec::new();
            body.extend_from_slice(&id.to_le_bytes());               // owner
            body.extend_from_slice(&self.atoms.wm_s0.to_le_bytes()); // selection
            body.extend_from_slice(&0u32.to_le_bytes());             // CurrentTime
            self.request(op::SET_SELECTION_OWNER, 0, &body);
            unsafe { write_stderr("[xwm] took the WM_S0 selection: the display is open\n") };
        }
    }

    fn event(&mut self, msg: &[u8], client: &mut Client) {
        let kind = msg[0] & 0x7f;

        let word = |at: usize| u32::from_le_bytes([msg[at], msg[at + 1], msg[at + 2], msg[at + 3]]);
        let half = |at: usize| i16::from_le_bytes([msg[at], msg[at + 1]]) as i32;
        match kind {
            ev::CREATE_NOTIFY => {
                let window = word(8);
                let w = self.windows.entry(window).or_default();
                w.x = half(12);
                w.y = half(14);
                w.w = u16::from_le_bytes([msg[16], msg[17]]) as i32;
                w.h = u16::from_le_bytes([msg[18], msg[19]]) as i32;
                w.override_redirect = msg[22] != 0;
                self.redirect(window);
            }
            ev::DESTROY_NOTIFY => {
                let window = word(8);
                // Everything keyed by the id goes with the window: the id is
                // the next window's to reuse (see `redirected`).
                self.redirected.remove(&window);
                self.pending_pairs.retain(|(w, _)| *w != window);
                if let Some(w) = self.windows.remove(&window) {
                    if let Some(surface) = w.surface {
                        client.forget_x11(surface);
                    }
                }
            }
            ev::MAP_REQUEST => {
                // We are the window manager, so this is ours to grant. Every
                // one of them is granted: there is no policy here beyond
                // "windows appear".
                let window = word(8);
                self.windows.entry(window).or_default();
                // A window created before the manager was listening gets no
                // CreateNotify, so redirect here too. Redirecting twice is
                // BadAccess and harmless.
                self.redirect(window);
                self.set_wm_state_normal(window);
                self.map_window(window);
                // Which ICCCM messages this client accepts. Asked at map time
                // because that is when the client has set the property and
                // before the compositor gives it the focus.
                let protocols = self.atoms.wm_protocols;
                if protocols != 0 {
                    self.get_property(window, protocols, Awaiting::Protocols(window));
                }
            }
            ev::MAP_NOTIFY => {
                let window = word(8);
                let w = self.windows.entry(window).or_default();
                w.mapped = true;
                w.override_redirect = msg[12] != 0;
                w.announced = false;
                // The title, if the client set one before mapping.
                let name = self.atoms.net_wm_name;
                if name != 0 {
                    self.get_property(window, name, Awaiting::Title(window));
                } else {
                    self.announce(window, client);
                }
            }
            ev::UNMAP_NOTIFY => {
                let window = word(8);
                if let Some(w) = self.windows.get_mut(&window) {
                    w.mapped = false;
                    let surface = w.surface.take();
                    w.announced = false;
                    if let Some(surface) = surface {
                        client.forget_x11(surface);
                    }
                }
            }
            ev::CONFIGURE_REQUEST => {
                // Granted as asked: the compositor decides where a window goes
                // on the *output*, and the X geometry only has to stay
                // consistent with what the client believes.
                let window = word(8);
                let mask = u16::from_le_bytes([msg[26], msg[27]]);
                let (x, y) = (half(16), half(18));
                let w = u16::from_le_bytes([msg[20], msg[21]]) as i32;
                let h = u16::from_le_bytes([msg[22], msg[23]]) as i32;
                let mut values = Vec::new();
                let mut out_mask = 0u16;
                if mask & CW_X != 0 {
                    out_mask |= CW_X;
                    values.push(x as u32);
                }
                if mask & CW_Y != 0 {
                    out_mask |= CW_Y;
                    values.push(y as u32);
                }
                if mask & CW_WIDTH != 0 {
                    out_mask |= CW_WIDTH;
                    values.push(w as u32);
                }
                if mask & CW_HEIGHT != 0 {
                    out_mask |= CW_HEIGHT;
                    values.push(h as u32);
                }
                if out_mask != 0 {
                    self.configure_window(window, out_mask, &values);
                }
            }
            ev::CONFIGURE_NOTIFY => {
                let window = word(8);
                if let Some(w) = self.windows.get_mut(&window) {
                    w.x = half(16);
                    w.y = half(18);
                    w.w = u16::from_le_bytes([msg[20], msg[21]]) as i32;
                    w.h = u16::from_le_bytes([msg[22], msg[23]]) as i32;
                    w.announced = false;
                }
                self.announce(window, client);
            }
            ev::PROPERTY_NOTIFY => {
                let window = word(4);
                let atom = word(8);
                if (atom == self.atoms.net_wm_name && atom != 0) || atom == 39 {
                    let name = self.atoms.net_wm_name;
                    self.get_property(window, name, Awaiting::Title(window));
                }
            }
            ev::CLIENT_MESSAGE => {
                let window = word(4);
                let ty = word(8);
                if ty == self.atoms.wl_surface_id && ty != 0 {
                    // The pairing: this X window's contents are that
                    // `wl_surface`, named by its object id on Xwayland's own
                    // Wayland connection.
                    let surface = word(12);
                    let w = self.windows.entry(window).or_default();
                    w.surface = Some(surface);
                    w.announced = false;
                    self.pending_pairs.push((window, surface));
                }
            }
            _ => {}
        }
    }

    /// Tell the compositor what this window is, once it has both halves.
    fn announce(&mut self, window: u32, client: &mut Client) {
        let Some(w) = self.windows.get(&window).cloned() else {
            return;
        };
        let Some(surface) = w.surface else { return };
        if !w.mapped || w.announced {
            return;
        }
        unsafe {
            crate::write_stderr(&format!(
                "[xwm] window 0x{window:x} is surface {surface}: {}x{} at {},{}{}\n",
                w.w,
                w.h,
                w.x,
                w.y,
                if w.override_redirect { " (menu)" } else { "" }
            ));
        }
        client.adopt_x11(surface, &w.title, w.override_redirect, w.x, w.y);
        // And which window it is: a GL client names its drawable, and the
        // compositor has no other way to know which surface that became.
        client.set_xid(surface, window);
        // Publish it even before the X server has anything to commit.
        //
        // Xwayland commits a window's surface when the window is damaged, and
        // a client whose drawing goes to the host's GL rather than through X
        // never damages anything: SDL's window has no background, so not even
        // the initial fill happens. Xwayland created the surface, set its
        // input region and allocated a pool for it -- and never
        // committed, so the compositor never learned the window
        // existed and had nowhere to put the frames the host was drawing.
        //
        // A window this manager has adopted is a window that exists, which is
        // reason enough to publish it. It is not a claim about where the
        // pixels are: a buffer is still uploaded whenever one arrives, so an
        // ordinary X client is unaffected beyond appearing a frame earlier.
        client.attach_external(surface, w.w.max(1), w.h.max(1));
        if let Some(e) = self.windows.get_mut(&window) {
            e.announced = true;
        }
    }

    /// A `WL_SURFACE_ID` can arrive before the surface itself; keep trying.
    fn retry_pairs(&mut self, client: &mut Client) {
        if self.pending_pairs.is_empty() {
            return;
        }
        let pairs = std::mem::take(&mut self.pending_pairs);
        for (window, surface) in pairs {
            let known = self.windows.get(&window).map(|w| w.mapped).unwrap_or(false);
            if !known {
                // Not mapped yet: the pairing stands, `MAP_NOTIFY` will use it.
                continue;
            }
            if !client.has_surface(surface) {
                self.pending_pairs.push((window, surface));
                continue;
            }
            self.announce(window, client);
        }
    }

    // -- what the compositor asks of the X side ----------------------------

    /// The X window a surface belongs to, if it is an X window at all.
    fn window_of(&self, surface: u32) -> Option<u32> {
        self.windows
            .iter()
            .find(|(_, w)| w.surface == Some(surface))
            .map(|(id, _)| *id)
    }

    /// The compositor's close button on an X window.
    pub fn close(&mut self, surface: u32) -> bool {
        let Some(window) = self.window_of(surface) else {
            return false;
        };
        // Said out loud: asking a client to close is the one thing here that
        // ends a session, and an X client obeys by hanging up its whole process
        // group. If one arrives that nobody asked for, this is the line that
        // says so.
        unsafe {
            write_stderr(&format!(
                "[xwm] asking window {window:#x} (surface {surface}) to close\n"
            ))
        };
        self.send_delete(window);
        true
    }

    /// The compositor gave this window the keyboard.
    ///
    /// Two things, because clients disagree about which they watch: the X input
    /// focus, and the ICCCM `WM_TAKE_FOCUS` message. A client that lists
    /// `WM_TAKE_FOCUS` in `WM_PROTOCOLS` and is only given the first may never
    /// consider itself focused, and then it ignores every key.
    pub fn focus(&mut self, surface: u32) -> bool {
        let Some(window) = self.window_of(surface) else {
            return false;
        };
        self.set_input_focus(window);
        // Only if the client asked for it: see `send_take_focus`.
        if self.windows.get(&window).is_some_and(|w| w.takes_focus) {
            self.send_take_focus(window);
        }
        true
    }

    /// The compositor moved the window. The X side is told so that anything the
    /// client positions in X coordinates -- a menu, a tooltip -- lands where
    /// the window now is.
    pub fn moved(&mut self, surface: u32, x: i32, y: i32) -> bool {
        let Some(window) = self.window_of(surface) else {
            return false;
        };
        if let Some(w) = self.windows.get_mut(&window) {
            w.x = x;
            w.y = y;
        }
        self.configure_window(window, CW_X | CW_Y, &[x as u32, y as u32]);
        true
    }

    /// The compositor resized the window (its frame dragged, or maximized):
    /// the X window is configured to that size, the client gets its
    /// ConfigureNotify and redraws, and Xwayland's surface follows.
    pub fn resized(&mut self, surface: u32, w: i32, h: i32) -> bool {
        let Some(window) = self.window_of(surface) else {
            return false;
        };
        if w <= 0 || h <= 0 {
            return false;
        }
        self.configure_window(window, CW_WIDTH | CW_HEIGHT, &[w as u32, h as u32]);
        true
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use gex_kernel::errno::Errno;
    use gex_kernel::vfs::{FileType, Metadata};

    /// A connection that takes whatever is written and has nothing to read.
    #[derive(Debug)]
    struct Sink;
    impl FileHandle for Sink {
        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> gex_kernel::errno::SysResult {
            Err(Errno::EAGAIN)
        }
        fn write_at(&self, _offset: u64, buf: &[u8]) -> gex_kernel::errno::SysResult {
            Ok(buf.len() as u64)
        }
        fn metadata(&self) -> Result<Metadata, Errno> {
            let mut m = Metadata::regular(0);
            m.file_type = FileType::Fifo;
            Ok(m)
        }
    }

    fn notify(kind: u8, window: u32) -> Vec<u8> {
        let mut m = vec![0u8; 32];
        m[0] = kind;
        m[8..12].copy_from_slice(&window.to_le_bytes());
        m
    }

    /// Composite `RedirectWindow` requests in what the manager has written.
    fn redirects(xwm: &Xwm) -> usize {
        let mut n = 0;
        let mut at = 0;
        while at + 4 <= xwm.outbox.len() {
            let units = u16::from_le_bytes([xwm.outbox[at + 2], xwm.outbox[at + 3]]) as usize;
            if xwm.outbox[at] == xwm.composite && xwm.outbox[at + 1] == composite::REDIRECT_WINDOW {
                n += 1;
            }
            at += units.max(1) * 4;
        }
        n
    }

    #[test]
    fn a_window_id_used_again_is_redirected_again() {
        let mut xwm = Xwm::start(Arc::new(Sink)).unwrap();
        xwm.composite = 140;
        xwm.outbox.clear();
        let mut client = Client::new();
        // The first glxgears: created, then gone.
        xwm.event(&notify(ev::CREATE_NOTIFY, 0x60_0002), &mut client);
        xwm.event(&notify(ev::DESTROY_NOTIFY, 0x60_0002), &mut client);
        assert_eq!(redirects(&xwm), 1);
        // The second, with the same id: it needs its own redirection, or the
        // X server never makes it a surface.
        xwm.event(&notify(ev::CREATE_NOTIFY, 0x60_0002), &mut client);
        assert_eq!(redirects(&xwm), 2, "the reused id was not redirected");
        // And a second meeting of one live window is still only one.
        xwm.event(&notify(ev::MAP_REQUEST, 0x60_0002), &mut client);
        assert_eq!(redirects(&xwm), 2);
    }
}
