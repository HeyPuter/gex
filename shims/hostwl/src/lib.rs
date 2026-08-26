//! The compositor for a host-side X server.
//!
//! When Xwayland is a guest program, its compositor is the one in that
//! process's shim ([`gex_browser::wl`]) and its window manager talks to it over
//! a guest unix socket. When Xwayland is a *host* module -- `Xwayland.wasm`,
//! built by `web/build-xwayland-wasm.sh` -- neither of those places exists, so
//! this module is the other end of both of its connections:
//!
//!   * the Wayland connection, which is [`gex_wayland::Client`] fed the bytes
//!     the server writes, with its events handed straight back. No socket: the
//!     server is a module in the same Worker, and its "socket" is a queue.
//!
//!   * the X connection the window manager needs, which is [`gex_xwm`] over a
//!     queue of the same kind. Xwayland is started with `-wm <fd>`, so the
//!     manager is a client it was handed rather than one that connected.
//!
//! What crosses to the page is what crosses for a guest: fixed-size scene
//! records saying where each surface's pixels are, and input events coming
//! back. The pixels themselves are in the X server's own memory, which the
//! page's compositor Worker reads directly -- so an X window reaches the screen
//! with no copy, the same as a guest's `wl_shm` buffer.

use gex_kernel::errno::Errno;
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_wayland::{Buffers, Client, SurfaceInfo};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Words in one published surface record, and words in one input event. The
/// same layout the guest shim publishes, because the page reads one arena.
const REC: usize = 28;
const EV: usize = 8;
const TITLE_BYTES: usize = 32;

#[link(wasm_import_module = "gex_host")]
extern "C" {
    /// Where the pixels of the pool this descriptor names are, in the X
    /// server's memory: two words, address then length, written to `out`.
    /// Returns 0 when the descriptor is not a pool.
    ///
    /// The Worker keeps that table because the Worker is what handed the
    /// descriptor out (see `gex_shm_alloc` in `web/xwayland/gexos.c`).
    fn host_locate(fd: u32, out: u32) -> i32;
    /// A line for the page's console.
    fn host_trace(ptr: u32, len: u32);
}

fn trace(s: &str) {
    unsafe { host_trace(s.as_ptr() as u32, s.len() as u32) };
}

/// One end of a byte queue, as a file the window manager can read and write.
///
/// [`gex_xwm`] speaks the X protocol over an `Arc<dyn FileHandle>` because in a
/// guest that is a unix socket. Here the peer is the X server in this same
/// Worker, so the "socket" is two buffers and no kernel at all.
#[derive(Default)]
struct Pipe {
    inbox: Mutex<VecDeque<u8>>,
    outbox: Mutex<Vec<u8>>,
}

impl std::fmt::Debug for Pipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pipe")
    }
}

impl FileHandle for Pipe {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<u64, Errno> {
        let mut q = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return Err(Errno(11)); // EAGAIN, which is what a live empty socket says
        }
        let n = buf.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<u64, Errno> {
        self.outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::Socket,
            size: 0,
            mode: 0o600,
            ino: 0,
            mtime: 0,
            nlink: 1,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_readable(&self) -> bool {
        !self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// Everything one host X server needs. One per Worker; a page that starts two X
/// servers gives each its own Worker and its own module instance.
struct State {
    client: Client,
    xwm: Option<gex_xwm::Xwm>,
    /// The manager's end of the `-wm` connection.
    wm: Arc<Pipe>,
    /// Events the compositor has produced and the server has not read.
    out: VecDeque<u8>,
    /// Descriptors the server has sent, in arrival order: what the compositor
    /// calls a buffer's "fd" is an index into this, not a number the server
    /// chose.
    fds: Vec<u32>,
    /// Whether the scene changed since the page last took it.
    dirty: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// A staging buffer JS copies through. One per module, grown as needed.
///
/// Words, not bytes, so the address is always 4-aligned: JS reads scene records
/// and input events out of it as an `Int32Array`, which *throws* on an
/// unaligned offset, and writes fd numbers into it by index -- and a fractional
/// index in JavaScript is a silently ignored property store, not an error. That
/// last one cost an afternoon: every buffer arrived with descriptor 0 and the
/// compositor could not place a single one.
static SCRATCH: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    g.as_mut().map(f)
}

/// Resolves a descriptor the X server sent to the pixels behind it.
struct Pools<'a> {
    fds: &'a [u32],
}

impl Buffers for Pools<'_> {
    fn locate(&self, fd: u32) -> Option<(u32, u32)> {
        let id = *self.fds.get(fd as usize)?;
        let mut out = [0u32; 2];
        let ok = unsafe { host_locate(id, out.as_mut_ptr() as u32) };
        if ok == 0 || out[0] == 0 {
            return None;
        }
        Some((out[0], out[1]))
    }

    fn map(&self, _fd: u32, _size: usize) -> Option<Vec<u8>> {
        // Never: the page reads the pixels where they are. A pool that cannot
        // be located is a bug here, not a case to copy around.
        None
    }
}

// -- what the Worker calls ---------------------------------------------------

/// Start the compositor for an output of this size.
#[no_mangle]
pub extern "C" fn hw_init(w: i32, h: i32) {
    hw_set_trace(0);
    gex_wayland::set_trace_sink(|s| trace(s));
    gex_xwm::set_trace_sink(|s| trace(s));
    let mut client = Client::new();
    client.set_default_size(w.max(1), h.max(1));
    let wm = Arc::new(Pipe::default());
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State {
        client,
        xwm: None,
        wm,
        out: VecDeque::new(),
        fds: Vec::new(),
        dirty: true,
    });
}

