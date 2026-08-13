//! A virtio-gpu renderer in a browser tab.
//!
//! Same protocol as the native device: the guest runs Debian's own Mesa Gallium
//! `virgl` driver, which connects to a UNIX socket and speaks vtest. Here the
//! other end is libvirglrenderer compiled to wasm, replaying the stream onto a
//! WebGL 2 context (`web/build-virgl-wasm.sh` builds it, `web/src/virgl.js` is
//! the page side). The guest must negotiate with the renderer that will serve
//! it -- WebGL 2 offers only capset version 1 (308 bytes) against native's
//! version 2 (5,508) -- so a recorded stream cannot substitute for this.
//!
//! Pump between the send syscall and the wait: a guest blocked with nobody
//! servicing its socket never wakes.

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

/// Set when the page said it brought a renderer. Off by default: Mesa's vtest
/// winsys does not fall back, so a socket nobody serves leaves it blocked
/// reading a reply that cannot come -- worse than finding no socket at all.
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
    /// Which of the renderer's clients this is: its own vtest context, resource
    /// handles and stream, kept apart by this number. Sharing one across clients
    /// took the whole stream down when the second's `CREATE_RENDERER` hit a
    /// renderer that already had a context.
    id: u32,
    socket: Arc<UnixSocket>,
    /// Reply bytes the guest's socket had no room for yet, and how many have
    /// gone. A vtest reply can exceed the socket's 256 KB (a 300x300
    /// `TRANSFER_GET` read-back is 360,000 bytes); dropping the remainder does
    /// not fail, it leaves Mesa blocked forever waiting for the rest.
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

    /// Narrate a pump, when `gex_set_virgl(2)` asked for it. Through stderr
    /// because in a page that is the only channel there is.
    fn trace(text: &str) {
        if unsafe { HAVE_VIRGL } {
            // SAFETY: the one thread a page has, and `write` only reads.
            unsafe { crate::write_stderr(text) };
        }
    }

    /// Move bytes in both directions for every client.
    pub fn pump(&self) {
        // Reported on a curve to tell "never ran" from "ran a million times
        // without getting anywhere".
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
            crate::virgl_release_session(id as i32);
        }
    }
}

impl HostListener for VirglDevice {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.accepted += 1;
        // From the global kernel: the number is the renderer's own context id,
        // shared across processes. A per-process table gave two of them zero.
        let shared = crate::virgl_take_session();
        let picked = if shared >= 0 {
            Some(shared as usize)
        } else {
            (0..MAX_SESSIONS).find(|&i| !inner.taken[i])
        };
        let Some(id) = picked else {
            // Refused rather than given a number in use: a collision renders
            // into another program's resources, which looks like corruption.
            Self::trace("[virgl] no free session: refusing a client\n");
            return;
        };
        inner.taken[id] = true;
        // Not only under the verbose flag: this is the moment Mesa finds the
        // GPU, which separates "still translating libGL" from "talking to the
        // renderer".
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
