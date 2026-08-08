//! A virtio-gpu renderer in a browser tab.
//!
//! The same device the native host provides, and deliberately the same
//! protocol: the guest runs Debian's own Mesa with its Gallium `virgl`
//! driver, which connects to a UNIX socket and speaks vtest over it. Nothing
//! in the guest knows which side is listening, or that it is a web page.
//!
//! Natively the other end is the host's libvirglrenderer on the host's real
//! OpenGL. Here it is the same renderer compiled to WebAssembly, replaying
//! the same stream onto a WebGL 2 context -- `web/build-virgl-wasm.sh` builds
//! it and `web/src/virgl.js` is the page's side.
//!
//! # Why this is the only way to know whether it works
//!
//! A recorded stream can be replayed into the browser renderer, and that
//! proves the transport, the framing and the dispatch. It cannot prove a
//! picture, and no recording ever will: the guest's Mesa asks what the host
//! can do before it does anything, and every shader, format and draw follows
//! from the answer. Natively the answer is capability set version 2, 5,508
//! bytes of it; WebGL 2 can offer only version 1, 308 bytes. A stream
//! recorded against one is not a valid input to the other, whichever host API
//! was used -- forcing the native side onto GLES does not change the capset
//! version. So the guest has to negotiate with the renderer that will serve
//! it, which means this file.
//!
//! # Shape
//!
//! The compositor's, and for the same reason: a client writes a request and
//! then waits for the reply, and the write is a syscall, so the pump has to
//! run between the syscall that sends and the one that waits. A guest blocked
//! with nobody servicing its socket is a guest that never wakes.

use gex_kernel::unix::{HostListener, UnixSocket};
use std::sync::{Arc, Mutex};

#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Hand the renderer `len` bytes at `ptr` and run every command they
    /// complete. Returns the number of reply bytes waiting, or negative if
    /// the page has no renderer.
    fn virgl_submit(session: u32, ptr: u32, len: u32) -> i32;
    /// Copy up to `max` reply bytes into `ptr`; returns how many were copied.
    fn virgl_take(session: u32, ptr: u32, max: u32) -> i32;
    /// Let go of a client's context and its buffers.
    fn virgl_close(session: u32);
}

/// Set when the page said it brought a renderer.
///
/// Off by default. A guest that connects to a socket nobody serves is worse
/// off than one that finds no socket at all: Mesa's vtest winsys does not
/// fall back, it writes its handshake into the dead descriptor and then
/// blocks reading a reply that cannot come.
static mut HAVE_VIRGL: bool = false;

/// Say that the page has a renderer behind the imports above.
///
/// # Safety
/// Called from JS before the guest runs, on the one thread a page has.
#[no_mangle]
pub extern "C" fn gex_set_virgl(on: i32) {
    unsafe { HAVE_VIRGL = on != 0 }
    TRACE.store(on == 2, std::sync::atomic::Ordering::Relaxed);
}

pub fn have_virgl() -> bool {
    unsafe { HAVE_VIRGL }
}

/// Narrate every pump. Set by passing 2 rather than 1 to `gex_set_virgl`.
pub static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Where Mesa's vtest winsys looks unless `VTEST_SOCKET_NAME` says otherwise.
pub const DEFAULT_SOCKET: &str = "/tmp/.virgl_test";

/// How many GL programs can have the renderer open at once. Matches the
/// renderer's own table; a connection past it is refused rather than given a
/// number that would collide with a live one.
const MAX_SESSIONS: usize = 16;

struct Session {
    /// Which of the renderer's clients this is.
    ///
    /// Each one has its own vtest context, its own resource handles and its
    /// own stream, and the renderer keeps them apart by this number. They
    /// used to share one of each, which held up for exactly as long as there
    /// was one client: the second's `CREATE_RENDERER` arrived at a renderer
    /// that already had a context, was refused as an unknown command, and
    /// took the stream down with it. Nothing said so -- the guest simply
    /// waited for a reply for ever.
    id: u32,
    socket: Arc<UnixSocket>,
    /// Reply bytes the guest's socket had no room for yet, and how much of
    /// them has gone.
    ///
    /// A UNIX socket holds 256 KB and a vtest reply can be larger: reading
    /// back a 300x300 framebuffer is 360,000 bytes in one `TRANSFER_GET`.
    /// The native device dropped that remainder once, and what it looked like
    /// was not an error -- Mesa read what arrived and blocked forever waiting
    /// for the rest, so glxgears drew exactly one frame and stopped with
    /// every component reporting success.
    out: Vec<u8>,
    sent: usize,
}