/// Narrate the Wayland protocol, message by message, the way `?wltrace=1` does
/// for a guest's compositor. The only way to tell a client that never drew from
/// one whose pixels went nowhere.
#[no_mangle]
pub extern "C" fn hw_set_trace(on: i32) {
    gex_wayland::set_trace(on != 0);
}

/// A buffer in this module's memory that JS can write into or read out of,
/// at least `len` bytes long. The same buffer every time.
#[no_mangle]
pub extern "C" fn hw_scratch(len: u32) -> u32 {
    let words = (len as usize + 3) / 4;
    let mut s = SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
    if s.len() < words {
        s.resize(words, 0);
    }
    s.as_ptr() as u32
}

/// Bytes the X server wrote on its Wayland connection, with any descriptors it
/// sent alongside them (`nfds` words at `fds`).
#[no_mangle]
pub extern "C" fn hw_wayland_receive(ptr: u32, len: u32, fds: u32, nfds: u32) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let sent = unsafe { std::slice::from_raw_parts(fds as *const u32, nfds as usize) };
    with(|st| {
        let first = st.fds.len();
        st.fds.extend_from_slice(sent);
        let tokens: Vec<u32> = (first..st.fds.len()).map(|i| i as u32).collect();
        let view = Pools { fds: &st.fds };
        st.client.receive(bytes, &tokens, &view);
        let events = st.client.take_outgoing();
        st.out.extend(events);
    });
}

/// Events for the X server to read on its Wayland connection: up to `max`
/// bytes into `ptr`, returning how many.
#[no_mangle]
pub extern "C" fn hw_wayland_take(ptr: u32, max: u32) -> u32 {
    with(|st| {
        let n = st.out.len().min(max as usize);
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, n) };
        for (i, b) in st.out.drain(..n).enumerate() {
            dst[i] = b;
        }
        n as u32
    })
    .unwrap_or(0)
}

/// Whether the server has Wayland events waiting.
#[no_mangle]
pub extern "C" fn hw_wayland_pending() -> u32 {
    with(|st| st.out.len() as u32).unwrap_or(0)
}

/// Bytes the X server wrote on the window manager's connection.
#[no_mangle]
pub extern "C" fn hw_x_receive(ptr: u32, len: u32) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    with(|st| {
        st.wm
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(bytes.iter().copied());
    });
}

/// Requests the window manager has for the X server.
#[no_mangle]
pub extern "C" fn hw_x_take(ptr: u32, max: u32) -> u32 {
    with(|st| {
        let mut out = st.wm.outbox.lock().unwrap_or_else(|e| e.into_inner());
        let n = out.len().min(max as usize);
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, n) };
        dst.copy_from_slice(&out[..n]);
        out.drain(..n);
        n as u32
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn hw_x_pending() -> u32 {
    with(|st| {
        st.wm
            .outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32
    })
    .unwrap_or(0)
}

/// Run the window manager over whatever has arrived, and collect the
/// compositor's events. Called once per turn of the server's loop.
#[no_mangle]
pub extern "C" fn hw_tick() {
    with(|st| {
        if st.xwm.is_none() {
            // The manager's connection is a queue that always exists, so
            // unlike the guest path there is nothing to wait for: start it the
            // first time round.
            let sock: Arc<dyn FileHandle> = Arc::clone(&st.wm) as Arc<dyn FileHandle>;
            st.xwm = gex_xwm::Xwm::start(sock);
        }
        if let Some(xwm) = st.xwm.as_mut() {
            xwm.pump(&mut st.client);
            if xwm.failed() {
                trace("[xwm] the manager's connection failed\n");
                st.xwm = None;
            }
        }
        let events = st.client.take_outgoing();
        if !events.is_empty() {
            st.out.extend(events);
        }
        // Frames are the copy-out path for a host that cannot read the client's
        // memory. This one can, so a frame here is a buffer that could not be
        // placed; take them so they do not accumulate.
        let refused = st.client.take_frames();
        if !refused.is_empty() {
            trace(&format!(
                "[wl] {} buffer(s) could not be placed and were dropped\n",
                refused.len()
            ));
        }
        st.dirty |= st.client.take_dirty();
    });
}

/// This turn's scene, as `REC`-word records: how many were written.
#[no_mangle]
pub extern "C" fn hw_scene(ptr: u32, max_records: u32) -> u32 {
    with(|st| {
        let mut out: Vec<i32> = Vec::new();
        let infos: Vec<SurfaceInfo> = st.client.scene();
        for info in &infos {
            if out.len() / REC >= max_records as usize {
                break;
            }
            push_record(&mut out, 1, info);
            st.client.clear_damage(info.surface);
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr as *mut i32, out.len()) };
        dst.copy_from_slice(&out);
        st.dirty = false;
        (out.len() / REC) as u32
    })
    .unwrap_or(0)
}