#[derive(Default)]
struct Inner {
    sessions: Vec<Session>,
    /// Which client numbers are in use. The renderer keeps a fixed table, so
    /// a long run that opens and closes windows has to give numbers back
    /// rather than count upwards.
    taken: [bool; MAX_SESSIONS],
    /// How many clients have ever connected, so a session that vanishes can
    /// be told from one that never arrived.
    accepted: u64,
}

#[derive(Clone, Default)]
pub struct VirglDevice {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for VirglDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VirglDevice")
    }
}

impl VirglDevice {
    pub fn new() -> VirglDevice {
        VirglDevice::default()
    }

    /// Narrate a pump, when `gex_set_virgl(2)` asked for it.
    ///
    /// Through the same import the guest's output uses, because in a page
    /// that is the only channel there is -- and a pump that never produces
    /// anything is indistinguishable from a pump that never runs.
    fn trace(text: &str) {
        if unsafe { HAVE_VIRGL } {
            // SAFETY: the one thread a page has, and `write` only reads.
            unsafe { crate::write_stderr(text) };
        }
    }

    /// Move bytes in both directions for every client.
    pub fn pump(&self) {
        // Counted, and reported on a curve, because the two failures this has
        // to tell apart are "never ran" and "ran a million times without
        // getting anywhere" -- and a trace that only prints when there are
        // bytes to move cannot distinguish them.
        if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 4 || n.is_power_of_two() {
                let live = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.len();
                Self::trace(&format!("[virgl] pump #{n}, {live} sessions\n"));
            }
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.sessions.is_empty() {
            return;
        }
        // Mesa submits a whole command buffer in one write, and those run to
        // tens of kilobytes for a frame with real geometry.
        let mut buf = vec![0u8; 256 * 1024];
        let mut freed = Vec::new();
        inner.sessions.retain_mut(|s| {
            let mut gone = false;
            loop {
                let n = match s.socket.recv(&mut buf, 0, false) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => n as usize,
                    // Nothing more waiting.
                    Err(_) => break,
                };
                // SAFETY: the page reads `n` bytes at this address out of
                // this module's memory, which is the same arrangement the GL
                // device uses, and does not retain the pointer.
                let waiting = unsafe { virgl_submit(s.id, buf.as_ptr() as u32, n as u32) };
                if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                    Self::trace(&format!("[virgl] fed {n} bytes, {waiting} waiting\n"));
                }
                if waiting > 0 {
                    let at = s.out.len();
                    s.out.resize(at + waiting as usize, 0);
                    // SAFETY: as above, and the slice is exactly `waiting`.
                    let took = unsafe {
                        virgl_take(s.id, s.out[at..].as_mut_ptr() as u32, waiting as u32)
                    };
                    if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                        Self::trace(&format!("[virgl] took {took} of {waiting}\n"));
                    }
                    s.out.truncate(at + took.max(0) as usize);
                }
                if n < buf.len() {
                    break;
                }
            }

            // As much as the socket will take. Whatever is left waits for the
            // next pump, by which time the guest will have read some of what
            // is already there and made room.
            while s.sent < s.out.len() {
                match s.socket.send(&s.out[s.sent..]) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => s.sent += k as usize,
                }
            }
            if TRACE.load(std::sync::atomic::Ordering::Relaxed) && !s.out.is_empty() {
                Self::trace(&format!(
                    "[virgl] queued {}, sent {}\n",
                    s.out.len(),
                    s.sent
                ));
            }
            if s.sent == s.out.len() {
                s.out.clear();
                s.sent = 0;
            }
            if gone {
                // SAFETY: an import that takes a number and retains nothing.
                unsafe { virgl_close(s.id) };
                freed.push(s.id);
            }
            !gone
        });
        for id in freed {
            inner.taken[id as usize] = false;
        }
    }
}

impl HostListener for VirglDevice {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.accepted += 1;
        let Some(id) = (0..MAX_SESSIONS).find(|&i| !inner.taken[i]) else {
            // Refused out loud. A client given a number already in use would
            // render into another program's resources, which looks like
            // corruption rather than like being turned away.
            Self::trace("[virgl] no free session: refusing a client\n");
            return;
        };
        inner.taken[id] = true;
        // Said out loud, and not only under the verbose flag. This is the
        // moment the guest's Mesa finds the GPU, and it is the one event that
        // separates "still translating libGL" from "talking to the renderer"
        // -- which is a distinction that cost a day of watching a pump report
        // zero sessions with nothing to say why.
        Self::trace(&format!(
            "[virgl] a guest connected: session {}\n",
            inner.accepted
        ));
        inner.sessions.push(Session {
            id: id as u32,
            socket: peer,
            out: Vec::new(),
            sent: 0,
        });
    }
}