/// Whether anything about the scene changed since it was last taken.
#[no_mangle]
pub extern "C" fn hw_dirty() -> u32 {
    with(|st| u32::from(st.dirty)).unwrap_or(0)
}

/// The keymap payload a `wl_keyboard.keymap` event owes a descriptor for, if
/// one is waiting: its length, or zero. The compositor state machine cannot
/// make a descriptor (it has no kernel), so the Worker turns this into a pool
/// of bytes in the X server's memory and sends its descriptor alongside.
#[no_mangle]
pub extern "C" fn hw_keymap_take(ptr: u32, max: u32) -> u32 {
    with(|st| match st.client.take_keymap() {
        Some(km) => {
            let n = km.len().min(max as usize);
            let dst = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, n) };
            dst.copy_from_slice(&km[..n]);
            n as u32
        }
        None => 0,
    })
    .unwrap_or(0)
}

/// Whether the client has asked for a pointer or a keyboard yet. Input is not
/// drained from the page until it has: an event delivered to a client with no
/// seat is an event lost.
#[no_mangle]
pub extern "C" fn hw_wants_input() -> u32 {
    with(|st| u32::from(st.client.wants_input())).unwrap_or(0)
}

/// Input and control events from the page: `count` records of `EV` words, in
/// the layout `wl-bridge.mjs` writes and the guest shim also reads.
#[no_mangle]
pub extern "C" fn hw_input(ptr: u32, count: u32) {
    let raw = unsafe { std::slice::from_raw_parts(ptr as *const i32, count as usize * EV) };
    with(|st| {
        for i in 0..count as usize {
            let q = &raw[i * EV..i * EV + EV];
            let surface = q[6] as u32;
            match q[0] {
                0 => st.client.pointer_to(surface, q[1], q[2], q[3] as u32),
                1 => st.client.pointer_button(q[1] as u32, q[2] != 0, q[3] as u32),
                2 => st.client.pointer_axis(q[1] as u32, q[2], q[3] as u32),
                3 => st.client.keyboard_key(q[1] as u32, q[2] != 0, q[3] as u32),
                4 => st.client.keyboard_modifiers(
                    q[1] as u32,
                    q[2] as u32,
                    q[3] as u32,
                    q[4] as u32,
                ),
                5 => st.client.pointer_clear(),
                6 => {
                    st.client.keyboard_focus(surface);
                    // An X window takes the input focus through the X server,
                    // not the Wayland connection: the manager tells it.
                    if surface != 0 && st.client.is_x11(surface) {
                        if let Some(xwm) = st.xwm.as_mut() {
                            xwm.focus(surface);
                        }
                    }
                }
                7 => st.client.configure(surface, q[1], q[2], q[4] != 0),
                8 => {
                    // An X window has no xdg_toplevel to close: the manager
                    // asks the client with WM_DELETE_WINDOW.
                    let x11 = st.client.is_x11(surface);
                    match (x11, st.xwm.as_mut()) {
                        (true, Some(xwm)) => {
                            xwm.close(surface);
                        }
                        _ => st.client.close(surface),
                    }
                }
                9 => st.client.dismiss_popup(surface),
                10 => {
                    if let Some(xwm) = st.xwm.as_mut() {
                        xwm.moved(surface, q[1], q[2]);
                    }
                }
                _ => {}
            }
        }
        let events = st.client.take_outgoing();
        st.out.extend(events);
    });
}

/// The output size changed (the page's canvas was resized).
#[no_mangle]
pub extern "C" fn hw_set_output(w: i32, h: i32) {
    with(|st| st.client.set_default_size(w.max(1), h.max(1)));
}

/// Why the compositor threw the client out, if it did: the message into
/// `scratch`, its length returned. Zero means the client is still fine.
#[no_mangle]
pub extern "C" fn hw_failed(ptr: u32, max: u32) -> u32 {
    with(|st| match st.client.failed.clone() {
        Some(why) => {
            let b = why.as_bytes();
            let n = b.len().min(max as usize);
            let dst = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, n) };
            dst.copy_from_slice(&b[..n]);
            n as u32
        }
        None => 0,
    })
    .unwrap_or(0)
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
    r[16] = i32::from(info.move_requested);
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
}
